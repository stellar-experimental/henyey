# stellar-core Parity Status

**Crate**: `henyey-app`
**Upstream**: `stellar-core/src/main/`
**Overall Parity**: 70%
**Last Updated**: 2026-04-27

## Summary

| Area | Status | Notes |
|------|--------|-------|
| Application lifecycle and runtime wiring | Full | Init, run, catchup, shutdown, recovery loops; `lost_sync_count` metric matches stellar-core single-site `mLostSync.Mark()` (#2612). The operational readiness generation/barrier is henyey-only extension plumbing with no protocol-visible behavior. |
| Configuration loading and compat translation | Partial | Core TOML and captive-core translation work; many stellar-core helpers omitted |
| HTTP admin and query surfaces | Partial | Core endpoints exist including generateLoad; several compat admin routes are stubbed or absent |
| Compat `/metrics` medida rates/percentiles | Partial | EWMA meter rates (`scp.value.valid`/`scp.value.invalid`, `ledger.ledger.close` rate fields) are an **exact** port of medida `ewma.cc`/`meter.cc`; timer/histogram percentiles (`ledger.ledger.close`, `ledger.transaction.count`) use an R7-over-256-sample-ring **documented approximation** of medida's CKMS-30s sample (#3296) — see [Compat `/metrics` medida accumulators](#compat-metrics-medida-accumulators-srcmedida_compatrs) |
| Catchup and restart recovery | Full | Archive catchup, replay, restart restore, publish flow wired |
| Persistent state integration | Partial | Critical state persisted through `henyey-db`; some SCP helper APIs absent |
| Transaction flooding | Partial | Pull-mode advert/demand scheduling; classic flood budgets use stellar-core truncate-then-round integer arithmetic with distinct `FLOOD_TX_PERIOD_MS` for broadcast budget; Soroban advert/demand capacity remains a gap; advert flush cadence is consolidated with broadcast (200 ms) rather than separate (100 ms); removed non-parity `base_fee` overlay pre-filter on `GeneralizedTxSet` receive (stellar-core does zero validation at receive layer); overlay receive layer now matches stellar-core (zero structural validation — all validation deferred to herder via `prepare_for_apply`); buffered ledger close validates tx sets via `prepare_for_apply` before application (matching stellar-core `LedgerManagerImpl.cpp:1542`); flood advert stamping uses `tracking_consensus_ledger_index()` matching stellar-core `Peer::recvFloodAdvert()` (`Peer.cpp:2089`) |
| Background maintenance | Full | Periodic pruning and RPC-retention cleanup implemented; **divergence**: stale publish queue entries are evicted after `MAX_PUBLISH_QUEUE_CHECKPOINT_DISTANCE` (30) intervals to prevent unbounded retention from failing archives — stellar-core does not evict (#2004) |
| Survey and network diagnostics | Partial | Time-sliced surveys implemented; survey ledger stamping uses `tracking_consensus_ledger_index()` matching stellar-core `SurveyManager.cpp`; `Diagnostics::bucketStats()` absent |
| Metadata streaming | Full | Main stream, debug rotation, gzip segments supported |
| Logging and runtime controls | Partial | Dynamic log levels work; compat `/ll` behavior is incomplete |
| Banned account persistence | None | No `FILTERED_G_ADDRESSES` / account-ban subsystem yet |

## File Mapping

| stellar-core File | Rust Module | Notes |
|--------------------|-------------|-------|
| `Application.h` / `ApplicationImpl.h` | `src/app/mod.rs` | Main runtime object and subsystem ownership |
| `ApplicationImpl.cpp` | `src/app/lifecycle.rs` | Event loop, startup, shutdown, timers |
| `ApplicationImpl.cpp` | `src/app/ledger_close.rs` | Ledger-close persistence and buffered apply |
| `ApplicationImpl.cpp` | `src/app/catchup_impl.rs` | Catchup orchestration and restart recovery |
| `ApplicationImpl.cpp` | `src/app/consensus.rs` | SCP recovery, timeout, and sync logic |
| `ApplicationImpl.cpp` | `src/app/peers.rs` | Peer inspection, connect/drop/unban helpers |
| `ApplicationImpl.cpp` | `src/app/publish.rs` | History checkpoint publishing |
| `ApplicationImpl.cpp` | `src/app/survey_impl.rs` | Survey command execution and aggregation |
| `ApplicationImpl.cpp`, `TransactionQueue.cpp`, `TxAdverts.cpp`, `TxDemandsManager.cpp` | `src/app/tx_flooding.rs` | Tx advert/demand scheduling; classic flood budget rounding matches stellar-core integer arithmetic; broadcast period uses `flood_tx_period_ms` (200 ms, matching `FLOOD_TX_PERIOD_MS`) |
| `Config.h` / `Config.cpp` | `src/config.rs`, `src/compat_config.rs` | Native TOML config plus stellar-core-format translation |
| `CommandHandler.h` / `CommandHandler.cpp` | `src/http/mod.rs`, `src/http/handlers/`, `src/compat_http/` | Native Axum server plus compat wire-format server |
| `lib/libmedida/.../stats/ewma.cc`, `meter.cc`, `stats/snapshot.cc` | `src/medida_compat.rs` | EWMA meter rates (exact); R7-reservoir timer/histogram percentiles (documented CKMS approximation) for the compat `/metrics` endpoint (#3296) |
| `QueryServer.h` / `QueryServer.cpp` | `src/http/mod.rs`, `src/http/handlers/query.rs` | Separate query server with snapshot lookups |
| `Maintainer.h` / `Maintainer.cpp` | `src/maintainer.rs` | Automatic background maintenance |
| `PersistentState.h` / `PersistentState.cpp` | `src/app/mod.rs`, `henyey-db` | App owns usage; storage primitives live in `henyey-db` |
| `ApplicationUtils.h` | `src/run_cmd.rs`, `src/catchup_cmd.rs`, `src/app/*.rs` | Runtime subset only; many CLI utilities live elsewhere |
| `Diagnostics.h` / `Diagnostics.cpp` | — | No Rust equivalent in this crate |
| `BannedAccountsPersistor.h` / `BannedAccountsPersistor.cpp` | — | No Rust equivalent in this crate |

## Component Mapping

### Application core (`src/app/mod.rs`, `src/app/lifecycle.rs`, `src/app/ledger_close.rs`, `src/app/catchup_impl.rs`, `src/app/consensus.rs`)

Corresponds to: `Application.h`, `ApplicationImpl.h`

| stellar-core | Rust | Status |
|--------------|------|--------|
| `Application::create()` | `App::new()` | Full |
| `initialize()` | `App::new()` initialization path | Full |
| `resetLedgerState()` | — | None |
| `timeNow()` | runtime clock usage in `App` | Full |
| `getConfig()` | `App::config()` | Full |
| `getState()` / `getStateHuman()` | `App::state()`, `AppState` display | Full |
| `isStopping()` | shutdown state tracking | Full |
| `getMetrics()` / `clearMetrics()` | ad-hoc counters plus `App::clear_metrics()` | Partial |
| `syncOwnMetrics()` / `syncAllMetrics()` | — | None |

> **Note:** `stellar_ledger_close_time_ms` was removed — its data is captured by the `stellar_ledger_close_duration_seconds` histogram.

| subsystem getters (`getLedgerManager`, `getBucketManager`, `getHerder`, `getOverlayManager`, `getDatabase`) | direct `App` accessors and owned fields | Full |
| `getHistoryArchiveManager()` / `getHistoryManager()` / `getHerderPersistence()` / `getInvariantManager()` / `getPersistentState()` / `getWorkScheduler()` / `getStatusManager()` | distributed across `App` + sibling crates | Partial |
| `getBannedAccountsPersistor()` | — | None |
| `postOnMainThread()` / background posting helpers | Tokio spawning and async tasks | Full |
| `start()` / `gracefulStop()` / `joinAllThreads()` | `App::run()`, `App::shutdown()`, task shutdown | Full |
| `manualClose()` | `App::manual_close_ledger()` | Partial |
| `applyCfgCommands()` / `reportCfgMetrics()` | — | None |
| `getJsonInfo()` / `reportInfo()` | `App::info()`, `print_startup_info()` | Full |
| `scheduleSelfCheck()` | `App::self_check()` only | Partial |
| `getNetworkID()` / `validateNetworkPassphrase()` | `App::network_id()`, startup validation | Full |

### Configuration (`src/config.rs`, `src/compat_config.rs`)

Corresponds to: `Config.h`

| stellar-core | Rust | Status |
|--------------|------|--------|
| `Config()` | `AppConfig::default()` | Full |
| `load(filename)` | `AppConfig::from_file()` | Full |
| `load(istream)` | — | None |
| `adjust()` | — | None |
| `resolveNodeID()` / `toShortString()` / `toStrKey()` / `toString(qset)` | — | None |
| network, overlay, history, maintenance, metadata, diagnostics, query fields | `AppConfig` sub-structs | Full |
| `FORCE_SCP`, `MANUAL_CLOSE`, `CATCHUP_COMPLETE`, `CATCHUP_RECENT` | native fields in `AppConfig` | Full |
| stellar-core flat config parsing | `translate_stellar_core_config()` | Full |
| `generateQuorumSet()` / `generateQuorumSetHelper()` / `computeDefaultThreshold()` | `generate_quorum_set()` / `generate_quorum_set_helper()` in `compat_config.rs` — hierarchical per-home-domain/quality auto-qset (sort quality-DESC/domain-ASC, SIMPLE_MAJORITY per-domain inner sets, BFT/ALL_REQUIRED top, HIGH/CRITICAL≥3 + ascending-quality guards). Thresholds encoded as percents (51/67/100) and converted via `QuorumSetConfig::to_xdr` (`1+(n*p−1)/100`), byte-identical to core for tier-1 sizes (n≤50 SM / n≤102 BFT). | Full |
| `loadQset()` (nested `[QUORUM_SET.subN]`) | `load_qset()` in `compat_config.rs` — recursive, default 67%, `parseNodeID` first-token node parsing, any table-valued key = inner set, level>4 rejection, empty-qset rejection | Full |
| `PREFERRED_PEER_KEYS`, `PREFERRED_PEERS_ONLY` | Translated via strict helpers in compat config; passed to overlay | Full |
| `INVARIANT_CHECKS`, `INVARIANT_EXTRA_CHECKS`, `STATE_SNAPSHOT_INVARIANT_LEDGER_FREQUENCY` | Validated in compat translation; non-default values of the two security-relevant keys are rejected (no InvariantManager subsystem in henyey). The frequency tuning knob is silently accepted as known-but-unsupported. | Partial |
| testing knobs (`ARTIFICIALLY_*`, `LOADGEN_*`, `APPLY_LOAD_*`) | small supported subset only | Partial |
| helper methods such as `modeDoesCatchupWithBucketList()`, `allBucketsInMemory()`, `parallelLedgerClose()`, `setNoListen()`, `setNoPublish()` | — | None |

### Run and catchup orchestration (`src/run_cmd.rs`, `src/catchup_cmd.rs`, `src/app/catchup_impl.rs`, `src/app/publish.rs`)

Corresponds to: `ApplicationUtils.h` runtime subset

| stellar-core | Rust | Status |
|--------------|------|--------|
| `setupApp()` | `App::new()` + `run_node()` setup path | Full |
| `runApp()` | `run_node()` | Full |
| `initializeDatabase()` | DB init inside `App::new()` | Full |
| `selfCheck()` | `App::self_check()` | Partial |
| `catchup()` | `run_catchup()` / `App::catchup_with_mode()` | Full |
| `applyBucketsForLCL()` | catchup bucket-apply flow | Full |
| `publish()` | publish flow in `src/app/publish.rs` | Full |
| `writeCatchupInfo()` | — | None |
| `setForceSCPFlag()` / `httpCommand()` / `mergeBucketList()` / `dumpStateArchivalStatistics()` / `calculateAssetSupply()` / `reportLastHistoryCheckpoint()` | — | None |

### HTTP command surface (`src/http/handlers/`, `src/compat_http/handlers/`)

Corresponds to: `CommandHandler.h`

| stellar-core | Rust | Status |
|--------------|------|--------|
| `info()` / `metrics()` / `peers()` / `quorum()` / `scpInfo()` / `tx()` / `upgrades()` / `dumpProposedSettings()` / `sorobanInfo()` | native and compat handlers | Full |
| `upgrades()` `mode=set` parameter set | compat handler parses the full SSC param set 1:1 with `CommandHandler.cpp:613-671`; validated offline (#3300) by `test_upgrades_set_parses_full_ssc_param_set`; two read/validation divergences documented below | Full (set path) |
| `maintenance()` / `clearMetrics()` / `selfCheck()` | native and compat handlers | Full |
| `generateLoad()` | native and compat handlers (feature-gated via `loadgen`) | Full |
| `testAcc()` | compat handler with deterministic key derivation | Full |
| `manualClose()` | works, but explicit seq/time params are rejected in compat/native handlers | Partial |
| `connect()` / `dropPeer()` / `unban()` / `bans()` | native and compat handlers wired to overlay/DB side effects; two intentional compat divergences (see note below) | Full |
| `ll()` | native dynamic log control works; compat handler is minimal | Partial |
| `logRotate()` | placeholder response only | Partial |
| `banaccounts()` / `unbanaccounts()` | — | None |
| legacy survey commands (`surveyTopology()`) | — (removed upstream in favor of time-sliced) | None |
| time-sliced survey commands (`startSurveyCollecting()`, `stopSurveyCollecting()`, `surveyTopologyTimeSliced()`, `stopSurvey()`, `getSurveyResult()`) | native handlers implemented; compat endpoints wired to native `App` survey methods with stellar-core param names (`nonce`/`node`/`inboundpeerindex`/`outboundpeerindex`), plain-text `retStr` (JSON for `getsurveyresult` with **strkey**-encoded `backlog`/`badResponseNodes`), and the `Synced`/`Validating` booted gate (#3298). One bounded, documented divergence on `surveytopologytimesliced`'s duplicate/self-peer edge (native fused bool) | Full |

**Intentional compat divergences for `connect` / `dropPeer` / `unban` / `bans`** (CommandHandler.cpp:478/~543/566/553), both documented and deliberate:

1. **Trailing newline.** stellar-core's `retStr` carries no `\n`; henyey's compat
   plain-text handlers append a trailing `\n` by the established compat convention
   (e.g. `maintenance` → `"Done\n"`), so all compat plain-text endpoints are
   internally consistent. Message text otherwise byte-matches core
   (`"Connect to: PEER:PORT"`, `"Drop peer: NODE"`, `"Drop and ban peer: NODE"`,
   `"Peer NODE not found"`, `"Unban peer: NODE"`, and the two "Must specify…"
   guidance strings).
2. **Empty `/bans` shape.** stellar-core's `bans()` emits jsoncpp `{"bans": null}`
   when no bans exist; henyey emits `{"bans": []}` to match its own native
   `/bans` (`BansResponse`) and because an empty array is unambiguous. SSC /
   stellar-rpc consumers treat the two equivalently.

**Documented `/upgrades` read/validation divergences (#3300).** The
parity-load-bearing path for an SSC protocol-upgrade mission is `mode=set`
(the harness drives upgrades by setting them); that path parses the exact
stellar-core parameter set (`upgradetime`, `protocolversion`, `basefee`,
`basereserve`, `maxtxsetsize`, `flags`, base64 `configupgradesetkey`,
`maxsorobantxsetsize`, `nominationtimeoutlimit`, `expirationminutes`) and is
pinned 1:1 by `test_upgrades_set_parses_full_ssc_param_set`. Two divergences are
on read/validation-feedback paths the mission's SET→observe-externalize flow
does not require — recorded here as **candidate follow-ups**, not fixed in the
validation PR:

1. **Empty-mode read shape.** stellar-core returns `"mode required"` on empty
   mode and serializes `getUpgradesJson()` (`{time,version,fee,maxtxsize,reserve}`,
   `Upgrades.cpp:58-62`) only under `mode=get`. Henyey returns a bespoke
   `{current,scheduled}` body on the empty/default mode. Follow-up only if the
   live mission reads upgrade state via `/upgrades?mode=get`.
2. **SET-time `configupgradesetkey` validation.** stellar-core validates the key
   via `ConfigUpgradeSetFrame::makeFromKey` + `isValidForApply` at SET time and
   rejects an invalid key with `"Error setting configUpgradeSet"`
   (`CommandHandler.cpp:648-655`). Henyey decodes the key but **defers** validity
   to nomination/apply (an invalid key simply never nominates). Likely benign,
   but loud-vs-silent rejection differs. Follow-up only if the live mission
   relies on SET-time rejection feedback.

### Query server (`src/http/mod.rs`, `src/http/handlers/query.rs`)

Corresponds to: `QueryServer.h`

| stellar-core | Rust | Status |
|--------------|------|--------|
| `QueryServer(...)` | `QueryServer::new()` | Full |
| `getLedgerEntryRaw()` | `getledgerentryraw_handler()` | Full |
| `getLedgerEntry()` | `getledgerentry_handler()` | Full |

### Persistent state and maintenance (`src/app/mod.rs`, `src/maintainer.rs`)

Corresponds to: `PersistentState.h`, `Maintainer.h`

| stellar-core | Rust | Status |
|--------------|------|--------|
| `PersistentState::getState()` / `setMainState()` / `setMiscState()` | `henyey-db::StateQueries` usage from `App` | Full |
| `getSCPStateAllSlots()` | `ScpQueries::get_scp_state_all_slots()` | Full |
| `getTxSetsForAllSlots()` / `setSCPStateV1ForSlot()` | partial `ScpQueries` support | Partial |
| `getTxSetHashesForAllSlots()` / `hasTxSet()` / `deleteTxSets()` | — | None |
| `REBUILD_FOR_OFFER_TABLE` flag (`set`/`should`/`clear`) | `CATCHUP_PERSIST_PENDING` sentinel + non-authoritative pre-LCL writes (§14.5 two-window design; see `crates/app/README.md`) | Partial (startup/catchup path is safe; CLI readers still use `MAX(ledgerseq)`) |
| `Maintainer::start()` / `performMaintenance()` | `Maintainer::start()`, `perform_maintenance()`, `perform_maintenance_with_count()` | Full |

### Surveys, metadata, and logging (`src/survey.rs`, `src/meta_stream.rs`, `src/logging.rs`)

Corresponds to: survey parts of `CommandHandler.cpp`, metadata output in `ApplicationImpl.cpp`, and `Diagnostics.h`

| stellar-core | Rust | Status |
|--------------|------|--------|
| time-sliced survey collection/reporting | `SurveyDataManager` | Full |
| survey message dedup/rate limiting | `SurveyMessageLimiter` | Full |
| metadata output stream (`METADATA_OUTPUT_STREAM`) | `MetaStreamManager` | Full |
| rotating debug metadata segments | `MetaStreamManager::maybe_rotate_debug_stream()` | Full |
| dynamic partition log levels | `LogLevelHandle` | Full |
| `diagnostics::bucketStats()` | — | None |

### Account-ban persistence

Corresponds to: `BannedAccountsPersistor.h`

| stellar-core | Rust | Status |
|--------------|------|--------|
| persisted banned-account store | — | None |
| `FILTERED_G_ADDRESSES` migration | — | None |
| `banaccounts` / `unbanaccounts` HTTP integration | — | None |

## Intentional Omissions

Features excluded by design. These are NOT counted against parity %.

| stellar-core Component | Reason |
|------------------------|--------|
| `AppConnector` | Tokio + `Arc`/locks replace thread-isolation helper API |
| `VirtualClock` / explicit `ThreadType` / per-subsystem io_context getters | Runtime uses Tokio tasks instead of ASIO thread pools |
| `TmpDirManager`, `ProcessManager`, raw `LedgerTxnRoot` exposure | Different runtime architecture; not part of `henyey-app`'s public API |
| Protocol-23 corruption verifier/reconciler | Repository targets protocol 24+ only |
| `CommandLine.h`, `SettingsUpgradeUtils.h`, and `dumpxdr.h` utilities | Owned by the `henyey` binary crate, not `henyey-app` |
| `minimalDBForInMemoryMode()` / `canRebuildInMemoryLedgerFromBuckets()` | Test-only upstream helpers not mirrored in this crate. The corresponding `new-db --minimal-for-in-memory-mode` CLI flag (in the `henyey` binary crate) is an intentional accepted-no-op — parity-correct, since v26 stellar-core removed the flag and always builds a full persistent DB (#3299). |
| `BUILD_TESTS`-only overlay toggle (`getRunInOverlayOnlyMode` / `setRunInOverlayOnlyMode`) | Rust test strategy uses different hooks and feature gates |
| `testTx()` | Test-only wire-format endpoint; no production use |

## Gaps

Features not yet implemented. These ARE counted against parity %.

| stellar-core Component | Priority | Notes |
|------------------------|----------|-------|
| `BannedAccountsPersistor` and `FILTERED_G_ADDRESSES` flow | High | No persisted banned-account subsystem or admin endpoints |
| `manualclose` explicit sequence/close-time parameters | Medium | Upstream standalone semantics are not exposed by handlers |
| Scheduled online self-check parity | Medium | Manual self-check exists, but upstream periodic scheduling test is unmatched |
| `PersistentState` tx-set hash helpers | Medium | Several SCP persistence helpers remain absent |
| `diagnostics::bucketStats()` | Low | No bucket statistics offline tool |
| `Config` helper methods (`resolveNodeID`, stringifiers, adjust/no-listen/no-publish) | Low | Native config model omits these convenience APIs |
| `writeCatchupInfo()` | Low | No catchup-info file output helper |

## Architectural Differences

1. **Async Runtime**
   - **stellar-core**: ASIO `io_context` instances split across main, worker, overlay, eviction, and ledger-close threads.
   - **Rust**: A Tokio runtime drives all async work, with blocking work isolated only where needed.
   - **Rationale**: The Rust crate centralizes concurrency around futures/tasks instead of exposing thread-specific interfaces.

2. **Configuration model**
   - **stellar-core**: A large mutable `Config` object with many testing-only knobs and helper methods.
   - **Rust**: Serde-backed typed config structs plus a separate compatibility translator for flat stellar-core TOML.
   - **Rationale**: The crate keeps runtime config strongly typed while still accepting stellar-core captive-core files.

3. **HTTP surfaces**
   - **stellar-core**: One command server defines both behavior and wire format.
   - **Rust**: Native Axum endpoints and a second compatibility server coexist.
   - **Rationale**: The native API is cleaner for henyey callers, while compat routes only cover the stellar-rpc subset currently needed.

4. **Persistence split**
   - **stellar-core**: `PersistentState` lives inside `src/main/`.
   - **Rust**: `henyey-app` owns restart/catchup policy while low-level persistence APIs live in `henyey-db`.
   - **Rationale**: The workspace factors storage concerns into a dedicated crate instead of keeping them inside the app layer.

5. **Stateless fresh-genesis recovery (`src/app/catchup_impl.rs`, #3410)** — operator-approved deviation
   - **stellar-core**: a fresh node joining a network with existing history
     adopts archive state via `CatchupWork::downloadApplyBuckets`; a knit-to-LCL
     disagreement is surfaced as a retryable `CatchupWork::fatalFailure()`
     boolean, and core has no self-wipe-on-corruption mechanism at all.
   - **Rust**: when a node's LCL is **exactly genesis** (ledger 1, i.e. no
     committed local state), a forced recovery catchup is routed to an
     archive-authoritative bucket-apply (effective `Minimal` depth, so
     `CatchupRange::calculate` selects `BucketsOnly` / `BucketApplyAndReplay`
     instead of replay-from-genesis), and a knit-to-LCL mismatch at genesis is
     reclassified as a retryable bucket-apply (`catchup_needs_full_reset`)
     rather than escalating to `FATAL: unrecoverable local state failure`. The
     inter-catchup-retry backoff is also shortened at genesis (3s vs the 10s
     steady-state cooldown) so a fresh node re-attempts the bucket-apply quickly.
   - **Rationale**: under the SSC mixed-image mission a fresh Henyey's
     force-bootstrapped genesis hash legitimately differs from the
     earlier-started peers' archive history chain, so a replay-from-genesis
     would always fail `verify_knit_to_lcl` and the resulting FATAL was a
     cold-start crash, not real corruption (#3410). The carveout is gated
     **strictly on `LCL == genesis`**: for `LCL > genesis` the `CatchupRange`
     selection and the knit-to-LCL fatal classification are byte-for-byte
     unchanged, fully preserving the #3282/#3288 terminal-wipe semantics for a
     node that has real state to protect. Change 2 (don't-FATAL-at-genesis) is
     additive henyey-specific robustness with no analogue or divergence for
     core's non-genesis path.

## Test Coverage

| Area | stellar-core Tests | Rust Tests | Notes |
|------|-------------------|------------|-------|
| Config and compat translation | 9 TEST_CASE / 21 SECTION | 64 `#[test]` | Strong coverage for loading, validation, and captive-core translation |
| Command handler / compat HTTP | 5 TEST_CASE / 27 SECTION | 63 `#[test]` | Includes handler helpers, generateLoad, testacc, live-App peer-admin tests (connect/droppeer/unban/bans), and live-App survey tests (start/stop-collecting, surveytopologytimesliced, stopsurvey, getsurveyresult — booted gate, param validation, strkey projection) (#3298) |
| Query server | 1 TEST_CASE / 9 SECTION | 7 `#[test]` | Good coverage of lookup ordering and validation |
| Run/catchup utilities | 4 TEST_CASE / 5 SECTION | 19 `#[test]` | Target parsing and run-mode helpers are well covered |
| Self-check scheduling | 1 TEST_CASE / 0 SECTION | 0 `#[test]` | No dedicated periodic self-check scheduling tests |
| Maintenance | 0 TEST_CASE / 0 SECTION | 12 `#[test]` | Strong regression coverage for retention thresholds |
| Banned accounts | 4 TEST_CASE / 21 SECTION | 0 `#[test]` | Subsystem not implemented; upstream has comprehensive tests |
| App core types/runtime | — | 35 `#[test]` | App state, recovery bookkeeping, and runtime helpers |
| Metadata and logging | — | 10 `#[test]` | Basic coverage for stream rotation and log-level handling |

### Test Gaps

- Compat peer-admin endpoints (`connect`, `droppeer`, `unban`, `bans`) and survey-control endpoints (`startsurveycollecting`, `stopsurveycollecting`, `surveytopologytimesliced`, `stopsurvey`, `getsurveyresult`) now have live-App tests asserting the booted gate, required-param/strkey validation, plain-text-vs-JSON medium, and strkey-encoded peer lists (#3298). The `surveytopologytimesliced` success-token branch (Survey started vs already running) needs a fully-booted App and is covered only by the documented bounded-divergence note.
- There is no Rust equivalent of upstream's scheduled online self-check test in `SelfCheckTests.cpp`.
- Account-ban persistence has no Rust tests because the subsystem is not implemented. Upstream has 4 TEST_CASE / 21 SECTION.
- HTTP threaded server behavior (3 TEST_CASE upstream in `HttpThreadedTests.cpp`) has no direct equivalent.

### Compat `/metrics` medida accumulators (`src/medida_compat.rs`)

Corresponds to: `lib/libmedida/src/medida/stats/ewma.cc`, `meter.cc`,
`stats/snapshot.cc`; reported by `src/compat_http/handlers/metrics.rs`.

stellar-core's `/metrics` endpoint emits medida JSON with in-process EWMA rates
and timer/histogram percentiles. henyey's native metrics layer is Prometheus-based
and does not maintain these, so the compat handler previously emitted hardcoded
`0.0`s. `src/medida_compat.rs` adds in-process accumulators for exactly the four
metrics SSC missions read (#3296):

| medida metric | henyey accumulator | Parity |
|---------------|--------------------|--------|
| `ledger.ledger.close` rate fields (`mean_rate`, `1/5/15_min_rate`) | `EwmaMeter` (embedded, `event_type="calls"`) | **Full** — exact port of `ewma.cc`/`meter.cc`: alphas `1-exp(-5/(60·N))`, 5s lazy `TickIfNecessary`, first-sample seeding, `mean_rate = count·1e9/elapsed_ns` |
| `scp.value.valid` / `scp.value.invalid` meters | `EwmaMeter` (`event_type="value"`, `HerderSCPDriver.cpp:55,57`) | **Full** — same EWMA port; fed app-side by delta-marking the already-exposed `ScpMetricsSnapshot` cumulative totals on the periodic metrics refresh (no herder→app coupling) |
| `ledger.ledger.close` / `ledger.transaction.count` percentiles + `min/max/mean/stddev/sum` | `ReservoirSample` (256-entry ring + R7 interpolation) | **Partial** — see divergence below |

**Documented percentile divergence (Partial).** stellar-core's default
`Timer`/`Histogram` sample is a **CKMS** error-bounded streaming sketch over a
30-second sliding window (`Timer::GetSnapshot()` → `Snapshot::CKMSImpl::getValue`).
henyey instead keeps the last **256 observations** in a fixed-capacity ring and
computes percentiles with the **R7 (Hyndman-Fan) interpolation over a sorted
vector** copied verbatim from medida's `snapshot.cc Snapshot::VectorImpl::getValue`.
Two precise differences:

1. **Algorithm:** R7 exact-sorted interpolation vs CKMS error-bounded sketch — the
   percentile *values* differ.
2. **Window:** a **256-observation capacity window, NOT a time window** vs CKMS's
   30-second time window. Under accelerated-time missions (e.g. 1s closes) 256
   closes span ~256s of history, far wider than 30s, so percentiles are
   smoother/laggier than stellar-core's.

This is sufficient for SSC's presence/ordering/non-zero assertion class (SSC has
no oracle for the exact CKMS values a henyey node would produce). `min`/`max`/
`mean`/`sum` are stored/scaled in milliseconds to match stellar-core's timer
`duration_unit`. If a mission proves sensitive to the window, the ring capacity
can be tuned or a real CKMS port landed as a follow-up.

## Verification Results

- **Testnet verification**: Node successfully syncs and tracks consensus on testnet, closing ledgers in parity with stellar-core validators.
- **Catchup gap recovery**: Successfully bridges 20-30 slot gaps between catchup checkpoint and live consensus (verified January 2026).
- **Event loop stability**: Multiple event loop freeze bugs identified and fixed (February 2026): blocking flood demand sends, unbounded buffered ledger close loops, blocking bucket GC during catchup, and SCP drain starvation.
- **Post-catchup convergence**: Fixed several convergence failures including dead loops targeting stale checkpoints, deadlocks from frozen `latest_externalized`, and SCP EXTERNALIZE envelope emission for validator nodes (March–April 2026).
- **TX queue parity**: Implemented stellar-core `updateQueue` semantics with correct invalidation and revalidation ordering (March 2026).
- **Audit fixes**: Resolved audit findings including config passphrase matching, quorum threshold rounding, unsolicited quorum set rejection, compat config validator entry validation, and compat `SURVEYOR_KEYS` translation (March–April 2026).
- **Out-of-sync recovery parity (#2909)**: Recovery now matches stellar-core's two-part sequence: rebroadcast only current-slot `getLatestMessagesSend(lcl+1)` envelopes, then issue bounded `GetScpState` pull to up to 2 random authenticated peers (previously flooded historical envelopes from `getSCPState(lcl-5)` and broadcast `GetScpState` to all peers).
- **Survey protocol**: Time-sliced surveys successfully collect and report topology data from testnet peers.

## Parity Calculation

| Category | Count |
|----------|-------|
| Implemented (Full) | 92 |
| Gaps (None + Partial) | 40 |
| Intentional Omissions | 24 |
| **Parity** | **92 / (92 + 40) = 70%** |
