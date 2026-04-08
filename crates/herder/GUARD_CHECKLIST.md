# Herder Guard Checklist

**Upstream reference**: `stellar-core/src/herder/HerderImpl.cpp`, `PendingEnvelopes.cpp`, `NominationProtocol.cpp`
**Last updated**: 2025-07-29

This checklist enumerates every guard clause that stellar-core applies at SCP
envelope processing entry points, cross-referenced with Henyey's implementation status.

## Summary

| Status | Count |
|--------|-------|
| PRESENT | 14 |
| N/A | 1 |
| **Total** | **15** |

## receive_scp_envelope (herder.rs → HerderImpl::recvSCPEnvelope)

| Guard | stellar-core Location | Henyey Location | Status | Issue |
|-------|----------------------|-----------------|--------|-------|
| Manual close mode bypass | `HerderImpl.cpp:805-808` | — | N/A | |
| State check (`can_receive_scp`) | `HerderImpl.cpp:810` | `herder.rs:858` | PRESENT | |
| Close-time pre-filter (before sig verify) | `HerderImpl.cpp:818-829` | `herder.rs:871` (`check_envelope_close_time`) | PRESENT | |
| Ledger sequence range filtering | `HerderImpl.cpp:831-874` | `herder.rs:929` (min/max ledger seq) | PRESENT | |
| Signature verification | `HerderImpl.cpp:877-883` | `herder.rs:943` (`verify_envelope`) | PRESENT | |
| Self-message filtering (skip own envelopes) | `HerderImpl.cpp:885-891` | `herder.rs` (node_id comparison after sig verify) | PRESENT | |

## Fetching / Pending Envelopes (fetching_envelopes.rs → PendingEnvelopes.cpp)

| Guard | stellar-core Location | Henyey Location | Status | Issue |
|-------|----------------------|-----------------|--------|-------|
| Quorum membership check (`isNodeDefinitelyInQuorum`) | `PendingEnvelopes.cpp:293-298` | `herder.rs` (`quorum_tracker.is_node_definitely_in_quorum`) | PRESENT | |
| STELLAR_VALUE_SIGNED check (reject Basic) | `PendingEnvelopes.cpp:300-316` | `fetching_envelopes.rs:746` | PRESENT | |
| Discard cache (skip already-processed envelopes) | `PendingEnvelopes.cpp:325-328` | `fetching_envelopes.rs:194` (`processed` / `discarded` sets) | PRESENT | |
| Fetch completion gating (all deps available) | `PendingEnvelopes.cpp:568-576` | `fetching_envelopes.rs:670` (`check_dependencies`) | PRESENT | |
| NOMINATE tx_set fetch dependency | `PendingEnvelopes.cpp:568-576` (`getValidatedTxSetHashes` handles all types) | `fetching_envelopes.rs` (`extract_tx_set_hashes` via `Slot::get_statement_values`) | PRESENT | |
| XDR corruption handling | `PendingEnvelopes.cpp:388-394` | `fetching_envelopes.rs` (XDR deserialization error paths) | PRESENT | |

## SCP Driver Callbacks (scp_driver.rs → NominationProtocol.cpp + HerderSCPDriver)

| Guard | stellar-core Location | Henyey Location | Status | Issue |
|-------|----------------------|-----------------|--------|-------|
| `extract_valid_value` called for Invalid validation level | `NominationProtocol.cpp:81-85` (else branch calls `extractValidValue` for all non-FullyValidated) | `nomination.rs:528,801` (MaybeValid \| Invalid arms) | PRESENT | |
| `validate_value` signature verification | `HerderSCPDriver.cpp` | `scp_driver.rs:727` (`validate_value_impl`) | PRESENT | |
| `validate_value` upgrade ordering check | `HerderSCPDriver.cpp` | `scp_driver.rs` (`check_upgrade_ordering`) | PRESENT | |
| `verify_stellar_value_signature` | `HerderSCPDriver.cpp` | `scp_driver.rs:940` | PRESENT | |
