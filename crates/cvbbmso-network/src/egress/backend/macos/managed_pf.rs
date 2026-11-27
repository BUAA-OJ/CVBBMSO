//! Temporary first filter entry for supported, quiescent Darwin PF rulesets.

use std::path::Path;

use super::{ANCHOR, PFCTL, ROOT_RULE, verify_deployment};
use crate::NetworkError;
use crate::egress::{commands::Commands, state};

const SYSTEM_CONFIG: &str = "/private/etc/pf.conf";
const APPLE_CONFIG: &str = "/private/etc/pf.anchors/com.apple";
const APPLE_SOURCE: &str = r#"anchor "200.AirDrop/*"
anchor "250.ApplicationFirewall/*""#;
const APPLE_CHILDREN: [&str; 2] = ["200.AirDrop", "250.ApplicationFirewall"];
const JOURNAL: &str = "/Library/Application Support/com.cvbbmso/managed-pf.v1";
const FILTER_FILE: &str = "/Library/Application Support/com.cvbbmso/managed-filter.conf";
const STOCK_SOURCE: &str = r#"scrub-anchor "com.apple/*"
nat-anchor "com.apple/*"
rdr-anchor "com.apple/*"
dummynet-anchor "com.apple/*"
anchor "com.apple/*"
load anchor "com.apple" from "/etc/pf.anchors/com.apple""#;
const STOCK_FILTER: &str = "scrub-anchor \"com.apple/*\"\nanchor \"com.apple/*\"\n";
const MANAGED_FILTER: &str =
    "scrub-anchor \"com.apple/*\"\nanchor \"com.cvbbmso.egress\"\nanchor \"com.apple/*\"\n";
const TODESK_RULE: &str = "anchor \"com.todesk/network-rules/*\" out quick inet all";
const TODESK_FILTER: &str = concat!(
    "scrub-anchor \"com.apple/*\"\n",
    "anchor \"com.todesk/network-rules/*\" out quick inet all\n",
    "anchor \"com.apple/*\"\n",
);
const TODESK_MANAGED_FILTER: &str = concat!(
    "scrub-anchor \"com.apple/*\"\n",
    "anchor \"com.cvbbmso.egress\"\n",
    "anchor \"com.todesk/network-rules/*\" out quick inet all\n",
    "anchor \"com.apple/*\"\n",
);
const LIVE_SCRUB: &str = "scrub-anchor \"com.apple/*\" all fragment reassemble";
const LEGACY_ROOT_RULE: &str = "anchor \"com.cvbbmso.egress\" out all";
const LIVE_APPLE: &str = "anchor \"com.apple/*\" all";
const LIVE_NAT: &[&str] = &[
    "nat-anchor \"com.apple/*\" all",
    "rdr-anchor \"com.apple/*\" all",
];

#[derive(Clone, Copy, PartialEq, Eq)]
enum Baseline {
    Boot,
    Empty,
    WithToDesk,
}

impl Baseline {
    fn filter(self, managed: bool) -> &'static str {
        match (self, managed) {
            (Self::Boot, false) => STOCK_FILTER,
            (Self::Boot, true) => MANAGED_FILTER,
            (Self::Empty, false) => "",
            (Self::Empty, true) => "anchor \"com.cvbbmso.egress\"\n",
            (Self::WithToDesk, false) => TODESK_FILTER,
            (Self::WithToDesk, true) => TODESK_MANAGED_FILTER,
        }
    }
}

pub(super) fn authorize(commands: &dyn Commands) -> Result<(), NetworkError> {
    if verify_deployment(commands).is_ok() {
        // The dedicated anchor itself must be reserved even on managed hosts.
        if empty_sections(commands, ANCHOR, &["rules", "nat", "Tables", "Anchors"])? {
            return Ok(());
        }
        return Err(NetworkError::MacosUnsupportedPfConfiguration);
    }
    for (path, directory) in [
        ("/private", true),
        ("/private/etc", true),
        ("/private/etc/pf.anchors", true),
        (SYSTEM_CONFIG, false),
        (APPLE_CONFIG, false),
    ] {
        state::check_protected(Path::new(path), directory)
            .map_err(|_| NetworkError::MacosUnsupportedPfConfiguration)?;
    }
    activate(commands)
}

fn stock_on_disk(commands: &dyn Commands) -> Result<bool, NetworkError> {
    for (path, expected) in [(SYSTEM_CONFIG, STOCK_SOURCE), (APPLE_CONFIG, APPLE_SOURCE)] {
        let source = commands.read(path)?.unwrap_or_default();
        if !lines(&source)
            .filter(|line| !line.starts_with('#'))
            .eq(expected.lines())
        {
            return Ok(false);
        }
    }
    Ok(true)
}

fn lines(text: &str) -> impl Iterator<Item = &str> {
    text.lines().map(str::trim).filter(|line| !line.is_empty())
}

fn empty_sections(
    commands: &dyn Commands,
    anchor: &str,
    sections: &[&str],
) -> Result<bool, NetworkError> {
    for section in sections {
        if !commands
            .output(PFCTL, &["-a", anchor, "-s", section])?
            .trim()
            .is_empty()
        {
            return Ok(false);
        }
    }
    Ok(true)
}

fn apple_dispatch_is_idle(commands: &dyn Commands) -> Result<bool, NetworkError> {
    let rules = commands.output(PFCTL, &["-a", "com.apple", "-s", "rules"])?;
    let children = commands.output(PFCTL, &["-a", "com.apple", "-s", "Anchors"])?;
    let expected_rules = APPLE_CHILDREN.map(|name| format!("anchor \"{name}/*\" all"));
    let absolute_rules = APPLE_CHILDREN.map(|name| format!("anchor \"com.apple/{name}/*\" all"));
    let absolute_children = APPLE_CHILDREN.map(|name| format!("com.apple/{name}"));
    if !(lines(&rules).eq(expected_rules.iter().map(String::as_str))
        || lines(&rules).eq(absolute_rules.iter().map(String::as_str)))
        || !(lines(&children).eq(APPLE_CHILDREN)
            || lines(&children).eq(absolute_children.iter().map(String::as_str)))
        || !empty_sections(commands, "com.apple", &["nat", "Tables"])?
    {
        return Ok(false);
    }
    for name in absolute_children {
        if !empty_sections(commands, &name, &["rules", "nat", "Tables", "Anchors"])? {
            return Ok(false);
        }
    }
    Ok(true)
}

fn quiescent(
    commands: &dyn Commands,
    baseline: Baseline,
    entry: Option<&str>,
) -> Result<bool, NetworkError> {
    let apple = baseline != Baseline::Empty;
    let todesk = baseline == Baseline::WithToDesk;
    let expected_rules = apple
        .then_some(LIVE_SCRUB)
        .into_iter()
        .chain(entry)
        .chain(todesk.then_some(TODESK_RULE))
        .chain(apple.then_some(LIVE_APPLE));
    let expected_anchors = apple
        .then_some("com.apple")
        .into_iter()
        .chain(entry.map(|_| ANCHOR));
    let expected_nat = if apple { LIVE_NAT } else { &[] };
    let status = commands.output(PFCTL, &["-s", "info"])?;
    let references = commands.output(PFCTL, &["-s", "References"])?;
    let rules = commands.output(PFCTL, &["-s", "rules"])?;
    let anchors = commands.output(PFCTL, &["-s", "Anchors"])?;
    let has_anchor = lines(&anchors).any(|name| name == ANCHOR);
    // A filter-only load keeps the vendor/Apple child rulesets in the kernel.
    // Other root anchors may be registered by system services. None are
    // referenced by the filter being replaced, and they are not reloaded.
    let expected_anchors = if todesk {
        lines(&anchors).any(|name| name == "com.apple")
            && lines(&anchors).any(|name| name == "com.todesk")
            && (entry.is_none() || has_anchor)
    } else {
        lines(&anchors).eq(expected_anchors)
    };
    if !lines(&status)
        .next()
        .is_some_and(|line| line.starts_with("Status: Disabled"))
        || !matches!(
            references.trim(),
            "" | "No pf starter references held" | "No pf_enabled references"
        )
        || !lines(&rules)
            .filter(|line| *line != "FILTER RULES:")
            .eq(expected_rules)
        || !expected_anchors
        // ToDesk's filter contains no table references. Its existing NAT and
        // tables stay in place, and install_with checks for bypass translations
        // in all anchors before enabling PF.
        || (!todesk
            && (!lines(&commands.output(PFCTL, &["-s", "nat"])?).eq(expected_nat.iter().copied())
                || !commands.output(PFCTL, &["-s", "Tables"])?.trim().is_empty()))
        || !commands.output(PFCTL, &["-s", "states"])?.trim().is_empty()
        || (baseline == Baseline::Boot && !apple_dispatch_is_idle(commands)?)
        || (has_anchor
            && !empty_sections(commands, ANCHOR, &["rules", "nat", "Tables", "Anchors"])?)
    {
        return Ok(false);
    }
    let interfaces = commands.output(PFCTL, &["-vv", "-s", "Interfaces"])?;
    Ok(!lines(&interfaces).any(|line| {
        let mut words = line.split_ascii_whitespace();
        words.next().is_some_and(|name| name != "lo0") && words.any(|word| word == "(skip)")
    }))
}

fn activate(commands: &dyn Commands) -> Result<(), NetworkError> {
    if !stock_on_disk(commands)? {
        return Err(NetworkError::MacosUnsupportedPfConfiguration);
    }
    let baseline = if quiescent(commands, Baseline::Boot, None)? {
        Baseline::Boot
    } else if quiescent(commands, Baseline::Empty, None)? {
        Baseline::Empty
    } else if quiescent(commands, Baseline::WithToDesk, None)? {
        Baseline::WithToDesk
    } else {
        return Err(NetworkError::MacosUnsupportedPfConfiguration);
    };
    if commands
        .read(JOURNAL)?
        .is_some_and(|saved| saved != baseline.filter(false))
    {
        return Err(NetworkError::MacosUnsupportedPfConfiguration);
    }
    commands.write(JOURNAL, baseline.filter(false))?;
    match replace_filter(commands, baseline, None, Some(ROOT_RULE)) {
        Ok(()) => Ok(()),
        Err(error) => {
            let _ = restore(commands);
            Err(error)
        }
    }
}

fn replace_filter(
    commands: &dyn Commands,
    baseline: Baseline,
    before: Option<&str>,
    after: Option<&str>,
) -> Result<(), NetworkError> {
    commands.write(FILTER_FILE, baseline.filter(after.is_some()))?;
    commands.run(PFCTL, &["-n", "-R", "-o", "none", "-f", FILTER_FILE])?;
    // PF has no public compare-and-swap interface for the main ruleset.
    // Recheck immediately before the filter-only transaction and never rewrite
    // an unexpected snapshot to try to recover from a competing change.
    if !stock_on_disk(commands)? || !quiescent(commands, baseline, before)? {
        return Err(NetworkError::MacosUnsupportedPfConfiguration);
    }
    commands.run(PFCTL, &["-R", "-o", "none", "-f", FILTER_FILE])?;
    if !quiescent(commands, baseline, after)? {
        return Err(NetworkError::MacosUnsupportedPfConfiguration);
    }
    Ok(())
}

pub(super) fn has_journal(commands: &dyn Commands) -> Result<bool, NetworkError> {
    Ok(commands.read(JOURNAL)?.is_some())
}

pub(super) fn restore(commands: &dyn Commands) -> Result<(), NetworkError> {
    let Some(saved) = commands.read(JOURNAL)? else {
        return Ok(());
    };
    let baseline = match saved.as_str() {
        STOCK_FILTER => Baseline::Boot,
        "" => Baseline::Empty,
        TODESK_FILTER => Baseline::WithToDesk,
        _ => return Err(NetworkError::MacosUnsupportedPfConfiguration),
    };
    if !stock_on_disk(commands)? {
        return Err(NetworkError::MacosUnsupportedPfConfiguration);
    }
    if !quiescent(commands, baseline, None)? {
        // Older packaged clients used an outbound-only entry. Only a saved
        // stock baseline and an exact, idle snapshot permit its removal.
        let entry = if quiescent(commands, baseline, Some(ROOT_RULE))? {
            ROOT_RULE
        } else if baseline != Baseline::WithToDesk {
            LEGACY_ROOT_RULE
        } else {
            return Err(NetworkError::MacosUnsupportedPfConfiguration);
        };
        replace_filter(commands, baseline, Some(entry), None)?;
    }
    commands.remove(FILTER_FILE)?;
    commands.remove(JOURNAL)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::egress::commands::testing::{Call, Recorder};

    fn stock_disk() -> Recorder {
        let recorder = Recorder::with_file(SYSTEM_CONFIG, STOCK_SOURCE);
        recorder.write(APPLE_CONFIG, APPLE_SOURCE).unwrap();
        recorder
    }

    fn stock(recorder: &Recorder, entry: Option<&str>) {
        recorder.reply(
            &["-a", "com.apple", "-s", "rules"],
            Ok("anchor \"200.AirDrop/*\" all\nanchor \"250.ApplicationFirewall/*\" all"),
        );
        recorder.reply(
            &["-a", "com.apple", "-s", "Anchors"],
            Ok("200.AirDrop\n250.ApplicationFirewall"),
        );
        recorder.reply(&["-s", "info"], Ok("Status: Disabled for 0 days"));
        recorder.reply(&["-s", "References"], Ok("No pf_enabled references"));
        let rules = if let Some(entry) = entry {
            format!("{LIVE_SCRUB}\n{entry}\n{LIVE_APPLE}\n")
        } else {
            format!("{LIVE_SCRUB}\n{LIVE_APPLE}\n")
        };
        recorder.reply(&["-s", "rules"], Ok(&rules));
        recorder.reply(
            &["-s", "Anchors"],
            Ok(if entry.is_some() {
                "  com.apple\n  com.cvbbmso.egress"
            } else {
                "  com.apple"
            }),
        );
        recorder.reply(&["-s", "nat"], Ok(&LIVE_NAT.join("\n")));
    }

    fn todesk(recorder: &Recorder, entry: Option<&str>) {
        recorder.reply(&["-s", "info"], Ok("Status: Disabled for 0 days"));
        recorder.reply(&["-s", "References"], Ok("No pf_enabled references"));
        let rules = [Some(LIVE_SCRUB), entry, Some(TODESK_RULE), Some(LIVE_APPLE)]
            .into_iter()
            .flatten()
            .collect::<Vec<_>>()
            .join("\n");
        recorder.reply(&["-s", "rules"], Ok(&rules));
        recorder.reply(
            &["-s", "Anchors"],
            Ok("  com.apple\n  com.apple.internet-sharing\n  com.cvbbmso.egress\n  com.todesk"),
        );
    }

    #[test]
    fn installs_and_restores_the_observed_todesk_filter() {
        let recorder = stock_disk();
        for _ in 0..4 {
            todesk(&recorder, None);
        }
        todesk(&recorder, Some(ROOT_RULE));

        assert_eq!(activate(&recorder), Ok(()));
        assert_eq!(recorder.contents(JOURNAL).as_deref(), Some(TODESK_FILTER));
        assert_eq!(
            recorder.contents(FILTER_FILE).as_deref(),
            Some(TODESK_MANAGED_FILTER)
        );

        for _ in 0..3 {
            todesk(&recorder, Some(ROOT_RULE));
        }
        todesk(&recorder, None);
        assert_eq!(restore(&recorder), Ok(()));
        assert!(recorder.contents(JOURNAL).is_none());
        assert_eq!(
            recorder
                .calls()
                .iter()
                .filter(|call| matches!(call, Call::Run(_, args)
                    if args == &["-R", "-o", "none", "-f", FILTER_FILE]))
                .count(),
            2
        );
    }

    #[test]
    fn does_not_overwrite_a_todesk_rule_removed_during_the_session() {
        let recorder = stock_disk();
        recorder.write(JOURNAL, TODESK_FILTER).unwrap();
        stock(&recorder, Some(ROOT_RULE));
        stock(&recorder, Some(ROOT_RULE));
        assert_eq!(
            restore(&recorder),
            Err(NetworkError::MacosUnsupportedPfConfiguration)
        );
        assert_eq!(recorder.contents(JOURNAL).as_deref(), Some(TODESK_FILTER));
        assert!(
            !recorder
                .calls()
                .iter()
                .any(|call| matches!(call, Call::Run(..)))
        );
    }

    #[test]
    fn refuses_a_nonempty_reserved_anchor_on_the_todesk_host() {
        let recorder = stock_disk();
        for _ in 0..3 {
            todesk(&recorder, None);
        }
        recorder.reply(&["-a", ANCHOR, "-s", "rules"], Ok("pass in all"));
        assert_eq!(
            activate(&recorder),
            Err(NetworkError::MacosUnsupportedPfConfiguration)
        );
        assert!(recorder.contents(JOURNAL).is_none());
    }

    #[test]
    fn activates_only_filter_and_journals_the_exact_stock_baseline() {
        let recorder = stock_disk();
        stock(&recorder, None);
        stock(&recorder, None);
        stock(&recorder, Some(ROOT_RULE));
        assert_eq!(activate(&recorder), Ok(()));
        assert_eq!(recorder.contents(JOURNAL).as_deref(), Some(STOCK_FILTER));
        assert_eq!(
            recorder.contents(SYSTEM_CONFIG).as_deref(),
            Some(STOCK_SOURCE)
        );
        let invocations = recorder.calls.borrow();
        assert!(invocations.iter().any(|call| matches!(call, Call::Run(_, args) if args == &["-R", "-o", "none", "-f", FILTER_FILE])));
        assert!(!invocations.iter().any(|call| matches!(call, Call::Run(_, args) if args.contains(&"-F".to_owned()) || args.contains(&"-d".to_owned()))));
    }

    #[test]
    fn stops_on_host_changes_before_loading_and_keeps_recovery_evidence() {
        let recorder = stock_disk();
        stock(&recorder, None);
        recorder.reply(&["-s", "info"], Ok("Status: Enabled"));
        assert_eq!(
            activate(&recorder),
            Err(NetworkError::MacosUnsupportedPfConfiguration)
        );
        assert_eq!(recorder.contents(JOURNAL).as_deref(), Some(STOCK_FILTER));
        assert!(!recorder.calls.borrow().iter().any(|call| matches!(call, Call::Run(_, args) if args.first().is_some_and(|first| first == "-R"))));
    }

    #[test]
    fn restores_stock_only_after_release_of_the_last_pf_reference() {
        let recorder = stock_disk();
        recorder.write(JOURNAL, STOCK_FILTER).unwrap();
        stock(&recorder, Some(ROOT_RULE));
        stock(&recorder, Some(ROOT_RULE));
        stock(&recorder, Some(ROOT_RULE));
        stock(&recorder, None);
        assert_eq!(restore(&recorder), Ok(()));
        assert!(recorder.contents(JOURNAL).is_none());
    }

    #[test]
    fn recovery_retries_the_old_managed_entry_before_retiring_its_lease() {
        use super::super::lifecycle::recover_job;
        use crate::egress::cleanup::Job;
        use crate::egress::managed::RECOVERY_SERVICE;
        use crate::egress::state::GuardState;

        let recorder = stock_disk();
        recorder.write(JOURNAL, STOCK_FILTER).unwrap();
        state::write(&recorder, &GuardState::new(1234, None)).unwrap();
        let job = Job::Legacy;
        job.schedule(&recorder, RECOVERY_SERVICE).unwrap();

        recorder.reply(&["-s", "Anchors"], Ok(ANCHOR));
        for _ in 0..3 {
            stock(&recorder, Some("pass in all"));
        }
        assert_eq!(
            recover_job(&recorder, &job, true),
            Err(NetworkError::MacosUnsupportedPfConfiguration)
        );
        assert!(
            recorder
                .calls()
                .iter()
                .all(|call| !matches!(call, Call::Run(_, args)
                if args.first().is_some_and(|arg| arg == "-R" || arg == "bootout")))
        );
        assert_eq!(recorder.contents(JOURNAL).as_deref(), Some(STOCK_FILTER));
        let owner = state::read(&recorder).unwrap().unwrap();
        assert_eq!(owner.parent, 1234);
        assert_eq!(owner.generation, None);
        assert_eq!(Job::pending(&recorder).unwrap(), None);

        recorder.reply(&["-s", "Anchors"], Ok(ANCHOR));
        for _ in 0..3 {
            stock(&recorder, Some(LEGACY_ROOT_RULE));
        }
        stock(&recorder, None);
        assert_eq!(recover_job(&recorder, &job, true), Ok(()));
        assert!(recorder.contents(JOURNAL).is_none());
        assert!(state::read(&recorder).unwrap().is_none());
        assert_eq!(Job::pending(&recorder).unwrap(), None);
        let calls = recorder.calls();
        let restore = calls
            .iter()
            .position(|call| {
                matches!(call, Call::Run(_, args)
            if args.first().is_some_and(|arg| arg == "-R"))
            })
            .unwrap();
        let bootout = calls
            .iter()
            .position(|call| {
                matches!(call, Call::Run(_, args)
            if args.first().is_some_and(|arg| arg == "bootout"))
            })
            .unwrap();
        assert!(restore < bootout);
    }

    #[test]
    fn an_admins_outbound_only_entry_cannot_be_automatically_adopted() {
        let recorder = stock_disk();
        // The running PF configuration has an administrator entry without a
        // managed journal. The installed disk files happen to be stock.
        for _ in 0..3 {
            stock(&recorder, Some(LEGACY_ROOT_RULE));
        }
        assert_eq!(restore(&recorder), Ok(()));
        assert_eq!(verify_deployment(&recorder), Err(NetworkError::Platform));
        assert_eq!(
            activate(&recorder),
            Err(NetworkError::MacosUnsupportedPfConfiguration)
        );
        assert!(recorder.contents(JOURNAL).is_none());
        assert!(
            recorder
                .calls()
                .iter()
                .all(|call| matches!(call, Call::Output(..)))
        );
    }

    #[test]
    fn custom_disk_rules_are_not_an_automatic_activation_target() {
        let recorder = Recorder::with_file(SYSTEM_CONFIG, "block drop out all");
        assert_eq!(
            activate(&recorder),
            Err(NetworkError::MacosUnsupportedPfConfiguration)
        );
        assert!(recorder.calls.borrow().is_empty());
        assert!(recorder.contents(JOURNAL).is_none());
    }

    #[test]
    fn does_not_reload_if_an_apple_dispatcher_has_live_child_rules() {
        let recorder = stock_disk();
        stock(&recorder, None);
        recorder.reply(
            &["-a", "com.apple/200.AirDrop", "-s", "rules"],
            Ok("pass in on en0 all"),
        );
        recorder.reply(&["-s", "info"], Ok("Status: Enabled"));
        assert_eq!(
            activate(&recorder),
            Err(NetworkError::MacosUnsupportedPfConfiguration)
        );
        assert!(recorder.contents(JOURNAL).is_none());
    }

    #[test]
    fn loads_only_the_cvbbmso_entry_in_an_empty_kernel() {
        let recorder = stock_disk();
        for _ in 0..4 {
            recorder.reply(&["-s", "info"], Ok("Status: Disabled for 0 days"));
        }
        for _ in 0..3 {
            recorder.reply(&["-s", "rules"], Ok(""));
            recorder.reply(&["-s", "Anchors"], Ok(""));
        }
        recorder.reply(&["-s", "rules"], Ok(ROOT_RULE));
        recorder.reply(&["-s", "Anchors"], Ok(ANCHOR));
        assert_eq!(activate(&recorder), Ok(()));
        assert_eq!(recorder.contents(JOURNAL).as_deref(), Some(""));
        assert_eq!(
            recorder.contents(FILTER_FILE).as_deref(),
            Some("anchor \"com.cvbbmso.egress\"\n")
        );
    }
}
