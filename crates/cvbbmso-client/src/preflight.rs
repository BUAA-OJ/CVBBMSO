//! Local virtualization preflight and command admission.

use std::future::Future;

use serde::Serialize;
use tokio::sync::watch;

#[path = "preflight/command.rs"]
pub(crate) mod command;
#[path = "preflight/evidence.rs"]
pub(crate) mod evidence;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum Diagnostic {
    GuestIdentity,
    HardwareIdentity,
    #[cfg(not(target_os = "macos"))]
    HypervisorPresent,
    ProbeUnavailable,
    ProbeFailed,
    TimedOut,
    InvalidEvidence,
    ConflictingEvidence,
    Interrupted,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(tag = "status", content = "reason", rename_all = "snake_case")]
pub(crate) enum Verdict {
    Detected(Diagnostic),
    NotDetected(Diagnostic),
    Unknown(Diagnostic),
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Check {
    Pending,
    Running,
    Finished(Verdict),
}

pub(crate) struct Preflight {
    state: watch::Sender<Check>,
}

impl Preflight {
    pub(crate) fn new() -> Self {
        Self {
            state: watch::Sender::new(Check::Pending),
        }
    }

    // Claim the pending run synchronously. A detached task owns completion
    // so that cancelling or reloading local IPC cannot strand the check.
    pub(crate) fn schedule<F>(
        &self,
        retry: bool,
        probe: F,
    ) -> Option<impl Future<Output = ()> + use<F>>
    where
        F: Future<Output = Verdict>,
    {
        let started = self.state.send_if_modified(|state| {
            if matches!(state, Check::Pending)
                || (retry && matches!(state, Check::Finished(Verdict::Unknown(_))))
            {
                *state = Check::Running;
                true
            } else {
                false
            }
        });
        if !started {
            return None;
        }

        let state = self.state.clone();
        Some(async move {
            state.send_replace(Check::Finished(probe.await));
        })
    }

    pub(crate) async fn verdict(&self) -> Verdict {
        let mut receiver = self.state.subscribe();
        match receiver
            .wait_for(|state| matches!(state, Check::Finished(_)))
            .await
            .as_deref()
        {
            Ok(Check::Finished(verdict)) => *verdict,
            _ => Verdict::Unknown(Diagnostic::Interrupted),
        }
    }

    pub(crate) fn is_admitted(&self) -> bool {
        matches!(
            *self.state.borrow(),
            Check::Finished(Verdict::NotDetected(_))
        )
    }
}
