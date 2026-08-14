//! Process-boundary regression test for #3870.
//!
//! #3868 wired the destructive `cleanup_ahead_of_lcl()` into the `self-check`
//! CLI subcommand — a read-oriented diagnostic. Because a running node/catchup
//! holds `App`'s process-lifetime db lock while these CLI paths take no lock,
//! running `self-check` while a catchup is persisting ahead-of-LCL rows (#3827)
//! DELETES those rows. Once catchup advances the LCL past them, the deleted
//! range becomes a permanent hole — the exact silent data loss #3811/#3827 set
//! out to eliminate.
//!
//! This test constructs a database that mirrors a catchup that persisted rows
//! ahead of the durable LCL (LCL = L, but `ledgerheaders`/`txhistory` rows exist
//! for seqs `1..=L+10`), runs `henyey self-check` as a real subprocess, then
//! reopens the DB and asserts the ahead-of-LCL rows are still present.
//!
//! FAILS on origin/main: `cmd_self_check` calls `cleanup_ahead_of_lcl()` as its
//! first DB op, deleting seqs `L+1..=L+10` before any verification, so the
//! reopened DB has no rows above L.
//! PASSES after the fix: the CLI no longer mutates history — it anchors reads at
//! the durable LCL via `durable_read_anchor()` instead.

use std::path::PathBuf;
use std::process::{Command, Stdio};

use henyey_app::AppConfig;
use henyey_db::queries::StateQueries;
use henyey_db::Database;

/// Durable last-closed-ledger for the test fixture.
const LCL: u32 = 100;
/// Highest ledger sequence persisted ahead of the LCL (simulating an in-flight
/// catchup that persisted its batch before advancing LCL, per #3827).
const MAX_SEQ: u32 = 110;

/// Build a minimal testnet `AppConfig` with DB and bucket dirs inside `tmp`,
/// serialize it to a TOML file the subprocess consumes via `--config`, and
/// return the config + db paths.
fn write_test_config(tmp: &std::path::Path) -> anyhow::Result<(PathBuf, PathBuf)> {
    let mut config = AppConfig::testnet();

    let db_path = tmp.join("henyey.sqlite");
    let bucket_dir = tmp.join("buckets");
    std::fs::create_dir_all(&bucket_dir)?;
    config.database.path = db_path.clone();
    config.buckets.directory = bucket_dir;

    // Not a validator; self-check does not need a seed.
    config.node.is_validator = false;
    config.node.node_seed = None;

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
fn self_check_does_not_delete_ahead_of_lcl_rows() {
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
        .arg("self-check")
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    cmd.env_remove("RS_STELLAR_CORE_DATABASE_PATH");
    cmd.env_remove("RS_STELLAR_CORE_BUCKETS_DIRECTORY");
    cmd.env_remove("RS_STELLAR_CORE_NETWORK_PASSPHRASE");

    // Exit code is intentionally ignored: the synthetic ledger hashes make
    // header-chain verification fail, but that happens *after* the anchor step.
    // What we assert is the persistence side effect, not the verdict.
    let output = cmd.output().expect("run henyey self-check");

    // Reopen the DB the subprocess operated on and assert the ahead-of-LCL rows
    // survived. txhistory holes are the specific data loss the issue calls out,
    // so we assert on txhistory as well as ledgerheaders.
    let db = Database::open(&db_path).expect("reopen db after self-check");

    let stderr = String::from_utf8_lossy(&output.stderr);
    let stdout = String::from_utf8_lossy(&output.stdout);

    assert_eq!(
        count_above_lcl(&db, "ledgerheaders"),
        expected_above,
        "self-check must NOT delete ahead-of-LCL ledgerheaders rows (#3870)\n\
         stdout:\n{stdout}\nstderr:\n{stderr}"
    );
    assert_eq!(
        count_above_lcl(&db, "txhistory"),
        expected_above,
        "self-check must NOT delete ahead-of-LCL txhistory rows — these become a \
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
        "self-check must leave MAX(ledgerseq) at {MAX_SEQ}; a lower value means \
         ahead-of-LCL rows were truncated (#3870)"
    );
}
