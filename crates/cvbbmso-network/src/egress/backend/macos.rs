//! macOS PF management. Deployment provisions the main ruleset hook. The
//! guard loads and flushes only its reserved outbound anchor.
#![allow(
    unsafe_code,
    reason = "PF needs native privilege and interface queries"
)]

use std::collections::BTreeSet;
use std::net::{IpAddr, Ipv4Addr, Ipv6Addr};
use std::sync::Mutex;

use crate::NetworkError;
use crate::egress::backend::{GuardBackend, InstallResult};
use crate::egress::commands::Commands;
use crate::egress::{policy::Plan, state};

mod lifecycle;
mod rules;
use lifecycle::Prepared;
pub(crate) use lifecycle::{HELPER, watchdog_entry};
use rules::Rules;

const ANCHOR: &str = "com.cvbbmso.egress";
const RULES: &str = "/Library/Application Support/com.cvbbmso/pf.rules";
const TOKEN: &str = "/Library/Application Support/com.cvbbmso/pf.enable";
const PFCTL: &str = "/sbin/pfctl";
const ROOT_RULE: &str = "anchor \"com.cvbbmso.egress\" out all";
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
    rules: Rules,
    kernel: String,
}

impl GuardBackend for PfGuard {
    fn apply(&self, plan: &Plan) -> Result<(), NetworkError> {
        let lock = state::lock()?;
        let commands = crate::egress::commands::SystemCommands(Some(&lock));
        self.verify_owner()?;
        let mut expected = self.expected.lock().map_err(|_| NetworkError::Platform)?;
        verify_with(&commands, &expected)?;
        replace_with(&commands, &mut expected, Rules::new(plan), local_addresses)
    }

    fn verify(&self) -> Result<(), NetworkError> {
        let lock = state::lock()?;
        let commands = crate::egress::commands::SystemCommands(Some(&lock));
        let mut watchdog = self.watchdog.lock().map_err(|_| NetworkError::Platform)?;
        let watchdog = watchdog.as_mut().ok_or(NetworkError::Platform)?;
        let mut guard_state = watchdog.check()?;
        let expected = self.expected.lock().map_err(|_| NetworkError::Platform)?;
        verify_with(&commands, &expected)?;
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
    let expected = stage(commands, Rules::new(plan))?;
    load_anchor(commands)?;
    commands.capture(PFCTL, &["-E"], TOKEN)?;
    read_token(commands)?.ok_or(NetworkError::Platform)?;
    verify_with(commands, &expected)?;
    // A PF state predating the anchor would otherwise pass without matching
    // the new rules. Only remove states whose initiating address is local.
    for ip in local.iter().filter(|ip| !ip.is_loopback()) {
        commands.run(PFCTL, &["-k", &ip.to_string()])?;
    }
    Ok(expected)
}

fn stage(commands: &dyn Commands, rules: Rules) -> Result<Expected, NetworkError> {
    commands.write(RULES, &rules.source)?;
    let output = commands.output(
        PFCTL,
        &["-a", ANCHOR, "-n", "-v", "-o", "none", "-f", RULES],
    )?;
    let filter = output
        .lines()
        .filter(|line| !line.starts_with("table ") && !line.trim().is_empty())
        .collect::<Vec<_>>();
    if filter.is_empty()
        || filter
            .iter()
            .any(|line| !line.starts_with("block drop out quick "))
    {
        return Err(NetworkError::Platform);
    }
    Ok(Expected {
        rules,
        kernel: filter.join("\n"),
    })
}

fn replace_with(
    commands: &dyn Commands,
    expected: &mut Expected,
    rules: Rules,
    local: impl FnOnce() -> Result<Vec<IpAddr>, NetworkError>,
) -> Result<(), NetworkError> {
    if rules != expected.rules {
        let revoke = expected.rules.revoked_destinations(&rules);
        let next = stage(commands, rules)?;
        load_anchor(commands)?;
        let checked = verify_with(commands, &next).and_then(|()| {
            if !revoke.is_empty() {
                // Host pass rules may have created states for previously
                // allowed ports. PF checks those states before evaluating rules.
                for source in local()?.into_iter().filter(|source| !source.is_loopback()) {
                    for target in revoke.iter().filter(|ip| ip.is_ipv4() == source.is_ipv4()) {
                        commands.run(
                            PFCTL,
                            &["-k", &source.to_string(), "-k", &target.to_string()],
                        )?;
                    }
                }
            }
            Ok(())
        });
        if let Err(error) = checked {
            commands.write(RULES, &expected.rules.source)?;
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

fn first_filter_line(stdout: &str) -> Option<&str> {
    // pfctl can print packet-normalization rules ahead of the filter rules.
    // Keep unrecognized lines so the anchor check fails closed.
    stdout.lines().map(str::trim).find(|line| {
        if *line == "FILTER RULES:" {
            return false;
        }
        let mut words = line.split_ascii_whitespace();
        !matches!(
            (words.next(), words.next(), words.next()),
            (None, _, _)
                | (Some("scrub" | "scrub-anchor"), Some(_), _)
                | (Some("no"), Some("scrub"), Some(_))
        )
    })
}

fn verify_deployment(commands: &dyn Commands) -> Result<(), NetworkError> {
    let root = commands.output(PFCTL, &["-s", "rules"])?;
    let interfaces = commands.output(PFCTL, &["-vv", "-s", "Interfaces"])?;
    let translations = commands.output(PFCTL, &["-a", "*", "-s", "nat"])?;
    let skipped = interfaces.lines().any(|line| {
        let mut words = line.split_whitespace();
        words.next().is_some_and(|name| name != "lo0") && words.any(|word| word == "(skip)")
    });
    let skips_filtering = translations.lines().any(|line| {
        let mut fields = line.split_whitespace();
        matches!(
            (fields.next(), fields.next()),
            (Some("nat" | "rdr" | "binat"), Some("pass"))
        )
    });
    // Translation rules using 'pass' can terminate processing before the
    // filter stage, even when the guard is the first filter rule.
    if first_filter_line(&root) != Some(ROOT_RULE) || skipped || skips_filtering {
        return Err(NetworkError::Platform);
    }
    Ok(())
}

fn verify_with(commands: &dyn Commands, expected: &Expected) -> Result<(), NetworkError> {
    verify_deployment(commands)?;
    let info = commands.output(PFCTL, &["-s", "info"])?;
    let live = commands.output(PFCTL, &["-a", ANCHOR, "-s", "rules"])?;
    let reference = read_token(commands)?.ok_or(NetworkError::Platform)?;
    let references = commands.output(PFCTL, &["-s", "References"])?;
    if !info.lines().any(|line| line.starts_with("Status: Enabled"))
        || live.trim() != expected.kernel
        || !has_reference(&references, reference)
    {
        return Err(NetworkError::Platform);
    }
    let tables = commands.output(PFCTL, &["-a", ANCHOR, "-s", "Tables"])?;
    if tables.split_whitespace().collect::<BTreeSet<_>>()
        != expected.rules.tables.keys().map(String::as_str).collect()
    {
        return Err(NetworkError::Platform);
    }
    for (table, addresses) in &expected.rules.tables {
        let live = commands.output(PFCTL, &["-a", ANCHOR, "-t", table, "-T", "show"])?;
        let live = live
            .lines()
            .map(str::trim)
            .filter(|line| !line.is_empty())
            .map(normalize_address)
            .collect::<Result<BTreeSet<_>, _>>()?;
        if &live != addresses {
            return Err(NetworkError::Platform);
        }
    }
    Ok(())
}

fn normalize_address(address: &str) -> Result<String, NetworkError> {
    let (ip, prefix) = match address.split_once('/') {
        Some((ip, prefix)) => (
            ip,
            Some(prefix.parse::<u8>().map_err(|_| NetworkError::Platform)?),
        ),
        None => (address, None),
    };
    let ip = ip.parse::<IpAddr>().map_err(|_| NetworkError::Platform)?;
    match prefix {
        Some(prefix) if prefix != if ip.is_ipv4() { 32 } else { 128 } => {
            Ok(format!("{ip}/{prefix}"))
        }
        _ => Ok(ip.to_string()),
    }
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
    let tables = commands.output(PFCTL, &["-a", ANCHOR, "-s", "Tables"])?;
    if !live.trim().is_empty() || !tables.trim().is_empty() {
        // A load always writes the protected journal before changing PF.
        // Existing rules without that journal cannot be attributed to us.
        commands.read(RULES)?.ok_or(NetworkError::Platform)?;
        if !live.trim().is_empty() {
            commands.run(PFCTL, &["-a", ANCHOR, "-F", "rules"])?;
        }
        if !tables.trim().is_empty() {
            commands.run(PFCTL, &["-a", ANCHOR, "-F", "Tables"])?;
        }
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
    use crate::egress::policy::{Service, Target, Transport};

    const PF_CONF: &str = "/etc/pf.conf";
    const BASE: &str = "# on-disk PF configuration\nanchor \"com.cvbbmso.egress\" out\n";
    const LIVE_ROOT: &str = concat!(
        "scrub-anchor \"com.apple/*\" all fragment reassemble\n",
        "anchor \"com.cvbbmso.egress\" out all\n",
        "anchor \"system.keep/*\" all\n",
        "block drop out quick inet proto tcp from any to 203.0.113.10 port = 443\n",
        "pass out quick all\n",
    );
    const BLOCK: &str = "block drop out quick from any to ! <cvbbmso_destinations>";
    const PARSE: &[&str] = &["-a", ANCHOR, "-n", "-v", "-o", "none", "-f", RULES];
    const LIVE: &[&str] = &["-a", ANCHOR, "-s", "rules"];
    const ROOT: &[&str] = &["-s", "rules"];
    const TABLES: &[&str] = &["-a", ANCHOR, "-s", "Tables"];

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

    fn healthy(recorder: &Recorder, live: &str, rules: &Rules) {
        recorder.reply(&["-s", "info"], Ok("Status: Enabled for 0 days"));
        recorder.reply(ROOT, Ok(LIVE_ROOT));
        recorder.reply(LIVE, Ok(live));
        recorder.reply(&["-s", "References"], Ok("202 pfctl 42 0 days"));
        recorder.reply(
            TABLES,
            Ok(&rules.tables.keys().cloned().collect::<Vec<_>>().join("\n")),
        );
        for (name, addresses) in &rules.tables {
            recorder.reply(
                &["-a", ANCHOR, "-t", name, "-T", "show"],
                Ok(&addresses.iter().cloned().collect::<Vec<_>>().join("\n")),
            );
        }
    }

    #[test]
    fn install_confirms_the_reference_and_rechecks_existing_outbound_flows() {
        let recorder = Recorder::with_file(PF_CONF, BASE);
        recorder.reply(ROOT, Ok(LIVE_ROOT));
        recorder.reply(PARSE, Ok(BLOCK));
        recorder.reply(&["-E"], Ok("pf enabled\nToken : 42\n"));
        healthy(&recorder, BLOCK, &Rules::new(&plan()));

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
            rules: Rules::new(&plan()),
            kernel: BLOCK.to_owned(),
        };
        recorder.reply(PARSE, Ok(BLOCK));
        recorder.reply(
            &["-a", ANCHOR, "-o", "none", "-f", RULES],
            Err(NetworkError::Platform),
        );
        assert_eq!(
            replace_with(
                &recorder,
                &mut expected,
                Rules::new(&Plan::default()),
                || Ok(Vec::new())
            ),
            Err(NetworkError::Platform)
        );
        assert!(expected.rules == Rules::new(&plan()));
        let calls = recorder.calls();
        assert_eq!(calls.len(), 2);
        assert!(calls.iter().all(|call| match call {
            Call::Run(_, args) | Call::Output(_, args) =>
                args.starts_with(&["-a".into(), ANCHOR.into()]),
        }));
    }

    #[test]
    fn verification_detects_partial_rules_or_a_missing_enable_reference() {
        let expected = Expected {
            rules: Rules::new(&plan()),
            kernel: BLOCK.to_owned(),
        };

        let recorder = Recorder::with_file(TOKEN, "Token : 42\n");
        healthy(&recorder, "pass out quick on lo0 all", &expected.rules);
        assert_eq!(
            verify_with(&recorder, &expected),
            Err(NetworkError::Platform)
        );

        let recorder = Recorder::with_file(TOKEN, "Token : 42\n");
        healthy(&recorder, BLOCK, &expected.rules);
        assert_eq!(verify_with(&recorder, &expected), Ok(()));
        healthy(&recorder, BLOCK, &expected.rules);
        recorder.remove(TOKEN).unwrap();
        assert_eq!(
            verify_with(&recorder, &expected),
            Err(NetworkError::Platform)
        );

        let recorder = Recorder::with_file(TOKEN, "Token : 42\n");
        recorder.reply(
            &["-a", ANCHOR, "-t", "cvbbmso_destinations", "-T", "show"],
            Ok("192.0.2.99"),
        );
        healthy(&recorder, BLOCK, &expected.rules);
        assert_eq!(
            verify_with(&recorder, &expected),
            Err(NetworkError::Platform)
        );
    }

    #[test]
    fn refresh_rechecks_states_for_revoked_destinations() {
        let recorder = Recorder::with_file(TOKEN, "Token : 42\n");
        let mut expected = Expected {
            rules: Rules::new(&plan()),
            kernel: BLOCK.to_owned(),
        };
        let mut next = plan();
        next.services.retain(|service| service.remote.port() != 67);
        let next = Rules::new(&next);
        recorder.reply(PARSE, Ok(BLOCK));
        healthy(&recorder, BLOCK, &next);
        replace_with(&recorder, &mut expected, next, || {
            Ok(vec!["192.0.2.2".parse().unwrap()])
        })
        .unwrap();
        assert_eq!(
            recorder.calls().last(),
            Some(&Call::Run(
                PFCTL.to_owned(),
                vec![
                    "-k".into(),
                    "192.0.2.2".into(),
                    "-k".into(),
                    "192.0.2.67".into()
                ]
            )),
        );
    }

    #[test]
    fn rules_parse_as_terminal_drops_and_preserve_the_hosts_decisions() {
        let rules = Rules::new(&plan());
        assert!(rules.tables["cvbbmso_tcp_53"].contains("192.0.2.53"));
        assert!(rules.tables["cvbbmso_udp_53"].contains("192.0.2.53"));
        assert!(rules.tables["cvbbmso_udp_67"].contains("192.0.2.67"));
        assert!(rules.tables["cvbbmso_udp_123"].contains("192.0.2.123"));
        assert!(rules.tables["cvbbmso_tcp_80"].contains("203.0.113.20"));
        assert!(!rules.tables.contains_key("cvbbmso_udp_80"));
        assert!(rules.tables["cvbbmso_tcp_443"].contains("203.0.113.10"));
        assert!(!rules.tables["cvbbmso_tcp_443"].contains("192.0.2.99"));
        assert!(
            rules
                .source
                .contains("proto tcp from any to ! <cvbbmso_tcp_443> port 443")
        );
        assert!(
            rules
                .source
                .contains("port { 0:52 54:79 81:442 444:8442 8444:65535 }")
        );
        let directory = tempfile::tempdir().unwrap();
        let rules_path = directory.path().join("guard.rules");
        std::fs::write(&rules_path, rules.source).unwrap();
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
        let filters = printed
            .lines()
            .filter(|line| !line.starts_with("table "))
            .collect::<Vec<_>>();
        assert!(filters.contains(&BLOCK));
        assert!(
            filters
                .iter()
                .all(|line| line.starts_with("block drop out quick "))
        );

        let root_path = directory.path().join("pf.conf");
        let host_policy = format!(
            "scrub in all fragment reassemble\nno scrub out all\n{BASE}block drop out quick proto tcp from any to 203.0.113.10 port 443\npass out quick all\n"
        );
        std::fs::write(root_path.as_path(), host_policy).unwrap();
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
        let live_root = String::from_utf8(root.stdout).unwrap();
        assert!(live_root.starts_with("scrub in all"));
        assert_eq!(first_filter_line(&live_root), Some(ROOT_RULE));
        assert!(
            live_root.contains(
                "block drop out quick inet proto tcp from any to 203.0.113.10 port = 443"
            )
        );
        assert!(live_root.contains("pass out quick all"));
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
        let live = "anchor \"com.cvbbmso.egress\" out quick all\npass out quick all";
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
    fn deployment_accepts_a_filter_header_and_normalization_rules() {
        let recorder = Recorder::default();
        let live = format!(
            " \nFILTER RULES:\n\nscrub in all fragment reassemble\nno scrub out all\n{LIVE_ROOT}"
        );
        recorder.reply(ROOT, Ok(&live));

        assert_eq!(verify_deployment(&recorder), Ok(()));
    }

    #[test]
    fn deployment_rejects_any_other_line_before_the_guard_filter() {
        for line in [
            "pass out quick all",
            "block drop out quick all",
            "match out all",
            "anchor \"system.keep/*\" all",
            "scrub-anchor-unknown \"system.keep/*\" all",
            "no nat on en0 all",
            "UNKNOWN HEADER:",
        ] {
            let recorder = Recorder::with_file(PF_CONF, BASE);
            let live =
                format!("FILTER RULES:\nscrub in all fragment reassemble\n{line}\n{LIVE_ROOT}");
            recorder.reply(ROOT, Ok(&live));

            assert_eq!(
                install_with(&recorder, &plan(), &[]).err(),
                Some(NetworkError::Platform),
                "{line}",
            );
            assert!(recorder.contents(RULES).is_none(), "{line}");
            assert!(
                recorder
                    .calls()
                    .iter()
                    .all(|call| matches!(call, Call::Output(..))),
                "{line}",
            );
        }
    }

    #[test]
    fn deployment_rejects_translation_that_skips_filtering() {
        let recorder = Recorder::with_file(PF_CONF, BASE);
        recorder.reply(ROOT, Ok(LIVE_ROOT));
        recorder.reply(&["-a", "*", "-s", "nat"], Ok("nat-anchor \"system/*\" all {\n  nat pass on en0 inet from 192.0.2.0/24 to any -> 203.0.113.1\n}"));
        assert_eq!(
            install_with(&recorder, &plan(), &[]).err(),
            Some(NetworkError::Platform)
        );
        assert!(recorder.contents(RULES).is_none());
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
