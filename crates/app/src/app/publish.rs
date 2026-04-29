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
        if !self.config.history.publish_enabled() {
            tracing::debug!(
                total_archives = self.config.history.archives.len(),
                "publish: no writable command archives found"
            );
            return;
        }

        // Load one queued checkpoint
        let queued = match self
            .db_blocking("load-publish-queue", |db| {
                db.load_publish_queue(Some(1)).map_err(Into::into)
            })
            .await
        {
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

        tokio::task::spawn_blocking(move || {
            let command_archives: Vec<_> = app
                .config
                .history
                .archives
                .iter()
                .filter(|a| a.put_enabled && a.put.is_some())
                .collect();

            match app.publish_single_checkpoint(checkpoint, &command_archives) {
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
    fn publish_single_checkpoint(
        &self,
        checkpoint: u32,
        archives: &[&crate::config::HistoryArchiveEntry],
    ) -> anyhow::Result<()> {
        use henyey_bucket::BucketList;
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

        let bucket_manager = self.bucket_manager.clone();

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
        manager.publish_checkpoint(
            checkpoint,
            &headers,
            &tx_entries,
            &tx_results,
            &bucket_list,
            Some(&has),
        )?;

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
                        henyey_common::fs_utils::atomic_gzip_copy(&src, &dest)?;
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
        henyey_common::fs_utils::atomic_write_bytes(&root_path, has_json.as_bytes())?;

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
    henyey_common::fs_utils::atomic_write_with(&path, |file| {
        let mut encoder = GzEncoder::new(&mut *file, Compression::default());
        for entry in entries {
            henyey_history::write_record_marked_xdr(&mut encoder, entry)?;
        }
        encoder.finish()?;
        Ok(())
    })?;
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

#[cfg(test)]
mod tests {
    use super::*;

    use std::sync::Arc;
    use std::time::Duration;

    use henyey_bucket::{BucketList, HotArchiveBucketList};
    use henyey_common::Hash256;
    use henyey_db::queries::{
        BucketListQueries, HistoryQueries, LedgerQueries, PublishQueueQueries, StateQueries,
    };
    use henyey_db::schema::state_keys;
    use henyey_history::publish::build_history_archive_state;
    use henyey_ledger::TransactionSetVariant;
    use stellar_xdr::curr::{
        Hash, LedgerHeader, LedgerHeaderExt, Limits, StellarValue, StellarValueExt,
        TransactionHistoryEntry, TransactionHistoryEntryExt, TransactionHistoryResultEntry,
        TransactionHistoryResultEntryExt, TransactionResultSet, TransactionSet, VecM, WriteXdr,
    };

    const CHECKPOINT: u32 = 63;
    const TOTAL_COINS: i64 = 100_000_000_000_000_000;

    fn combined_bucket_hash(live: Hash256, hot: Hash256) -> Hash256 {
        let mut bytes = Vec::with_capacity(64);
        bytes.extend_from_slice(live.as_bytes());
        bytes.extend_from_slice(hot.as_bytes());
        Hash256::hash(&bytes)
    }

    fn empty_tx_history(seq: u32, previous_ledger_hash: Hash256) -> TransactionHistoryEntry {
        TransactionHistoryEntry {
            ledger_seq: seq,
            tx_set: TransactionSet {
                previous_ledger_hash: Hash(previous_ledger_hash.0),
                txs: VecM::default(),
            },
            ext: TransactionHistoryEntryExt::V0,
        }
    }

    fn empty_tx_result(seq: u32) -> TransactionHistoryResultEntry {
        TransactionHistoryResultEntry {
            ledger_seq: seq,
            tx_result_set: TransactionResultSet {
                results: VecM::default(),
            },
            ext: TransactionHistoryResultEntryExt::V0,
        }
    }

    fn make_header(
        seq: u32,
        previous_ledger_hash: Hash256,
        tx_set_hash: Hash256,
        tx_result_hash: Hash256,
        bucket_list_hash: Hash256,
    ) -> LedgerHeader {
        LedgerHeader {
            ledger_version: 24,
            previous_ledger_hash: Hash(previous_ledger_hash.0),
            scp_value: StellarValue {
                tx_set_hash: Hash(tx_set_hash.0),
                close_time: stellar_xdr::curr::TimePoint(seq as u64),
                upgrades: VecM::default(),
                ext: StellarValueExt::Basic,
            },
            tx_set_result_hash: Hash(tx_result_hash.0),
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
        }
    }

    fn bucket_levels(bucket_list: &BucketList) -> Vec<(Hash256, Hash256)> {
        bucket_list
            .levels()
            .iter()
            .map(|level| (level.curr.hash(), level.snap.hash()))
            .collect()
    }

    fn canonical_bucket_file_count(path: &std::path::Path) -> usize {
        files_matching(path, |path| {
            path.file_name()
                .and_then(|name| name.to_str())
                .is_some_and(|name| name.ends_with(".bucket.xdr"))
        })
    }

    fn archive_bucket_file_count(path: &std::path::Path) -> usize {
        files_matching(path, |path| {
            path.to_string_lossy().contains("/bucket/")
                && path
                    .file_name()
                    .and_then(|name| name.to_str())
                    .is_some_and(|name| name.starts_with("bucket-") && name.ends_with(".xdr.gz"))
        })
    }

    fn files_matching(
        path: &std::path::Path,
        predicate: impl Fn(&std::path::Path) -> bool,
    ) -> usize {
        fn visit(path: &std::path::Path, predicate: &dyn Fn(&std::path::Path) -> bool) -> usize {
            if !path.exists() {
                return 0;
            }
            if path.is_file() {
                return usize::from(predicate(path));
            }
            std::fs::read_dir(path)
                .unwrap()
                .map(|entry| visit(&entry.unwrap().path(), predicate))
                .sum()
        }

        visit(path, &predicate)
    }

    async fn wait_for_publish_queue_to_drain(app: &App) {
        let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
        loop {
            let queue = app
                .db_blocking("check-publish-queue", |db| {
                    db.load_publish_queue(None).map_err(Into::into)
                })
                .await
                .unwrap();
            if queue.is_empty() {
                return;
            }
            if tokio::time::Instant::now() >= deadline {
                panic!(
                    "publish queue did not drain; queue={queue:?}, publish_in_progress={}",
                    app.publish_in_progress.load(Ordering::SeqCst)
                );
            }
            tokio::time::sleep(Duration::from_millis(25)).await;
        }
    }

    #[test]
    fn app_runtime_does_not_reconstruct_bucket_manager_from_config() {
        let app_dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("src/app");
        let constructors = [
            ["BucketManager::", "with_cache_size"].concat(),
            ["BucketManager::", "new"].concat(),
        ];
        let config_bucket_dir = ["config.buckets", ".directory"].concat();
        let mut offenders = Vec::new();
        let mut stack = vec![app_dir];

        while let Some(path) = stack.pop() {
            for entry in std::fs::read_dir(&path).unwrap() {
                let path = entry.unwrap().path();
                if path.is_dir() {
                    stack.push(path);
                    continue;
                }
                if path.extension().and_then(|ext| ext.to_str()) != Some("rs") {
                    continue;
                }

                let source = std::fs::read_to_string(&path).unwrap();
                if path.file_name().and_then(|name| name.to_str()) == Some("mod.rs")
                    && source.matches(&constructors[0]).count() == 1
                    && !source.contains(&constructors[1])
                {
                    continue;
                }
                if constructors
                    .iter()
                    .any(|constructor| source.contains(constructor))
                    && source.contains(&config_bucket_dir)
                {
                    offenders.push(path.clone());
                }
            }
        }

        assert!(
            offenders.is_empty(),
            "app runtime must use App::bucket_manager after construction; offenders: {offenders:?}"
        );
    }

    #[tokio::test]
    async fn custom_bucket_directory_survives_genesis_publish_and_restore() {
        let dir = tempfile::tempdir().expect("temp dir");
        let db_dir = dir.path().join("db");
        let db_path = db_dir.join("henyey.sqlite");
        let custom_bucket_dir = dir.path().join("custom-buckets");
        let derived_bucket_dir = db_dir.join("buckets");
        let archive_dir = dir.path().join("archive");
        std::fs::create_dir_all(&db_dir).unwrap();
        std::fs::create_dir_all(&custom_bucket_dir).unwrap();
        std::fs::create_dir_all(&archive_dir).unwrap();

        let mut config = crate::config::ConfigBuilder::new()
            .database_path(&db_path)
            .bucket_directory(&custom_bucket_dir)
            .validator(true)
            .node_seed("SAFTEV5U6QDFE2DRMSD7HBE76XG7SQZJD6VIUTHIXTJGO77RUQYVURLA")
            .build();
        config.history.archives = vec![crate::config::HistoryArchiveEntry {
            name: "local-test".to_string(),
            url: format!("file://{}", archive_dir.display()),
            get_enabled: false,
            put_enabled: true,
            put: Some(format!(
                "mkdir -p {}/$(dirname {{1}}) && cp {{0}} {}/{{1}}",
                archive_dir.display(),
                archive_dir.display()
            )),
            mkdir: Some(format!("mkdir -p {}/{{0}}", archive_dir.display())),
        }];

        {
            let app = Arc::new(App::new(config.clone()).await.unwrap());
            app.set_self_arc().await;
            assert_eq!(app.bucket_manager.bucket_dir(), custom_bucket_dir.as_path());

            let passphrase = app.config.network.passphrase.clone();
            let genesis_entries = App::build_genesis_entries(&passphrase, 0, TOTAL_COINS);
            let live_probe_header = make_header(
                1,
                Hash256::ZERO,
                Hash256::ZERO,
                Hash256::ZERO,
                Hash256::ZERO,
            );
            let mut probe_bucket_list = BucketList::new();
            probe_bucket_list.set_bucket_dir(custom_bucket_dir.clone());
            probe_bucket_list
                .add_batch(
                    1,
                    0,
                    stellar_xdr::curr::BucketListType::Live,
                    genesis_entries.clone(),
                    vec![],
                    vec![],
                )
                .unwrap();
            let live_hash = probe_bucket_list.hash();
            let hot_archive = HotArchiveBucketList::new();
            let combined_hash = combined_bucket_hash(live_hash, hot_archive.hash());

            let live_header = LedgerHeader {
                bucket_list_hash: Hash(live_hash.0),
                ..live_probe_header
            };
            let mut bucket_list =
                App::create_genesis_bucket_list(&custom_bucket_dir, genesis_entries, &live_header)
                    .unwrap();
            bucket_list.set_ledger_seq(CHECKPOINT);
            let has = build_history_archive_state(
                CHECKPOINT,
                &bucket_list,
                Some(&hot_archive),
                Some(passphrase.clone()),
            )
            .unwrap();
            let has_json = has.to_json().unwrap();

            let empty_result = empty_tx_result(1);
            let empty_result_hash = Hash256::hash(
                &empty_result
                    .tx_result_set
                    .to_xdr(Limits::none())
                    .expect("empty result xdr"),
            );

            let mut headers_and_history = Vec::new();
            let mut previous_header_hash = Hash256::ZERO;
            let mut checkpoint_header = None;
            let mut checkpoint_header_hash = Hash256::ZERO;
            for seq in 1..=CHECKPOINT {
                let tx_entry = empty_tx_history(seq, previous_header_hash);
                let tx_hash = if seq == 1 {
                    Hash256::ZERO
                } else {
                    henyey_history::verify::compute_tx_set_hash(&TransactionSetVariant::Classic(
                        tx_entry.tx_set.clone(),
                    ))
                    .unwrap()
                };
                let result_entry = empty_tx_result(seq);
                let result_hash = if seq == 1 {
                    Hash256::ZERO
                } else {
                    empty_result_hash
                };
                let header = make_header(
                    seq,
                    previous_header_hash,
                    tx_hash,
                    result_hash,
                    combined_hash,
                );
                let header_xdr = header.to_xdr(Limits::none()).unwrap();
                previous_header_hash = henyey_ledger::compute_header_hash(&header).unwrap();
                if seq == CHECKPOINT {
                    checkpoint_header_hash = previous_header_hash;
                    checkpoint_header = Some(header.clone());
                }
                headers_and_history.push((header, header_xdr, tx_entry, result_entry));
            }

            app.db_blocking("seed-custom-bucket-checkpoint", {
                let bucket_levels = bucket_levels(&bucket_list);
                let has_json = has_json.clone();
                let headers_and_history = headers_and_history.clone();
                move |db| {
                    db.with_connection(|conn| {
                        for (header, header_xdr, tx_entry, result_entry) in &headers_and_history {
                            conn.store_ledger_header(header, header_xdr)?;
                            conn.store_tx_history_entry(header.ledger_seq, tx_entry)?;
                            conn.store_tx_result_entry(header.ledger_seq, result_entry)?;
                        }
                        conn.set_last_closed_ledger(CHECKPOINT)?;
                        conn.store_bucket_list(CHECKPOINT, &bucket_levels)?;
                        conn.set_state(state_keys::HISTORY_ARCHIVE_STATE, &has_json)?;
                        conn.enqueue_publish(CHECKPOINT, &has_json)?;
                        Ok::<_, henyey_db::DbError>(())
                    })
                    .map_err(Into::into)
                }
            })
            .await
            .unwrap();

            app.ledger_manager
                .initialize(
                    bucket_list,
                    hot_archive,
                    checkpoint_header.expect("checkpoint header"),
                    checkpoint_header_hash,
                )
                .unwrap();

            assert!(canonical_bucket_file_count(&custom_bucket_dir) > 0);
            assert_eq!(canonical_bucket_file_count(&derived_bucket_dir), 0);

            app.maybe_publish_history().await;
            wait_for_publish_queue_to_drain(&app).await;

            assert!(archive_bucket_file_count(&archive_dir) > 0);
            assert_eq!(canonical_bucket_file_count(&derived_bucket_dir), 0);
        }

        let second_app = App::new(config).await.unwrap();
        assert_eq!(
            second_app.load_last_known_ledger().await.unwrap(),
            RestoreResult::Restored
        );
        assert_eq!(
            second_app.bucket_manager.bucket_dir(),
            custom_bucket_dir.as_path()
        );
        assert_eq!(canonical_bucket_file_count(&derived_bucket_dir), 0);
    }

    #[test]
    fn test_write_scp_history_file_valid_gzip_no_temp_files() {
        use flate2::read::GzDecoder;
        use henyey_history::paths::checkpoint_path;
        use std::io::Read;
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

        // Verify the file is valid gzip with correct XDR record marks
        let path = tmp.path().join(checkpoint_path("scp", 63, "xdr.gz"));
        assert!(path.exists());
        let file = std::fs::File::open(&path).unwrap();
        let mut decoder = GzDecoder::new(file);
        let mut bytes = Vec::new();
        decoder.read_to_end(&mut bytes).unwrap();
        assert!(bytes.len() > 4);

        let mark = u32::from_be_bytes(bytes[0..4].try_into().unwrap());
        assert_ne!(
            mark & 0x8000_0000,
            0,
            "high bit must be set for record mark"
        );

        // No temp files
        assert_eq!(
            files_matching(tmp.path(), |p| p
                .file_name()
                .unwrap()
                .to_string_lossy()
                .contains(".tmp.")),
            0,
            "no temp files should remain"
        );
    }
}
