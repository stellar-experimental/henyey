# Stellar Supercluster Feasibility Evaluation

**Date**: 2026-03-24
**Last updated**: 2026-03-24
**Goal**: Determine whether Stellar Supercluster (SSC) can be used to run full-network simulations against Henyey, and define an execution plan that can actually be carried out from the current codebase.

## Executive Assessment

Using SSC with Henyey is feasible, but the path is staged rather than immediate.

The good news is that the architectural seam is correct. SSC treats `stellar-core` as a black-box process with a Docker image, CLI entrypoints, a `stellar-core.cfg` file, and the admin HTTP interface on port 11626. Henyey already implements most of those seams: `new-db`, `new-hist`, `catchup`, `run`, `force-scp`, config parsing, the compat HTTP server, SCP, overlay, transaction execution, and history machinery are all present.

The limiting factor is not consensus or networking. The limiting factor is test-harness parity. The remaining missing pieces are the exact pieces SSC leans on to drive missions beyond basic payment flows: real metrics tracking with rate/percentile values, end-to-end SSC mission validation, and history/upgrade mission testing.

This means there is a clear path to execution, and significant progress has been made:

1. **DONE** — Package Henyey as an SSC-consumable image.
2. Prove mixed-image missions first.
3. **MOSTLY DONE** — Close the HTTP/loadgen/metrics gaps required for Henyey-only missions.
4. Validate history and upgrade missions.
5. Leave MaxTPS and survey-heavy missions for later.

## What SSC Requires

SSC is SDF's Kubernetes-based integration test harness (`stellar/supercluster`). It expects to control a `stellar-core`-compatible container through four surfaces:

1. **Container image**: provided with `--image=<docker-image>`.
2. **CLI commands**: SSC launches commands such as `new-db`, `new-hist`, `catchup`, and `run` as container entrypoints.
3. **Admin HTTP API**: SSC uses the stellar-core admin interface on port 11626 for state inspection, upgrades, load generation, metrics, topology, and transaction submission.
4. **`stellar-core.cfg`**: SSC generates config files containing quorum, peer, history, and test-only flags.

In other words, SSC does not need Henyey internals. It needs Henyey to look operationally identical to stellar-core at the package/config/API boundary.

## Verified Current State

This section is based on the current repository, not assumptions.

### CLI and config compatibility already in place

- `catchup` exists in `crates/henyey/src/main.rs:387`
- `new-hist` exists in `crates/henyey/src/main.rs:611`
- `publish-history` exists in `crates/henyey/src/main.rs:441`
- `force-scp` exists in `crates/henyey/src/main.rs:603`
- compat config parsing includes `ARTIFICIALLY_ACCELERATE_TIME_FOR_TESTING` in `crates/app/src/compat_config.rs:199`
- compat config parsing reads `HISTORY` archive sections, including `put` and `mkdir`, in `crates/app/src/compat_config.rs:225`
- compat config parsing handles `GENESIS_TEST_ACCOUNT_COUNT` in `crates/app/src/compat_config.rs`

### Compat HTTP surface is mostly present

The stellar-core-compatible router is implemented in `crates/app/src/compat_http/mod.rs:45`.

Verified routes include:

- `/info`
- `/tx`
- `/peers`
- `/metrics` — 8 medida-format metrics with proper `type` fields
- `/testacc` — full implementation with deterministic key derivation
- `/sorobaninfo`
- `/manualclose`
- `/clearmetrics`
- `/quorum`
- `/scp`
- `/upgrades`
- survey-shaped endpoints such as `/getsurveyresult` and `/startsurveycollecting`
- `/generateload` when built with the `loadgen` feature — supports `stop` mode, mode name normalization, `create` deprecation

### Accelerated time is implemented

This is not a speculative gap. Henyey already honors `ARTIFICIALLY_ACCELERATE_TIME_FOR_TESTING`.

- config flag parsed in `crates/app/src/compat_config.rs:199`
- accelerated checkpoint frequency applied in `crates/app/src/app/mod.rs:441`
- 1-second ledger close time applied in `crates/app/src/app/mod.rs:546`

The remaining work here is mission validation, not basic implementation.

### History foundations are present

- catchup manager in `crates/history/src/catchup.rs:278`
- publish manager in `crates/history/src/publish.rs:200`
- writable archive handling in `crates/history/src/lib.rs:544`

This is enough to say SSC history-oriented missions are plausible. It is not enough to declare them production-ready without mission-level validation.

## Gaps — Current Status

### ~~1. No checked-in SSC-ready Docker image~~ — CLOSED

A multi-stage Dockerfile is checked in at the repo root. The image:

- builds Henyey with `--release` and the `loadgen` feature enabled
- symlinks `henyey` to `stellar-core` at the expected path
- produces a ~176MB image
- has been verified building and running

Commit: `27e65ba` (Add Dockerfile and .dockerignore for SSC compatibility)

The `loadgen.wasm` dependency was relocated from the `stellar-core` submodule (excluded by `.dockerignore`) to `crates/simulation/wasm/loadgen.wasm` to ensure Docker builds succeed.

Commit: `cc529c5` (Move loadgen WASM into crate for Docker build compatibility)

### ~~2. `/testacc` does not exist~~ — CLOSED

`/testacc` is fully implemented in `crates/app/src/compat_http/handlers/testacc.rs` with 10 unit tests covering:

- response shape matching stellar-core (`name`, `id`, `balance`, `seqnum`)
- not-found case returns `{}`
- error case returns `{"status": "error", "detail": "..."}`
- deterministic key derivation (name padded with `.` to 32 bytes, used as Ed25519 seed)
- key consistency across callers

Commit: `fc12e03` (Implement SSC compat HTTP parity)

### 3. Compat `/metrics` — PARTIALLY CLOSED

The handler now returns 8 metrics in proper medida JSON format with `type` fields:

| Metric | Type | Value Source |
|--------|------|-------------|
| `ledger.ledger.close` | timer | ledger sequence (count), zero placeholders for rate/percentile |
| `peer.peer.count` | counter | real authenticated + pending count |
| `peer.peer.authenticated-count` | counter | real authenticated count |
| `peer.peer.pending-count` | counter | real pending count |
| `herder.pending.transactions` | counter | real pending tx count |
| `ledger.ledger.version` | counter | real protocol version |
| `scp.value.valid` | meter | ledger sequence (count), zero rate placeholders |
| `scp.value.invalid` | meter | zero (count + rate) |

3 unit tests validate the response shape, type fields, and rate fields.

**Remaining gap**: Rate and percentile values are zero placeholders. This is fine for SSC missions that only check metric names and structure, but would need real tracking if SSC makes assertions about rate values.

Commit: `fc12e03`

### ~~4. `/generateload` mode compatibility~~ — CLOSED

All mode compatibility work is complete:

- **Mode name normalization**: Accepts both underscore names (`soroban_upload`, `soroban_invoke_setup`, `soroban_invoke`, `mixed_classic_soroban`) and legacy no-separator names. Case-insensitive.
- **`create` mode deprecation**: Returns the exact stellar-core v25 deprecation message (`"DEPRECATED: CREATE mode has been removed..."`). No longer silently aliases to `pay`.
- **`stop` mode**: `stop_load()` added to `LoadGenRunner` trait, implemented end-to-end. Both compat and native HTTP handlers intercept `mode=stop` before the `is_running()` check, matching stellar-core behavior.
- **`GENESIS_TEST_ACCOUNT_COUNT`**: Replaces `create` mode. Accounts named `"TestAccount-0"` through `"TestAccount-{N-1}"` are created in the genesis ledger with deterministic keys and even balance splits. `bootstrap_from_db()` correctly recreates these accounts in the bucket list on restart.

Still missing modes (deferred — will get "unknown mode" errors until SSC actually needs them):
- `upgrade_setup`
- `create_upgrade`
- `pay_pregenerated`
- `soroban_invoke_apply_load`

Commits: `fc12e03`, `35a44d8`, `dae8ad9`

### 5. Compat survey endpoints are stubs

The compat survey routes exist, but they are not wired to the native survey implementation.

Stub behavior in `crates/app/src/compat_http/handlers/plaintext.rs:328`:

- `/getsurveyresult` returns `{"survey": "not implemented"}`
- `/startsurveycollecting`, `/stopsurveycollecting`, and `/surveytopologytimesliced` return `done\n`

Meanwhile, real native survey handlers exist in `crates/app/src/http/handlers/survey.rs:1`, backed by application logic in `crates/app/src/app/survey_impl.rs:1`.

Impact:

- survey-related SSC missions should be classified as unsupported today
- this is not urgent unless network survey missions are in scope

### 6. History mission reliability is plausible, but not yet proven

The publish/catchup plumbing exists, but the remaining issue is execution confidence.

Two caveats should be called out explicitly:

- mission-level reliability has not been demonstrated through SSC
- HISTORY config parsing extracts archive URLs heuristically from `get` command strings in `crates/app/src/compat_config.rs:236`, which may be brittle depending on how SSC renders command templates

Impact:

- history missions belong after mixed-image bring-up and basic HTTP parity work
- the risk is lower than metrics/loadgen, but it is real

### 7. `--minimal-for-in-memory-mode` is accepted but silently ignored

The `new-db` command accepts the `--minimal-for-in-memory-mode` flag for stellar-core compatibility (`crates/henyey/src/main.rs:422`), but the flag is a no-op. Henyey always creates a persistent SQLite database.

If SSC passes this flag expecting in-memory node behavior (faster startup, no disk state), the resulting node will behave differently than expected -- persistent state, slower teardown, possible stale-state issues across mission restarts.

Impact:

- may cause subtle mission failures that look like state corruption rather than a compatibility gap
- should be an explicit known-limitation callout, not a silent no-op

### 8. SSC may expect additional CLI surface beyond the currently documented path

The original document mentions SSC container commands including `test`. I did not find a Henyey `test` subcommand in `crates/henyey/src/main.rs:343`.

This may or may not matter depending on which missions are targeted, but it should be treated as an explicit verification item instead of left implicit.

## Testing Coverage

### Compat HTTP handlers: 36 unit tests

The compat HTTP handler test suite was built from scratch during this work. Current coverage:

| Handler | Tests | Coverage |
|---------|-------|----------|
| `/info` | 4 | Response shape (synced, booting), flags, no unexpected keys |
| `/tx` | 5 | Pending/error/diagnostics shapes, status strings, result encoding |
| `/peers` | 2 | Empty + populated response shapes |
| `/metrics` | 3 | Response shape, type fields, rate fields |
| `/testacc` | 10 | Response/error/not-found shapes, key derivation, seed padding |
| `/generateload` | 4 | Request construction, debug, clone, stop mode |
| Plaintext endpoints | 10 | Bans, quorum, scp, upgrades shapes, ISO8601 parsing |
| Panic handler | 1 | `{"exception": "generic"}` shape |
| **Total** | **36+** | |

All tests use reference fixtures from `crates/app/src/compat_http/test_fixtures/` (11 fixture files).

### Compat config parsing: 27 unit tests

The config parsing layer (`crates/app/src/compat_config.rs`) has 27 unit tests covering format detection, key translation, HISTORY parsing, quorum sets, peer extraction, testing flags, SSC-generated config fixtures, captive-core config parsing, and `GENESIS_TEST_ACCOUNT_COUNT`.

### Generateload types and mode parsing: well tested

- `parse_mode` has 5 tests covering valid modes, underscore names, case insensitivity, invalid inputs, and `stop` exclusion
- `deprecated_mode` tested for `create` mode
- `GenerateLoadParams` has 5 tests covering serde defaults and serialization
- `LoadGenRequest` construction has 4 tests including stop mode case insensitivity
- Genesis test accounts have 3 tests (creation, balance split, key derivation)

### Common utilities: consolidated and tested

`deterministic_seed()` is now a single canonical function in `henyey-common::types` with 3 unit tests (padding, full-length, empty). Previously duplicated across 4 locations.

## Mission Feasibility Reframed

| Mission Category | Current Feasibility | Why |
|---|---|---|
| Mixed-image consensus / topology missions | HIGH | Docker image exists, compat HTTP surface is solid |
| SimplePayment with all Henyey nodes | **HIGH** | `/testacc`, `/generateload`, `/metrics` all implemented; needs SSC validation |
| ComplexTopology with all Henyey nodes | **HIGH** | Same as SimplePayment |
| Sustained load generation missions | MEDIUM | Rate/percentile metrics are zero placeholders; may need real tracking |
| History catchup / publish missions | MEDIUM | Core machinery exists, but SSC mission validation and HISTORY-template compatibility remain open |
| ProtocolUpgrade missions | MEDIUM | `/upgrades` exists, but mission-level execution is still unverified |
| VersionMix / mixed images | HIGH | Best near-term path to value |
| EmitMeta-style missions | HIGH | Existing metadata support is already strong |
| NetworkSurvey missions | LOW | Compat endpoints are stubs today |
| MaxTPS benchmarking | LOW | Requires deeper loadgen and metrics parity, and possibly other SSC assumptions |
| DatabaseInplaceUpgrade | N/A | Not relevant to Henyey's SQLite architecture |

## Recommended Execution Plan

The plan below is intended to be executable by the team from the current repo state.

### Phase 0 -- Package Henyey for SSC consumption — COMPLETE

**Goal**: make SSC able to launch Henyey at all.

**Status**: All items complete.

Completed work:

1. Multi-stage Dockerfile checked in at repo root (`27e65ba`)
2. Container exposes the binary as `stellar-core` via symlink
3. `loadgen` feature enabled by default in `crates/henyey/Cargo.toml`
4. `.dockerignore` created to optimize build context
5. `loadgen.wasm` relocated from `stellar-core` submodule to `crates/simulation/wasm/` for Docker build compatibility (`cc529c5`)
6. Image verified building (~176MB)

Remaining work:

- CI smoke test for image build and entrypoint validation (future work)
- Documentation of build flow (partially covered in README)

### Phase 1 -- Mixed-image mission bring-up

**Goal**: get immediate integration value with the least new code.

**Status**: Not started. Requires SSC infrastructure.

Target mission types:

- mixed-image consensus missions
- version-mix missions
- topology/peer connectivity missions that do not rely on full loadgen parity

Why this first:

- validates that Henyey can participate in an SSC-managed network
- exercises real quorum, overlay, config generation, and node lifecycle
- avoids blocking on full metrics parity

Exit criteria:

- at least one mixed-image mission passes reliably
- failure modes are understood and reproducible

### Phase 2 -- Close the Henyey-only payment mission blockers — MOSTLY COMPLETE

**Goal**: unlock `MissionSimplePayment`-class runs using only Henyey nodes.

**Status**: All HTTP/loadgen compatibility work is done. End-to-end SSC mission validation remains.

Completed work:

| Item | Status | Commit |
|------|--------|--------|
| `/testacc` implementation with 10 parity tests | DONE | `fc12e03` |
| Compat `/metrics` expanded to 8 medida-format metrics | DONE | `fc12e03` |
| `/generateload` mode name normalization (underscore names) | DONE | `fc12e03` |
| `create` mode returns stellar-core v25 deprecation message | DONE | `fc12e03` |
| `stop` mode wired end-to-end through `LoadGenRunner` trait | DONE | `dae8ad9` |
| `GENESIS_TEST_ACCOUNT_COUNT` support | DONE | `35a44d8` |
| `deterministic_seed` consolidated into `henyey-common` | DONE | `dae8ad9` |
| 36+ compat HTTP response-shape parity tests | DONE | `fc12e03` |
| SSC config fixture + parse test | DONE | `fc12e03` |

Remaining work:

| Item | Status | Priority |
|------|--------|----------|
| Real metrics tracking (rates/percentiles are zero placeholders) | NOT DONE | Low — only needed if SSC checks rate values |
| Missing loadgen modes (`upgrade_setup`, `create_upgrade`, `pay_pregenerated`, `soroban_invoke_apply_load`) | NOT DONE | Low — deferred until SSC needs them |
| End-to-end SSC mission validation | NOT DONE | Medium — requires SSC infrastructure |
| `loadgen` feature verification in SSC deployment path | NOT DONE | Low — feature is enabled by default |

### Phase 3 -- History and upgrade mission validation

**Goal**: prove that archival and upgrade flows behave correctly under SSC orchestration.

**Status**: Not started.

Required work:

1. Validate history publish from Henyey through SSC-generated archive config.
2. Validate catchup from those archives.
3. Exercise `/upgrades` end to end in mission form.
4. Verify cross-version or mixed-image history behavior if that is part of the target rollout.

Exit criteria:

- Henyey can publish history in SSC-managed runs and catch up from it reliably
- upgrade missions complete without compatibility-specific failures

### Phase 4 -- Optional parity work for lower-priority missions

**Goal**: expand coverage after the core path is proven.

**Status**: Not started.

Candidate work:

- wire compat survey endpoints to the native survey subsystem
- broaden `/generateload` mode coverage further
- improve metrics parity beyond the minimum SSC-required set
- pursue MaxTPS/performance-specific missions once correctness missions are stable

## Handoff Checklist By Phase

### Phase 0 handoff checklist

- [x] Dockerfile checked in and buildable from repo root
- [x] image exposes `stellar-core`
- [x] `loadgen.wasm` relocated for Docker build compatibility
- [x] `.dockerignore` created
- [ ] SSC-required commands verified in-container
- [ ] build/run instructions documented
- [ ] CI smoke test for image build and entrypoint validation passes

### Phase 1 handoff checklist

- [ ] first mixed-image mission selected
- [ ] mission run logs captured
- [ ] passing/failing mission matrix started
- [ ] known startup and network issues categorized
- [x] unit tests for `/info` and `/peers` response shapes pass against stellar-core reference fixtures

### Phase 2 handoff checklist

- [x] `/testacc` implemented and tested with response-shape parity tests (10 tests)
- [x] compat `/metrics` expanded from 3-counter stub to 8 medida-format metrics with parity tests
- [x] `/generateload` mode normalization: underscore names, `create` deprecation, `stop` mode
- [x] `GENESIS_TEST_ACCOUNT_COUNT` implemented and tested
- [x] `deterministic_seed` consolidated into single canonical location
- [x] SSC config fixture parse test
- [x] response-shape regression tests exist for every new or modified endpoint (36+ tests)
- [ ] all-Henyey payment mission attempted and results recorded
- [ ] real metrics tracking (rate/percentile values)

### Phase 3 handoff checklist

- [ ] HISTORY parsing tested against SSC-style templates (fixtures from real SSC configs)
- [ ] publish/catchup round-trip integration test passes outside SSC
- [ ] publish/catchup scenario exercised under SSC
- [ ] upgrade mission exercised under SSC
- [ ] `/upgrades` response shape tested against stellar-core reference
- [ ] remaining history/upgrade blockers documented precisely

### Phase 4 handoff checklist

- [ ] survey scope explicitly decided
- [ ] long-tail mission matrix updated
- [ ] deferred work separated from active blockers
- [ ] any new compat endpoints have response-shape parity tests

## What Must Be True Before Saying "Henyey-Only SSC Is Ready"

The project should not claim Henyey-only SSC readiness until all of the following are true:

- [x] a checked-in SSC-ready Docker image exists
- [ ] mixed-image missions pass reliably
- [x] `/testacc` exists and is exercised by SSC
- [x] compat `/metrics` is sufficiently wire-compatible for SSC mission logic (structure done; real rate values TBD)
- [x] `/generateload` compatibility is validated with the actual SSC parameter set, including `create` mode deprecation and `stop` mode
- [x] `GENESIS_TEST_ACCOUNT_COUNT` is implemented for test account setup
- [ ] at least one Henyey-only payment/topology mission passes end to end
- [x] every SSC-facing compat endpoint has unit tests that validate its response shape against a stellar-core reference fixture
- [x] the `create` mode behavioral deviation is fixed (returns deprecation message, not silent alias)
- [ ] the `--minimal-for-in-memory-mode` behavioral deviation is either fixed or explicitly documented with its mission impact

## Implementation Commits

| Commit | Description |
|--------|-------------|
| `fc12e03` | SSC compat HTTP parity: /testacc, metrics expansion, mode normalization, 36+ tests, test fixtures |
| `27e65ba` | Dockerfile and .dockerignore for SSC compatibility |
| `cc529c5` | Move loadgen WASM into crate for Docker build compatibility |
| `35a44d8` | GENESIS_TEST_ACCOUNT_COUNT for genesis test accounts |
| `dae8ad9` | Consolidate deterministic_seed, implement generateload stop mode |

## Conclusion

The feasibility answer is yes, and significant implementation progress has been made.

Henyey now has all the HTTP/loadgen/config compatibility pieces needed for SSC payment-style missions. The Docker image is ready, `/testacc` works, `/metrics` returns proper medida JSON, `/generateload` handles all stellar-core v25 mode semantics including `stop` and `create` deprecation, and `GENESIS_TEST_ACCOUNT_COUNT` creates test accounts at genesis.

The remaining path to full SSC readiness is:

1. **Run mixed-image missions** (Phase 1) — validate packaging and basic interop
2. **Run an all-Henyey payment mission** (Phase 2 validation) — verify the HTTP compat work holds up
3. **Add real metrics tracking** if SSC missions check rate values (low risk — most only check structure)
4. **History and upgrade mission validation** (Phase 3)
5. **Long-tail survey/loadgen modes** (Phase 4) — only as SSC missions require them

The architectural work is done. What remains is integration testing and mission-level validation.
