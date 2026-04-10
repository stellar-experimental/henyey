//! Asynchronous meta stream writer.
//!
//! Wraps [`MetaStreamManager`] behind a bounded async channel and a dedicated
//! writer thread. This removes blocking I/O from the ledger-close and catchup
//! hot paths, isolating the main Tokio runtime from filesystem stalls.
//!
//! The writer thread consumes [`MetaWriteCommand`] messages sequentially,
//! performing rotation and XDR writes. Fatal main-stream write errors set an
//! atomic flag and abort the process (matching the existing abort-on-error
//! behavior in ledger_close.rs and catchup_impl.rs).

use crate::meta_stream::{MetaStreamError, MetaStreamManager};
use henyey_common::LedgerSeq;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use stellar_xdr::curr::LedgerCloseMeta;
use tokio::sync::mpsc;

/// Capacity of the bounded write channel.
///
/// With one write per ledger close (~5s on mainnet), 16 slots provides
/// ~80s of buffering — enough to absorb filesystem stalls without
/// unbounded growth.
const CHANNEL_CAPACITY: usize = 16;

/// Commands sent to the writer thread.
pub enum MetaWriteCommand {
    /// Write a LedgerCloseMeta frame.
    Write {
        meta: Box<LedgerCloseMeta>,
        ledger_seq: LedgerSeq,
    },
    /// Shut down the writer thread.
    Shutdown,
}

/// Async-safe handle to the meta stream writer.
///
/// Provides non-blocking `write_meta` for the live ledger-close path and
/// blocking `write_meta_blocking` for the catchup callback. Both send
/// through the same bounded channel to a dedicated writer thread.
pub struct MetaWriter {
    tx: mpsc::Sender<MetaWriteCommand>,
    /// Set by the writer thread on fatal main-stream error.
    fatal: Arc<AtomicBool>,
    /// Writer thread join handle — joined on drop.
    _thread_handle: Option<std::thread::JoinHandle<()>>,
}

impl MetaWriter {
    /// Create a new MetaWriter, spawning the dedicated writer thread.
    pub fn new(stream: MetaStreamManager) -> Self {
        let (tx, rx) = mpsc::channel(CHANNEL_CAPACITY);
        let fatal = Arc::new(AtomicBool::new(false));
        let fatal_clone = Arc::clone(&fatal);

        let handle = std::thread::Builder::new()
            .name("meta-writer".into())
            .spawn(move || {
                Self::writer_loop(rx, stream, fatal_clone);
            })
            .expect("failed to spawn meta-writer thread");

        Self {
            tx,
            fatal,
            _thread_handle: Some(handle),
        }
    }

    /// Async write — used by the live ledger-close path.
    ///
    /// Returns `Err` if the channel is closed (writer thread crashed/exited).
    pub async fn write_meta(
        &self,
        meta: LedgerCloseMeta,
        ledger_seq: LedgerSeq,
    ) -> Result<(), MetaWriterError> {
        if self.fatal.load(Ordering::Relaxed) {
            return Err(MetaWriterError::Fatal);
        }
        self.tx
            .send(MetaWriteCommand::Write {
                meta: Box::new(meta),
                ledger_seq,
            })
            .await
            .map_err(|_| MetaWriterError::ChannelClosed)
    }

    /// Blocking write — used by the catchup meta callback.
    ///
    /// This is safe to call from `std::thread` or `tokio::task::spawn_blocking`
    /// contexts. Uses `tokio::sync::mpsc::Sender::blocking_send`.
    pub fn write_meta_blocking(
        &self,
        meta: LedgerCloseMeta,
        ledger_seq: LedgerSeq,
    ) -> Result<(), MetaWriterError> {
        if self.fatal.load(Ordering::Relaxed) {
            return Err(MetaWriterError::Fatal);
        }
        self.tx
            .blocking_send(MetaWriteCommand::Write {
                meta: Box::new(meta),
                ledger_seq,
            })
            .map_err(|_| MetaWriterError::ChannelClosed)
    }

    /// Request the writer thread to shut down.
    pub async fn shutdown(&self) {
        let _ = self.tx.send(MetaWriteCommand::Shutdown).await;
    }

    /// Check if a fatal error has occurred.
    pub fn has_fatal_error(&self) -> bool {
        self.fatal.load(Ordering::Relaxed)
    }

    /// Clone the underlying mpsc::Sender for use in callbacks.
    ///
    /// Used by the catchup meta callback which needs a `Send + 'static`
    /// sender. Note: the catchup callback uses `try_send` (not
    /// `blocking_send`) because it runs inside the tokio runtime.
    pub fn clone_sender(&self) -> mpsc::Sender<MetaWriteCommand> {
        self.tx.clone()
    }

    /// The writer thread's main loop.
    fn writer_loop(
        mut rx: mpsc::Receiver<MetaWriteCommand>,
        mut stream: MetaStreamManager,
        fatal: Arc<AtomicBool>,
    ) {
        // We need a minimal Tokio runtime to drive the mpsc::Receiver.
        // Using current_thread since this thread only does blocking I/O.
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_time()
            .build()
            .expect("failed to build meta-writer runtime");

        rt.block_on(async {
            while let Some(cmd) = rx.recv().await {
                match cmd {
                    MetaWriteCommand::Write { meta, ledger_seq } => {
                        // Rotate debug stream if needed.
                        if let Err(e) = stream.maybe_rotate_debug_stream(ledger_seq) {
                            tracing::warn!(
                                error = %e,
                                ledger_seq = ledger_seq.get(),
                                "Failed to rotate debug meta stream"
                            );
                        }

                        match stream.emit_meta(&meta) {
                            Ok(()) => {}
                            Err(MetaStreamError::MainStreamWrite(e)) => {
                                tracing::error!(
                                    error = %e,
                                    ledger_seq = ledger_seq.get(),
                                    "Fatal: metadata output stream write failed"
                                );
                                fatal.store(true, Ordering::Relaxed);
                                std::process::abort();
                            }
                            Err(MetaStreamError::DebugStreamWrite(e)) => {
                                tracing::warn!(
                                    error = %e,
                                    ledger_seq = ledger_seq.get(),
                                    "Debug metadata stream write failed"
                                );
                            }
                        }
                    }
                    MetaWriteCommand::Shutdown => {
                        tracing::info!("Meta writer shutting down");
                        break;
                    }
                }
            }
        });
    }
}

/// Errors from the MetaWriter.
#[derive(Debug)]
pub enum MetaWriterError {
    /// The writer thread has encountered a fatal error.
    Fatal,
    /// The write channel is closed (writer thread has exited).
    ChannelClosed,
}

impl std::fmt::Display for MetaWriterError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            MetaWriterError::Fatal => write!(f, "meta writer encountered a fatal error"),
            MetaWriterError::ChannelClosed => write!(f, "meta writer channel closed"),
        }
    }
}

impl std::error::Error for MetaWriterError {}

/// Extract the ledger sequence number from a LedgerCloseMeta.
pub fn extract_ledger_seq(meta: &LedgerCloseMeta) -> LedgerSeq {
    match meta {
        LedgerCloseMeta::V0(v0) => v0.ledger_header.header.ledger_seq.into(),
        LedgerCloseMeta::V1(v1) => v1.ledger_header.header.ledger_seq.into(),
        LedgerCloseMeta::V2(v2) => v2.ledger_header.header.ledger_seq.into(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::MetadataConfig;
    use tempfile::TempDir;

    fn make_test_stream(dir: &TempDir) -> MetaStreamManager {
        let config = MetadataConfig {
            output_stream: None,
            debug_ledgers: 0,
            ..Default::default()
        };
        MetaStreamManager::new(&config, dir.path()).unwrap()
    }

    #[tokio::test]
    async fn test_meta_writer_shutdown() {
        let dir = TempDir::new().unwrap();
        let stream = make_test_stream(&dir);
        let writer = MetaWriter::new(stream);

        assert!(!writer.has_fatal_error());
        writer.shutdown().await;
    }

    #[tokio::test]
    async fn test_meta_writer_channel_closed_after_shutdown() {
        let dir = TempDir::new().unwrap();
        let stream = make_test_stream(&dir);
        let writer = MetaWriter::new(stream);

        writer.shutdown().await;
        // Give writer thread time to exit
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;

        // Channel should be closed now
        let meta = LedgerCloseMeta::V0(stellar_xdr::curr::LedgerCloseMetaV0 {
            ledger_header: stellar_xdr::curr::LedgerHeaderHistoryEntry {
                hash: stellar_xdr::curr::Hash([0; 32]),
                header: stellar_xdr::curr::LedgerHeader {
                    ledger_version: 21,
                    previous_ledger_hash: stellar_xdr::curr::Hash([0; 32]),
                    scp_value: stellar_xdr::curr::StellarValue {
                        tx_set_hash: stellar_xdr::curr::Hash([0; 32]),
                        close_time: stellar_xdr::curr::TimePoint(0),
                        upgrades: vec![].try_into().unwrap(),
                        ext: stellar_xdr::curr::StellarValueExt::Basic,
                    },
                    tx_set_result_hash: stellar_xdr::curr::Hash([0; 32]),
                    bucket_list_hash: stellar_xdr::curr::Hash([0; 32]),
                    ledger_seq: 1,
                    total_coins: 0,
                    fee_pool: 0,
                    inflation_seq: 0,
                    id_pool: 0,
                    base_fee: 100,
                    base_reserve: 5000000,
                    max_tx_set_size: 100,
                    skip_list: [
                        stellar_xdr::curr::Hash([0; 32]),
                        stellar_xdr::curr::Hash([0; 32]),
                        stellar_xdr::curr::Hash([0; 32]),
                        stellar_xdr::curr::Hash([0; 32]),
                    ],
                    ext: stellar_xdr::curr::LedgerHeaderExt::V0,
                },
                ext: stellar_xdr::curr::LedgerHeaderHistoryEntryExt::V0,
            },
            tx_set: stellar_xdr::curr::TransactionSet {
                previous_ledger_hash: stellar_xdr::curr::Hash([0; 32]),
                txs: vec![].try_into().unwrap(),
            },
            tx_processing: vec![].try_into().unwrap(),
            upgrades_processing: vec![].try_into().unwrap(),
            scp_info: vec![].try_into().unwrap(),
        });

        let result = writer.write_meta(meta, 1.into()).await;
        assert!(result.is_err(), "should fail after shutdown");
    }
}
