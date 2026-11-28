use std::sync::{
    Arc,
    atomic::{AtomicBool, AtomicUsize, Ordering},
};

use cvbbmso_network::NetworkError;
use tokio::sync::Notify;

use super::Failure;
#[cfg(target_os = "macos")]
use super::Status;

#[cfg(target_os = "macos")]
pub(in crate::desktop) fn check_startup_network_recovery() {
    for failure in [
        Failure::InspectionFailed,
        Failure::ProxyConfigured,
        Failure::ProxyUnknown,
        Failure::TunnelConfigured,
        Failure::TunnelUnknown,
        Failure::MonitorUnreachable,
    ] {
        let bootstrap = Status::Blocked(failure.before_guard(false));
        assert_eq!(bootstrap, Status::Blocked(failure));
        assert!(!bootstrap.can_start(false));
        assert!(bootstrap.can_start(true));
        assert!(bootstrap.require_ready().is_err());

        let installed = Status::Blocked(failure.before_guard(true));
        assert_eq!(
            installed,
            Status::Blocked(Failure::MacosNetworkRestartRequired)
        );
        assert!(!installed.can_start(false));
        assert!(!installed.can_start(true));
        assert!(
            installed
                .require_ready()
                .unwrap_err()
                .contains("从「应用程序」重新打开")
        );
    }
    assert_eq!(
        Failure::MonitorUnconfigured.before_guard(true),
        Failure::MonitorUnconfigured
    );
    let rejected = Status::Blocked(super::install_failure(
        NetworkError::MacosServiceUnavailable,
    ));
    assert!(!rejected.can_start(true));
    assert!(
        rejected
            .require_ready()
            .unwrap_err()
            .contains("从「应用程序」重新打开")
    );
    println!("macOS startup network recovery checks passed.");
}

#[derive(Clone, Default)]
pub(in crate::desktop) struct Admission(Arc<Counters>);

#[derive(Default)]
struct Counters {
    proxy_once: AtomicBool,
    runtime_proxy: AtomicBool,
    #[cfg(target_os = "macos")]
    screen_once: AtomicBool,
    #[cfg(target_os = "macos")]
    pf_blocked: AtomicBool,
    pause_install: AtomicBool,
    fail_verify: AtomicBool,
    fail_shutdown: AtomicBool,
    installs: AtomicUsize,
    shutdowns: AtomicUsize,
    verifies: AtomicUsize,
    network_checks: AtomicUsize,
    pause_network: AtomicBool,
    network_paused: Notify,
    resume_network: Notify,
    installing: Notify,
    resume: Notify,
}

impl Admission {
    pub(in crate::desktop) fn pause(&self) {
        self.0.pause_install.store(true, Ordering::Relaxed);
    }

    pub(in crate::desktop) fn require_retry(&self) {
        self.pause();
        self.0.proxy_once.store(true, Ordering::Relaxed);
        #[cfg(target_os = "macos")]
        {
            self.0.screen_once.store(true, Ordering::Relaxed);
            self.0.pf_blocked.store(true, Ordering::Relaxed);
        }
    }

    pub(in crate::desktop) async fn install_started(&self) {
        self.0.installing.notified().await;
    }

    pub(in crate::desktop) fn resume(&self) {
        self.0.resume.notify_one();
    }

    pub(in crate::desktop) fn lose_guard(&self) {
        self.0.fail_verify.store(true, Ordering::Relaxed);
    }

    pub(in crate::desktop) fn set_runtime_proxy(&self, enabled: bool) {
        self.0.runtime_proxy.store(enabled, Ordering::Relaxed);
    }

    pub(in crate::desktop) fn pause_runtime_network(&self) {
        self.0.pause_network.store(true, Ordering::Relaxed);
    }

    pub(in crate::desktop) async fn runtime_network_paused(&self) {
        self.0.network_paused.notified().await;
    }

    pub(in crate::desktop) fn resume_runtime_network(&self) {
        self.0.resume_network.notify_one();
    }

    pub(in crate::desktop) fn network_checks(&self) -> usize {
        self.0.network_checks.load(Ordering::Relaxed)
    }

    pub(in crate::desktop) fn fail_shutdown(&self) {
        self.0.fail_shutdown.store(true, Ordering::Relaxed);
    }

    pub(in crate::desktop) fn installs(&self) -> usize {
        self.0.installs.load(Ordering::Relaxed)
    }

    pub(in crate::desktop) fn shutdowns(&self) -> usize {
        self.0.shutdowns.load(Ordering::Relaxed)
    }

    pub(in crate::desktop) fn verifies(&self) -> usize {
        self.0.verifies.load(Ordering::Relaxed)
    }

    pub(super) fn check_network(&self) -> Result<(), Failure> {
        if self.0.proxy_once.swap(false, Ordering::Relaxed) {
            return Err(Failure::ProxyConfigured);
        }
        Ok(())
    }

    #[cfg(target_os = "macos")]
    pub(in crate::desktop) fn enable_pf(&self) {
        self.0.pf_blocked.store(false, Ordering::Relaxed);
    }

    #[cfg(target_os = "macos")]
    pub(super) fn check_helper(&self) -> Result<(), Failure> {
        if self.0.pf_blocked.load(Ordering::Relaxed) {
            return Err(Failure::MacosUnsupportedPfConfiguration);
        }
        Ok(())
    }

    pub(super) async fn install(&self) -> Guard {
        self.0.installs.fetch_add(1, Ordering::Relaxed);
        self.0.installing.notify_one();
        if self.0.pause_install.swap(false, Ordering::Relaxed) {
            self.0.resume.notified().await;
        }
        Guard(Arc::clone(&self.0))
    }

    #[cfg(target_os = "macos")]
    pub(super) fn screen(&self) -> Result<(), Failure> {
        if self.0.screen_once.swap(false, Ordering::Relaxed) {
            Err(Failure::ScreenPermission)
        } else {
            Ok(())
        }
    }
}

#[derive(Clone)]
pub(super) struct Guard(Arc<Counters>);

impl Guard {
    pub(super) async fn check_network(&self) -> Result<(), Failure> {
        self.0.network_checks.fetch_add(1, Ordering::Relaxed);
        if self.0.pause_network.swap(false, Ordering::Relaxed) {
            self.0.network_paused.notify_one();
            self.0.resume_network.notified().await;
        }
        if self.0.runtime_proxy.load(Ordering::Relaxed) {
            return Err(Failure::ProxyConfigured);
        }
        Ok(())
    }

    pub(super) fn verify(&self) -> bool {
        self.0.verifies.fetch_add(1, Ordering::Relaxed);
        !self.0.fail_verify.load(Ordering::Relaxed)
    }

    pub(super) fn shutdown(&self) -> Result<(), NetworkError> {
        self.0.shutdowns.fetch_add(1, Ordering::Relaxed);
        if self.0.fail_shutdown.load(Ordering::Relaxed) {
            return Err(NetworkError::PermissionDenied);
        }
        Ok(())
    }
}
