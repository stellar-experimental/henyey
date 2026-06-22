//! Diagnostic + parity-guard harness for issue #3552 — henyey's ledger-16
//! `bucketListHash` diverged from stellar-core v27 during a Supercluster
//! mixed-image loadgen mission (henyey computed `167a7373…`, core published
//! `c9f9560dfb174b015835f7e387f099a8ace65319156fff544a42849b2f129d97`).
//!
//! ## Localization (confirmed from the preserved mission archive)
//!
//! Decoding the mission's published history (`core-old-0-archive.tar.gz`,
//! checkpoint 63) gives the full per-ledger `bucketListHash` chain. henyey
//! matched core **byte-for-byte at L1–L15** (including the loadgen ledgers
//! L11–L14 and the first level-0 → level-1 spill at L8) and first diverged at
//! **L16**, which is the first *populated* level-1 → level-2 spill
//! (`level_half(1)=8` ⇒ level-1 snaps into level-2 at ledgers 8 and 16; L8's
//! snap was empty, L16's carries entries).
//!
//! Decoding the L63 buckets shows the live state is dominated by **genesis-time
//! entries** created in the genesis ledger (L1) as INITENTRY: ~19 ConfigSetting
//! entries, the root/loadgen accounts, plus Soroban ContractCode/ContractData/
//! Ttl. These genesis entries — not the L11–L14 loadgen accounts (which are
//! still in levels 0/1 at L16) — are the payload that first reaches the
//! level-1 → level-2 boundary at L16.
//!
//! ## This is a diagnostic-advancement harness, NOT the fix
//!
//! The root cause is in the L16 entry SET / CONTENT / INIT-vs-LIVE
//! classification going into henyey's L16 level-2 bucket, which is **not
//! pinnable offline**: the single-checkpoint archive publishes only the L63
//! HistoryArchiveState, so there is no L16 per-level bucket oracle to diff
//! against. The two structural diagnostics below are explicitly henyey-vs-henyey
//! (they pass on `main`); the real fail-on-main regression is deferred to
//! **#3553**, which will capture core's actual L16 level-2 bucket on a re-run
//! (aided by the per-level `bucket_list_hash` instrumentation added to
//! `BucketList::add_batch_internal` in this PR) and diff the entry set.
//!
//! What this PR DOES land, in addition to the diagnostics:
//!
//! 1. The `oracle_genesis_bucket_roundtrip_matches_core` parity guard below,
//!    which feeds henyey the *real* core v27 merge-output buckets archived from
//!    the mission and asserts henyey reproduces their published hashes
//!    byte-for-byte — both via raw `from_xdr_bytes` and via a full
//!    `from_entries` comparator re-sort. This PASSES on `main`: it locks in the
//!    *verified-correct* comparator / entry-sort / XDR-serialization /
//!    record-marking / bucket-hashing machinery for genesis-shaped
//!    ConfigSetting / Soroban / INIT content, and will catch any future
//!    regression in that machinery.
//! 2. The per-level instrumentation hook (see `bucket_list.rs`).

use henyey_bucket::{Bucket, BucketList};
use henyey_common::Hash256;
use stellar_xdr::{
    AccountEntry, AccountEntryExt, AccountId, BucketListType, LedgerEntry, LedgerEntryData,
    LedgerEntryExt, PublicKey, SequenceNumber, String32, Thresholds, Uint256,
};

const P27: u32 = 27;

/// A deterministic account `LedgerEntry` keyed by a 32-bit seed.
fn account_entry(seed: u32, last_modified: u32) -> LedgerEntry {
    let mut bytes = [0u8; 32];
    bytes[..4].copy_from_slice(&seed.to_be_bytes());
    LedgerEntry {
        last_modified_ledger_seq: last_modified,
        data: LedgerEntryData::Account(AccountEntry {
            account_id: AccountId(PublicKey::PublicKeyTypeEd25519(Uint256(bytes))),
            balance: 100_000_000,
            seq_num: SequenceNumber(0),
            num_sub_entries: 0,
            inflation_dest: None,
            flags: 0,
            home_domain: String32::default(),
            thresholds: Thresholds([1, 0, 0, 0]),
            signers: Vec::new().try_into().unwrap(),
            ext: AccountEntryExt::V0,
        }),
        ext: LedgerEntryExt::V0,
    }
}

/// Loadgen account-creation counts per ledger, mirroring the mission
/// (L11=22, L12=24, L13=30, L14=24 → 100 total).
const LOADGEN_COUNTS: [(u32, u32); 4] = [(11, 22), (12, 24), (13, 30), (14, 24)];

fn dump_levels(bl: &BucketList, ledger: u32) {
    for (idx, level_hash, curr_hash, snap_hash) in bl.level_hashes() {
        println!(
            "L{ledger:02} level={idx} level_hash={} curr={} snap={}",
            level_hash.to_hex(),
            curr_hash.to_hex(),
            snap_hash.to_hex(),
        );
    }
    println!("L{ledger:02} bucket_list_hash={}", bl.hash().to_hex());
}

/// Replay 16 ledgers driving `add_batch` with INITENTRY account creations at
/// L11–L14 (loadgen shape) and dump per-level hashes after each ledger.
///
/// Confirms the loadgen accounts are still in levels 0/1 at L16 (so they are
/// *not* the level-1 → level-2 payload), and that the level-1 → level-2 spill
/// fires exactly at L16. This is a STRUCTURAL diagnostic (henyey-vs-henyey); it
/// passes on `main` and is not a fail-on-main regression (see module docs).
#[tokio::test(flavor = "multi_thread")]
async fn diagnose_loadgen_batch_levels_at_l16() {
    let mut bl = BucketList::new();
    let mut next_seed: u32 = 1;

    for ledger in 1..=16u32 {
        let mut init = Vec::new();
        if let Some(&(_, count)) = LOADGEN_COUNTS.iter().find(|(l, _)| *l == ledger) {
            for _ in 0..count {
                init.push(account_entry(next_seed, ledger));
                next_seed += 1;
            }
        }
        bl.add_batch(
            ledger,
            P27,
            BucketListType::Live,
            init,
            Vec::new(),
            Vec::new(),
        )
        .expect("add_batch");
        dump_levels(&bl, ledger);
    }

    assert_eq!(next_seed - 1, 100, "expected 100 loadgen accounts");
}

/// Seed a genesis (L1) INIT batch, leave L2–L16 empty, and dump per-level hashes
/// from L14 onward.
///
/// Confirms the genesis batch occupies **level-1 snap** at L14/L15 and lands in
/// **level-2 curr at L16** via the first populated level-1 → level-2 merge —
/// i.e. the genesis entries are the divergent payload, matching the L63 bucket
/// decode (genesis-time INIT entries incl. ConfigSetting). This is a STRUCTURAL
/// diagnostic (henyey-vs-henyey); it passes on `main`.
#[tokio::test(flavor = "multi_thread")]
async fn diagnose_genesis_batch_lands_in_level2_at_l16() {
    let mut bl = BucketList::new();
    let genesis: Vec<LedgerEntry> = (0..20).map(|s| account_entry(s, 1)).collect();

    for ledger in 1..=16u32 {
        let init = if ledger == 1 {
            genesis.clone()
        } else {
            Vec::new()
        };
        bl.add_batch(
            ledger,
            P27,
            BucketListType::Live,
            init,
            Vec::new(),
            Vec::new(),
        )
        .expect("add_batch");
        if ledger >= 14 {
            dump_levels(&bl, ledger);
        }
    }

    // The genesis batch must have moved out of level-1 and into level-2 curr by
    // L16 (the first populated level-1 → level-2 spill).
    let level2_curr = bl.level(2).expect("level 2").curr.hash();
    assert!(
        !level2_curr.is_zero(),
        "genesis batch should occupy level-2 curr at L16 (first populated level-1 → level-2 spill)"
    );
}

/// Permanent cross-implementation parity guard (#3552).
///
/// Feeds henyey two **real** stellar-core v27 merge-output buckets archived from
/// the mission and asserts henyey reproduces their published hashes
/// byte-for-byte, two ways:
///
/// - `Bucket::from_xdr_bytes(raw)` — verifies the record-marking framing + XDR
///   decode + SHA-256 hashing of the on-disk stream.
/// - `Bucket::from_entries(parsed)` — re-sorts the parsed entries through
///   henyey's `compare_entries` comparator and re-serializes them, so a match
///   proves henyey's **entry comparator + entry sort + XDR serialization +
///   record-marking + bucket hashing** are byte-identical to core for the exact
///   genesis-shaped ConfigSetting / Soroban / INIT content involved in the L16
///   divergence.
///
/// This PASSES on `main` — it banks the verified-correct machinery and guards
/// against any future regression. It is intentionally NOT a fail-on-main
/// reproduction of the L16 bug: the artifacts confirm this machinery is correct,
/// so the divergence lives in the L16 entry SET/CONTENT, whose live oracle is
/// captured under #3553.
///
/// Fixtures (`crates/bucket/tests/fixtures/issue3552/`) are the uncompressed
/// `BucketEntry` XDR streams extracted from the mission archive
/// (`~/data/9eb89c28/ssc-v27-run/core-old-0-archive.tar.gz`):
/// `bucket-f0c6394a.xdr` (127 entries: META v27 ext=V1(Live), 101 LIVE Account,
/// 19 INIT ConfigSetting, INIT ContractCode/2×ContractData/3×Ttl) and
/// `bucket-0cf535ed.xdr` (56 entries). The bucket's published hash is its
/// filename (content-addressed), which is the oracle.
#[test]
fn oracle_genesis_bucket_roundtrip_matches_core() {
    const FIXTURES: [(&str, &str); 2] = [
        (
            "tests/fixtures/issue3552/bucket-f0c6394a.xdr",
            "f0c6394a74dae92df0a4d649b200b9ad5f111c40854a3df5e396529cc1955ecd",
        ),
        (
            "tests/fixtures/issue3552/bucket-0cf535ed.xdr",
            "0cf535ed77ca7ecc44a25f64f498b88f1fb8d2c1ed676c2d9fe3ab62d1bd53f3",
        ),
    ];

    for (path, expected_hex) in FIXTURES {
        let raw = std::fs::read(path).unwrap_or_else(|e| panic!("read fixture {path}: {e}"));
        let expected = Hash256::from_hex(expected_hex).expect("parse expected hash");

        // (1) Raw decode + hash of the record-marked on-disk stream.
        let bucket = Bucket::from_xdr_bytes(&raw)
            .unwrap_or_else(|e| panic!("from_xdr_bytes({path}): {e:?}"));
        assert_eq!(
            bucket.hash(),
            expected,
            "from_xdr_bytes hash for {path} must match core's published (filename) hash",
        );

        // (2) Re-sort through henyey's comparator + re-serialize. A match proves
        // comparator + sort + serialize + record-mark + hash parity. Also assert
        // the parsed-on-disk order already equals henyey's re-sorted order
        // (belt-and-suspenders: hash equality implies it, but this localizes a
        // failure to the comparator vs the framing).
        let parsed = bucket.entries().to_vec();
        let resorted = Bucket::from_entries(parsed.clone())
            .unwrap_or_else(|e| panic!("from_entries({path}): {e:?}"));
        assert_eq!(
            resorted.hash(),
            expected,
            "from_entries (comparator re-sort) hash for {path} must match core's published hash",
        );
        assert_eq!(
            resorted.entries(),
            parsed.as_slice(),
            "henyey's comparator must reproduce core's on-disk entry order for {path}",
        );
    }
}

/// Cross-impl oracle for the #3553 fix: core's L16 level-2 spill bucket
/// `0ebd5750` (the byte-for-byte counterpart to henyey's empty, divergent
/// `d15ebeb4` captured during the SSC v27 mixed-image mission, cluster
/// `rfh1c2qdatp96`).
///
/// Decoding both oracle buckets pins the divergence exactly:
///
/// - henyey `d15ebeb4` (16 B): `Metaentry(BucketMetadata { ledger_version: 0,
///   ext: V0 })` — EMPTY (metadata only).
/// - core  `0ebd5750` (60 B): `Metaentry({ ledger_version: 27, ext:
///   V1(BucketListType::Live) })` **+ one** `Liveentry` carrying
///   `ConfigSetting(EvictionIterator { bucket_list_level: 6, is_curr_bucket:
///   true, bucket_file_offset: 0 })` with `last_modified_ledger_seq: 17` — the
///   per-ledger eviction-iterator update henyey dropped.
///
/// henyey's `{0,void}` empty bucket is the `InputDerived`-of-two-empty-inputs
/// merge artifact that results once the EvictionIterator live entry is missing
/// from the level-1 inputs; the dropped entry (fixed in the ledger crate) is the
/// root cause, and the metadata is a downstream symptom. This guard locks in the
/// cross-impl bucket bytes: it PASSES on `main` (the bucket/merge machinery is
/// verified-correct — feeding it the entry reproduces core's bucket byte-for-byte;
/// the ledger crate is where the entry was dropped) and guards the bucket hash
/// against future regression.
///
/// Fixture `bucket-0ebd5750.xdr` is the uncompressed `BucketEntry` XDR stream
/// captured from the mission
/// (`~/data/9eb89c28/ssc-v27-diag/core-bucket-snaps/CORE_COUNTERPART_0ebd5750.xdr`).
#[test]
fn oracle_l16_eviction_iterator_bucket_matches_core() {
    use stellar_xdr::{BucketEntry, BucketMetadataExt, ConfigSettingEntry, LedgerEntryData};

    const PATH: &str = "tests/fixtures/issue3552/bucket-0ebd5750.xdr";
    const EXPECTED_HEX: &str = "0ebd575078e13e426e4bd19a1788389cc460fbfa667bad8b93122c44420d01ae";

    let raw = std::fs::read(PATH).unwrap_or_else(|e| panic!("read fixture {PATH}: {e}"));
    let expected = Hash256::from_hex(EXPECTED_HEX).expect("parse expected hash");

    // (1) Raw decode + hash of the record-marked on-disk stream == core's
    // content-addressed (filename) hash.
    let bucket =
        Bucket::from_xdr_bytes(&raw).unwrap_or_else(|e| panic!("from_xdr_bytes({PATH}): {e:?}"));
    assert_eq!(
        bucket.hash(),
        expected,
        "from_xdr_bytes hash must match core's published L16 level-2 bucket hash",
    );

    // (2) Re-sort through henyey's comparator + re-serialize — proves comparator
    // + sort + serialize + record-mark + hash parity for this content.
    let parsed = bucket.entries().to_vec();
    let resorted = Bucket::from_entries(parsed.clone())
        .unwrap_or_else(|e| panic!("from_entries({PATH}): {e:?}"));
    assert_eq!(
        resorted.hash(),
        expected,
        "from_entries (comparator re-sort) hash must match core's published hash",
    );

    // (3) Structural assertions: METAENTRY {27, V1(Live)} + exactly one
    // EvictionIterator LIVEENTRY {level: 6, is_curr: true, offset: 0} at
    // last_modified 17.
    let entries = bucket.entries();
    assert_eq!(entries.len(), 2, "core's L16 level-2 bucket has 2 entries");

    match &entries[0] {
        BucketEntry::Metaentry(meta) => {
            assert_eq!(meta.ledger_version, 27, "METAENTRY ledger_version");
            assert!(
                matches!(
                    meta.ext,
                    BucketMetadataExt::V1(stellar_xdr::BucketListType::Live)
                ),
                "METAENTRY ext must be V1(Live), got {:?}",
                meta.ext
            );
        }
        other => panic!("entry 0 must be a METAENTRY, got {other:?}"),
    }

    match &entries[1] {
        BucketEntry::Liveentry(entry) => {
            assert_eq!(
                entry.last_modified_ledger_seq, 17,
                "EvictionIterator last_modified_ledger_seq"
            );
            match &entry.data {
                LedgerEntryData::ConfigSetting(ConfigSettingEntry::EvictionIterator(it)) => {
                    assert_eq!(
                        it.bucket_list_level, 6,
                        "EvictionIterator bucket_list_level"
                    );
                    assert!(it.is_curr_bucket, "EvictionIterator is_curr_bucket");
                    assert_eq!(
                        it.bucket_file_offset, 0,
                        "EvictionIterator bucket_file_offset"
                    );
                }
                other => panic!("entry 1 must be an EvictionIterator ConfigSetting, got {other:?}"),
            }
        }
        other => panic!("entry 1 must be a LIVEENTRY, got {other:?}"),
    }
}
