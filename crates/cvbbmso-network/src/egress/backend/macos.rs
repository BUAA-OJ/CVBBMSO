//! macOS PF management. Deployment provisions the main ruleset hook. The
//! guard loads and flushes only its reserved outbound anchor.
#![allow(
    unsafe_code,
    reason = "PF needs native privilege and interface queries"
)]

use std::net::{IpAddr, Ipv4Addr, Ipv6Addr};
use std::sync::Mutex;

use crate::NetworkError;
use crate::egress::backend::{GuardBackend, InstallResult};
use crate::egress::commands::Commands;
use crate::egress::{
    policy::{Plan, Transport},
    state,
};

mod lifecycle;
use lifecycle::Prepared;
pub(crate) use lifecycle::{HELPER, watchdog_entry};

const ANCHOR: &str = "com.cvbbmso.egress";
const RULES: &str = "/Library/Application Support/com.cvbbmso/pf.rules";
const TOKEN: &str = "/Library/Application Support/com.cvbbmso/pf.enable";
const PFCTL: &str = "/sbin/pfctl";
const ROOT_RULE: &str = "anchor \"com.cvbbmso.egress\" out quick all";
pub(crate) fn prepare() -> Result<(), NetworkError> {
    Prepared::new().map(drop)
}

pub(crate) fn install(plan: &Plan) -> Result<InstallResult, NetworkError> {
    Prepared::new()?.install(plan)
}

struct PfGuard {
    expected: Mutex<Expected>,
    watchdog: Mutex<Option<lifecycle::Watchdog>>,
}

struct Expected {
    rendered: String,
    kernel: String,
}

impl GuardBackend for PfGuard {
    fn apply(&self, plan: &Plan) -> Result<(), NetworkError> {
        let lock = state::lock()?;
        let commands = crate::egress::commands::SystemCommands(Some(&lock));
        self.verify_owner()?;
        let mut expected = self.expected.lock().map_err(|_| NetworkError::Platform)?;
        verify_with(&commands, &expected.kernel)?;
        replace_with(&commands, &mut expected, render_rules(plan))
    }

    fn verify(&self) -> Result<(), NetworkError> {
        let lock = state::lock()?;
        let commands = crate::egress::commands::SystemCommands(Some(&lock));
        let mut watchdog = self.watchdog.lock().map_err(|_| NetworkError::Platform)?;
        let watchdog = watchdog.as_mut().ok_or(NetworkError::Platform)?;
        let mut guard_state = watchdog.check()?;
        let expected = self.expected.lock().map_err(|_| NetworkError::Platform)?;
        verify_with(&commands, &expected.kernel)?;
        if guard_state.renewal_due() {
            crate::egress::cleanup::verify(&commands)?;
            guard_state.renew();
            state::write(&commands, &guard_state)?;
        }
        Ok(())
    }

    fn revert(&self) -> Result<(), NetworkError> {
        let lock = state::lock()?;
        let commands = crate::egress::commands::SystemCommands(Some(&lock));
        let mut watchdog = self.watchdog.lock().map_err(|_| NetworkError::Platform)?;
        if let Some(running) = watchdog.as_mut() {
            if running.owns_state()? {
                lifecycle::recover(&commands)?;
            }
            running.stop();
            *watchdog = None;
        }
        Ok(())
    }
}

impl PfGuard {
    fn verify_owner(&self) -> Result<(), NetworkError> {
        let mut watchdog = self.watchdog.lock().map_err(|_| NetworkError::Platform)?;
        watchdog.as_mut().ok_or(NetworkError::Platform)?.check()?;
        Ok(())
    }
}

fn install_with(
    commands: &dyn Commands,
    plan: &Plan,
    local: &[IpAddr],
) -> Result<Expected, NetworkError> {
    verify_deployment(commands)?;
    let expected = stage(commands, render_rules(plan))?;
    load_anchor(commands)?;
    commands.capture(PFCTL, &["-E"], TOKEN)?;
    read_token(commands)?.ok_or(NetworkError::Platform)?;
    verify_with(commands, &expected.kernel)?;
    // A PF state predating the anchor would otherwise pass without matching
    // the new rules. Only remove states whose initiating address is local.
    for ip in local.iter().filter(|ip| !ip.is_loopback()) {
        commands.run(PFCTL, &["-k", &ip.to_string()])?;
    }
    Ok(expected)
}

fn stage(commands: &dyn Commands, rendered: String) -> Result<Expected, NetworkError> {
    commands.write(RULES, &rendered)?;
    let output = commands.output(
        PFCTL,
        &["-a", ANCHOR, "-n", "-v", "-o", "none", "-f", RULES],
    )?;
    let kernel = output.trim().to_owned();
    if kernel.lines().last() != Some("block drop out quick all") {
        return Err(NetworkError::Platform);
    }
    Ok(Expected { rendered, kernel })
}

fn replace_with(
    commands: &dyn Commands,
    expected: &mut Expected,
    rendered: String,
) -> Result<(), NetworkError> {
    if rendered != expected.rendered {
        let next = stage(commands, rendered)?;
        load_anchor(commands)?;
        if let Err(error) = verify_with(commands, &next.kernel) {
            commands.write(RULES, &expected.rendered)?;
            load_anchor(commands)?;
            return Err(error);
        }
        *expected = next;
    }
    Ok(())
}

fn load_anchor(commands: &dyn Commands) -> Result<(), NetworkError> {
    commands.run(PFCTL, &["-a", ANCHOR, "-o", "none", "-f", RULES])
}

fn verify_deployment(commands: &dyn Commands) -> Result<(), NetworkError> {
    let root = commands.output(PFCTL, &["-s", "rules"])?;
    let interfaces = commands.output(PFCTL, &["-vv", "-s", "Interfaces"])?;
    let skipped = interfaces.lines().any(|line| {
        let mut words = line.split_whitespace();
        words.next().is_some_and(|name| name != "lo0") && words.any(|word| word == "(skip)")
    });
    // An earlier terminal rule could skip the guard. A runtime root reload
    // cannot restore unrelated live rules and children from the disk config.
    if root.lines().next().map(str::trim) != Some(ROOT_RULE) || skipped {
        return Err(NetworkError::Platform);
    }
    Ok(())
}

fn verify_with(commands: &dyn Commands, expected: &str) -> Result<(), NetworkError> {
    verify_deployment(commands)?;
    let info = commands.output(PFCTL, &["-s", "info"])?;
    let live = commands.output(PFCTL, &["-a", ANCHOR, "-s", "rules"])?;
    let reference = read_token(commands)?.ok_or(NetworkError::Platform)?;
    let references = commands.output(PFCTL, &["-s", "References"])?;
    if !info.lines().any(|line| line.starts_with("Status: Enabled"))
        || live.trim() != expected
        || !has_reference(&references, reference)
    {
        return Err(NetworkError::Platform);
    }
    Ok(())
}

fn read_token(commands: &dyn Commands) -> Result<Option<u64>, NetworkError> {
    let Some(contents) = commands.read(TOKEN)? else {
        return Ok(None);
    };
    Ok(contents.lines().find_map(|line| {
        let (key, value) = line.split_once(':')?;
        (key.trim() == "Token")
            .then(|| value.trim().parse::<u64>().ok())
            .flatten()
    }))
}

fn recover_with(commands: &dyn Commands) -> Result<(), NetworkError> {
    let live = commands.output(PFCTL, &["-a", ANCHOR, "-s", "rules"])?;
    if !live.trim().is_empty() {
        // A load always writes the protected journal before changing PF.
        // Existing rules without that journal cannot be attributed to us.
        commands.read(RULES)?.ok_or(NetworkError::Platform)?;
        commands.run(PFCTL, &["-a", ANCHOR, "-F", "rules"])?;
    }

    if let Some(token) = read_token(commands)?
        && commands.run(PFCTL, &["-X", &token.to_string()]).is_err()
    {
        let references = commands.output(PFCTL, &["-s", "References"])?;
        if has_reference(&references, token) {
            return Err(NetworkError::Platform);
        }
    }
    for path in [TOKEN, RULES] {
        commands.remove(path)?;
    }
    Ok(())
}

fn has_reference(output: &str, token: u64) -> bool {
    output.lines().any(|line| {
        line.split_whitespace()
            .nth(2)
            .and_then(|value| value.parse::<u64>().ok())
            == Some(token)
    })
}

/// Renders the complete PF rule set for `plan`.
fn render_rules(plan: &Plan) -> String {
    let mut rules = String::from("# cvbbmso egress guard\n");
    rules.push_str("pass out quick on lo0 inet from any to 127.0.0.1 no state\npass out quick on lo0 inet6 from any to ::1 no state\n");

    rules.push_str(concat!(
        "pass out quick inet6 proto ipv6-icmp from any to ff02::2 icmp6-type routersol no state\n",
        "pass out quick inet6 proto ipv6-icmp from any to ff02::1:ff00:0/104 icmp6-type neighbrsol no state\n",
        "pass out quick inet6 proto ipv6-icmp from any to ff02::1 icmp6-type neighbradv no state\n",
        "pass out quick inet6 proto ipv6-icmp from any to fe80::/10 icmp6-type { neighbrsol neighbradv } no state\n",
    ));
    for service in &plan.services {
        let protocol = match service.transport {
            Transport::Tcp => "tcp",
            Transport::Udp => "udp",
        };
        push_pass(
            &mut rules,
            &service.remote.ip(),
            service.remote.port(),
            protocol,
        );
    }
    for target in [&plan.accoding, &plan.server] {
        for address in &target.addresses {
            push_pass(&mut rules, address, target.port, "{ tcp udp }");
        }
    }
    for address in &plan.certificate {
        push_pass(&mut rules, &address.ip(), address.port(), "tcp");
    }
    rules.push_str("block drop out quick all\n");
    rules
}

/// Appends one pass rule for TCP and UDP towards `address:port`.
fn push_pass(rules: &mut String, address: &IpAddr, port: u16, protocol: &str) {
    let family = if address.is_ipv4() { "inet" } else { "inet6" };
    let rule = format!(
        "pass out quick {family} proto {protocol} from any to {address} port {port} no state\n"
    );
    rules.push_str(&rule);
}

fn local_addresses() -> Result<Vec<IpAddr>, NetworkError> {
    let mut head = std::ptr::null_mut();
    // SAFETY: getifaddrs initializes head on success, and its allocation is
    // valid until freeifaddrs. Every node is inspected within that lifetime.
    if unsafe { libc::getifaddrs(&raw mut head) } != 0 {
        return Err(NetworkError::Platform);
    }
    let mut addresses = Vec::new();
    let mut next = head;
    while !next.is_null() {
        // SAFETY: next is a live node from the OS-owned list. The address
        // family determines the concrete type of each non-null socket address.
        let interface = unsafe { &*next };
        let address = interface.ifa_addr;
        if !address.is_null() {
            match i32::from(unsafe { (*address).sa_family }) {
                libc::AF_INET => {
                    // SAFETY: this address was reported as AF_INET.
                    let address = unsafe { address.cast::<libc::sockaddr_in>().read_unaligned() };
                    addresses.push(Ipv4Addr::from(address.sin_addr.s_addr.to_ne_bytes()).into());
                }
                libc::AF_INET6 => {
                    // SAFETY: this address was reported as AF_INET6.
                    let address = unsafe { address.cast::<libc::sockaddr_in6>().read_unaligned() };
                    let mut octets = address.sin6_addr.s6_addr;
                    // Darwin embeds interface scope in bytes 2..4 for link-local addresses.
                    if Ipv6Addr::from(octets).is_unicast_link_local() {
                        octets[2..4].fill(0);
                    }
                    addresses.push(Ipv6Addr::from(octets).into());
                }
                _ => {}
            }
        }
        next = interface.ifa_next;
    }
    // SAFETY: head is the original pointer returned by getifaddrs.
    unsafe { libc::freeifaddrs(head) };
    addresses.sort();
    addresses.dedup();
    Ok(addresses)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::egress::commands::testing::{Call, Recorder};
    use crate::egress::policy::{Service, Target};

    const PF_CONF: &str = "/etc/pf.conf";
    const BASE: &str = "# on-disk PF configuration\nanchor \"com.cvbbmso.egress\" out quick\n";
    const LIVE_ROOT: &str = concat!(
        "anchor \"com.cvbbmso.egress\" out quick all\n",
        "anchor \"system.keep/*\" all\n",
        "block drop in on en0 all\n",
    );
    const BLOCK: &str = "block drop out quick all";
    const PARSE: &[&str] = &["-a", ANCHOR, "-n", "-v", "-o", "none", "-f", RULES];
    const LIVE: &[&str] = &["-a", ANCHOR, "-s", "rules"];
    const ROOT: &[&str] = &["-s", "rules"];

    fn plan() -> Plan {
        let ip = |text: &str| text.parse::<IpAddr>().unwrap();
        Plan::new(
            Target::new(vec![ip("203.0.113.10")], 443),
            Target::new(vec![ip("2001:db8::1")], 8443),
            vec!["203.0.113.20:80".parse().unwrap()],
            vec![
                Service::new(ip("192.0.2.53"), 53, Transport::Tcp),
                Service::new(ip("192.0.2.53"), 53, Transport::Udp),
                Service::new(ip("192.0.2.67"), 67, Transport::Udp),
                Service::new(ip("192.0.2.123"), 123, Transport::Udp),
            ],
        )
    }

    fn healthy(recorder: &Recorder, live: &str) {
        recorder.reply(&["-s", "info"], Ok("Status: Enabled for 0 days"));
        recorder.reply(ROOT, Ok(LIVE_ROOT));
        recorder.reply(LIVE, Ok(live));
        recorder.reply(&["-s", "References"], Ok("202 pfctl 42 0 days"));
    }

    #[test]
    fn install_confirms_the_reference_and_rechecks_existing_outbound_flows() {
        let recorder = Recorder::with_file(PF_CONF, BASE);
        recorder.reply(ROOT, Ok(LIVE_ROOT));
        recorder.reply(PARSE, Ok(BLOCK));
        recorder.reply(&["-E"], Ok("pf enabled\nToken : 42\n"));
        healthy(&recorder, BLOCK);

        let local = "192.0.2.123".parse().unwrap();
        install_with(&recorder, &plan(), &[local]).unwrap();
        assert_eq!(recorder.contents(PF_CONF).as_deref(), Some(BASE));
        assert_only_own_mutations(&recorder);
        assert_eq!(read_token(&recorder).unwrap(), Some(42));
        assert_eq!(
            recorder.calls().last(),
            Some(&Call::Run(
                PFCTL.to_owned(),
                vec!["-k".into(), local.to_string()]
            )),
        );
    }

    #[test]
    fn failed_enable_verification_leaves_a_recoverable_reference() {
        let recorder = Recorder::with_file(PF_CONF, BASE);
        recorder.reply(ROOT, Ok(LIVE_ROOT));
        recorder.reply(PARSE, Ok(BLOCK));
        recorder.reply(&["-E"], Ok("pf enabled\nToken : 42\n"));
        recorder.reply(&["-s", "info"], Ok("Status: Disabled"));
        recorder.reply(ROOT, Ok(LIVE_ROOT));
        recorder.reply(LIVE, Ok(BLOCK));
        recorder.reply(&["-s", "References"], Ok("202 pfctl 42 0 days"));

        assert_eq!(
            install_with(&recorder, &plan(), &[]).err(),
            Some(NetworkError::Platform)
        );

        recorder.reply(LIVE, Ok(BLOCK));
        lifecycle::recover(&recorder).unwrap();
        assert_only_own_mutations(&recorder);
        assert_eq!(recorder.contents(PF_CONF).as_deref(), Some(BASE));
        assert!(recorder.contents(TOKEN).is_none());
        assert!(recorder.contents(RULES).is_none());
        assert!(
            recorder
                .calls()
                .contains(&Call::Run(PFCTL.to_owned(), vec!["-X".into(), "42".into()]))
        );
    }

    #[test]
    fn a_rejected_anchor_refresh_leaves_the_current_rules() {
        let recorder = Recorder::with_file(TOKEN, "Token : 42\n");
        let mut expected = Expected {
            rendered: BLOCK.to_owned(),
            kernel: BLOCK.to_owned(),
        };
        recorder.reply(PARSE, Ok(BLOCK));
        recorder.reply(
            &["-a", ANCHOR, "-o", "none", "-f", RULES],
            Err(NetworkError::Platform),
        );
        assert_eq!(
            replace_with(&recorder, &mut expected, format!("# next\n{BLOCK}")),
            Err(NetworkError::Platform)
        );
        assert_eq!(expected.rendered, BLOCK);
        let calls = recorder.calls();
        assert_eq!(calls.len(), 2);
        assert!(calls.iter().all(|call| match call {
            Call::Run(_, args) | Call::Output(_, args) =>
                args.starts_with(&["-a".into(), ANCHOR.into()]),
        }));
    }

    #[test]
    fn verification_detects_partial_rules_or_a_missing_enable_reference() {
        let recorder = Recorder::with_file(TOKEN, "Token : 42\n");
        healthy(&recorder, "pass out quick on lo0 all");
        assert_eq!(verify_with(&recorder, BLOCK), Err(NetworkError::Platform));
        healthy(&recorder, BLOCK);
        assert_eq!(verify_with(&recorder, BLOCK), Ok(()));
        healthy(&recorder, BLOCK);
        recorder.remove(TOKEN).unwrap();
        assert_eq!(verify_with(&recorder, BLOCK), Err(NetworkError::Platform));
    }

    #[test]
    fn rules_parse_with_pf_and_keep_service_permissions_narrow() {
        let rules = render_rules(&plan());
        assert!(rules.contains("proto tcp from any to 192.0.2.53 port 53"));
        assert!(rules.contains("proto udp from any to 192.0.2.53 port 53"));
        assert!(rules.contains("proto udp from any to 192.0.2.67 port 67"));
        assert!(rules.contains("proto udp from any to 192.0.2.123 port 123"));
        assert!(rules.contains("proto tcp from any to 203.0.113.20 port 80"));
        assert!(!rules.contains("to any port"));
        assert!(!rules.contains("udp from any to 203.0.113.20"));
        let directory = tempfile::tempdir().unwrap();
        let rules_path = directory.path().join("guard.rules");
        std::fs::write(&rules_path, rules).unwrap();
        let output = std::process::Command::new(PFCTL)
            .args(["-a", ANCHOR, "-n", "-v", "-o", "none", "-f"])
            .arg(rules_path)
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        let printed = String::from_utf8(output.stdout).unwrap();
        assert_eq!(printed.trim().lines().last(), Some(BLOCK));

        let root_path = directory.path().join("pf.conf");
        std::fs::write(root_path.as_path(), BASE).unwrap();
        let root = std::process::Command::new(PFCTL)
            .args(["-n", "-v", "-f"])
            .arg(root_path)
            .output()
            .unwrap();
        assert!(
            root.status.success(),
            "{}",
            String::from_utf8_lossy(&root.stderr)
        );
        assert_eq!(
            String::from_utf8(root.stdout).unwrap().lines().next(),
            Some(ROOT_RULE)
        );
    }

    fn assert_only_own_mutations(recorder: &Recorder) {
        for item in recorder.calls() {
            if let Call::Run(program, args) = item
                && program == PFCTL
                && args.iter().any(|arg| matches!(arg.as_str(), "-f" | "-F"))
            {
                assert_eq!(&args[..2], &["-a", ANCHOR]);
            }
        }
    }

    #[test]
    fn an_on_disk_hook_cannot_make_an_incompatible_live_ruleset_safe() {
        let recorder = Recorder::with_file(PF_CONF, BASE);
        let live = "anchor \"system.keep/*\" all\npass out quick all";
        recorder.reply(ROOT, Ok(live));
        assert_eq!(
            install_with(&recorder, &plan(), &[]).err(),
            Some(NetworkError::Platform)
        );
        assert_eq!(recorder.contents(PF_CONF).as_deref(), Some(BASE));
        assert!(recorder.contents(RULES).is_none());
        assert!(recorder.contents(TOKEN).is_none());
        assert!(
            recorder
                .calls()
                .iter()
                .all(|call| matches!(call, Call::Output(..)))
        );
    }

    #[test]
    fn recovery_preserves_an_anchor_without_a_project_journal() {
        let recorder = Recorder::with_file(PF_CONF, BASE);
        recorder.reply(LIVE, Ok("pass out all"));
        assert_eq!(recover_with(&recorder), Err(NetworkError::Platform));
        assert_eq!(recorder.contents(PF_CONF).as_deref(), Some(BASE));
        assert!(
            recorder
                .calls()
                .iter()
                .all(|call| matches!(call, Call::Output(..)))
        );
    }
}
