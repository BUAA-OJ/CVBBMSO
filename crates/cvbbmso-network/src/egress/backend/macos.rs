//! macOS PF management. Deployment provisions the main ruleset hook. The
//! guard loads and flushes only its reserved filter anchor.
#![allow(
    unsafe_code,
    reason = "PF needs native privilege and interface queries"
)]

use std::collections::{BTreeMap, BTreeSet};
use std::net::{IpAddr, Ipv4Addr, Ipv6Addr};
use std::sync::Mutex;

use serde::{Deserialize, Serialize};

use crate::NetworkError;
use crate::egress::backend::{GuardBackend, InstallResult};
use crate::egress::commands::{CAPTURE_PID, CAPTURE_PID_DIGITS, Commands};
use crate::egress::{policy::Plan, state};

mod lifecycle;
mod rules;
use lifecycle::Prepared;
pub(crate) use lifecycle::{HELPER, watchdog_entry};
use rules::Rules;

const ANCHOR: &str = "com.cvbbmso.egress";
const RULES: &str = "/Library/Application Support/com.cvbbmso/pf.rules";
const TOKEN: &str = "/Library/Application Support/com.cvbbmso/pf.enable";
const ENABLE_BASELINE: &str = "/Library/Application Support/com.cvbbmso/pf.enable.before";
const ENABLE_CAPTURE: &str = "/Library/Application Support/com.cvbbmso/pf.enable.capture";
const ENABLE_OWNER: &str = "/Library/Application Support/com.cvbbmso/pf.enable.owner";
const ENABLE_RELEASING: &str = "/Library/Application Support/com.cvbbmso/pf.enable.releasing";
const PFCTL: &str = "/sbin/pfctl";
const REFERENCES: &[&str] = &["-s", "References"];
const ROOT_RULE: &str = "anchor \"com.cvbbmso.egress\" all";
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
        let commands = crate::egress::commands::SystemCommands::new(Some(&lock));
        self.verify_owner()?;
        let mut expected = self.expected.lock().map_err(|_| NetworkError::Platform)?;
        verify_with(&commands, &expected)?;
        replace_with(&commands, &mut expected, Rules::new(plan), local_addresses)
    }

    fn verify(&self) -> Result<(), NetworkError> {
        let lock = state::lock()?;
        let commands = crate::egress::commands::SystemCommands::new(Some(&lock));
        let mut watchdog = self.watchdog.lock().map_err(|_| NetworkError::Platform)?;
        let watchdog = watchdog.as_mut().ok_or(NetworkError::Platform)?;
        let mut guard_state = watchdog.check()?;
        let expected = self.expected.lock().map_err(|_| NetworkError::Platform)?;
        verify_with(&commands, &expected)?;
        if guard_state.renewal_due() {
            watchdog.job.verify(&commands)?;
            guard_state.renew();
            state::write(&commands, &guard_state)?;
        }
        Ok(())
    }

    fn revert(&self) -> Result<(), NetworkError> {
        let lock = state::lock()?;
        let commands = crate::egress::commands::SystemCommands::new(Some(&lock));
        let mut watchdog = self.watchdog.lock().map_err(|_| NetworkError::Platform)?;
        if let Some(running) = watchdog.as_mut() {
            match state::read(&commands)? {
                Some(state) if running.owns(&state) => {
                    lifecycle::recover_job(&commands, &running.job)?;
                }
                None => {
                    lifecycle::recover_without_owner(
                        &commands,
                        &running.job,
                        Some(std::process::id()),
                    )?;
                }
                _ => {}
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
    enable_pf(commands)?;
    verify_with(commands, &expected)?;
    // PF tests state before either filter direction. Recheck connections
    // initiated from the local address as well as to it, without flushing PF.
    for ip in local.iter().filter(|ip| !ip.is_loopback()) {
        let address = ip.to_string();
        let remote = if ip.is_ipv4() { "0.0.0.0/0" } else { "::/0" };
        commands.run(PFCTL, &["-k", &address])?;
        commands.run(PFCTL, &["-k", remote, "-k", &address])?;
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
    if !filter
        .iter()
        .any(|line| line.starts_with("block drop in quick "))
        || !filter
            .iter()
            .any(|line| line.starts_with("block drop out quick "))
        || filter.iter().any(|line| {
            !line.starts_with("block drop in quick ") && !line.starts_with("block drop out quick ")
        })
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
        if let Err(error) = load_anchor(commands) {
            if error == NetworkError::Io {
                // A timed-out pfctl may have committed before it exited.
                // Restore the last verified plan while we still own the lock.
                commands.write(RULES, &expected.rules.source)?;
                load_anchor(commands)?;
            }
            return Err(error);
        }
        let checked = verify_with(commands, &next).and_then(|()| {
            if !revoke.is_empty() {
                // Host pass rules may have created states for previously
                // allowed ports. PF checks those states before evaluating rules.
                for source in local()?.into_iter().filter(|source| !source.is_loopback()) {
                    for target in revoke.iter().filter(|ip| ip.is_ipv4() == source.is_ipv4()) {
                        let local = source.to_string();
                        let remote = target.to_string();
                        commands.run(PFCTL, &["-k", &local, "-k", &remote])?;
                        commands.run(PFCTL, &["-k", &remote, "-k", &local])?;
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
    let references = reference_list(commands)?;
    let owner = references.get(&reference).ok_or(NetworkError::Platform)?;
    if !info.lines().any(|line| line.starts_with("Status: Enabled"))
        || live.trim() != expected.kernel
        || read_reference_owner(commands)?
            .is_some_and(|(token, saved)| token != reference || saved.distinct_from(owner))
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
    Ok(commands.read(TOKEN)?.as_deref().and_then(complete_token))
}

fn complete_token(contents: &str) -> Option<u64> {
    let mut records = contents.split_inclusive('\n').filter_map(|line| {
        let (label, value) = line.split_once(':')?;
        (label.trim() == "Token").then_some(value)
    });
    // A missing newline could leave a valid-looking prefix of another token.
    let token = records.next()?.strip_suffix('\n')?.trim().parse().ok()?;
    records.next().is_none().then_some(token)
}

fn enable_pf(commands: &dyn Commands) -> Result<(), NetworkError> {
    let before = reference_list(commands)?
        .into_keys()
        .collect::<BTreeSet<_>>();
    let baseline = serde_json::to_string(&before).map_err(|_| NetworkError::Io)?;
    commands.write(ENABLE_BASELINE, &baseline)?;
    commands.capture(PFCTL, &["-E"], ENABLE_CAPTURE)?;
    let capture = commands.read(ENABLE_CAPTURE)?.ok_or(NetworkError::Io)?;
    let token = complete_token(&capture).ok_or(NetworkError::Platform)?;
    let pid = capture_pid(&capture).ok_or(NetworkError::Io)?;
    let references = reference_list(commands)?;
    let owner = references.get(&token).ok_or(NetworkError::Platform)?;
    if before.contains(&token) || !owner.matches_capture(Some(pid)) || !owner.exact_start() {
        return Err(NetworkError::Io);
    }
    write_reference_owner(commands, token, owner)?;
    // The deployed watchdog may predate this transaction. Give it only
    // one complete, atomically published token at its original path.
    commands.publish(ENABLE_CAPTURE, TOKEN)
}

fn capture_pid(contents: &str) -> Option<u32> {
    contents
        .lines()
        .next()?
        .strip_prefix(CAPTURE_PID)?
        .parse()
        .ok()
}

fn capture_stopped_before_exec(contents: &str) -> bool {
    CAPTURE_PID.starts_with(contents)
        || contents.strip_prefix(CAPTURE_PID).is_some_and(|digits| {
            digits.len() <= CAPTURE_PID_DIGITS && digits.bytes().all(|digit| digit.is_ascii_digit())
        })
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

    recover_reference(commands)?;
    for path in [
        ENABLE_CAPTURE,
        ENABLE_RELEASING,
        ENABLE_OWNER,
        ENABLE_BASELINE,
        RULES,
    ] {
        commands.remove(path)?;
    }
    Ok(())
}

fn recover_reference(commands: &dyn Commands) -> Result<(), NetworkError> {
    let (contents, journal) = match (
        commands.read(TOKEN)?,
        commands.read(ENABLE_CAPTURE)?,
        commands.read(ENABLE_RELEASING)?,
    ) {
        (Some(canonical), None, None) => {
            // Old helpers read this path without checking ownership. Once a
            // new helper starts release, only the new retry path may see it.
            commands.publish(TOKEN, ENABLE_RELEASING)?;
            (canonical, ENABLE_RELEASING)
        }
        (None, Some(capture), None) => (capture, ENABLE_CAPTURE),
        (None, None, Some(releasing)) => (releasing, ENABLE_RELEASING),
        (None, None, None) => return Ok(()),
        _ => return Err(NetworkError::Io),
    };
    let baseline = commands.read(ENABLE_BASELINE)?;
    if journal == ENABLE_CAPTURE && baseline.is_none() {
        return Err(NetworkError::Io);
    }
    let before = baseline
        .as_deref()
        .map(serde_json::from_str::<BTreeSet<u64>>)
        .transpose()
        .map_err(|_| NetworkError::Io)?;
    if journal == ENABLE_CAPTURE && capture_stopped_before_exec(&contents) {
        // A short PID write aborts pre_exec before pfctl can acquire a reference.
        return if commands.read(ENABLE_OWNER)?.is_none() {
            Ok(())
        } else {
            Err(NetworkError::Io)
        };
    }
    if let Some(token) = complete_token(&contents) {
        if before
            .as_ref()
            .is_some_and(|tokens| tokens.contains(&token))
        {
            return if reference_list(commands)?.contains_key(&token) {
                Err(NetworkError::Io)
            } else {
                Ok(())
            };
        }
        let pid = capture_pid(&contents);
        if before.is_some() && pid.is_none() {
            return Err(NetworkError::Io);
        }
        release_reference(commands, token, pid, journal)?;
    } else if !reference_list(commands)?.is_empty() {
        // DIOCSTARTREF may have committed before pfctl printed its token.
        // Even a baseline token could have been released and recycled.
        return Err(NetworkError::Io);
    }
    Ok(())
}

fn release_reference(
    commands: &dyn Commands,
    token: u64,
    pid: Option<u32>,
    journal: &str,
) -> Result<(), NetworkError> {
    let references = reference_list(commands)?;
    let Some(current) = references.get(&token) else {
        return Ok(());
    };
    if let Some((saved_token, owner)) = read_reference_owner(commands)? {
        if saved_token != token || !owner.matches_capture(pid) {
            return Err(NetworkError::Io);
        }
        if owner.distinct_from(current) {
            return Ok(());
        }
        if !owner.definitely_matches(current) {
            return Err(NetworkError::Io);
        }
    } else {
        if !current.matches_capture(pid) {
            return Ok(());
        }
        // A later reading may resolve this sampling window. Don't store an
        // ambiguous first observation or run a release before it is exact.
        if !current.exact_start() {
            return Err(NetworkError::Io);
        }
        if pid.is_none() {
            // Older helpers never recorded a process identity, but their
            // original token file must have been written after acquisition.
            let written = commands.modified_seconds(journal)?;
            if current.earliest_start > written {
                return Ok(());
            }
            if current.latest_start > written {
                return Err(NetworkError::Io);
            }
        }
        // A complete unpublished capture or a legacy token has no saved owner
        // yet. Preserve the observed identity before a release can reach PF.
        write_reference_owner(commands, token, current)?;
    }
    if let Err(error) = commands.run(PFCTL, &["-X", &token.to_string()]) {
        let after = reference_list(commands)?;
        if after
            .get(&token)
            .is_some_and(|owner| !current.distinct_from(owner))
        {
            return Err(error);
        }
    }
    Ok(())
}

fn read_reference_owner(
    commands: &dyn Commands,
) -> Result<Option<(u64, ReferenceOwner)>, NetworkError> {
    let Some(record) = commands.read(ENABLE_OWNER)? else {
        return Ok(None);
    };
    let (token, owner): (u64, ReferenceOwner) =
        serde_json::from_str(&record).map_err(|_| NetworkError::Io)?;
    if owner.earliest_start > owner.latest_start {
        return Err(NetworkError::Io);
    }
    Ok(Some((token, owner)))
}

fn write_reference_owner(
    commands: &dyn Commands,
    token: u64,
    owner: &ReferenceOwner,
) -> Result<(), NetworkError> {
    let record = serde_json::to_string(&(token, owner)).map_err(|_| NetworkError::Io)?;
    commands.write(ENABLE_OWNER, &record)
}

fn has_enable_record(commands: &dyn Commands) -> Result<bool, NetworkError> {
    Ok(commands.read(ENABLE_BASELINE)?.is_some()
        || commands.read(ENABLE_CAPTURE)?.is_some()
        || commands.read(ENABLE_OWNER)?.is_some()
        || commands.read(ENABLE_RELEASING)?.is_some())
}

#[derive(Deserialize, Serialize)]
struct ReferenceOwner {
    pid: u32,
    process: String,
    earliest_start: u64,
    latest_start: u64,
}

impl ReferenceOwner {
    fn matches_capture(&self, pid: Option<u32>) -> bool {
        self.process == "pfctl" && pid.is_none_or(|pid| pid == self.pid)
    }

    fn distinct_from(&self, other: &Self) -> bool {
        self.pid != other.pid
            || self.process != other.process
            || self.latest_start < other.earliest_start
            || other.latest_start < self.earliest_start
    }

    fn definitely_matches(&self, other: &Self) -> bool {
        self.exact_start() && other.exact_start() && !self.distinct_from(other)
    }

    fn exact_start(&self) -> bool {
        self.earliest_start == self.latest_start
    }
}

fn reference_list(commands: &dyn Commands) -> Result<BTreeMap<u64, ReferenceOwner>, NetworkError> {
    let mut start = commands.unix_seconds()?;
    let mut output = commands.output(PFCTL, REFERENCES)?;
    let mut end = commands.unix_seconds()?;
    if start < end {
        // pfctl prints elapsed whole seconds, so prefer a sample entirely
        // within one clock second before relying on its reported start time.
        start = end;
        output = commands.output(PFCTL, REFERENCES)?;
        end = commands.unix_seconds()?;
    }
    if end < start {
        return Err(NetworkError::Io);
    }
    parse_references(&output, start, end)
}

fn parse_references(
    output: &str,
    start: u64,
    end: u64,
) -> Result<BTreeMap<u64, ReferenceOwner>, NetworkError> {
    let mut lines = output
        .lines()
        .map(str::trim)
        .filter(|line| !line.is_empty());
    let first = lines.next().ok_or(NetworkError::Platform)?;
    if first == "No pf starter references held" && lines.next().is_none() {
        return Ok(BTreeMap::new());
    }
    if first != "TOKENS:"
        || !lines.next().is_some_and(|line| {
            line.split_ascii_whitespace()
                .eq(["PID", "Process", "Name", "TOKEN", "TIMESTAMP"])
        })
    {
        return Err(NetworkError::Platform);
    }
    let mut tokens = BTreeMap::new();
    for line in lines {
        let mut fields = line.split_ascii_whitespace();
        let pid = fields
            .next()
            .and_then(|value| value.parse().ok())
            .ok_or(NetworkError::Platform)?;
        let time = fields.next_back().ok_or(NetworkError::Platform)?;
        let time = time
            .split(':')
            .map(str::parse::<u64>)
            .collect::<Result<Vec<_>, _>>()
            .map_err(|_| NetworkError::Platform)?;
        let [hours @ 0..=23, minutes @ 0..=59, seconds @ 0..=59] = time[..] else {
            return Err(NetworkError::Platform);
        };
        if fields.next_back() != Some("days") {
            return Err(NetworkError::Platform);
        }
        let days = fields
            .next_back()
            .and_then(|value| value.parse::<u32>().ok())
            .ok_or(NetworkError::Platform)?;
        let age = u64::from(days) * 86400 + hours * 3600 + minutes * 60 + seconds;
        let token = fields
            .next_back()
            .and_then(|value| value.parse::<u64>().ok())
            .ok_or(NetworkError::Platform)?;
        let owner = ReferenceOwner {
            pid,
            process: fields.collect::<Vec<_>>().join(" "),
            earliest_start: start.checked_sub(age).ok_or(NetworkError::Platform)?,
            latest_start: end.checked_sub(age).ok_or(NetworkError::Platform)?,
        };
        if owner.process.is_empty() || tokens.insert(token, owner).is_some() {
            return Err(NetworkError::Platform);
        }
    }
    Ok(tokens)
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
    const BASE: &str = "# on-disk PF configuration\nanchor \"com.cvbbmso.egress\"\n";
    const LIVE_ROOT: &str = concat!(
        "scrub-anchor \"com.apple/*\" all fragment reassemble\n",
        "anchor \"com.cvbbmso.egress\" all\n",
        "anchor \"system.keep/*\" all\n",
        "block drop out quick inet proto tcp from any to 203.0.113.10 port = 443\n",
        "pass out quick all\n",
    );
    const BLOCK: &str = "block drop out quick from any to ! <cvbbmso_destinations>";
    const BOTH: &str = concat!(
        "block drop out quick from any to ! <cvbbmso_destinations>\n",
        "block drop in quick proto tcp from ! <cvbbmso_tcp_443> port = 443 to any",
    );
    const PARSE: &[&str] = &["-a", ANCHOR, "-n", "-v", "-o", "none", "-f", RULES];
    const LIVE: &[&str] = &["-a", ANCHOR, "-s", "rules"];
    const ROOT: &[&str] = &["-s", "rules"];
    const TABLES: &[&str] = &["-a", ANCHOR, "-s", "Tables"];
    const NO_REFERENCES: &str = "No pf starter references held\n";
    const PROJECT_REFERENCE: &str = concat!(
        "TOKENS:\nPID Process Name TOKEN TIMESTAMP\n",
        "202 pfctl 42 0 days 00:00:01\n",
    );
    const HOST_REFERENCE: &str = concat!(
        "TOKENS:\nPID Process Name TOKEN TIMESTAMP\n",
        "101 Host Firewall 4 0 days 01:02:03\n",
    );
    const BOTH_REFERENCES: &str = concat!(
        "TOKENS:\nPID Process Name TOKEN TIMESTAMP\n",
        "202 pfctl 42 0 days 00:00:01\n",
        "101 Host Firewall 4 0 days 01:02:03\n",
    );

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
        recorder.reply(REFERENCES, Ok(PROJECT_REFERENCE));
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
    fn install_rechecks_existing_states_in_both_families_and_directions() {
        let recorder = Recorder::with_file(PF_CONF, BASE);
        recorder.reply(ROOT, Ok(LIVE_ROOT));
        recorder.reply(PARSE, Ok(BOTH));
        recorder.reply(&["-E"], Ok("pf enabled\nToken : 42\n"));
        recorder.reply(REFERENCES, Ok(NO_REFERENCES));
        recorder.reply(REFERENCES, Ok(PROJECT_REFERENCE));
        healthy(&recorder, BOTH, &Rules::new(&plan()));

        let local = ["192.0.2.123", "2001:db8::123", "127.0.0.1"].map(|ip| ip.parse().unwrap());
        install_with(&recorder, &plan(), &local).unwrap();
        assert_eq!(recorder.contents(PF_CONF).as_deref(), Some(BASE));
        assert_only_own_mutations(&recorder);
        assert_eq!(read_token(&recorder).unwrap(), Some(42));
        assert!(recorder.contents(ENABLE_CAPTURE).is_none());
        assert_eq!(
            state_kills(&recorder),
            [
                vec!["-k", "192.0.2.123"],
                vec!["-k", "0.0.0.0/0", "-k", "192.0.2.123"],
                vec!["-k", "2001:db8::123"],
                vec!["-k", "::/0", "-k", "2001:db8::123"],
            ],
        );
    }

    #[test]
    fn failed_enable_verification_leaves_a_recoverable_reference() {
        let recorder = Recorder::with_file(PF_CONF, BASE);
        recorder.reply(ROOT, Ok(LIVE_ROOT));
        recorder.reply(PARSE, Ok(BOTH));
        recorder.reply(&["-E"], Ok("pf enabled\nToken : 42\n"));
        recorder.reply(REFERENCES, Ok(NO_REFERENCES));
        recorder.reply(REFERENCES, Ok(PROJECT_REFERENCE));
        recorder.reply(&["-s", "info"], Ok("Status: Disabled"));
        recorder.reply(ROOT, Ok(LIVE_ROOT));
        recorder.reply(LIVE, Ok(BOTH));
        recorder.reply(REFERENCES, Ok(PROJECT_REFERENCE));

        assert_eq!(
            install_with(&recorder, &plan(), &[]).err(),
            Some(NetworkError::Platform)
        );

        recorder.reply(LIVE, Ok(BOTH));
        recorder.reply(REFERENCES, Ok(PROJECT_REFERENCE));
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
    fn a_failed_capture_before_exec_clears_the_journal_and_allows_retry() {
        for partial in [
            "",
            "Capture PI",
            "Capture PID: ",
            "Capture PID: 00000002",
            "Capture PID: 0000000202",
        ] {
            let recorder = Recorder::with_file(ENABLE_CAPTURE, partial);
            recorder.reply(REFERENCES, Ok(HOST_REFERENCE));
            recorder.reply(&["-E"], Err(NetworkError::Io));
            assert_eq!(enable_pf(&recorder), Err(NetworkError::Io));
            assert_eq!(recorder.contents(ENABLE_BASELINE).as_deref(), Some("[4]"));

            // The host reference is still present. Recovery should leave it
            // alone and the next attempt should still see it as its baseline.
            recorder.reply(REFERENCES, Ok(HOST_REFERENCE));
            let start = recorder.calls().len();
            assert_eq!(recover_with(&recorder), Ok(()), "{partial:?}");
            assert!(!has_enable_record(&recorder).unwrap());
            assert_eq!(
                &recorder.calls()[start..],
                [LIVE, TABLES].map(|args| Call::Output(
                    PFCTL.to_owned(),
                    args.iter().map(|arg| (*arg).to_owned()).collect()
                ))
            );

            recorder.reply(REFERENCES, Ok(BOTH_REFERENCES));
            recorder.reply(&["-E"], Ok("Token : 42\n"));
            enable_pf(&recorder).unwrap();
            assert_eq!(recorder.contents(ENABLE_BASELINE).as_deref(), Some("[4]"));
            assert_eq!(read_token(&recorder).unwrap(), Some(42));
        }
    }

    #[test]
    fn enable_timeout_with_missing_or_partial_token_keeps_unknown_references() {
        for partial in [
            "Capture PID: 0000000202\n",
            "Capture PID: 0000000202\nToken : 4",
            "Capture PID: malformed",
            "Capture PID: 00000000202",
            "Token : 4",
        ] {
            let recorder = Recorder::with_file(ENABLE_CAPTURE, partial);
            recorder.reply(REFERENCES, Ok(HOST_REFERENCE));
            recorder.reply(&["-E"], Err(NetworkError::Io));
            assert_eq!(enable_pf(&recorder), Err(NetworkError::Io));
            assert!(recorder.contents(TOKEN).is_none());

            recorder.reply(REFERENCES, Ok(BOTH_REFERENCES));
            assert_eq!(recover_with(&recorder), Err(NetworkError::Io));
            assert_eq!(recorder.contents(ENABLE_CAPTURE).as_deref(), Some(partial));
            assert_eq!(recorder.contents(ENABLE_BASELINE).as_deref(), Some("[4]"));
            assert!(recorder.calls().iter().all(|call| !matches!(call,
                Call::Run(_, args) if args.first().is_some_and(|arg| arg == "-X")
            )));
        }
    }

    #[test]
    fn a_partial_capture_does_not_discard_conflicting_owner_evidence() {
        let recorder = Recorder::with_file(ENABLE_CAPTURE, "Capture PID: 0000000202");
        recorder.write(ENABLE_BASELINE, "[4]").unwrap();
        let owner = r#"[42,{"pid":202,"process":"pfctl","earliest_start":1799999999,"latest_start":1799999999}]"#;
        recorder.write(ENABLE_OWNER, owner).unwrap();

        assert_eq!(recover_with(&recorder), Err(NetworkError::Io));
        assert_eq!(recorder.contents(ENABLE_OWNER).as_deref(), Some(owner));
        assert!(recorder.contents(ENABLE_CAPTURE).is_some());
    }

    #[test]
    fn an_incomplete_capture_can_be_cleared_when_no_references_remain() {
        let recorder = Recorder::with_file(ENABLE_CAPTURE, "Capture PID: 0000000202\n");
        recorder.write(ENABLE_BASELINE, "[4]").unwrap();
        recorder.reply(REFERENCES, Ok(HOST_REFERENCE));
        assert_eq!(recover_with(&recorder), Err(NetworkError::Io));
        assert!(has_enable_record(&recorder).unwrap());

        recorder.reply(REFERENCES, Ok(NO_REFERENCES));
        recover_with(&recorder).unwrap();
        assert!(!has_enable_record(&recorder).unwrap());
        assert!(
            recorder
                .calls()
                .iter()
                .all(|call| matches!(call, Call::Output(..)))
        );
    }

    #[test]
    fn after_publication_old_cleanup_cannot_leave_a_staged_token_to_release_again() {
        let recorder = Recorder::default();
        recorder.reply(REFERENCES, Ok(HOST_REFERENCE));
        recorder.reply(REFERENCES, Ok(BOTH_REFERENCES));
        recorder.reply(&["-E"], Ok("Token : 42\n"));
        enable_pf(&recorder).unwrap();

        // The old helper released 42 and only knew the canonical path.
        assert_eq!(read_token(&recorder).unwrap(), Some(42));
        recorder.remove(TOKEN).unwrap();
        recover_with(&recorder).unwrap();
        assert!(!has_enable_record(&recorder).unwrap());
        assert!(
            recorder
                .calls()
                .iter()
                .all(|call| matches!(call, Call::Output(..)))
        );
    }

    #[test]
    fn a_failed_release_with_an_unreadable_reference_list_keeps_the_token() {
        let recorder = Recorder::with_file(TOKEN, "Token : 42\n");
        recorder.reply(REFERENCES, Ok(PROJECT_REFERENCE));
        recorder.reply(&["-X", "42"], Err(NetworkError::Io));
        recorder.reply(REFERENCES, Ok(""));
        assert_eq!(recover_with(&recorder), Err(NetworkError::Platform));
        assert!(read_token(&recorder).unwrap().is_none());
        assert_eq!(
            recorder.contents(ENABLE_RELEASING).as_deref(),
            Some("Token : 42\n")
        );
        assert!(recorder.contents(ENABLE_OWNER).is_some());
    }

    #[test]
    fn a_release_that_committed_before_timeout_does_not_replay_on_a_recycled_token() {
        for recycled in [
            "101 Host Firewall 42 0 days 00:00:00\n",
            "202 pfctl 42 0 days 00:00:00\n",
        ] {
            let recorder = Recorder::with_file(TOKEN, "Token : 42\n");
            recorder.reply(REFERENCES, Ok(PROJECT_REFERENCE));
            recorder.reply(&["-X", "42"], Err(NetworkError::Io));
            recorder.reply(REFERENCES, Err(NetworkError::Io));
            assert_eq!(recover_with(&recorder), Err(NetworkError::Io));
            // An older watchdog can no longer see the token at its old path.
            assert!(recorder.contents(TOKEN).is_none());
            assert!(recorder.contents(ENABLE_RELEASING).is_some());
            assert!(recorder.contents(ENABLE_OWNER).is_some());
            recorder.reply(
                REFERENCES,
                Ok(&format!(
                    "TOKENS:\nPID Process Name TOKEN TIMESTAMP\n{recycled}"
                )),
            );
            recover_with(&recorder).unwrap();
            assert!(!has_enable_record(&recorder).unwrap());
            let releases = recorder
                .calls()
                .into_iter()
                .filter(|call| {
                    matches!(call,
                        Call::Run(_, args) if args == &["-X", "42"]
                    )
                })
                .count();
            assert_eq!(releases, 1);
        }
    }

    #[test]
    fn a_reference_with_ambiguous_creation_time_is_not_released() {
        let recorder = Recorder::with_file(TOKEN, "Token : 42\n");
        let now = recorder.unix_seconds().unwrap();
        let owner = ReferenceOwner {
            pid: 202,
            process: "pfctl".to_owned(),
            earliest_start: now - 2,
            latest_start: now,
        };
        write_reference_owner(&recorder, 42, &owner).unwrap();
        recorder.reply(REFERENCES, Ok(PROJECT_REFERENCE));
        assert_eq!(recover_with(&recorder), Err(NetworkError::Io));
        assert!(recorder.contents(TOKEN).is_none());
        assert!(recorder.contents(ENABLE_RELEASING).is_some());
        assert!(recorder.contents(ENABLE_OWNER).is_some());
        assert!(
            recorder
                .calls()
                .iter()
                .all(|call| matches!(call, Call::Output(..)))
        );
    }

    #[test]
    fn an_inexact_enable_snapshot_does_not_publish_and_can_be_recovered_later() {
        let recorder = Recorder::default();
        let now = recorder.unix_seconds().unwrap();
        recorder.times([
            now,
            now,
            now,
            now + 1,
            now + 2,
            now + 3,
            now + 4,
            now + 5,
            now + 6,
            now + 6,
        ]);
        recorder.reply(REFERENCES, Ok(HOST_REFERENCE));
        for _ in 0..5 {
            recorder.reply(REFERENCES, Ok(BOTH_REFERENCES));
        }
        recorder.reply(&["-E"], Ok("Token : 42\n"));

        assert_eq!(enable_pf(&recorder), Err(NetworkError::Io));
        assert!(recorder.contents(TOKEN).is_none());
        assert!(recorder.contents(ENABLE_OWNER).is_none());
        assert!(recorder.contents(ENABLE_CAPTURE).is_some());
        assert_eq!(recover_with(&recorder), Err(NetworkError::Io));
        assert!(recorder.contents(ENABLE_OWNER).is_none());
        assert!(
            recorder
                .calls()
                .iter()
                .all(|call| matches!(call, Call::Output(..)))
        );

        recover_with(&recorder).unwrap();
        let releases = recorder
            .calls()
            .into_iter()
            .filter_map(|call| match call {
                Call::Run(_, args) => Some(args),
                Call::Output(..) => None,
            })
            .collect::<Vec<_>>();
        assert_eq!(releases, [vec!["-X", "42"]]);
        assert!(!has_enable_record(&recorder).unwrap());
    }

    #[test]
    fn a_rejected_anchor_refresh_leaves_the_current_rules() {
        let recorder = Recorder::with_file(TOKEN, "Token : 42\n");
        let mut expected = Expected {
            rules: Rules::new(&plan()),
            kernel: BOTH.to_owned(),
        };
        recorder.reply(PARSE, Ok(BOTH));
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
    fn a_timed_out_anchor_refresh_reloads_the_prior_rules_before_returning() {
        let recorder = Recorder::with_file(TOKEN, "Token : 42\n");
        let mut expected = Expected {
            rules: Rules::new(&plan()),
            kernel: BOTH.to_owned(),
        };
        let replace = &["-a", ANCHOR, "-o", "none", "-f", RULES];
        recorder.reply(PARSE, Ok(BOTH));
        recorder.reply(replace, Err(NetworkError::Io));
        recorder.reply(replace, Ok(""));
        assert_eq!(
            replace_with(
                &recorder,
                &mut expected,
                Rules::new(&Plan::default()),
                || Ok(Vec::new())
            ),
            Err(NetworkError::Io)
        );
        assert_eq!(
            recorder.contents(RULES).as_deref(),
            Some(expected.rules.source.as_str())
        );
        assert_eq!(
            recorder
                .calls()
                .iter()
                .filter(|call| match call {
                    Call::Run(program, args) => program == PFCTL && args == replace,
                    Call::Output(..) => false,
                })
                .count(),
            2
        );
    }

    #[test]
    fn verification_detects_partial_rules_or_a_missing_enable_reference() {
        let expected = Expected {
            rules: Rules::new(&plan()),
            kernel: BOTH.to_owned(),
        };

        let recorder = Recorder::with_file(TOKEN, "Token : 42\n");
        healthy(&recorder, "pass out quick on lo0 all", &expected.rules);
        assert_eq!(
            verify_with(&recorder, &expected),
            Err(NetworkError::Platform)
        );

        let recorder = Recorder::with_file(TOKEN, "Token : 42\n");
        healthy(&recorder, BOTH, &expected.rules);
        assert_eq!(verify_with(&recorder, &expected), Ok(()));
        healthy(&recorder, BOTH, &expected.rules);
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
        healthy(&recorder, BOTH, &expected.rules);
        assert_eq!(
            verify_with(&recorder, &expected),
            Err(NetworkError::Platform)
        );
    }

    #[test]
    fn refresh_rechecks_only_the_revoked_peers_in_both_directions() {
        let recorder = Recorder::with_file(TOKEN, "Token : 42\n");
        let mut expected = Expected {
            rules: Rules::new(&plan()),
            kernel: BOTH.to_owned(),
        };
        let mut next = plan();
        next.services.retain(|service| service.remote.port() != 67);
        next.server.addresses = vec!["2001:db8::2".parse().unwrap()];
        let next = Rules::new(&next);
        recorder.reply(PARSE, Ok(BOTH));
        healthy(&recorder, BOTH, &next);
        replace_with(&recorder, &mut expected, next, || {
            Ok(["192.0.2.2", "2001:db8::3"]
                .map(|ip| ip.parse().unwrap())
                .to_vec())
        })
        .unwrap();
        assert_eq!(
            state_kills(&recorder),
            [
                vec!["-k", "192.0.2.2", "-k", "192.0.2.67"],
                vec!["-k", "192.0.2.67", "-k", "192.0.2.2"],
                vec!["-k", "2001:db8::3", "-k", "2001:db8::1"],
                vec!["-k", "2001:db8::1", "-k", "2001:db8::3"],
            ],
        );
    }

    fn state_kills(recorder: &Recorder) -> Vec<Vec<String>> {
        recorder
            .calls()
            .into_iter()
            .filter_map(|call| match call {
                Call::Run(program, args) if program == PFCTL && args[0] == "-k" => Some(args),
                _ => None,
            })
            .collect()
    }

    #[test]
    fn staging_refuses_a_ruleset_without_ingress_checks() {
        let recorder = Recorder::default();
        recorder.reply(PARSE, Ok(BLOCK));
        assert_eq!(
            stage(&recorder, Rules::new(&plan())).err(),
            Some(NetworkError::Platform)
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
            rules.source.contains(
                "block drop in quick proto tcp from ! <cvbbmso_tcp_8443> port 8443 to any"
            )
        );
        assert!(
            rules
                .source
                .contains("block drop in quick proto udp from ! <cvbbmso_udp_67> port 67 to any")
        );
        assert!(
            !rules
                .source
                .lines()
                .any(|line| line.starts_with("block drop in quick ") && line.contains("ipv6-icmp"))
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
                .all(|line| line.starts_with("block drop out quick ")
                    || line.starts_with("block drop in quick "))
        );
        assert!(
            filters.contains(
                &"block drop in quick proto tcp from ! <cvbbmso_tcp_443> port = 443 to any"
            )
        );

        let root_path = directory.path().join("pf.conf");
        let host_policy = format!(
            "scrub in all fragment reassemble\nno scrub out all\n{BASE}block drop in quick proto tcp from 203.0.113.10 port 443 to any\nblock drop out quick proto tcp from any to 203.0.113.10 port 443\npass out quick all\n"
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
        assert!(
            live_root
                .contains("block drop in quick inet proto tcp from 203.0.113.10 port = 443 to any")
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
        for hook in ["out all", "out quick all", "quick all"] {
            let recorder = Recorder::with_file(PF_CONF, BASE);
            let live = format!("anchor \"com.cvbbmso.egress\" {hook}\npass out quick all");
            recorder.reply(ROOT, Ok(&live));
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
