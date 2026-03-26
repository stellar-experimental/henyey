# henyey-scp

Deterministic Rust implementation of the Stellar Consensus Protocol.

## Overview

`henyey-scp` implements the federated Byzantine agreement engine used to drive ledger consensus in henyey. It sits beneath the herder layer, tracks consensus independently for each slot, and mirrors the upstream `stellar-core/src/scp/` subsystem closely enough to support parity work against v25.x behavior. The crate exposes the top-level `SCP<D>` coordinator, quorum utilities, quorum-set configuration helpers, and structured diagnostics for slot and quorum inspection.

## Architecture

```mermaid
stateDiagram-v2
    [*] --> Idle
    Idle --> Nominating: `SCP::nominate`
    Idle --> Externalized: `force_externalize`
    Nominating --> Nominating: leader adoption / timer rounds
    Nominating --> Prepare: composite candidate formed
    Prepare --> Prepare: accept prepared / bump ballot
    Prepare --> Confirm: accept commit
    Confirm --> Confirm: extend commit interval
    Confirm --> Externalized: confirm commit
    Externalized --> [*]

    state Nominating {
        [*] --> NominationProtocol
    }

    state Prepare {
        [*] --> BallotProtocol
    }

    state Confirm {
        [*] --> BallotProtocol
    }
```

## Key Types

| Type | Description |
|------|-------------|
| `SCP<D>` | Top-level coordinator that owns slots, routes envelopes, and exposes the public consensus API. |
| `Slot` | Per-slot consensus container combining nomination state, ballot state, and envelope history. |
| `SCPDriver` | Application callback trait for value validation, hashing, signing, timers, quorum lookup, and notifications. |
| `SlotState` | Lightweight snapshot of a slot's consensus progress for monitoring and tests. |
| `EnvelopeState` | Result of processing an incoming envelope: invalid, valid-but-old, or valid-and-state-changing. |
| `BallotPhase` | Public enum describing ballot progression through `Prepare`, `Confirm`, and `Externalize`. |
| `ValidationLevel` | Driver-reported validation result used to gate nomination and ballot progress. |
| `SCPTimerType` | Identifies nomination versus ballot timers. |
| `ScpError` | Error enum for malformed messages, invalid quorum sets, and internal SCP failures. |
| `QuorumConfigError` | Error enum for parsing and validating Rust-side quorum-set configuration. |
| `SlotInfo` | Serializable slot summary combining nomination and ballot diagnostics. |
| `NominationInfo` | Serializable nomination-state snapshot. |
| `BallotInfo` | Serializable ballot-state snapshot, including prepared and commit ranges. |
| `QuorumInfo` / `NodeInfo` | Serializable quorum participation view for a slot and its peers. |
| `SingletonQuorumSetCache` | Cache for repeatedly constructing one-node quorum sets during quorum operations. |

## Usage

```rust
use std::sync::Arc;

use henyey_scp::{EnvelopeState, SCP, SCPDriver, SCPTimerType, ValidationLevel, Value};

struct MyDriver;

impl SCPDriver for MyDriver {
    fn validate_value(&self, _slot: u64, _value: &Value, _nomination: bool) -> ValidationLevel {
        ValidationLevel::FullyValidated
    }

    fn combine_candidates(&self, _slot: u64, candidates: &[Value]) -> Option<Value> {
        candidates.first().cloned()
    }

    fn extract_valid_value(&self, _slot: u64, value: &Value) -> Option<Value> {
        Some(value.clone())
    }

    fn emit_envelope(&self, _envelope: &henyey_scp::ScpEnvelope) {}
    fn get_quorum_set(&self, _node_id: &henyey_scp::NodeId) -> Option<henyey_scp::ScpQuorumSet> { None }
    fn nominating_value(&self, _slot: u64, _value: &Value) {}
    fn value_externalized(&self, _slot: u64, _value: &Value) {}
    fn ballot_did_prepare(&self, _slot: u64, _ballot: &henyey_scp::ScpBallot) {}
    fn ballot_did_confirm(&self, _slot: u64, _ballot: &henyey_scp::ScpBallot) {}
    fn compute_hash_node(&self, _slot: u64, _prev: &Value, _priority: bool, _round: u32, _node: &henyey_scp::NodeId) -> u64 { 0 }
    fn compute_value_hash(&self, _slot: u64, _prev: &Value, _round: u32, _value: &Value) -> u64 { 0 }
    fn compute_timeout(&self, _round: u32, _is_nomination: bool) -> std::time::Duration { std::time::Duration::from_secs(1) }
    fn sign_envelope(&self, _envelope: &mut henyey_scp::ScpEnvelope) {}
    fn verify_envelope(&self, _envelope: &henyey_scp::ScpEnvelope) -> bool { true }
    fn setup_timer(&self, _slot: u64, _timer: SCPTimerType, _timeout: std::time::Duration) {}
}

let driver = Arc::new(MyDriver);
let scp = SCP::new(local_node_id, true, local_quorum_set, driver);
```

```rust
let updated = scp.nominate(slot_index, value.clone(), &previous_value);
assert!(updated);

match scp.receive_envelope(peer_envelope) {
    EnvelopeState::ValidNew => {
        if let Some(externalized) = scp.get_externalized_value(slot_index) {
            // Apply the agreed value.
            let _ = externalized;
        }
    }
    EnvelopeState::Valid | EnvelopeState::Invalid => {}
}
```

```rust
// Catchup and replay can bypass live SCP rounds.
scp.force_externalize(ledger_seq, historical_value);

// Structured diagnostics are available for monitoring and debugging.
let slot_info = scp.get_info(ledger_seq);
let quorum_info = scp.get_quorum_info(ledger_seq);
```

## Module Layout

| Module | Description |
|--------|-------------|
| `lib.rs` | Public exports, common type aliases, and helpers shared by nomination and ballot processing. |
| `scp.rs` | `SCP<D>` coordinator, slot map management, envelope routing, purge logic, and public inspection APIs. |
| `slot.rs` | Per-slot orchestration that ties nomination, ballot, timers, validation state, and externalization together. |
| `nomination.rs` | Nomination phase state machine, leader selection, value adoption, and composite-candidate formation. |
| `ballot/mod.rs` | Core `BallotProtocol` state, public accessors, info reporting, and top-level ballot entry points. |
| `ballot/state_machine.rs` | Prepare/confirm/externalize transition logic, commit interval search, and ballot bumping. |
| `ballot/envelope.rs` | Local ballot statement construction, self-processing, and emission gating. |
| `ballot/statements.rs` | Ballot statement ordering, sanity checks, quorum-set resolution, and federated accept/ratify helpers. |
| `quorum.rs` | Quorum-slice, quorum, v-blocking, normalization, hashing, and singleton quorum-set utilities. |
| `quorum_config.rs` | Rust-side quorum configuration parsing, validation, known validator presets, and strkey conversion. |
| `driver.rs` | `SCPDriver` trait plus shared node-weight and timeout helper logic. |
| `compare.rs` | Cross-statement ordering helpers used to compare nomination and ballot progress. |
| `format.rs` | Human-readable formatting for nodes, ballots, envelopes, and values. |
| `info.rs` | Serde-friendly diagnostic structs used by `get_info()` and `get_quorum_info()`. |
| `error.rs` | Crate-level error definitions. |

## Design Notes

- Local self-envelopes are recursively re-processed before final emission so the slot can cascade through multiple ballot transitions inside one top-level message handling pass, matching stellar-core behavior.
- Envelope emission is gated on full validation, so henyey can track partially validated state without broadcasting local statements too early.
- `SlotContext` replaces the back-reference chain used in stellar-core (`SCP` -> `Slot` -> protocol objects) with explicit borrowed context, which keeps the Rust implementation borrow-checker friendly without changing protocol semantics.

## stellar-core Mapping

| Rust | stellar-core |
|------|--------------|
| `scp.rs` | `src/scp/SCP.cpp`, `src/scp/SCP.h` |
| `slot.rs` | `src/scp/Slot.cpp`, `src/scp/Slot.h` |
| `nomination.rs` | `src/scp/NominationProtocol.cpp`, `src/scp/NominationProtocol.h` |
| `ballot/mod.rs` | `src/scp/BallotProtocol.cpp`, `src/scp/BallotProtocol.h` |
| `ballot/state_machine.rs` | `src/scp/BallotProtocol.cpp` |
| `ballot/envelope.rs` | `src/scp/BallotProtocol.cpp` |
| `ballot/statements.rs` | `src/scp/BallotProtocol.cpp` |
| `quorum.rs` | `src/scp/LocalNode.cpp`, `src/scp/LocalNode.h`, `src/scp/QuorumSetUtils.cpp`, `src/scp/QuorumSetUtils.h` |
| `driver.rs` | `src/scp/SCPDriver.cpp`, `src/scp/SCPDriver.h` |
| `compare.rs` | Statement ordering logic split across `src/scp/Slot.h`, `src/scp/BallotProtocol.h`, and `src/scp/NominationProtocol.h` |
| `format.rs` | String-formatting helpers embedded across `src/scp/` |
| `info.rs` | JSON reporting assembled in `src/scp/SCP.cpp`, `src/scp/Slot.cpp`, and protocol classes |
| `quorum_config.rs` | No direct upstream equivalent; henyey-specific configuration layer |
| `error.rs` | No direct upstream equivalent; Rust-specific error surface |

## Parity Status

See [PARITY_STATUS.md](PARITY_STATUS.md) for detailed stellar-core parity analysis.
