//! Target adapter for platforms without a proxy inspection backend.
//!
//! The client ships for Windows and macOS only. This module keeps the crate
//! buildable and testable on other hosts, for example the Linux CI job, without
//! pretending to know the proxy state.

use super::{ProxyFacts, ProxyState};
use crate::NetworkError;

/// Reports the proxy state as unknown because no adapter exists.
///
/// The signature is shared with the platform adapters so that dispatching needs
/// no `cfg`; on this target the result is always `Ok`, which the expectation
/// documents to the lint instead of hiding it behind an `allow`.
#[expect(
    clippy::unnecessary_wraps,
    reason = "the signature is shared with the platform adapters"
)]
pub(super) fn inspect() -> Result<ProxyState, NetworkError> {
    let facts = ProxyFacts {
        unreadable: true,
        ..ProxyFacts::default()
    };
    Ok(facts.state())
}
