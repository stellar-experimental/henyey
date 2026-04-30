use std::time::Duration;

use henyey_simulation::{SimulationMode, Topologies};

#[tokio::test]
async fn test_3_nodes_close_10_ledgers() {
    let mut sim = Topologies::core3(SimulationMode::OverLoopback);
    sim.start_all_nodes().await;

    let ok = sim
        .crank_until(|s| s.have_all_externalized(11, 2), Duration::from_secs(30))
        .await;

    assert!(ok, "all nodes should externalize at least 10 ledgers");
}

#[tokio::test]
async fn test_partition_and_recovery() {
    let mut sim = Topologies::core3(SimulationMode::OverLoopback);
    sim.start_all_nodes().await;

    assert!(
        sim.crank_until(|s| s.have_all_externalized(5, 2), Duration::from_secs(20))
            .await,
        "nodes should externalize to ledger 5 before partitioning"
    );

    let ids = sim.node_ids();
    let node2 = ids[2].clone();
    sim.partition(&node2);

    assert!(
        sim.crank_until(|s| s.ledger_seq(&ids[0]) >= 8, Duration::from_secs(20))
            .await,
        "non-partitioned nodes should advance to ledger 8"
    );
    assert!(sim.ledger_seq(&node2) < 8);

    sim.heal_partition(&node2);
    let ok = sim
        .crank_until(|s| s.have_all_externalized(10, 2), Duration::from_secs(30))
        .await;
    assert!(ok);
}

#[tokio::test]
async fn test_deterministic_replay() {
    async fn run_once() -> Vec<[u8; 32]> {
        let mut sim = Topologies::core3(SimulationMode::OverLoopback);
        sim.start_all_nodes().await;
        assert!(
            sim.crank_until(|s| s.have_all_externalized(11, 2), Duration::from_secs(30))
                .await,
            "nodes should externalize at least 10 ledgers for replay"
        );
        sim.ledger_hashes().into_iter().map(|h| h.0).collect()
    }

    let h1 = run_once().await;
    let h2 = run_once().await;
    assert_eq!(h1, h2, "simulation replay should be deterministic");
}

#[tokio::test]
async fn test_message_loss() {
    let mut sim = Topologies::core3(SimulationMode::OverLoopback);
    sim.start_all_nodes().await;

    for (a, b) in sim.all_links() {
        sim.set_drop_prob(&a, &b, 0.10);
    }

    let ok = sim
        .crank_until(|s| s.have_all_externalized(11, 3), Duration::from_secs(60))
        .await;
    assert!(ok, "consensus should survive bounded message loss");
}

#[tokio::test]
async fn test_cycle_topology_converges() {
    let mut sim = Topologies::cycle(5, SimulationMode::OverLoopback);
    sim.start_all_nodes().await;

    let ok = sim
        .crank_until(|s| s.have_all_externalized(9, 2), Duration::from_secs(30))
        .await;
    assert!(ok);
}

#[tokio::test]
async fn test_separate_topology_not_fully_connected() {
    let mut sim = Topologies::separate(SimulationMode::OverLoopback);
    sim.start_all_nodes().await;

    assert!(!sim.is_fully_connected());

    let progressed = sim
        .crank_until(
            |s| s.ledger_seq("node0") >= 4 && s.ledger_seq("node2") >= 4,
            Duration::from_secs(20),
        )
        .await;
    assert!(progressed);
}

#[tokio::test]
async fn test_additional_topology_builders_exist() {
    let cycle4 = Topologies::cycle4(SimulationMode::OverLoopback);
    assert_eq!(cycle4.node_ids().len(), 4);

    let branched = Topologies::branchedcycle(5, SimulationMode::OverLoopback);
    assert_eq!(branched.node_ids().len(), 5);

    let hierarchical = Topologies::hierarchical_quorum(2, SimulationMode::OverLoopback);
    assert!(hierarchical.node_ids().len() >= 6);

    let simplified = Topologies::hierarchical_quorum_simplified(4, 3, SimulationMode::OverLoopback);
    assert_eq!(simplified.node_ids().len(), 7);

    let custom = Topologies::custom_a(SimulationMode::OverLoopback);
    assert_eq!(custom.node_ids().len(), 7);

    let asymmetric = Topologies::asymmetric(SimulationMode::OverLoopback);
    assert_eq!(asymmetric.node_ids().len(), 7);
}

#[tokio::test]
async fn test_populate_app_nodes_from_existing() {
    let mut sim = Topologies::core3(SimulationMode::OverTcp);
    sim.populate_app_nodes_from_existing_with_quorum_adjuster(67, |id, mut qset| {
        if id == "node0" {
            qset.threshold_percent = 100;
        }
        qset
    });

    assert_eq!(sim.app_node_ids(), vec!["node0", "node1", "node2"]);

    let plan = sim.generate_load_plan_for_app_nodes(2, 3, 100, 10);
    assert_eq!(plan.len(), 3);
    assert_eq!(plan.iter().map(|s| s.transactions.len()).sum::<usize>(), 6);
}
