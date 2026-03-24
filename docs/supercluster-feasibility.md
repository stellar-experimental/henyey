# Stellar Supercluster Feasibility Evaluation

**Date**: 2026-03-24
**Goal**: Determine whether Stellar Supercluster (SSC) can be used to run full-network simulations against Henyey, and define an execution plan that can actually be carried out from the current codebase.

## Executive Assessment

Using SSC with Henyey is feasible, but the path is staged rather than immediate.

The good news is that the architectural seam is correct. SSC treats `stellar-core` as a black-box process with a Docker image, CLI entrypoints, a `stellar-core.cfg` file, and the admin HTTP interface on port 11626. Henyey already implements most of those seams: `new-db`, `new-hist`, `catchup`, `run`, `force-scp`, config parsing, the compat HTTP server, SCP, overlay, transaction execution, and history machinery are all present.

The limiting factor is not consensus or networking. The limiting factor is test-harness parity. The missing pieces are the exact pieces SSC leans on to drive missions: a checked-in SSC-ready Docker image, the `/testacc` endpoint, a much more complete stellar-core-compatible `/metrics` response, and tighter `/generateload` compatibility.

This means there is a clear path to execution, but it should be framed as:

1. Package Henyey as an SSC-consumable image.
2. Prove mixed-image missions first.
3. Close the HTTP/loadgen/metrics gaps required for Henyey-only missions.
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

### Compat HTTP surface is mostly present

The stellar-core-compatible router is implemented in `crates/app/src/compat_http/mod.rs:45`.

Verified routes include:

- `/info`
- `/tx`
- `/peers`
- `/metrics`
- `/sorobaninfo`
- `/manualclose`
- `/clearmetrics`
- `/quorum`
- `/scp`
- `/upgrades`
- survey-shaped endpoints such as `/getsurveyresult` and `/startsurveycollecting`
- `/generateload` when built with the `loadgen` feature

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

## Gaps That Actually Block Execution

### 1. No checked-in SSC-ready Docker image

This is the first operational blocker.

Henyey currently documents how to build a quickstart-compatible image by symlinking `henyey` to `stellar-core` in `README.md:266`, but there is no checked-in Dockerfile or image target dedicated to SSC workflows.

What is needed:

- a maintained Dockerfile in the repo
- a repeatable build command
- the binary available at the path SSC expects (`stellar-core`)
- a documented feature/build profile for SSC runs

Until this exists, the plan is not executable by another engineer without reconstructing packaging details.

### 2. `/testacc` does not exist

SSC uses `/testacc?name=<account>` during load-generation-style missions to inspect balances and sequence numbers for named test accounts.

That route is not present in `crates/app/src/compat_http/mod.rs:45`, and no implementation was found in the repo.

Impact:

- blocks payment/load-style Henyey-only missions
- should be treated as a required compatibility feature, not a nice-to-have

### 3. Compat `/metrics` is currently a stub, not SSC-compatible

This is the most under-described gap in the original document.

The compat handler in `crates/app/src/compat_http/handlers/metrics.rs:1` explicitly says full medida JSON conversion is a future enhancement. Today it only returns three counters:

- `ledger.ledger.close`
- `peer.peer.count`
- `herder.pending.transactions`

SSC missions inspect specific stellar-core metric names and expect a broader wire-compatible structure. The current implementation is adequate for lightweight health checks, but not for SSC parity.

Impact:

- many missions cannot be trusted even if they start
- mission pass/fail logic may be wrong or incomplete
- this should be treated as a primary execution blocker for Henyey-only missions

### 4. `/generateload` exists, but only partially matches SSC expectations

There are three separate realities here.

**Build-time requirement**

`/generateload` is only present when compiled with the `loadgen` feature in `crates/app/src/compat_http/mod.rs:111`.

**Runtime requirement**

The endpoint also requires:

- `ARTIFICIALLY_GENERATE_LOAD_FOR_TESTING=true` in config, enforced in `crates/app/src/compat_http/handlers/plaintext.rs:420`
- a `LoadGenRunner` to be injected when the compat server starts, wired in `crates/app/src/run_cmd.rs:236`

**Mode compatibility requirement**

The accepted parameter set in `crates/app/src/http/types/generateload.rs:10` and `crates/henyey/src/main.rs:120` supports these effective modes:

- `create`
- `pay`
- `sorobanupload`
- `sorobaninvokesetup`
- `sorobaninvoke`
- `mixed` / `mixedclassicsoroban`

The original feasibility document listed SSC-style names such as:

- `soroban_upload`
- `soroban_invoke`
- `mixed_classic_soroban`
- `upgrade_setup`
- `create_upgrade`
- `stop`
- `pay_pregenerated`

Those names and behaviors are not currently evidenced in the codebase.

Impact:

- the endpoint exists, but mission compatibility should be treated as partial and unverified
- mode-name normalization may be required even before deeper behavioral work

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

### 7. SSC may expect additional CLI surface beyond the currently documented path

The original document mentions SSC container commands including `test`. I did not find a Henyey `test` subcommand in `crates/henyey/src/main.rs:343`.

This may or may not matter depending on which missions are targeted, but it should be treated as an explicit verification item instead of left implicit.

## Mission Feasibility Reframed

The original feasibility levels were directionally useful, but they should be tightened around what is actually blocked.

| Mission Category | Current Feasibility | Why |
|---|---|---|
| Mixed-image consensus / topology missions | HIGH | Requires Docker packaging and mission validation more than new protocol work |
| SimplePayment with all Henyey nodes | MEDIUM | Blocked on `/testacc`, compat `/metrics`, and `/generateload` parity |
| ComplexTopology with all Henyey nodes | MEDIUM | Same blockers as SimplePayment |
| Sustained load generation missions | LOW-MEDIUM | Same as above, plus stronger behavioral parity requirements |
| History catchup / publish missions | MEDIUM | Core machinery exists, but SSC mission validation and HISTORY-template compatibility remain open |
| ProtocolUpgrade missions | MEDIUM | `/upgrades` exists, but mission-level execution is still unverified |
| VersionMix / mixed images | HIGH | Best near-term path to value |
| EmitMeta-style missions | HIGH | Existing metadata support is already strong |
| NetworkSurvey missions | LOW | Compat endpoints are stubs today |
| MaxTPS benchmarking | LOW | Requires deeper loadgen and metrics parity, and possibly other SSC assumptions |
| DatabaseInplaceUpgrade | N/A | Not relevant to Henyey's SQLite architecture |

## Recommended Execution Plan

The plan below is intended to be executable by the team from the current repo state.

### Phase 0 -- Package Henyey for SSC consumption

**Goal**: make SSC able to launch Henyey at all.

Required work:

1. Check in an SSC-ready Dockerfile.
2. Ensure the container exposes the binary as `stellar-core`.
3. Document the exact build flags/features required for SSC, especially whether `loadgen` is enabled.
4. Verify the container can run the required entrypoints: `new-db`, `new-hist`, `catchup`, `run`, and any additional command SSC actually invokes.

Exit criteria:

- another engineer can build the image from the repo without tribal knowledge
- SSC can start a Henyey container and invoke the required commands successfully

Concrete engineering tasks:

1. Add a checked-in Dockerfile for SSC use.
2. Add any helper script or Make/cargo target needed to build the image reproducibly.
3. Ensure the image places the Henyey binary at `stellar-core` or provides a symlink with that exact name.
4. Document the image build flow in the repo docs, including required Rust target and feature flags.
5. Verify the container can execute `new-db`, `new-hist`, `catchup`, `run`, `force-scp`, `version`, and `http-command`.
6. Confirm whether SSC invokes any additional entrypoints such as `test`; if yes, either implement the missing command or explicitly constrain the supported mission set.
7. Smoke-test the image locally by launching the container with a generated compat config and hitting `/info` on port 11626.
8. Capture a short troubleshooting section for image/build failures so the next engineer does not need to rediscover packaging details.

Suggested deliverables:

- checked-in Dockerfile and build instructions
- one short runbook section for local image verification
- a verified list of supported SSC entrypoint commands

### Phase 1 -- Mixed-image mission bring-up

**Goal**: get immediate integration value with the least new code.

Target mission types:

- mixed-image consensus missions
- version-mix missions
- topology/peer connectivity missions that do not rely on full loadgen parity

Why this first:

- validates that Henyey can participate in an SSC-managed network
- exercises real quorum, overlay, config generation, and node lifecycle
- avoids blocking on `/testacc` and full metrics parity

Exit criteria:

- at least one mixed-image mission passes reliably
- failure modes are understood and reproducible

Concrete engineering tasks:

1. Stand up SSC against the new Henyey image without changing mission code yet.
2. Select one minimal mixed-image mission as the first target and document why it was chosen.
3. Run the mission with one or more Henyey nodes replacing stellar-core nodes.
4. Record all startup failures separately by category: image/entrypoint, config parsing, peer connectivity, consensus, HTTP compatibility, and mission harness assumptions.
5. Fix any packaging or config-compat issues discovered in the first run before widening mission scope.
6. Verify that Henyey nodes join quorum, connect to peers, externalize ledgers, and remain queryable through `/info`, `/peers`, `/scp`, and `/quorum`.
7. Add a mission matrix to the document or a sibling handoff note listing which mixed-image missions were attempted, passed, failed, or remain untried.
8. Once the first mission passes, expand to one topology-oriented mixed-image mission and one version-mix mission.

Suggested deliverables:

- one passing mixed-image mission with notes
- a failure log for any blocked mixed-image missions
- a short compatibility matrix for mixed-image coverage

### Phase 2 -- Close the Henyey-only payment mission blockers

**Goal**: unlock `MissionSimplePayment`-class runs using only Henyey nodes.

Required work:

1. Implement `/testacc` on the compat server.
2. Expand compat `/metrics` to a real stellar-core-compatible shape with the metric names SSC actually reads.
3. Normalize `/generateload` mode names and parameters to SSC expectations.
4. Verify `loadgen` feature, config gate, and runner injection are all enabled in the intended deployment path.
5. Run end-to-end mission validation against SSC, not just unit-level tests.

Exit criteria:

- simple payment mission passes end to end on all-Henyey topology
- SSC pass/fail logic is driven by real compatible metrics, not stubs

Concrete engineering tasks:

1. Implement `/testacc` on the compat server with the response shape SSC expects.
2. Identify exactly which ledger/account fields SSC reads from `/testacc`, then add unit tests that match those semantics.
3. Replace the compat `/metrics` stub with a broader stellar-core-compatible JSON structure.
4. Audit SSC mission code to enumerate the exact metric names it reads for payment/topology missions.
5. Implement those metric names first, then add tests that validate the compat JSON payload shape and counters.
6. Audit SSC `generateload` requests and build a translation table between SSC mode names and Henyey-supported mode names.
7. Decide whether to normalize SSC aliases at the HTTP layer or expand the internal mode parser to accept both spellings.
8. Verify the `loadgen` feature is enabled in the intended build path and that the compat server always receives a `LoadGenRunner` in that configuration.
9. Add end-to-end tests or scripted local checks for `/generateload` covering enabled, disabled, missing-runner, and invalid-mode cases.
10. Run an all-Henyey simple payment mission in SSC and capture any residual incompatibilities.
11. Repeat until the mission passes without relying on stubbed metrics or manual intervention.

Suggested deliverables:

- `/testacc` implementation with tests
- expanded compat `/metrics` with SSC-targeted coverage
- documented `/generateload` compatibility table
- one passing all-Henyey simple payment mission

### Phase 3 -- History and upgrade mission validation

**Goal**: prove that archival and upgrade flows behave correctly under SSC orchestration.

Required work:

1. Validate history publish from Henyey through SSC-generated archive config.
2. Validate catchup from those archives.
3. Exercise `/upgrades` end to end in mission form.
4. Verify cross-version or mixed-image history behavior if that is part of the target rollout.

Exit criteria:

- Henyey can publish history in SSC-managed runs and catch up from it reliably
- upgrade missions complete without compatibility-specific failures

Concrete engineering tasks:

1. Inspect the exact HISTORY command templates SSC generates and compare them to what `crates/app/src/compat_config.rs` successfully parses today.
2. Add focused tests for HISTORY parsing, especially `get`, `put`, and `mkdir` command forms that SSC emits.
3. Run a small SSC mission where Henyey publishes to a local or ephemeral archive and another Henyey node catches up from it.
4. Capture failure modes separately for archive initialization, publish, checkpoint visibility, and catchup verification.
5. Verify `new-hist` works in the same container/image flow used by SSC rather than only in ad hoc local runs.
6. Run a protocol-upgrade-oriented mission and validate that upgrades scheduled via `/upgrades` are reflected in ledger progress and mission success criteria.
7. If mixed-image history flows are in scope, test both directions: Henyey publishing for stellar-core catchup and stellar-core publishing for Henyey catchup.
8. Document any unsupported archive command variants or mission assumptions that require either code changes or explicit non-support.

Suggested deliverables:

- HISTORY parsing tests for SSC-generated templates
- one passing publish/catchup SSC scenario
- one passing upgrade-oriented SSC scenario or a precise blocker report

### Phase 4 -- Optional parity work for lower-priority missions

**Goal**: expand coverage after the core path is proven.

Candidate work:

- wire compat survey endpoints to the native survey subsystem
- broaden `/generateload` mode coverage further
- improve metrics parity beyond the minimum SSC-required set
- pursue MaxTPS/performance-specific missions once correctness missions are stable

Exit criteria:

- lower-priority SSC mission categories can be attempted intentionally rather than experimentally

Concrete engineering tasks:

1. Replace compat survey endpoint stubs with adapters to the native survey implementation.
2. Validate the compat survey response formats against what SSC expects before attempting survey missions.
3. Expand `/generateload` compatibility for any still-missing SSC modes that matter after Phase 2.
4. Broaden compat `/metrics` coverage beyond the minimum mission-critical set so additional SSC missions can run without ad hoc metric work.
5. Evaluate whether MaxTPS missions require additional backend or build assumptions beyond current Henyey support, and document those explicitly.
6. Create a backlog table of lower-priority SSC missions with status, blockers, and whether each blocker is protocol, packaging, metrics, loadgen, or survey related.

Suggested deliverables:

- wired survey compat endpoints if survey missions are in scope
- expanded mission/blocker matrix for long-tail SSC coverage
- a clear defer/not-planned list for missions that remain out of scope

## Handoff Checklist By Phase

This section is intended for the next engineer picking up the work.

### Phase 0 handoff checklist

- Dockerfile checked in and buildable from repo root
- image exposes `stellar-core`
- SSC-required commands verified in-container
- build/run instructions documented

### Phase 1 handoff checklist

- first mixed-image mission selected
- mission run logs captured
- passing/failing mission matrix started
- known startup and network issues categorized

### Phase 2 handoff checklist

- `/testacc` implemented and tested
- compat `/metrics` no longer a 3-counter stub
- `/generateload` alias/mode behavior documented and tested
- all-Henyey payment mission attempted and results recorded

### Phase 3 handoff checklist

- HISTORY parsing tested against SSC-style templates
- publish/catchup scenario exercised under SSC
- upgrade mission exercised under SSC
- remaining history/upgrade blockers documented precisely

### Phase 4 handoff checklist

- survey scope explicitly decided
- long-tail mission matrix updated
- deferred work separated from active blockers

## What Must Be True Before Saying "Henyey-Only SSC Is Ready"

The project should not claim Henyey-only SSC readiness until all of the following are true:

- a checked-in SSC-ready Docker image exists
- mixed-image missions pass reliably
- `/testacc` exists and is exercised by SSC
- compat `/metrics` is sufficiently wire-compatible for SSC mission logic
- `/generateload` compatibility is validated with the actual SSC parameter set
- at least one Henyey-only payment/topology mission passes end to end

Anything short of that is promising groundwork, not execution readiness.

## Conclusion

The feasibility answer is yes, but the execution answer is phased.

Henyey is already close enough to stellar-core at the node boundary that SSC integration is realistic. The repo contains the hard architectural pieces: consensus, overlay, config compatibility, catchup, history publishing, and a stellar-core-shaped admin interface. That is the reason this effort is worth doing.

However, the shortest path is not to jump directly into all-Henyey SSC missions. The shortest path is:

1. package Henyey as an SSC-native image,
2. pass mixed-image missions,
3. implement the remaining harness-facing compatibility gaps,
4. then promote to Henyey-only missions.

That path is concrete, incremental, and executable from the current codebase.
