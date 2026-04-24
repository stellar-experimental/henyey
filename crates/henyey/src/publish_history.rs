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
use henyey_history::verify;
use henyey_ledger::compute_header_hash;
use henyey_ledger::TransactionSetVariant;
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
            tx_entries.push(tx_entry);

            let tx_result = db
                .get_tx_result_entry(seq)?
                .ok_or_else(|| anyhow::anyhow!("Missing tx result entry {}", seq))?;
            tx_results.push(tx_result);
        }

        let scp_entries = build_scp_history_entries(&db, start_ledger, checkpoint)?;

        for ((header_entry, tx_entry), tx_result_entry) in
            headers.iter().zip(tx_entries.iter()).zip(tx_results.iter())
        {
            let header = &header_entry.header;
            let tx_set = TransactionSetVariant::from(tx_entry);
            let tx_set_hash = verify::compute_tx_set_hash(&tx_set)?;
            let expected_tx_set = Hash256::from(header.scp_value.tx_set_hash.0);
            if tx_set_hash != expected_tx_set {
                anyhow::bail!(
                    "Tx set hash mismatch at {} (expected {}, got {})",
                    header.ledger_seq,
                    expected_tx_set.to_hex(),
                    tx_set_hash.to_hex()
                );
            }

            let tx_result_hash = Hash256::hash_xdr(&tx_result_entry.tx_result_set);
            let expected_tx_result = Hash256::from(header.tx_set_result_hash.0);
            if tx_result_hash != expected_tx_result {
                anyhow::bail!(
                    "Tx result hash mismatch at {} (expected {}, got {})",
                    header.ledger_seq,
                    expected_tx_result.to_hex(),
                    tx_result_hash.to_hex()
                );
            }
        }

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
            let sanitized_name = config
                .node
                .name
                .chars()
                .map(|c| if c.is_ascii_alphanumeric() { c } else { '_' })
                .collect::<String>();
            let publish_dir = std::env::temp_dir().join(format!(
                "rs-stellar-core-publish-{}-{}",
                sanitized_name, checkpoint
            ));
            if publish_dir.exists() {
                std::fs::remove_dir_all(&publish_dir)?;
            }
            std::fs::create_dir_all(&publish_dir)?;

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
            Some(publish_dir)
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

        if let Some(ref publish_dir) = command_publish_dir {
            write_scp_history_file(publish_dir, checkpoint, &scp_entries)?;
            for archive in &command_targets {
                upload_publish_directory(archive, publish_dir)?;
                println!("OK (command: {})", archive.name);
                published_any = true;
            }
        }

        if let Some(publish_dir) = command_publish_dir {
            if let Err(err) = std::fs::remove_dir_all(&publish_dir) {
                tracing::warn!(
                    path = %publish_dir.display(),
                    error = %err,
                    "Failed to remove publish temp directory"
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
    std::fs::write(&root_path, has.to_json()?)?;
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
    use flate2::write::GzEncoder;
    use flate2::Compression;
    use henyey_history::paths::checkpoint_path;
    use std::io::Write;
    use stellar_xdr::curr::{Limits, WriteXdr};

    let path = base_dir.join(checkpoint_path("scp", checkpoint, "xdr.gz"));
    super::create_parent_dir(&path)?;
    let file = std::fs::File::create(&path)?;
    let mut encoder = GzEncoder::new(file, Compression::default());

    for entry in entries {
        let xdr = entry.to_xdr(Limits::none())?;
        /// XDR record mark: high bit indicates last/only fragment.
        const XDR_RECORD_MARK: u32 = 0x8000_0000;
        let marked_len = (xdr.len() as u32) | XDR_RECORD_MARK;
        encoder.write_all(&marked_len.to_be_bytes())?;
        encoder.write_all(&xdr)?;
    }
    encoder.finish()?;
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
}
