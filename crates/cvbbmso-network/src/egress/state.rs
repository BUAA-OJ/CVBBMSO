//! Guard state persisted between runs of the application.

use std::fs;
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};

use crate::NetworkError;

/// Format version of the state file.
const STATE_VERSION: u32 = 1;

/// State that lets the next start clean up rules left behind by a crash.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub(crate) struct GuardState {
    /// Format version; a newer file is never interpreted.
    pub(crate) version: u32,
    /// Unix timestamp after which the guard must be treated as abandoned.
    pub(crate) expires_at_unix: i64,
    /// Process id of the watchdog that owns the cleanup.
    pub(crate) watchdog_pid: u32,
}

impl GuardState {
    /// Creates state that expires after `ttl`.
    pub(crate) fn new(ttl: Duration) -> Self {
        let mut state = Self {
            version: STATE_VERSION,
            expires_at_unix: 0,
            watchdog_pid: 0,
        };
        state.renew(ttl);
        state
    }

    /// Pushes the expiry into the future relative to now.
    pub(crate) fn renew(&mut self, ttl: Duration) {
        self.expires_at_unix = unix_now().saturating_add(duration_secs(ttl));
    }

    /// Returns whether the recorded lifetime is over.
    pub(crate) fn is_expired(&self) -> bool {
        self.version < STATE_VERSION || self.expires_at_unix <= unix_now()
    }

    /// Records the watchdog process that owns the cleanup.
    pub(crate) fn set_watchdog(&mut self, pid: u32) {
        self.watchdog_pid = pid;
    }
}

fn unix_now() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |elapsed| {
            i64::try_from(elapsed.as_secs()).unwrap_or(i64::MAX)
        })
}

fn duration_secs(ttl: Duration) -> i64 {
    i64::try_from(ttl.as_secs()).unwrap_or(i64::MAX)
}

/// Directory that holds the guard state.
pub(crate) fn directory() -> PathBuf {
    platform_directory()
}

/// Path of the guard state file.
pub(crate) fn path() -> PathBuf {
    directory().join("egress.state")
}

/// Writes the state atomically, replacing an existing file.
///
/// # Errors
///
/// Returns [`NetworkError::Io`] when the directory or file cannot be written.
pub(crate) fn write(state: &GuardState) -> Result<(), NetworkError> {
    write_to(&path(), state)
}

/// Writes the state to `path` atomically, replacing an existing file.
///
/// # Errors
///
/// Returns [`NetworkError::Io`] when the directory or file cannot be written.
pub(crate) fn write_to(path: &Path, state: &GuardState) -> Result<(), NetworkError> {
    let directory = path.parent().ok_or(NetworkError::Io)?;
    fs::create_dir_all(directory).map_err(|_| NetworkError::Io)?;
    let encoded = serde_json::to_vec(state).map_err(|_| NetworkError::Io)?;
    let temporary = path.with_extension("tmp");
    fs::write(&temporary, &encoded).map_err(|_| NetworkError::Io)?;
    fs::rename(&temporary, path).map_err(|_| NetworkError::Io)
}

/// Reads the state file, if one exists and can be parsed.
pub(crate) fn read() -> Option<GuardState> {
    read_from(&path())
}

/// Reads the state file at `path`, if it exists and can be parsed.
pub(crate) fn read_from(path: &Path) -> Option<GuardState> {
    let encoded = fs::read(path).ok()?;
    serde_json::from_slice(&encoded).ok()
}

/// Removes the state file. Succeeds when there is nothing to remove.
///
/// # Errors
///
/// Returns [`NetworkError::Io`] when an existing file cannot be removed.
pub(crate) fn remove() -> Result<(), NetworkError> {
    match fs::remove_file(path()) {
        Ok(()) => Ok(()),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(_) => Err(NetworkError::Io),
    }
}

#[cfg(target_os = "macos")]
fn platform_directory() -> PathBuf {
    PathBuf::from("/Library/Application Support/com.cvbbmso")
}

#[cfg(windows)]
fn platform_directory() -> PathBuf {
    std::env::var_os("ProgramData").map_or_else(
        || PathBuf::from(r"C:\ProgramData").join("cvbbmso"),
        |root| PathBuf::from(root).join("cvbbmso"),
    )
}

#[cfg(not(any(target_os = "macos", windows)))]
fn platform_directory() -> PathBuf {
    std::env::temp_dir().join("cvbbmso")
}

#[cfg(test)]
mod tests {
    use super::GuardState;
    use std::time::Duration;

    #[test]
    fn a_fresh_state_is_live() {
        let state = GuardState::new(Duration::from_secs(60));
        assert!(!state.is_expired());
    }

    #[test]
    fn a_zero_ttl_state_is_expired_at_once() {
        let state = GuardState::new(Duration::ZERO);
        assert!(state.is_expired());
    }

    #[test]
    fn renewal_pushes_the_expiry() {
        let mut state = GuardState::new(Duration::ZERO);
        assert!(state.is_expired());
        state.renew(Duration::from_secs(60));
        assert!(!state.is_expired());
    }

    #[test]
    fn the_watchdog_pid_is_recorded() {
        let mut state = GuardState::new(Duration::from_secs(60));
        state.set_watchdog(4242);
        assert_eq!(state.watchdog_pid, 4242);
    }
}
