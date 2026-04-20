# Testing Guide

## Quick Reference

| What | When to use | Command |
|------|-------------|---------|
| Unit + integration tests | Every change | `cargo test --all` |
| Lint | Before PR | `cargo clippy --all -- -D warnings` |
| Format check | Before PR | `cargo fmt --all -- --check` |
| Focused crate tests | Iterating on one crate | `cargo test -p henyey-ledger --tests` |
| Local quickstart (fast) | Test core only | `./scripts/quickstart-local.sh --enable core` |
| Local quickstart (full) | Test core + RPC + Horizon | `./scripts/quickstart-local.sh` |
| Local quickstart (galexie) | Test galexie ingestion | `./scripts/quickstart-local.sh --enable core,galexie` |

## Unit & Integration Tests

```bash
# All tests
cargo test --all

# Single crate (integration tests only)
cargo test -p henyey-ledger --tests

# Single crate (unit tests only)
cargo test -p henyey-tx --lib

# With output
cargo test --all -- --nocapture

# Lint (CI requires zero warnings)
cargo clippy --all -- -D warnings

# Format check
cargo fmt --all -- --check
```

Tests live alongside code in `crates/*/src` (unit) and `crates/*/tests` (integration). Name tests by behavior, e.g. `test_execute_transaction_min_seq_num_precondition`.

## CI Pipeline

Two GitHub Actions workflows run on every push to `main` and every PR.

### CI Workflow (`ci.yml`)

```
fmt ──┬── clippy     (lint, -D warnings)
      ├── test       (cargo test --all)
      └── build-release (cargo build --release --all)
```

`fmt` runs first; `clippy`, `test`, and `build-release` run in parallel after it passes. All four jobs must pass to merge.

### Quickstart Workflow (`quickstart.yml`)

Uses the upstream [stellar/quickstart](https://github.com/stellar/docker-stellar-core-horizon) reusable build workflow to run Docker integration tests. The workflow builds a quickstart image with henyey replacing stellar-core, then runs the test matrix:

| Network | Services | What it validates |
|---------|----------|-------------------|
| local | core, rpc, horizon | Standalone genesis → ledger close → RPC health → Horizon indexing |
| testnet | core, horizon | Catchup from testnet → sync → Horizon against live data (RPC temporarily disabled, see [#1848](https://github.com/stellar-experimental/henyey/issues/1848)) |
| pubnet | core, rpc, horizon | Catchup from mainnet → sync → RPC + Horizon against live data |

The quickstart workflow runs on `amd64` only. It uses the `stellar/quickstart:testing` base image with `horizon_skip_protocol_version_check: true` to allow henyey's version string.

Both workflows must pass before merging.

## Local Quickstart Testing

### Why

The quickstart CI workflow takes ~20 minutes on GitHub Actions. Running locally with `quickstart-local.sh` gives ~30-second iteration cycles for the same Docker integration tests.

### Prerequisites

- Docker
- Rust toolchain (for building henyey)
- `stellar` CLI, `curl`, `jq` (for sanity tests)

### Commands

```bash
# Full stack: core + RPC + Horizon (default)
./scripts/quickstart-local.sh

# Core only — fastest (~5s to healthy)
./scripts/quickstart-local.sh --enable core

# Core + RPC (no Horizon)
./scripts/quickstart-local.sh --enable core,rpc

# Skip the cargo build (reuse last binary)
./scripts/quickstart-local.sh --no-build

# Start without running tests (keep container alive)
./scripts/quickstart-local.sh --no-test --keep

# Tail container logs
./scripts/quickstart-local.sh --logs

# Use testnet instead of local standalone
./scripts/quickstart-local.sh --network testnet

# Custom health timeout (default 300s)
./scripts/quickstart-local.sh --timeout 600
```

Makefile shortcuts:

```bash
make quickstart-local      # ./scripts/quickstart-local.sh
make quickstart-build      # build binary + Docker image only
make quickstart-logs       # docker logs -f henyey-quickstart
make quickstart-stop       # stop + remove container
```

### How It Works

The script:

1. Builds henyey in release mode (`cargo build --release -p henyey`)
2. Pulls `stellar/quickstart:testing` if not cached
3. Builds a thin overlay image via `Dockerfile.quickstart-local` that replaces `stellar-core` with the henyey binary (`ln -sf /usr/bin/henyey /usr/bin/stellar-core`)
4. Starts the container with the requested services
5. Polls health endpoints until core reports `Synced!` and RPC reports `healthy`
6. Runs the sanity test scripts
7. Cleans up the container (unless `--keep`)

### Port Mapping

| Port | Service |
|------|---------|
| 8000 | Horizon / RPC (`/rpc` path) |
| 11626 | Core HTTP (node) |
| 11726 | Core HTTP (Horizon captive core) |
| 11826 | Core HTTP (RPC captive core) |

### Service Configurations

The `--enable` flag controls which processes start inside the quickstart container:

| Flag | Processes | Use case |
|------|-----------|----------|
| `core` | Node only | Fastest; test core sync and HTTP API |
| `core,rpc` | Node + stellar-rpc | Test RPC without Horizon overhead |
| `core,rpc,horizon` | Node + stellar-rpc + Horizon | Full stack (default) |
| `core,galexie` | Node + galexie | Test galexie ledger ingestion |

## Sanity Test Scripts

### `test-rpc-sanity.sh`

~20 checks covering the full Stellar JSON-RPC 2.0 surface:

- **Error handling**: unknown method (`-32601`), empty keys (`-32602`/`-32603`), missing params (`-32602`)
- **Read endpoints**: `getHealth`, `getNetwork`, `getLatestLedger`, `getFeeStats`, `getVersionInfo`
- **Account setup**: generate + fund two accounts via `stellar keys generate --fund`
- **Contract lifecycle**: deploy native SAC, simulate invocation, submit transfer
- **Query endpoints**: `getLedgerEntries`, `getTransaction`, `getTransactions` (with pagination), `getLedgers`, `getEvents` (filtered by contract + topic)
- **Consistency**: cross-check `getLatestLedger` vs `getHealth` ledger bounds, verify tx ledger falls within retention window

```bash
# Standalone (against a running RPC endpoint)
./scripts/test-rpc-sanity.sh --rpc-url http://localhost:8000/rpc \
  --network-passphrase "Standalone Network ; February 2017"

# Against public testnet (default)
./scripts/test-rpc-sanity.sh
```

### `test-horizon-sanity.sh`

~10 checks covering Horizon REST API basics:

- **Health**: root endpoint, `fee_stats`, latest ledger
- **Account funding**: friendbot for two accounts, verify account lookup
- **Classic payment**: build + sign + submit a native payment via Horizon `/transactions`
- **Indexing verification**: `GET /transactions/{hash}`, `GET /transactions/{hash}/operations`, `GET /accounts/{addr}/payments`

```bash
# Standalone
./scripts/test-horizon-sanity.sh \
  --horizon-url http://localhost:8000 \
  --rpc-url http://localhost:8000/rpc \
  --network-passphrase "Standalone Network ; February 2017"
```

### Running via quickstart-local

When `quickstart-local.sh` runs with `--enable core,rpc,horizon` (the default), it automatically invokes both sanity scripts after the container is healthy. You don't need to run them separately unless you're debugging.

## Debugging CI Failures

### Reproducing Locally

Match the CI configuration as closely as possible:

```bash
# Same services as CI quickstart
./scripts/quickstart-local.sh --enable core,rpc,horizon

# Testnet mode (matches CI testnet job — RPC temporarily disabled, see #1848)
./scripts/quickstart-local.sh --network testnet --enable core,horizon
```

### Inspecting Container Logs

```bash
# Tail logs from a running container
docker logs -f henyey-quickstart

# Last 200 lines from a stopped container
docker logs --tail 200 henyey-quickstart

# Makefile shortcut
make quickstart-logs
```

### Keeping the Container Alive

Start without tests and with `--keep` to inspect the running environment:

```bash
./scripts/quickstart-local.sh --no-test --keep

# Then manually run tests or inspect:
docker exec -it henyey-quickstart /bin/bash
curl -sf http://localhost:11626/info | jq .
```

### Common Issues

| Symptom | Cause | Fix |
|---------|-------|-----|
| Port already in use | Previous container not cleaned up | `make quickstart-stop` or `docker rm -f henyey-quickstart` |
| Health timeout | Slow catchup (testnet/pubnet) | Increase `--timeout 600`; check logs for catchup progress |
| `stellar-core` not found | Binary not copied correctly | Verify `target/release/henyey` exists; try without `--no-build` |
| Stale image | Old quickstart base image | `docker pull stellar/quickstart:testing` |
| Tests pass locally, fail in CI | Architecture mismatch (arm vs amd64) | CI runs on `amd64`; ensure your binary targets the same |

## Bugs Caught by CI

The quickstart CI has caught several bugs that unit tests alone would not have found. These are integration-level issues that only manifest when henyey runs inside the full quickstart stack with Horizon, stellar-rpc, and captive core:

- **Empty `scp_value.upgrades` in ledger headers** (commit `7f205a66`): Ledger headers were missing upgrade data, causing captive core startup race conditions. Only visible when Horizon's captive core ingested ledgers.
- **Captive core startup race** (commit `34f525df`): Core sync timing issue that caused captive core to fail to connect. Only manifested in the multi-process quickstart container.
- **`PREFERRED_UPGRADE_PROTOCOL_VERSION` not translated** (commit `bafe30e1`): stellar-core config key not handled in the compatibility translation layer. Quickstart sets this in its generated configs.
- **HotArchiveBucketList not initialized from genesis** (commit `5bdeea7c`): Missing initialization when starting from genesis ledger. Only triggered in quickstart local mode.
- **Galexie catchup mode** (commits `74889388`, `0c2f4d64`, `05bee33b`): Galexie's `Complete` mode from genesis needed replay instead of bucket-apply. Required multiple iterations to get right.

These bugs demonstrate the value of Docker integration testing beyond `cargo test`.
