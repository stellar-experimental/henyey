# henyey-scp Crate Overview

## Purpose
Stellar Consensus Protocol (SCP) implementation for the henyey Stellar client.

## Tech Stack
- Rust workspace crate (`henyey-scp`)
- Dependencies: `stellar-xdr`, `stellar-strkey`, `henyey-common`, `henyey-crypto`, `serde`, `thiserror`, `tracing`, `parking_lot`, `hex`

## Structure
- `src/lib.rs` - Public API, re-exports of XDR types
- `src/scp.rs` - Main SCP struct
- `src/slot.rs` - Slot management
- `src/nomination.rs` - Nomination protocol
- `src/ballot/` - Ballot protocol (mod.rs, envelope.rs, state_machine.rs, statements.rs)
- `src/quorum.rs` - Quorum set operations
- `src/quorum_config.rs` - Quorum configuration parsing
- `src/driver.rs` - SCPDriver trait
- `src/compare.rs` - Statement comparison
- `src/format.rs` - Display formatting
- `src/info.rs` - Info/debug structs (JSON-serializable)
- `src/error.rs` - Error types

## Commands
- Build: `cargo build -p henyey-scp`
- Test: `cargo test -p henyey-scp --lib`
- Clippy: `cargo clippy -p henyey-scp -- -D warnings`

## XDR Type Usage
The crate re-exports XDR types from `stellar_xdr::curr`: NodeId, ScpBallot, ScpEnvelope, ScpNomination, ScpQuorumSet, etc.
Uses `henyey_common::Hash256` for crypto hashing (legitimate crypto wrapper).
