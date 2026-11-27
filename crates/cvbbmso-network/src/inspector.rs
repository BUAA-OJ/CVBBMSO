//! The public entry point of the crate.

use url::Url;

use crate::NetworkError;
use crate::endpoint::{self, EndpointState};
use crate::proxy::{self, ProxyState};

/// Snapshot of the network environment relevant to an exam.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct NetworkReport {
    /// State of the system proxy configuration.
    pub proxy: ProxyState,
    /// Reachability of the monitoring server.
    pub endpoint: EndpointState,
}

/// Inspector for the network environment of the local machine.
///
/// One inspector is bound to the origin of one monitoring server and reads the
/// environment of the current user.
#[derive(Debug, Clone)]
pub struct Inspector {
    origin: Url,
}

impl Inspector {
    /// Creates an inspector that probes `server_origin`.
    ///
    /// `server_origin` is the HTTP(S) origin of the monitoring server; a path
    /// other than `/` is treated as a reverse-proxy prefix.
    #[must_use]
    pub fn new(server_origin: Url) -> Self {
        Self {
            origin: server_origin,
        }
    }

    /// Inspects the system proxy configuration and probes the server.
    ///
    /// The two inspections are independent: a configured proxy does not prevent
    /// the endpoint from being reported as reachable.
    ///
    /// # Errors
    ///
    /// Returns [`NetworkError::Platform`] when the platform proxy API fails, and
    /// [`NetworkError::Io`] when a local resource needed by the probe is
    /// unavailable. A server that does not answer is reported as
    /// [`EndpointState::Unreachable`], not as an error.
    pub async fn check(&self) -> Result<NetworkReport, NetworkError> {
        let proxy = Self::check_proxy().await?;
        let endpoint = endpoint::probe(&self.origin).await?;

        Ok(NetworkReport { proxy, endpoint })
    }

    /// Reads the current proxy configuration without making a network request.
    ///
    /// # Errors
    ///
    /// Returns [`NetworkError::Platform`] if the platform inspection fails.
    pub async fn check_proxy() -> Result<ProxyState, NetworkError> {
        // Reading the system proxy configuration calls blocking platform APIs
        // (and, on macOS, a per-user System Configuration session), so it runs
        // off the async runtime. A join error means the adapter produced no
        // result at all, which is a platform failure.
        tokio::task::spawn_blocking(proxy::inspect)
            .await
            .map_err(|_| NetworkError::Platform)?
    }

    /// Checks whether a tunnel carries a default or split-default route.
    ///
    /// # Errors
    ///
    /// Returns an error when the routing table cannot be read.
    #[cfg(any(target_os = "macos", windows))]
    pub async fn tunnel_configured() -> Result<bool, NetworkError> {
        tokio::task::spawn_blocking(crate::tunnel::inspect)
            .await
            .map_err(|_| NetworkError::Platform)?
    }
}
