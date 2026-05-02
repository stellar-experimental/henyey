//! History archive publishing — materializes checkpoints to local/remote archives.

use std::path::PathBuf;

use henyey_app::AppConfig;
use henyey_bucket::{BucketList, BucketManager};
use henyey_common::Hash256;
use henyey_history::archive_state::HistoryArchiveState;
use henyey_history::checkpoint::{checkpoint_containing, next_checkpoint};
use henyey_history::checkpoint_frequency;
use henyey_history::paths::root_has_path;
use henyey_history::publish::{build_history_archive_state, PublishConfig, PublishManager};

use henyey_ledger::compute_header_hash;
use url::Url;

/// Publish history command handler.
pub(crate) async fn cmd_publish_history(config: AppConfig, force: bool) -> anyhow::Result<()> {
    use std::fs;

    if !config.node.is_validator {
        anyhow::bail!("Only validators can publish history");
    }

    println!("Publishing history to archives...");
    println!();

    // Check for writable archives
    let writable_archives: Vec<_> = config
        .history
        .archives
        .iter()
        .filter(|a| a.put_enabled)
        .collect();

    if writable_archives.is_empty() {
        anyhow::bail!(
            "No writable history archives configured. Add 'put = true' to an archive config."
        );
    }

    let mut local_targets = Vec::new();
    let mut command_targets = Vec::new();
    for archive in &writable_archives {
        if let Some(put) = archive.put.clone() {
            command_targets.push(CommandArchiveTarget {
                name: archive.name.clone(),
                put,
                mkdir: archive.mkdir.clone(),
            });
            continue;
        }

        let path = match Url::parse(&archive.url) {
            Ok(url) if url.scheme() == "file" => url
                .to_file_path()
                .map_err(|_| anyhow::anyhow!("Invalid file URL: {}", archive.url))?,
            Ok(_) => {
                tracing::warn!(archive = %archive.url, "Remote publish not supported (missing put command)");
                continue;
            }
            Err(_) => PathBuf::from(&archive.url),
        };
        local_targets.push((archive.url.as_str(), path));
    }

    if local_targets.is_empty() && command_targets.is_empty() {
        anyhow::bail!("No publish archives configured (local paths or put commands required).");
    }

    println!(
        "Writable archives: {}",
        local_targets.len() + command_targets.len()
    );
    for (url, _) in &local_targets {
        println!("  - {}", url);
    }
    for archive in &command_targets {
        println!("  - {} (command)", archive.name);
    }
    println!();

    // Open database to get current state
    let db = henyey_db::Database::open(&config.database.path)?;

    // Get current ledger from database
    let current_ledger = db
        .get_latest_ledger_seq()?
        .ok_or_else(|| anyhow::anyhow!("No ledger data in database. Run the node first."))?;

    println!("Current ledger in database: {}", current_ledger);

    // Calculate checkpoints to publish
    let latest_checkpoint = henyey_history::checkpoint::latest_checkpoint_before_or_at(
        current_ledger,
    )
    .ok_or_else(|| anyhow::anyhow!("No checkpoint available for ledger {}", current_ledger))?;

    println!("Latest publishable checkpoint: {}", latest_checkpoint);

    let queued_checkpoints = db.load_publish_queue(None)?;
    let mut queued_checkpoints = queued_checkpoints
        .into_iter()
        .filter(|checkpoint| *checkpoint <= latest_checkpoint)
        .collect::<Vec<_>>();
    queued_checkpoints.sort_unstable();

    // Check what's already published across local archives.
    let mut published_ledger = latest_checkpoint;
    for (_, path) in &local_targets {
        let root_path = path.join(root_has_path());
        let ledger = if root_path.exists() {
            let json = fs::read_to_string(&root_path)?;
            let has = HistoryArchiveState::from_json(&json)?;
            has.current_ledger()
        } else {
            0
        };
        published_ledger = published_ledger.min(ledger);
    }

    println!("Already published up to: {}", published_ledger);

    if published_ledger >= latest_checkpoint && !force && queued_checkpoints.is_empty() {
        println!();
        println!("Archive is up to date. Use --force to republish.");
        return Ok(());
    }

    let mut checkpoints_to_publish = Vec::new();
    if !queued_checkpoints.is_empty() && !force {
        checkpoints_to_publish = queued_checkpoints;
    } else {
        // Calculate range to publish
        let start_checkpoint = if published_ledger > 0 && !force {
            next_checkpoint(published_ledger)
        } else {
            // Start from the first checkpoint we have
            checkpoint_containing(1)
        };

        if start_checkpoint > latest_checkpoint {
            println!("Nothing new to publish.");
            return Ok(());
        }

        let mut checkpoint = start_checkpoint;
        while checkpoint <= latest_checkpoint {
            checkpoints_to_publish.push(checkpoint);
            checkpoint = next_checkpoint(checkpoint);
        }
    }

    if checkpoints_to_publish.is_empty() {
        println!("Nothing queued to publish.");
        return Ok(());
    }

    println!();
    let first_checkpoint = *checkpoints_to_publish
        .first()
        .expect("checked non-empty above");
    let last_checkpoint = *checkpoints_to_publish
        .last()
        .expect("checked non-empty above");
    println!(
        "Publishing checkpoints {} to {}...",
        first_checkpoint, last_checkpoint
    );

    let bucket_manager = BucketManager::with_cache_size(
        config.buckets.directory.clone(),
        config.buckets.cache_size,
    )?;

    let mut published_count = 0;
    for checkpoint in checkpoints_to_publish {
        print!("  Publishing checkpoint {}... ", checkpoint);

        let start_ledger = checkpoint.saturating_sub(checkpoint_frequency() - 1);
        let start_ledger = if start_ledger == 0 { 1 } else { start_ledger };
        let mut headers = Vec::new();
        let mut tx_entries = Vec::new();
        let mut tx_results = Vec::new();

        for seq in start_ledger..=checkpoint {
            let header = db
                .get_ledger_header(seq)?
                .ok_or_else(|| anyhow::anyhow!("Missing ledger header {}", seq))?;
            let hash = compute_header_hash(&header)?;
            headers.push(stellar_xdr::curr::LedgerHeaderHistoryEntry {
                header,
                hash: stellar_xdr::curr::Hash(hash.0),
                ext: stellar_xdr::curr::LedgerHeaderHistoryEntryExt::V0,
            });

            let tx_entry = db
                .get_tx_history_entry(seq)?
                .ok_or_else(|| anyhow::anyhow!("Missing tx history entry {}", seq))?;

            let tx_result = db
                .get_tx_result_entry(seq)?
                .ok_or_else(|| anyhow::anyhow!("Missing tx result entry {}", seq))?;

            // Match stellar-core: only include tx/result entries for ledgers
            // with transactions (CheckpointBuilder.cpp:140).
            if !tx_result.tx_result_set.results.is_empty() {
                tx_entries.push(tx_entry);
                tx_results.push(tx_result);
            }
        }

        let scp_entries = build_scp_history_entries(&db, start_ledger, checkpoint)?;

        // Verification of tx set and result hashes is handled by
        // PublishManager::publish_checkpoint — no need to duplicate here.

        let levels = db
            .load_bucket_list(checkpoint)?
            .ok_or_else(|| anyhow::anyhow!("Missing bucket list snapshot {}", checkpoint))?;
        let mut bucket_list = BucketList::new();
        bucket_list.set_bucket_dir(bucket_manager.bucket_dir().to_path_buf());
        for (idx, (curr_hash, snap_hash)) in levels.iter().enumerate() {
            let curr_bucket = bucket_manager.load_bucket(curr_hash)?;
            let snap_bucket = bucket_manager.load_bucket(snap_hash)?;
            let level = bucket_list
                .level_mut(idx)
                .ok_or_else(|| anyhow::anyhow!("Missing bucket level {}", idx))?;
            level.set_curr((*curr_bucket).clone());
            level.set_snap((*snap_bucket).clone());
        }

        let expected_hash = Hash256::from(headers.last().unwrap().header.bucket_list_hash.0);
        let actual_hash = bucket_list.hash();
        if expected_hash != actual_hash {
            anyhow::bail!(
                "Bucket list hash mismatch at {} (expected {}, got {})",
                checkpoint,
                expected_hash.to_hex(),
                actual_hash.to_hex()
            );
        }

        // Build HAS once for all publish targets (same arguments, deterministic result)
        let has = build_history_archive_state(
            checkpoint,
            &bucket_list,
            None,
            Some(config.network.passphrase.clone()),
        )?;

        let command_publish_dir = if command_targets.is_empty() {
            None
        } else {
            let publish_tmp = bucket_manager.create_staging_dir()?;
            let publish_dir = publish_tmp.path().to_path_buf();

            let publish_config = PublishConfig {
                local_path: publish_dir.clone(),
                network_passphrase: Some(config.network.passphrase.clone()),
                ..Default::default()
            };
            let manager = PublishManager::new(publish_config);
            manager.publish_checkpoint(
                checkpoint,
                &headers,
                &tx_entries,
                &tx_results,
                &bucket_list,
                None,
            )?;

            write_root_has(&publish_dir, &has)?;
            Some((publish_dir, publish_tmp))
        };

        let mut published_any = false;
        for (url, path) in &local_targets {
            let publish_config = PublishConfig {
                local_path: path.clone(),
                network_passphrase: Some(config.network.passphrase.clone()),
                ..Default::default()
            };
            let manager = PublishManager::new(publish_config);
            if !force && manager.is_published(checkpoint) {
                continue;
            }
            manager.publish_checkpoint(
                checkpoint,
                &headers,
                &tx_entries,
                &tx_results,
                &bucket_list,
                None,
            )?;
            write_scp_history_file(path, checkpoint, &scp_entries)?;
            write_root_has(path, &has)?;
            println!("OK ({})", url);
            published_any = true;
        }

        if let Some((ref publish_dir, _)) = command_publish_dir {
            write_scp_history_file(publish_dir, checkpoint, &scp_entries)?;
            for archive in &command_targets {
                upload_publish_directory(archive, publish_dir)?;
                println!("OK (command: {})", archive.name);
                published_any = true;
            }
        }

        if let Some((_publish_dir, publish_tmp)) = command_publish_dir {
            if let Err(err) = publish_tmp.close() {
                tracing::warn!(
                    error = %err,
                    "Failed to remove publish staging directory"
                );
            }
        }

        if published_any {
            published_count += 1;
        } else {
            println!("SKIP (already published)");
        }

        if let Err(err) = db.remove_publish(checkpoint) {
            tracing::warn!(checkpoint, error = %err, "Failed to remove publish queue entry");
        }
    }

    println!();
    println!("Publishing complete:");
    println!("  Checkpoints processed: {}", published_count);
    println!();

    Ok(())
}

/// Builds SCP history entries for a checkpoint range from the database.
///
/// Collects SCP envelopes and quorum sets for each ledger in the range,
/// packaging them into the format required for history archive publishing.
fn build_scp_history_entries(
    db: &henyey_db::Database,
    start_ledger: u32,
    checkpoint: u32,
) -> anyhow::Result<Vec<stellar_xdr::curr::ScpHistoryEntry>> {
    use henyey_common::Hash256;
    use std::collections::HashSet;
    use stellar_xdr::curr::{LedgerScpMessages, ScpHistoryEntry, ScpHistoryEntryV0};

    let mut entries = Vec::new();
    for seq in start_ledger..=checkpoint {
        let envelopes = db.load_scp_history(seq)?;
        if envelopes.is_empty() {
            continue;
        }

        let mut qset_hashes = HashSet::new();
        for envelope in &envelopes {
            let hash = henyey_common::scp_quorum_set_hash(&envelope.statement);
            qset_hashes.insert(Hash256::from_bytes(hash.0));
        }

        let mut qset_hashes = qset_hashes.into_iter().collect::<Vec<_>>();
        qset_hashes.sort_unstable_by(|a, b| a.as_bytes().cmp(b.as_bytes()));

        let mut qsets = Vec::new();
        for hash in qset_hashes {
            let qset = db
                .load_scp_quorum_set(&hash)?
                .ok_or_else(|| anyhow::anyhow!("Missing quorum set {}", hash.to_hex()))?;
            qsets.push(qset);
        }

        let quorum_sets = qsets
            .try_into()
            .map_err(|_| anyhow::anyhow!("Too many quorum sets for ledger {}", seq))?;
        let messages = envelopes
            .try_into()
            .map_err(|_| anyhow::anyhow!("Too many SCP envelopes for ledger {}", seq))?;
        let entry = ScpHistoryEntry::V0(ScpHistoryEntryV0 {
            quorum_sets,
            ledger_messages: LedgerScpMessages {
                ledger_seq: seq,
                messages,
            },
        });
        entries.push(entry);
    }

    Ok(entries)
}

/// Write the root `HistoryArchiveState` JSON file into a directory.
fn write_root_has(
    dir: &std::path::Path,
    has: &henyey_history::HistoryArchiveState,
) -> anyhow::Result<()> {
    use henyey_history::paths::root_has_path;
    let root_path = dir.join(root_has_path());
    super::create_parent_dir(&root_path)?;
    henyey_common::fs_utils::atomic_write_bytes(&root_path, has.to_json()?.as_bytes())?;
    Ok(())
}

/// Writes SCP history entries to a gzip-compressed XDR file.
///
/// Creates the file at the standard history archive path for SCP data.
fn write_scp_history_file(
    base_dir: &std::path::Path,
    checkpoint: u32,
    entries: &[stellar_xdr::curr::ScpHistoryEntry],
) -> anyhow::Result<()> {
    use henyey_history::paths::checkpoint_path;

    let path = base_dir.join(checkpoint_path("scp", checkpoint, "xdr.gz"));
    super::create_parent_dir(&path)?;
    henyey_common::fs_utils::atomic_gzip_xdr_write_slice(&path, entries)?;
    Ok(())
}

/// Configuration for publishing to a remote archive via shell commands.
#[derive(Clone)]
struct CommandArchiveTarget {
    /// Human-readable name for the archive.
    name: String,
    /// Shell command template for uploading files ({0} = local path, {1} = remote path).
    put: String,
    /// Optional shell command template for creating directories ({0} = remote dir).
    mkdir: Option<String>,
}

/// Uploads a local publish directory to a remote archive using shell commands.
///
/// Iterates through all files in the directory, creating remote directories
/// as needed, then uploads each file using the configured put command.
fn upload_publish_directory(
    target: &CommandArchiveTarget,
    publish_dir: &std::path::Path,
) -> anyhow::Result<()> {
    use std::collections::HashSet;

    let mut files = collect_files(publish_dir)?;
    files.sort();

    let mut created_dirs = HashSet::new();
    for file in files {
        let rel = file
            .strip_prefix(publish_dir)
            .map_err(|e| anyhow::anyhow!("invalid publish path: {}", e))?;
        let rel_str = path_to_unix_string(rel);

        if let Some(ref mkdir_cmd) = target.mkdir {
            if let Some(parent) = rel.parent() {
                if !parent.as_os_str().is_empty() {
                    let remote_dir = path_to_unix_string(parent);
                    if created_dirs.insert(remote_dir.clone()) {
                        let cmd = render_mkdir_command(mkdir_cmd, &remote_dir);
                        run_shell_command(&cmd)?;
                    }
                }
            }
        }

        let cmd = render_put_command(&target.put, &file, &rel_str);
        run_shell_command(&cmd)?;
    }

    Ok(())
}

/// Recursively collects all files under a directory.
fn collect_files(root: &std::path::Path) -> anyhow::Result<Vec<std::path::PathBuf>> {
    let mut files = Vec::new();
    let mut stack = vec![root.to_path_buf()];

    while let Some(dir) = stack.pop() {
        for entry in std::fs::read_dir(&dir)? {
            let entry = entry?;
            let path = entry.path();
            if path.is_dir() {
                stack.push(path);
            } else if path.is_file() {
                files.push(path);
            }
        }
    }

    Ok(files)
}

/// Converts a path to a Unix-style string with forward slashes.
fn path_to_unix_string(path: &std::path::Path) -> String {
    path.components()
        .map(|c| c.as_os_str().to_string_lossy().into_owned())
        .collect::<Vec<_>>()
        .join("/")
}

/// Renders a put command template with local and remote paths.
fn render_put_command(template: &str, local_path: &std::path::Path, remote_path: &str) -> String {
    template
        .replace("{0}", local_path.to_string_lossy().as_ref())
        .replace("{1}", remote_path)
}

/// Renders a mkdir command template with the remote directory.
fn render_mkdir_command(template: &str, remote_dir: &str) -> String {
    template.replace("{0}", remote_dir)
}

/// Executes a shell command and returns an error if it fails.
fn run_shell_command(cmd: &str) -> anyhow::Result<()> {
    use std::process::Command;

    let status = Command::new("sh").arg("-c").arg(cmd).status()?;
    if status.success() {
        Ok(())
    } else {
        anyhow::bail!("command failed (exit {}): {}", status, cmd);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Read;
    use stellar_xdr::curr::ReadXdr;

    /// Regression test for #2097: CLI publish staging must use `<bucket_dir>/tmp/`,
    /// not the system temp directory.
    #[tokio::test]
    async fn test_cmd_publish_staging_uses_bucket_dir_tmp() {
        use henyey_db::queries::{
            BucketListQueries, HistoryQueries, LedgerQueries, PublishQueueQueries, StateQueries,
        };
        use henyey_db::schema::state_keys;
        use stellar_xdr::curr::{
            Hash, LedgerHeader, LedgerHeaderExt, Limits, StellarValue, StellarValueExt,
            TransactionHistoryEntry, TransactionHistoryEntryExt, TransactionHistoryResultEntry,
            TransactionHistoryResultEntryExt, TransactionResultSet, TransactionSet, VecM, WriteXdr,
        };

        let dir = tempfile::tempdir().expect("temp dir");
        let db_path = dir.path().join("db").join("henyey.sqlite");
        let bucket_dir = dir.path().join("buckets");
        let log_file = dir.path().join("put-sources.log");
        std::fs::create_dir_all(dir.path().join("db")).unwrap();
        std::fs::create_dir_all(&bucket_dir).unwrap();

        let put_cmd = format!("echo {{0}} >> {}", log_file.display());

        const CHECKPOINT: u32 = 63;
        const TOTAL_COINS: i64 = 100_000_000_000_000_000;

        let passphrase = "Test SDF Network ; September 2015".to_string();

        // Build an empty bucket list (no genesis entries needed — we just need
        // consistent hashes between bucket list and headers).
        let mut bucket_list = BucketList::new();
        bucket_list.set_bucket_dir(bucket_dir.clone());
        let bucket_list_hash = bucket_list.hash();

        // Seed the database
        let db = henyey_db::Database::open(&db_path).unwrap();

        let mut previous_header_hash = Hash256::ZERO;
        db.with_connection(|conn| {
            for seq in 1..=CHECKPOINT {
                let tx_entry = TransactionHistoryEntry {
                    ledger_seq: seq,
                    tx_set: TransactionSet {
                        previous_ledger_hash: Hash(previous_header_hash.0),
                        txs: VecM::default(),
                    },
                    ext: TransactionHistoryEntryExt::V0,
                };
                let tx_hash = henyey_history::verify::compute_tx_set_hash(
                    &henyey_ledger::TransactionSetVariant::Classic(tx_entry.tx_set.clone()),
                )
                .unwrap();
                let result_entry = TransactionHistoryResultEntry {
                    ledger_seq: seq,
                    tx_result_set: TransactionResultSet {
                        results: VecM::default(),
                    },
                    ext: TransactionHistoryResultEntryExt::V0,
                };
                let result_hash =
                    Hash256::hash(&result_entry.tx_result_set.to_xdr(Limits::none()).unwrap());

                let header = LedgerHeader {
                    ledger_version: 24,
                    previous_ledger_hash: Hash(previous_header_hash.0),
                    scp_value: StellarValue {
                        tx_set_hash: Hash(tx_hash.0),
                        close_time: stellar_xdr::curr::TimePoint(seq as u64),
                        upgrades: VecM::default(),
                        ext: StellarValueExt::Basic,
                    },
                    tx_set_result_hash: Hash(result_hash.0),
                    bucket_list_hash: Hash(bucket_list_hash.0),
                    ledger_seq: seq,
                    total_coins: TOTAL_COINS,
                    fee_pool: 0,
                    inflation_seq: 0,
                    id_pool: seq as u64,
                    base_fee: 100,
                    base_reserve: 5_000_000,
                    max_tx_set_size: 100,
                    skip_list: [Hash([0; 32]), Hash([0; 32]), Hash([0; 32]), Hash([0; 32])],
                    ext: LedgerHeaderExt::V0,
                };
                let header_xdr = header.to_xdr(Limits::none()).unwrap();
                previous_header_hash = compute_header_hash(&header).unwrap();

                conn.store_ledger_header(&header, &header_xdr)?;
                conn.store_tx_history_entry(seq, &tx_entry)?;
                conn.store_tx_result_entry(seq, &result_entry)?;
            }

            conn.set_last_closed_ledger(CHECKPOINT)?;

            let bucket_levels: Vec<(Hash256, Hash256)> = bucket_list
                .levels()
                .iter()
                .map(|level| (level.curr.hash(), level.snap.hash()))
                .collect();
            conn.store_bucket_list(CHECKPOINT, &bucket_levels)?;

            // Store HAS in state (needed by cmd_publish_history)
            let has = henyey_history::publish::build_history_archive_state(
                CHECKPOINT,
                &bucket_list,
                None,
                Some(passphrase.clone()),
            )
            .unwrap();
            let has_json = has.to_json().unwrap();
            conn.set_state(state_keys::HISTORY_ARCHIVE_STATE, &has_json)?;
            conn.enqueue_publish(CHECKPOINT, &has_json)?;

            Ok(())
        })
        .unwrap();

        // Build AppConfig with command archive
        let mut config = henyey_app::config::ConfigBuilder::new()
            .database_path(&db_path)
            .bucket_directory(&bucket_dir)
            .validator(true)
            .node_seed("SAFTEV5U6QDFE2DRMSD7HBE76XG7SQZJD6VIUTHIXTJGO77RUQYVURLA")
            .build();
        config.history.archives = vec![henyey_app::config::HistoryArchiveEntry {
            name: "staging-test".to_string(),
            url: "file:///unused".to_string(),
            get_enabled: false,
            put_enabled: true,
            put: Some(put_cmd),
            mkdir: Some("true".to_string()),
        }];

        // Run publish with force=true to ensure it publishes
        cmd_publish_history(config, true).await.unwrap();

        // Verify: log file must exist and contain at least one source path
        let log_contents = std::fs::read_to_string(&log_file)
            .expect("put command log file should exist (publish must have run)");
        let paths: Vec<&str> = log_contents.lines().filter(|l| !l.is_empty()).collect();
        assert!(
            !paths.is_empty(),
            "publish must have uploaded at least one file"
        );

        // Every source path must start with <bucket_dir>/tmp/
        let expected_prefix = bucket_dir.join("tmp");
        for path in &paths {
            assert!(
                path.starts_with(expected_prefix.to_str().unwrap()),
                "staging path {:?} does not start with {:?}",
                path,
                expected_prefix
            );
        }

        // Staging directory should be cleaned up after successful publish
        let tmp_dir = bucket_dir.join("tmp");
        if tmp_dir.exists() {
            let remaining: Vec<_> = std::fs::read_dir(&tmp_dir)
                .unwrap()
                .filter_map(|e| e.ok())
                .collect();
            assert!(
                remaining.is_empty(),
                "staging dir should be empty after publish, found: {:?}",
                remaining.iter().map(|e| e.path()).collect::<Vec<_>>()
            );
        }
    }

    #[test]
    fn test_write_scp_history_file_uses_record_marks() {
        use flate2::read::GzDecoder;
        use henyey_history::paths::checkpoint_path;
        use tempfile::TempDir;

        let tmp = TempDir::new().unwrap();
        let entry = stellar_xdr::curr::ScpHistoryEntry::V0(stellar_xdr::curr::ScpHistoryEntryV0 {
            quorum_sets: stellar_xdr::curr::VecM::default(),
            ledger_messages: stellar_xdr::curr::LedgerScpMessages {
                ledger_seq: 63,
                messages: stellar_xdr::curr::VecM::default(),
            },
        });

        write_scp_history_file(tmp.path(), 63, std::slice::from_ref(&entry)).unwrap();

        let path = tmp.path().join(checkpoint_path("scp", 63, "xdr.gz"));
        let file = std::fs::File::open(path).unwrap();
        let mut decoder = GzDecoder::new(file);
        let mut bytes = Vec::new();
        decoder.read_to_end(&mut bytes).unwrap();

        assert!(bytes.len() > 4);
        let mark = u32::from_be_bytes(bytes[0..4].try_into().unwrap());
        assert_ne!(mark & 0x8000_0000, 0);

        let payload_len = (mark & 0x7fff_ffff) as usize;
        assert_eq!(payload_len, bytes.len() - 4);

        let parsed = stellar_xdr::curr::ScpHistoryEntry::from_xdr(
            &bytes[4..],
            stellar_xdr::curr::Limits::none(),
        )
        .unwrap();
        assert_eq!(parsed, entry);
    }

    #[test]
    fn test_write_scp_history_file_no_temp_files_remain() {
        use tempfile::TempDir;

        let tmp = TempDir::new().unwrap();
        let entry = stellar_xdr::curr::ScpHistoryEntry::V0(stellar_xdr::curr::ScpHistoryEntryV0 {
            quorum_sets: stellar_xdr::curr::VecM::default(),
            ledger_messages: stellar_xdr::curr::LedgerScpMessages {
                ledger_seq: 63,
                messages: stellar_xdr::curr::VecM::default(),
            },
        });

        write_scp_history_file(tmp.path(), 63, std::slice::from_ref(&entry)).unwrap();

        // No .tmp files should remain anywhere in the output tree
        fn has_tmp_files(dir: &std::path::Path) -> bool {
            if !dir.exists() {
                return false;
            }
            for entry in std::fs::read_dir(dir).unwrap() {
                let entry = entry.unwrap();
                let path = entry.path();
                if path.is_dir() {
                    if has_tmp_files(&path) {
                        return true;
                    }
                } else if path
                    .file_name()
                    .unwrap()
                    .to_string_lossy()
                    .contains(".tmp.")
                {
                    return true;
                }
            }
            false
        }
        assert!(
            !has_tmp_files(tmp.path()),
            "temp files should be cleaned up after atomic write"
        );
    }

    #[test]
    fn test_write_root_has_produces_valid_json_no_temp_files() {
        use tempfile::TempDir;

        let tmp = TempDir::new().unwrap();
        let has = henyey_history::publish::build_history_archive_state(
            0,
            &henyey_bucket::BucketList::new(),
            None,
            None,
        )
        .unwrap();
        write_root_has(tmp.path(), &has).unwrap();

        let root_path = tmp.path().join(henyey_history::paths::root_has_path());
        assert!(root_path.exists(), "root HAS file should be written");

        // Verify it's valid JSON matching the original HAS
        let content = std::fs::read_to_string(&root_path).unwrap();
        let parsed: serde_json::Value = serde_json::from_str(&content).unwrap();
        assert!(parsed.is_object());

        // No temp files in the tree
        fn has_tmp_files(dir: &std::path::Path) -> bool {
            if !dir.exists() {
                return false;
            }
            for entry in std::fs::read_dir(dir).unwrap() {
                let entry = entry.unwrap();
                let path = entry.path();
                if path.is_dir() {
                    if has_tmp_files(&path) {
                        return true;
                    }
                } else if path
                    .file_name()
                    .unwrap()
                    .to_string_lossy()
                    .contains(".tmp.")
                {
                    return true;
                }
            }
            false
        }
        assert!(
            !has_tmp_files(tmp.path()),
            "temp files should be cleaned up after atomic write"
        );
    }
}
