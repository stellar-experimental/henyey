# 3-Node Deterministic Simulation Plan

Full-parity deterministic simulation matching stellar-core's `Simulation` architecture:
single-process, virtual-clock, loopback overlay, crankable event loop.

## Architecture Overview

A `Simulation` harness manages 3 (or N) `App` instances in one process. Each node
gets its own `VirtualClock` and is connected via in-memory loopback channels instead
of TCP. Time only advances when the harness "cranks" it forward, making the entire
system deterministic and fast.

```
Simulation (master clock + node registry)
  |
  +-- SimNode[0]: { VirtualClock[0], App[0], LoopbackOverlay }
  |     |-- Herder -> SCP (local consensus)
  |     |-- LedgerManager (applies closed ledgers)
  |     |-- LoopbackTransport -> channel pair -> SimNode[1]
  |     |-- LoopbackTransport -> channel pair -> SimNode[2]
  |
  +-- SimNode[1]: { VirtualClock[1], App[1], LoopbackOverlay }
  |
  +-- SimNode[2]: { VirtualClock[2], App[2], LoopbackOverlay }
  |
  +-- LoopbackNetwork: channel registry + fault injection
```

**Crank cycle** (per stellar-core `Simulation.cpp:439-537`):
1. Set quantum timer on master clock (100ms virtual).
2. For each node, advance its virtual clock up to the master's next event.
3. Each clock crank either processes pending channel messages or jumps time forward.
4. Crank master clock.
5. Repeat until predicate (e.g., all nodes externalized ledger N) or timeout.

## Stellar-Core Reference

The plan is derived from stellar-core's simulation system (v25, submodule at `stellar-core/`):

| Component | File | Purpose |
|-----------|------|---------|
| `Simulation` | `src/simulation/Simulation.{h,cpp}` | Orchestrator: owns nodes, connections, crank loop |
| `Topologies` | `src/simulation/Topologies.{h,cpp}` | Factory: `core()`, `pair()`, `cycle()`, etc. |
| `VirtualClock` | `src/util/Timer.{h,cpp}` | Deterministic clock with `crank()` / `advanceToNext()` |
| `LoopbackPeer` | `src/overlay/test/LoopbackPeer.{h,cpp}` | In-memory transport with fault injection |
| `LoopbackOverlayManager` | `src/simulation/Simulation.h:148-186` | Overrides `connectToImpl` for loopback |
| `CoreTests` | `src/simulation/CoreTests.cpp` | 3-node simulation tests |

## Current State in Henyey

**What exists:**
- `SimulationDriver` in `crates/scp/tests/multi_node_simulation.rs` — SCP-only, synchronous message pump, no time, no networking, no ledger close
- `TestSCPDriver` in SCP parity tests — single-node with simulated timer offset
- One real TCP overlay integration test (`crates/overlay/tests/overlay_scp_integration.rs`)
- `HerderCallback` trait — clean 3-method abstraction (`close_ledger`, `validate_tx_set`, `broadcast_scp_message`)
- `SCPDriver` trait — generic, already suitable for simulation

**Gaps:**
- No `VirtualClock` or clock abstraction — ~290 direct `Instant::now()` / `SystemTime::now()` calls
- No `OverlayManager` trait — hardcoded to TCP (`TcpStream`, `TcpListener`)
- No loopback transport
- `App` is a God object (~50 fields, all concrete types)
- `tokio/test-util` feature not enabled
- Main event loop (`App::run()` in `lifecycle.rs`) uses 14 `tokio::time::interval` timers in a `tokio::select!` loop — not directly "crankable"

## Execution Plan (Authoritative)

This section is the delivery runbook for executing the simulation work. The
later phase breakdown remains as technical design reference, but execution
priority, sequencing, and acceptance are defined here.

### Stage 0: Baseline and Guardrails (1 day)

**Objective:** Freeze a baseline before refactors and define pass/fail gates.

**Tasks:**
- Capture current workspace status: `cargo build --all`, `cargo test --all`.
- Record current simulation baseline from `crates/scp/tests/multi_node_simulation.rs`.
- Define deterministic acceptance criteria and store them in this document.

**Exit criteria:**
- Baseline build and tests are green.
- Determinism criteria documented (same inputs => same final ledger hash).
- Non-goals for v1 explicitly listed (no full chaos matrix, no custom scheduler).

### Stage 1: Clock crate and minimal wiring (2-3 days)

**Objective:** Introduce clock abstractions without changing runtime behavior.

**Tasks:**
- Add `crates/clock/` with `Clock`, `RealClock`, and `VirtualClock`.
- Add workspace membership and dependencies.
- Inject `Arc<dyn Clock>` into top-level `App` construction only.

**Exit criteria:**
- `cargo build --all` and `cargo test --all` pass.
- Existing runtime behavior unchanged in non-simulation modes.

### Stage 2: Behavioral time migration (4-6 days)

**Objective:** Migrate consensus-relevant time paths to `Clock`.

**Tasks:**
- Migrate `app` and `herder` behavioral timers first.
- Keep observational/perf-only timing on `std::time::Instant`.
- Ensure all timeout/interval logic in consensus path uses injected clock.

**Exit criteria:**
- Full test suite passes.
- No liveness regressions in validator/watcher startup and run flow.

### Stage 3: Overlay transport abstraction (4-5 days)

**Objective:** Make overlay transport pluggable while preserving TCP by default.

**Tasks:**
- Introduce `Transport`, `TransportListener`, `ConnectionFactory`.
- Adapt existing TCP code into `TcpConnectionFactory`.
- Refactor `OverlayManager` to use factory-based connect/bind/accept.

**Exit criteria:**
- Existing TCP integration tests pass without test changes.
- Default behavior remains TCP unless simulation mode opts into loopback.

### Stage 4: Loopback network and simulation harness (4-5 days)

**Objective:** Build deterministic multi-node app-level simulation.

**Tasks:**
- Add `crates/simulation/` with `Simulation`, `SimNode`, and crank loop.
- Add `LoopbackNetwork` and loopback connection factory.
- Start with `core3` topology and deterministic seed/key generation.

**Exit criteria:**
- 3-node loopback simulation externalizes at least 10 ledgers deterministically.
- Crank loop supports timeout-bound predicates.

### Stage 5: Validation suite and reliability checks (3-4 days)

**Objective:** Add high-signal tests validating convergence and determinism.

**Tasks:**
- Add tests for: basic consensus, degraded quorum, partition/heal, replay determinism.
- Add one bounded message-loss scenario (drop probability) after stability.
- Run repeated deterministic replay to catch flaky scheduling issues.

**Exit criteria:**
- New simulation test suite is stable across repeated runs.
- Deterministic replay test passes consistently (no hash drift).

### Stage 6: Documentation, parity tracking, and handoff (1 day)

**Objective:** Make the work executable and maintainable for future contributors.

**Tasks:**
- Update crate-level docs for any new public simulation/clock APIs.
- Update `PARITY_STATUS.md` where behavior parity status changed.
- Add a short runbook: how to execute simulation tests locally and in CI.

**Exit criteria:**
- Documentation reflects delivered behavior and limitations.
- CI/local commands are documented and verified.

### PR Slicing and Merge Order

To reduce risk, land changes in narrow PRs in this order:

1. `clock` crate skeleton + workspace wiring.
2. `App` constructor clock injection with no callsite migration.
3. `app` + `herder` behavioral time migration.
4. transport traits + TCP factory adapter.
5. `OverlayManager` factory integration.
6. loopback network + loopback factory.
7. simulation harness crate (`core3` only).
8. simulation validation tests.
9. docs/parity updates.

### Deterministic Definition of Done

Simulation v1 is complete when all of the following are true:

- `Topologies::core3(SimulationMode::OverLoopback)` closes 10 ledgers within virtual timeout.
- Partition/heal scenario converges after recovery.
- Running the same scenario twice yields identical final ledger hash across all nodes.
- Default runtime mode remains TCP + real time with no regression in existing overlay tests.

### Execution Commands (required at each stage boundary)

```bash
cargo build --all
cargo test --all
```

For simulation stages, also run:

```bash
cargo test -p henyey-simulation --tests
```

## Execution Progress

- [x] Stage 0 complete: baseline captured and deterministic gates documented.
- [x] Stage 1 complete: `crates/clock/` added (`Clock`, `RealClock`, `VirtualClock`) and wired into workspace.
- [x] Stage 1 complete: `App::new_with_clock(config, clock)` added and `App::new(config)` now delegates to `RealClock`.
- [x] Stage 2 complete (initial scope): migrated core startup/consensus timing touchpoints to clock (`lifecycle.rs`, `consensus.rs`, `peers.rs`).
- [x] Stage 2 expanded: migrated additional consensus-adjacent timing paths in `catchup_impl.rs`, `tx_flooding.rs`, `survey_impl.rs`, and `ledger_close.rs` to injected clock where behavior-sensitive.
- [x] Stage 2 expanded further: migrated `App` construction-time time anchors (`last_externalized_at`, `last_scp_state_request_at`, survey scheduler/reporting initialization, and `/info` age calculation) to the injected clock.
- [x] Stage 2 expanded further: migrated SCP latency tracker timing from direct wall clock calls to clock-provided instants (`record_first_seen`, `record_self_sent`, `record_other_after_self`).
- [x] Stage 3 complete (TCP-preserving abstraction): added `ConnectionFactory` + `TcpConnectionFactory` and wired `OverlayManager` through factory-based connect/bind.
- [x] Stage 4 complete (v1 harness): added `crates/simulation/` deterministic harness with `Simulation`, `SimNode`, `SimulationMode`, `Topologies::core3/pair`.
- [x] Stage 4 refined: extracted `LoopbackNetwork` model (`partition`, `heal`, `drop_prob`, `link_active`) to separate simulation network module for clearer loopback semantics.
- [x] Stage 4 refined: added additional deterministic topology support (`cycle`, `separate`) and connectivity helper (`is_fully_connected`) in simulation harness.
- [x] Stage 5 complete (v1 tests): added simulation tests for 3-node close, partition/heal recovery, deterministic replay, and bounded message loss.
- [x] Stage 5 expanded: added topology coverage tests for cycle convergence and separate-cluster non-connectivity behavior.
- [x] Stage 6 complete (handoff update): this plan now reflects delivered execution status and command validation.

### Current Validation Status

- `cargo test -p henyey-clock` passes.
- `cargo test -p henyey-app` passes.
- `cargo test -p henyey-overlay` passes.
- `cargo test -p henyey-simulation --tests` passes.
- `cargo test -p henyey-simulation --tests` passes (including cycle/separate topology scenarios).
- `cargo test -p henyey-simulation --test serious_simulation -- --nocapture` passes (core7 long-run staged fault schedule + deterministic replay).
- `cargo test --all` passes.

## Detailed Design Phases (Reference)

### Phase 1: Clock Abstraction (3-5 days)

**New crate:** `crates/clock/`

Create a `Clock` trait with real and virtual implementations:

```rust
pub trait Clock: Send + Sync + 'static {
    /// Monotonic instant (replaces std::time::Instant::now())
    fn now(&self) -> Instant;

    /// Wall-clock time (replaces SystemTime::now())
    fn system_now(&self) -> SystemTime;

    /// Async sleep (replaces tokio::time::sleep)
    fn sleep(&self, duration: Duration) -> BoxFuture<'_, ()>;

    /// Periodic tick (replaces tokio::time::interval)
    fn interval(&self, period: Duration) -> BoxStream<'_, ()>;

    /// Bounded async wait (replaces tokio::time::timeout)
    fn timeout<F: Future>(&self, duration: Duration, f: F)
        -> BoxFuture<'_, Result<F::Output, Elapsed>>;
}
```

**`RealClock`** — delegates to `std::time` and `tokio::time`. Zero overhead.

**`VirtualClock`** — initial implementation leverages `tokio/test-util`:
- Add `tokio = { features = ["test-util"] }` for test/simulation builds.
- Use `tokio::time::pause()` at simulation start to freeze tokio's internal clock.
- Use `tokio::time::advance(quantum)` in the crank loop.
- Maintain a separate `virtual_now: Mutex<Instant>` for `std::time::Instant` replacements.
- `VirtualClock::crank(quantum)`: advance both `virtual_now` and tokio's mock clock by `quantum`.

This approach gets us running quickly. If tokio's test-util proves insufficient
(e.g., edge cases with multi-runtime or `std::thread::sleep`), we can build a fully
custom event queue later.

**Observational-only time** (perf logging in `ledger/manager.rs`, `app/logging.rs`,
HTTP request timing) stays as bare `std::time::Instant::now()` — does not affect
determinism and keeps the diff smaller.

### Phase 2: Clock Injection (5-8 days)

Replace behavioral time calls with `clock.*()` methods. This is the largest phase.

**What to inject** (~150 behavioral call sites):

| Crate | Module | Call Sites | Examples |
|-------|--------|------------|---------|
| `app` | `lifecycle.rs` | ~14 | All `tokio::time::interval` timers in the event loop |
| `app` | `consensus.rs` | 1 | Local time for SCP trigger |
| `app` | `tx_flooding.rs` | ~5 | Demand/advert age cutoffs |
| `app` | `peers.rs` | ~4 | Ping tracking, reconnect timing |
| `app` | `catchup_impl.rs` | ~10 | Catchup timing, cache aging |
| `herder` | `herder.rs` | ~8 | Close time computation, nomination timestamps |
| `herder` | `scp_driver.rs` | ~3 | Close time validation |
| `herder` | `sync_recovery.rs` | ~8 | Consensus stuck detection, recovery deadlines |
| `herder` | `timer_manager.rs` | ~4 | SCP timer management |
| `herder` | `upgrades.rs` | 1 | Upgrade time computation |
| `overlay` | `manager.rs` | ~20 | Peer tick, DNS scheduling, flow control |
| `overlay` | `peer_manager.rs` | ~5 | Next-attempt scheduling |
| `overlay` | `auth.rs` | 2 | Certificate expiration/validation |
| `overlay` | `ban_manager.rs` | ~6 | Ban expiration |
| `overlay` | `flow_control.rs` | ~4 | Throttle/capacity tracking |
| `overlay` | `flood.rs` | ~6 | Flood gate TTL, rate window |
| `overlay` | `item_fetcher.rs` | 2 | Fetch timestamps |
| `common` | `time.rs` | 2 | `current_timestamp()`, `current_timestamp_ms()` |

**What to leave as `std::time::Instant::now()`** (~140 observational call sites):

| Crate | Module | Call Sites | Reason |
|-------|--------|------------|--------|
| `ledger` | `manager.rs` | ~20 | Perf timing for ledger close phases (log-only) |
| `ledger` | `execution/` | ~4 | Transaction execution timing (log-only) |
| `app` | `logging.rs` | ~3 | Progress tracker (log-only) |
| `app` | `http/`, `compat_http/` | ~3 | Request timing (log-only) |
| `henyey` | `main.rs` | ~5 | CLI operation timing (log-only) |
| `work` | `lib.rs` | 1 | Work item timing (log-only) |
| `tx` | `soroban/host.rs` | 2 | Soroban invoke timing (log-only) |

**Approach:**
1. Add `clock: Arc<dyn Clock>` field to `App`, `Herder`, `OverlayManager`, and subsystems.
2. Thread it through constructors (`App::new(config, clock)` etc.).
3. Replace calls crate by crate: `common` -> `herder` -> `overlay` -> `app`.
4. Run `cargo test --all` after each crate to catch regressions.

### Phase 3: Transport Abstraction & Loopback Overlay (5-7 days)

**Goal:** Make `OverlayManager` transport-agnostic.

#### 3a. Transport trait

```rust
/// Bidirectional message transport (TCP or loopback)
#[async_trait]
pub trait Transport: Send + Sync {
    async fn send(&self, msg: &[u8]) -> Result<()>;
    async fn recv(&mut self) -> Result<Vec<u8>>;
    fn close(&self);
    fn peer_address(&self) -> PeerAddress;
}
```

**TCP implementation:** Wraps existing `Framed<TcpStream, MessageCodec>`.

**Loopback implementation:**
```rust
struct LoopbackTransport {
    tx: mpsc::UnboundedSender<TimestampedMessage>,
    rx: Mutex<mpsc::UnboundedReceiver<TimestampedMessage>>,
    // Fault injection (mirrors stellar-core LoopbackPeer.h:38-43)
    drop_prob: AtomicF64,
    reorder_prob: AtomicF64,
    damage_prob: AtomicF64,
    corked: AtomicBool,      // simulate partition
    straggling: AtomicBool,  // simulate slow node
}
```

#### 3b. ConnectionFactory trait

```rust
#[async_trait]
pub trait ConnectionFactory: Send + Sync {
    type Listener: TransportListener;
    async fn connect(&self, addr: PeerAddress) -> Result<Box<dyn Transport>>;
    async fn bind(&self, port: u16) -> Result<Self::Listener>;
}

#[async_trait]
pub trait TransportListener: Send + Sync {
    async fn accept(&mut self) -> Result<(Box<dyn Transport>, PeerAddress)>;
}
```

**TCP factory:** Wraps `TcpStream::connect` and `TcpListener::bind`.

**Loopback factory:**
```rust
struct LoopbackConnectionFactory {
    network: Arc<LoopbackNetwork>,
    local_node_id: NodeId,
}
```
- `connect(addr)`: looks up target node in `LoopbackNetwork` registry, creates bidirectional channel pair.
- `bind(port)`: registers this node's endpoint in the network registry.
- `accept()`: waits for incoming connections from other nodes via a channel.

#### 3c. OverlayManager refactor

Key changes to `crates/overlay/src/manager.rs`:
- Constructor accepts `Box<dyn ConnectionFactory>` instead of hardcoded port.
- `start_listener()` uses `factory.bind()` + `listener.accept()` loop.
- Outbound connections use `factory.connect()`.
- Peer read/write loops use `Transport::send`/`recv`.
- The `Connection` struct in `connection.rs` becomes the TCP `Transport` impl.

**Risk mitigation:** Keep `TcpConnectionFactory` as the default. All existing tests and
production code continue to work unchanged. Loopback is opt-in for simulation.

#### 3d. LoopbackNetwork

```rust
pub struct LoopbackNetwork {
    /// Registry of node endpoints
    registry: DashMap<NodeId, LoopbackEndpoint>,
    /// Active connections for fault injection control
    connections: Mutex<Vec<Arc<LoopbackConnection>>>,
}

impl LoopbackNetwork {
    /// Create a bidirectional connection between two nodes
    pub fn connect(&self, a: NodeId, b: NodeId) -> (LoopbackTransport, LoopbackTransport);

    /// Partition: cork all connections involving a node
    pub fn partition(&self, node: NodeId);
    pub fn heal_partition(&self, node: NodeId);

    /// Fine-grained fault injection on a specific link
    pub fn set_drop_prob(&self, a: NodeId, b: NodeId, prob: f64);
    pub fn set_reorder_prob(&self, a: NodeId, b: NodeId, prob: f64);
    pub fn set_damage_prob(&self, a: NodeId, b: NodeId, prob: f64);
}
```

### Phase 4: Simulation Harness (4-6 days)

**New crate:** `crates/simulation/`

```rust
pub struct Simulation {
    nodes: HashMap<NodeId, SimNode>,
    loopback_network: Arc<LoopbackNetwork>,
    master_clock: Arc<VirtualClock>,
    mode: SimulationMode,
}

struct SimNode {
    clock: Arc<VirtualClock>,
    app: Arc<App>,
    secret_key: SecretKey,
    node_id: NodeId,
}

pub enum SimulationMode {
    /// In-memory channels, virtual time, deterministic
    OverLoopback,
    /// Real TCP, real time, non-deterministic (for debugging)
    OverTcp,
}
```

**Key methods:**

```rust
impl Simulation {
    /// Add a node with its keypair and quorum configuration
    pub fn add_node(
        &mut self,
        secret_key: SecretKey,
        quorum_set: ScpQuorumSet,
        config_override: Option<fn(&mut AppConfig)>,
    );

    /// Register a connection to establish at start
    pub fn add_pending_connection(&mut self, a: NodeId, b: NodeId);

    /// Start all nodes and establish connections
    pub async fn start_all_nodes(&mut self);

    /// Crank all nodes forward by one quantum (100ms virtual)
    pub async fn crank_all_nodes(&self) -> bool;

    /// Crank until predicate is true or timeout
    pub async fn crank_until(
        &self,
        predicate: impl Fn(&Simulation) -> bool,
        timeout: Duration,
    ) -> bool;

    /// Check if all nodes have externalized at least `ledger_seq`
    pub fn have_all_externalized(&self, ledger_seq: u32, max_spread: u32) -> bool;

    /// Get a node's current ledger sequence
    pub fn ledger_seq(&self, node: &NodeId) -> u32;

    /// Access the loopback network for fault injection
    pub fn network(&self) -> &LoopbackNetwork;
}
```

**Per-node App creation:**
- Each node gets: temp dir for DB + buckets, unique port number, deterministic keypair.
- `AppConfig` with `testing.accelerate_time = true` (1s close interval).
- `Arc<VirtualClock>` as its clock.
- `LoopbackConnectionFactory` connected to the shared `LoopbackNetwork`.

**Crank implementation** (mirroring `Simulation.cpp:439-537`):
```rust
async fn crank_all_nodes(&self) -> bool {
    let quantum = Duration::from_millis(100);
    let mut did_work = false;

    // Set quantum timer on master clock
    let deadline = self.master_clock.now() + quantum;

    // Crank each node up to the deadline
    for node in self.nodes.values() {
        did_work |= self.crank_node(node, deadline).await;
    }

    // Advance master clock
    self.master_clock.advance(quantum);

    did_work
}

async fn crank_node(&self, node: &SimNode, deadline: Instant) -> bool {
    // Advance node's virtual clock, processing events up to deadline
    node.clock.crank_until_time(deadline).await
}
```

### Phase 5: Topologies Factory (1-2 days)

```rust
pub struct Topologies;

impl Topologies {
    /// 3-node full mesh, threshold 2 — the primary use case
    pub fn core3(mode: SimulationMode) -> Simulation {
        Self::core(3, 0.67, mode)
    }

    /// N-node full mesh with uniform quorum set
    pub fn core(n: usize, threshold_fraction: f64, mode: SimulationMode) -> Simulation {
        let threshold = (n as f64 * threshold_fraction).ceil() as u32;
        let keys: Vec<SecretKey> = (0..n)
            .map(|i| SecretKey::from_seed(sha256(format!("NODE_SEED_{i}"))))
            .collect();

        let quorum_set = ScpQuorumSet {
            threshold,
            validators: keys.iter().map(|k| k.public_key()).collect(),
            inner_sets: vec![],
        };

        let mut sim = Simulation::new(mode);
        for key in &keys {
            sim.add_node(key.clone(), quorum_set.clone(), None);
        }

        // Full mesh connections
        for i in 0..n {
            for j in (i + 1)..n {
                sim.add_pending_connection(
                    keys[i].public_key(),
                    keys[j].public_key(),
                );
            }
        }

        sim
    }

    /// 2-node pair (both in each other's quorum)
    pub fn pair(mode: SimulationMode) -> Simulation {
        Self::core(2, 1.0, mode)
    }

    /// N-node one-way ring
    pub fn cycle(n: usize, threshold: u32, mode: SimulationMode) -> Simulation;

    /// N nodes, same quorum, no connections (test isolated discovery)
    pub fn separate(n: usize, threshold_fraction: f64, mode: SimulationMode) -> Simulation;
}
```

### Phase 6: Tests & Validation (3-5 days)

Test suite in `crates/simulation/tests/`:

#### 6a. Basic consensus

```rust
#[tokio::test]
async fn test_3_nodes_close_10_ledgers() {
    let mut sim = Topologies::core3(SimulationMode::OverLoopback);
    sim.start_all_nodes().await;

    let ok = sim.crank_until(
        |s| s.have_all_externalized(11, 5),
        Duration::from_secs(120),  // virtual time
    ).await;

    assert!(ok, "all 3 nodes should externalize 10 ledgers");
}
```

#### 6b. Degraded quorum (1 node down)

```rust
#[tokio::test]
async fn test_3_nodes_2_running_threshold_2() {
    // Create 3-node quorum set but only start 2 nodes
    let keys = generate_keys(3);
    let qset = quorum_set(&keys, 2);
    let mut sim = Simulation::new(SimulationMode::OverLoopback);
    sim.add_node(keys[0].clone(), qset.clone(), None);
    sim.add_node(keys[1].clone(), qset.clone(), None);
    sim.add_pending_connection(keys[0].public_key(), keys[1].public_key());
    sim.start_all_nodes().await;

    let ok = sim.crank_until(
        |s| s.have_all_externalized(11, 5),
        Duration::from_secs(120),
    ).await;

    assert!(ok, "2 of 3 nodes should reach consensus with threshold 2");
}
```

#### 6c. Network partition and recovery

```rust
#[tokio::test]
async fn test_partition_and_recovery() {
    let mut sim = Topologies::core3(SimulationMode::OverLoopback);
    sim.start_all_nodes().await;

    // Close a few ledgers normally
    sim.crank_until(|s| s.have_all_externalized(5, 1), Duration::from_secs(30)).await;

    // Partition node 2
    let node2 = sim.node_ids()[2];
    sim.network().partition(node2);

    // Nodes 0 and 1 should still close (threshold 2)
    sim.crank_until(|s| s.ledger_seq(&s.node_ids()[0]) >= 10, Duration::from_secs(60)).await;
    assert!(sim.ledger_seq(&node2) < 10, "partitioned node should be behind");

    // Heal partition
    sim.network().heal_partition(node2);

    // All nodes should catch up and converge
    sim.crank_until(|s| s.have_all_externalized(15, 2), Duration::from_secs(120)).await;
    assert!(sim.have_all_externalized(15, 2));
}
```

#### 6d. Deterministic replay

```rust
#[tokio::test]
async fn test_deterministic_replay() {
    // Run the same simulation twice
    let hash1 = run_simulation_and_get_ledger_hash(10).await;
    let hash2 = run_simulation_and_get_ledger_hash(10).await;
    assert_eq!(hash1, hash2, "deterministic simulation should produce identical results");
}
```

#### 6e. Message loss resilience

```rust
#[tokio::test]
async fn test_message_loss() {
    let mut sim = Topologies::core3(SimulationMode::OverLoopback);
    sim.start_all_nodes().await;

    // Set 10% message drop on all links
    for (a, b) in sim.all_links() {
        sim.network().set_drop_prob(a, b, 0.10);
    }

    // Should still reach consensus (slower)
    let ok = sim.crank_until(
        |s| s.have_all_externalized(11, 5),
        Duration::from_secs(300),  // more time allowed
    ).await;

    assert!(ok, "consensus should survive 10% message loss");
}
```

## Estimated Effort

| Phase | Days | Risk |
|-------|------|------|
| 1. Clock abstraction (`crates/clock/`) | 3-5 | Low |
| 2. Clock injection across codebase | 5-8 | **High** — many files, regression risk |
| 3. Transport abstraction + loopback | 5-7 | Medium — overlay refactor |
| 4. Simulation harness (`crates/simulation/`) | 4-6 | Low — new code on top of phases 1-3 |
| 5. Topologies factory | 1-2 | Low |
| 6. Tests & validation | 3-5 | Medium — integration debugging |
| **Total** | **21-33** | |

## Key Design Decisions

1. **Clock approach:** Start with `tokio/test-util` (`pause()` + `advance()`) for tokio timers, plus a custom `virtual_now` field for `std::time::Instant` replacements. Upgrade to a fully custom event queue only if tokio's test-util proves insufficient.

2. **Observational time:** Leave perf-timing `Instant::now()` calls in `ledger/manager.rs`, `app/logging.rs`, and HTTP handlers as bare `std::time::Instant`. They don't affect determinism.

3. **Transport abstraction level:** Operate at the byte-buffer level (like stellar-core's `LoopbackPeer`), not at the `StellarMessage` level. This tests the full serialization path.

4. **Fault injection parity:** Match stellar-core's `LoopbackPeer` capabilities: drop, reorder, damage, cork (partition), straggle (slow node).

5. **`App` construction for simulation:** Each node gets a temp directory with in-memory SQLite, unique port, deterministic keypair. `AppConfig::testing.accelerate_time = true` for 1-second close intervals.

## Risks & Mitigations

| Risk | Mitigation |
|------|-----------|
| Clock injection breaks existing tests | Inject crate by crate, run `cargo test --all` after each |
| Overlay refactor breaks TCP mode | Keep `TcpConnectionFactory` as default; loopback is opt-in |
| tokio test-util insufficient for multi-node | Each node can share the same tokio runtime with paused time; crank advances globally |
| `App` God object makes lightweight construction hard | Create test helpers: `App::new_for_simulation(config, clock, factory)` |
| 3 concurrent `App` instances exhaust resources | Use temp dirs, in-memory SQLite, small bucket caches |

## Future Extensions

Once the foundation is in place:
- **Load generator:** Submit synthetic transactions during simulation
- **Upgrade testing:** Simulate protocol upgrades across the network
- **Hierarchical quorum topologies:** Test tier-1/tier-2 validator configurations
- **Chaos testing:** Random partitions, node restarts, Byzantine behavior
- **Performance benchmarking:** Measure consensus latency under controlled conditions
- **Full custom VirtualClock:** Replace tokio test-util with a custom event queue for complete control
