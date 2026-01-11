//! coordinator-core: Shared types for coordinator server and WASM client
//!
//! This crate contains types that are shared between the server and browser client.

pub mod errors;
pub mod types;
pub mod validation;

pub use errors::*;
pub use types::*;
pub use validation::*;
