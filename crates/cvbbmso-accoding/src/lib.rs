//! Session cookie handling and identity verification for the Accoding student site.

#![forbid(unsafe_code)]

mod cookies;
mod error;
mod verifier;

pub use cookies::AccodingCookies;
pub use error::AccodingError;
pub use verifier::Verifier;
