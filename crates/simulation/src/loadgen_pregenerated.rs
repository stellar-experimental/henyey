//! Pre-generated transaction file reader for the `PayPregenerated` load mode.
//!
//! Mirrors stellar-core's `XDRInputFileStream::readOne` framing: the file is a
//! sequence of XDR records, each prefixed by a 4-byte big-endian *record mark*
//! (RFC 4506 record marking). The high bit (`0x80000000`) flags the last
//! fragment of a record; the lower 31 bits give the fragment length. A
//! `TransactionEnvelope` is marshaled into each record.
//!
//! This is a record-marking reader, **not** a naive concatenated-XDR reader:
//! envelopes are length-prefixed, so a reader that simply decoded
//! back-to-back `TransactionEnvelope`s would desynchronize on the first record.
//!
//! The reader is stateful — it loads the whole file into memory once and yields
//! one envelope per [`PregeneratedTxReader::read_one`] call, advancing an
//! internal offset. This matches stellar-core's behavior where the open file
//! stream's position persists across load-generation steps.

use std::io::Write;
use std::path::Path;

use stellar_xdr::{Limits, ReadXdr, TransactionEnvelope, WriteXdr};

/// RFC 4506 record-marking high-bit flag for the last fragment of a record.
const LAST_FRAGMENT_FLAG: u32 = 0x8000_0000;
/// Mask for the lower 31 bits that carry the fragment length.
const FRAGMENT_LEN_MASK: u32 = 0x7FFF_FFFF;

/// Build the 4-byte big-endian record mark for a *single, last* fragment of
/// `len` bytes.
///
/// Mirrors stellar-core `XDROutputFileStream::writeOne` (`XDRStream.h:483`):
/// the high bit of the high byte is set to flag the last fragment, and the
/// lower 31 bits carry the byte length, big-endian.
///
/// This is the single source of truth for the framing — both [`write_one`]
/// and [`PregeneratedTxReader::read_one`] go through it, so the writer and
/// reader can never drift.
fn record_mark(len: usize) -> [u8; 4] {
    debug_assert!(
        (len as u64) < LAST_FRAGMENT_FLAG as u64,
        "record fragment length must fit in 31 bits"
    );
    ((len as u32) | LAST_FRAGMENT_FLAG).to_be_bytes()
}

/// Write a single `TransactionEnvelope` as one record-marked, single-fragment
/// record: a 4-byte big-endian record mark (`xdr_len | 0x8000_0000`) followed
/// by the marshaled XDR bytes.
///
/// Mirrors stellar-core `XDROutputFileStream::writeOne`. The bytes produced
/// here are read back identically by [`PregeneratedTxReader::read_one`].
pub fn write_one(out: &mut impl Write, env: &TransactionEnvelope) -> anyhow::Result<()> {
    let bytes = env
        .to_xdr(Limits::none())
        .map_err(|e| anyhow::anyhow!("failed to encode TransactionEnvelope: {e}"))?;
    out.write_all(&record_mark(bytes.len()))?;
    out.write_all(&bytes)?;
    Ok(())
}

/// Stateful reader over a record-marked file of `TransactionEnvelope`s.
pub struct PregeneratedTxReader {
    data: Vec<u8>,
    offset: usize,
}

impl PregeneratedTxReader {
    /// Open and buffer a pre-generated transactions file.
    pub fn open(path: &Path) -> anyhow::Result<Self> {
        let data = std::fs::read(path)
            .map_err(|e| anyhow::anyhow!("failed to open preloaded tx file {path:?}: {e}"))?;
        Ok(Self { data, offset: 0 })
    }

    /// Construct a reader over an in-memory record-marked buffer (testing).
    #[cfg(test)]
    pub fn from_bytes(data: Vec<u8>) -> Self {
        Self { data, offset: 0 }
    }

    /// Read the next `TransactionEnvelope`.
    ///
    /// Returns `Ok(None)` at end of stream. Mirrors
    /// `XDRInputFileStream::readOne`: a record may be split into multiple
    /// fragments, each with its own 4-byte mark; fragments are concatenated
    /// until the last-fragment flag is seen, then the assembled bytes are
    /// decoded as a single `TransactionEnvelope`.
    pub fn read_one(&mut self) -> anyhow::Result<Option<TransactionEnvelope>> {
        if self.offset >= self.data.len() {
            return Ok(None);
        }

        let mut record: Vec<u8> = Vec::new();
        loop {
            if self.offset + 4 > self.data.len() {
                anyhow::bail!(
                    "truncated record mark at offset {} (file len {})",
                    self.offset,
                    self.data.len()
                );
            }
            let mark = u32::from_be_bytes([
                self.data[self.offset],
                self.data[self.offset + 1],
                self.data[self.offset + 2],
                self.data[self.offset + 3],
            ]);
            self.offset += 4;

            let last_fragment = (mark & LAST_FRAGMENT_FLAG) != 0;
            let frag_len = (mark & FRAGMENT_LEN_MASK) as usize;

            if self.offset + frag_len > self.data.len() {
                anyhow::bail!(
                    "record fragment length {} exceeds remaining {} at offset {}",
                    frag_len,
                    self.data.len() - self.offset,
                    self.offset - 4
                );
            }
            record.extend_from_slice(&self.data[self.offset..self.offset + frag_len]);
            self.offset += frag_len;

            if last_fragment {
                break;
            }
        }

        let env = TransactionEnvelope::from_xdr(&record, Limits::none())
            .map_err(|e| anyhow::anyhow!("failed to decode TransactionEnvelope: {e}"))?;
        Ok(Some(env))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use stellar_xdr::{
        Memo, MuxedAccount, Operation, OperationBody, PaymentOp, Preconditions, SequenceNumber,
        Transaction, TransactionExt, TransactionV1Envelope, Uint256, VecM, WriteXdr,
    };

    /// Build a minimal signed-less payment `TransactionEnvelope` with the given
    /// sequence number, so tests can assert the reader preserves seq order.
    fn make_env(seq: i64) -> TransactionEnvelope {
        let src = MuxedAccount::Ed25519(Uint256([7u8; 32]));
        let op = Operation {
            source_account: None,
            body: OperationBody::Payment(PaymentOp {
                destination: MuxedAccount::Ed25519(Uint256([9u8; 32])),
                asset: stellar_xdr::Asset::Native,
                amount: 1,
            }),
        };
        let tx = Transaction {
            source_account: src,
            fee: 100,
            seq_num: SequenceNumber(seq),
            cond: Preconditions::None,
            memo: Memo::None,
            operations: vec![op].try_into().unwrap(),
            ext: TransactionExt::V0,
        };
        TransactionEnvelope::Tx(TransactionV1Envelope {
            tx,
            signatures: VecM::default(),
        })
    }

    /// Wrap a single XDR blob in one record mark (last-fragment set), reusing
    /// the production [`super::record_mark`] framing so tests cannot diverge
    /// from the writer/reader.
    fn record_marked(bytes: &[u8]) -> Vec<u8> {
        let mut out = Vec::new();
        out.extend_from_slice(&super::record_mark(bytes.len()));
        out.extend_from_slice(bytes);
        out
    }

    #[test]
    fn reads_record_marked_envelopes_in_order() {
        let envs = [make_env(10), make_env(20), make_env(30)];
        let mut file = Vec::new();
        for e in &envs {
            file.extend_from_slice(&record_marked(&e.to_xdr(Limits::none()).unwrap()));
        }

        let mut reader = PregeneratedTxReader::from_bytes(file);
        for expected in &envs {
            let got = reader.read_one().unwrap().expect("envelope present");
            assert_eq!(&got, expected);
        }
        // EOF.
        assert!(reader.read_one().unwrap().is_none());
    }

    #[test]
    fn naive_concatenated_xdr_would_desync() {
        // Concatenated (NO record marks) — a record-marking reader must reject
        // it (it would read the first 4 XDR bytes as a bogus length).
        let env = make_env(42);
        let raw = env.to_xdr(Limits::none()).unwrap();
        let mut file = Vec::new();
        file.extend_from_slice(&raw);
        file.extend_from_slice(&raw);

        let mut reader = PregeneratedTxReader::from_bytes(file);
        // The first 4 bytes of a TransactionEnvelope are the envelope-type
        // discriminant (0,0,0,2 for ENVELOPE_TYPE_TX) → tiny length, so the
        // decode of the "record" fails rather than yielding the real envelope.
        let result = reader.read_one();
        assert!(
            result.is_err() || result.unwrap() != Some(env),
            "record-marking reader must not accept naive concatenated XDR"
        );
    }

    #[test]
    fn handles_multi_fragment_record() {
        let env = make_env(99);
        let raw = env.to_xdr(Limits::none()).unwrap();
        let split = raw.len() / 2;

        let mut file = Vec::new();
        // Fragment 1 (not last).
        let mark1 = split as u32; // high bit clear
        file.extend_from_slice(&mark1.to_be_bytes());
        file.extend_from_slice(&raw[..split]);
        // Fragment 2 (last).
        let mark2 = ((raw.len() - split) as u32) | 0x8000_0000;
        file.extend_from_slice(&mark2.to_be_bytes());
        file.extend_from_slice(&raw[split..]);

        let mut reader = PregeneratedTxReader::from_bytes(file);
        let got = reader.read_one().unwrap().expect("envelope present");
        assert_eq!(got, env);
    }

    #[test]
    fn write_one_roundtrips_through_reader() {
        let envs = [make_env(1), make_env(2), make_env(3)];

        // Write each envelope with the production writer.
        let mut file = Vec::new();
        for e in &envs {
            write_one(&mut file, e).unwrap();
        }

        // The leading 4 bytes of the first record must be the single-fragment
        // mark `0x8000_0000 | xdr_len`.
        let xdr_len = envs[0].to_xdr(Limits::none()).unwrap().len() as u32;
        let mark = u32::from_be_bytes([file[0], file[1], file[2], file[3]]);
        assert_eq!(mark, 0x8000_0000 | xdr_len);
        assert_ne!(mark & 0x8000_0000, 0, "last-fragment bit must be set");

        // Read them all back via the reader — identical, in order.
        let mut reader = PregeneratedTxReader::from_bytes(file);
        for expected in &envs {
            let got = reader.read_one().unwrap().expect("envelope present");
            assert_eq!(&got, expected);
        }
        assert!(reader.read_one().unwrap().is_none());
    }
}
