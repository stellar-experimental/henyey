# Quickstart Local Network Mode Plan

## Goal

Get the `stellar/quickstart` Docker image working in `--local` mode with henyey replacing stellar-core. This creates a standalone single-node network from genesis — no external peers, no remote history archives.

## Gap Analysis

Six gaps between henyey's current capabilities and what quickstart `--local` requires:

### Gap 1: `new-db` does not create genesis ledger state

**Current:** Creates empty SQLite with schema tables only.

**Required:** stellar-core's `new-db` initializes ledger 1 with:
- Genesis header (protocol 0, seq 1, 100B XLM total supply)
- Root account (public key = SHA256(network_passphrase), balance = 100B XLM)
- Bucket list containing the root account entry
- DB state: `lastclosedledger=1`, `networkpassphrase`, `historyarchivestate`

**Fix:** Extend `cmd_new_db()` to create and persist genesis state using existing pieces (`create_genesis_header()`, `BucketList`, `Database` store methods).

### Gap 2: `force-scp` is a no-op

**Current:** Logs a message and returns. The `run` startup flow requires catchup, which fails on a genesis node with no history archive.

**Required:** After `new-db` + `force-scp`, a solo validator must start producing ledgers immediately.

**Fix:** `force-scp` sets a DB flag. `run_main_loop()` checks this flag — if set, skips catchup, loads LCL, bootstraps herder, enters event loop. The 1-second consensus tick triggers nomination; with a self-only quorum, SCP externalizes immediately.

### Gap 3: `new-hist` is a no-op

**Current:** Logs a message and returns.

**Required:** Initializes local filesystem history archive (`.well-known/stellar-history.json`).

**Fix:** Wire CLI to existing `initialize_history_archive()` method in `crates/history/src/lib.rs`.

### Gap 4: `/upgrades?mode=set` HTTP endpoint not implemented

**Current:** Compat `/upgrades` is GET-only, returns current state.

**Required:** Quickstart calls `curl "http://localhost:11626/upgrades?mode=set&upgradetime=...&protocolversion=N&basereserve=5000000"`.

**Fix:** Add parameter parsing to compat handler. Wire the existing `Upgrades` scheduling struct (`crates/herder/src/upgrades.rs`) into herder nomination flow. The struct has `set_parameters()`, `create_upgrades_for()`, `remove_upgrades()` — all implemented but unwired.

### Gap 5: `ARTIFICIALLY_ACCELERATE_TIME_FOR_TESTING` not supported

**Current:** Silently ignored. Ledger close time hardcoded to 5s.

**Required:** Local mode closes ledgers every ~1 second.

**Fix:** Parse config option, map to `HerderConfig.ledger_close_time = 1`. Note: the consensus timer already ticks every 1s, so a solo validator already closes at ~1s regardless. This mainly affects the close_time value in headers.

### Gap 6: Automatic history publishing during `run`

**Current:** Checkpoints are enqueued in SQLite but never published during `run`.

**Required:** The standalone node must publish history so captive core instances (Horizon, RPC) can catch up from the local archive.

**Fix:** Add publish drain to the main event loop in `lifecycle.rs` after each checkpoint-boundary ledger close.

## Soroban Config Upgrade

The quickstart `--limits default` flag skips the Soroban config upgrade entirely, avoiding the need for `get-settings-upgrade-txs`. CONFIG_SETTING entries are created with v20 minimum values during protocol upgrade. Classic transactions and simple Soroban work; complex contracts are limited. Full Soroban config support (`get-settings-upgrade-txs`) is deferred.

## Implementation Order

| Phase | Gap | Description | Effort | Status |
|-------|-----|-------------|--------|--------|
| 1 | Gap 1 | Genesis ledger in `new-db` | Medium | **Done** |
| 2 | Gap 2 | Real `force-scp` implementation | Small-Medium | **Done** |
| 3 | Gap 3 | Real `new-hist` implementation | Small | **Done** |
| 4 | Gap 4 | `/upgrades?mode=set` HTTP endpoint | Medium | **Done** |
| 5 | Gap 5 | `ARTIFICIALLY_ACCELERATE_TIME_FOR_TESTING` | Small | **Done** |
| 6 | Gap 6 | Automatic history publishing in `run` | Medium | **Done** |

All phases complete. The quickstart `--local` mode is fully operational with henyey replacing stellar-core. Verified end-to-end: validator closes ledgers at 1/sec, checkpoints publish to local archive, captive core catches up, RPC and Horizon serve requests, Friendbot creates accounts, and transactions submitted via RPC are included in ledgers.
