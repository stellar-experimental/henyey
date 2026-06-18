# Supercluster Mission #3295 — History Publish/Catchup Mission

**Issue:** [#3295](https://github.com/stellar-experimental/henyey/issues/3295)
**Companion docs:** [`supercluster-henyey-mixed-mission.md`](./supercluster-henyey-mixed-mission.md) (the first mixed-image mission — pattern + harness source), [`supercluster-nsc-workflow.md`](./supercluster-nsc-workflow.md) (#3304 — the verified `nsc` build/publish/launch surface), [`supercluster-feasibility.md`](./supercluster-feasibility.md) (§6 history reliability).

This runbook drives an **SSC history mission**: a henyey node **publishes** its
own checkpoints to a history archive and **catches up** from an SSC-managed
archive, proving henyey's history publish/catchup interops with an SSC-generated
`[HISTORY.*]` archive configuration.

## What this mission validates (acceptance criteria)

| AC | Criterion | How it is checked | Autonomous / Operator |
|----|-----------|-------------------|------------------------|
| AC#1 | a real SSC-generated `stellar-core.cfg` with `[HISTORY.*]` is captured as a fixture | `crates/app/src/compat_http/test_fixtures/ssc_mission_history.cfg` (shape flagged for live confirmation) | **Autonomous** |
| AC#2 | henyey parses that config **without manual patching** | `test_ssc_mission_history_config_parse` + hardened `extract_url_from_curl_cmd` unit tests in `crates/app/src/compat_config.rs` | **Autonomous** |
| AC#3 | publish→catchup round-trip with SSC-style archive command templates | `crates/history/tests/publish_catchup_roundtrip.rs` — real publish → `cp`/`mkdir` command-template upload → `file://` catchup, asserting ledger-hash agreement | **Autonomous** |
| AC#4 | run an SSC history mission (or closest equivalent) | live `nsc` + `stellar/supercluster` dotnet harness | **Operator** |
| AC#5 | henyey-published archive files are readable by henyey **and, where practical, by stellar-core tools** | cross-tool read against a live mission archive | **Operator** |
| AC#6 | catchup reaches the **expected ledger hash in the live mission** | live mission observation | **Operator** |
| AC#7 | logs / config / image digest / exact invocation **captured** | run-dir layout + the artifact checklist below | **Operator** |
| AC#8 | any failure becomes a **concrete follow-up issue** | operator files a follow-up per the checklist | **Operator** |

## Why most of the henyey-side risk is already closed offline

Per the #3295 triage, henyey's history machinery is production-grade: catchup &
replay, verification, archive HTTP + shell-command access, the checkpoint
builder, and publish-queue persistence are all at **Full** parity
(`crates/history/PARITY_STATUS.md`). henyey performs publishing **natively in the
`history` crate** rather than via stellar-core's Work-class wrappers, so
`historywork`'s "Publish pipeline = None" row is a taxonomy gap, **not** a
functional one.

The one genuine henyey-side compat risk the feasibility doc §6 flagged was the
**heuristic HISTORY-template URL extraction** (`extract_url_from_curl_cmd`).
That is now hardened and pinned (AC#2), and the publish→catchup path is now
CI-pinned offline (AC#3). What remains is **live execution confidence** —
cross-tool archive readability and live catchup-to-hash — which needs a real
SSC RUN and is handed to the operator below.

## Archive-layout parity (what AC#5 cross-tool readability depends on)

The offline tests prove **henyey reads what henyey wrote**. True cross-tool
readability (stellar-core reading henyey's archive and vice versa) is the
operator's AC#5. The three load-bearing surfaces are at stellar-core v26.0.1
parity:

- **Root HAS path:** `.well-known/stellar-history.json` (`paths.rs`), matching
  stellar-core's well-known location.
- **Checkpoint/bucket path sharding:**
  `category/ll/ll/ll/<category>-<8hexledger>.<ext>` and
  `bucket/hh/hh/hh/bucket-<hash>.xdr.gz` (`paths.rs`).
- **HAS JSON shape:** `version` / `server` / `currentLedger` /
  `networkPassphrase` / `currentBuckets` / `hotArchiveBuckets`, version 2 for
  protocol 24+ (with hot archive), matching `HistoryArchive.cpp`.
- **Upload ordering:** data files → per-checkpoint HAS → root HAS **last**
  (`upload.rs`), matching stellar-core's "root HAS marks the archive
  initialized, must be last" rule.

## Autonomous deliverable vs. operator-executed run

This is a **validation mission**: the deliverable is *fixtures + tests + runbook
+ a follow-up-on-failure handoff*, not production code. The live multi-hour k8s
mission **cannot run inside one autonomous task** — it needs an interactive
`nsc login` (browser OAuth), a long-lived k8s cluster, and the external
`stellar/supercluster` dotnet harness (not vendored here).

**Shipped autonomously (in the PR):**

- `crates/app/src/compat_http/test_fixtures/ssc_mission_history.cfg` — the SSC publishing-validator config fixture (AC#1).
- `crates/app/src/compat_config.rs::test_ssc_mission_history_config_parse` + hardened `extract_url_from_curl_cmd` unit tests (AC#2).
- `crates/history/tests/publish_catchup_roundtrip.rs` — the offline publish→catchup round-trip (AC#3).
- doc updates to `crates/history/PARITY_STATUS.md` and `docs/supercluster-feasibility.md` §6 (AC#8 docs).
- this runbook.

**Operator-executed (NOT done in the PR):** the mission RUN itself (AC#4 / AC#5
cross-tool / AC#6) — see the checklist below.

## OPERATOR CHECKLIST — the live history mission RUN

> Everything below is **operator-executed** against live infra. The PR does NOT
> run any of it and does NOT fabricate a mission transcript.

1. **Auth** (one-time per session): `nsc login` (interactive browser OAuth).
   Review the verified build/publish/launch commands in
   [`supercluster-nsc-workflow.md`](./supercluster-nsc-workflow.md) (#3304).
2. **Build + push + provision (nsc side):** build and push the henyey image,
   capture the **`sha256` digest**, and `nsc create` an ephemeral k8s instance.
   (Reuse the `scripts/ssc/launch-henyey-mixed-mission.sh` wrapper if launching a mixed
   topology; a history mission additionally needs a **publishing** node, i.e.
   `NODE_IS_VALIDATOR=true` with a writable `[HISTORY.<name>]` archive — see the
   fixture shape.)
3. **Drive the SSC dotnet harness** (`stellar/supercluster`, external) pointed at
   the **digest-pinned** image + the instance kubeconfig (`nsc kubeconfig write`).
   Configure at least one henyey publishing node and a shared/cluster-local
   history archive (e.g. a mounted volume the pods `cp` into, or an in-cluster
   HTTP archive served by a sidecar). Let it externalize past **at least one
   checkpoint boundary** (every 64 ledgers) so a publish actually occurs.
4. **AC#2 (live) — confirm the fixture shape:** capture the **SSC-generated
   `stellar-core.cfg`** and `diff` its `[HISTORY.*]` `get`/`put`/`mkdir`
   templates against
   `crates/app/src/compat_http/test_fixtures/ssc_mission_history.cfg`. The
   fixture's `put`/`mkdir`/mount-path strings are inferred from stellar-core
   conventions; **a structural skew is a follow-up issue (AC#8), not a silent
   patch.** Confirm henyey's binary `load_config` accepts the real config
   unmodified.
5. **AC#6 (live) — catchup to hash:** point a fresh henyey node at the mission
   archive and run `henyey catchup` to a published checkpoint. Assert the
   reported ledger hash matches the network's ledger hash at that seq:
   ```bash
   # On the catching-up henyey node (file://, http(s)://, or cmd archive):
   henyey catchup --archive <archive-url> --to <checkpoint-ledger>
   # Compare against the network's seq+hash:
   curl -s http://<publishing-pod>:11626/info | jq '.info.ledger'
   ```
6. **AC#5 (cross-tool readability):**
   - **henyey→henyey** is already proven offline (AC#3). Live, confirm a second
     henyey node catches up from the **henyey-published** archive (step 5).
   - **henyey→stellar-core:** point a stellar-core node (or `stellar-core
     catchup` / its archive reader) at the **henyey-published** archive and
     confirm it reads the root HAS, the per-checkpoint HAS, the headers, and the
     buckets without error. Byte-check a henyey-published
     `.well-known/stellar-history.json` against stellar-core's expectations.
   - **stellar-core→henyey:** point henyey at a **stellar-core-published**
     mission archive and catch up (step 5 against that archive).
   - Any read failure or HAS-shape divergence → **follow-up issue (AC#8)**.
7. **Capture artifacts** into `runs/<date>-mission-3295/` and onto #3295:
   - `image-digest.txt` (the immutable `sha256:` reference),
   - `instance.json` (nsc instance metadata),
   - the **SSC-generated `stellar-core.cfg`** (and the diff vs. the fixture),
   - the exact `nsc` + dotnet invocations,
   - a sample of the **henyey-published archive tree** (root HAS + one
     checkpoint's HAS/headers/buckets),
   - the catchup-to-hash output (henyey and, where run, stellar-core),
   - `nsc kubectl get all -A` + per-pod henyey/stellar-core logs.
8. **Teardown:** `nsc destroy <instance-id> --force`, then `nsc list -o json` to
   confirm the ephemeral cluster is gone (see #3304 §5).
9. **On any failure:** file a **concrete follow-up issue** (AC#8) with the
   captured artifacts — do NOT silently patch henyey or the fixture to make the
   run pass.

### Watch-item (#3299)

`--minimal-for-in-memory-mode` is currently accepted-and-ignored by henyey
(#3299). henyey runs persistent SQLite, which is fine for one bounded mission.
Watch for subtle state issues across mission restarts; if the SSC default node
bootstrap relies on ephemeral in-memory semantics, file a follow-up rather than
blocking the mission.
