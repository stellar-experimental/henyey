# Supercluster Mission #3292 — First Mixed-Image Network

**Issue:** [#3292](https://github.com/stellar-experimental/henyey/issues/3292)
**Companion docs:** [`supercluster-nsc-workflow.md`](./supercluster-nsc-workflow.md) (#3304 — the verified `nsc` build/publish/launch surface), [`supercluster-feasibility.md`](./supercluster-feasibility.md).

This runbook drives the **first mixed-image Supercluster (SSC) mission**: a small
network of **3 stellar-core validators + 1 henyey validator**, with a
**stellar-core-majority quorum**, that exercises the henyey↔stellar-core interop
boundary end-to-end.

## What this mission validates (acceptance criteria)

| AC | Criterion | How it is checked |
|----|-----------|-------------------|
| AC#1 | henyey image launches under the `stellar-core` entrypoint | Dockerfile `henyey`→`stellar-core` symlink (already merged); the mission pods come up |
| AC#2 | SSC-generated `stellar-core.cfg` accepted by henyey **without manual patching** | Offline regression test `test_ssc_mission_mixed_config_parse` over `crates/app/src/compat_http/test_fixtures/ssc_mission_mixed.cfg`; closed fully only by the live run (see residual below) |
| AC#3 | henyey establishes **authenticated overlay** connections with stellar-core peers | `assert-mission-3292.sh` — `peer.peer.authenticated-count > 0` |
| AC#4 | the mixed network **externalizes ledgers** over a window | `assert-mission-3292.sh` — henyey ledger seq advances |
| AC#5 | henyey **agrees with stellar-core on latest ledger seq+hash** | `assert-mission-3292.sh` — `(ledger num, ledger hash)` match at a common seq |
| AC#6 | henyey stays **`Synced!`** | `assert-mission-3292.sh` — `/info.state == "Synced!"` |
| AC#7 | logs / config / image digest / exact invocation **captured** | run-dir layout + this runbook's artifact checklist |
| AC#8 | any failure becomes a **concrete follow-up issue** | operator files a follow-up per the checklist |

**Scope (first mission, deliberately minimal):** EXCLUDES history (#3295),
survey (#3298), loadgen/MaxTPS (#3297), protocol upgrades (#3300), and topology
admin (#3294). It needs none of the richer compat endpoints (#3296 real metric
rate/percentile values) — AC#5/#6 read ledger seq/hash + the real
`peer.peer.authenticated-count` counter, not rate/percentile values.

## Topology / mixed-cluster config

The henyey node runs as a **minority validator** in a stellar-core-majority
quorum, so it predominantly *follows* externalized values — the EXTERNALIZE
receipt path, which is at **Full** parity (`crates/herder/PARITY_STATUS.md`).
Overlay auth/handshake is also **Full** (`crates/overlay/PARITY_STATUS.md`).

The mission-shaped config is fixtured at
`crates/app/src/compat_http/test_fixtures/ssc_mission_mixed.cfg`:

- `NODE_IS_VALIDATOR = true`, `NODE_SEED = "… self"` — henyey is its own validator.
- One shared `[[HOME_DOMAINS]]` at `QUALITY = "MEDIUM"` — MEDIUM/LOW quality
  validators do **not** require a HISTORY archive (stellar-core
  `Config.cpp:744–752`), so the mission is genuinely history-free *and*
  stellar-core-acceptable.
- `[[VALIDATORS]]` lists the **3 stellar-core peers only**. henyey does **not**
  list itself — stellar-core's `addSelfToValidators` (`Config.cpp:869–898`)
  appends the local node's key automatically (and never dedups), and henyey
  mirrors this. The full 4-node quorum = 3 listed + auto-added self.
- `UNSAFE_QUORUM = true` — **required**: `Config.cpp:918–923` rejects a small
  4-node quorum otherwise.
- `ARTIFICIALLY_ACCELERATE_TIME_FOR_TESTING = true` for a bounded run.
- in-cluster pod DNS hostnames (`*.svc.cluster.local`), NOT real testnet peers.

> **AC#2 residual (operator step):** the offline test proves henyey's *translator*
> accepts a faithfully-authored mission config. It does **not** prove the binary's
> full `load_config` path accepts the **first real SSC-emitted** config. During
> the live run, diff the SSC-generated `stellar-core.cfg` against this fixture; a
> structural skew (e.g. SSC renders a flat `KNOWN_PEERS` of pod IPs rather than
> `[[VALIDATORS]].ADDRESS`) should spawn a **follow-up issue**, not a silent patch.

## Autonomous deliverable vs. operator-executed run

This is a **validation mission**: the deliverable is *fixtures + tooling +
runbook + a follow-up-on-failure handoff*, not production code. The live
multi-hour k8s mission **cannot run inside one autonomous task** — it needs an
interactive `nsc login` (browser OAuth), a long-lived k8s cluster, and the
external `stellar/supercluster` dotnet harness (not vendored here).

**Shipped autonomously (in the PR):**

- `crates/app/src/compat_http/test_fixtures/ssc_mission_mixed.cfg` — the durable, CI-enforced mission config fixture.
- `crates/app/src/compat_config.rs::test_ssc_mission_mixed_config_parse` — the config-acceptance regression test (AC#2).
- `scripts/ssc/launch-mission-3292.sh` — thin wrapper over #3304's verified `nsc` commands; `--dry-run` for CI.
- `scripts/ssc/assert-mission-3292.sh` — encodes AC#3–#6 as a runnable check; `--self-check` for CI.
- this runbook.

**Operator-executed (NOT done in the PR):** the mission RUN itself — see the
checklist below.

## OPERATOR CHECKLIST — the live mission RUN

> Everything below is **operator-executed** against live infra. The PR does NOT
> run any of it and does NOT fabricate a mission transcript.

1. **Auth** (one-time per session): `nsc login` (interactive browser OAuth), then
   `scripts/ssc/launch-mission-3292.sh --dry-run` to review the commands.
2. **Build + push + provision (nsc side):** run `scripts/ssc/launch-mission-3292.sh`
   (no `--dry-run`). It runs the smoke check, builds+pushes the henyey image,
   captures the **`sha256` digest**, `nsc create`s an ephemeral k8s instance,
   writes `runs/<date>-mission-3292/`, and prints the exact SSC dotnet invocation.
3. **Drive the SSC dotnet harness** (`stellar/supercluster`, external) pointed at
   the **digest-pinned** image + the instance kubeconfig (`nsc kubeconfig write`).
   Configure the mixed topology (3 stellar-core + 1 henyey, core-majority quorum).
4. **Validate** with `scripts/ssc/assert-mission-3292.sh`:
   ```bash
   scripts/ssc/assert-mission-3292.sh \
     --henyey-info    http://<henyey-pod>:11626/info \
     --core-info      http://<core-pod>:11626/info \
     --henyey-metrics http://<henyey-pod>:11626/metrics \
     --window-secs 60
   ```
   This checks AC#3 (authenticated peers), AC#4 (ledger advances), AC#5 (seq+hash
   agreement), AC#6 (Synced!).
5. **Capture artifacts** into `runs/<date>-mission-3292/` and onto #3292:
   - `image-digest.txt` (the immutable `sha256:` reference),
   - `instance.json` (nsc instance metadata),
   - the **SSC-generated `stellar-core.cfg`** (and diff vs. the fixture — see AC#2 residual),
   - the exact `nsc` + dotnet invocations,
   - `nsc kubectl get all -A` + per-pod henyey/stellar-core logs,
   - the `assert-mission-3292.sh` output.
6. **Teardown:** `nsc destroy <instance-id> --force`, then `nsc list -o json` to
   confirm the ephemeral cluster is gone (see #3304 §5).
7. **On any failure:** file a **concrete follow-up issue** (AC#8) with the captured
   artifacts — do NOT silently patch henyey or the fixture to make the run pass.

### Watch-item (#3299)

`--minimal-for-in-memory-mode` is currently accepted-and-ignored by henyey
(#3299). henyey runs persistent SQLite, which is fine for one bounded mission.
Watch for subtle state issues across mission restarts; if the SSC default node
bootstrap relies on ephemeral in-memory semantics, file a follow-up rather than
blocking the mission.
