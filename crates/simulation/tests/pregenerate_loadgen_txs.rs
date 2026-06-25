//! Integration tests for the pre-generated payment-transaction file writer
//! (`TxGenerator::generate_payment_txs_to_file`), the generator half of the
//! `pregenerate-loadgen-txs` CLI subcommand.
//!
//! Parity reference: stellar-core `generateTransactions` (`TestUtils.cpp:486`)
//! + `runGenerateSyntheticLoad` (`CommandLine.cpp:1972`).

use std::sync::Arc;

use henyey_app::config::ConfigBuilder;
use henyey_app::App;
use henyey_simulation::{initialize_genesis_ledger, PregeneratedTxReader, TxGenerator};
use stellar_xdr::{MuxedAccount, TransactionEnvelope, Uint256};

/// Build a genesis-bootstrapped standalone `App` with `account_count` test
/// accounts, mirroring `cmd_pregenerate_loadgen_txs` / `cmd_apply_load`.
async fn genesis_app(account_count: u32) -> (Arc<App>, String, tempfile::TempDir) {
    let data_dir = tempfile::tempdir().unwrap();
    let mut config = ConfigBuilder::simulation()
        .database_path(data_dir.path().join("pregen.db"))
        .bucket_directory(data_dir.path().join("buckets"))
        .validator(true)
        .build();
    config.node.manual_close = true;
    config.testing.run_standalone = true;
    config.http.enabled = false;
    config.compat_http.enabled = false;
    config.testing.genesis_test_account_count = account_count;

    // Ephemeral node seed (required for validators).
    if config.node.node_seed.is_none() {
        let ephemeral = henyey_crypto::SecretKey::generate();
        config.node.node_seed = Some(ephemeral.to_strkey());
    }

    std::fs::create_dir_all(&config.buckets.directory).unwrap();

    let passphrase = config.network.passphrase.clone();
    initialize_genesis_ledger(&config, &passphrase).unwrap();

    let app = Arc::new(App::new(config).await.unwrap());
    app.set_self_arc().await;
    app.bootstrap_from_db().await.unwrap();
    (app, passphrase, data_dir)
}

fn source_pubkey(env: &TransactionEnvelope) -> [u8; 32] {
    match env {
        TransactionEnvelope::Tx(e) => match &e.tx.source_account {
            MuxedAccount::Ed25519(Uint256(k)) => *k,
            MuxedAccount::MuxedEd25519(m) => m.ed25519.0,
        },
        other => panic!("unexpected envelope variant: {other:?}"),
    }
}

fn source_seq(env: &TransactionEnvelope) -> i64 {
    match env {
        TransactionEnvelope::Tx(e) => e.tx.seq_num.0,
        other => panic!("unexpected envelope variant: {other:?}"),
    }
}

#[tokio::test]
async fn generates_round_robin_payment_file() {
    const K: u32 = 5;
    const N: u32 = 17;

    let (app, passphrase, data_dir) = genesis_app(K).await;
    let out = data_dir.path().join("stellar-load-transactions.xdr");

    let mut txgen = TxGenerator::new(app, passphrase);
    txgen
        .generate_payment_txs_to_file(&out, N, K, 0)
        .expect("generation succeeds");

    // Read the file back through the existing reader.
    let mut reader = PregeneratedTxReader::open(&out).unwrap();
    let mut envs = Vec::new();
    while let Some(env) = reader.read_one().unwrap() {
        envs.push(env);
    }

    // Exactly N envelopes.
    assert_eq!(envs.len() as u32, N, "expected {N} envelopes");

    // Source cycles round-robin (i % K) over TestAccount-(i % K). The generator
    // cached each account it used; the source id for envelope i is (i % K), so
    // its pubkey must match the cached TestAccount-(i % K).
    for (i, env) in envs.iter().enumerate() {
        let id = (i as u64) % (K as u64);
        let expected = *txgen
            .get_account(id)
            .expect("source account cached during generation")
            .secret_key
            .public_key()
            .as_bytes();
        assert_eq!(
            source_pubkey(env),
            expected,
            "envelope {i} source must be TestAccount-{id}"
        );
    }

    // Per-account sequence numbers strictly increment.
    let mut last_seq: std::collections::HashMap<[u8; 32], i64> = std::collections::HashMap::new();
    for (i, env) in envs.iter().enumerate() {
        let src = source_pubkey(env);
        let seq = source_seq(env);
        if let Some(prev) = last_seq.get(&src) {
            assert!(
                seq > *prev,
                "envelope {i}: seq {seq} must exceed previous {prev} for the same account"
            );
        }
        last_seq.insert(src, seq);
    }
}

#[tokio::test]
async fn generate_rejects_zero_accounts() {
    let (app, passphrase, data_dir) = genesis_app(1).await;
    let out = data_dir.path().join("stellar-load-transactions.xdr");

    let mut txgen = TxGenerator::new(app, passphrase);
    let err = txgen.generate_payment_txs_to_file(&out, 10, 0, 0);
    assert!(err.is_err(), "accounts == 0 must be rejected");
}
