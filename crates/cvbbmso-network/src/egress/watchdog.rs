//! Watchdog that reverts the guard when the client exits abnormally.
//!
//! The guard must not survive a crash of the client, so a small helper process
//! watches the client process and the state file lifetime; when either ends, it
//! runs the platform recovery. The same helper implements the TTL fallback that
//! the one-shot OS task starts.

use std::path::{Path, PathBuf};
use std::process::{Child, Command, ExitCode};
use std::time::Duration;

use crate::NetworkError;
use crate::egress::backend;
use crate::egress::state;

/// How often the helper checks the watched process.
const POLL_INTERVAL: Duration = Duration::from_secs(1);

/// A running watchdog helper process.
pub(crate) struct Watchdog {
    child: Child,
}

impl Watchdog {
    /// Starts the helper for the current process.
    ///
    /// # Errors
    ///
    /// Returns [`NetworkError::Io`] when the helper binary cannot be started.
    pub(crate) fn start(state_path: &Path) -> Result<Self, NetworkError> {
        let child = Command::new(helper_path()?)
            .arg("--parent-pid")
            .arg(std::process::id().to_string())
            .arg("--state")
            .arg(state_path)
            .spawn()
            .map_err(|_| NetworkError::Io)?;
        Ok(Self { child })
    }

    /// Returns the process id of the helper.
    pub(crate) fn pid(&self) -> u32 {
        self.child.id()
    }

    /// Stops the helper process.
    pub(crate) fn stop(mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

/// Returns the path of the watchdog helper next to the current executable.
///
/// # Errors
///
/// Returns [`NetworkError::Io`] when the executable path is unknown.
pub(crate) fn helper_path() -> Result<PathBuf, NetworkError> {
    let current = std::env::current_exe().map_err(|_| NetworkError::Io)?;
    let directory = current.parent().ok_or(NetworkError::Io)?;
    Ok(directory.join("cvbbmso-egress-watchdog"))
}

/// Watches `parent` and the state file, then reverts the guard.
///
/// `revert` is injected so the supervision logic can be tested without touching
/// the system firewall.
pub(crate) fn supervise(
    parent: u32,
    state_path: &Path,
    revert: impl FnOnce() -> Result<(), NetworkError>,
) {
    loop {
        let state_gone = state::read_from(state_path).is_none_or(|state| state.is_expired());
        if !process_alive(parent) || state_gone {
            break;
        }
        std::thread::sleep(POLL_INTERVAL);
    }
    let _ = revert();
}

/// Runs the helper binary with its command line arguments.
pub(crate) fn run_from_args(args: impl Iterator<Item = String>) -> ExitCode {
    let args = args.collect::<Vec<_>>();

    if args.iter().any(|argument| argument == "--recover") {
        // The TTL fallback only acts when the recorded lifetime is over.
        let expired = state::read().is_none_or(|state| state.is_expired());
        if expired && backend::platform::recover().is_ok() {
            let _ = state::remove();
            let _ = super::cleanup::cancel();
        }
        return ExitCode::SUCCESS;
    }

    let mut parent = None;
    let mut state_path = None;
    let mut arguments = args.into_iter();
    while let Some(argument) = arguments.next() {
        match argument.as_str() {
            "--parent-pid" => parent = arguments.next().and_then(|value| value.parse::<u32>().ok()),
            "--state" => state_path = arguments.next().map(PathBuf::from),
            _ => {}
        }
    }

    let (Some(parent), Some(state_path)) = (parent, state_path) else {
        return ExitCode::FAILURE;
    };

    supervise(parent, &state_path, backend::platform::recover);
    let _ = state::remove();
    ExitCode::SUCCESS
}

#[cfg(target_os = "macos")]
#[allow(
    unsafe_code,
    reason = "the adapter probes process liveness through libc"
)]
fn process_alive(pid: u32) -> bool {
    // SAFETY: `kill` with signal 0 performs an existence check only.
    let result = unsafe { libc::kill(pid as libc::pid_t, 0) };
    if result == 0 {
        return true;
    }
    // A process we may not signal still exists.
    std::io::Error::last_os_error().raw_os_error() == Some(libc::EPERM)
}

#[cfg(windows)]
#[allow(
    unsafe_code,
    reason = "the adapter probes process liveness through Win32"
)]
fn process_alive(pid: u32) -> bool {
    use windows_sys::Win32::Foundation::{CloseHandle, WAIT_TIMEOUT};
    use windows_sys::Win32::System::Threading::{
        OpenProcess, PROCESS_QUERY_LIMITED_INFORMATION, WaitForSingleObject,
    };

    // SAFETY: the handle is opened for a query, used immediately and closed.
    unsafe {
        let handle = OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, 0, pid);
        if handle.is_null() {
            return false;
        }
        let wait = WaitForSingleObject(handle, 0);
        CloseHandle(handle);
        wait == WAIT_TIMEOUT
    }
}

#[cfg(not(any(target_os = "macos", windows)))]
fn process_alive(_pid: u32) -> bool {
    false
}

#[cfg(test)]
mod tests {
    use super::supervise;
    use crate::NetworkError;
    use crate::egress::state::{GuardState, write_to};
    use std::cell::Cell;
    use std::time::Duration;

    #[test]
    fn an_expired_state_file_triggers_recovery() {
        let directory = tempfile::tempdir().expect("temp dir");
        let path = directory.path().join("egress.state");
        write_to(&path, &GuardState::new(Duration::ZERO)).expect("write state");

        let called = Cell::new(false);
        supervise(u32::MAX, &path, || {
            called.set(true);
            Ok(())
        });
        assert!(called.get());
    }

    #[test]
    fn a_missing_state_file_triggers_recovery() {
        let directory = tempfile::tempdir().expect("temp dir");
        let path = directory.path().join("missing.state");

        let called = Cell::new(false);
        supervise(std::process::id(), &path, || -> Result<(), NetworkError> {
            called.set(true);
            Ok(())
        });
        assert!(called.get());
    }
}
