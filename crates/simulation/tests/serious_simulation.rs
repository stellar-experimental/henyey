use std::collections::HashMap;
use std::time::Duration;

use henyey_common::Hash256;
use henyey_simulation::{Simulation, SimulationMode, Topologies};

async fn run_core7_fault_schedule() -> (Simulation, Vec<Hash256>) {
    let mut sim = Topologies::core(7, SimulationMode::OverLoopback);
    sim.start_all_nodes().await;

    let converged_20 = sim
        .crank_until(
            |s| s.have_all_externalized(20.into(), 2),
            Duration::from_secs(60),
        )
        .await;
    assert!(converged_20, "core7 should converge to ledger 20");

    sim.partition("node6");
    let progressed_with_partition = sim
        .crank_until(
            |s| {
                let progressed = (0..6).all(|i| s.ledger_seq(&format!("node{}", i)) >= 30);
                progressed && s.ledger_seq("node6") < 30
            },
            Duration::from_secs(60),
        )
        .await;
    assert!(
        progressed_with_partition,
        "majority cluster should progress while one node is partitioned"
    );

    // Temporary hard drops on a subset of links.
    let dropped_edges = [("node0", "node1"), ("node2", "node3"), ("node4", "node5")];
    for (a, b) in dropped_edges {
        sim.set_drop_prob(a, b, 1.0);
    }

    let progressed_with_drops = sim
        .crank_until(
            |s| (0..6).all(|i| s.ledger_seq(&format!("node{}", i)) >= 40),
            Duration::from_secs(60),
        )
        .await;
    assert!(
        progressed_with_drops,
        "cluster should continue progressing under partial hard drops"
    );

    // Heal everything.
    sim.heal_partition("node6");
    for (a, b) in dropped_edges {
        sim.set_drop_prob(a, b, 0.0);
    }

    let converged_60 = sim
        .crank_until(
            |s| s.have_all_externalized(60.into(), 2),
            Duration::from_secs(120),
        )
        .await;
    assert!(
        converged_60,
        "full network should reconverge to ledger 60 after healing"
    );

    let hashes = sim.ledger_hashes();
    (sim, hashes)
}

#[tokio::test]
async fn test_core7_long_run_with_fault_schedule() {
    let (sim, _) = run_core7_fault_schedule().await;

    let final_ledgers: HashMap<String, u32> = sim
        .node_ids()
        .into_iter()
        .map(|id| {
            let seq = sim.ledger_seq(&id).get();
            (id, seq)
        })
        .collect();

    assert!(
        final_ledgers.values().all(|seq| *seq >= 60),
        "all nodes should reach at least ledger 60"
    );
}

#[tokio::test]
async fn test_core7_fault_schedule_replay_is_deterministic() {
    let (_, h1) = run_core7_fault_schedule().await;
    let (_, h2) = run_core7_fault_schedule().await;
    assert_eq!(h1, h2, "serious scenario replay should be deterministic");
}
