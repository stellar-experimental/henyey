//! Shared test-support utilities, gated behind the `test-support` feature.
//!
//! This module is dev-only test infrastructure. It is compiled only when the
//! `test-support` feature is enabled, which consumer crates opt into via
//! `[dev-dependencies] henyey-common = { workspace = true, features =
//! ["test-support"] }`. It MUST NOT be relied upon by non-test code.

pub mod tracing_capture;
