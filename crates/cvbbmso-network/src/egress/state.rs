//! macOS recovery state shared by the guard, watchdog and launchd.
#![allow(
    unsafe_code,
    reason = "Darwin supplies the interprocess lock and ACL checks"
)]

use std::ffi::{CString, c_void};
use std::fs::{self, DirBuilder, File};
use std::os::fd::AsRawFd;
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::{DirBuilderExt, MetadataExt, OpenOptionsExt};
use std::path::Path;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};

use super::commands::Commands;
use crate::NetworkError;

const DIRECTORY: &str = "/Library/Application Support/com.cvbbmso";
const STATE: &str = "/Library/Application Support/com.cvbbmso/egress.state";
const LOCK: &str = "/Library/Application Support/com.cvbbmso/egress.lock";
const LEASE_COUNTER: &str = "/Library/Application Support/com.cvbbmso/egress.lease-counter";
const TTL_SECONDS: u64 = 15 * 60;
const RENEW_AFTER_SECONDS: u64 = 5 * 60;

#[derive(Deserialize, Serialize)]
pub(crate) struct GuardState {
    version: u32,
    pub(crate) parent: u32,
    pub(crate) watchdog: u32,
    expires_at_unix: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) generation: Option<Generation>,
}

impl GuardState {
    pub(crate) fn new(parent: u32, generation: Option<Generation>) -> Self {
        let mut state = Self {
            version: if generation.is_some() { 3 } else { 2 },
            parent,
            watchdog: 0,
            expires_at_unix: 0,
            generation,
        };
        state.renew();
        state
    }

    pub(crate) fn renew(&mut self) {
        self.expires_at_unix = unix_now().saturating_add(TTL_SECONDS);
    }

    pub(crate) fn expired(&self) -> bool {
        self.expires_at_unix <= unix_now()
    }

    pub(crate) fn renewal_due(&self) -> bool {
        self.expires_at_unix.saturating_sub(unix_now()) <= TTL_SECONDS - RENEW_AFTER_SECONDS
    }
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(try_from = "String")]
pub(crate) struct Generation(String);

impl Generation {
    /// Allocates a lease name under the recovery lock. The counter prevents
    /// reuse, and entropy keeps names independent across fresh deployments.
    pub(crate) fn allocate(commands: &dyn Commands) -> Result<Self, NetworkError> {
        let current = commands.read(LEASE_COUNTER)?;
        let current = current.as_deref().unwrap_or("0");
        let next = current
            .parse::<u64>()
            .ok()
            .and_then(|value| value.checked_add(1))
            .ok_or(NetworkError::Io)?;
        let nonce = lease_nonce()?;
        commands.write(LEASE_COUNTER, &next.to_string())?;
        Ok(Self(format!("{next:016x}{nonce:016x}")))
    }

    pub(crate) fn as_str(&self) -> &str {
        &self.0
    }
}

fn lease_nonce() -> Result<u64, NetworkError> {
    let mut nonce = 0u64;
    // SAFETY: nonce is an initialized 8-byte output buffer.
    if unsafe { libc::getentropy((&raw mut nonce).cast(), size_of::<u64>()) } != 0 {
        return Err(NetworkError::Io);
    }
    Ok(nonce)
}

impl TryFrom<String> for Generation {
    type Error = NetworkError;

    fn try_from(value: String) -> Result<Self, Self::Error> {
        if value.len() == 32
            && value
                .bytes()
                .all(|byte| matches!(byte, b'0'..=b'9' | b'a'..=b'f'))
        {
            Ok(Self(value))
        } else {
            Err(NetworkError::Policy)
        }
    }
}

fn unix_now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |elapsed| elapsed.as_secs())
}

pub(crate) fn write(commands: &dyn Commands, state: &GuardState) -> Result<(), NetworkError> {
    let encoded = serde_json::to_string(state).map_err(|_| NetworkError::Io)?;
    commands.write(STATE, &encoded)
}

pub(crate) fn read(commands: &dyn Commands) -> Result<Option<GuardState>, NetworkError> {
    let Some(encoded) = commands.read(STATE)? else {
        return Ok(None);
    };
    let encoded =
        serde_json::from_str::<serde_json::Value>(&encoded).map_err(|_| NetworkError::Io)?;
    match encoded.get("version").and_then(serde_json::Value::as_u64) {
        Some(1) => Ok(None), // The original implementation did not identify the owner.
        Some(2 | 3) => {
            let state =
                serde_json::from_value::<GuardState>(encoded).map_err(|_| NetworkError::Io)?;
            if (state.version == 3) != state.generation.is_some() {
                return Err(NetworkError::Io);
            }
            Ok(Some(state))
        }
        _ => Err(NetworkError::Io),
    }
}

pub(crate) fn remove(commands: &dyn Commands) -> Result<(), NetworkError> {
    commands.remove(STATE)
}

pub(crate) fn lock() -> Result<File, NetworkError> {
    for directory in ["/", "/Library", "/Library/Application Support"] {
        check_protected(Path::new(directory), true)?;
    }
    match DirBuilder::new().mode(0o700).create(DIRECTORY) {
        Ok(()) => {}
        Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {}
        Err(_) => return Err(NetworkError::Io),
    }
    check_protected(Path::new(DIRECTORY), true)?;
    let file = File::options()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .mode(0o600)
        .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK)
        .open(LOCK)
        .map_err(|_| NetworkError::Io)?;
    check_protected(Path::new(LOCK), false)?;
    lock_exclusive(&file, Duration::from_secs(15))?;
    Ok(file)
}

pub(super) fn lock_exclusive(file: &File, timeout: Duration) -> Result<(), NetworkError> {
    let deadline = Instant::now() + timeout;
    // SAFETY: file owns an open descriptor. Closing it releases the lock once
    // all children that inherited the same open file description have exited.
    while unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) } != 0 {
        if !matches!(
            std::io::Error::last_os_error().raw_os_error(),
            Some(libc::EINTR | libc::EWOULDBLOCK)
        ) {
            return Err(NetworkError::Io);
        }
        let remaining = deadline.saturating_duration_since(Instant::now());
        if remaining.is_zero() {
            return Err(NetworkError::Io);
        }
        std::thread::sleep(remaining.min(Duration::from_millis(20)));
    }
    Ok(())
}

pub(crate) fn check_protected(path: &Path, directory: bool) -> Result<(), NetworkError> {
    let metadata = fs::symlink_metadata(path).map_err(|_| NetworkError::Io)?;
    if metadata.uid() != 0
        || metadata.mode() & 0o022 != 0
        || if directory {
            !metadata.is_dir()
        } else {
            !metadata.is_file() || metadata.nlink() != 1
        }
        || has_acl(path)?
    {
        return Err(NetworkError::PermissionDenied);
    }
    Ok(())
}

unsafe extern "C" {
    fn acl_get_link_np(path: *const libc::c_char, acl_type: libc::c_uint) -> *mut c_void;
    fn acl_get_entry(acl: *mut c_void, id: libc::c_int, entry: *mut *mut c_void) -> libc::c_int;
    fn acl_free(acl: *mut c_void) -> libc::c_int;
}

fn has_acl(path: &Path) -> Result<bool, NetworkError> {
    const ACL_TYPE_EXTENDED: libc::c_uint = 0x100;
    const ACL_FIRST_ENTRY: libc::c_int = 0;
    let path = CString::new(path.as_os_str().as_bytes()).map_err(|_| NetworkError::Io)?;
    // SAFETY: path is a live null-terminated string. The ACL is freed below.
    let acl = unsafe { acl_get_link_np(path.as_ptr(), ACL_TYPE_EXTENDED) };
    if acl.is_null() {
        return match std::io::Error::last_os_error().raw_os_error() {
            Some(libc::ENOENT | libc::ENOATTR) => Ok(false),
            _ => Err(NetworkError::Io),
        };
    }
    let mut entry = std::ptr::null_mut();
    // SAFETY: acl is live; acl_get_entry writes a borrowed entry pointer.
    let status = unsafe { acl_get_entry(acl, ACL_FIRST_ENTRY, &raw mut entry) };
    let error = std::io::Error::last_os_error();
    // SAFETY: acl is the allocation returned by acl_get_link_np above.
    unsafe { acl_free(acl) };
    match (status, error.raw_os_error()) {
        (0, _) => Ok(true),
        (-1, Some(libc::EINVAL)) => Ok(false),
        _ => Err(NetworkError::Io),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::egress::commands::testing::Recorder;

    #[test]
    fn a_new_state_round_trips_before_the_watchdog_is_spawned() {
        let recorder = Recorder::default();
        write(&recorder, &GuardState::new(1234, None)).unwrap();
        let state = read(&recorder).unwrap().unwrap();
        assert_eq!(state.parent, 1234);
        assert_eq!(state.watchdog, 0);
        assert!(!state.expired());
        assert!(!state.renewal_due());
    }

    #[test]
    fn versioned_state_carries_a_distinct_generation_and_rejects_missing_generations() {
        let recorder = Recorder::default();
        let first = Generation::allocate(&recorder).unwrap();
        let second = Generation::allocate(&recorder).unwrap();
        assert_ne!(first, second);
        assert!(first.as_str().starts_with("0000000000000001"));
        assert!(second.as_str().starts_with("0000000000000002"));
        write(&recorder, &GuardState::new(1234, Some(second.clone()))).unwrap();
        assert_eq!(read(&recorder).unwrap().unwrap().generation, Some(second));
        recorder
            .write(
                STATE,
                r#"{"version":3,"parent":1234,"watchdog":0,"expires_at_unix":0}"#,
            )
            .unwrap();
        assert_eq!(read(&recorder).err(), Some(NetworkError::Io));
    }

    #[test]
    fn protected_install_paths_reject_a_symlink() {
        check_protected(Path::new("/"), true).expect("the macOS root is protected");
        let dir = tempfile::tempdir().unwrap();
        let link = dir.path().join("helper");
        std::os::unix::fs::symlink("/sbin/pfctl", &link).unwrap();
        assert_eq!(
            check_protected(&link, false),
            Err(NetworkError::PermissionDenied)
        );
    }

    #[test]
    fn recovery_leaves_unknown_state_versions_in_place() {
        let recorder = Recorder::with_file(STATE, r#"{"version":4}"#);
        assert_eq!(read(&recorder).err(), Some(NetworkError::Io));
        assert!(recorder.contents(STATE).is_some());
    }
}
