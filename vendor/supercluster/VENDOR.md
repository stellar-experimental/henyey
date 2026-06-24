# Vendored Supercluster fork

This directory is a **vendored snapshot** of our fork of
[stellar/supercluster](https://github.com/stellar/supercluster) — the F#
mission runner used to drive henyey ↔ stellar-core mixed-image testing on
ephemeral Kubernetes clusters.

It is vendored here (rather than referenced as a submodule) so the exact
mission harness used to validate henyey is versioned alongside henyey itself
and is reproducible without a separate fork.

## Provenance

- Upstream: `stellar/supercluster`
- Vendored from fork commit `e0934a55d5e17e6e63371e2bcad0331a15dabbb1`
  (also pushed as branch `henyey-mixed-image-missions` on the personal fork
  `tomerweller/supercluster`).
- Snapshot is the tracked tree only; build artifacts (`src/**/bin`,
  `src/**/obj`), the nested `.git`, and crash dumps (`core.*`) are excluded.

## Changes on top of upstream

1. **Pod-exec metrics scrape** (6 commits): route stellar-core HTTP through
   `kubectl exec … curl` when `--core-http-via-pod-exec` is set, and scrape
   per-node `/metrics` at mission end. Needed because henyey's HTTP endpoint
   isn't reachable via the usual cluster-DNS/ingress path.
2. **`MixedImageLoadGeneration` extensions** (commit `e0934a5`):
   - All-image compositions: `MixedImageLoadGenerationAllOldImage` (3 core / 0
     henyey, pure-core baseline) and `MixedImageLoadGenerationAllNewImage`
     (0 core / 3 henyey, pure-henyey) alongside the existing old/new-majority
     missions; zero-node compositions form clean single-image networks.
   - Tunable load: driven from `MissionContext`
     (`numAccounts`/`numTxs`/`txRate`/`genesisTestAccountCount`) with
     `SmallTestResources`, so `--num-txs/--num-accounts/--tx-rate/
     --genesis-test-account-count` are honored. Pass large values to reproduce
     the prior heavy profile (measured ~9 min core-majority / ~18 min
     henyey-majority at 20k accts / 50k txs vs ~5 min light).

Used to validate the henyey create_upgrade / loadgen fixes (#3601, #3602,
#3604, #3606, #3607).

## Updating

Re-vendor by re-running `git archive` of the fork's HEAD into this directory.
Keep this file's provenance commit in sync. Upstreaming these changes to
`stellar/supercluster` (and the wasm-size handling already merged on the
henyey side via #3606) would let this vendored copy be retired.
