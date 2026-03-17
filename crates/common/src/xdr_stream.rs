//! XDR output stream writer for size-prefixed binary frames.
//!
//! Implements the same wire format as stellar-core's `XDROutputFileStream`,
//! enabling binary-compatible metadata streaming to downstream consumers
//! (Horizon, ingestion pipelines).
//!
//! # Wire Format
//!
//! Each frame is written as:
//! ```text
//! [4-byte big-endian size with continuation bit] [XDR payload]
//! ```
//!
//! The size header has bit 31 set (the "continuation bit"), matching the
//! XDR record marking standard (RFC 1832 / RFC 4506).

use std::fs::File;
use std::io::{self, BufReader, BufWriter, Read, Write};
use std::path::Path;

use stellar_xdr::curr::{Limits, ReadXdr, WriteXdr};

const FRAME_CONTINUATION_BIT: u8 = 0x80;
const MAX_FRAME_SIZE: u32 = 0x8000_0000;

/// Compute the XDR-encoded byte length of a value without heap allocation.
///
/// Uses a counting writer that discards output bytes, avoiding the `Vec<u8>`
/// allocation that `to_xdr(Limits::none())` would perform. For values
/// serialized only to measure their size (e.g., contract events, return values,
/// ledger entry size validation), this is significantly faster.
pub fn xdr_encoded_len(val: &impl WriteXdr) -> usize {
    struct CountingWriter(usize);
    impl Write for CountingWriter {
        fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
            self.0 += buf.len();
            Ok(buf.len())
        }
        fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }
    }
    let mut w = stellar_xdr::curr::Limited::new(CountingWriter(0), Limits::none());
    val.write_xdr(&mut w).map(|_| w.inner.0).unwrap_or(0)
}

#[inline]
fn encode_frame_header(size: u32) -> [u8; 4] {
    [
        ((size >> 24) & 0xFF) as u8 | FRAME_CONTINUATION_BIT,
        ((size >> 16) & 0xFF) as u8,
        ((size >> 8) & 0xFF) as u8,
        (size & 0xFF) as u8,
    ]
}

#[inline]
fn decode_frame_size(header: [u8; 4]) -> u32 {
    (((header[0] & !FRAME_CONTINUATION_BIT) as u32) << 24)
        | ((header[1] as u32) << 16)
        | ((header[2] as u32) << 8)
        | (header[3] as u32)
}

/// Serialize a value to XDR and write it as a size-prefixed frame.
///
/// Returns the total number of bytes written (4-byte header + payload).
///
/// # Wire Format
///
/// ```text
/// byte[0] = ((sz >> 24) & 0xFF) | 0x80   // continuation bit set
/// byte[1] = (sz >> 16) & 0xFF
/// byte[2] = (sz >> 8) & 0xFF
/// byte[3] = sz & 0xFF
/// byte[4..4+sz] = XDR payload
/// ```
///
/// # Panics
///
/// Panics if the serialized XDR payload is >= 0x80000000 bytes (2 GiB),
/// matching stellar-core's `releaseAssertOrThrow`.
fn write_frame<W: Write>(writer: &mut W, value: &impl WriteXdr) -> io::Result<usize> {
    let payload = value
        .to_xdr(Limits::none())
        .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))?;

    let sz = payload.len() as u32;
    assert!(
        sz < MAX_FRAME_SIZE,
        "XDR payload size {} exceeds maximum (0x80000000)",
        sz
    );

    // Write 4-byte size header with continuation bit (bit 31) set
    let header = encode_frame_header(sz);

    writer.write_all(&header)?;
    writer.write_all(&payload)?;

    Ok(4 + payload.len())
}

/// An output stream that writes XDR values with size-prefix framing.
///
/// Each value is serialized to XDR, then written as a 4-byte big-endian
/// size header (with bit 31 set) followed by the XDR payload bytes.
/// This matches the wire format produced by stellar-core's
/// `XDROutputFileStream::writeOne`.
pub struct XdrOutputStream {
    writer: BufWriter<Box<dyn Write + Send>>,
}

impl XdrOutputStream {
    /// Open an XDR output stream writing to a file path.
    ///
    /// The path can be a regular file or a named pipe (FIFO).
    pub fn open(path: &str) -> io::Result<Self> {
        let file = std::fs::OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(true)
            .open(path)?;
        Ok(Self {
            writer: BufWriter::new(Box::new(file)),
        })
    }

    /// Create an XDR output stream from a raw file descriptor.
    ///
    /// # Safety
    ///
    /// The caller must ensure the file descriptor is valid and open for writing.
    /// This function takes ownership of the file descriptor.
    #[cfg(unix)]
    pub fn from_fd(fd: std::os::unix::io::RawFd) -> io::Result<Self> {
        use std::os::unix::io::FromRawFd;
        // SAFETY: caller guarantees the fd is valid and open for writing
        let file = unsafe { std::fs::File::from_raw_fd(fd) };
        Ok(Self {
            writer: BufWriter::new(Box::new(file)),
        })
    }

    /// Create an XDR output stream from any writer.
    ///
    /// Useful for testing with in-memory buffers.
    pub fn from_writer(writer: Box<dyn Write + Send>) -> Self {
        Self {
            writer: BufWriter::new(writer),
        }
    }

    /// Serialize a value to XDR and write it as a size-prefixed frame.
    ///
    /// Returns the total number of bytes written (4-byte header + payload).
    pub fn write_one<T: WriteXdr>(&mut self, value: &T) -> io::Result<usize> {
        let n = write_frame(&mut self.writer, value)?;
        self.writer.flush()?;
        Ok(n)
    }

    /// Flush the underlying writer.
    pub fn flush(&mut self) -> io::Result<()> {
        self.writer.flush()
    }
}

/// A durable XDR output stream that fsyncs after every write.
///
/// This matches stellar-core's `XDROutputFileStream` with `fsyncOnClose=true`
/// and its `durableWriteOne()` method (flush + fsync after every entry).
///
/// Used during crash recovery to write truncated checkpoint entries, where
/// each entry must be durably persisted before proceeding.
///
/// Unlike [`XdrOutputStream`], this wraps a [`File`] directly so that
/// `sync_all()` can be called on the underlying file descriptor.
pub struct DurableXdrOutputStream {
    writer: BufWriter<File>,
}

impl DurableXdrOutputStream {
    /// Open a durable XDR output stream writing to a file path.
    ///
    /// Creates the file if it doesn't exist, truncates if it does.
    pub fn open(path: &Path) -> io::Result<Self> {
        let file = std::fs::OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(true)
            .open(path)?;
        Ok(Self {
            writer: BufWriter::new(file),
        })
    }

    /// Write an XDR entry durably: serialize, flush buffer, then fsync.
    ///
    /// This matches stellar-core's `durableWriteOne()`: the entry is written
    /// to the buffer, the buffer is flushed to the OS, and then `fsync` is
    /// called to ensure the data is on stable storage.
    ///
    /// Returns the total number of bytes written (4-byte header + payload).
    pub fn durable_write_one<T: WriteXdr>(&mut self, value: &T) -> io::Result<usize> {
        let n = write_frame(&mut self.writer, value)?;

        // Flush the BufWriter to the OS
        self.writer.flush()?;

        // Fsync to stable storage
        self.writer.get_ref().sync_all()?;

        Ok(n)
    }

    /// Close the stream, flushing and fsyncing.
    pub fn close(mut self) -> io::Result<()> {
        self.writer.flush()?;
        self.writer.get_ref().sync_all()?;
        Ok(())
    }
}

/// An input stream that reads XDR values with size-prefix framing.
///
/// Reads the same wire format as [`XdrOutputStream`]: a 4-byte big-endian
/// size header (with bit 31 set as the continuation bit) followed by the
/// XDR payload bytes. This matches stellar-core's `XDRInputFileStream`.
pub struct XdrInputStream {
    reader: BufReader<Box<dyn Read + Send>>,
}

impl XdrInputStream {
    /// Open an XDR input stream from a file path.
    pub fn open(path: &str) -> io::Result<Self> {
        let file = std::fs::File::open(path)?;
        Ok(Self {
            reader: BufReader::new(Box::new(file)),
        })
    }

    /// Create an XDR input stream from any reader.
    ///
    /// Useful for testing with in-memory buffers.
    pub fn from_reader(reader: Box<dyn Read + Send>) -> Self {
        Self {
            reader: BufReader::new(reader),
        }
    }

    /// Read one XDR value from the stream.
    ///
    /// Returns `None` at end of stream, or an error if the data is malformed.
    pub fn read_one<T: ReadXdr>(&mut self) -> io::Result<Option<T>> {
        // Read 4-byte size header
        let mut header = [0u8; 4];
        match self.reader.read_exact(&mut header) {
            Ok(()) => {}
            Err(e) if e.kind() == io::ErrorKind::UnexpectedEof => return Ok(None),
            Err(e) => return Err(e),
        }

        // Extract size (strip continuation bit from high byte)
        let sz = decode_frame_size(header);

        // Read payload
        let mut payload = vec![0u8; sz as usize];
        self.reader.read_exact(&mut payload)?;

        // Deserialize XDR
        let value = T::from_xdr(&payload, Limits::none())
            .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))?;

        Ok(Some(value))
    }

    /// Read all XDR values from the stream until EOF.
    pub fn read_all<T: ReadXdr>(&mut self) -> io::Result<Vec<T>> {
        let mut entries = Vec::new();
        while let Some(entry) = self.read_one::<T>()? {
            entries.push(entry);
        }
        Ok(entries)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::{Arc, Mutex};
    use stellar_xdr::curr::{LedgerCloseMeta, LedgerCloseMetaV2, ReadXdr};

    /// A thread-safe in-memory writer that allows reading the buffer after writing.
    #[derive(Clone)]
    struct SharedBuffer(Arc<Mutex<Vec<u8>>>);

    impl SharedBuffer {
        fn new() -> Self {
            Self(Arc::new(Mutex::new(Vec::new())))
        }
        fn data(&self) -> Vec<u8> {
            self.0.lock().unwrap().clone()
        }
    }

    impl io::Write for SharedBuffer {
        fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
            self.0.lock().unwrap().extend_from_slice(buf);
            Ok(buf.len())
        }
        fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }
    }

    /// Helper to extract size from a 4-byte header with continuation bit.
    fn read_frame_size(data: &[u8], offset: usize) -> u32 {
        decode_frame_size([
            data[offset],
            data[offset + 1],
            data[offset + 2],
            data[offset + 3],
        ])
    }

    #[test]
    fn test_write_one_header_format() {
        let buf = SharedBuffer::new();
        let mut stream = XdrOutputStream::from_writer(Box::new(buf.clone()));

        // Write a minimal LedgerCloseMeta::V2
        let meta = LedgerCloseMeta::V2(LedgerCloseMetaV2::default());
        let bytes_written = stream.write_one(&meta).unwrap();

        let data = buf.data();

        // Verify the header has the continuation bit set
        assert!(
            data[0] & FRAME_CONTINUATION_BIT != 0,
            "continuation bit must be set"
        );

        // Extract the size from the header
        let sz = read_frame_size(&data, 0);

        // Verify size matches payload
        assert_eq!(sz as usize, data.len() - 4);
        assert_eq!(bytes_written, data.len());

        // Verify the payload decodes back to the same value
        let decoded =
            LedgerCloseMeta::from_xdr(&data[4..], stellar_xdr::curr::Limits::none()).unwrap();
        assert_eq!(
            decoded.to_xdr(stellar_xdr::curr::Limits::none()).unwrap(),
            meta.to_xdr(stellar_xdr::curr::Limits::none()).unwrap()
        );
    }

    #[test]
    fn test_write_multiple_frames() {
        let buf = SharedBuffer::new();
        let mut stream = XdrOutputStream::from_writer(Box::new(buf.clone()));

        let meta1 = LedgerCloseMeta::V2(LedgerCloseMetaV2::default());
        let meta2 = LedgerCloseMeta::V2(LedgerCloseMetaV2::default());

        let bytes1 = stream.write_one(&meta1).unwrap();
        let bytes2 = stream.write_one(&meta2).unwrap();

        let data = buf.data();
        assert_eq!(data.len(), bytes1 + bytes2);

        // Decode first frame
        let sz1 = read_frame_size(&data, 0);
        let frame1_end = 4 + sz1 as usize;
        let _decoded1 =
            LedgerCloseMeta::from_xdr(&data[4..frame1_end], stellar_xdr::curr::Limits::none())
                .unwrap();

        // Decode second frame
        let sz2 = read_frame_size(&data, frame1_end);
        let frame2_end = frame1_end + 4 + sz2 as usize;
        let _decoded2 = LedgerCloseMeta::from_xdr(
            &data[frame1_end + 4..frame2_end],
            stellar_xdr::curr::Limits::none(),
        )
        .unwrap();
    }

    // XdrInputStream tests

    #[test]
    fn test_xdr_input_stream_roundtrip() {
        // Write entries with XdrOutputStream, read back with XdrInputStream
        let buf = SharedBuffer::new();
        let mut out = XdrOutputStream::from_writer(Box::new(buf.clone()));

        let meta1 = LedgerCloseMeta::V2(LedgerCloseMetaV2::default());
        let meta2 = LedgerCloseMeta::V2(LedgerCloseMetaV2::default());
        out.write_one(&meta1).unwrap();
        out.write_one(&meta2).unwrap();

        let data = buf.data();
        let cursor = std::io::Cursor::new(data);
        let mut input = XdrInputStream::from_reader(Box::new(cursor));

        let entries: Vec<LedgerCloseMeta> = input.read_all().unwrap();
        assert_eq!(entries.len(), 2);

        // Verify roundtrip fidelity
        assert_eq!(
            entries[0].to_xdr(Limits::none()).unwrap(),
            meta1.to_xdr(Limits::none()).unwrap()
        );
    }

    #[test]
    fn test_xdr_input_stream_empty() {
        let cursor = std::io::Cursor::new(Vec::<u8>::new());
        let mut input = XdrInputStream::from_reader(Box::new(cursor));

        let entries: Vec<LedgerCloseMeta> = input.read_all().unwrap();
        assert!(entries.is_empty());
    }

    #[test]
    fn test_xdr_input_stream_read_one() {
        let buf = SharedBuffer::new();
        let mut out = XdrOutputStream::from_writer(Box::new(buf.clone()));

        let meta = LedgerCloseMeta::V2(LedgerCloseMetaV2::default());
        out.write_one(&meta).unwrap();

        let data = buf.data();
        let cursor = std::io::Cursor::new(data);
        let mut input = XdrInputStream::from_reader(Box::new(cursor));

        let entry: Option<LedgerCloseMeta> = input.read_one().unwrap();
        assert!(entry.is_some());

        // Second read should return None (EOF)
        let entry2: Option<LedgerCloseMeta> = input.read_one().unwrap();
        assert!(entry2.is_none());
    }

    #[test]
    fn test_xdr_input_stream_truncated_header() {
        // Only 2 bytes — not enough for a 4-byte header
        let cursor = std::io::Cursor::new(vec![0x80, 0x00]);
        let mut input = XdrInputStream::from_reader(Box::new(cursor));

        let result: io::Result<Option<LedgerCloseMeta>> = input.read_one();
        // Should return None (EOF during header read) since UnexpectedEof maps to None
        assert!(result.unwrap().is_none());
    }

    #[test]
    fn test_xdr_input_stream_truncated_payload() {
        // Valid header saying 100 bytes, but only 4 bytes of payload
        let mut data = vec![0x80, 0x00, 0x00, 100]; // header: size=100
        data.extend_from_slice(&[0u8; 4]); // only 4 bytes of payload, need 100
        let cursor = std::io::Cursor::new(data);
        let mut input = XdrInputStream::from_reader(Box::new(cursor));

        let result: io::Result<Option<LedgerCloseMeta>> = input.read_one();
        assert!(result.is_err()); // UnexpectedEof during payload read
    }

    #[test]
    fn test_xdr_input_stream_multiple_roundtrip() {
        let buf = SharedBuffer::new();
        let mut out = XdrOutputStream::from_writer(Box::new(buf.clone()));

        // Write 5 entries
        for _ in 0..5 {
            let meta = LedgerCloseMeta::V2(LedgerCloseMetaV2::default());
            out.write_one(&meta).unwrap();
        }

        let data = buf.data();
        let cursor = std::io::Cursor::new(data);
        let mut input = XdrInputStream::from_reader(Box::new(cursor));

        let entries: Vec<LedgerCloseMeta> = input.read_all().unwrap();
        assert_eq!(entries.len(), 5);
    }

    #[test]
    fn test_xdr_input_stream_file_roundtrip() {
        let tmp = tempfile::NamedTempFile::new().unwrap();
        let path = tmp.path().to_str().unwrap().to_string();

        // Write
        {
            let mut out = XdrOutputStream::open(&path).unwrap();
            let meta1 = LedgerCloseMeta::V2(LedgerCloseMetaV2::default());
            let meta2 = LedgerCloseMeta::V2(LedgerCloseMetaV2::default());
            out.write_one(&meta1).unwrap();
            out.write_one(&meta2).unwrap();
        }

        // Read back
        {
            let mut input = XdrInputStream::open(&path).unwrap();
            let entries: Vec<LedgerCloseMeta> = input.read_all().unwrap();
            assert_eq!(entries.len(), 2);
        }
    }

    #[cfg(unix)]
    #[test]
    fn test_from_fd() {
        use std::os::unix::io::IntoRawFd;

        let tmp = tempfile::NamedTempFile::new().unwrap();
        let path = tmp.path().to_path_buf();
        let file = std::fs::OpenOptions::new().write(true).open(&path).unwrap();
        let fd = file.into_raw_fd();

        let mut stream = XdrOutputStream::from_fd(fd).unwrap();
        let meta = LedgerCloseMeta::V2(LedgerCloseMetaV2::default());
        stream.write_one(&meta).unwrap();
        drop(stream);

        // Read back and verify
        let data = std::fs::read(&path).unwrap();
        assert!(data[0] & FRAME_CONTINUATION_BIT != 0);
        let sz = read_frame_size(&data, 0);
        let _decoded =
            LedgerCloseMeta::from_xdr(&data[4..4 + sz as usize], stellar_xdr::curr::Limits::none())
                .unwrap();
    }

    #[test]
    fn test_durable_write_one_basic() {
        let tmp = tempfile::NamedTempFile::new().unwrap();
        let path = tmp.path().to_path_buf();

        // Write durably
        {
            let mut stream = DurableXdrOutputStream::open(&path).unwrap();
            let meta = LedgerCloseMeta::V2(LedgerCloseMetaV2::default());
            let bytes_written = stream.durable_write_one(&meta).unwrap();
            assert!(bytes_written > 4); // header + payload
            stream.close().unwrap();
        }

        // Read back with XdrInputStream
        {
            let mut input = XdrInputStream::open(path.to_str().unwrap()).unwrap();
            let entries: Vec<LedgerCloseMeta> = input.read_all().unwrap();
            assert_eq!(entries.len(), 1);
        }
    }

    #[test]
    fn test_durable_write_one_multiple_entries() {
        let tmp = tempfile::NamedTempFile::new().unwrap();
        let path = tmp.path().to_path_buf();

        // Write 3 entries durably
        {
            let mut stream = DurableXdrOutputStream::open(&path).unwrap();
            for _ in 0..3 {
                let meta = LedgerCloseMeta::V2(LedgerCloseMetaV2::default());
                stream.durable_write_one(&meta).unwrap();
            }
            stream.close().unwrap();
        }

        // Read back
        {
            let mut input = XdrInputStream::open(path.to_str().unwrap()).unwrap();
            let entries: Vec<LedgerCloseMeta> = input.read_all().unwrap();
            assert_eq!(entries.len(), 3);
        }
    }

    #[test]
    fn test_durable_write_one_wire_compatible_with_xdr_output_stream() {
        // Verify that DurableXdrOutputStream produces the same wire format as XdrOutputStream
        let tmp_durable = tempfile::NamedTempFile::new().unwrap();
        let tmp_normal = tempfile::NamedTempFile::new().unwrap();
        let path_durable = tmp_durable.path().to_path_buf();
        let path_normal = tmp_normal.path().to_path_buf();

        let meta = LedgerCloseMeta::V2(LedgerCloseMetaV2::default());

        // Write with durable stream
        {
            let mut stream = DurableXdrOutputStream::open(&path_durable).unwrap();
            stream.durable_write_one(&meta).unwrap();
            stream.close().unwrap();
        }

        // Write with normal stream
        {
            let mut stream = XdrOutputStream::open(path_normal.to_str().unwrap()).unwrap();
            stream.write_one(&meta).unwrap();
            drop(stream);
        }

        // Compare bytes — should be identical
        let bytes_durable = std::fs::read(&path_durable).unwrap();
        let bytes_normal = std::fs::read(&path_normal).unwrap();
        assert_eq!(bytes_durable, bytes_normal, "Wire format must be identical");
    }
}
