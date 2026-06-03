# LEDGER_SPEC Adherence — henyey-ledger

Spec: `stellar-specs/LEDGER_SPEC.md` (v26, Protocol 26).
Crate: `crates/ledger` (with integration in `crates/app`, `crates/invariant`, `crates/common`).

**Last updated:** 2026-06-03
**Overall adherence:** ~98% — Full 14 invariants + core pipeline | Partial 0 | Absent 1 | Drift 0 | N/A several

> Adherence counts the protocol-deterministic invariants (§15) and the
> normative pipeline checks. henyey's flat `CloseLedgerState` + `LedgerDelta`
> design deliberately replaces stellar-core's nested `LedgerTxn` chain, so most
> of §7 (LedgerTxn nesting, child/parent commit, merge matrix) and the
> threading-model claims of §2/§5 are **N/A** — see `PARITY_STATUS.md`.

This audit was regenerated against `origin/main` (`ee9fefea`), spec submodule
`6814d6e4`. It supersedes the stale 2026-05-21 report whose Absent/Drift list
had largely been implemented since.

## Invariant coverage (§15)

| Invariant | Status | Enforcement |
|-----------|--------|-------------|
| INV-L1 (Single-child LedgerTxn) | N/A | henyey uses flat `CloseLedgerState`, no nested child/parent LedgerTxn chain. |
| INV-L2 (Same-thread access) | N/A | No mutable cross-thread LedgerTxn handles in the flat design. |
| INV-L3 (Monotonic seq + hash chain) | Full | `header.rs:285` `create_next_header` increments seq and links `previous_ledger_hash`; txset rooting checked in `app/ledger_close.rs`. |
| **INV-L4 (Total coins conservation)** | **Absent** | `ConservationOfLumens` has **no concrete `Invariant` impl** — only a stub (`invariant/src/lib.rs:55,58` "None until ConservationOfLumens invariant is implemented"). Config recognizes the name (`compat_config.rs`) but `InvariantManager` cannot register/enable it; the three registered invariants are `AccountSubEntriesCountIsValid`, `SponsorshipCountIsValid`, `LedgerEntryIsValid`. `total_coins_delta` is tracked (`close_state.rs:226`) but never asserted against conservation. |
| INV-L5 (Restored entries mutual exclusion) | Full | `execution/apply.rs:126-206` panics on overlap ("data_key already restored from live BL" / "key already restored from hot archive"). |
| INV-L6 (Sealed-after-commit) | N/A | Flat design: no re-mutation-after-seal LedgerTxn semantics; delta is finalized once. |
| INV-L7 (Fee pool non-negative) | Full | `common/src/header_validation.rs:22` rejects `fee_pool < 0`; invoked from `header.rs:318` on every header construction. |
| INV-L8 (Phase-state safety) | Full | Apply-state phase machine in `execution/mod.rs` / `apply.rs`; in-memory Soroban state mutated only in setup/commit. |
| INV-L9 (LedgerHeader validity) | Full | `common/src/header_validation.rs:21` checks all four bounds: `fee_pool >= 0`, `ledger_seq <= i32::MAX`, `id_pool <= i64::MAX`, `close_time <= i64::MAX`. |
| INV-L10 (TxSet rooting) | Full | previousLedgerHash + contentsHash checks in `app/ledger_close.rs` before apply. |
| INV-L11 (Expected-hash check) | Full | `manager.rs:5563,8673` — LCL-corruption guard on hash mismatch when `expectedHash` set. |
| INV-L12 (Single SCP value per LCL) | N/A | Enforced by LedgerApplyManager queue ordering (CATCHUP_SPEC / app layer), not the ledger crate. |
| INV-L13 (HAS / LCL agreement on reload) | Full | `app/ledger_close.rs:737` — `load_last_known_ledger` bails "LCL seq X does not agree with HAS current_ledger Y". |
| INV-L14 (Configuration immutability) | Full | `CONFIG_SETTING` erase rejected; config settings only created at V20 upgrade or updated via `LEDGER_UPGRADE_CONFIG`. |
| INV-L15 (Header re-seal must not modify entries) | N/A | Flat design exposes header mutation via `create_next_header` without an entry map to corrupt. |

## Pipeline checks (selected)

| Section | Topic | Status | Implementation |
|---------|-------|--------|----------------|
| §4.2 | LedgerCloseData validation (version/txset/hash throws) | Full | `app/ledger_close.rs` (validation precedes close in henyey's design). |
| §6.2 | `MAX_SEQ_NUM_TO_APPLY` plumbing (merge_seen, accToMaxSeq) | Full | `execution/tx_set.rs:347` `compute_max_seq_num_to_apply` + `set_max_seq_num_to_apply`. |
| §9.2 | Header validity bounds | Full | `common/src/header_validation.rs` (see INV-L7/L9). |
| §9.3 | Skip-list construction | Full | `header.rs:102` `calculate_skip_values`; SKIP_1..4 = 50/5000/50000/500000. |
| §13.1 | Meta version selection (v0/v1/v2) | N/A | henyey is **protocol-24+ only** (CLAUDE.md), which is always V2; `manager.rs:6019` documents the unconditional-V2 choice. v0/v1 branches are unreachable. |
| §14.1 | Genesis constants | Full | genesis ledger constants; anchor at `manager.rs:6047`. |

## Dangling Spec anchors

- `crates/ledger/src/manager.rs:6047` — `// Spec: LEDGER_SPEC §13.1 — genesis ledger constants.`
  Genesis constants are now **§14.1** in the v26 spec; §13.1 is "Ledger Close Meta — Selection". Anchor should be renumbered to §14.1.
- `crates/ledger/src/manager.rs:5981,6019` — comment cites `§12.2` / `§15.11` for meta version selection; current section is **§13.1**. Comment text only (not a `// Spec:` anchor), low priority.

(The stale 2026-05-21 report claimed "13 dangling anchors"; current main has only **one** real `// Spec:` anchor in the crate, and it is the genesis one above.)

## Drift items

None. The previously-flagged §13.1 always-V2 meta is **correct by design** (protocol-24+-only), not drift.

## Genuine gaps remaining

1. **INV-L4 — `ConservationOfLumens` runtime invariant is not implemented.** It is a
   stub; configuring `INVARIANT_CHECKS=["ConservationOfLumens"]` cannot be enabled
   because no concrete impl is registered. This is the only true Absent item.

Everything else the prior report listed as Absent/Partial/Drift (§6.2 MAX_SEQ_NUM,
INV-L7/L9 header bounds, INV-L13 HAS/LCL reload, §13.1 meta) is now Present or
N/A-by-design.
