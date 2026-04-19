//! Common types and utilities for henyey.
//!
//! This crate provides shared types, traits, and utilities used across
//! all henyey modules. It is designed to be dependency-light and
//! contains pure data types and helpers with no I/O or side effects, making
//! it suitable as a foundation for all other crates in the workspace.
//!
//! # Overview
//!
//! The crate is organized into the following modules:
//!
//! - [`asset`] - Asset validation, conversion, and balance utilities
//! - [`config`] - Configuration types for node setup (network, database, history archives)
//! - [`error`] - Common error types and the [`Result`] type alias
//! - [`fs_utils`] - Crash-safe filesystem operations (durable rename with fsync)
//! - [`math`] - 128-bit arithmetic and overflow-safe math operations
//! - [`meta`] - Ledger metadata normalization for deterministic hashing
//! - [`network`] - Network identity derived from network passphrases
//! - [`protocol`] - Protocol version constants and feature gating utilities
//! - [`resource`] - Resource accounting for transaction limits and surge pricing
//! - [`time`] - Time utilities for Unix/Stellar timestamp conversions
//! - [`tracking`] - Per-call and per-phase timing telemetry primitives
//! - [`types`] - Core types like [`Hash256`] used throughout the codebase
//! - [`version`] - Version string construction and protocol version invariant
//! - [`xdr_stream`] - Size-prefixed XDR frame I/O (RFC 4506 record marking)
//!
//! # Key Types
//!
//! - [`Hash256`] - A 32-byte SHA-256 hash with convenient constructors and conversions
//! - [`NetworkId`] - Network identifier derived from the network passphrase
//! - [`Config`] - Main configuration struct for node setup
//! - [`Error`] and [`Result`] - Common error handling types
//!
//! # Example
//!
//! ```rust
//! use henyey_common::{Hash256, NetworkId, Config};
//!
//! // Create a hash from data
//! let hash = Hash256::hash(b"hello world");
//! println!("Hash: {}", hash.to_hex());
//!
//! // Get the testnet network ID
//! let network_id = NetworkId::testnet();
//!
//! // Create a default testnet configuration
//! let config = Config::testnet();
//! ```

pub mod asset;
pub mod checked_types;
pub mod config;
pub mod error;
pub mod fs_utils;
pub mod math;
pub mod memory;
pub mod meta;
pub mod network;
pub mod protocol;
pub mod resource;
pub mod time;
pub mod tracking;
pub mod types;
pub mod version;
pub mod xdr_stream;

// Re-export key types at crate root for convenience
pub use asset::LIQUIDITY_POOL_FEE_V18;
pub use config::{BucketListDbConfig, Config};
pub use error::{Error, Result};
pub use meta::*;
pub use network::NetworkId;
pub use protocol::*;
pub use resource::*;
pub use types::{deterministic_seed, *};
pub use xdr_stream::{xdr_encoded_len, xdr_to_bytes};

/// Re-export stellar-xdr for convenience.
///
/// This allows other crates to access XDR types through `henyey_common::stellar_xdr`
/// without needing to add a direct dependency on the stellar-xdr crate.
pub use stellar_xdr;
