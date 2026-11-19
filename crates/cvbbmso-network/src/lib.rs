//! Network environment inspection for the CVBBMSO client.
//!
//! The crate answers two independent questions during the client startup
//! preflight and reports both answers as a [`NetworkReport`]:
//!
//! * Is a system proxy active? The monitoring server must be reached directly,
//!   so `cvbbmso-client` only lets a candidate enter an exam while the proxy
//!   state is [`ProxyState::Clear`].
//! * Does the monitoring server answer on its health endpoint?
//!
//! The client runs the check at application startup and again whenever the user
//! retries to enter an exam. Inspection is read-only; this crate never changes
//! system proxy settings. Platform APIs stay inside the private `proxy` module:
//! macOS reads the System Configuration proxy dictionary, Windows reads the
//! current user's WinHTTP/Internet Explorer proxy configuration, and other
//! targets report [`ProxyState::Unknown`] because no adapter exists. Proxy
//! environment variables are inspected on every platform.
//!
//! [`Inspector::check`] fails only when the check itself cannot be executed, not
//! when the environment looks wrong: a configuration problem is reported as a
//! [state](ProxyState), never as an error.

// `missing_docs` is enforced by the workspace lints. `unsafe_code` is denied
// crate-wide and re-allowed only in the Windows adapter, which is the single
// place that calls platform FFI.
#![deny(unsafe_code)]

mod endpoint;
mod inspector;
mod proxy;

pub use self::endpoint::EndpointState;
pub use self::inspector::{Inspector, NetworkReport};
pub use self::proxy::ProxyState;

/// Failure to execute a network environment check.
///
/// An error means the check could not be completed; it is not a statement about
/// the network. The client keeps the candidate outside the exam and shows the
/// failure, and a proxy state that cannot be determined is reported as
/// [`ProxyState::Unknown`] instead.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
#[non_exhaustive]
pub enum NetworkError {
    /// A platform API needed to read the system proxy configuration failed.
    #[error("the system proxy configuration could not be read")]
    Platform,
    /// A local operation needed by the check failed.
    #[error("the network environment check could not be executed")]
    Io,
}
