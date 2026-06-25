# Henyey Supercluster Mixed-Image Mission

**Companion docs:** [`supercluster-nsc-workflow.md`](./supercluster-nsc-workflow.md)
(building/publishing the henyey image and provisioning clusters via `nsc`),
[`supercluster-feasibility.md`](./supercluster-feasibility.md), and
[`../vendor/supercluster/VENDOR.md`](../vendor/supercluster/VENDOR.md)
(the vendored harness: provenance, fork changes, and the heavy-load runbook).

This runbook documents how to run a Henyey ↔ stellar-core **mixed-image**
Supercluster (SSC) mission: henyey is injected as one image and stellar-core as
the other, and the two interoperate in a single network.

## The harness is vendored in-tree

The SSC harness is vendored at [`vendor/supercluster/`](../vendor/supercluster/)
(a snapshot of our fork; see `VENDOR.md` for provenance and the changes on top of
upstream). Run the dotnet harness from there — no separate checkout or fork is
needed, and the `--core-http-via-pod-exec` routing is already bundled.

## Prerequisite: a BUILD_TESTS stellar-core image

Loadgen missions **require a `BUILD_TESTS` stellar-core image** as the
`--old-image`. The `generateload` HTTP command and `GENESIS_TEST_ACCOUNT_COUNT`
account creation are both `#ifdef BUILD_TESTS` in stellar-core, so **no public
`stellar/stellar-core` tag works** — `stellar/stellar-core:latest` has neither
and the mission cannot drive load against it.

Build one from stellar-core's canonical `docker/Dockerfile.testing`, **minus**
the `--enable-next-protocol-version-unsafe-for-production` flag (so the max
protocol stays at the released version, matching henyey) and **minus** the
privileged sysctl / `--enable-tracy` lines (tests stay on by default):

```bash
nsc docker login
nsc build -f Dockerfile.testing --platform linux/amd64 --push \
  -n nscr.io/<workspace>/stellar-core-testing:v27.0.0 .
```

Pin the **digest** (not the tag) when running missions. The currently published
image is `nscr.io/k4jkul01t5rr0/stellar-core-testing@sha256:bc1de6bc77e729b6d34a67a325114d0d08b9ff30cecaa022087f8fec1754aece`
(stellar-core v27.0.0 == the henyey submodule pin; has `test` / `gen-fuzz` /
`pregenerate-loadgen-txs`). Match the protocol version to henyey's.

## Missions

The vendored fork (`src/FSLibrary/MissionMixedImageLoadGeneration.fs`) provides
four compositions over a 3-node network. The loadgen runs on the **majority**
image (`coreSets[0]`):

| mission | core / henyey | loadgen driver |
|---|---|---|
| `MixedImageLoadGenerationWithOldImageMajority` | 2 core / 1 henyey | stellar-core |
| `MixedImageLoadGenerationWithNewImageMajority` | 1 core / 2 henyey | henyey |
| `MixedImageLoadGenerationAllOldImage` | 3 core / 0 henyey | stellar-core (pure-core baseline) |
| `MixedImageLoadGenerationAllNewImage` | 0 core / 3 henyey | henyey (pure-henyey) |

Henyey is injected with `--image`; stellar-core with `--old-image`. Each mission
boots both images from genesis, upgrades protocol + `maxTxSetSize`, runs a
classic payment loadgen, then a Soroban config-upgrade + upload loadgen.

## Running a mission

From `vendor/supercluster/`:

```bash
dotnet run --project src/App/App.fsproj --configuration Release -- mission \
  MixedImageLoadGenerationWithNewImageMajority \
  --kubeconfig /path/to/kubeconfig \
  --namespace default \
  --destination /path/to/artifacts \
  --core-http-via-pod-exec \
  --image nscr.io/<workspace>/henyey:<tag-or-digest> \
  --old-image nscr.io/<workspace>/stellar-core-testing@sha256:<digest> \
  --probe-timeout 240 \
  --num-accounts 20000 \
  --num-txs 50000 \
  --tx-rate 150 \
  --genesis-test-account-count 20000
```

- **`--core-http-via-pod-exec`** is required on Namespace/k3s runners: `.local`
  ingress names and `svc.cluster.local` names aren't resolvable from the local
  dotnet runner, so core HTTP is routed through `kubectl exec … curl`.
- **`--genesis-test-account-count`** (match `--num-accounts`) pre-funds the
  loadgen accounts at genesis. Without it the loadgen's account-creation step is
  unreliable against the BUILD_TESTS core image on these clusters.
- **`--tx-rate`**: pick a *sustainable* rate. The per-ledger apply ceiling is
  `maxTxSetSize / ledger_close_time` ≈ `1000 / ~5.4s` ≈ **185 tx/s**. `150` is
  representative; `250` deliberately over-drives the network (both images), which
  surfaces a loadgen run-1 timeout artifact — see
  [`VENDOR.md` → Running the heavy mixed-image A/B](../vendor/supercluster/VENDOR.md)
  and henyey issues #3611 / #3612.

See [`supercluster-nsc-workflow.md`](./supercluster-nsc-workflow.md) for building
and publishing the henyey image and provisioning the cluster + kubeconfig.

## How henyey is injected

`mixedImageLoadGeneration n` builds `n` old-image (core) nodes and `3 - n`
new-image (henyey) nodes; `WithOldImageMajority = 2`, `WithNewImageMajority = 1`,
`AllOldImage = 3`, `AllNewImage = 0`:

```fsharp
let newImage = context.image                           // --image     → henyey
let oldImage = GetOrDefault context.oldImage newImage  // --old-image  → stellar-core
```

## What has been verified

- Henyey launches under the `stellar-core` entrypoint; SSC-generated
  `stellar-core.cfg` is accepted without manual patching.
- Henyey parses SSC explicit quorum sets (`"$PUBKEY $NAME"`) and translates SSC
  invariant regex patterns Rust regex can't parse.
- Henyey honors stellar-core-compatible `FORCE_SCP` defaulting; overlay framing
  uses the XDR final-fragment bit; auth certs derive from the configured network
  passphrase; henyey authenticates with stellar-core peers and participates in
  quorum (`missing=0`).
- Compat `/metrics` exposes the error-metric keys SSC's `CheckNoErrorMetrics`
  expects; SSC HTTP access works via pod-exec.
- End-to-end loadgen + create_upgrade + soroban-upload missions pass for both
  core-majority and henyey-majority (after the loadgen fixes #3601/#3602/#3604/
  #3606/#3607 and #3609/#3610/#3613/#3614).

## Genesis parity note

stellar-core's genesis ledger-1 is root-only at protocol 0 in **non-BUILD_TESTS**
images and is invariant to `GENESIS_TEST_ACCOUNT_COUNT`; henyey injects the N
test accounts into genesis. With a **BUILD_TESTS** core image (as required above)
both create the N genesis accounts, so `--genesis-test-account-count N` keeps the
two genesis ledgers consistent. With a non-BUILD_TESTS core image they only match
at `count=0`.

## Artifact checklist

Capture: henyey image tag + digest; the BUILD_TESTS core image digest; the
kubeconfig path and Namespace instance metadata; the exact mission command;
SSC-generated `stellar-core.cfg`; henyey + stellar-core pod logs; the per-node
`*.metrics.json` SSC emits; and the final pass/fail reason + follow-up links.
