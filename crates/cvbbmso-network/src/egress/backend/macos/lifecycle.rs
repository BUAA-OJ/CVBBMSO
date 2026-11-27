//! Owns installation and recovery across the client and its privileged helpers.

use std::fs::File;
use std::path::Path;
use std::process::{Child, ExitCode, Stdio};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use crate::NetworkError;
use crate::egress::backend::InstallResult;
use crate::egress::cleanup::Job;
use crate::egress::commands::{Commands, Diagnostic, SystemCommands};
use crate::egress::state::{self, Generation, GuardState};
use crate::egress::{managed, policy::Plan};

use super::{
    ANCHOR, PFCTL, PfGuard, RULES, TOKEN, has_enable_record, install_with, local_addresses,
    managed_pf, recover_with, verify_deployment,
};

const HELPER: &str = "/Library/PrivilegedHelperTools/com.cvbbmso.egress-watchdog";
const CAPABILITIES: &str = "--capabilities";
const LEASE_CAPABILITY: &str = "cvbbmso-egress-leases-v2";

pub(crate) struct Prepared {
    lock: File,
    helper: &'static str,
    versioned_helper: bool,
    managed: bool,
}

impl Prepared {
    pub(crate) fn new() -> Result<Self, NetworkError> {
        // SAFETY: geteuid has no preconditions.
        if unsafe { libc::geteuid() } != 0 {
            return Err(NetworkError::PermissionDenied);
        }
        for path in ["/", "/Library", "/Library/PrivilegedHelperTools"] {
            state::check_protected(Path::new(path), true)?;
        }
        state::check_protected(Path::new(HELPER), false)?;
        let commands = SystemCommands::new(None);
        let versioned_helper = supports_versioned_leases(&commands, HELPER)?;
        verify_deployment(&commands)?;
        let lock = state::lock()?;
        let commands = SystemCommands::new(Some(&lock));
        verify_deployment(&commands)?;
        if state::read(&commands)?
            .is_some_and(|state| !state.expired() && process_alive(state.parent))
        {
            return Err(NetworkError::Platform);
        }
        recover(&commands, false)?;
        Ok(Self {
            lock,
            helper: HELPER,
            versioned_helper,
            managed: false,
        })
    }

    pub(crate) fn managed() -> Result<Self, NetworkError> {
        let lock = prepare_managed(true)?;
        Ok(Self {
            lock,
            helper: managed::RECOVERY_SERVICE,
            versioned_helper: true,
            managed: true,
        })
    }

    #[cfg(feature = "macos-pf-trial")]
    pub(crate) fn trial() -> Result<Self, NetworkError> {
        let lock = prepare_trial(true)?;
        Ok(Self {
            lock,
            helper: crate::egress::trial::HELPER,
            versioned_helper: true,
            managed: true,
        })
    }

    #[cfg(feature = "macos-pf-trial")]
    pub(crate) fn clean_before_trial() -> Result<File, NetworkError> {
        prepare_trial(false)
    }

    pub(crate) fn clean_before_resolution() -> Result<(), NetworkError> {
        prepare_managed(false).map(drop)
    }

    pub(crate) fn install(self, plan: &Plan) -> Result<InstallResult, NetworkError> {
        let commands = SystemCommands::new(Some(&self.lock));
        let generation = self
            .versioned_helper
            .then(|| Generation::allocate(&commands))
            .transpose()?;
        let mut guard_state = GuardState::new(std::process::id(), generation.clone());
        let job = Job::new(generation);
        state::write(&commands, &guard_state)?;
        let managed = self.managed;
        let mut watchdog = match Watchdog::start(job.clone(), self.helper) {
            Ok(watchdog) => watchdog,
            Err(error) => {
                recover_job(&commands, &job, managed)?;
                return Err(error);
            }
        };
        guard_state.watchdog = watchdog.child.id();
        let result = (|| {
            state::write(&commands, &guard_state)?;
            job.schedule(&commands, self.helper)?;
            // Make the recovery lease durable before modifying the main PF ruleset.
            if managed {
                managed_pf::authorize(&commands)?;
            }
            install_with(&commands, plan, &local_addresses()?)
        })();
        match result {
            Ok(expected) => Ok(Arc::new(PfGuard {
                expected: Mutex::new(expected),
                watchdog: Mutex::new(Some(watchdog)),
                managed,
            })),
            Err(error) => {
                recover_job(&commands, &job, managed)?;
                watchdog.stop();
                Err(error)
            }
        }
    }
}

fn prepare_managed(installing: bool) -> Result<File, NetworkError> {
    prepare_root_policy(installing, |commands| {
        managed::stage_recovery()?;
        require_managed_helper(commands, managed::RECOVERY_SERVICE)
    })
}

#[cfg(feature = "macos-pf-trial")]
fn prepare_trial(installing: bool) -> Result<File, NetworkError> {
    prepare_root_policy(installing, |commands| {
        crate::egress::trial::check_helper()?;
        require_managed_helper(commands, crate::egress::trial::HELPER)
    })
}

fn prepare_root_policy(
    installing: bool,
    ensure_helper: impl FnOnce(&SystemCommands<'_>) -> Result<(), NetworkError>,
) -> Result<File, NetworkError> {
    // SAFETY: geteuid has no preconditions.
    if unsafe { libc::geteuid() } != 0 {
        return Err(NetworkError::PermissionDenied);
    }
    let lock = state::lock()?;
    let commands = SystemCommands::new(Some(&lock));
    let previous = state::read(&commands)?;
    if previous
        .as_ref()
        .is_some_and(|state| !state.expired() && process_alive(state.parent))
    {
        return Err(NetworkError::MacosBusy);
    }
    let needs_recovery = previous.is_some()
        || Job::pending(&commands)?.is_some()
        || has_guard_records(&commands)?
        || managed_pf::has_journal(&commands)?;
    if installing || needs_recovery {
        ensure_helper(&commands)?;
    }
    if needs_recovery {
        recover(&commands, true)?;
    }
    Ok(lock)
}

fn require_managed_helper(commands: &dyn Commands, helper: &str) -> Result<(), NetworkError> {
    if supports_versioned_leases(commands, helper)? {
        Ok(())
    } else {
        Err(NetworkError::Platform)
    }
}

fn supports_versioned_leases(commands: &dyn Commands, helper: &str) -> Result<bool, NetworkError> {
    match commands.diagnostic(helper, &[CAPABILITIES])? {
        Diagnostic::Success(output) if output.trim() == LEASE_CAPABILITY => Ok(true),
        Diagnostic::Rejected(output) if output.trim() == NetworkError::Policy.to_string() => {
            Ok(false)
        }
        _ => Err(NetworkError::Platform),
    }
}

fn has_guard_records(commands: &dyn Commands) -> Result<bool, NetworkError> {
    Ok(commands.read(RULES)?.is_some()
        || commands.read(TOKEN)?.is_some()
        || has_enable_record(commands)?)
}

pub(super) fn recover(commands: &dyn Commands, managed: bool) -> Result<(), NetworkError> {
    if let Some(state) = state::read(commands)? {
        recover_job(commands, &Job::new(state.generation), managed)
    } else if let Some(pending) = Job::pending(commands)? {
        pending.finish_cleanup(commands)
    } else {
        recover_job(commands, &Job::Legacy, managed)
    }
}

pub(super) fn recover_job(
    commands: &dyn Commands,
    job: &Job,
    managed: bool,
) -> Result<(), NetworkError> {
    // An interrupted managed setup can have an owner before any anchor exists.
    if !managed
        || has_guard_records(commands)?
        || commands
            .output(PFCTL, &["-s", "Anchors"])?
            .lines()
            .any(|line| line.trim() == ANCHOR)
    {
        recover_with(commands)?;
    }
    managed_pf::restore(commands)?;
    job.record_cleanup(commands)?;
    state::remove(commands)?;
    job.finish_cleanup(commands)
}

pub(super) struct Watchdog {
    child: Child,
    pub(super) job: Job,
}

impl Watchdog {
    fn start(job: Job, helper: &str) -> Result<Self, NetworkError> {
        let parent = std::process::id().to_string();
        let mut command = std::process::Command::new(helper);
        match &job {
            Job::Legacy => command.args(["--watch", &parent]),
            Job::Lease(generation) => command.args(["--watch-lease", &parent, generation.as_str()]),
        };
        let child = command
            .env_clear()
            .stdin(Stdio::piped())
            .spawn()
            .map_err(|_| NetworkError::Io)?;
        Ok(Self { child, job })
    }

    pub(super) fn check(&mut self) -> Result<GuardState, NetworkError> {
        let state = state::read(&SystemCommands::new(None))?
            .filter(|state| self.owns(state) && !state.expired())
            .ok_or(NetworkError::Platform)?;
        if self
            .child
            .try_wait()
            .map_err(|_| NetworkError::Io)?
            .is_some()
        {
            return Err(NetworkError::Platform);
        }
        Ok(state)
    }

    pub(super) fn owns(&self, state: &GuardState) -> bool {
        state.parent == std::process::id()
            && state.watchdog == self.child.id()
            && matches_job(state, &self.job)
    }

    pub(super) fn stop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

fn process_alive(pid: u32) -> bool {
    let Ok(pid) = libc::pid_t::try_from(pid) else {
        return false;
    };
    // SAFETY: signal 0 checks existence; the PID is positive.
    pid > 1
        && (unsafe { libc::kill(pid, 0) } == 0
            || std::io::Error::last_os_error().raw_os_error() == Some(libc::EPERM))
}

pub(crate) fn watchdog_entry() -> ExitCode {
    run_watchdog(false)
}

pub(crate) fn managed_watchdog_entry() -> ExitCode {
    run_watchdog(true)
}

fn run_watchdog(managed: bool) -> ExitCode {
    let args = std::env::args().skip(1).collect::<Vec<_>>();
    let result = match args.as_slice() {
        [flag] if flag == CAPABILITIES => {
            println!("{LEASE_CAPABILITY}");
            Ok(())
        }
        [flag] if flag == "--recover" => recovery_tick(&Job::Legacy, None, managed).map(drop),
        [flag, generation] if flag == "--recover-lease" => {
            Generation::try_from(generation.to_owned())
                .and_then(|generation| recovery_tick(&Job::Lease(generation), None, managed))
                .map(drop)
        }
        [flag, parent] if flag == "--watch" => parent
            .parse::<u32>()
            .map_err(|_| NetworkError::Policy)
            .map(|parent| watch_until_recovered(&Job::Legacy, parent, managed)),
        [flag, parent, generation] if flag == "--watch-lease" => {
            Generation::try_from(generation.to_owned()).and_then(|generation| {
                let parent = parent.parse::<u32>().map_err(|_| NetworkError::Policy)?;
                watch_until_recovered(&Job::Lease(generation), parent, managed);
                Ok(())
            })
        }
        _ => Err(NetworkError::Policy),
    };
    if let Err(error) = result {
        eprintln!("{error}");
        ExitCode::FAILURE
    } else {
        ExitCode::SUCCESS
    }
}

fn watch_until_recovered(job: &Job, parent: u32, managed: bool) {
    let mut reported = false;
    loop {
        match recovery_tick(job, Some(parent), managed) {
            Ok(true) => return,
            Ok(false) => reported = false,
            Err(error) if !reported => {
                eprintln!("watchdog recovery will retry: {error}");
                reported = true;
            }
            Err(_) => {}
        }
        std::thread::sleep(Duration::from_secs(1));
    }
}

fn recovery_tick(job: &Job, parent: Option<u32>, managed: bool) -> Result<bool, NetworkError> {
    let lock = state::lock()?;
    let commands = SystemCommands::new(Some(&lock));
    recovery_tick_with(&commands, job, parent, managed)
}

fn recovery_tick_with(
    commands: &dyn Commands,
    job: &Job,
    parent: Option<u32>,
    managed: bool,
) -> Result<bool, NetworkError> {
    let Some(state) = state::read(commands)? else {
        return recover_without_owner(commands, job, parent, managed);
    };
    if !matches_job(&state, job) {
        return retire_if_scheduled(commands, job, parent);
    }
    // SAFETY: getppid has no preconditions. The kernel reparents an orphan
    // instead of associating it with an unrelated process that reuses a PID.
    let real_parent = u32::try_from(unsafe { libc::getppid() }).ok();
    let recover_now = match parent {
        Some(parent) => {
            if state.parent != parent
                || (state.watchdog != 0 && state.watchdog != std::process::id())
            {
                return Ok(true);
            }
            state.expired() || real_parent != Some(parent) || control_pipe_closed()?
        }
        None => state.expired() || !process_alive(state.parent),
    };
    if recover_now {
        recover_job(commands, job, managed)?;
    }
    Ok(recover_now)
}

pub(super) fn recover_without_owner(
    commands: &dyn Commands,
    job: &Job,
    parent: Option<u32>,
    managed: bool,
) -> Result<bool, NetworkError> {
    if let Some(pending) = Job::pending(commands)? {
        // PF and its owner record were already cleared. The watchdog or
        // the next scheduled run can finish just the outstanding bootout.
        if pending == *job {
            pending.finish_cleanup(commands)?;
        } else {
            return retire_if_scheduled(commands, job, parent);
        }
    } else if *job != Job::Legacy {
        return retire_if_scheduled(commands, job, parent);
    } else if parent.is_none()
        || has_enable_record(commands)?
        || (managed && managed_pf::has_journal(commands)?)
    {
        recover_job(commands, job, managed)?;
    }
    Ok(true)
}

fn matches_job(state: &GuardState, job: &Job) -> bool {
    match (job, &state.generation) {
        (Job::Legacy, None) => true,
        (Job::Lease(expected), Some(active)) => expected == active,
        _ => false,
    }
}

fn retire_if_scheduled(
    commands: &dyn Commands,
    job: &Job,
    parent: Option<u32>,
) -> Result<bool, NetworkError> {
    if matches!(job, Job::Lease(_)) && parent.is_none() {
        job.unload(commands)?;
    }
    Ok(true)
}

/// The parent owns the pipe writer for the lifetime of the guard. It closes
/// on a canceled installation, a dropped guard, or process exit.
fn control_pipe_closed() -> Result<bool, NetworkError> {
    let mut stdin = libc::pollfd {
        fd: libc::STDIN_FILENO,
        events: libc::POLLIN,
        revents: 0,
    };
    // SAFETY: stdin is one initialized pollfd. Polling with timeout zero does
    // not read from the descriptor or transfer ownership.
    let status = unsafe { libc::poll(&raw mut stdin, 1, 0) };
    if status < 0 {
        if std::io::Error::last_os_error().kind() == std::io::ErrorKind::Interrupted {
            return Ok(false);
        }
        return Err(NetworkError::Io);
    }
    Ok(stdin.revents & (libc::POLLHUP | libc::POLLERR | libc::POLLNVAL) != 0)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::egress::backend::macos::{ENABLE_BASELINE, ENABLE_CAPTURE};
    use crate::egress::commands::testing::{Call, Recorder};

    const DOMAIN: &str = "system/com.cvbbmso.egress-cleanup";

    #[test]
    fn only_the_recognized_legacy_helper_allows_downgrade() {
        let recorder = Recorder::default();
        recorder.reply(&[CAPABILITIES], Ok(LEASE_CAPABILITY));
        assert_eq!(supports_versioned_leases(&recorder, HELPER), Ok(true));
        recorder.rejected(&[CAPABILITIES], &NetworkError::Policy.to_string());
        assert_eq!(supports_versioned_leases(&recorder, HELPER), Ok(false));
        recorder.rejected(&[CAPABILITIES], "Permission denied");
        assert_eq!(
            supports_versioned_leases(&recorder, HELPER),
            Err(NetworkError::Platform)
        );
        recorder.reply(&[CAPABILITIES], Err(NetworkError::Io));
        assert_eq!(
            supports_versioned_leases(&recorder, HELPER),
            Err(NetworkError::Io)
        );
        assert!(
            recorder
                .calls()
                .iter()
                .all(|call| matches!(call, Call::Output(program, _) if program == HELPER))
        );
    }

    #[test]
    fn managed_recovery_requires_v2_on_the_protected_helper() {
        let recorder = Recorder::default();
        recorder.rejected(&[CAPABILITIES], &NetworkError::Policy.to_string());
        assert_eq!(
            require_managed_helper(&recorder, managed::RECOVERY_SERVICE),
            Err(NetworkError::Platform)
        );
        recorder.reply(&[CAPABILITIES], Ok(LEASE_CAPABILITY));
        assert_eq!(
            require_managed_helper(&recorder, managed::RECOVERY_SERVICE),
            Ok(())
        );
        assert_eq!(
            recorder.calls(),
            vec![
                Call::Output(
                    managed::RECOVERY_SERVICE.to_owned(),
                    vec![CAPABILITIES.to_owned()],
                );
                2
            ]
        );
    }

    #[test]
    fn delayed_helpers_and_unloads_cannot_recover_another_lease() {
        for first in [
            Job::Legacy,
            Job::Lease(
                Generation::try_from("10000000000000000000000000000000".to_owned()).unwrap(),
            ),
        ] {
            let recorder = Recorder::default();
            let (first_path, first_domain, absence) = match &first {
                Job::Legacy => (
                    "/Library/LaunchDaemons/com.cvbbmso.egress-cleanup.plist".to_owned(),
                    DOMAIN.to_owned(),
                    "Could not find service \"com.cvbbmso.egress-cleanup\" in domain for system"
                        .to_owned(),
                ),
                Job::Lease(generation) => {
                    let label = format!("com.cvbbmso.egress-cleanup.v2.{}", generation.as_str());
                    (
                        format!("/Library/LaunchDaemons/{label}.plist"),
                        format!("system/{label}"),
                        format!("Could not find service \"{label}\" in domain for system"),
                    )
                }
            };
            state::write(
                &recorder,
                &GuardState::new(
                    1234,
                    match &first {
                        Job::Legacy => None,
                        Job::Lease(generation) => Some(generation.clone()),
                    },
                ),
            )
            .unwrap();
            recorder.reply(&["bootstrap", "system", &first_path], Err(NetworkError::Io));
            assert_eq!(first.schedule(&recorder, HELPER), Err(NetworkError::Io));
            recorder.reply(&["bootout", &first_domain], Err(NetworkError::Io));
            assert_eq!(recover(&recorder, false), Err(NetworkError::Io));
            assert_eq!(Job::pending(&recorder).unwrap(), Some(first.clone()));
            assert!(state::read(&recorder).unwrap().is_none());
            assert!(recorder.contents(&first_path).is_none());
            recorder.rejected(&["bootout", &first_domain], "Boot-out failed");
            recorder.rejected(&["print", &first_domain], &absence);
            recover(&recorder, false).unwrap();

            let generation = Generation::allocate(&recorder).unwrap();
            let next = Job::Lease(generation.clone());
            state::write(
                &recorder,
                &GuardState::new(u32::MAX, Some(generation.clone())),
            )
            .unwrap();
            next.schedule(&recorder, HELPER).unwrap();
            recorder.write(RULES, "next lease owns PF").unwrap();
            let next_label = format!("com.cvbbmso.egress-cleanup.v2.{}", generation.as_str());
            let next_domain = format!("system/{next_label}");
            let next_path = format!("/Library/LaunchDaemons/{next_label}.plist");
            assert_ne!(first_domain, next_domain);
            assert_ne!(first_path, next_path);
            // Replay the two timed-out RPCs after the new job is live. Bootstrap
            // cannot read its old definition and bootout only names the old job.
            let mut loaded = std::collections::BTreeSet::from([next_domain.as_str()]);
            assert!(recorder.contents(&first_path).is_none());
            loaded.remove(first_domain.as_str());
            assert!(loaded.contains(next_domain.as_str()));

            recorder.calls.borrow_mut().clear();
            assert_eq!(
                recovery_tick_with(&recorder, &first, Some(1234), false),
                Ok(true)
            );
            assert_eq!(recovery_tick_with(&recorder, &first, None, false), Ok(true));
            assert!(
                recorder
                    .calls()
                    .iter()
                    .all(|call| matches!(call, Call::Run(program, args)
                if program == "/bin/launchctl" && args == &["bootout", first_domain.as_str()]))
            );
            assert_eq!(
                recorder.contents(RULES).as_deref(),
                Some("next lease owns PF")
            );
            assert!(recorder.contents(&next_path).is_some());
            assert_eq!(
                state::read(&recorder).unwrap().unwrap().generation,
                Some(generation)
            );
        }
    }

    #[test]
    fn a_new_helper_can_resume_after_the_old_helper_clears_the_anchor_and_owner() {
        for (capture, expected_release, result) in [
            (
                "Capture PID: 0000000202\npf enabled\nToken : 42\n",
                true,
                Ok(true),
            ),
            ("pf enabled\nToken : 4", false, Err(NetworkError::Io)),
        ] {
            let recorder = Recorder::with_file(ENABLE_CAPTURE, capture);
            recorder.write(ENABLE_BASELINE, "[4]").unwrap();
            recorder.write(RULES, "block drop out quick all").unwrap();
            state::write(&recorder, &GuardState::new(1234, None)).unwrap();

            // The old helper has no token at its legacy path. It flushes the
            // anchor and clears the owner and job using only its own files.
            assert!(recorder.contents(TOKEN).is_none());
            recorder.run(PFCTL, &["-a", ANCHOR, "-F", "rules"]).unwrap();
            recorder.remove(RULES).unwrap();
            state::remove(&recorder).unwrap();
            recorder
                .run("/bin/launchctl", &["bootout", DOMAIN])
                .unwrap();
            assert_eq!(recorder.contents(ENABLE_CAPTURE).as_deref(), Some(capture));
            assert_eq!(recorder.contents(ENABLE_BASELINE).as_deref(), Some("[4]"));

            recorder.reply(
                &["-s", "References"],
                Ok(concat!(
                    "TOKENS:\nPID Process Name TOKEN TIMESTAMP\n",
                    "202 pfctl 42 0 days 00:00:01\n",
                    "101 Host Firewall 4 0 days 00:00:03\n",
                )),
            );
            assert_eq!(
                recover_without_owner(&recorder, &Job::Legacy, Some(1234), false),
                result
            );
            let releases = recorder
                .calls()
                .into_iter()
                .filter_map(|call| match call {
                    Call::Run(_, args) if args.first().is_some_and(|arg| arg == "-X") => Some(args),
                    _ => None,
                })
                .collect::<Vec<_>>();
            if expected_release {
                assert_eq!(releases, [vec!["-X", "42"]]);
                assert!(!has_enable_record(&recorder).unwrap());
            } else {
                assert_eq!(releases, Vec::<Vec<String>>::new());
                assert!(has_enable_record(&recorder).unwrap());
            }
        }
    }

    #[test]
    fn surviving_recovery_retries_bootout_after_the_guard_state_is_removed() {
        for parent in [Some(1234), None] {
            let recorder = Recorder::default();
            state::write(&recorder, &GuardState::new(1234, None)).unwrap();
            recorder.reply(&["bootout", DOMAIN], Err(NetworkError::Io));
            assert_eq!(recover(&recorder, false), Err(NetworkError::Io));
            assert!(state::read(&recorder).unwrap().is_none());
            assert_eq!(Job::pending(&recorder).unwrap(), Some(Job::Legacy));

            recorder.calls.borrow_mut().clear();
            recorder.reply(&["bootout", DOMAIN], Err(NetworkError::Io));
            assert_eq!(
                recover_without_owner(&recorder, &Job::Legacy, parent, false),
                Err(NetworkError::Io)
            );
            assert_eq!(Job::pending(&recorder).unwrap(), Some(Job::Legacy));

            // The original RPC may have removed the task after the timeout.
            recorder.rejected(&["bootout", DOMAIN], "Boot-out failed");
            recorder.rejected(
                &["print", DOMAIN],
                "Could not find service \"com.cvbbmso.egress-cleanup\" in domain for system",
            );
            assert_eq!(
                recover_without_owner(&recorder, &Job::Legacy, parent, false),
                Ok(true)
            );
            assert_eq!(Job::pending(&recorder).unwrap(), None);
            assert!(recorder.calls().iter().all(|call| match call {
                Call::Run(program, _) | Call::Output(program, _) => program == "/bin/launchctl",
            }));
        }
    }
}
