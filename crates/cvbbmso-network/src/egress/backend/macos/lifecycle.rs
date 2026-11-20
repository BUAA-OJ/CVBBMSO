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
use crate::egress::managed;
use crate::egress::policy::Plan;
use crate::egress::state::{self, GuardState};

use super::{
    PfGuard, RULES, TOKEN, install_with, local_addresses, managed_pf, recover_with,
    verify_deployment,
};

const HELPER: &str = "/Library/PrivilegedHelperTools/com.cvbbmso.egress-watchdog";

pub(crate) struct Prepared {
    lock: File,
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
        verify_deployment(&SystemCommands(None))?;
        let lock = state::lock()?;
        let commands = SystemCommands(Some(&lock));
        verify_deployment(&commands)?;
        if state::read(&commands)?
            .is_some_and(|state| !state.expired() && process_alive(state.parent))
        {
            return Err(NetworkError::Platform);
        }
        recover(&commands, false)?;
        Ok(Self {
            lock,
            managed: false,
        })
    }

    pub(crate) fn managed() -> Result<Self, NetworkError> {
        // SAFETY: geteuid has no preconditions.
        if unsafe { libc::geteuid() } != 0 {
            return Err(NetworkError::PermissionDenied);
        }
        let lock = state::lock()?;
        let commands = SystemCommands(Some(&lock));
        if state::read(&commands)?
            .is_some_and(|state| !state.expired() && process_alive(state.parent))
        {
            return Err(NetworkError::MacosBusy);
        }
        managed::stage_recovery()?;
        managed_pf::authorize(&commands)?;
        if let Err(error) = verify_deployment(&commands).and_then(|()| recover(&commands, false)) {
            let _ = managed_pf::restore(&commands);
            return Err(error);
        }
        Ok(Self {
            lock,
            managed: true,
        })
    }

    pub(crate) fn clean_before_resolution() -> Result<(), NetworkError> {
        // SAFETY: geteuid has no preconditions.
        if unsafe { libc::geteuid() } != 0 {
            return Err(NetworkError::PermissionDenied);
        }
        let lock = state::lock()?;
        let commands = SystemCommands(Some(&lock));
        let previous = state::read(&commands)?;
        if previous
            .as_ref()
            .is_some_and(|state| !state.expired() && process_alive(state.parent))
        {
            return Err(NetworkError::MacosBusy);
        }
        if previous.is_some() || commands.read(RULES)?.is_some() || commands.read(TOKEN)?.is_some()
        {
            recover(&commands, false)?;
        }
        Ok(())
    }

    pub(crate) fn install(self, plan: &Plan) -> Result<InstallResult, NetworkError> {
        let commands = SystemCommands(Some(&self.lock));
        let mut guard_state = GuardState::new(std::process::id());
        if let Err(error) = state::write(&commands, &guard_state) {
            if self.managed {
                let _ = managed_pf::restore(&commands);
            }
            return Err(error);
        }
        let helper = if self.managed {
            managed::RECOVERY_SERVICE
        } else {
            HELPER
        };
        let mut watchdog = match Watchdog::start(helper) {
            Ok(watchdog) => watchdog,
            Err(error) => {
                recover(&commands, self.managed)?;
                return Err(error);
            }
        };
        guard_state.watchdog = watchdog.child.id();
        let result = (|| {
            state::write(&commands, &guard_state)?;
            cleanup::schedule(&commands, helper)?;
            install_with(&commands, plan, &local_addresses()?)
        })();
        match result {
            Ok(expected) => Ok(Arc::new(PfGuard {
                expected: Mutex::new(expected),
                watchdog: Mutex::new(Some(watchdog)),
                managed: self.managed,
            })),
            Err(error) => {
                recover(&commands, self.managed)?;
                watchdog.stop();
                Err(error)
            }
        }
    }
}

pub(super) fn recover(commands: &dyn Commands, managed: bool) -> Result<(), NetworkError> {
    recover_with(commands)?;
    state::remove(commands)?;
    let restored = if managed {
        managed_pf::restore(commands)
    } else {
        Ok(())
    };
    restored.and(cleanup::cancel(commands))
}

pub(super) struct Watchdog {
    child: Child,
}

impl Watchdog {
    fn start(helper: &str) -> Result<Self, NetworkError> {
        let child = std::process::Command::new(helper)
            .args(["--watch", &std::process::id().to_string()])
            .env_clear()
            .stdin(Stdio::piped())
            .spawn()
            .map_err(|_| NetworkError::Io)?;
        Ok(Self { child })
    }

    pub(super) fn owns_state(&self) -> Result<bool, NetworkError> {
        let commands = SystemCommands(None);
        Ok(state::read(&commands)?.is_some_and(|state| self.owns(&state)))
    }

    pub(super) fn check(&mut self) -> Result<GuardState, NetworkError> {
        let state = state::read(&SystemCommands(None))?
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
    run_watchdog(false)
}

pub(crate) fn managed_watchdog_entry() -> ExitCode {
    run_watchdog(true)
}

fn run_watchdog(managed: bool) -> ExitCode {
    let args = std::env::args().skip(1).collect::<Vec<_>>();
    let result = match args.as_slice() {
        [flag] if flag == "--recover" => recovery_tick(None, managed).map(drop),
        [flag, parent] if flag == "--watch" => parent
            .parse::<u32>()
            .map_err(|_| NetworkError::Policy)
            .and_then(|parent| {
                while !recovery_tick(Some(parent), managed)? {
                    std::thread::sleep(Duration::from_secs(1));
                }
                Ok(())
            }),
        _ => Err(NetworkError::Policy),
    };
    if let Err(error) = result {
        eprintln!("{error}");
        ExitCode::FAILURE
    } else {
        ExitCode::SUCCESS
    }
}

fn recovery_tick(parent: Option<u32>, managed: bool) -> Result<bool, NetworkError> {
    let lock = state::lock()?;
    let commands = SystemCommands(Some(&lock));
    let state = state::read(&commands)?;
    // SAFETY: getppid has no preconditions. The kernel reparents an orphan
    // instead of associating it with an unrelated process that reuses a PID.
    let real_parent = u32::try_from(unsafe { libc::getppid() }).ok();
    let recover_now = match (parent, state) {
        (Some(_), None) => return Ok(true),
        (Some(parent), Some(state)) => {
            if state.parent != parent
                || (state.watchdog != 0 && state.watchdog != std::process::id())
            {
                return Ok(true);
            }
            state.expired() || real_parent != Some(parent) || control_pipe_closed()?
        }
        (None, state) => state.is_none_or(|state| state.expired() || !process_alive(state.parent)),
    };
    if recover_now {
        recover(&commands, managed)?;
    }
    Ok(recover_now)
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
