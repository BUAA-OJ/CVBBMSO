//! Platform egress filter adapters.

use std::sync::Arc;

use crate::NetworkError;
use crate::egress::policy::Plan;

#[cfg(target_os = "macos")]
pub(crate) mod macos;
#[cfg(not(any(target_os = "macos", windows)))]
pub(crate) mod other;
#[cfg(windows)]
pub(crate) mod windows;

#[cfg(target_os = "macos")]
pub(crate) use self::macos as platform;
#[cfg(not(any(target_os = "macos", windows)))]
pub(crate) use self::other as platform;
#[cfg(windows)]
pub(crate) use self::windows as platform;

/// An installed platform filter.
pub(crate) trait GuardBackend: Send + Sync {
    /// Replaces the active rule set with `plan`.
    fn apply(&self, plan: &Plan) -> Result<(), NetworkError>;

    /// Confirms that the rules are still installed.
    fn verify(&self) -> Result<(), NetworkError>;

    /// Removes the rules. Calling it twice is not an error.
    fn revert(&self) -> Result<(), NetworkError>;
}

/// Installs the platform filter and returns a handle to it.
pub(crate) type InstallResult = Arc<dyn GuardBackend>;
