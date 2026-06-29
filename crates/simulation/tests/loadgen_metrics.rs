//! Integration test for #3569: henyey's loadgen path must emit the
//! stellar-core `loadgen.*` Prometheus meters so supercluster's
//! `IsLoadGenComplete` can observe henyey-driven loadgen completion.
//!
//! This drives the real `LoadGenerator::generate_load` async loop against a
//! live app-backed standalone validator and asserts that the per-tx meters
//! (`loadgen_txn_attempted`, `loadgen_payment_submitted`, ...) move during the
//! run. It fails on origin/main, where the meters are never emitted (the
//! rendered `/metrics` text contains no `loadgen_*` series at all).
//!
//! The metrics recorder is a process-global singleton, so the test installs it
//! exactly once (via an `OnceLock` guard wrapping the public
//! `henyey_app::metrics::install_recorder()`), runs `#[serial]`, and uses
//! delta-based (`>=`) assertions to stay robust against other simulation tests
//! sharing the same recorder.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, OnceLock};
use std::time::Duration;

use henyey_app::App;
use henyey_common::{deterministic_seed, Hash256, NetworkId};
use henyey_crypto::{sign_hash, SecretKey};
use henyey_simulation::{
    GeneratedLoadConfig, LoadGenMode, LoadGenerator, LoadResult, Simulation, SimulationMode,
};
use henyey_tx::TransactionFrame;
use metrics_exporter_prometheus::PrometheusHandle;
use serial_test::serial;
use stellar_xdr::{
    AccountId, CreateAccountOp, DecoratedSignature, Memo, MuxedAccount, Operation, OperationBody,
    Preconditions, PublicKey, SequenceNumber, Signature, SignatureHint, Transaction,
    TransactionEnvelope, TransactionExt, TransactionV1Envelope, Uint256, VecM,
};

const NETWORK_PASSPHRASE: &str = "Test SDF Network ; September 2015";

/// Install the process-global metrics recorder exactly once and return its
/// render handle. `install_recorder` panics if called twice, so the `OnceLock`
/// guard makes repeated calls (across tests sharing the process) safe.
fn global_recorder() -> &'static PrometheusHandle {
    static HANDLE: OnceLock<PrometheusHandle> = OnceLock::new();
    HANDLE.get_or_init(henyey_app::metrics::install_recorder)
}

/// Parse a single counter value out of a Prometheus exposition text body.
/// Returns 0.0 if the series is absent (which is exactly the on-main failure
/// mode this test guards against).
fn scrape_counter(render: &str, name: &str) -> f64 {
    for line in render.lines() {
        if line.starts_with('#') {
            continue;
        }
        // Unlabeled counter line: "<name> <value>".
        if let Some(rest) = line.strip_prefix(name) {
            if let Some(value) = rest.trim().split_whitespace().next() {
                if rest.starts_with(' ') {
                    return value.parse().unwrap_or(0.0);
                }
            }
        }
    }
    0.0
}

/// Derive the deterministic keypair the loadgen uses for numbered account `id`
/// (`TestAccount-{id}`), matching `TestAccount::from_name`.
fn loadgen_account_secret(id: u64) -> SecretKey {
    SecretKey::from_seed(&deterministic_seed(&format!("TestAccount-{}", id)))
}

fn root_secret() -> SecretKey {
    let network_id = NetworkId::from_passphrase(NETWORK_PASSPHRASE);
    SecretKey::from_seed(network_id.as_bytes())
}

fn sign(mut envelope: TransactionEnvelope, secret: &SecretKey) -> TransactionEnvelope {
    let network_id = NetworkId::from_passphrase(NETWORK_PASSPHRASE);
    let hash = TransactionFrame::hash_envelope(&envelope, &network_id).expect("hash envelope");
    let signature = sign_hash(secret, &hash);
    let pk = secret.public_key();
    let pk_bytes = pk.as_bytes();
    let hint = SignatureHint([pk_bytes[28], pk_bytes[29], pk_bytes[30], pk_bytes[31]]);
    let decorated = DecoratedSignature {
        hint,
        signature: Signature(signature.0.to_vec().try_into().unwrap_or_default()),
    };
    if let TransactionEnvelope::Tx(ref mut env) = envelope {
        env.signatures = vec![decorated].try_into().unwrap_or_default();
    }
    envelope
}

/// Fund loadgen accounts `[0, n)` from the root account in a single tx, then
/// close a ledger so they exist in the bucket list.
async fn fund_loadgen_accounts(sim: &Simulation, app: &Arc<App>, n: u64, balance: i64) {
    let root = root_secret();
    let root_aid = AccountId(PublicKey::PublicKeyTypeEd25519(Uint256(
        *root.public_key().as_bytes(),
    )));
    let root_seq = app
        .load_account_sequence(&root_aid)
        .expect("load root seq")
        .expect("root account exists");

    let mut ops = Vec::new();
    for id in 0..n {
        let dest = loadgen_account_secret(id).public_key();
        ops.push(Operation {
            source_account: None,
            body: OperationBody::CreateAccount(CreateAccountOp {
                destination: AccountId(PublicKey::PublicKeyTypeEd25519(Uint256(*dest.as_bytes()))),
                starting_balance: balance,
            }),
        });
    }

    let tx = Transaction {
        source_account: MuxedAccount::Ed25519(Uint256(*root.public_key().as_bytes())),
        fee: 100 * n as u32,
        seq_num: SequenceNumber(root_seq + 1),
        cond: Preconditions::None,
        memo: Memo::None,
        operations: ops.try_into().expect("ops vec"),
        ext: TransactionExt::V0,
    };
    let envelope = sign(
        TransactionEnvelope::Tx(TransactionV1Envelope {
            tx,
            signatures: VecM::default(),
        }),
        &root,
    );

    let result = app.submit_transaction(envelope).await;
    assert!(
        matches!(result, henyey_herder::TxQueueResult::Added),
        "create-accounts tx must be accepted, got {:?}",
        result
    );

    let target = app.current_ledger_seq() + 1;
    manual_close_until(sim, target, 1, Duration::from_secs(40)).await;
}

/// Minimal manual-close loop for a standalone node (mirrors the helper in
/// `app_simulation.rs`, duplicated here to keep this test self-contained).
async fn manual_close_until(sim: &Simulation, target: u32, _quiet: u32, timeout: Duration) {
    let deadline = std::time::Instant::now() + timeout;
    loop {
        let seq = sim.app("node0").expect("node0 app").current_ledger_seq();
        if seq >= target {
            return;
        }
        let _ = sim.manual_close_app_node("node0").await;
        if std::time::Instant::now() > deadline {
            panic!(
                "manual_close_until: did not reach ledger {} (at {})",
                target, seq
            );
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
}

#[tokio::test]
#[serial]
async fn test_generate_load_increments_loadgen_meters() {
    let handle = global_recorder();

    // Standalone single-node validator (threshold 100%, self-quorum) — closes
    // ledgers without peers.
    let mut sim = Simulation::with_network(SimulationMode::OverTcp, NETWORK_PASSPHRASE);
    let seed = Hash256::hash(b"LOADGEN_METRICS_3569");
    let secret = SecretKey::from_seed(&seed.0);
    let quorum_set = henyey_app::config::QuorumSetConfig {
        threshold_percent: 100,
        validators: vec![secret.public_key().to_strkey()],
        inner_sets: Vec::new(),
    };
    sim.add_app_node("node0", secret, quorum_set);
    sim.start_all_nodes().await;

    // Wait until the standalone validator is validating (can close ledgers).
    let deadline = std::time::Instant::now() + Duration::from_secs(15);
    loop {
        if let Some(app) = sim.app("node0") {
            if app.state().await == henyey_app::AppState::Validating {
                break;
            }
        }
        assert!(
            std::time::Instant::now() < deadline,
            "node0 did not reach Validating in time"
        );
        tokio::time::sleep(Duration::from_millis(100)).await;
    }

    let app = sim.app("node0").expect("node0 app exists");

    // Fund a small pool of loadgen accounts on-ledger so Pay-mode submissions
    // are accepted (so the run reaches LoadResult::Done).
    const N_ACCOUNTS: u64 = 4;
    fund_loadgen_accounts(&sim, &app, N_ACCOUNTS, 100_000_000).await;

    // Snapshot meters before the run (delta-based assertions).
    let before = handle.render();
    let attempted_before = scrape_counter(&before, "loadgen_txn_attempted");
    let payment_before = scrape_counter(&before, "loadgen_payment_submitted");
    let step_before = scrape_counter(&before, "loadgen_step_count");

    // Drive a short Pay-mode run. tx_rate is per-second; a handful of accounts
    // and a low target keeps the run brief. We close ledgers concurrently so
    // pending txs clear and accounts become available again.
    let mut generator = LoadGenerator::new(Arc::clone(&app), NETWORK_PASSPHRASE.to_string());
    let mut config = GeneratedLoadConfig {
        mode: LoadGenMode::Pay,
        n_accounts: N_ACCOUNTS as u32,
        offset: 0,
        n_txs: N_ACCOUNTS as u32,
        tx_rate: 100,
        // Bound the transient-backpressure retry so a saturated queue can't
        // wedge this test for the full production retry budget (6000 paced
        // tries ≈ 10+ min). The production/SSC default stays at
        // QUEUE_FULL_MAX_TRIES; here a tiny cap keeps the run to seconds while
        // still exercising the same Pay-mode submit path and meters (#3600).
        queue_full_max_tries: 5,
        ..Default::default()
    };

    let stop = AtomicBool::new(false);
    let sim_for_close = &sim;
    // Keep closing ledgers until the run signals completion via `run_done`.
    // `generate_load` no longer returns on submission — it calls
    // `wait_till_complete`, which polls until the submitted accounts sync and
    // is bounded ONLY by ledger advancement (#3601, commit 186ba676). So the
    // closer must keep advancing ledgers for the whole run; if it stopped
    // early (the old fixed `0..20` loop) the run could never sync and would
    // hang forever waiting for closes that never come (#3600). A generous
    // ceiling bounds the closer so a genuinely stuck run still terminates.
    let run_done = AtomicBool::new(false);
    let run_done_ref = &run_done;
    let closer = async {
        for _ in 0..400 {
            if run_done_ref.load(Ordering::Relaxed) {
                break;
            }
            tokio::time::sleep(Duration::from_millis(150)).await;
            let _ = sim_for_close.manual_close_app_node("node0").await;
        }
    };
    let run = async {
        let r = generator.generate_load(&mut config, &stop).await;
        run_done_ref.store(true, Ordering::Relaxed);
        r
    };

    let (result, _) = tokio::join!(run, closer);

    // The run should complete (all N txs submitted). Even if it failed, the
    // attempt meters must have moved — the key #3569 assertion.
    let after = handle.render();
    let attempted_after = scrape_counter(&after, "loadgen_txn_attempted");
    let payment_after = scrape_counter(&after, "loadgen_payment_submitted");
    let step_after = scrape_counter(&after, "loadgen_step_count");

    assert!(
        attempted_after >= attempted_before + N_ACCOUNTS as f64,
        "loadgen_txn_attempted should increase by >= {} (was {}, now {})",
        N_ACCOUNTS,
        attempted_before,
        attempted_after
    );
    assert!(
        payment_after > payment_before,
        "loadgen_payment_submitted should increase (was {}, now {})",
        payment_before,
        payment_after
    );
    assert!(
        step_after > step_before,
        "loadgen_step_count should increase (was {}, now {})",
        step_before,
        step_after
    );

    // The run reached a terminal state; if Done, supercluster's Success path
    // (run_start == run_complete) is exercised by the binary's runner — here we
    // assert the per-tx side-channel, which is the part that lives in
    // henyey-simulation.
    assert!(
        matches!(result, LoadResult::Done { .. } | LoadResult::Failed),
        "run reached a terminal state, got {:?}",
        result
    );

    sim.stop_all_nodes().await.expect("stop standalone node");
}

/// Regression: across consecutive `generate_load` runs (each MaxTPSClassic
/// binary-search step reuses the same network + pregenerated file), the
/// PayPregenerated reader is opened once and read sequentially, so
/// `curr_preloaded` (the round-robin account index `curr_preloaded % n`) MUST
/// persist across runs to stay aligned with the reader's file position.
///
/// Previously `start()` reset `curr_preloaded` to 0 each run while the reader
/// continued from a later position; a non-K-aligned step boundary then
/// misattributed each tx to the wrong account, so the loadgen tracked the wrong
/// account's expected seq and `check_accounts_synced` falsely reported on-chain
/// AHEAD of expected — capping/destabilizing the measured max-TPS ceiling.
/// Parity: stellar-core `mCurrPreloadedTransaction` is never reset in `reset()`.
///
/// FAILS on the buggy code: after step 2 `curr_preloaded` is 4 (reset+4) not 10,
/// and accounts are not synced.
#[tokio::test]
#[serial]
async fn pregenerated_curr_preloaded_persists_across_runs() {
    use henyey_simulation::TxGenerator;

    let mut sim = Simulation::with_network(SimulationMode::OverTcp, NETWORK_PASSPHRASE);
    let seed = Hash256::hash(b"PREGEN_CURR_PRELOADED");
    let secret = SecretKey::from_seed(&seed.0);
    let quorum_set = henyey_app::config::QuorumSetConfig {
        threshold_percent: 100,
        validators: vec![secret.public_key().to_strkey()],
        inner_sets: Vec::new(),
    };
    sim.add_app_node("node0", secret, quorum_set);
    sim.start_all_nodes().await;

    let deadline = std::time::Instant::now() + Duration::from_secs(15);
    loop {
        if let Some(app) = sim.app("node0") {
            if app.state().await == henyey_app::AppState::Validating {
                break;
            }
        }
        assert!(std::time::Instant::now() < deadline, "node0 not Validating");
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    let app = sim.app("node0").expect("node0 app exists");

    const K: u64 = 8;
    fund_loadgen_accounts(&sim, &app, K, 1_000_000_000).await;

    // Pregenerated file (round-robin over the K funded accounts).
    let dir = std::env::temp_dir().join(format!("pregen-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let out = dir.join("txs.xdr");
    {
        let mut txgen = TxGenerator::new(Arc::clone(&app), NETWORK_PASSPHRASE.to_string());
        txgen
            .generate_payment_txs_to_file(&out, 64, K as u32, 0)
            .expect("generate pregenerated file");
    }

    let mut lg = LoadGenerator::new(Arc::clone(&app), NETWORK_PASSPHRASE.to_string());
    let mk = |n_txs: u32| GeneratedLoadConfig {
        mode: LoadGenMode::PayPregenerated,
        n_accounts: K as u32,
        offset: 0,
        n_txs,
        tx_rate: 100,
        queue_full_max_tries: 5,
        preloaded_transactions_file: Some(out.clone()),
        ..Default::default()
    };

    let run_done = AtomicBool::new(false);
    let run_done_ref = &run_done;
    let sim_ref = &sim;
    let closer = async {
        for _ in 0..800 {
            if run_done_ref.load(Ordering::Relaxed) {
                break;
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
            let _ = sim_ref.manual_close_app_node("node0").await;
        }
    };
    let work = async {
        let stop = AtomicBool::new(false);
        // Step 1: 6 txns (NOT a multiple of K, so a reset would misalign).
        let mut c1 = mk(6);
        lg.generate_load(&mut c1, &stop).await;
        let curr1 = lg.curr_preloaded();
        // Step 2: 4 more; the reader is at position 6.
        let mut c2 = mk(4);
        lg.generate_load(&mut c2, &stop).await;
        let curr2 = lg.curr_preloaded();
        run_done_ref.store(true, Ordering::Relaxed);
        (curr1, curr2)
    };
    let ((curr1, curr2), _) = tokio::join!(work, closer);

    let _ = std::fs::remove_dir_all(&dir);

    // curr_preloaded must persist across runs (parity with core
    // mCurrPreloadedTransaction). The reader is read sequentially across both
    // runs, so after reading 6 then 4 it is at 10. The bug reset it to 0 at the
    // start of run 2 (-> 4), misaligning the round-robin account attribution.
    assert_eq!(curr1, 6, "step 1 consumes 6 pregenerated txns");
    assert_eq!(
        curr2, 10,
        "curr_preloaded must persist across runs; got {curr2} \
         (bug resets each run -> 4, misattributing accounts)"
    );
}
