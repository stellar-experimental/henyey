//! History publishing: SCP history assembly, checkpoint generation, and archive uploads.

use super::*;

impl App {
    /// Drain the publish queue, publishing one checkpoint at a time.
    ///
    /// This is called after each ledger close to check if there are queued
    /// checkpoints ready to be published. For command-based archives (e.g.,
    /// local `cp`/`mkdir` archives used in quickstart local mode), this builds
    /// the checkpoint files in a temp directory and uploads them.
    ///
    /// Publishing runs as a background task to avoid blocking the event loop.
    /// The `publish_in_progress` flag prevents concurrent publishes.
    pub async fn maybe_publish_history(&self) {
        // Only validators publish
        if !self.is_validator {
            return;
        }

        // Don't start a new publish if one is already running in the background
        if self.publish_in_progress.load(Ordering::SeqCst) {
            return;
        }

        // Check for writable archives with put commands
        let has_writable = self
            .config
            .history
            .archives
            .iter()
            .any(|a| a.put_enabled && a.put.is_some());

        if !has_writable {
            tracing::debug!(
                total_archives = self.config.history.archives.len(),
                "publish: no writable command archives found"
            );
            return;
        }

        // Load one queued checkpoint
        let queued = match self.db.load_publish_queue(Some(1)) {
            Ok(q) => q,
            Err(e) => {
                tracing::warn!(error = %e, "Failed to load publish queue");
                return;
            }
        };

        let checkpoint = match queued.first() {
            Some(&cp) => cp,
            None => return,
        };

        // Don't publish a checkpoint that's still being closed
        let current_ledger = self.current_ledger_seq();
        if checkpoint > current_ledger {
            return;
        }

        // Try to acquire the publish lock
        if self.publish_in_progress.swap(true, Ordering::SeqCst) {
            return; // Another call raced us
        }

        tracing::info!(
            checkpoint,
            "Publishing checkpoint to history archive (background)"
        );

        // Spawn the publish as a background task via self_arc to avoid
        // blocking the event loop. Publishing involves gzip compression
        // of XDR files and shell command execution (cp/mkdir), which can
        // take 30-50 seconds on large checkpoints.
        let app = {
            let weak = self.self_arc.read().await;
            match weak.upgrade() {
                Some(arc) => arc,
                None => {
                    self.publish_in_progress.store(false, Ordering::SeqCst);
                    tracing::warn!("Cannot spawn publish task: self_arc unavailable");
                    return;
                }
            }
        };

        tokio::spawn(async move {
            let command_archives: Vec<_> = app
                .config
                .history
                .archives
                .iter()
                .filter(|a| a.put_enabled && a.put.is_some())
                .collect();

            match app
                .publish_single_checkpoint(checkpoint, &command_archives)
                .await
            {
                Ok(()) => {
                    if let Err(e) = app.db.remove_publish(checkpoint) {
                        tracing::warn!(checkpoint, error = %e, "Failed to dequeue published checkpoint");
                    }
                    tracing::info!(checkpoint, "Checkpoint published successfully");
                }
                Err(e) => {
                    tracing::warn!(checkpoint, error = %e, "Failed to publish checkpoint");
                }
            }

            app.publish_in_progress.store(false, Ordering::SeqCst);
        });
    }

    /// Publish a single checkpoint to all command-based archives.
    async fn publish_single_checkpoint(
        &self,
        checkpoint: u32,
        archives: &[&crate::config::HistoryArchiveEntry],
    ) -> anyhow::Result<()> {
        use henyey_bucket::{BucketList, BucketManager};
        use henyey_history::paths::root_has_path;
        use henyey_history::publish::{PublishConfig, PublishManager};

        let freq = checkpoint_frequency();

        // Calculate ledger range for this checkpoint
        let start_ledger = checkpoint.saturating_sub(freq - 1);
        let start_ledger = if start_ledger == 0 { 1 } else { start_ledger };

        // Load headers, transactions, and results from DB
        let mut headers = Vec::new();
        let mut tx_entries = Vec::new();
        let mut tx_results = Vec::new();

        for seq in start_ledger..=checkpoint {
            let header = self
                .db
                .get_ledger_header(seq)?
                .ok_or_else(|| anyhow::anyhow!("Missing ledger header {}", seq))?;
            let hash = henyey_ledger::compute_header_hash(&header)?;
            headers.push(stellar_xdr::curr::LedgerHeaderHistoryEntry {
                header,
                hash: stellar_xdr::curr::Hash(hash.0),
                ext: stellar_xdr::curr::LedgerHeaderHistoryEntryExt::V0,
            });

            let tx_entry = self
                .db
                .get_tx_history_entry(seq)?
                .ok_or_else(|| anyhow::anyhow!("Missing tx history entry {}", seq))?;
            tx_entries.push(tx_entry);

            let tx_result = self
                .db
                .get_tx_result_entry(seq)?
                .ok_or_else(|| anyhow::anyhow!("Missing tx result entry {}", seq))?;
            tx_results.push(tx_result);
        }

        // Build SCP history entries
        let scp_entries = self.build_scp_history_entries(start_ledger, checkpoint)?;

        // Load bucket list for this checkpoint
        let levels = self
            .db
            .load_bucket_list(checkpoint)?
            .ok_or_else(|| anyhow::anyhow!("Missing bucket list snapshot {}", checkpoint))?;

        let bucket_manager = BucketManager::with_cache_size(
            self.config.buckets.directory.clone(),
            self.config.buckets.cache_size,
        )?;

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

        // Load the HAS that was captured at checkpoint close time.
        // This includes hot archive bucket hashes (protocol >= 23) which
        // are only available at close time, not at publish time.
        let has_json = self
            .db
            .load_publish_has(checkpoint)?
            .ok_or_else(|| anyhow::anyhow!("Missing HAS for checkpoint {}", checkpoint))?;
        let has: henyey_history::HistoryArchiveState = serde_json::from_str(&has_json)?;

        // Build checkpoint files in a temp directory
        let publish_dir = std::env::temp_dir().join(format!("henyey-publish-{}", checkpoint));
        if publish_dir.exists() {
            std::fs::remove_dir_all(&publish_dir)?;
        }
        std::fs::create_dir_all(&publish_dir)?;

        let publish_config = PublishConfig {
            local_path: publish_dir.clone(),
            network_passphrase: Some(self.config.network.passphrase.clone()),
            ..Default::default()
        };
        let manager = PublishManager::new(publish_config);
        manager
            .publish_checkpoint(
                checkpoint,
                &headers,
                &tx_entries,
                &tx_results,
                &bucket_list,
                Some(&has),
            )
            .await?;

        // Write hot archive bucket files if the HAS includes them.
        // These are written separately because publish_checkpoint only
        // handles live bucket files (from the BucketList). Hot archive
        // bucket files were persisted to disk during ledger close.
        if let Some(ref hot_buckets) = has.hot_archive_buckets {
            use henyey_history::paths::bucket_path as archive_bucket_path;
            let bucket_dir = bucket_manager.bucket_dir();
            for level in hot_buckets {
                for hex_hash in [&level.curr, &level.snap] {
                    let hash = henyey_common::Hash256::from_hex(hex_hash)
                        .map_err(|e| anyhow::anyhow!("Invalid hot archive bucket hash: {}", e))?;
                    if hash.is_zero() {
                        continue;
                    }
                    let dest = publish_dir.join(archive_bucket_path(&hash));
                    if dest.exists() {
                        continue;
                    }
                    if let Some(parent) = dest.parent() {
                        std::fs::create_dir_all(parent)?;
                    }
                    // Find the backing file on disk
                    let src = bucket_dir.join(henyey_bucket::canonical_bucket_filename(&hash));
                    if src.exists() {
                        use flate2::write::GzEncoder;
                        use flate2::Compression;
                        use std::io::{Read, Write};
                        let file = std::fs::File::create(&dest)?;
                        let mut encoder = GzEncoder::new(file, Compression::default());
                        let mut src_file = std::fs::File::open(&src)?;
                        let mut buf = [0u8; 64 * 1024];
                        loop {
                            let n = src_file.read(&mut buf)?;
                            if n == 0 {
                                break;
                            }
                            encoder.write_all(&buf[..n])?;
                        }
                        encoder.finish()?;
                        tracing::debug!(hash = %hash.to_hex(), "Published hot archive bucket");
                    } else {
                        anyhow::bail!(
                            "Hot archive bucket file not found on disk: hash={}, path={}",
                            hash.to_hex(),
                            src.display()
                        );
                    }
                }
            }
        }

        // Write SCP history
        write_scp_history_file(&publish_dir, checkpoint, &scp_entries)?;

        // Write root HAS
        let root_path = publish_dir.join(root_has_path());
        if let Some(parent) = root_path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        std::fs::write(&root_path, &has_json)?;

        // Upload to each command-based archive
        for archive in archives {
            let put_cmd = archive.put.as_ref().unwrap();
            let mkdir_cmd = archive.mkdir.as_deref();

            upload_publish_directory(put_cmd, mkdir_cmd, &publish_dir)?;
        }

        // Clean up temp directory
        if let Err(e) = std::fs::remove_dir_all(&publish_dir) {
            tracing::warn!(
                path = %publish_dir.display(),
                error = %e,
                "Failed to remove publish temp directory"
            );
        }

        Ok(())
    }

    /// Build SCP history entries for a checkpoint range from the database.
    fn build_scp_history_entries(
        &self,
        start_ledger: u32,
        checkpoint: u32,
    ) -> anyhow::Result<Vec<stellar_xdr::curr::ScpHistoryEntry>> {
        use henyey_common::Hash256;
        use std::collections::HashSet;
        use stellar_xdr::curr::{LedgerScpMessages, ScpHistoryEntry, ScpHistoryEntryV0};

        let mut entries = Vec::new();
        for seq in start_ledger..=checkpoint {
            let envelopes = self.db.load_scp_history(seq)?;
            if envelopes.is_empty() {
                continue;
            }

            let mut qset_hashes = HashSet::new();
            for envelope in &envelopes {
                let hash = henyey_common::scp_quorum_set_hash(&envelope.statement);
                qset_hashes.insert(Hash256::from_bytes(hash.0));
            }

            let mut qset_hashes_vec: Vec<_> = qset_hashes.into_iter().collect();
            qset_hashes_vec.sort_unstable_by(|a, b| a.as_bytes().cmp(b.as_bytes()));

            let mut qsets = Vec::new();
            for hash in qset_hashes_vec {
                match self.db.load_scp_quorum_set(&hash)? {
                    Some(qset) => qsets.push(qset),
                    None => {
                        anyhow::bail!(
                            "Missing quorum set {} referenced by SCP history at ledger {}",
                            hash,
                            seq
                        );
                    }
                }
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
}

/// Write SCP history entries to a gzip-compressed XDR file.
fn write_scp_history_file(
    base_dir: &std::path::Path,
    checkpoint: u32,
    entries: &[stellar_xdr::curr::ScpHistoryEntry],
) -> anyhow::Result<()> {
    use flate2::write::GzEncoder;
    use flate2::Compression;
    use henyey_history::paths::checkpoint_path;

    let path = base_dir.join(checkpoint_path("scp", checkpoint, "xdr.gz"));
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let file = std::fs::File::create(&path)?;
    let mut encoder = GzEncoder::new(file, Compression::default());

    for entry in entries {
        henyey_history::write_record_marked_xdr(&mut encoder, entry)?;
    }
    encoder.finish()?;
    Ok(())
}

/// Upload all files from a publish directory to a remote archive using shell commands.
fn upload_publish_directory(
    put_cmd: &str,
    mkdir_cmd: Option<&str>,
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

        if let Some(mkdir) = mkdir_cmd {
            if let Some(parent) = rel.parent() {
                if !parent.as_os_str().is_empty() {
                    let remote_dir = path_to_unix_string(parent);
                    if created_dirs.insert(remote_dir.clone()) {
                        let cmd = mkdir.replace("{0}", &remote_dir);
                        run_shell_command(&cmd)?;
                    }
                }
            }
        }

        let cmd = put_cmd
            .replace("{0}", file.to_string_lossy().as_ref())
            .replace("{1}", &rel_str);
        run_shell_command(&cmd)?;
    }

    Ok(())
}

/// Recursively collect all files under a directory.
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

/// Convert a path to a Unix-style string with forward slashes.
fn path_to_unix_string(path: &std::path::Path) -> String {
    path.components()
        .map(|c| c.as_os_str().to_string_lossy().into_owned())
        .collect::<Vec<_>>()
        .join("/")
}

/// Execute a shell command and return an error if it fails.
fn run_shell_command(cmd: &str) -> anyhow::Result<()> {
    use std::process::Command;

    let status = Command::new("sh").arg("-c").arg(cmd).status()?;
    if status.success() {
        Ok(())
    } else {
        anyhow::bail!("command failed (exit {}): {}", status, cmd);
    }
}
