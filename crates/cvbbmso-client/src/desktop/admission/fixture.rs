use std::sync::{
    Arc,
    atomic::{AtomicBool, AtomicUsize, Ordering},
};

use cvbbmso_network::NetworkError;
use tokio::sync::Notify;

use super::Failure;

#[derive(Clone, Default)]
pub(in crate::desktop) struct Admission(Arc<Counters>);

#[derive(Default)]
struct Counters {
    proxy_once: AtomicBool,
    monitor_once: AtomicBool,
    #[cfg(target_os = "macos")]
    screen_once: AtomicBool,
    #[cfg(target_os = "macos")]
    approval_required: AtomicBool,
    #[cfg(target_os = "macos")]
    pf_blocked: AtomicBool,
    #[cfg(target_os = "macos")]
    settings_opens: AtomicUsize,
    #[cfg(target_os = "macos")]
    settings_opened: Notify,
    pause_install: AtomicBool,
    fail_verify: AtomicBool,
    fail_shutdown: AtomicBool,
    installs: AtomicUsize,
    shutdowns: AtomicUsize,
    verifies: AtomicUsize,
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
        self.0.monitor_once.store(true, Ordering::Relaxed);
        #[cfg(target_os = "macos")]
        {
            self.0.screen_once.store(true, Ordering::Relaxed);
            self.0.approval_required.store(true, Ordering::Relaxed);
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
        if self.0.monitor_once.swap(false, Ordering::Relaxed) {
            return Err(Failure::MonitorUnreachable);
        }
        Ok(())
    }

    #[cfg(target_os = "macos")]
    pub(in crate::desktop) fn grant_approval(&self) {
        self.0.approval_required.store(false, Ordering::Relaxed);
    }

    #[cfg(target_os = "macos")]
    pub(in crate::desktop) fn enable_pf(&self) {
        self.0.pf_blocked.store(false, Ordering::Relaxed);
    }

    #[cfg(target_os = "macos")]
    pub(in crate::desktop) fn settings_opens(&self) -> usize {
        self.0.settings_opens.load(Ordering::Relaxed)
    }

    #[cfg(target_os = "macos")]
    pub(in crate::desktop) async fn settings_opened(&self) {
        self.0.settings_opened.notified().await;
    }

    #[cfg(target_os = "macos")]
    pub(super) fn open_approval_settings(&self) {
        self.0.settings_opens.fetch_add(1, Ordering::Relaxed);
        self.0.settings_opened.notify_one();
    }

    #[cfg(target_os = "macos")]
    pub(super) fn check_helper(&self) -> Result<(), Failure> {
        if self.0.approval_required.load(Ordering::Relaxed) {
            return Err(Failure::MacosApprovalRequired);
        }
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

pub(super) struct Guard(Arc<Counters>);

impl Guard {
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
