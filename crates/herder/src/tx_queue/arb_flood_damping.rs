//! Arbitrage flood damping.
//!
//! Detects transactions containing path-payment loops (potential arbitrage)
//! and probabilistically dampens their broadcast to reduce flooding pressure.
//!
//! Mirrors stellar-core's `allowTxBroadcast()` and
//! `findAllAssetPairsInvolvedInPaymentLoops()` in `TransactionQueue.cpp`.

use std::collections::{HashMap, HashSet};

use henyey_ledger::offer::AssetPair;
use rand::distributions::Distribution;
use rand::rngs::StdRng;
use rand::SeedableRng;
use stellar_xdr::{Asset, Operation, OperationBody};

// ---------------------------------------------------------------------------
// Loop detection
// ---------------------------------------------------------------------------

/// Find all directed asset pairs that participate in payment loops (cycles)
/// within the given operations.
///
/// Builds a directed graph of asset edges from `PathPaymentStrictReceive` and
/// `PathPaymentStrictSend` operations, then uses Tarjan's SCC algorithm to
/// find cycles. Returns every directed `(src, dst)` edge within a non-trivial
/// strongly connected component (size > 1).
///
/// Mirrors stellar-core `findAllAssetPairsInvolvedInPaymentLoops`.
pub fn find_all_asset_pairs_in_payment_loops(ops: &[Operation]) -> Vec<AssetPair> {
    let mut asset_to_num: HashMap<AssetKey, usize> = HashMap::new();
    let mut num_to_asset: Vec<Asset> = Vec::new();

    // Collect all directed edges from path payment operations.
    let mut edges: Vec<(Asset, Asset)> = Vec::new();
    for op in ops {
        match &op.body {
            OperationBody::PathPaymentStrictReceive(pop) => {
                collect_segment_edges(&pop.send_asset, &pop.dest_asset, &pop.path, &mut edges);
            }
            OperationBody::PathPaymentStrictSend(pop) => {
                collect_segment_edges(&pop.send_asset, &pop.dest_asset, &pop.path, &mut edges);
            }
            _ => continue,
        }
    }

    // Intern all assets referenced by edges.
    for (src, dst) in &edges {
        intern_asset(&mut asset_to_num, &mut num_to_asset, src);
        intern_asset(&mut asset_to_num, &mut num_to_asset, dst);
    }

    // Build adjacency graph.
    let mut graph: Vec<HashSet<usize>> = vec![HashSet::new(); num_to_asset.len()];
    for (src, dst) in &edges {
        let si = asset_to_num[&AssetKey::from(src)];
        let di = asset_to_num[&AssetKey::from(dst)];
        graph[si].insert(di);
    }

    if graph.is_empty() {
        return Vec::new();
    }

    // Find SCCs via Tarjan's algorithm
    let sccs = tarjan_scc(graph.len(), &graph);

    // Collect all edges within non-trivial SCCs
    let mut result = Vec::new();
    for scc in &sccs {
        if scc.len() <= 1 {
            continue;
        }
        for &src in scc {
            for &dst in &graph[src] {
                if scc.contains(&dst) {
                    result.push(AssetPair::new(
                        num_to_asset[src].clone(),
                        num_to_asset[dst].clone(),
                    ));
                }
            }
        }
    }
    result
}

/// Collect directed edges for a path payment segment: src → path[0] → ... → dst.
fn collect_segment_edges(
    src: &Asset,
    dst: &Asset,
    path: &[Asset],
    edges: &mut Vec<(Asset, Asset)>,
) {
    let mut prev = src;
    for asset in path {
        edges.push((prev.clone(), asset.clone()));
        prev = asset;
    }
    edges.push((prev.clone(), dst.clone()));
}

/// Intern an asset into the numbering maps, returning its index.
fn intern_asset(
    asset_to_num: &mut HashMap<AssetKey, usize>,
    num_to_asset: &mut Vec<Asset>,
    asset: &Asset,
) -> usize {
    let key = AssetKey::from(asset);
    let n = num_to_asset.len();
    *asset_to_num.entry(key).or_insert_with(|| {
        num_to_asset.push(asset.clone());
        n
    })
}

// ---------------------------------------------------------------------------
// Asset key for interning (XDR-based equality)
// ---------------------------------------------------------------------------

/// A wrapper around Asset that provides Hash + Eq based on XDR encoding,
/// used only for the intern map during loop detection.
#[derive(Clone)]
struct AssetKey(Vec<u8>);

impl AssetKey {
    fn from(asset: &Asset) -> Self {
        use stellar_xdr::{Limits, WriteXdr};
        Self(
            asset
                .to_xdr(Limits::none())
                .expect("Asset XDR encoding should not fail"),
        )
    }
}

impl std::hash::Hash for AssetKey {
    fn hash<H: std::hash::Hasher>(&self, state: &mut H) {
        self.0.hash(state);
    }
}

impl PartialEq for AssetKey {
    fn eq(&self, other: &Self) -> bool {
        self.0 == other.0
    }
}

impl Eq for AssetKey {}

// ---------------------------------------------------------------------------
// Tarjan SCC (local, minimal)
// ---------------------------------------------------------------------------

/// Compute strongly connected components using Tarjan's algorithm.
///
/// Returns a `Vec<HashSet<usize>>` where each set is one SCC.
fn tarjan_scc(num_nodes: usize, graph: &[HashSet<usize>]) -> Vec<HashSet<usize>> {
    struct State {
        index: Vec<Option<usize>>,
        low_link: Vec<usize>,
        on_stack: Vec<bool>,
        stack: Vec<usize>,
        current_index: usize,
        sccs: Vec<HashSet<usize>>,
    }

    fn strongconnect(v: usize, graph: &[HashSet<usize>], state: &mut State) {
        state.index[v] = Some(state.current_index);
        state.low_link[v] = state.current_index;
        state.current_index += 1;
        state.stack.push(v);
        state.on_stack[v] = true;

        for &w in &graph[v] {
            if state.index[w].is_none() {
                strongconnect(w, graph, state);
                state.low_link[v] = state.low_link[v].min(state.low_link[w]);
            } else if state.on_stack[w] {
                state.low_link[v] = state.low_link[v].min(state.index[w].unwrap());
            }
        }

        if state.low_link[v] == state.index[v].unwrap() {
            let mut scc = HashSet::new();
            loop {
                let w = state.stack.pop().unwrap();
                state.on_stack[w] = false;
                scc.insert(w);
                if w == v {
                    break;
                }
            }
            state.sccs.push(scc);
        }
    }

    let mut state = State {
        index: vec![None; num_nodes],
        low_link: vec![0; num_nodes],
        on_stack: vec![false; num_nodes],
        stack: Vec::new(),
        current_index: 0,
        sccs: Vec::new(),
    };

    for v in 0..num_nodes {
        if state.index[v].is_none() {
            strongconnect(v, graph, &mut state);
        }
    }

    state.sccs
}

// ---------------------------------------------------------------------------
// Arbitrage flood damper
// ---------------------------------------------------------------------------

/// Result of an arbitrage broadcast check.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ArbBroadcastResult {
    /// Transaction does not contain payment loops, or feature is disabled.
    NotArb,
    /// Transaction contains payment loops and was allowed through.
    /// Counters were incremented.
    Allowed,
    /// Transaction contains payment loops and was dampened (not broadcast).
    Dampened,
}

/// Probabilistic flood damper for arbitrage transactions.
///
/// Mirrors stellar-core's `mArbitrageFloodDamping` + `allowTxBroadcast()`
/// in `ClassicTransactionQueue`.
///
/// The damper tracks per-asset-pair broadcast counters. The first
/// `base_allowance` broadcasts for any pair are allowed unconditionally.
/// Beyond that, each additional broadcast is allowed with geometrically
/// decreasing probability controlled by `damping_factor`.
pub struct ArbitrageFloodDamper {
    pub(crate) damping_map: HashMap<AssetPair, u32>,
    base_allowance: i32,
    damping_factor: f64,
    rng: StdRng,
}

impl ArbitrageFloodDamper {
    /// Create a new damper with the given configuration.
    ///
    /// - `base_allowance`: Number of unconditional broadcasts per asset pair
    ///   per ledger. Set to `-1` to disable damping entirely.
    /// - `damping_factor`: Probability parameter for the geometric distribution
    ///   used beyond the allowance. Must be in `(0.0, 1.0]`.
    pub fn new(base_allowance: i32, damping_factor: f64) -> Self {
        Self {
            damping_map: HashMap::new(),
            base_allowance,
            damping_factor,
            rng: StdRng::from_entropy(),
        }
    }

    /// Create a damper with a fixed RNG seed for deterministic testing.
    #[cfg(test)]
    pub fn new_seeded(base_allowance: i32, damping_factor: f64, seed: u64) -> Self {
        Self {
            damping_map: HashMap::new(),
            base_allowance,
            damping_factor,
            rng: StdRng::seed_from_u64(seed),
        }
    }

    /// Check whether a transaction should be broadcast, applying arbitrage
    /// flood damping if it contains payment loops.
    ///
    /// Returns `NotArb` if the transaction has no payment loops or damping
    /// is disabled. Returns `Allowed` if the transaction passes (counters
    /// are incremented immediately). Returns `Dampened` if the transaction
    /// is probabilistically suppressed.
    ///
    /// Mirrors stellar-core `ClassicTransactionQueue::allowTxBroadcast`.
    pub fn allow_tx_broadcast(&mut self, ops: &[Operation]) -> ArbBroadcastResult {
        if self.base_allowance < 0 {
            return ArbBroadcastResult::NotArb;
        }

        let arb_pairs = find_all_asset_pairs_in_payment_loops(ops);
        if arb_pairs.is_empty() {
            return ArbBroadcastResult::NotArb;
        }

        let allowance = self.base_allowance as u32;

        // Find the maximum broadcast count across all pairs on the path,
        // emplacing 0 for new pairs (matching upstream).
        let mut max_broadcast: u32 = 0;
        for pair in &arb_pairs {
            let count = self.damping_map.entry(pair.clone()).or_insert(0);
            max_broadcast = max_broadcast.max(*count);
        }

        // Admit unconditionally if no pair has hit the allowance.
        let mut allow = max_broadcast < allowance;

        // Beyond the allowance, dampen transmission randomly using a
        // geometric distribution.
        if !allow {
            let k = max_broadcast - allowance;
            let geo = GeometricDistribution::new(self.damping_factor);
            let sample: u32 = geo.sample(&mut self.rng);
            allow = sample >= k;
        }

        if allow {
            // Bump all pairs on the path.
            for pair in &arb_pairs {
                if let Some(count) = self.damping_map.get_mut(pair) {
                    *count += 1;
                }
            }
            ArbBroadcastResult::Allowed
        } else {
            ArbBroadcastResult::Dampened
        }
    }

    /// Clear all damping state. Called once per ledger close (from `shift()`).
    pub fn clear(&mut self) {
        self.damping_map.clear();
    }
}

// ---------------------------------------------------------------------------
// Geometric distribution matching C++ std::geometric_distribution
// ---------------------------------------------------------------------------

/// A geometric distribution matching C++ `std::geometric_distribution<uint32_t>`.
///
/// C++ `std::geometric_distribution(p)` models the number of failures before
/// the first success, where each trial succeeds with probability `p`.
/// P(X = k) = (1 - p)^k * p  for k = 0, 1, 2, ...
struct GeometricDistribution {
    p: f64,
}

impl GeometricDistribution {
    fn new(p: f64) -> Self {
        debug_assert!(p > 0.0 && p <= 1.0);
        Self { p }
    }
}

impl Distribution<u32> for GeometricDistribution {
    fn sample<R: rand::Rng + ?Sized>(&self, rng: &mut R) -> u32 {
        if self.p >= 1.0 {
            return 0;
        }
        // Standard inversion method: floor(ln(U) / ln(1 - p))
        // where U is uniform in (0, 1).
        let u: f64 = rng.gen_range(f64::MIN_POSITIVE..1.0);
        let result = (u.ln() / (1.0 - self.p).ln()).floor();
        // Clamp to u32 range
        if result >= u32::MAX as f64 {
            u32::MAX
        } else {
            result as u32
        }
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use stellar_xdr::{
        AlphaNum4, AssetCode4, PathPaymentStrictReceiveOp, PathPaymentStrictSendOp, Uint256, VecM,
    };

    fn make_asset(code: &[u8; 4]) -> Asset {
        Asset::CreditAlphanum4(AlphaNum4 {
            asset_code: AssetCode4(*code),
            issuer: stellar_xdr::AccountId(stellar_xdr::PublicKey::PublicKeyTypeEd25519(Uint256(
                [0u8; 32],
            ))),
        })
    }

    fn native() -> Asset {
        Asset::Native
    }

    fn make_path_payment_strict_receive(send: Asset, dest: Asset, path: Vec<Asset>) -> Operation {
        Operation {
            source_account: None,
            body: OperationBody::PathPaymentStrictReceive(PathPaymentStrictReceiveOp {
                send_asset: send,
                send_max: 1_000_000,
                destination: stellar_xdr::MuxedAccount::Ed25519(Uint256([0u8; 32])),
                dest_asset: dest,
                dest_amount: 1,
                path: path.try_into().unwrap_or_else(|_| VecM::default()),
            }),
        }
    }

    fn make_path_payment_strict_send(send: Asset, dest: Asset, path: Vec<Asset>) -> Operation {
        Operation {
            source_account: None,
            body: OperationBody::PathPaymentStrictSend(PathPaymentStrictSendOp {
                send_asset: send,
                send_amount: 1_000_000,
                destination: stellar_xdr::MuxedAccount::Ed25519(Uint256([0u8; 32])),
                dest_asset: dest,
                dest_min: 1,
                path: path.try_into().unwrap_or_else(|_| VecM::default()),
            }),
        }
    }

    fn make_inflation_op() -> Operation {
        Operation {
            source_account: None,
            body: OperationBody::Inflation,
        }
    }

    // -----------------------------------------------------------------------
    // Loop detection tests
    // -----------------------------------------------------------------------

    #[test]
    fn test_no_path_payments() {
        let ops = vec![make_inflation_op()];
        assert!(find_all_asset_pairs_in_payment_loops(&ops).is_empty());
    }

    #[test]
    fn test_empty_ops() {
        assert!(find_all_asset_pairs_in_payment_loops(&[]).is_empty());
    }

    #[test]
    fn test_linear_path_no_loop() {
        // A → B → C (no cycle)
        let a = make_asset(b"AAAA");
        let b = make_asset(b"BBBB");
        let c = make_asset(b"CCCC");
        let ops = vec![make_path_payment_strict_receive(a, c, vec![b])];
        assert!(find_all_asset_pairs_in_payment_loops(&ops).is_empty());
    }

    #[test]
    fn test_simple_cycle_a_b() {
        // Op1: A → B, Op2: B → A => cycle
        let a = make_asset(b"AAAA");
        let b = make_asset(b"BBBB");
        let ops = vec![
            make_path_payment_strict_receive(a.clone(), b.clone(), vec![]),
            make_path_payment_strict_receive(b, a, vec![]),
        ];
        let pairs = find_all_asset_pairs_in_payment_loops(&ops);
        assert_eq!(pairs.len(), 2);
    }

    #[test]
    fn test_triangle_cycle() {
        // A → B → C → A
        let a = make_asset(b"AAAA");
        let b = make_asset(b"BBBB");
        let c = make_asset(b"CCCC");
        let ops = vec![
            make_path_payment_strict_receive(a.clone(), b.clone(), vec![]),
            make_path_payment_strict_receive(b, c.clone(), vec![]),
            make_path_payment_strict_receive(c, a, vec![]),
        ];
        let pairs = find_all_asset_pairs_in_payment_loops(&ops);
        // 3 directed edges in the triangle
        assert_eq!(pairs.len(), 3);
    }

    #[test]
    fn test_triangle_via_path() {
        // Single op: A →[B]→ C plus C → A forms a triangle
        let a = make_asset(b"AAAA");
        let b = make_asset(b"BBBB");
        let c = make_asset(b"CCCC");
        let ops = vec![
            make_path_payment_strict_receive(a.clone(), c.clone(), vec![b]),
            make_path_payment_strict_receive(c, a, vec![]),
        ];
        let pairs = find_all_asset_pairs_in_payment_loops(&ops);
        assert_eq!(pairs.len(), 3);
    }

    #[test]
    fn test_dual_independent_loops() {
        // Loop 1: A ↔ B, Loop 2: C ↔ D
        let a = make_asset(b"AAAA");
        let b = make_asset(b"BBBB");
        let c = make_asset(b"CCCC");
        let d = make_asset(b"DDDD");
        let ops = vec![
            make_path_payment_strict_receive(a.clone(), b.clone(), vec![]),
            make_path_payment_strict_receive(b, a, vec![]),
            make_path_payment_strict_send(c.clone(), d.clone(), vec![]),
            make_path_payment_strict_send(d, c, vec![]),
        ];
        let pairs = find_all_asset_pairs_in_payment_loops(&ops);
        // 2 edges per loop × 2 loops = 4
        assert_eq!(pairs.len(), 4);
    }

    #[test]
    fn test_mixed_ops_only_path_payments_form_graph() {
        // Inflation + path payment cycle
        let a = make_asset(b"AAAA");
        let b = make_asset(b"BBBB");
        let ops = vec![
            make_inflation_op(),
            make_path_payment_strict_receive(a.clone(), b.clone(), vec![]),
            make_inflation_op(),
            make_path_payment_strict_send(b, a, vec![]),
        ];
        let pairs = find_all_asset_pairs_in_payment_loops(&ops);
        assert_eq!(pairs.len(), 2);
    }

    #[test]
    fn test_long_path_cycle() {
        // A → B → C → D → E → A (via path)
        let a = make_asset(b"AAAA");
        let b = make_asset(b"BBBB");
        let c = make_asset(b"CCCC");
        let d = make_asset(b"DDDD");
        let e = make_asset(b"EEEE");
        let ops = vec![make_path_payment_strict_receive(
            a.clone(),
            a.clone(),
            vec![b, c, d, e],
        )];
        let pairs = find_all_asset_pairs_in_payment_loops(&ops);
        // 5 assets in a cycle: 5 directed edges
        assert_eq!(pairs.len(), 5);
    }

    #[test]
    fn test_self_loop_excluded() {
        // A → A should NOT form a non-trivial SCC (it's a self-loop, SCC size 1)
        let a = make_asset(b"AAAA");
        let ops = vec![make_path_payment_strict_receive(a.clone(), a, vec![])];
        let pairs = find_all_asset_pairs_in_payment_loops(&ops);
        // Self-loop: SCC size 1, excluded
        assert!(pairs.is_empty());
    }

    #[test]
    fn test_edge_dedup() {
        // Same A → B edge in two ops; graph deduplicates
        let a = make_asset(b"AAAA");
        let b = make_asset(b"BBBB");
        let ops = vec![
            make_path_payment_strict_receive(a.clone(), b.clone(), vec![]),
            make_path_payment_strict_receive(a.clone(), b.clone(), vec![]),
            make_path_payment_strict_receive(b, a, vec![]),
        ];
        let pairs = find_all_asset_pairs_in_payment_loops(&ops);
        // Deduplicated: only 2 directed edges (A→B, B→A)
        assert_eq!(pairs.len(), 2);
    }

    #[test]
    fn test_native_asset_in_loop() {
        // XLM → A → XLM
        let a = make_asset(b"AAAA");
        let ops = vec![
            make_path_payment_strict_receive(native(), a.clone(), vec![]),
            make_path_payment_strict_receive(a, native(), vec![]),
        ];
        let pairs = find_all_asset_pairs_in_payment_loops(&ops);
        assert_eq!(pairs.len(), 2);
    }

    #[test]
    fn test_strict_send_forms_loop() {
        let a = make_asset(b"AAAA");
        let b = make_asset(b"BBBB");
        let ops = vec![
            make_path_payment_strict_send(a.clone(), b.clone(), vec![]),
            make_path_payment_strict_send(b, a, vec![]),
        ];
        let pairs = find_all_asset_pairs_in_payment_loops(&ops);
        assert_eq!(pairs.len(), 2);
    }

    // -----------------------------------------------------------------------
    // Damper tests
    // -----------------------------------------------------------------------

    #[test]
    fn test_damper_disabled() {
        let mut damper = ArbitrageFloodDamper::new_seeded(-1, 0.8, 42);
        let a = make_asset(b"AAAA");
        let b = make_asset(b"BBBB");
        let ops = vec![
            make_path_payment_strict_receive(a.clone(), b.clone(), vec![]),
            make_path_payment_strict_receive(b, a, vec![]),
        ];
        assert_eq!(damper.allow_tx_broadcast(&ops), ArbBroadcastResult::NotArb);
    }

    #[test]
    fn test_damper_non_arb_tx() {
        let mut damper = ArbitrageFloodDamper::new_seeded(5, 0.8, 42);
        let ops = vec![make_inflation_op()];
        assert_eq!(damper.allow_tx_broadcast(&ops), ArbBroadcastResult::NotArb);
    }

    #[test]
    fn test_damper_within_allowance() {
        let mut damper = ArbitrageFloodDamper::new_seeded(3, 0.8, 42);
        let a = make_asset(b"AAAA");
        let b = make_asset(b"BBBB");
        let ops = vec![
            make_path_payment_strict_receive(a.clone(), b.clone(), vec![]),
            make_path_payment_strict_receive(b, a, vec![]),
        ];
        // First 3 should all be allowed
        for _ in 0..3 {
            assert_eq!(damper.allow_tx_broadcast(&ops), ArbBroadcastResult::Allowed);
        }
    }

    #[test]
    fn test_damper_beyond_allowance_some_dampened() {
        let mut damper = ArbitrageFloodDamper::new_seeded(2, 0.8, 42);
        let a = make_asset(b"AAAA");
        let b = make_asset(b"BBBB");
        let ops = vec![
            make_path_payment_strict_receive(a.clone(), b.clone(), vec![]),
            make_path_payment_strict_receive(b, a, vec![]),
        ];
        // First 2 always allowed
        assert_eq!(damper.allow_tx_broadcast(&ops), ArbBroadcastResult::Allowed);
        assert_eq!(damper.allow_tx_broadcast(&ops), ArbBroadcastResult::Allowed);

        // Beyond allowance: with deterministic seed, check that some are dampened
        let mut allowed = 0;
        let mut dampened = 0;
        for _ in 0..20 {
            match damper.allow_tx_broadcast(&ops) {
                ArbBroadcastResult::Allowed => allowed += 1,
                ArbBroadcastResult::Dampened => dampened += 1,
                _ => panic!("unexpected result"),
            }
        }
        // With high damping factor (0.8), we expect both allowed and dampened
        assert!(dampened > 0, "expected some dampened txs, got all allowed");
        assert!(allowed > 0, "expected some allowed txs, got all dampened");
    }

    #[test]
    fn test_damper_clear_resets() {
        let mut damper = ArbitrageFloodDamper::new_seeded(1, 0.8, 42);
        let a = make_asset(b"AAAA");
        let b = make_asset(b"BBBB");
        let ops = vec![
            make_path_payment_strict_receive(a.clone(), b.clone(), vec![]),
            make_path_payment_strict_receive(b, a, vec![]),
        ];
        // Use up the allowance
        assert_eq!(damper.allow_tx_broadcast(&ops), ArbBroadcastResult::Allowed);

        // Clear resets counters
        damper.clear();

        // Should be allowed again (within fresh allowance)
        assert_eq!(damper.allow_tx_broadcast(&ops), ArbBroadcastResult::Allowed);
    }

    #[test]
    fn test_damper_max_across_pairs() {
        // If one pair has been seen a lot, it affects new pairs sharing
        // an edge with it.
        let mut damper = ArbitrageFloodDamper::new_seeded(2, 0.8, 42);
        let a = make_asset(b"AAAA");
        let b = make_asset(b"BBBB");
        let c = make_asset(b"CCCC");

        // A ↔ B loop: use up allowance
        let ops_ab = vec![
            make_path_payment_strict_receive(a.clone(), b.clone(), vec![]),
            make_path_payment_strict_receive(b.clone(), a.clone(), vec![]),
        ];
        damper.allow_tx_broadcast(&ops_ab);
        damper.allow_tx_broadcast(&ops_ab);

        // Now A ↔ C loop: A→B already at 2 (max), but A→C and C→A are new (0).
        // The max across all pairs on the new path is max(A→C=0, C→A=0) = 0.
        // So it should still be within allowance.
        let ops_ac = vec![
            make_path_payment_strict_receive(a.clone(), c.clone(), vec![]),
            make_path_payment_strict_receive(c, a, vec![]),
        ];
        assert_eq!(
            damper.allow_tx_broadcast(&ops_ac),
            ArbBroadcastResult::Allowed
        );
    }

    #[test]
    fn test_damper_counters_increment_on_allow() {
        let mut damper = ArbitrageFloodDamper::new_seeded(5, 0.8, 42);
        let a = make_asset(b"AAAA");
        let b = make_asset(b"BBBB");
        let ops = vec![
            make_path_payment_strict_receive(a.clone(), b.clone(), vec![]),
            make_path_payment_strict_receive(b.clone(), a.clone(), vec![]),
        ];

        // Each allowed broadcast should increment counters
        damper.allow_tx_broadcast(&ops);
        let pair_ab = AssetPair::new(a.clone(), b.clone());
        let pair_ba = AssetPair::new(b, a);
        assert_eq!(damper.damping_map[&pair_ab], 1);
        assert_eq!(damper.damping_map[&pair_ba], 1);

        damper.allow_tx_broadcast(&ops);
        assert_eq!(damper.damping_map[&pair_ab], 2);
        assert_eq!(damper.damping_map[&pair_ba], 2);
    }

    // -----------------------------------------------------------------------
    // Tarjan SCC tests
    // -----------------------------------------------------------------------

    #[test]
    fn test_tarjan_empty_graph() {
        let sccs = tarjan_scc(0, &[]);
        assert!(sccs.is_empty());
    }

    #[test]
    fn test_tarjan_single_node_no_edge() {
        let graph = vec![HashSet::new()];
        let sccs = tarjan_scc(1, &graph);
        assert_eq!(sccs.len(), 1);
        assert_eq!(sccs[0].len(), 1); // trivial SCC
    }

    #[test]
    fn test_tarjan_two_node_cycle() {
        let mut graph = vec![HashSet::new(), HashSet::new()];
        graph[0].insert(1);
        graph[1].insert(0);
        let sccs = tarjan_scc(2, &graph);
        let big: Vec<_> = sccs.iter().filter(|s| s.len() > 1).collect();
        assert_eq!(big.len(), 1);
        assert!(big[0].contains(&0));
        assert!(big[0].contains(&1));
    }

    #[test]
    fn test_tarjan_no_cycle() {
        let mut graph = vec![HashSet::new(), HashSet::new()];
        graph[0].insert(1);
        let sccs = tarjan_scc(2, &graph);
        // All trivial SCCs
        assert!(sccs.iter().all(|s| s.len() == 1));
    }

    // -----------------------------------------------------------------------
    // Geometric distribution test
    // -----------------------------------------------------------------------

    #[test]
    fn test_geometric_distribution_basic() {
        let dist = GeometricDistribution::new(0.8);
        let mut rng = StdRng::seed_from_u64(42);
        let mut samples = Vec::new();
        for _ in 0..1000 {
            samples.push(dist.sample(&mut rng));
        }
        // With p=0.8, mean = (1-p)/p = 0.25, most samples should be 0
        let zeros = samples.iter().filter(|&&x| x == 0).count();
        assert!(
            zeros > 500,
            "expected majority zeros with p=0.8, got {zeros}"
        );
    }

    #[test]
    fn test_geometric_distribution_p_one() {
        let dist = GeometricDistribution::new(1.0);
        let mut rng = StdRng::seed_from_u64(42);
        // p=1.0 should always return 0
        for _ in 0..100 {
            assert_eq!(dist.sample(&mut rng), 0);
        }
    }
}
