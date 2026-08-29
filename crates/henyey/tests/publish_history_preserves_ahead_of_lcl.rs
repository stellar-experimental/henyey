//! Process-boundary regression test for #3870 (publish-history seam).
//!
//! #3868 wired the destructive `cleanup_ahead_of_lcl()` into the
//! `publish-history` CLI subcommand — a read-oriented, lock-free path. Running
//! it while a catchup is persisting ahead-of-LCL rows (#3827) DELETES those
//! rows; once catchup advances the LCL past them, the deleted range becomes a
//! permanent history hole — the exact silent data loss #3811/#3827 set out to
//! eliminate.
//!
//! This is the symmetric twin of `self_check_preserves_ahead_of_lcl.rs`. It
//! covers the *second* wiring seam the linked issue faults #3868 for leaving
//! untested: reverting `crates/henyey/src/publish_history.rs` from
//! `durable_read_anchor()` back to `cleanup_ahead_of_lcl()` must fail a test.
//! The `db`-level unit tests for `durable_read_anchor()` and the self-check
//! subprocess test do not exercise this call site, so without this test the
//! publish-history seam could regress silently.
//!
//! It constructs a database that mirrors a catchup that persisted rows ahead of
//! the durable LCL (LCL = L, but `ledgerheaders`/`txhistory` rows exist for
//! seqs `1..=L+10`), runs `henyey publish-history` as a real subprocess, then
//! reopens the DB and asserts the ahead-of-LCL rows are still present.
//!
//! FAILS on the pre-fix code: `cmd_publish_history` calls `cleanup_ahead_of_lcl()`
//! immediately after opening the DB — before any checkpoint publishing — deleting
//! seqs `L+1..=L+10`. The publish then errors out (synthetic headers/buckets), but
//! the DELETE is already committed, so the reopened DB has no rows above L.
//! PASSES after the fix: the CLI anchors reads at the durable LCL via
//! `durable_read_anchor()` and never mutates history.

use std::path::PathBuf;
use std::process::{Command, Stdio};

use henyey_app::config::HistoryArchiveEntry;
use henyey_app::AppConfig;
use henyey_db::queries::StateQueries;
use henyey_db::Database;

/// Durable last-closed-ledger for the test fixture.
const LCL: u32 = 100;
/// Highest ledger sequence persisted ahead of the LCL (simulating an in-flight
/// catchup that persisted its batch before advancing LCL, per #3827).
const MAX_SEQ: u32 = 110;

/// Build a minimal **validator** testnet `AppConfig` — `publish-history` bails
/// immediately unless the node is a validator with a writable archive — serialize
/// it to a TOML file the subprocess consumes via `--config`, and return the
/// config + db paths.
///
/// A single writable local (`file://`) archive is configured so the subprocess
/// gets past the "no writable archives" guard and reaches the history-anchoring
/// call site under test. `load_config` does not run `validate()`, so the config
/// only needs to deserialize — no full validator wiring is required.
fn write_test_config(tmp: &std::path::Path) -> anyhow::Result<(PathBuf, PathBuf)> {
    let mut config = AppConfig::testnet();

    let db_path = tmp.join("henyey.sqlite");
    let bucket_dir = tmp.join("buckets");
    let archive_dir = tmp.join("archive");
    std::fs::create_dir_all(&bucket_dir)?;
    std::fs::create_dir_all(&archive_dir)?;
    config.database.path = db_path.clone();
    config.buckets.directory = bucket_dir;

    // publish-history is validator-only. A valid 56-char S... seed satisfies the
    // (unused-here) format check; no SCP wiring runs because publish errors out
    // before consensus is touched.
    config.node.is_validator = true;
    config.node.node_seed =
        Some("SAFTEV5U6QDFE2DRMSD7HBE76XG7SQZJD6VIUTHIXTJGO77RUQYVURLA".to_string());

    // Exactly one writable local archive so `publish-history` reaches the
    // anchor step. `put = None` + a `file://` URL routes it to the local-target
    // path (not a command target). Absolute paths start with `/`, so
    // `file://{abs}` yields a valid `file:///…` URL.
    let archive_url = format!("file://{}", archive_dir.display());
    config.history.archives = vec![HistoryArchiveEntry {
        name: "local-test".to_string(),
        url: archive_url,
        get_enabled: false,
        put_enabled: true,
        put: None,
        mkdir: None,
    }];

    let config_path = tmp.join("henyey.toml");
    let toml = toml::to_string(&config)?;
    std::fs::write(&config_path, toml)?;
    Ok((config_path, db_path))
}

/// Populate the DB with a durable LCL = `LCL` and `ledgerheaders` + `txhistory`
/// rows for seqs `1..=MAX_SEQ` — i.e. rows `LCL+1..=MAX_SEQ` are legitimately
/// ahead of the durable LCL (mirroring a catchup that persisted them per #3827).
fn seed_ahead_of_lcl_db(db_path: &std::path::Path) {
    let db = Database::open(db_path).expect("open db for seeding");
    db.with_connection(|conn| {
        conn.set_last_closed_ledger(LCL)?;
        for seq in 1..=MAX_SEQ {
            // Values are literals (seq is a u32, blobs are X'00') so the test
            // needs no rusqlite param bindings — henyey has no direct rusqlite
            // dev-dependency and we deliberately avoid adding one here.
            conn.execute(
                &format!(
                    "INSERT INTO ledgerheaders \
                     (ledgerhash, prevhash, bucketlisthash, ledgerseq, closetime, data) \
                     VALUES ('h{seq}', 'p{seq}', 'b{seq}', {seq}, 0, X'00')"
                ),
                [],
            )?;
            conn.execute(
                &format!(
                    "INSERT INTO txhistory \
                     (txid, ledgerseq, txindex, txbody, txresult, txmeta, status) \
                     VALUES ('tx{seq}', {seq}, 0, X'00', X'00', NULL, 0)"
                ),
                [],
            )?;
        }
        Ok(())
    })
    .expect("seed ahead-of-LCL rows");
}

/// Count rows in `table` whose `ledgerseq` is strictly greater than `LCL`.
fn count_above_lcl(db: &Database, table: &str) -> i64 {
    db.with_connection(|conn| {
        let n: i64 = conn.query_row(
            &format!("SELECT COUNT(*) FROM {table} WHERE ledgerseq > {LCL}"),
            [],
            |r| r.get(0),
        )?;
        Ok(n)
    })
    .expect("count ahead-of-LCL rows")
}

#[test]
fn publish_history_does_not_delete_ahead_of_lcl_rows() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let (config_path, db_path) = write_test_config(tmp.path()).expect("write config");

    seed_ahead_of_lcl_db(&db_path);

    let expected_above = (MAX_SEQ - LCL) as i64;
    {
        // Sanity: the fixture really does have ahead-of-LCL rows before we run.
        let db = Database::open(&db_path).expect("reopen seeded db");
        assert_eq!(
            count_above_lcl(&db, "ledgerheaders"),
            expected_above,
            "fixture setup: ledgerheaders should have {expected_above} rows above LCL"
        );
        assert_eq!(
            count_above_lcl(&db, "txhistory"),
            expected_above,
            "fixture setup: txhistory should have {expected_above} rows above LCL"
        );
    }

    let bin = env!("CARGO_BIN_EXE_henyey");
    let mut cmd = Command::new(bin);
    cmd.arg("--config")
        .arg(&config_path)
        .arg("publish-history")
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    cmd.env_remove("RS_STELLAR_CORE_DATABASE_PATH");
    cmd.env_remove("RS_STELLAR_CORE_BUCKETS_DIRECTORY");
    cmd.env_remove("RS_STELLAR_CORE_NETWORK_PASSPHRASE");

    // Exit code is intentionally ignored: the synthetic ledger headers make
    // checkpoint publishing fail, but that happens *after* the anchor step.
    // What we assert is the persistence side effect, not the verdict — the
    // pre-fix DELETE commits before publishing is even attempted.
    let output = cmd.output().expect("run henyey publish-history");

    // Reopen the DB the subprocess operated on and assert the ahead-of-LCL rows
    // survived. txhistory holes are the specific data loss the issue calls out,
    // so we assert on txhistory as well as ledgerheaders.
    let db = Database::open(&db_path).expect("reopen db after publish-history");

    let stderr = String::from_utf8_lossy(&output.stderr);
    let stdout = String::from_utf8_lossy(&output.stdout);

    assert_eq!(
        count_above_lcl(&db, "ledgerheaders"),
        expected_above,
        "publish-history must NOT delete ahead-of-LCL ledgerheaders rows (#3870)\n\
         stdout:\n{stdout}\nstderr:\n{stderr}"
    );
    assert_eq!(
        count_above_lcl(&db, "txhistory"),
        expected_above,
        "publish-history must NOT delete ahead-of-LCL txhistory rows — these become a \
         permanent history hole once catchup advances LCL past them (#3870)\n\
         stdout:\n{stdout}\nstderr:\n{stderr}"
    );

    // And the highest stored ledger is unchanged: nothing was truncated.
    let latest = db
        .get_latest_ledger_seq()
        .expect("get latest ledger seq")
        .expect("db is non-empty");
    assert_eq!(
        latest, MAX_SEQ,
        "publish-history must leave MAX(ledgerseq) at {MAX_SEQ}; a lower value means \
         ahead-of-LCL rows were truncated (#3870)"
    );
}
