use flate2::{write::GzEncoder, Compression};
use henyey_history::download::{
    decompress_gzip, parse_record_marked_xdr_stream_or_empty, parse_xdr_stream,
};
use stellar_xdr::{
    Hash, LedgerHeader, LedgerHeaderExt, LedgerHeaderHistoryEntry, LedgerHeaderHistoryEntryExt,
    StellarValue, StellarValueExt, TimePoint, VecM, WriteXdr,
};

fn gzip_bytes(data: &[u8]) -> Vec<u8> {
    let mut encoder = GzEncoder::new(Vec::new(), Compression::default());
    use std::io::Write;
    encoder.write_all(data).expect("gzip write");
    encoder.finish().expect("gzip finish")
}

fn record_marked(payload: &[u8]) -> Vec<u8> {
    let len = payload.len() as u32;
    let mark = len | 0x8000_0000;
    let mut out = Vec::with_capacity(4 + payload.len());
    out.extend_from_slice(&mark.to_be_bytes());
    out.extend_from_slice(payload);
    out
}

fn make_header(seq: u32) -> LedgerHeaderHistoryEntry {
    let header = LedgerHeader {
        ledger_version: 25,
        previous_ledger_hash: Hash([0u8; 32]),
        scp_value: StellarValue {
            tx_set_hash: Hash([1u8; 32]),
            close_time: TimePoint(0),
            upgrades: VecM::default(),
            ext: StellarValueExt::Basic,
        },
        tx_set_result_hash: Hash([2u8; 32]),
        bucket_list_hash: Hash([3u8; 32]),
        ledger_seq: seq,
        total_coins: 1_000_000,
        fee_pool: 0,
        inflation_seq: 0,
        id_pool: 0,
        base_fee: 100,
        base_reserve: 100,
        max_tx_set_size: 100,
        skip_list: [
            Hash([0u8; 32]),
            Hash([0u8; 32]),
            Hash([0u8; 32]),
            Hash([0u8; 32]),
        ],
        ext: LedgerHeaderExt::V0,
    };

    LedgerHeaderHistoryEntry {
        hash: Hash([0u8; 32]),
        header,
        ext: LedgerHeaderHistoryEntryExt::default(),
    }
}

#[test]
fn test_decompress_gzip_roundtrip() {
    let payload = b"history-compress-test";
    let compressed = gzip_bytes(payload);
    let decompressed = decompress_gzip(&compressed).expect("decompress");
    assert_eq!(decompressed, payload);
}

#[test]
fn test_parse_xdr_stream_raw() {
    let entry_a = make_header(63);
    let entry_b = make_header(64);
    let mut stream = Vec::new();
    stream.extend_from_slice(&entry_a.to_xdr(stellar_xdr::Limits::none()).expect("xdr a"));
    stream.extend_from_slice(&entry_b.to_xdr(stellar_xdr::Limits::none()).expect("xdr b"));

    let parsed = parse_xdr_stream::<LedgerHeaderHistoryEntry>(&stream).expect("parse");
    assert_eq!(parsed.len(), 2);
    assert_eq!(parsed[0].header.ledger_seq, 63);
    assert_eq!(parsed[1].header.ledger_seq, 64);
}

#[test]
fn test_parse_xdr_stream_record_marked() {
    let entry_a = make_header(127);
    let entry_b = make_header(128);
    let entry_a_xdr = entry_a.to_xdr(stellar_xdr::Limits::none()).expect("xdr a");
    let entry_b_xdr = entry_b.to_xdr(stellar_xdr::Limits::none()).expect("xdr b");

    let mut stream = Vec::new();
    stream.extend_from_slice(&record_marked(&entry_a_xdr));
    stream.extend_from_slice(&record_marked(&entry_b_xdr));

    let parsed = parse_record_marked_xdr_stream_or_empty::<LedgerHeaderHistoryEntry>(&stream)
        .expect("parse");
    assert_eq!(parsed.len(), 2);
    assert_eq!(parsed[0].header.ledger_seq, 127);
    assert_eq!(parsed[1].header.ledger_seq, 128);
}
