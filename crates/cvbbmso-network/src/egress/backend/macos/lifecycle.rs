//! Owns installation and recovery across the client and its privileged helpers.

use std::fs::File;
use std::process::{Child, Stdio};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use crate::NetworkError;
use crate::egress::backend::InstallResult;
use crate::egress::cleanup::Job;
use crate::egress::commands::{Commands, Diagnostic, SystemCommands};
use crate::egress::policy::Plan;
use crate::egress::session::{HELPER, check_helper};
use crate::egress::state::{self, Generation, GuardState};

use super::{
    ANCHOR, PFCTL, PfGuard, RULES, TOKEN, has_enable_record, install_with, local_addresses,
    managed_pf, recover_with,
};

const CAPABILITIES: &str = "--capabilities";
const LEASE_CAPABILITY: &str = "cvbbmso-egress-leases-v2";

pub(crate) struct Prepared(File);

impl Prepared {
    pub(crate) fn new() -> Result<Self, NetworkError> {
        prepare(true).map(Self)
    }

    pub(crate) fn install(self, plan: &Plan) -> Result<InstallResult, NetworkError> {
        let commands = SystemCommands::new(Some(&self.0));
        let generation = Generation::allocate(&commands)?;
        let mut guard_state = GuardState::new(std::process::id(), Some(generation.clone()));
        let job = Job::Lease(generation.clone());
        state::write(&commands, &guard_state)?;
        let mut watchdog = match Watchdog::start(&generation) {
            Ok(watchdog) => watchdog,
            Err(error) => {
                recover_job(&commands, &job)?;
                return Err(error);
            }
        };
        guard_state.watchdog = watchdog.child.id();
        let result = (|| {
            state::write(&commands, &guard_state)?;
            Job::schedule(&commands, &generation, HELPER)?;
            // Register recovery before changing the main PF ruleset.
            managed_pf::authorize(&commands)?;
            install_with(&commands, plan, &local_addresses()?)
        })();
        match result {
            Ok(expected) => Ok(Arc::new(PfGuard {
                expected: Mutex::new(expected),
                watchdog: Mutex::new(Some(watchdog)),
            })),
            Err(error) => {
                recover_job(&commands, &job)?;
                watchdog.stop();
                Err(error)
            }
        }
    }
}

pub(super) fn prepare(installing: bool) -> Result<File, NetworkError> {
    check_helper()?;
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
        require_helper(&commands)?;
    }
    if needs_recovery {
        recover(&commands)?;
    }
    Ok(lock)
}

fn require_helper(commands: &dyn Commands) -> Result<(), NetworkError> {
    match commands.diagnostic(HELPER, &[CAPABILITIES])? {
        Diagnostic::Success(output) if output.trim() == LEASE_CAPABILITY => Ok(()),
        _ => Err(NetworkError::Platform),
    }
}

fn has_guard_records(commands: &dyn Commands) -> Result<bool, NetworkError> {
    Ok(commands.read(RULES)?.is_some()
        || commands.read(TOKEN)?.is_some()
        || has_enable_record(commands)?)
}

pub(super) fn recover(commands: &dyn Commands) -> Result<(), NetworkError> {
    if let Some(state) = state::read(commands)? {
        recover_job(commands, &Job::new(state.generation))
    } else if let Some(pending) = Job::pending(commands)? {
        pending.finish_cleanup(commands)
    } else {
        recover_job(commands, &Job::Legacy)
    }
}

pub(super) fn recover_job(commands: &dyn Commands, job: &Job) -> Result<(), NetworkError> {
    // An interrupted managed setup can have an owner before any anchor exists.
    if has_guard_records(commands)?
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
    fn start(generation: &Generation) -> Result<Self, NetworkError> {
        let parent = std::process::id().to_string();
        let child = std::process::Command::new(HELPER)
            .args(["--watch-lease", &parent, generation.as_str()])
            .env_clear()
            .stdin(Stdio::piped())
            .stdout(Stdio::null())
            .spawn()
            .map_err(|_| NetworkError::Io)?;
        Ok(Self {
            child,
            job: Job::Lease(generation.clone()),
        })
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

pub(crate) fn run_watchdog(args: &[String]) -> Result<(), NetworkError> {
    match args {
        [flag] if flag == CAPABILITIES => {
            println!("{LEASE_CAPABILITY}");
            Ok(())
        }
        [flag] if flag == "--recover" => recovery_tick(&Job::Legacy, None).map(drop),
        [flag, generation] if flag == "--recover-lease" => {
            Generation::try_from(generation.to_owned())
                .and_then(|generation| recovery_tick(&Job::Lease(generation), None))
                .map(drop)
        }
        [flag, parent] if flag == "--watch" => parent
            .parse::<u32>()
            .map_err(|_| NetworkError::Policy)
            .map(|parent| watch_until_recovered(&Job::Legacy, parent)),
        [flag, parent, generation] if flag == "--watch-lease" => {
            Generation::try_from(generation.to_owned()).and_then(|generation| {
                let parent = parent.parse::<u32>().map_err(|_| NetworkError::Policy)?;
                watch_until_recovered(&Job::Lease(generation), parent);
                Ok(())
            })
        }
        _ => Err(NetworkError::Policy),
    }
}

fn watch_until_recovered(job: &Job, parent: u32) {
    let mut reported = false;
    loop {
        match recovery_tick(job, Some(parent)) {
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

fn recovery_tick(job: &Job, parent: Option<u32>) -> Result<bool, NetworkError> {
    let lock = state::lock()?;
    let commands = SystemCommands::new(Some(&lock));
    recovery_tick_with(&commands, job, parent)
}

fn recovery_tick_with(
    commands: &dyn Commands,
    job: &Job,
    parent: Option<u32>,
) -> Result<bool, NetworkError> {
    let Some(state) = state::read(commands)? else {
        return recover_without_owner(commands, job, parent);
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
        recover_job(commands, job)?;
    }
    Ok(recover_now)
}

pub(super) fn recover_without_owner(
    commands: &dyn Commands,
    job: &Job,
    parent: Option<u32>,
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
    } else if parent.is_none() || has_enable_record(commands)? || managed_pf::has_journal(commands)?
    {
        recover_job(commands, job)?;
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
    fn recovery_requires_v2_on_the_protected_helper() {
        let recorder = Recorder::default();
        recorder.rejected(&[CAPABILITIES], &NetworkError::Policy.to_string());
        assert_eq!(require_helper(&recorder), Err(NetworkError::Platform));
        recorder.reply(&[CAPABILITIES], Ok(LEASE_CAPABILITY));
        assert_eq!(require_helper(&recorder), Ok(()));
        assert_eq!(
            recorder.calls(),
            vec![Call::Output(HELPER.to_owned(), vec![CAPABILITIES.to_owned()]); 2]
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
            // The interrupted install left its existing job definition.
            recorder.write(&first_path, "old job definition").unwrap();
            recorder.reply(&["bootout", &first_domain], Err(NetworkError::Io));
            assert_eq!(recover(&recorder), Err(NetworkError::Io));
            assert_eq!(Job::pending(&recorder).unwrap(), Some(first.clone()));
            assert!(state::read(&recorder).unwrap().is_none());
            assert!(recorder.contents(&first_path).is_none());
            recorder.rejected(&["bootout", &first_domain], "Boot-out failed");
            recorder.rejected(&["print", &first_domain], &absence);
            recover(&recorder).unwrap();

            let generation = Generation::allocate(&recorder).unwrap();
            state::write(
                &recorder,
                &GuardState::new(u32::MAX, Some(generation.clone())),
            )
            .unwrap();
            Job::schedule(&recorder, &generation, HELPER).unwrap();
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
            assert_eq!(recovery_tick_with(&recorder, &first, Some(1234)), Ok(true));
            assert_eq!(recovery_tick_with(&recorder, &first, None), Ok(true));
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
                recover_without_owner(&recorder, &Job::Legacy, Some(1234)),
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
            assert_eq!(recover(&recorder), Err(NetworkError::Io));
            assert!(state::read(&recorder).unwrap().is_none());
            assert_eq!(Job::pending(&recorder).unwrap(), Some(Job::Legacy));

            recorder.calls.borrow_mut().clear();
            recorder.reply(&["bootout", DOMAIN], Err(NetworkError::Io));
            assert_eq!(
                recover_without_owner(&recorder, &Job::Legacy, parent),
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
                recover_without_owner(&recorder, &Job::Legacy, parent),
                Ok(true)
            );
            assert_eq!(Job::pending(&recorder).unwrap(), None);
            assert!(recorder.calls().iter().all(|call| match call {
                Call::Run(program, _) | Call::Output(program, _) => program == "/bin/launchctl",
            }));
        }
    }
}
