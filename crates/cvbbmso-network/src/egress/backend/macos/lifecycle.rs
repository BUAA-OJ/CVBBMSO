//! Owns installation and recovery across the client and its privileged helpers.

use std::fs::File;
use std::path::Path;
use std::process::{Child, ExitCode, Stdio};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use crate::NetworkError;
use crate::egress::backend::InstallResult;
use crate::egress::cleanup;
use crate::egress::commands::{Commands, SystemCommands};
use crate::egress::policy::Plan;
use crate::egress::state::{self, GuardState};

use super::{
    PfGuard, has_enable_record, install_with, local_addresses, recover_with, verify_deployment,
};

pub(crate) const HELPER: &str = "/Library/PrivilegedHelperTools/com.cvbbmso.egress-watchdog";

pub(crate) struct Prepared {
    lock: File,
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
        verify_deployment(&SystemCommands::new(None))?;
        let lock = state::lock()?;
        let commands = SystemCommands::new(Some(&lock));
        verify_deployment(&commands)?;
        if state::read(&commands)?
            .is_some_and(|state| !state.expired() && process_alive(state.parent))
        {
            return Err(NetworkError::Platform);
        }
        recover(&commands)?;
        Ok(Self { lock })
    }

    pub(crate) fn install(self, plan: &Plan) -> Result<InstallResult, NetworkError> {
        let commands = SystemCommands::new(Some(&self.lock));
        let mut guard_state = GuardState::new(std::process::id());
        state::write(&commands, &guard_state)?;
        let mut watchdog = match Watchdog::start() {
            Ok(watchdog) => watchdog,
            Err(error) => {
                recover(&commands)?;
                return Err(error);
            }
        };
        guard_state.watchdog = watchdog.child.id();
        let result = (|| {
            state::write(&commands, &guard_state)?;
            cleanup::schedule(&commands)?;
            install_with(&commands, plan, &local_addresses()?)
        })();
        match result {
            Ok(expected) => Ok(Arc::new(PfGuard {
                expected: Mutex::new(expected),
                watchdog: Mutex::new(Some(watchdog)),
            })),
            Err(error) => {
                recover(&commands)?;
                watchdog.stop();
                Err(error)
            }
        }
    }
}

pub(super) fn recover(commands: &dyn Commands) -> Result<(), NetworkError> {
    recover_with(commands)?;
    state::remove(commands)?;
    cleanup::cancel(commands)
}

pub(super) struct Watchdog {
    child: Child,
}

impl Watchdog {
    fn start() -> Result<Self, NetworkError> {
        let child = std::process::Command::new(HELPER)
            .args(["--watch", &std::process::id().to_string()])
            .env_clear()
            .stdin(Stdio::piped())
            .spawn()
            .map_err(|_| NetworkError::Io)?;
        Ok(Self { child })
    }

    pub(super) fn owns_state(&self) -> Result<bool, NetworkError> {
        let commands = SystemCommands::new(None);
        Ok(state::read(&commands)?.is_some_and(|state| self.owns(&state)))
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

    fn owns(&self, state: &GuardState) -> bool {
        state.parent == std::process::id() && state.watchdog == self.child.id()
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
    let args = std::env::args().skip(1).collect::<Vec<_>>();
    let result = match args.as_slice() {
        [flag] if flag == "--recover" => recovery_tick(None).map(drop),
        [flag, parent] if flag == "--watch" => parent
            .parse::<u32>()
            .map_err(|_| NetworkError::Policy)
            .map(watch_until_recovered),
        _ => Err(NetworkError::Policy),
    };
    if let Err(error) = result {
        eprintln!("{error}");
        ExitCode::FAILURE
    } else {
        ExitCode::SUCCESS
    }
}

fn watch_until_recovered(parent: u32) {
    let mut reported = false;
    loop {
        match recovery_tick(Some(parent)) {
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

fn recovery_tick(parent: Option<u32>) -> Result<bool, NetworkError> {
    let lock = state::lock()?;
    let commands = SystemCommands::new(Some(&lock));
    let Some(state) = state::read(&commands)? else {
        return recover_without_owner(&commands, parent);
    };
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
        recover(&commands)?;
    }
    Ok(recover_now)
}

fn recover_without_owner(
    commands: &dyn Commands,
    parent: Option<u32>,
) -> Result<bool, NetworkError> {
    if cleanup::pending(commands)? {
        // PF and its owner record were already cleared. The watchdog or
        // the next scheduled run can finish just the outstanding bootout.
        cleanup::cancel(commands)?;
    } else if parent.is_none() || has_enable_record(commands)? {
        recover(commands)?;
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
    use crate::egress::backend::macos::{
        ANCHOR, ENABLE_BASELINE, ENABLE_CAPTURE, PFCTL, RULES, TOKEN,
    };
    use crate::egress::commands::testing::{Call, Recorder};

    const DOMAIN: &str = "system/com.cvbbmso.egress-cleanup";

    #[test]
    fn a_new_helper_can_resume_after_the_old_helper_clears_the_anchor_and_owner() {
        for (capture, expected_release, result) in [
            ("pf enabled\nToken : 42\n", true, Ok(true)),
            ("pf enabled\nToken : 4", false, Err(NetworkError::Io)),
        ] {
            let recorder = Recorder::with_file(ENABLE_CAPTURE, capture);
            recorder.write(ENABLE_BASELINE, "[4]").unwrap();
            recorder.write(RULES, "block drop out quick all").unwrap();
            state::write(&recorder, &GuardState::new(1234)).unwrap();

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
            assert_eq!(recover_without_owner(&recorder, Some(1234)), result);
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
                assert!(releases.is_empty());
                assert!(has_enable_record(&recorder).unwrap());
            }
        }
    }

    #[test]
    fn surviving_recovery_retries_bootout_after_the_guard_state_is_removed() {
        for parent in [Some(1234), None] {
            let recorder = Recorder::default();
            state::write(&recorder, &GuardState::new(1234)).unwrap();
            recorder.reply(&["bootout", DOMAIN], Err(NetworkError::Io));
            assert_eq!(recover(&recorder), Err(NetworkError::Io));
            assert!(state::read(&recorder).unwrap().is_none());
            assert!(cleanup::pending(&recorder).unwrap());

            recorder.calls.borrow_mut().clear();
            recorder.reply(&["bootout", DOMAIN], Err(NetworkError::Io));
            assert_eq!(
                recover_without_owner(&recorder, parent),
                Err(NetworkError::Io)
            );
            assert!(cleanup::pending(&recorder).unwrap());

            // The original RPC may have removed the task after the timeout.
            recorder.rejected(&["bootout", DOMAIN], "Boot-out failed");
            recorder.rejected(
                &["print", DOMAIN],
                "Could not find service \"com.cvbbmso.egress-cleanup\" in domain for system",
            );
            assert_eq!(recover_without_owner(&recorder, parent), Ok(true));
            assert!(!cleanup::pending(&recorder).unwrap());
            assert!(recorder.calls().iter().all(|call| match call {
                Call::Run(program, _) | Call::Output(program, _) => program == "/bin/launchctl",
            }));
        }
    }
}
