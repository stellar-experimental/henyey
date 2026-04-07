//! Crash-safe checkpoint building for history archives.
//!
//! This module provides the `CheckpointBuilder` struct for building checkpoints
//! in a crash-safe manner. It writes checkpoint data to `.dirty` temporary files
//! first, then atomically renames them to final paths on commit.
//!
//! # Crash Safety
//!
//! The checkpoint builder follows these principles for crash safety:
//!
//! 1. **Write to dirty files**: All data is first written to `.dirty` temporary files
//! 2. **Fsync after writes**: Data is fsynced to disk before proceeding
//! 3. **Atomic rename on commit**: Dirty files are atomically renamed to final paths
//! 4. **Recovery on startup**: The `cleanup()` method recovers from crashes
//!
//! # Recovery Scenarios
//!
//! On startup, `cleanup(lcl)` handles these scenarios:
//!
//! - **Both dirty and final exist**: Delete dirty (leftover from completed checkpoint)
//! - **Only dirty exists**: Truncate to LCL if needed, rename to final
//! - **Only final exists**: Validate it ends at correct ledger
//! - **Neither exists**: First run or publish was disabled
//!
//! # Example
//!
//! ```no_run
//! use henyey_history::checkpoint_builder::CheckpointBuilder;
//! use std::path::PathBuf;
//!
//! let mut builder = CheckpointBuilder::new(PathBuf::from("/tmp/history"));
//!
//! // On startup, clean up any partial state
//! builder.cleanup(63)?;  // LCL = 63
//!
//! // During ledger close, append data
//! // builder.append_ledger_header(&header)?;
//! // builder.append_transaction_set(&tx_set, &results)?;
//!
//! // At checkpoint boundary, commit
//! builder.checkpoint_complete(63)?;
//! # Ok::<(), henyey_history::HistoryError>(())
//! ```

use crate::paths::{checkpoint_path, checkpoint_path_dirty, dirty_to_final_path, is_dirty_path};
use crate::{HistoryError, Result};
use flate2::read::GzDecoder;
use flate2::write::GzEncoder;
use flate2::Compression;
use henyey_common::fs_utils::durable_rename;
use std::fs::{self, File, OpenOptions};
use std::io::{self, BufReader, BufWriter, Read, Write};
use std::path::{Path, PathBuf};
use stellar_xdr::curr::{
    LedgerHeaderHistoryEntry, Limits, ReadXdr, TransactionHistoryEntry,
    TransactionHistoryResultEntry, WriteXdr,
};
use tracing::{debug, info, warn};

/// File categories for checkpoint files.
const FILE_CATEGORIES: &[&str] = &["ledger", "transactions", "results"];

/// Read one XDR entry from a gzip-compressed stream using record marking format.
///
/// Reads a 4-byte length-prefixed entry (with bit 31 set as the "last fragment" marker),
/// followed by the XDR payload padded to 4-byte boundary. Returns `None` at EOF,
/// or `Err` on deserialization failure (which indicates a partial/corrupt write).
fn read_one_from_gz<T: ReadXdr>(reader: &mut impl Read) -> io::Result<Option<T>> {
    // Read 4-byte length header
    let mut header = [0u8; 4];
    match reader.read_exact(&mut header) {
        Ok(()) => {}
        Err(e) if e.kind() == io::ErrorKind::UnexpectedEof => return Ok(None),
        Err(e) => return Err(e),
    }

    // Extract size (strip the "last fragment" bit)
    let marked_len = u32::from_be_bytes(header);
    let len = marked_len & 0x7FFF_FFFF;

    // Read payload
    let mut payload = vec![0u8; len as usize];
    reader.read_exact(&mut payload)?;

    // Read padding (XDR entries are padded to 4-byte boundary)
    let padding = (4 - (len % 4)) % 4;
    if padding > 0 {
        let mut pad = vec![0u8; padding as usize];
        reader.read_exact(&mut pad)?;
    }

    // Deserialize XDR
    let value = T::from_xdr(&payload, Limits::none())
        .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))?;

    Ok(Some(value))
}

/// Write a single XDR entry using RFC 5531 record marking format.
///
/// Writes a 4-byte big-endian length prefix with bit 31 set ("last fragment"),
/// followed by the XDR-serialized payload, padded to a 4-byte boundary.
///
/// This is the write counterpart to `download::read_record_marked_xdr` and
/// matches stellar-core's `XDROutputFileStream::writeOne`.
pub fn write_record_marked_xdr<T: WriteXdr>(writer: &mut impl Write, entry: &T) -> io::Result<()> {
    let xdr_bytes = entry
        .to_xdr(Limits::none())
        .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))?;

    let len = xdr_bytes.len() as u32;
    let marked_len = len | 0x8000_0000;
    writer.write_all(&marked_len.to_be_bytes())?;
    writer.write_all(&xdr_bytes)?;

    // Pad to 4-byte boundary
    let padding = (4 - (len % 4)) % 4;
    if padding > 0 {
        writer.write_all(&vec![0u8; padding as usize])?;
    }

    Ok(())
}

/// Recover a gzip-compressed dirty checkpoint file by truncating entries beyond LCL.
///
/// This reads all valid entries from `dirty_path`, writes those with ledger_seq <= lcl
/// to a temp file, then durably renames the temp over the dirty path.
///
/// If `enforce_lcl` is true, the file must end exactly at LCL (used for ledger headers
/// where every ledger has an entry). For transactions/results, gaps are allowed since
/// empty ledgers don't produce entries.
///
/// Matches stellar-core's `CheckpointBuilder::cleanup()` recovery closure.
fn recover_dirty_file<T, F>(
    dirty_path: &Path,
    lcl: u32,
    enforce_lcl: bool,
    get_ledger_seq: F,
) -> Result<()>
where
    T: ReadXdr + WriteXdr,
    F: Fn(&T) -> u32,
{
    let tmp_dir = dirty_path
        .parent()
        .ok_or_else(|| HistoryError::CatchupFailed("dirty path has no parent".into()))?;
    let tmp_file = tmp_dir.join(format!(
        "truncated-{}",
        dirty_path.file_name().unwrap_or_default().to_string_lossy()
    ));

    // Read from dirty, write to temp
    let file_in = File::open(dirty_path)?;
    let mut reader = BufReader::new(GzDecoder::new(file_in));

    let file_out = OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(true)
        .open(&tmp_file)?;
    let buf_out = BufWriter::new(file_out);
    let mut writer = GzEncoder::new(buf_out, Compression::default());

    let mut last_read_ledger_seq: u32 = 0;

    loop {
        match read_one_from_gz::<T>(&mut reader) {
            Ok(Some(entry)) => {
                let seq = get_ledger_seq(&entry);
                if seq > lcl {
                    info!(
                        dirty = %dirty_path.display(),
                        lcl,
                        truncated_at = seq,
                        "Truncating dirty file at LCL"
                    );
                    break;
                }
                last_read_ledger_seq = seq;
                write_record_marked_xdr(&mut writer, &entry)?;
            }
            Ok(None) => {
                // EOF — check LCL enforcement
                if enforce_lcl && last_read_ledger_seq != 0 && last_read_ledger_seq != lcl {
                    return Err(HistoryError::CatchupFailed(format!(
                        "Corrupt checkpoint file {}, ends on ledger {}, LCL is {}",
                        dirty_path.display(),
                        last_read_ledger_seq,
                        lcl,
                    )));
                }
                break;
            }
            Err(e) => {
                // Deserialization error — partial write, truncate here
                info!(
                    dirty = %dirty_path.display(),
                    error = %e,
                    last_ledger = last_read_ledger_seq,
                    "Encountered partial write in dirty file, truncating"
                );
                break;
            }
        }
    }

    // Finalize the gzip stream and sync
    let buf_writer = writer.finish()?;
    let file = buf_writer
        .into_inner()
        .map_err(|e| HistoryError::Io(e.into_error()))?;
    file.sync_all()?;
    drop(file);

    // Durable rename temp → dirty
    durable_rename(&tmp_file, dirty_path)?;

    info!(
        dirty = %dirty_path.display(),
        last_ledger = last_read_ledger_seq,
        "Recovered dirty file"
    );

    Ok(())
}

/// XDR stream writer with gzip compression.
///
/// Writes XDR entries to a gzip-compressed file with RFC 5531 record marking
/// (4-byte length prefix followed by padded XDR data).
struct XdrStreamWriter {
    /// The underlying gzip encoder wrapped in a buffered writer.
    encoder: GzEncoder<BufWriter<File>>,
    /// Path to the dirty file being written.
    dirty_path: PathBuf,
    /// Path to the final file (after rename).
    final_path: PathBuf,
    /// Number of entries written.
    entry_count: u32,
    /// Last ledger sequence written.
    last_ledger: u32,
}

impl XdrStreamWriter {
    /// Create a new XDR stream writer.
    fn new(dirty_path: PathBuf, final_path: PathBuf) -> Result<Self> {
        // Create parent directories if needed
        if let Some(parent) = dirty_path.parent() {
            fs::create_dir_all(parent)?;
        }

        let file = OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(true)
            .open(&dirty_path)?;

        let buf_writer = BufWriter::new(file);
        let encoder = GzEncoder::new(buf_writer, Compression::default());

        Ok(Self {
            encoder,
            dirty_path,
            final_path,
            entry_count: 0,
            last_ledger: 0,
        })
    }

    /// Write an XDR entry with record marking.
    fn write_xdr<T: WriteXdr>(&mut self, entry: &T, ledger_seq: u32) -> Result<()> {
        write_record_marked_xdr(&mut self.encoder, entry)?;
        self.entry_count += 1;
        self.last_ledger = ledger_seq;
        Ok(())
    }

    /// Finish writing and return info about the file.
    fn finish(self) -> Result<(PathBuf, PathBuf, u32)> {
        // Finish the gzip stream
        let buf_writer = self.encoder.finish()?;

        // Flush and sync the file
        // Map IntoInnerError to HistoryError::Io by extracting the underlying IO error
        let file = buf_writer
            .into_inner()
            .map_err(|e| HistoryError::Io(e.into_error()))?;
        file.sync_all()?;

        Ok((self.dirty_path, self.final_path, self.last_ledger))
    }
}

/// Builder for crash-safe checkpoint construction.
///
/// The checkpoint builder accumulates ledger data (headers, transactions, results)
/// and writes them to temporary `.dirty` files. On checkpoint completion, the
/// dirty files are atomically renamed to their final paths.
pub struct CheckpointBuilder {
    /// Directory where checkpoint files are written.
    publish_dir: PathBuf,
    /// Current checkpoint being built (if any).
    current_checkpoint: Option<u32>,
    /// Writer for ledger headers.
    headers_writer: Option<XdrStreamWriter>,
    /// Writer for transactions.
    transactions_writer: Option<XdrStreamWriter>,
    /// Writer for transaction results.
    results_writer: Option<XdrStreamWriter>,
    /// Whether startup validation has been performed.
    startup_validated: bool,
}

impl CheckpointBuilder {
    /// Create a new checkpoint builder.
    ///
    /// # Arguments
    ///
    /// * `publish_dir` - Directory where checkpoint files will be written
    pub fn new(publish_dir: PathBuf) -> Self {
        Self {
            publish_dir,
            current_checkpoint: None,
            headers_writer: None,
            transactions_writer: None,
            results_writer: None,
            startup_validated: false,
        }
    }

    /// Get the current checkpoint being built, if any.
    pub fn current_checkpoint(&self) -> Option<u32> {
        self.current_checkpoint
    }

    /// Check if a checkpoint is currently being built.
    pub fn is_open(&self) -> bool {
        self.current_checkpoint.is_some()
    }

    /// Ensure writers are open for the given checkpoint.
    fn ensure_open(&mut self, checkpoint: u32) -> Result<()> {
        if let Some(current) = self.current_checkpoint {
            if current != checkpoint {
                return Err(HistoryError::CatchupFailed(format!(
                    "checkpoint mismatch: building {} but asked for {}",
                    current, checkpoint
                )));
            }
            return Ok(());
        }

        // Open new writers
        let headers_dirty = self
            .publish_dir
            .join(checkpoint_path_dirty("ledger", checkpoint, "xdr.gz"));
        let headers_final = self
            .publish_dir
            .join(checkpoint_path("ledger", checkpoint, "xdr.gz"));
        self.headers_writer = Some(XdrStreamWriter::new(headers_dirty, headers_final)?);

        let tx_dirty =
            self.publish_dir
                .join(checkpoint_path_dirty("transactions", checkpoint, "xdr.gz"));
        let tx_final = self
            .publish_dir
            .join(checkpoint_path("transactions", checkpoint, "xdr.gz"));
        self.transactions_writer = Some(XdrStreamWriter::new(tx_dirty, tx_final)?);

        let results_dirty = self
            .publish_dir
            .join(checkpoint_path_dirty("results", checkpoint, "xdr.gz"));
        let results_final = self
            .publish_dir
            .join(checkpoint_path("results", checkpoint, "xdr.gz"));
        self.results_writer = Some(XdrStreamWriter::new(results_dirty, results_final)?);

        self.current_checkpoint = Some(checkpoint);
        debug!(checkpoint, "Opened checkpoint builders");
        Ok(())
    }

    /// Append a ledger header to the checkpoint.
    ///
    /// # Arguments
    ///
    /// * `header` - The ledger header to append
    /// * `checkpoint` - The checkpoint this header belongs to
    pub fn append_ledger_header(
        &mut self,
        header: &LedgerHeaderHistoryEntry,
        checkpoint: u32,
    ) -> Result<()> {
        self.ensure_open(checkpoint)?;

        let ledger_seq = header.header.ledger_seq;
        self.headers_writer
            .as_mut()
            .unwrap()
            .write_xdr(header, ledger_seq)?;

        debug!(ledger_seq, checkpoint, "Appended ledger header");
        Ok(())
    }

    /// Append a transaction set and its results to the checkpoint.
    ///
    /// # Arguments
    ///
    /// * `tx_entry` - The transaction history entry
    /// * `result_entry` - The transaction result history entry
    /// * `checkpoint` - The checkpoint these belong to
    pub fn append_transaction_set(
        &mut self,
        tx_entry: &TransactionHistoryEntry,
        result_entry: &TransactionHistoryResultEntry,
        checkpoint: u32,
    ) -> Result<()> {
        self.ensure_open(checkpoint)?;

        let ledger_seq = tx_entry.ledger_seq;

        self.transactions_writer
            .as_mut()
            .unwrap()
            .write_xdr(tx_entry, ledger_seq)?;

        self.results_writer
            .as_mut()
            .unwrap()
            .write_xdr(result_entry, ledger_seq)?;

        debug!(ledger_seq, checkpoint, "Appended transaction set");
        Ok(())
    }

    /// Complete a checkpoint by atomically renaming dirty files to final paths.
    ///
    /// # Arguments
    ///
    /// * `checkpoint` - The checkpoint to complete
    pub fn checkpoint_complete(&mut self, checkpoint: u32) -> Result<()> {
        if self.current_checkpoint != Some(checkpoint) {
            return Err(HistoryError::CatchupFailed(format!(
                "checkpoint_complete called for {} but building {:?}",
                checkpoint, self.current_checkpoint
            )));
        }

        // Finish all active writers and collect file info
        let writers = [
            self.headers_writer.take(),
            self.transactions_writer.take(),
            self.results_writer.take(),
        ];
        let mut files_to_rename = Vec::new();
        for writer in writers.into_iter().flatten() {
            let (dirty, final_path, _last) = writer.finish()?;
            files_to_rename.push((dirty, final_path));
        }

        // Atomically rename all dirty files to final paths
        for (dirty, final_path) in &files_to_rename {
            // Create parent directories for final path if needed
            if let Some(parent) = final_path.parent() {
                fs::create_dir_all(parent)?;
            }
            durable_rename(dirty, final_path)?;
            debug!(
                dirty = %dirty.display(),
                final_path = %final_path.display(),
                "Renamed dirty file to final"
            );
        }

        self.current_checkpoint = None;
        info!(checkpoint, "Checkpoint complete");
        Ok(())
    }

    /// Clean up and recover state on startup.
    ///
    /// This should be called on startup with the last committed ledger (LCL).
    /// It handles recovery from crashes during checkpoint building.
    ///
    /// # Arguments
    ///
    /// * `lcl` - The last committed ledger sequence
    pub fn cleanup(&mut self, lcl: u32) -> Result<()> {
        info!(lcl, publish_dir = %self.publish_dir.display(), "Cleaning up checkpoint builder");

        // Scan for dirty files in each category
        for category in FILE_CATEGORIES {
            self.cleanup_category(category, lcl)?;
        }

        self.startup_validated = true;
        Ok(())
    }

    /// Clean up dirty files for a specific category.
    fn cleanup_category(&self, category: &str, lcl: u32) -> Result<()> {
        let category_dir = self.publish_dir.join(category);
        if !category_dir.exists() {
            return Ok(());
        }

        // Walk through the category directory looking for dirty files
        self.scan_for_dirty_files(&category_dir, lcl)?;
        Ok(())
    }

    /// Recursively scan for dirty files and handle them.
    fn scan_for_dirty_files(&self, dir: &Path, lcl: u32) -> Result<()> {
        if !dir.is_dir() {
            return Ok(());
        }

        for entry in fs::read_dir(dir)? {
            let entry = entry?;
            let path = entry.path();

            if path.is_dir() {
                self.scan_for_dirty_files(&path, lcl)?;
            } else if is_dirty_path(&path) {
                self.handle_dirty_file(&path, lcl)?;
            }
        }

        Ok(())
    }

    /// Handle a single dirty file found during cleanup.
    fn handle_dirty_file(&self, dirty_path: &Path, lcl: u32) -> Result<()> {
        let final_path = match dirty_to_final_path(dirty_path) {
            Some(p) => p,
            None => return Ok(()), // Not a valid dirty path
        };

        let dirty_exists = dirty_path.exists();
        let final_exists = final_path.exists();

        match (dirty_exists, final_exists) {
            (true, true) => {
                // Both exist - delete the dirty file (leftover from completed checkpoint)
                warn!(
                    dirty = %dirty_path.display(),
                    final_path = %final_path.display(),
                    "Deleting leftover dirty file (final exists)"
                );
                fs::remove_file(dirty_path)?;
            }
            (true, false) => {
                // Only dirty exists — truncate entries beyond LCL to recover
                // partial checkpoint. Determine file category from path to
                // select the right XDR type and LCL enforcement.
                let category = self.category_from_dirty_path(dirty_path);
                match category.as_deref() {
                    Some("ledger") => {
                        info!(
                            dirty = %dirty_path.display(),
                            lcl,
                            "Recovering dirty ledger header file (enforceLCL=true)"
                        );
                        recover_dirty_file::<LedgerHeaderHistoryEntry, _>(
                            dirty_path,
                            lcl,
                            true, // enforceLCL
                            |entry| entry.header.ledger_seq,
                        )?;
                    }
                    Some("transactions") => {
                        info!(
                            dirty = %dirty_path.display(),
                            lcl,
                            "Recovering dirty transactions file"
                        );
                        recover_dirty_file::<TransactionHistoryEntry, _>(
                            dirty_path,
                            lcl,
                            false,
                            |entry| entry.ledger_seq,
                        )?;
                    }
                    Some("results") => {
                        info!(
                            dirty = %dirty_path.display(),
                            lcl,
                            "Recovering dirty results file"
                        );
                        recover_dirty_file::<TransactionHistoryResultEntry, _>(
                            dirty_path,
                            lcl,
                            false,
                            |entry| entry.ledger_seq,
                        )?;
                    }
                    _ => {
                        // Unknown category — can't recover, delete
                        warn!(
                            dirty = %dirty_path.display(),
                            lcl,
                            "Unknown file category, deleting dirty file"
                        );
                        fs::remove_file(dirty_path)?;
                    }
                }
            }
            (false, true) => {
                // Only final exists - this is normal, nothing to do
                debug!(
                    final_path = %final_path.display(),
                    "Final file exists, no cleanup needed"
                );
            }
            (false, false) => {
                // Neither exists - nothing to do
            }
        }

        Ok(())
    }

    /// Extract the file category from a dirty path.
    ///
    /// The category is the top-level directory under publish_dir, e.g. "ledger",
    /// "transactions", or "results".
    fn category_from_dirty_path(&self, dirty_path: &Path) -> Option<String> {
        let relative = dirty_path.strip_prefix(&self.publish_dir).ok()?;
        let first_component = relative.components().next()?;
        Some(first_component.as_os_str().to_string_lossy().into_owned())
    }

    /// Check if startup validation has been performed.
    pub fn is_validated(&self) -> bool {
        self.startup_validated
    }

    /// Finalize a recovered checkpoint by renaming dirty files to final paths.
    ///
    /// This mirrors the rename portion of stellar-core's `checkpointComplete()`:
    /// for each file category, if a dirty file exists but the final file does not,
    /// durably rename dirty → final.
    ///
    /// Called from `restore_checkpoint` after `cleanup()` when LCL sits on a
    /// checkpoint boundary.
    pub fn finalize_recovered_checkpoint(&self, checkpoint: u32) -> Result<()> {
        for category in FILE_CATEGORIES {
            let dirty = self
                .publish_dir
                .join(checkpoint_path_dirty(category, checkpoint, "xdr.gz"));
            let final_path = self
                .publish_dir
                .join(checkpoint_path(category, checkpoint, "xdr.gz"));

            if !dirty.exists() {
                continue;
            }
            if final_path.exists() {
                debug!(
                    category,
                    final_path = %final_path.display(),
                    "Final file already exists, skipping rename"
                );
                continue;
            }

            // Create parent directories for final path if needed
            if let Some(parent) = final_path.parent() {
                fs::create_dir_all(parent)?;
            }
            durable_rename(&dirty, &final_path)?;
            info!(
                category,
                dirty = %dirty.display(),
                final_path = %final_path.display(),
                "Renamed recovered dirty file to final"
            );
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use stellar_xdr::curr::{
        Hash, LedgerHeader, LedgerHeaderExt, LedgerHeaderHistoryEntryExt, StellarValue,
        StellarValueExt, TimePoint, TransactionHistoryEntry, TransactionHistoryEntryExt,
        TransactionHistoryResultEntry, TransactionHistoryResultEntryExt, TransactionResultSet,
        TransactionSet, VecM,
    };

    fn make_header(seq: u32) -> LedgerHeaderHistoryEntry {
        LedgerHeaderHistoryEntry {
            hash: Hash([seq as u8; 32]),
            header: LedgerHeader {
                ledger_version: 20,
                previous_ledger_hash: Hash([0; 32]),
                scp_value: StellarValue {
                    tx_set_hash: Hash([0; 32]),
                    close_time: TimePoint(0),
                    upgrades: VecM::default(),
                    ext: StellarValueExt::Basic,
                },
                tx_set_result_hash: Hash([0; 32]),
                bucket_list_hash: Hash([0; 32]),
                ledger_seq: seq,
                total_coins: 0,
                fee_pool: 0,
                inflation_seq: 0,
                id_pool: 0,
                base_fee: 100,
                base_reserve: 5000000,
                max_tx_set_size: 100,
                skip_list: [Hash([0; 32]), Hash([0; 32]), Hash([0; 32]), Hash([0; 32])],
                ext: LedgerHeaderExt::V0,
            },
            ext: LedgerHeaderHistoryEntryExt::default(),
        }
    }

    fn make_tx_entry(seq: u32) -> TransactionHistoryEntry {
        TransactionHistoryEntry {
            ledger_seq: seq,
            tx_set: TransactionSet {
                previous_ledger_hash: Hash([0; 32]),
                txs: VecM::default(),
            },
            ext: TransactionHistoryEntryExt::default(),
        }
    }

    fn make_result_entry(seq: u32) -> TransactionHistoryResultEntry {
        TransactionHistoryResultEntry {
            ledger_seq: seq,
            tx_result_set: TransactionResultSet {
                results: VecM::default(),
            },
            ext: TransactionHistoryResultEntryExt::default(),
        }
    }

    #[test]
    fn test_checkpoint_builder_new() {
        let builder = CheckpointBuilder::new(PathBuf::from("/tmp/test"));
        assert!(!builder.is_open());
        assert!(builder.current_checkpoint().is_none());
        assert!(!builder.is_validated());
    }

    #[test]
    fn test_checkpoint_builder_basic_flow() {
        let temp_dir = tempfile::tempdir().unwrap();
        let mut builder = CheckpointBuilder::new(temp_dir.path().to_path_buf());

        let checkpoint = 63;

        // Append some data
        let header = make_header(1);
        builder.append_ledger_header(&header, checkpoint).unwrap();

        let tx = make_tx_entry(1);
        let results = make_result_entry(1);
        builder
            .append_transaction_set(&tx, &results, checkpoint)
            .unwrap();

        assert!(builder.is_open());
        assert_eq!(builder.current_checkpoint(), Some(checkpoint));

        // Dirty files should exist
        let dirty_headers = temp_dir
            .path()
            .join("ledger/00/00/00/ledger-0000003f.xdr.gz.dirty");
        assert!(dirty_headers.exists());

        // Complete the checkpoint
        builder.checkpoint_complete(checkpoint).unwrap();

        assert!(!builder.is_open());

        // Final files should exist, dirty files should not
        let final_headers = temp_dir
            .path()
            .join("ledger/00/00/00/ledger-0000003f.xdr.gz");
        assert!(final_headers.exists());
        assert!(!dirty_headers.exists());
    }

    #[test]
    fn test_cleanup_recovers_corrupt_dirty_files() {
        let temp_dir = tempfile::tempdir().unwrap();
        let mut builder = CheckpointBuilder::new(temp_dir.path().to_path_buf());

        // Create a dirty file with garbage data (simulating a corrupt crash)
        let dirty_path = temp_dir
            .path()
            .join("ledger/00/00/00/ledger-0000003f.xdr.gz.dirty");
        fs::create_dir_all(dirty_path.parent().unwrap()).unwrap();
        fs::write(&dirty_path, b"test data").unwrap();

        assert!(dirty_path.exists());

        // Cleanup should recover it (corrupt gzip → empty recovered file)
        builder.cleanup(63).unwrap();

        // The dirty file still exists (recovered, not deleted), but is now
        // a valid empty gzip file
        assert!(dirty_path.exists());
        assert!(builder.is_validated());
    }

    #[test]
    fn test_cleanup_keeps_final_files() {
        let temp_dir = tempfile::tempdir().unwrap();
        let mut builder = CheckpointBuilder::new(temp_dir.path().to_path_buf());

        // Create a final file
        let final_path = temp_dir
            .path()
            .join("ledger/00/00/00/ledger-0000003f.xdr.gz");
        fs::create_dir_all(final_path.parent().unwrap()).unwrap();
        fs::write(&final_path, b"test data").unwrap();

        assert!(final_path.exists());

        // Cleanup should keep it
        builder.cleanup(63).unwrap();

        assert!(final_path.exists());
    }

    #[test]
    fn test_cleanup_removes_dirty_when_final_exists() {
        let temp_dir = tempfile::tempdir().unwrap();
        let mut builder = CheckpointBuilder::new(temp_dir.path().to_path_buf());

        // Create both dirty and final files
        let dirty_path = temp_dir
            .path()
            .join("ledger/00/00/00/ledger-0000003f.xdr.gz.dirty");
        let final_path = temp_dir
            .path()
            .join("ledger/00/00/00/ledger-0000003f.xdr.gz");
        fs::create_dir_all(dirty_path.parent().unwrap()).unwrap();
        fs::write(&dirty_path, b"dirty data").unwrap();
        fs::write(&final_path, b"final data").unwrap();

        // Cleanup should remove dirty but keep final
        builder.cleanup(63).unwrap();

        assert!(!dirty_path.exists());
        assert!(final_path.exists());
    }

    /// Helper: write entries to a gzip-compressed dirty file in the same format
    /// as `XdrStreamWriter`.
    fn write_gz_dirty<T: WriteXdr>(path: &Path, entries: &[T]) {
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        let file = File::create(path).unwrap();
        let buf = BufWriter::new(file);
        let mut encoder = GzEncoder::new(buf, Compression::default());
        for entry in entries {
            write_record_marked_xdr(&mut encoder, entry).unwrap();
        }
        let buf = encoder.finish().unwrap();
        let file = buf.into_inner().unwrap();
        file.sync_all().unwrap();
    }

    /// Helper: read all entries from a gzip-compressed file.
    fn read_gz_all<T: ReadXdr>(path: &Path) -> Vec<T> {
        let file = File::open(path).unwrap();
        let mut reader = BufReader::new(GzDecoder::new(file));
        let mut entries = Vec::new();
        while let Ok(Some(entry)) = read_one_from_gz::<T>(&mut reader) {
            entries.push(entry);
        }
        entries
    }

    #[test]
    fn test_recovery_truncates_entries_beyond_lcl() {
        let temp_dir = tempfile::tempdir().unwrap();
        let mut builder = CheckpointBuilder::new(temp_dir.path().to_path_buf());

        // Write ledger headers 1..=65 to a dirty file (checkpoint 63, plus 2 beyond)
        let dirty_path = temp_dir
            .path()
            .join("ledger/00/00/00/ledger-0000003f.xdr.gz.dirty");
        let headers: Vec<_> = (1..=65).map(make_header).collect();
        write_gz_dirty(&dirty_path, &headers);

        // LCL is 63 — entries 64 and 65 should be truncated
        builder.cleanup(63).unwrap();

        // Read back the recovered file
        let recovered = read_gz_all::<LedgerHeaderHistoryEntry>(&dirty_path);
        assert_eq!(recovered.len(), 63);
        assert_eq!(recovered.last().unwrap().header.ledger_seq, 63);
    }

    #[test]
    fn test_recovery_keeps_all_entries_when_at_lcl() {
        let temp_dir = tempfile::tempdir().unwrap();
        let mut builder = CheckpointBuilder::new(temp_dir.path().to_path_buf());

        // Write ledger headers 1..=63 (exactly at LCL)
        let dirty_path = temp_dir
            .path()
            .join("ledger/00/00/00/ledger-0000003f.xdr.gz.dirty");
        let headers: Vec<_> = (1..=63).map(make_header).collect();
        write_gz_dirty(&dirty_path, &headers);

        builder.cleanup(63).unwrap();

        let recovered = read_gz_all::<LedgerHeaderHistoryEntry>(&dirty_path);
        assert_eq!(recovered.len(), 63);
    }

    #[test]
    fn test_recovery_transactions_no_enforce_lcl() {
        let temp_dir = tempfile::tempdir().unwrap();
        let mut builder = CheckpointBuilder::new(temp_dir.path().to_path_buf());

        // Write transaction entries for ledgers 5, 10, 15 (sparse — some ledgers have no txs)
        let dirty_path = temp_dir
            .path()
            .join("transactions/00/00/00/transactions-0000003f.xdr.gz.dirty");
        let entries: Vec<_> = [5, 10, 15].iter().map(|&seq| make_tx_entry(seq)).collect();
        write_gz_dirty(&dirty_path, &entries);

        // LCL is 12 — entry with seq 15 should be truncated, but no error
        // even though last_read_ledger_seq (10) != lcl (12) because enforceLCL=false
        builder.cleanup(12).unwrap();

        let recovered = read_gz_all::<TransactionHistoryEntry>(&dirty_path);
        assert_eq!(recovered.len(), 2);
        assert_eq!(recovered[0].ledger_seq, 5);
        assert_eq!(recovered[1].ledger_seq, 10);
    }

    #[test]
    fn test_recovery_results_truncation() {
        let temp_dir = tempfile::tempdir().unwrap();
        let mut builder = CheckpointBuilder::new(temp_dir.path().to_path_buf());

        let dirty_path = temp_dir
            .path()
            .join("results/00/00/00/results-0000003f.xdr.gz.dirty");
        let entries: Vec<_> = (1..=10).map(make_result_entry).collect();
        write_gz_dirty(&dirty_path, &entries);

        builder.cleanup(7).unwrap();

        let recovered = read_gz_all::<TransactionHistoryResultEntry>(&dirty_path);
        assert_eq!(recovered.len(), 7);
        assert_eq!(recovered.last().unwrap().ledger_seq, 7);
    }

    #[test]
    fn test_recovery_enforce_lcl_fails_on_short_file() {
        let temp_dir = tempfile::tempdir().unwrap();
        let mut builder = CheckpointBuilder::new(temp_dir.path().to_path_buf());

        // Write only ledger headers 1..=50, but LCL is 63
        // enforceLCL=true should fail because the file ends at 50, not 63
        let dirty_path = temp_dir
            .path()
            .join("ledger/00/00/00/ledger-0000003f.xdr.gz.dirty");
        let headers: Vec<_> = (1..=50).map(make_header).collect();
        write_gz_dirty(&dirty_path, &headers);

        let result = builder.cleanup(63);
        assert!(result.is_err());
        let err_msg = format!("{}", result.unwrap_err());
        assert!(err_msg.contains("Corrupt checkpoint file"));
        assert!(err_msg.contains("ends on ledger 50"));
    }

    #[test]
    fn test_read_write_gz_roundtrip() {
        let temp_dir = tempfile::tempdir().unwrap();
        let path = temp_dir.path().join("test.xdr.gz");

        let headers: Vec<_> = (1..=5).map(make_header).collect();
        write_gz_dirty(&path, &headers);

        let recovered = read_gz_all::<LedgerHeaderHistoryEntry>(&path);
        assert_eq!(recovered.len(), 5);
        for (i, hdr) in recovered.iter().enumerate() {
            assert_eq!(hdr.header.ledger_seq, (i + 1) as u32);
        }
    }

    #[test]
    fn test_finalize_recovered_checkpoint_renames_dirty_to_final() {
        let temp_dir = tempfile::tempdir().unwrap();
        let publish_dir = temp_dir.path().to_path_buf();
        let checkpoint = 63;

        // Create dirty files for all 3 categories at checkpoint 63
        let dirty_ledger = publish_dir.join("ledger/00/00/00/ledger-0000003f.xdr.gz.dirty");
        let dirty_tx = publish_dir.join("transactions/00/00/00/transactions-0000003f.xdr.gz.dirty");
        let dirty_results = publish_dir.join("results/00/00/00/results-0000003f.xdr.gz.dirty");

        let headers: Vec<_> = (1..=63).map(make_header).collect();
        write_gz_dirty(&dirty_ledger, &headers);

        let txs: Vec<_> = (1..=63).map(make_tx_entry).collect();
        write_gz_dirty(&dirty_tx, &txs);

        let results: Vec<_> = (1..=63).map(make_result_entry).collect();
        write_gz_dirty(&dirty_results, &results);

        assert!(dirty_ledger.exists());
        assert!(dirty_tx.exists());
        assert!(dirty_results.exists());

        // Finalize should rename dirty → final
        let builder = CheckpointBuilder::new(publish_dir.clone());
        builder.finalize_recovered_checkpoint(checkpoint).unwrap();

        // Final files should exist, dirty files should be gone
        let final_ledger = publish_dir.join("ledger/00/00/00/ledger-0000003f.xdr.gz");
        let final_tx = publish_dir.join("transactions/00/00/00/transactions-0000003f.xdr.gz");
        let final_results = publish_dir.join("results/00/00/00/results-0000003f.xdr.gz");

        assert!(final_ledger.exists());
        assert!(final_tx.exists());
        assert!(final_results.exists());
        assert!(!dirty_ledger.exists());
        assert!(!dirty_tx.exists());
        assert!(!dirty_results.exists());
    }

    #[test]
    fn test_finalize_skips_when_final_exists() {
        let temp_dir = tempfile::tempdir().unwrap();
        let publish_dir = temp_dir.path().to_path_buf();
        let checkpoint = 63;

        // Create both dirty and final for ledger
        let dirty_ledger = publish_dir.join("ledger/00/00/00/ledger-0000003f.xdr.gz.dirty");
        let final_ledger = publish_dir.join("ledger/00/00/00/ledger-0000003f.xdr.gz");

        let dirty_headers: Vec<_> = (1..=63).map(make_header).collect();
        write_gz_dirty(&dirty_ledger, &dirty_headers);
        write_gz_dirty(&final_ledger, &dirty_headers);

        assert!(dirty_ledger.exists());
        assert!(final_ledger.exists());

        // Finalize should NOT rename (final already exists)
        let builder = CheckpointBuilder::new(publish_dir);
        builder.finalize_recovered_checkpoint(checkpoint).unwrap();

        // Both should still exist (dirty was not renamed/deleted by finalize)
        assert!(dirty_ledger.exists());
        assert!(final_ledger.exists());
    }

    #[test]
    fn test_finalize_noop_when_no_dirty_files() {
        let temp_dir = tempfile::tempdir().unwrap();
        let publish_dir = temp_dir.path().to_path_buf();

        // No files at all
        let builder = CheckpointBuilder::new(publish_dir);
        builder.finalize_recovered_checkpoint(63).unwrap();
        // Should succeed silently
    }
}
