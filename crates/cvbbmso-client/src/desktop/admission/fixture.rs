use std::sync::{
    Arc,
    atomic::{AtomicBool, AtomicUsize, Ordering},
};

use tokio::sync::Notify;

use super::Failure;

#[derive(Clone, Default)]
pub(in crate::desktop) struct Admission(Arc<Counters>);

#[derive(Default)]
struct Counters {
    proxy_once: AtomicBool,
    #[cfg(target_os = "macos")]
    screen_once: AtomicBool,
    pause_install: AtomicBool,
    fail_verify: AtomicBool,
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
        #[cfg(target_os = "macos")]
        self.0.screen_once.store(true, Ordering::Relaxed);
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

    pub(super) async fn install(&self) -> Result<Guard, Failure> {
        self.0.installs.fetch_add(1, Ordering::Relaxed);
        self.0.installing.notify_one();
        if self.0.pause_install.swap(false, Ordering::Relaxed) {
            self.0.resume.notified().await;
        }
        Ok(Guard(Arc::clone(&self.0)))
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

    pub(super) fn shutdown(&self) -> bool {
        self.0.shutdowns.fetch_add(1, Ordering::Relaxed);
        true
    }
}
