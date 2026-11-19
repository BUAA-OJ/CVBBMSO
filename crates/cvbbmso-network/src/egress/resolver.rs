//! Host resolution for the egress guard.

use std::net::IpAddr;

use crate::NetworkError;

/// Resolves the names an egress policy depends on.
///
/// The trait is the test seam for the guard: the real implementation asks the
/// system resolver, tests inject deterministic answers.
pub(crate) trait Resolver {
    /// Resolves `host` to a deduplicated list of addresses.
    ///
    /// An empty list means the name currently has no address; the caller keeps
    /// the previous set in that case.
    ///
    /// # Errors
    ///
    /// Returns [`NetworkError::Resolution`] when resolution fails outright.
    async fn resolve(&self, host: &str, port: u16) -> Result<Vec<IpAddr>, NetworkError>;
}

/// Resolver backed by the system resolver.
pub(crate) struct SystemResolver;

impl Resolver for SystemResolver {
    async fn resolve(&self, host: &str, port: u16) -> Result<Vec<IpAddr>, NetworkError> {
        let mut addresses = tokio::net::lookup_host((host, port))
            .await
            .map_err(|_| NetworkError::Resolution)?
            .map(|address| address.ip())
            .collect::<Vec<_>>();
        addresses.sort();
        addresses.dedup();
        Ok(addresses)
    }
}
