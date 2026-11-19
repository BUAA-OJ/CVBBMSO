use http::header::InvalidHeaderValue;
use thiserror::Error;

/// Failures while preparing session cookies or verifying an Accoding account.
#[derive(Debug, Error)]
#[non_exhaustive]
pub enum AccodingError {
    /// The cookie collection has no nonempty `connect.sid`.
    #[error("missing Accoding session cookie")]
    MissingSession,
    /// A cookie name or value is invalid, or `connect.sid` occurs more than once.
    #[error("invalid Accoding cookie header")]
    InvalidHeader(#[source] Option<InvalidHeaderValue>),
    /// The identity endpoint rejected the session or returned an anonymous response.
    #[error("Accoding session is unauthenticated")]
    Unauthenticated,
    /// The HTTP client could not be initialized or complete a successful request.
    #[error("Accoding identity request failed")]
    Upstream(#[source] reqwest::Error),
    /// The identity endpoint returned an unexpected response.
    #[error("invalid response from the Accoding identity endpoint")]
    InvalidResponse(#[source] Option<serde_json::Error>),
}
