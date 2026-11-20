//! Adapter for targets without an egress filter implementation.
//!
//! The client ships for Windows and macOS only. On other hosts the crate stays
//! buildable and the guard refuses to install, which keeps the candidate outside
//! the exam instead of running without isolation.

use crate::NetworkError;
use crate::egress::backend::InstallResult;

/// Refuses to install the guard because no platform filter exists.
///
/// # Errors
///
/// Always returns [`NetworkError::Platform`].
pub(crate) fn install(_plan: &crate::egress::policy::Plan) -> Result<InstallResult, NetworkError> {
    Err(NetworkError::Platform)
}

pub(crate) fn prepare() -> Result<(), NetworkError> {
    Err(NetworkError::Platform)
}
