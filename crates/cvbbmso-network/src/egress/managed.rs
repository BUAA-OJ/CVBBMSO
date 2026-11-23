//! Trusted macOS service registration, session lifetime and fixed RPC operations.
#![allow(
    unsafe_code,
    reason = "the native bridge owns SMAppService and XPC objects"
)]

use std::ffi::{CString, c_char, c_int, c_void};
use std::fs::{self, File};
use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
use std::path::Path;
use std::process::ExitCode;
use std::sync::{Arc, Mutex};

use super::{EgressGuard, EgressPolicy, backend::macos, state};
use crate::NetworkError;

pub(super) const BUNDLED_SERVICE: &str =
    "/Applications/CVBBMSO.app/Contents/MacOS/cvbbmso-egress-service";
pub(super) const RECOVERY_SERVICE: &str =
    "/Library/Application Support/com.cvbbmso/cvbbmso-egress-service";
const ACCODING: &str = "https://accoding.buaa.edu.cn";
const INSTALL: u64 = 1;
const VERIFY: u64 = 2;
const SHUTDOWN: u64 = 3;
const DISCONNECTED: u64 = 4;

unsafe extern "C" {
    fn cvbbmso_register_service(team: *const c_char) -> c_int;
    fn cvbbmso_approval_settings() -> c_int;
    fn cvbbmso_validate_service(team: *const c_char, bundled: bool) -> bool;
    fn cvbbmso_validate_recovery(path: *const c_char, team: *const c_char, live_copy: bool)
    -> bool;
    fn cvbbmso_connect(team: *const c_char) -> *mut c_void;
    fn cvbbmso_request(handle: *mut c_void, operation: u64) -> c_int;
    fn cvbbmso_disconnect(handle: *mut c_void);
    fn cvbbmso_listen(
        team: *const c_char,
        context: *mut c_void,
        callback: extern "C" fn(*mut c_void, u64, u64) -> c_int,
    ) -> c_int;
}

/// An active system filter owned by the approved macOS service and this client connection.
pub struct MacosEgressGuard(Connection);

impl MacosEgressGuard {
    /// Registers the signed app service if necessary and installs and verifies the filter.
    ///
    /// # Errors
    /// May return an approval, signing, service, PF configuration or filter error.
    /// `policy` must match the two HTTPS origins sealed into the signed build.
    pub async fn install(policy: EgressPolicy) -> Result<Self, NetworkError> {
        if policy.validate()? != build_policy()?.validate()? {
            return Err(NetworkError::Policy);
        }
        let connection = blocking(move || {
            let team = trusted_team()?;
            // SAFETY: `team` is a live NUL-terminated string throughout each call.
            decode(unsafe { cvbbmso_register_service(team.as_ptr()) })?;
            // SAFETY: the native bridge returns an owned XPC handle or null.
            let raw = unsafe { cvbbmso_connect(team.as_ptr()) };
            let handle =
                std::ptr::NonNull::new(raw).ok_or(NetworkError::MacosServiceUnavailable)?;
            let connection = Connection(Arc::new(Handle(handle)));
            connection.request(INSTALL)?;
            Ok(connection)
        })
        .await?;
        Ok(Self(connection))
    }

    /// Rechecks live PF rules and refreshes addresses when due.
    ///
    /// # Errors
    /// Returns a service or guard error if the filter cannot be verified.
    pub async fn verify(&self) -> Result<(), NetworkError> {
        let connection = self.0.clone();
        blocking(move || connection.request(VERIFY)).await
    }

    /// Removes this connection's filter and releases the authenticated session.
    ///
    /// # Errors
    /// Returns a service or cleanup error if the operation cannot complete.
    pub async fn shutdown(self) -> Result<(), NetworkError> {
        blocking(move || self.0.request(SHUTDOWN)).await
    }

    /// Opens the system settings panel after an explicit user action.
    ///
    /// # Errors
    /// Returns [`NetworkError::MacosUnsupported`] on macOS before 14.
    pub fn open_approval_settings() -> Result<(), NetworkError> {
        // SAFETY: this API takes no arguments and dispatches to the main queue.
        decode(unsafe { cvbbmso_approval_settings() })
    }
}

#[derive(Clone)]
struct Connection(Arc<Handle>);

struct Handle(std::ptr::NonNull<c_void>);

// SAFETY: XPC retains the connection and allows calls across threads. The last
// Arc cancels and releases the native object after all requests have returned.
unsafe impl Send for Handle {}
// SAFETY: XPC supports concurrent sends and the bridge uses per-request replies.
unsafe impl Sync for Handle {}

impl Drop for Handle {
    fn drop(&mut self) {
        // SAFETY: the owned handle is released exactly once.
        unsafe { cvbbmso_disconnect(self.0.as_ptr()) };
    }
}

impl Connection {
    fn request(&self, operation: u64) -> Result<(), NetworkError> {
        // SAFETY: the handle outlives the native call and is safe on this thread.
        decode(unsafe { cvbbmso_request(self.0.0.as_ptr(), operation) })
    }
}

async fn blocking<T: Send + 'static>(
    call: impl FnOnce() -> Result<T, NetworkError> + Send + 'static,
) -> Result<T, NetworkError> {
    tokio::task::spawn_blocking(call)
        .await
        .map_err(|_| NetworkError::MacosServiceUnavailable)?
}

fn trusted_team() -> Result<CString, NetworkError> {
    let value = option_env!("CVBBMSO_MACOS_TEAM_ID").unwrap_or_default();
    if value.len() != 10
        || !value
            .bytes()
            .all(|c| c.is_ascii_uppercase() || c.is_ascii_digit())
    {
        return Err(NetworkError::MacosUntrustedInstallation);
    }
    CString::new(value).map_err(|_| NetworkError::MacosUntrustedInstallation)
}

fn build_policy() -> Result<EgressPolicy, NetworkError> {
    let origin = |value: &str| {
        let url = url::Url::parse(value).map_err(|_| NetworkError::Policy)?;
        if url.scheme() != "https"
            || url.host().is_none()
            || url.path() != "/"
            || url.query().is_some()
            || url.fragment().is_some()
            || !url.username().is_empty()
            || url.password().is_some()
        {
            return Err(NetworkError::Policy);
        }
        Ok(url)
    };
    Ok(EgressPolicy {
        accoding_origin: origin(ACCODING)?,
        server_origin: origin(
            option_env!("CVBBMSO_SERVER_ORIGIN").ok_or(NetworkError::MacosUntrustedInstallation)?,
        )?,
    })
}

pub(super) fn stage_recovery() -> Result<(), NetworkError> {
    const MAX_SIZE: u64 = 128 * 1024 * 1024;
    let team = trusted_team()?;
    let destination = Path::new(RECOVERY_SERVICE);
    let directory = destination.parent().ok_or(NetworkError::Io)?;
    state::check_protected(directory, true)?;
    match fs::symlink_metadata(destination) {
        Ok(_) => {
            state::check_protected(destination, false)?;
            check_recovery(destination, &team, false)?;
            if check_recovery(destination, &team, true).is_ok() {
                return Ok(());
            }
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(_) => return Err(NetworkError::Io),
    }
    let source = File::options()
        .read(true)
        .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK)
        .open(BUNDLED_SERVICE)
        .map_err(|_| NetworkError::Io)?;
    let metadata = source.metadata().map_err(|_| NetworkError::Io)?;
    if !metadata.is_file() || metadata.len() > MAX_SIZE {
        return Err(NetworkError::MacosUntrustedInstallation);
    }
    let mut staged = tempfile::NamedTempFile::new_in(directory).map_err(|_| NetworkError::Io)?;
    let mut bounded = std::io::Read::take(source, MAX_SIZE + 1);
    if std::io::copy(&mut bounded, &mut staged).map_err(|_| NetworkError::Io)? != metadata.len() {
        return Err(NetworkError::MacosUntrustedInstallation);
    }
    check_recovery(staged.path(), &team, true)?;
    staged
        .as_file()
        .set_permissions(fs::Permissions::from_mode(0o700))
        .map_err(|_| NetworkError::Io)?;
    staged.as_file().sync_all().map_err(|_| NetworkError::Io)?;
    // macOS disallows executing a file that is still open for writing.
    drop(staged.persist(destination).map_err(|_| NetworkError::Io)?);
    Ok(())
}

fn check_recovery(path: &Path, team: &CString, live_copy: bool) -> Result<(), NetworkError> {
    let path = CString::new(path.as_os_str().as_encoded_bytes()).map_err(|_| NetworkError::Io)?;
    // SAFETY: both C strings live for the duration of the call.
    if !unsafe { cvbbmso_validate_recovery(path.as_ptr(), team.as_ptr(), live_copy) } {
        return Err(NetworkError::MacosUntrustedInstallation);
    }
    Ok(())
}

pub(crate) fn service_main() -> ExitCode {
    match run_service() {
        Ok(status) => status,
        Err(error) => {
            eprintln!("macOS egress service: {error}");
            ExitCode::FAILURE
        }
    }
}

fn run_service() -> Result<ExitCode, NetworkError> {
    let team = trusted_team()?;
    let bundled = std::env::args_os().len() == 1;
    // SAFETY: `team` remains a live C string, and the bridge checks effective UID.
    if !unsafe { cvbbmso_validate_service(team.as_ptr(), bundled) } {
        return Err(NetworkError::MacosUntrustedInstallation);
    }
    if !bundled {
        if std::env::current_exe().map_err(|_| NetworkError::Io)? != Path::new(RECOVERY_SERVICE) {
            return Err(NetworkError::MacosUntrustedInstallation);
        }
        return Ok(macos::managed_watchdog_entry());
    }
    let server = Mutex::new(Service {
        policy: build_policy()?,
        runtime: tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .map_err(|_| NetworkError::Io)?,
        active: None,
    });
    // SAFETY: the context lives until the listener returns. The native bridge
    // blocks the main thread for process life after activation.
    decode(unsafe {
        cvbbmso_listen(
            team.as_ptr(),
            (&raw const server).cast_mut().cast(),
            service_request,
        )
    })?;
    Ok(ExitCode::SUCCESS)
}

extern "C" fn service_request(context: *mut c_void, session: u64, operation: u64) -> c_int {
    // SAFETY: the setup function passes a live mutex of this type to the listener.
    let service = unsafe { &*context.cast::<Mutex<Service>>() };
    encode(
        service
            .lock()
            .map_err(|_| NetworkError::Platform)
            .and_then(|mut service| service.handle(session, operation)),
    )
}

struct Service {
    policy: EgressPolicy,
    runtime: tokio::runtime::Runtime,
    active: Option<(u64, EgressGuard)>,
}

impl Service {
    fn handle(&mut self, session: u64, operation: u64) -> Result<(), NetworkError> {
        if operation == INSTALL {
            if self.active.is_some() {
                return Err(NetworkError::MacosBusy);
            }
            let guard = self.runtime.block_on(EgressGuard::install_using(
                self.policy.clone(),
                macos::prepare_managed,
                macos::install_managed,
            ))?;
            self.active = Some((session, guard));
            return Ok(());
        }
        let Some((owner, guard)) = self.active.as_ref() else {
            return if operation == DISCONNECTED {
                Ok(())
            } else {
                Err(NetworkError::Platform)
            };
        };
        if *owner != session {
            return if operation == DISCONNECTED {
                Ok(())
            } else {
                Err(NetworkError::MacosBusy)
            };
        }
        if operation == VERIFY {
            let result = self.runtime.block_on(guard.verify());
            if result.is_err() && self.runtime.block_on(guard.shutdown_inner()).is_ok() {
                self.active = None;
            }
            return result;
        }
        if operation == SHUTDOWN || operation == DISCONNECTED {
            let result = self.runtime.block_on(guard.shutdown_inner());
            if result.is_ok() || operation == DISCONNECTED {
                self.active = None;
            }
            return result;
        }
        Err(NetworkError::Platform)
    }
}

fn decode(status: c_int) -> Result<(), NetworkError> {
    Err(match status {
        0 => return Ok(()),
        1 => NetworkError::MacosUnsupported,
        2 => NetworkError::MacosUntrustedInstallation,
        3 => NetworkError::MacosApprovalRequired,
        4 => NetworkError::MacosServiceUnavailable,
        5 => NetworkError::MacosUnsupportedPfConfiguration,
        6 => NetworkError::MacosBusy,
        8 => NetworkError::Io,
        9 => NetworkError::Policy,
        10 => NetworkError::Resolution,
        _ => NetworkError::Platform,
    })
}

fn encode(result: Result<(), NetworkError>) -> c_int {
    match result {
        Ok(()) => 0,
        Err(NetworkError::MacosUnsupported) => 1,
        Err(NetworkError::MacosUntrustedInstallation | NetworkError::PermissionDenied) => 2,
        Err(NetworkError::MacosApprovalRequired) => 3,
        Err(NetworkError::MacosServiceUnavailable) => 4,
        Err(NetworkError::MacosUnsupportedPfConfiguration) => 5,
        Err(NetworkError::MacosBusy) => 6,
        Err(NetworkError::Platform) => 7,
        Err(NetworkError::Io) => 8,
        Err(NetworkError::Policy) => 9,
        Err(NetworkError::Resolution) => 10,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::egress::{
        Runtime, allow_set::AllowSet, backend::GuardBackend, policy::Plan, services::Services,
    };
    use std::sync::atomic::{AtomicUsize, Ordering};

    #[derive(Default)]
    struct Filter(AtomicUsize);

    impl GuardBackend for Filter {
        fn apply(&self, _: &Plan) -> Result<(), NetworkError> {
            Err(NetworkError::Platform)
        }
        fn verify(&self) -> Result<(), NetworkError> {
            Err(NetworkError::Platform)
        }
        fn revert(&self) -> Result<(), NetworkError> {
            self.0.fetch_add(1, Ordering::Relaxed);
            Ok(())
        }
    }

    #[test]
    fn only_the_installing_connection_can_verify_or_clear_the_filter() {
        let filter = Arc::new(Filter::default());
        let policy = EgressPolicy {
            accoding_origin: ACCODING.parse().unwrap(),
            server_origin: "https://monitor.invalid".parse().unwrap(),
        };
        let guard = EgressGuard {
            backend: filter.clone(),
            endpoints: policy.validate().unwrap(),
            runtime: Arc::new(Mutex::new(Runtime {
                allowed: AllowSet::new(std::time::Duration::ZERO),
                config: Services::default(),
                last_refresh: None,
            })),
        };
        let mut service = Service {
            policy,
            runtime: tokio::runtime::Builder::new_current_thread()
                .build()
                .unwrap(),
            active: Some((1, guard)),
        };
        for operation in [INSTALL, VERIFY, SHUTDOWN] {
            assert_eq!(service.handle(2, operation), Err(NetworkError::MacosBusy));
        }
        assert_eq!(service.handle(2, DISCONNECTED), Ok(()));
        assert_eq!(filter.0.load(Ordering::Relaxed), 0);
        assert_eq!(service.handle(1, DISCONNECTED), Ok(()));
        assert_eq!(filter.0.load(Ordering::Relaxed), 1);
        assert!(service.active.is_none());
    }

    #[test]
    fn native_status_round_trips_with_a_closed_error_mapping() {
        for status in 0..=10 {
            assert_eq!(encode(decode(status)), status);
        }
        assert_eq!(decode(-1), Err(NetworkError::Platform));
    }
}
