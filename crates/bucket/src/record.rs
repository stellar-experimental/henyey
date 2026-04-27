//! Shared XDR record-mark readers for bucket files.
//!
//! Stellar-core's `XDRInputFileStream::readOne` treats only a zero-byte read of
//! the next record mark as clean EOF. Partial marks, truncated bodies, and
//! decode failures must surface as errors so malformed bucket files cannot look
//! like valid prefixes.

use std::io::Read;

use crate::{BucketError, Result};

#[derive(Debug)]
#[allow(dead_code)]
pub struct Record<'a> {
    pub(crate) offset: u64,
    pub(crate) mark_bytes: [u8; 4],
    pub(crate) declared_len: usize,
    pub(crate) body: &'a [u8],
    pub(crate) next_offset: u64,
}

#[derive(Debug)]
#[allow(dead_code)]
pub struct SliceRecord<'a> {
    pub(crate) offset: usize,
    pub(crate) mark_bytes: [u8; 4],
    pub(crate) declared_len: usize,
    pub(crate) body: &'a [u8],
    pub(crate) next_offset: usize,
}

pub struct RecordMarkedReader<R> {
    reader: R,
    file_len: u64,
    position: u64,
    scratch: Vec<u8>,
    done: bool,
}

impl<R: Read> RecordMarkedReader<R> {
    pub(crate) fn new(reader: R, file_len: u64) -> Self {
        Self::new_at(reader, file_len, 0)
    }

    pub(crate) fn new_at(reader: R, file_len: u64, position: u64) -> Self {
        Self {
            reader,
            file_len,
            position,
            scratch: Vec::with_capacity(4096),
            done: false,
        }
    }

    pub(crate) fn position(&self) -> u64 {
        self.position
    }

    pub(crate) fn next_record(&mut self) -> Result<Option<Record<'_>>> {
        if self.done {
            return Ok(None);
        }

        if self.position == self.file_len {
            self.done = true;
            return Ok(None);
        }

        let remaining = self.file_len.checked_sub(self.position).ok_or_else(|| {
            BucketError::Serialization(format!(
                "record reader position {} exceeds file length {}",
                self.position, self.file_len
            ))
        })?;
        if remaining < 4 {
            self.done = true;
            return Err(BucketError::Io(std::io::Error::new(
                std::io::ErrorKind::UnexpectedEof,
                format!(
                    "short XDR record mark at offset {}: {} byte(s) available",
                    self.position, remaining
                ),
            )));
        }

        let offset = self.position;
        let mut mark_bytes = [0u8; 4];
        if let Err(e) = self.reader.read_exact(&mut mark_bytes) {
            self.done = true;
            return Err(BucketError::Io(e));
        }

        let declared_len = (u32::from_be_bytes(mark_bytes) & crate::XDR_RECORD_LEN_MASK) as usize;
        let body_start = offset.checked_add(4).ok_or_else(|| {
            BucketError::Serialization(format!("record offset overflow at {}", offset))
        })?;
        let body_len = u64::try_from(declared_len).map_err(|_| {
            BucketError::Serialization(format!(
                "record length {} at offset {} does not fit in u64",
                declared_len, offset
            ))
        })?;
        let next_offset = body_start.checked_add(body_len).ok_or_else(|| {
            BucketError::Serialization(format!(
                "record length {} at offset {} overflows file position",
                declared_len, offset
            ))
        })?;
        if next_offset > self.file_len {
            self.done = true;
            return Err(BucketError::Io(std::io::Error::new(
                std::io::ErrorKind::UnexpectedEof,
                format!(
                    "truncated XDR record at offset {}: declared {} byte body, {} byte(s) available",
                    offset,
                    declared_len,
                    self.file_len.saturating_sub(body_start)
                ),
            )));
        }

        self.scratch.resize(declared_len, 0);
        if let Err(e) = self.reader.read_exact(&mut self.scratch) {
            self.done = true;
            return Err(BucketError::Io(e));
        }
        self.position = next_offset;

        Ok(Some(Record {
            offset,
            mark_bytes,
            declared_len,
            body: &self.scratch,
            next_offset,
        }))
    }
}

pub(crate) struct RecordMarkedSliceIter<'a> {
    bytes: &'a [u8],
    position: usize,
    done: bool,
}

impl<'a> RecordMarkedSliceIter<'a> {
    pub(crate) fn new(bytes: &'a [u8]) -> Self {
        Self {
            bytes,
            position: 0,
            done: false,
        }
    }

    pub(crate) fn at(bytes: &'a [u8], position: usize) -> Self {
        Self {
            bytes,
            position,
            done: false,
        }
    }

    pub(crate) fn position(&self) -> usize {
        self.position
    }

    pub(crate) fn next_record(&mut self) -> Result<Option<SliceRecord<'a>>> {
        if self.done {
            return Ok(None);
        }

        if self.position == self.bytes.len() {
            self.done = true;
            return Ok(None);
        }

        if self.position > self.bytes.len() {
            self.done = true;
            return Err(BucketError::Serialization(format!(
                "record slice position {} exceeds length {}",
                self.position,
                self.bytes.len()
            )));
        }

        let remaining = self.bytes.len() - self.position;
        if remaining < 4 {
            self.done = true;
            return Err(BucketError::Io(std::io::Error::new(
                std::io::ErrorKind::UnexpectedEof,
                format!(
                    "short XDR record mark at offset {}: {} byte(s) available",
                    self.position, remaining
                ),
            )));
        }

        let offset = self.position;
        let mark_bytes: [u8; 4] = self.bytes[offset..offset + 4]
            .try_into()
            .expect("slice length checked");
        let declared_len = (u32::from_be_bytes(mark_bytes) & crate::XDR_RECORD_LEN_MASK) as usize;
        let body_start = offset.checked_add(4).ok_or_else(|| {
            BucketError::Serialization(format!("record offset overflow at {}", offset))
        })?;
        let next_offset = body_start.checked_add(declared_len).ok_or_else(|| {
            BucketError::Serialization(format!(
                "record length {} at offset {} overflows slice position",
                declared_len, offset
            ))
        })?;
        if next_offset > self.bytes.len() {
            self.done = true;
            return Err(BucketError::Io(std::io::Error::new(
                std::io::ErrorKind::UnexpectedEof,
                format!(
                    "truncated XDR record at offset {}: declared {} byte body, {} byte(s) available",
                    offset,
                    declared_len,
                    self.bytes.len().saturating_sub(body_start)
                ),
            )));
        }

        self.position = next_offset;
        Ok(Some(SliceRecord {
            offset,
            mark_bytes,
            declared_len,
            body: &self.bytes[body_start..next_offset],
            next_offset,
        }))
    }
}
