# Henyey Supercluster Mixed-Image Mission

**Companion docs:** [`supercluster-nsc-workflow.md`](./supercluster-nsc-workflow.md), [`supercluster-feasibility.md`](./supercluster-feasibility.md).

This runbook documents the current Henyey integration point with Stellar Supercluster (SSC): injecting a Henyey image as the **new image** in an existing mixed-image SSC mission.

## Current Verified State

The current verified mission is Supercluster's built-in:

```text
MixedImageLoadGenerationWithOldImageMajority
```

Henyey is not hardcoded in Supercluster. It is injected at runtime with the `--image` option. The stellar-core image is supplied as `--old-image`.

Verified command shape:

```bash
dotnet run --project src/App/App.fsproj --configuration Release -- mission \
  MixedImageLoadGenerationWithOldImageMajority \
  --kubeconfig /path/to/kubeconfig \
  --namespace default \
  --destination /path/to/artifacts \
  --keep-data \
  --core-http-via-pod-exec \
  --image nscr.io/<workspace>/henyey-ssc:<tag-or-digest> \
  --old-image stellar/stellar-core:latest \
  --probe-timeout 240 \
  --tx-rate 5 \
  --num-txs 100 \
  --num-accounts 100 \
  --genesis-test-account-count 100
```

`--core-http-via-pod-exec` requires the Supercluster branch/PR that routes core HTTP requests through pod exec. This is required for Namespace/k3s runners because `.local` ingress names and `svc.cluster.local` names are not resolvable from the local dotnet runner.

## How Henyey Is Injected

In Supercluster, `MixedImageLoadGenerationWithOldImageMajority` is defined in:

```text
src/FSLibrary/MissionMixedImageLoadGeneration.fs
```

The relevant logic is:

```fsharp
let newImage = context.image
let oldImage = GetOrDefault context.oldImage newImage
```

For `MixedImageLoadGenerationWithOldImageMajority`:

```fsharp
let mixedImageLoadGenerationWithOldImageMajority (context: MissionContext) =
    mixedImageLoadGeneration 2 context
```

That means:

- `--old-image stellar/stellar-core:latest` supplies the 2-node old-image majority.
- `--image nscr.io/<workspace>/henyey-ssc:<tag-or-digest>` supplies the 1-node new-image minority, which is Henyey.

## What Has Been Verified

The integration run has verified:

- Henyey image launches under the `stellar-core` entrypoint.
- SSC-generated `stellar-core.cfg` is accepted without manual patching.
- Henyey parses SSC explicit quorum sets with `"$PUBKEY $NAME"` validators.
- Henyey translates SSC invariant regex patterns that Rust regex cannot parse.
- Henyey honors stellar-core-compatible `FORCE_SCP` defaulting for validator configs.
- Henyey overlay framing uses the XDR final-fragment bit correctly.
- Henyey derives overlay auth certificates from the actual configured network passphrase.
- Henyey authenticates with stellar-core peers.
- Stellar-core quorum logs show Henyey participating in quorum (`missing=0`) after the overlay/network-ID fixes.
- SSC HTTP access works with the pod-exec Supercluster branch.
- Henyey compat `/metrics` exposes the error metric keys that SSC's `CheckNoErrorMetrics` expects.

## Current Caveat

`MixedImageLoadGenerationWithOldImageMajority` starts both images independently from genesis. Because Henyey and stellar-core construct different genesis ledgers, Supercluster's pairwise consistency check can fail at ledger 1:

```text
Inconsistent peers: ledger 1 = <core> on core-old-0 and <henyey> on core-new-0
```

This is a mission-shape limitation rather than evidence that Henyey cannot interoperate. A stronger final compatibility mission should start one side from the other's state, for example by using `VersionMixConsensus` or a custom mixed-image mission with `fetchDBFromPeer` / catchup from a common genesis.

## Recommended Next Mission

Use a mixed-image mission with a shared genesis state:

- Build and push Henyey as an SSC-compatible image.
- Use Henyey as `--image` and stellar-core as `--old-image`.
- Start an initial stellar-core majority network.
- Start Henyey from that network's state via database fetch or archive catchup.
- Require authenticated overlay, advancing ledgers, and common ledger hash agreement after catchup.

## Artifact Checklist

Capture:

- Henyey image tag and digest.
- Namespace instance metadata and kubeconfig path.
- Exact Supercluster command.
- SSC-generated `stellar-core.cfg` for the Henyey pod.
- Henyey and stellar-core pod logs.
- Metrics JSON files emitted by Supercluster.
- Final pass/fail reason and any follow-up issue links.
