# Stellar Supercluster Feasibility Evaluation

**Date**: 2026-03-13
**Goal**: Evaluate using Stellar Supercluster (SSC) for full network simulations with Henyey-only nodes.

## What is Stellar Supercluster?

SSC is SDF's production integration testing tool (`stellar/supercluster` on GitHub), written in F#, that orchestrates containerized stellar-core nodes on Kubernetes. It treats stellar-core as a black-box Docker image and controls it through:

1. **CLI commands**: `new-db`, `new-hist`, `catchup`, `run` (executed as Kubernetes container entrypoints)
2. **HTTP admin API** (port 11626): `/info`, `/metrics`, `/generateload`, `/upgrades`, `/tx`, `/manualclose`, `/sorobaninfo`, `/peers`, `/scp`, etc.
3. **stellar-core.cfg** (TOML): Generated per-node with quorum sets, history archives, peer lists, and testing flags

It supports 30+ "missions" ranging from simple payment tests to full pubnet topology simulations with TPS benchmarking. It requires a Kubernetes cluster with DNS and nginx-ingress controllers, .NET 8.0+, and is invoked via:

```
dotnet run --project src/App/App.fsproj --configuration Release -- mission <MissionName> --image=<docker-image>
```

## SSC Interface Surface

SSC interacts with stellar-core through three channels:

### CLI Commands

SSC defines its container commands as `["new-hist"; "new-db"; "catchup"; "run"; "test"]`. Keys are generated in-process by F# code (`KeyPair.Random()`), so `gen-seed` is never called.

### HTTP Admin API

SSC uses the following endpoints extensively:

- `/info` -- node state, ledger number, protocol version, peer count
- `/metrics` -- specific metric names parsed by F# type providers
- `/generateload` -- trigger internal load generation with mode, rate, accounts, txs
- `/upgrades` -- schedule protocol upgrades, max tx set size changes
- `/tx?blob=<base64>` -- submit signed transactions
- `/manualclose` -- trigger manual ledger close
- `/sorobaninfo` -- Soroban-specific ledger and transaction limits
- `/testacc?name=<account>` -- query test account balance/seqnum
- `/clearmetrics` -- reset metric counters
- `/peers`, `/scp`, `/quorum` -- network state queries
- `/startsurveycollecting`, `/stopsurveycollecting`, `/surveytopologytimesliced`, `/getsurveyresult` -- network survey

### Configuration

SSC generates `stellar-core.cfg` files in SCREAMING_CASE TOML format with fields including `DATABASE`, `NETWORK_PASSPHRASE`, `NODE_SEED`, `NODE_IS_VALIDATOR`, `PREFERRED_PEERS`, `QUORUM_SET`, `HISTORY`, `ARTIFICIALLY_ACCELERATE_TIME_FOR_TESTING`, `ARTIFICIALLY_GENERATE_LOAD_FOR_TESTING`, and many more.

### Key Metrics SSC Checks

- `scp.envelope.invalidsig`
- `history.publish.failure`
- `ledger.invariant.failure`
- `ledger.transaction.internal-error`
- `loadgen.run.start`, `loadgen.run.complete`, `loadgen.run.failed`
- `loadgen.account.created`, `loadgen.txn.attempted`
- `overlay.byte.read`

## Henyey Compatibility Assessment

### Already Compatible

| SSC Integration Point | Henyey Status |
|---|---|
| `new-db` CLI command | Supported |
| `new-hist` CLI command | Supported |
| `catchup` CLI command | Supported |
| `run` CLI command | Supported (validator + watcher modes) |
| `force-scp` CLI command | Supported |
| `http-command` CLI command | Supported |
| HTTP admin on port 11626 | Supported (20+ endpoints) |
| `/info` endpoint | Supported |
| `/metrics` endpoint | Supported |
| `/generateload` endpoint | Supported (feature-gated) |
| `/upgrades` endpoint | Supported |
| `/tx` (submit transaction) | Supported |
| `/manualclose` | Supported |
| `/sorobaninfo` | Supported |
| `/peers`, `/scp`, `/quorum` | Supported |
| stellar-core.cfg format parsing | Supported (auto-detected via compat layer) |
| SCP consensus (100% parity) | Fully implemented |
| Overlay/P2P networking (92% parity) | Fully implemented |
| Transaction execution (97% parity) | Fully implemented |
| History archives (catchup/publish) | Implemented (82% parity) |

### Gaps Requiring Work

**1. No standalone Dockerfile (small effort)**

SSC expects a Docker image via `--image=<stellar-core-docker-image>`. Henyey currently works by replacing the binary inside `stellar/quickstart`. For SSC, a Henyey Docker image is needed where the binary is available at the path `stellar-core` (SSC uses `stellarCoreBinPath = "stellar-core"`). A simple image with `henyey` symlinked to `stellar-core` would work.

**2. `/testacc` HTTP endpoint (medium effort)**

SSC uses `GET /testacc?name=<account>` to query test account state during load generation. This endpoint is not present in Henyey's compat HTTP server. It's used by load generation missions to check account balances and sequence numbers.

**3. `/generateload` end-to-end parity (needs verification)**

SSC sets `ARTIFICIALLY_GENERATE_LOAD_FOR_TESTING` in config and triggers load generation via the `/generateload` HTTP endpoint with modes: `pay`, `soroban_upload`, `soroban_invoke`, `mixed_classic_soroban`, `upgrade_setup`, `create_upgrade`, `soroban_invoke_setup`, `stop`, `pay_pregenerated`. Henyey has the endpoint but it's feature-gated. The internal load generator needs to be compatible with SSC's expected parameters and behavior.

**4. `ARTIFICIALLY_ACCELERATE_TIME_FOR_TESTING` support (needs verification)**

Many SSC missions set `accelerateTime = true` to close ledgers faster than the normal 5-second cadence. Henyey needs to honor this config flag.

**5. Metrics wire format compatibility (medium effort)**

SSC parses specific metric names from `/metrics` responses using F# type providers. The JSON structure and metric names must match exactly.

**6. History publish reliability (gap at 56% historywork parity)**

Several missions require nodes to publish to local history archives. The `put` and `mkdir` commands in the `HISTORY` config section need to work for missions where nodes publish and other nodes catch up.

**7. Network survey endpoints (low priority)**

Survey endpoints (`/startsurveycollecting`, `/stopsurveycollecting`, `/surveytopologytimesliced`, `/getsurveyresult`) are stubs in Henyey. Only needed for `MissionMixedImageNetworkSurvey`.

## Mission-by-Mission Feasibility

| Mission Category | Feasibility | Blockers |
|---|---|---|
| **SimplePayment** | HIGH | Needs `/generateload` + `/testacc` + metrics parity |
| **ComplexTopology** | HIGH | Same as SimplePayment |
| **LoadGeneration** (sustained) | MEDIUM | Same + `accelerateTime` + spike handling |
| **History catchup** (testnet/pubnet) | HIGH | Catchup is already well-tested |
| **VersionMix** (mixed images) | HIGH | Mix Henyey + stellar-core images; SSC supports this natively |
| **ProtocolUpgrade** | MEDIUM | `/upgrades` endpoint exists; needs end-to-end testing |
| **MaxTPS** benchmarking | LOW | Requires `perftests` build flags, Postgres support, full load gen |
| **Soroban load** | LOW | Needs Soroban-specific load gen modes, config upgrade flows |
| **DatabaseInplaceUpgrade** | N/A | Henyey uses SQLite; different DB schema |
| **EmitMeta** | HIGH | Metadata streaming already works |
| **NetworkSurvey** | LOW | Survey endpoints are stubs in Henyey |

## Recommended Approach

### Phase 1 -- Mixed-image testing (immediate value, minimal work)

Use SSC's existing `MissionVersionMixConsensus` and similar mixed-image missions with a mix of stellar-core and Henyey images. This validates that Henyey nodes participate correctly in a real network without requiring full load generation parity. SSC already supports mixed images natively.

Work required:
- Create a Henyey Docker image with `stellar-core` symlink

### Phase 2 -- Basic Henyey-only missions (moderate work)

Target `MissionSimplePayment` and `MissionComplexTopology` with all-Henyey nodes.

Work required:
1. Implement `/testacc` HTTP endpoint
2. Verify `/generateload` works end-to-end with SSC's parameters
3. Verify metrics wire format matches SSC's expectations
4. Test `accelerateTime` behavior

### Phase 3 -- Catchup and upgrade missions (moderate work)

Target history and protocol upgrade missions.

Work required:
1. Ensure history publish works reliably
2. Test `/upgrades` endpoint end-to-end with SSC
3. Validate cross-version catchup (Henyey publishes, Henyey catches up)

### Phase 4 -- Performance testing (significant work)

Target MaxTPS missions.

Work required:
1. Full load generation parity (all modes)
2. Postgres backend support (currently SSC uses Postgres for MaxTPS)
3. Performance-tuned build configuration

## Conclusion

Using SSC with Henyey-only nodes is feasible and architecturally sound. The fundamental architecture is well-aligned: Henyey already has SCP (100%), overlay (92%), the compat HTTP server on port 11626, and stellar-core config format parsing. The gaps are mostly in testing infrastructure features (load generation, test account queries, accelerated time) rather than in core consensus or networking.

The cleanest path to value is starting with mixed stellar-core/Henyey image testing (Phase 1) and progressively moving to all-Henyey networks as the HTTP API and load generation compatibility gaps are closed.
