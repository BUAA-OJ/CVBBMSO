//! Network environment inspection for the CVBBMSO client.
//!
//! The crate serves two purposes during the client startup flow, both defined by
//! `docs/design/crates/cvbbmso-network.md`:
//!
//! * A startup preflight that answers two independent questions and reports both
//!   answers as a [`NetworkReport`]: is a system proxy active, and does the
//!   monitoring server answer on its health endpoint? `cvbbmso-client` only lets
//!   a candidate enter an exam while the proxy state is [`ProxyState::Clear`] and
//!   the endpoint is [`EndpointState::Reachable`].
//! * A system egress guard ([`EgressGuard`]) that restricts outbound traffic to
//!   the Accoding origin, the monitoring server and the services an exam client
//!   needs, for as long as the application runs.
//!
//! Inspection is read-only; the preflight never changes system settings. Platform
//! APIs stay inside private adapter modules: macOS reads the System Configuration
//! proxy dictionary and drives PF, Windows reads the current user's WinHTTP/IE
//! proxy configuration and drives WFP, and targets without an adapter report
//! [`ProxyState::Unknown`] and refuse to install the guard. Proxy environment
//! variables are inspected on every platform.
//!
//! [`Inspector::check`] fails only when the check itself cannot be executed, not
//! when the environment looks wrong: a configuration problem is reported as a
//! [state](ProxyState), never as an error.

// `missing_docs` is enforced by the workspace lints. `unsafe_code` is denied
// crate-wide and re-allowed only in the platform adapters, which are the single
// place that calls platform APIs.
#![deny(unsafe_code)]

mod egress;
mod endpoint;
mod inspector;
mod proxy;

#[cfg(target_os = "macos")]
pub use self::egress::managed::MacosEgressGuard;
#[cfg(all(target_os = "macos", feature = "macos-pf-trial"))]
pub use self::egress::trial::MacosTrialGuard;
pub use self::egress::{EgressGuard, EgressPolicy};

/// Entry point of the administrator-launched macOS trial helper.
#[cfg(all(target_os = "macos", feature = "macos-pf-trial"))]
#[doc(hidden)]
#[must_use]
pub fn macos_pf_trial_main() -> std::process::ExitCode {
    egress::trial::main()
}

/// Entry point for the signed macOS `LaunchDaemon` and its recovery subprocess.
#[cfg(target_os = "macos")]
#[doc(hidden)]
#[must_use]
pub fn macos_egress_service_main() -> std::process::ExitCode {
    egress::managed::service_main()
}

/// Entry point of the `cvbbmso-egress-watchdog` helper binary.
///
/// The helper is a separate compilation unit, so it cannot call crate-private
/// items; this entry point exists only for it and is hidden from the
/// documentation.
#[doc(hidden)]
#[must_use = "return the watchdog status to the operating system"]
pub fn watchdog_main() -> std::process::ExitCode {
    egress::watchdog_entry()
}
pub use self::endpoint::EndpointState;
pub use self::inspector::{Inspector, NetworkReport};
pub use self::proxy::ProxyState;

/// Failure to execute a network environment operation.
///
/// An error means the requested work could not be completed; it is not a
/// statement about the network. The client keeps the candidate outside the exam
/// and shows the failure. A proxy state that cannot be determined is reported as
/// [`ProxyState::Unknown`] instead.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
#[non_exhaustive]
pub enum NetworkError {
    /// A platform API returned an error or is unavailable on this target.
    #[error("the platform network API failed")]
    Platform,
    /// A local file, process or socket operation failed.
    #[error("the network environment operation could not be executed")]
    Io,
    /// Administrator or root rights are required and were not granted.
    #[error("administrator or root rights are required")]
    PermissionDenied,
    /// The egress policy is not usable.
    #[error("the egress policy is invalid")]
    Policy,
    /// A name required by the egress policy could not be resolved.
    #[error("the egress policy address could not be resolved")]
    Resolution,
    /// The signed app service requires macOS 14 or later.
    #[cfg(target_os = "macos")]
    #[error("the macOS egress service requires macOS 14 or later")]
    MacosUnsupported,
    /// The build, signature or app location is ineligible for privileged use.
    #[cfg(target_os = "macos")]
    #[error("the signed macOS egress service or app installation could not be trusted")]
    MacosUntrustedInstallation,
    /// An administrator must approve the registered system service.
    #[cfg(target_os = "macos")]
    #[error("approve the egress service in macOS Login Items & Extensions")]
    MacosApprovalRequired,
    /// The registered service could not be contacted or validated.
    #[cfg(target_os = "macos")]
    #[error("the macOS egress service is unavailable")]
    MacosServiceUnavailable,
    /// The live or persistent host PF policy requires administrator review.
    #[cfg(target_os = "macos")]
    #[error("the host PF configuration needs administrator review")]
    MacosUnsupportedPfConfiguration,
    /// Another client or an unfinished cleanup owns the system filter.
    #[cfg(target_os = "macos")]
    #[error("another session owns the macOS egress guard")]
    MacosBusy,
}
