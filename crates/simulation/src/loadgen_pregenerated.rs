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

use std::path::Path;

use stellar_xdr::{Limits, ReadXdr, TransactionEnvelope};

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

            let last_fragment = (mark & 0x8000_0000) != 0;
            let frag_len = (mark & 0x7FFF_FFFF) as usize;

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

    /// Wrap a single XDR blob in one record mark (last-fragment set).
    fn record_mark(bytes: &[u8]) -> Vec<u8> {
        let mut out = Vec::new();
        let mark = (bytes.len() as u32) | 0x8000_0000;
        out.extend_from_slice(&mark.to_be_bytes());
        out.extend_from_slice(bytes);
        out
    }

    #[test]
    fn reads_record_marked_envelopes_in_order() {
        let envs = [make_env(10), make_env(20), make_env(30)];
        let mut file = Vec::new();
        for e in &envs {
            file.extend_from_slice(&record_mark(&e.to_xdr(Limits::none()).unwrap()));
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
}
