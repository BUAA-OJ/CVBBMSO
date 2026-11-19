//! PF adapter for macOS.
//!
//! The guard is a PF anchor loaded from `/etc/pf.anchors/com.cvbbmso.egress` and
//! referenced from `/etc/pf.conf` inside a marked block. Both files are restored
//! by [`recover`], which the watchdog and the TTL cleanup task use.
#![allow(
    unsafe_code,
    reason = "the adapter reads the effective user id through libc"
)]

use std::net::IpAddr;
use std::sync::Arc;

use crate::NetworkError;
use crate::egress::backend::{GuardBackend, InstallResult};
use crate::egress::commands::{Commands, SystemCommands};
use crate::egress::policy::Plan;

/// PF anchor that holds the guard rules.
const ANCHOR: &str = "com.cvbbmso.egress";
/// File PF loads the anchor from.
const ANCHOR_FILE: &str = "/etc/pf.anchors/com.cvbbmso.egress";
/// Main PF configuration that must reference the anchor.
const PF_CONF: &str = "/etc/pf.conf";
/// PF control program.
const PFCTL: &str = "/sbin/pfctl";
/// First marker line of the block inserted into `pf.conf`.
const BLOCK_BEGIN: &str = "# BEGIN cvbbmso egress guard";
/// Last marker line of the block inserted into `pf.conf`.
const BLOCK_END: &str = "# END cvbbmso egress guard";
/// Ports used to fetch certificate status.
const CERTIFICATE_PORTS: [u16; 2] = [80, 443];
/// Datagram services an exam client needs: DHCP and NTP.
const DATAGRAM_PORTS: [u16; 3] = [67, 68, 123];

/// Installs the PF anchor and returns a handle to it.
///
/// # Errors
///
/// [`NetworkError::PermissionDenied`] without root rights, and
/// [`NetworkError::Platform`] or [`NetworkError::Io`] when PF or the
/// configuration files cannot be updated.
pub(crate) fn install(plan: &Plan) -> Result<InstallResult, NetworkError> {
    require_root()?;
    apply_with(&SystemCommands, plan)?;
    Ok(Arc::new(PfGuard))
}

/// Removes every trace of the guard. Used by the watchdog and the TTL task.
///
/// # Errors
///
/// [`NetworkError::PermissionDenied`] without root rights, and
/// [`NetworkError::Platform`] when PF cannot be reloaded.
pub(crate) fn recover() -> Result<(), NetworkError> {
    require_root()?;
    recover_with(&SystemCommands)
}

/// Handle to an installed PF anchor.
struct PfGuard;

impl GuardBackend for PfGuard {
    fn apply(&self, plan: &Plan) -> Result<(), NetworkError> {
        apply_with(&SystemCommands, plan)
    }

    fn verify(&self) -> Result<(), NetworkError> {
        verify_with(&SystemCommands)
    }

    fn revert(&self) -> Result<(), NetworkError> {
        recover_with(&SystemCommands)
    }
}

/// Writes the anchor, makes `pf.conf` reference it, and reloads PF.
fn apply_with(commands: &dyn Commands, plan: &Plan) -> Result<(), NetworkError> {
    commands.write(ANCHOR_FILE, &render_rules(plan))?;

    let conf = commands.read(PF_CONF)?.unwrap_or_default();
    if !conf.contains(BLOCK_BEGIN) {
        let mut updated = conf;
        if !updated.is_empty() && !updated.ends_with('\n') {
            updated.push('\n');
        }
        let block = format!(
            "{BLOCK_BEGIN}\nanchor \"{ANCHOR}\"\nload anchor \"{ANCHOR}\" from \"{ANCHOR_FILE}\"\n{BLOCK_END}\n"
        );
        updated.push_str(&block);
        commands.write(PF_CONF, &updated)?;
    }

    commands.run(PFCTL, &["-f", PF_CONF])
}

/// Confirms that `pf.conf` still references the anchor and that PF loaded rules.
fn verify_with(commands: &dyn Commands) -> Result<(), NetworkError> {
    let conf = commands.read(PF_CONF)?.unwrap_or_default();
    if !conf.contains(BLOCK_BEGIN) {
        return Err(NetworkError::Platform);
    }
    let rules = commands.output(PFCTL, &["-a", ANCHOR, "-s", "rules"])?;
    if rules.trim().is_empty() {
        return Err(NetworkError::Platform);
    }
    Ok(())
}

/// Flushes the anchor, removes the `pf.conf` block and the anchor file.
fn recover_with(commands: &dyn Commands) -> Result<(), NetworkError> {
    commands.run(PFCTL, &["-a", ANCHOR, "-F", "all"])?;
    commands.remove(ANCHOR_FILE)?;

    let conf = commands.read(PF_CONF)?.unwrap_or_default();
    if conf.contains(BLOCK_BEGIN) {
        commands.write(PF_CONF, &strip_block(&conf))?;
        commands.run(PFCTL, &["-f", PF_CONF])?;
    }
    Ok(())
}

/// Removes the marked block from `pf.conf` contents, keeping everything else.
fn strip_block(conf: &str) -> String {
    let mut kept = String::with_capacity(conf.len());
    let mut inside = false;
    for line in conf.lines() {
        let trimmed = line.trim();
        if trimmed == BLOCK_BEGIN {
            inside = true;
            continue;
        }
        if trimmed == BLOCK_END {
            inside = false;
            continue;
        }
        if !inside {
            kept.push_str(line);
            kept.push('\n');
        }
    }
    kept
}

/// Renders the complete PF rule set for `plan`.
fn render_rules(plan: &Plan) -> String {
    let mut rules = String::from("# cvbbmso egress guard\n");
    rules.push_str("pass out quick on lo0 all\n");

    for resolver in &plan.resolvers {
        push_pass(&mut rules, resolver, 53);
    }
    for port in DATAGRAM_PORTS {
        let rule = format!("pass out quick proto udp from any to any port {port}\n");
        rules.push_str(&rule);
    }
    for target in [&plan.accoding, &plan.server] {
        for address in &target.addresses {
            push_pass(&mut rules, address, target.port);
        }
    }
    for address in &plan.certificate {
        for port in CERTIFICATE_PORTS {
            push_pass(&mut rules, address, port);
        }
    }

    rules.push_str("block drop out all\n");
    rules
}

/// Appends one pass rule for TCP and UDP towards `address:port`.
fn push_pass(rules: &mut String, address: &IpAddr, port: u16) {
    let family = if address.is_ipv4() { "inet" } else { "inet6" };
    let rule =
        format!("pass out quick {family} proto {{ tcp udp }} from any to {address} port {port}\n");
    rules.push_str(&rule);
}

/// Fails unless the process runs with root rights.
fn require_root() -> Result<(), NetworkError> {
    // SAFETY: `geteuid` has no preconditions and cannot fail.
    let effective = unsafe { libc::geteuid() };
    if effective == 0 {
        Ok(())
    } else {
        Err(NetworkError::PermissionDenied)
    }
}

#[cfg(test)]
mod tests {
    use super::{
        ANCHOR_FILE, BLOCK_BEGIN, BLOCK_END, PF_CONF, apply_with, recover_with, render_rules,
        strip_block, verify_with,
    };
    use crate::NetworkError;
    use crate::egress::commands::testing::{Call, Recorder};
    use crate::egress::policy::{Plan, Target};
    use std::net::{IpAddr, Ipv4Addr, Ipv6Addr};

    fn plan() -> Plan {
        Plan::new(
            Target::new(vec![IpAddr::V4(Ipv4Addr::new(203, 0, 113, 10))], 443),
            Target::new(vec![IpAddr::V6(Ipv6Addr::LOCALHOST)], 8443),
            vec![IpAddr::V4(Ipv4Addr::new(203, 0, 113, 20))],
            vec![IpAddr::V4(Ipv4Addr::new(192, 0, 2, 53))],
        )
    }

    #[test]
    fn rules_allow_the_configured_endpoints_only() {
        let rules = render_rules(&plan());
        assert!(rules.contains("pass out quick on lo0 all"));
        assert!(
            rules.contains(
                "pass out quick inet proto { tcp udp } from any to 203.0.113.10 port 443"
            )
        );
        assert!(rules.contains("pass out quick inet6 proto { tcp udp } from any to ::1 port 8443"));
        assert!(rules.contains("from any to 192.0.2.53 port 53"));
        assert!(rules.contains("from any to any port 123"));
        assert!(rules.contains("from any to 203.0.113.20 port 443"));
        assert!(rules.trim_end().ends_with("block drop out all"));
    }

    #[test]
    fn installation_writes_the_anchor_and_references_it_once() {
        let recorder = Recorder::default();
        apply_with(&recorder, &plan()).expect("apply succeeds");

        let anchor = recorder.contents(ANCHOR_FILE).expect("anchor file");
        assert!(anchor.contains("block drop out all"));

        let conf = recorder.contents(PF_CONF).expect("pf.conf");
        assert!(conf.contains(BLOCK_BEGIN));
        assert_eq!(conf.matches(BLOCK_BEGIN).count(), 1);

        // Applying twice does not duplicate the block.
        apply_with(&recorder, &plan()).expect("second apply succeeds");
        let conf = recorder.contents(PF_CONF).expect("pf.conf");
        assert_eq!(conf.matches(BLOCK_BEGIN).count(), 1);

        assert!(
            recorder
                .calls()
                .iter()
                .any(|call| matches!(call, Call::Run(program, _) if program == "/sbin/pfctl"))
        );
    }

    #[test]
    fn recovery_removes_the_block_and_the_anchor() {
        let recorder = Recorder::default();
        apply_with(&recorder, &plan()).expect("apply succeeds");
        recover_with(&recorder).expect("recover succeeds");

        assert!(recorder.contents(ANCHOR_FILE).is_none());
        let conf = recorder.contents(PF_CONF).expect("pf.conf");
        assert!(!conf.contains(BLOCK_BEGIN));
        assert!(!conf.contains(BLOCK_END));
    }

    #[test]
    fn stripping_keeps_the_rest_of_the_configuration() {
        let conf =
            format!("set skip on lo0\n{BLOCK_BEGIN}\nanchor \"x\"\n{BLOCK_END}\nscrub in all\n");
        assert_eq!(strip_block(&conf), "set skip on lo0\nscrub in all\n");
    }

    #[test]
    fn the_rendered_rules_parse_with_pf() {
        // `pfctl -n` parses without loading, so this needs no privileges and
        // cannot disturb the running firewall.
        let directory = tempfile::tempdir().expect("temp dir");
        let rules_path = directory.path().join("guard.rules");
        std::fs::write(&rules_path, render_rules(&plan())).expect("write rules");

        let status = std::process::Command::new("/sbin/pfctl")
            .arg("-n")
            .arg("-f")
            .arg(&rules_path)
            .status()
            .expect("pfctl runs");
        assert!(status.success(), "PF rejected the rendered rule set");
    }

    #[test]
    fn verification_fails_without_the_block() {
        let recorder = Recorder::default();
        assert_eq!(
            verify_with(&recorder).expect_err("no block"),
            NetworkError::Platform
        );
    }

    #[test]
    fn verification_fails_when_pf_reports_no_rules() {
        let recorder = Recorder::with_file(PF_CONF, &format!("{BLOCK_BEGIN}\n{BLOCK_END}\n"));
        assert_eq!(
            verify_with(&recorder).expect_err("empty anchor"),
            NetworkError::Platform
        );
    }
}
