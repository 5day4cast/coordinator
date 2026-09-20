//! Trusted Coordinator rules statically linked inside the custom enclave.
#[cfg(feature = "lnurl")]
pub mod lnurl_transport;
mod verifier;
pub use verifier::CoordinatorVerifier;
pub mod config;
