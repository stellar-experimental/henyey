# Supercluster Mission #3300 — Protocol-Upgrade Mission

**Issue:** [#3300](https://github.com/stellar-experimental/henyey/issues/3300)
**Companion docs:** [`supercluster-nsc-workflow.md`](./supercluster-nsc-workflow.md) (the verified `nsc` build/publish/launch surface), [`supercluster-henyey-mixed-mission.md`](./supercluster-henyey-mixed-mission.md) (Henyey mixed-image mission this builds on), [`supercluster-feasibility.md`](./supercluster-feasibility.md).

This runbook drives a **live multi-node SSC protocol-upgrade mission**: a mixed
network (stellar-core validators + at least one henyey node) where an operator
drives the stellar-core-compatible `/upgrades?mode=set` admin endpoint and
observes the full **schedule → nominate → externalize → apply** path land an
upgrade at the upgrade ledger, in agreement across the mixed cluster.

It is the protocol-upgrade counterpart to the history mission. Like the Henyey mixed-image mission, the upgrade
machinery is already **Full** parity end-to-end; the deliverable is
*mission-definition + offline regression artifacts + parity-doc updates + an
operator runbook*, not new production code.

## What this mission validates (acceptance criteria)

| AC | Criterion | How it is checked |
|----|-----------|-------------------|
| AC#1 | henyey accepts `/upgrades?mode=set` with the exact SSC parameter set | Offline regression test `test_upgrades_set_parses_full_ssc_param_set` (param set pinned 1:1 to stellar-core `CommandHandler.cpp:613-671`); live: the SSC harness `mode=set` request returns `status: ok` |
| AC#2 | each scheduled `LedgerUpgrade` variant is **nominated** when its value differs and **suppressed** as a no-op when equal | Offline `test_scheduled_params_nominate_each_variant` + `test_scheduled_config_variant_nominate`; live: the upgrade appears in `scp`/nomination and externalizes |
| AC#3 | the upgrade is **applied at the upgrade ledger** and reflected in the header/state | Offline `test_apply_each_ledger_upgrade_variant_mutates_header_and_state` + the soroban/config variant tests; live: `/info` / ledger header before vs. after the upgrade ledger |
| AC#4 | henyey and stellar-core **agree on the post-upgrade ledger** (seq+hash, and the upgraded field) | live: compare `/info` ledger num/hash and the upgraded field (e.g. `protocol_version`, `base_fee`) across the mixed cluster at a common seq |
| AC#5 | henyey stays **`Synced!`** across the upgrade | live: `/info.state == "Synced!"` before, during, and after |
| AC#6 | logs / config / image digest / exact `/upgrades?mode=set` invocation + before/after headers **captured** | run-dir layout + this runbook's artifact checklist |
| AC#7 | any param/parity gap the live run surfaces becomes a **concrete follow-up issue** | operator files a follow-up per the checklist; never a silent patch |

**Scope:** a protocol-upgrade mission over the Henyey mixed-image network. It does
NOT add new upgrade machinery — all three layers (admin endpoint parse, herder
nominate, ledger apply) are already implemented for every `LedgerUpgrade`
variant.

## The upgrade path being exercised (already Full)

| Layer | Location | Status |
|-------|----------|--------|
| Admin endpoint parse (`/upgrades?mode=set`) | `crates/app/src/compat_http/handlers/plaintext.rs` (`compat_upgrades_handler`) | Full — param set matches `CommandHandler.cpp:613-671` |
| Herder schedule → nominate | `crates/herder/src/upgrades.rs` (`create_upgrades_for`, `is_valid_for_nomination`) | Full |
| Ledger apply | `crates/ledger/src/close.rs` (`apply_to_header`, `apply_config_upgrades`, `apply_max_soroban_tx_set_size`) dispatched from `crates/ledger/src/manager.rs` (`apply_upgrades_to_delta`) | Full |
| Externalize (SCP) | `crates/herder` EXTERNALIZE receipt path | Full |

The `mode=set` parameters the SSC harness can drive, each mapped to a
`LedgerUpgrade` variant at apply time:

| `/upgrades?mode=set` param | `UpgradeParameters` field | `LedgerUpgrade` variant | Applied to |
|---|---|---|---|
| `upgradetime` | `upgrade_time` | (timing gate, all variants) | — |
| `protocolversion` | `protocol_version` | `Version` | `header.ledger_version` |
| `basefee` | `base_fee` | `BaseFee` | `header.base_fee` |
| `basereserve` | `base_reserve` | `BaseReserve` | `header.base_reserve` |
| `maxtxsetsize` | `max_tx_set_size` | `MaxTxSetSize` | `header.max_tx_set_size` |
| `flags` | `flags` | `Flags` | `header.ext` V1 `flags` |
| `configupgradesetkey` (base64 XDR) | `config_upgrade_set_key` | `Config` | config-setting state (delta) |
| `maxsorobantxsetsize` | `max_soroban_tx_set_size` | `MaxSorobanTxSetSize` | `ContractExecutionLanes.ledger_max_tx_count` (delta) |
| `nominationtimeoutlimit` | `nomination_timeout_limit` | (nomination timeout tuning) | — |
| `expirationminutes` | `expiration_minutes` | (proposal expiry) | — |

## Known read/validation divergences (documented, NOT blocking)

Both are on read/validation-feedback paths the mission's `mode=set` →
observe-externalize flow does NOT require. They are recorded in the relevant
`PARITY_STATUS.md` files and are **candidate follow-ups** only if the live run
exercises them:

1. **Empty-mode read shape.** stellar-core returns `"mode required"` on empty
   mode and serializes `getUpgradesJson()` (`{time,version,fee,maxtxsize,reserve}`)
   only under `mode=get`. Henyey returns a bespoke `{current,scheduled}` body on
   the empty/default mode. If the live mission reads upgrade state via
   `/upgrades?mode=get`, file a follow-up.
2. **SET-time `configupgradesetkey` validation.** stellar-core validates the key
   via `ConfigUpgradeSetFrame::makeFromKey` + `isValidForApply` at SET time and
   rejects an invalid key with `"Error setting configUpgradeSet"`
   (`CommandHandler.cpp:648-655`). Henyey decodes the key but **defers** validity
   to nomination/apply (an invalid key simply never nominates). Likely benign,
   but loud-vs-silent rejection differs. If the live mission relies on SET-time
   rejection feedback, file a follow-up.

## Autonomous deliverable vs. operator-executed run

This is a **validation mission**: the autonomous deliverable is *offline
regression tests + parity docs + this runbook + a follow-up-on-failure handoff*,
not production code. The live multi-hour k8s mission **cannot run inside one
autonomous task** — it needs an interactive `nsc login` (browser OAuth), a
long-lived k8s cluster, and the external `stellar/supercluster` dotnet harness
(not vendored here).

**Shipped autonomously (in the PR):**

- `crates/app/src/compat_http/handlers/plaintext.rs` — `/upgrades?mode=set` full
  SSC param-set pinning (10 params + base64 `configupgradesetkey`), `mode=clear`
  reset, ISO 8601 `upgradetime`, default-GET shape.
- `crates/ledger/tests/upgrade_sequence_integration.rs` — per-variant
  application driven through the real close-loop dispatcher seam, asserting
  header/state mutation for every `LedgerUpgrade` variant.
- `crates/herder/src/upgrades.rs` — per-variant schedule → nominate emission +
  no-op suppression + `is_valid_for_nomination` acceptance at/after
  `upgrade_time`.
- parity-doc updates (`crates/{app,herder,ledger}/PARITY_STATUS.md`,
  `docs/supercluster-feasibility.md`) including the two documented divergences.
- this runbook.

**Operator-executed (NOT done in the PR):** the mission RUN itself — see the
checklist below.

## OPERATOR CHECKLIST — the live mission RUN

> Everything below is **operator-executed** against live infra. The PR does NOT
> run any of it and does NOT fabricate a mission transcript.

1. **Auth + bring up the mixed network** (one-time per session): `nsc login`
   (interactive browser OAuth). Reuse the Henyey mixed-image topology (stellar-core
   validators + ≥1 henyey node, core-majority quorum). Confirm baseline:
   `/info.state == "Synced!"` and seq+hash agreement (run `assert-henyey-mixed-mission.sh`).
2. **Capture the pre-upgrade baseline.** Record henyey + stellar-core `/info`
   (ledger num/hash, `protocol_version`, `base_fee`, `base_reserve`,
   `max_tx_set_size`, flags) so the post-upgrade diff is unambiguous.
3. **Drive the upgrade** via the SSC dotnet harness (`stellar/supercluster`,
   external) or directly against a validator's admin port. Pick the variant(s)
   the mission targets, e.g. a protocol-version bump:
   ```
   GET /upgrades?mode=set&upgradetime=1970-01-01T00:00:00Z&protocolversion=<N>
   ```
   For a config upgrade, pass the base64 `configupgradesetkey` (the SSC harness
   constructs and uploads the `ConfigUpgradeSet` first). Confirm each `mode=set`
   request returns `status: ok` (AC#1). Drive the upgrade on the validators the
   mission designates (typically the stellar-core majority; henyey follows).
4. **Observe nominate → externalize → apply** (AC#2/#3): watch `/scp` and `/info`
   on henyey across the scheduled upgrade ledger. The upgraded header/state field
   must change exactly at the upgrade ledger and persist after.
5. **Validate agreement** (AC#4/#5): compare henyey vs. stellar-core `/info`
   ledger num/hash AND the upgraded field at a common post-upgrade seq; confirm
   henyey stays `Synced!` throughout.
6. **Capture artifacts** into `runs/<date>-mission-3300/` and onto #3300:
   - `image-digest.txt` (the immutable `sha256:` reference),
   - `instance.json` (nsc instance metadata),
   - the exact `/upgrades?mode=set` invocation(s) + params,
   - **before/after ledger headers** (the upgrade-ledger diff),
   - per-pod henyey/stellar-core logs spanning the upgrade ledger,
   - seq+hash agreement output.
7. **Teardown:** `nsc destroy <instance-id> --force`, then `nsc list -o json` to
   confirm the ephemeral cluster is gone.
8. **On any failure or surfaced param/parity gap:** file a **concrete follow-up
   issue** (AC#7) with the captured artifacts — do NOT silently patch henyey or
   the tests to make the run pass. In particular, watch for the two documented
   divergences above if the mission reads via `mode=get` or relies on SET-time
   config-key rejection.
