//! Two-process catchup-persist regression test for #1755 / #1749.
//!
//! Spawns `henyey catchup` as a real subprocess against an in-process
//! history-archive fixture, waits for the subprocess to exit cleanly, then
//! re-opens the SQLite database with a second connection (analogous to a
//! later `henyey run` invocation) and asserts the catchup state was
//! persisted.
//!
//! The regression class this catches: #1749 — catchup writing its terminal
//! header/HAS/LCL only to in-memory state and never flushing before
//! process exit. A unit test (`write_to_db_persists_header_has_and_lcl`)
//! already covers the DB-write contract; this test covers the same
//! contract at the *process boundary*, which is the only place the
//! "persisted before a second process starts" invariant can fail.

use std::path::PathBuf;
use std::process::{Command, Stdio};
use std::time::Duration;

use henyey_app::AppConfig;
use henyey_db::queries::StateQueries;
use henyey_db::Database;
use henyey_history::test_utils::build_single_checkpoint_archive;

/// RAII guard that sends SIGKILL to a child subprocess on drop, so a
/// panicking test never leaks a running `henyey` process.
struct ChildGuard(Option<std::process::Child>);

impl Drop for ChildGuard {
    fn drop(&mut self) {
        if let Some(mut child) = self.0.take() {
            let _ = child.kill();
            let _ = child.wait();
        }
    }
}

impl ChildGuard {
    fn take(mut self) -> std::process::Child {
        self.0.take().expect("child already taken")
    }
}

/// Build a minimal testnet `AppConfig` pointing at `fixture_url`, with
/// DB and bucket dirs inside `tmp`, then serialize to a TOML file the
/// subprocess will consume via `--config`.
fn write_test_config(
    tmp: &std::path::Path,
    fixture_url: &str,
    passphrase: &str,
) -> anyhow::Result<(PathBuf, PathBuf)> {
    use henyey_app::config::HistoryArchiveEntry;

    let mut config = AppConfig::testnet();
    config.network.passphrase = passphrase.to_string();

    let db_path = tmp.join("henyey.sqlite");
    let bucket_dir = tmp.join("buckets");
    std::fs::create_dir_all(&bucket_dir)?;
    config.database.path = db_path.clone();
    config.buckets.directory = bucket_dir;

    config.history.archives = vec![HistoryArchiveEntry {
        name: "fixture".to_string(),
        url: fixture_url.trim_end_matches('/').to_string(),
        get_enabled: true,
        put_enabled: false,
        put: None,
        mkdir: None,
    }];

    // Validators must have a seed; we're running catchup (not validating)
    // but the config validator still requires this to be consistent.
    config.node.is_validator = false;
    config.node.node_seed = None;

    let config_path = tmp.join("henyey.toml");
    let toml = toml::to_string(&config)?;
    std::fs::write(&config_path, toml)?;
    Ok((config_path, db_path))
}

#[test]
fn catchup_subprocess_persists_header_has_and_lcl() {
    // Run an explicit tokio runtime so the fixture server (spawned by
    // build_single_checkpoint_archive) lives for the full duration of the
    // subprocess catchup. We intentionally do NOT use #[tokio::test] +
    // spawn_blocking for the subprocess because cargo::cargo_bin path
    // resolution + process reaping is simpler in a plain fn.
    let rt = tokio::runtime::Runtime::new().expect("tokio runtime");
    let _fixture_guard = rt.enter();

    let checkpoint: u32 = 63;
    let fixture = match rt.block_on(build_single_checkpoint_archive(checkpoint)) {
        Ok(f) => f,
        Err(e) => {
            eprintln!("skipping test: {e}");
            return;
        }
    };
    let passphrase = fixture.network_passphrase.clone();

    let tmp = tempfile::tempdir().expect("tempdir");
    let (config_path, db_path) =
        write_test_config(tmp.path(), &fixture.base_url, &passphrase).expect("write config");

    // Locate the henyey binary that cargo built for this integration test.
    // Integration tests of a binary-owning crate automatically have
    // CARGO_BIN_EXE_<name> set.
    let bin = env!("CARGO_BIN_EXE_henyey");

    let mut cmd = Command::new(bin);
    cmd.arg("--config")
        .arg(&config_path)
        .arg("catchup")
        .arg(format!("{checkpoint}/0"))
        .arg("--mode")
        .arg("minimal")
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());

    // Make the fixture archive URL visible to any history-archive code
    // that consults environment variables (belt-and-braces; the TOML
    // config is the primary mechanism).
    cmd.env_remove("RS_STELLAR_CORE_DATABASE_PATH");
    cmd.env_remove("RS_STELLAR_CORE_BUCKETS_DIRECTORY");
    cmd.env_remove("RS_STELLAR_CORE_NETWORK_PASSPHRASE");

    // Keep the fixture's async server alive while the subprocess runs.
    // The runtime is held via `_fixture_guard` above.
    let child = cmd.spawn().expect("spawn henyey catchup");
    let mut guard = ChildGuard(Some(child));

    // Bound the subprocess to a reasonable wall-clock; catchup of a single
    // checkpoint against a localhost fixture should complete in seconds.
    let deadline = std::time::Instant::now() + Duration::from_secs(120);
    let exit_status = loop {
        match guard.0.as_mut().unwrap().try_wait() {
            Ok(Some(status)) => break status,
            Ok(None) => {
                if std::time::Instant::now() >= deadline {
                    panic!("catchup subprocess did not exit within 120s");
                }
                std::thread::sleep(Duration::from_millis(100));
            }
            Err(e) => panic!("wait error: {e}"),
        }
    };

    let child = guard.take();
    let output = child.wait_with_output().expect("wait_with_output");
    // wait_with_output after try_wait still needs to drain pipes; exit status
    // from try_wait is authoritative.
    assert!(
        exit_status.success(),
        "catchup exited with {exit_status:?}\nstdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr),
    );

    // Open the DB that the subprocess populated and assert persistence.
    let db = Database::open(&db_path).expect("open persisted db");
    let lcl: u32 = db
        .with_connection(|c| c.get_last_closed_ledger())
        .expect("get LCL")
        .expect("LCL present after catchup");

    assert_eq!(
        lcl,
        checkpoint,
        "catchup subprocess must persist LCL = {checkpoint} (header+HAS+LCL transactionally, \
         regression #1749)\nstderr tail:\n{}",
        String::from_utf8_lossy(&output.stderr)
            .lines()
            .rev()
            .take(20)
            .collect::<Vec<_>>()
            .into_iter()
            .rev()
            .collect::<Vec<_>>()
            .join("\n")
    );

    drop(fixture);
}
