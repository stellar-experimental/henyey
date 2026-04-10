//! Integration tests for pool share trustline queries.
//!
//! These tests match the behavior of the BucketIndexTests.cpp
//! loadPoolShareTrustLinesByAccountAndAsset tests to ensure parity with stellar-core.

use henyey_common::LedgerSeq;
use std::collections::BTreeMap;

use henyey_bucket::{BucketList, BucketListSnapshot, SearchableBucketListSnapshot};
use henyey_common::LIQUIDITY_POOL_FEE_V18;
use stellar_xdr::curr::{
    AccountId, AlphaNum4, Asset, AssetCode4, Hash, LedgerEntry, LedgerEntryData, LedgerEntryExt,
    LedgerHeader, LedgerHeaderExt, LiquidityPoolConstantProductParameters, LiquidityPoolEntry,
    LiquidityPoolEntryBody, LiquidityPoolEntryConstantProduct, PoolId, PublicKey, StellarValue,
    StellarValueExt, TrustLineAsset, TrustLineEntry, TrustLineEntryExt, TrustLineFlags, Uint256,
};

const TEST_PROTOCOL: u32 = 25;

/// Create an AccountId from bytes.
fn make_account_id(bytes: [u8; 32]) -> AccountId {
    AccountId(PublicKey::PublicKeyTypeEd25519(Uint256(bytes)))
}

/// Create an asset from a 4-character code.
fn make_asset(code: &str, issuer: [u8; 32]) -> Asset {
    let mut code_bytes = [0u8; 4];
    let code_bytes_src = code.as_bytes();
    code_bytes[..code_bytes_src.len().min(4)]
        .copy_from_slice(&code_bytes_src[..code_bytes_src.len().min(4)]);
    Asset::CreditAlphanum4(AlphaNum4 {
        asset_code: AssetCode4(code_bytes),
        issuer: make_account_id(issuer),
    })
}

/// Create a liquidity pool entry.
fn make_liquidity_pool(pool_id: PoolId, asset_a: Asset, asset_b: Asset) -> LedgerEntry {
    LedgerEntry {
        last_modified_ledger_seq: 1,
        data: LedgerEntryData::LiquidityPool(LiquidityPoolEntry {
            liquidity_pool_id: pool_id,
            body: LiquidityPoolEntryBody::LiquidityPoolConstantProduct(
                LiquidityPoolEntryConstantProduct {
                    params: LiquidityPoolConstantProductParameters {
                        asset_a,
                        asset_b,
                        fee: LIQUIDITY_POOL_FEE_V18,
                    },
                    reserve_a: 1_000_000,
                    reserve_b: 1_000_000,
                    total_pool_shares: 1_000_000,
                    pool_shares_trust_line_count: 1,
                },
            ),
        }),
        ext: LedgerEntryExt::V0,
    }
}

/// Create a pool share trustline for an account.
fn make_pool_share_trustline(account_id: AccountId, pool_id: PoolId) -> LedgerEntry {
    LedgerEntry {
        last_modified_ledger_seq: 1,
        data: LedgerEntryData::Trustline(TrustLineEntry {
            account_id,
            asset: TrustLineAsset::PoolShare(pool_id),
            balance: 100_000,
            limit: 1_000_000_000,
            flags: TrustLineFlags::AuthorizedFlag as u32,
            ext: TrustLineEntryExt::V0,
        }),
        ext: LedgerEntryExt::V0,
    }
}

/// Create a unique pool ID from a byte.
fn make_pool_id(byte: u8) -> PoolId {
    PoolId([byte; 32].into())
}

/// Create a minimal ledger header for testing.
fn make_ledger_header(ledger_seq: LedgerSeq) -> LedgerHeader {
    LedgerHeader {
        ledger_version: TEST_PROTOCOL,
        previous_ledger_hash: Hash([0u8; 32]),
        scp_value: StellarValue {
            tx_set_hash: Hash([0u8; 32]),
            close_time: stellar_xdr::curr::TimePoint(0),
            upgrades: Vec::new().try_into().unwrap(),
            ext: StellarValueExt::Basic,
        },
        tx_set_result_hash: Hash([0u8; 32]),
        bucket_list_hash: Hash([0u8; 32]),
        ledger_seq: ledger_seq.get(),
        total_coins: 100_000_000_000_000_000,
        fee_pool: 0,
        inflation_seq: 0,
        id_pool: 0,
        base_fee: 100,
        base_reserve: 5_000_000,
        max_tx_set_size: 100,
        skip_list: [
            Hash([0u8; 32]),
            Hash([0u8; 32]),
            Hash([0u8; 32]),
            Hash([0u8; 32]),
        ],
        ext: LedgerHeaderExt::V0,
    }
}

/// Helper to create a snapshot and searchable bucket list.
fn create_searchable_snapshot(bucket_list: &BucketList) -> SearchableBucketListSnapshot {
    let header = make_ledger_header(1.into());
    let snapshot = BucketListSnapshot::new(bucket_list, header);
    SearchableBucketListSnapshot::new(snapshot, BTreeMap::new())
}

/// Test loading pool share trustlines by account and asset.
///
/// This test creates several liquidity pools with different asset combinations
/// and verifies that querying for trustlines with a specific asset returns
/// only the trustlines for pools containing that asset.
#[tokio::test(flavor = "multi_thread")]
async fn test_load_pool_share_trustlines_by_account_and_asset() {
    let mut bucket_list = BucketList::new();

    // Create accounts
    let account_to_search = make_account_id([1u8; 32]);
    let other_account = make_account_id([2u8; 32]);

    // Create assets
    let issuer = [100u8; 32];
    let asset_to_search = make_asset("ast1", issuer);
    let asset2 = make_asset("ast2", issuer);
    let asset3 = make_asset("ast3", issuer);

    // Create pools with different asset combinations
    // Pool 1: asset_to_search + asset2
    let pool1_id = make_pool_id(1);
    let pool1 = make_liquidity_pool(pool1_id.clone(), asset_to_search.clone(), asset2.clone());
    let tl1_search = make_pool_share_trustline(account_to_search.clone(), pool1_id.clone());
    let tl1_other = make_pool_share_trustline(other_account.clone(), pool1_id.clone());

    // Pool 2: asset_to_search + asset3
    let pool2_id = make_pool_id(2);
    let pool2 = make_liquidity_pool(pool2_id.clone(), asset_to_search.clone(), asset3.clone());
    let tl2_search = make_pool_share_trustline(account_to_search.clone(), pool2_id.clone());

    // Pool 3: asset2 + asset3 (does NOT contain asset_to_search)
    let pool3_id = make_pool_id(3);
    let pool3 = make_liquidity_pool(pool3_id.clone(), asset2.clone(), asset3.clone());
    let tl3_search = make_pool_share_trustline(account_to_search.clone(), pool3_id.clone());

    // Add all entries to the bucket list
    bucket_list
        .add_batch(
            1.into(),
            TEST_PROTOCOL,
            stellar_xdr::curr::BucketListType::Live,
            vec![
                pool1.clone(),
                pool2.clone(),
                pool3.clone(),
                tl1_search.clone(),
                tl1_other,
                tl2_search.clone(),
                tl3_search,
            ],
            vec![],
            vec![],
        )
        .unwrap();

    // Create searchable snapshot
    let searchable = create_searchable_snapshot(&bucket_list);

    // Query for pool share trustlines with asset_to_search
    let result = searchable
        .load_pool_share_trustlines_by_account_and_asset(&account_to_search, &asset_to_search)
        .unwrap();

    // Should return trustlines for pool1 and pool2 (which contain asset_to_search)
    // but NOT pool3 (which doesn't contain asset_to_search)
    assert_eq!(result.len(), 2);

    // Verify the returned trustlines are for the correct pools
    let returned_pools: std::collections::HashSet<PoolId> = result
        .iter()
        .filter_map(|e| {
            if let LedgerEntryData::Trustline(tl) = &e.data {
                if let TrustLineAsset::PoolShare(pool_id) = &tl.asset {
                    return Some(pool_id.clone());
                }
            }
            None
        })
        .collect();

    assert!(returned_pools.contains(&pool1_id));
    assert!(returned_pools.contains(&pool2_id));
    assert!(!returned_pools.contains(&pool3_id));
}

/// Test that querying an account with no matching trustlines returns empty.
#[tokio::test(flavor = "multi_thread")]
async fn test_load_pool_share_trustlines_no_match() {
    let mut bucket_list = BucketList::new();

    let account_id = make_account_id([1u8; 32]);
    let other_account = make_account_id([2u8; 32]);

    let issuer = [100u8; 32];
    let asset_to_search = make_asset("ast1", issuer);
    let asset2 = make_asset("ast2", issuer);
    let asset3 = make_asset("ast3", issuer);

    // Create a pool that doesn't contain asset_to_search
    let pool_id = make_pool_id(1);
    let pool = make_liquidity_pool(pool_id.clone(), asset2, asset3);
    let tl = make_pool_share_trustline(other_account, pool_id);

    bucket_list
        .add_batch(
            1.into(),
            TEST_PROTOCOL,
            stellar_xdr::curr::BucketListType::Live,
            vec![pool, tl],
            vec![],
            vec![],
        )
        .unwrap();

    let searchable = create_searchable_snapshot(&bucket_list);

    // Query should return empty since:
    // 1. The pool doesn't contain asset_to_search
    // 2. account_id doesn't have any trustlines anyway
    let result = searchable
        .load_pool_share_trustlines_by_account_and_asset(&account_id, &asset_to_search)
        .unwrap();
    assert!(result.is_empty());
}

/// Test that deleted pools are not included in results.
#[tokio::test(flavor = "multi_thread")]
async fn test_load_pool_share_trustlines_deleted_pool() {
    let mut bucket_list = BucketList::new();

    let account_id = make_account_id([1u8; 32]);
    let issuer = [100u8; 32];
    let asset_to_search = make_asset("ast1", issuer);
    let asset2 = make_asset("ast2", issuer);

    // Create a pool
    let pool_id = make_pool_id(1);
    let pool = make_liquidity_pool(pool_id.clone(), asset_to_search.clone(), asset2);
    let tl = make_pool_share_trustline(account_id.clone(), pool_id.clone());

    // Add pool and trustline
    bucket_list
        .add_batch(
            1.into(),
            TEST_PROTOCOL,
            stellar_xdr::curr::BucketListType::Live,
            vec![pool, tl],
            vec![],
            vec![],
        )
        .unwrap();

    // Verify trustline is found
    let searchable = create_searchable_snapshot(&bucket_list);
    let result = searchable
        .load_pool_share_trustlines_by_account_and_asset(&account_id, &asset_to_search)
        .unwrap();
    assert_eq!(result.len(), 1);

    // Delete the pool
    let pool_key =
        stellar_xdr::curr::LedgerKey::LiquidityPool(stellar_xdr::curr::LedgerKeyLiquidityPool {
            liquidity_pool_id: pool_id,
        });
    bucket_list
        .add_batch(
            2.into(),
            TEST_PROTOCOL,
            stellar_xdr::curr::BucketListType::Live,
            vec![],
            vec![],
            vec![pool_key],
        )
        .unwrap();

    // Now the query should return empty (pool is deleted)
    let searchable = create_searchable_snapshot(&bucket_list);
    let result = searchable
        .load_pool_share_trustlines_by_account_and_asset(&account_id, &asset_to_search)
        .unwrap();
    assert!(result.is_empty());
}

/// Test multi-version entries (updates).
///
/// When an entry is updated, the newer version should be used.
#[tokio::test(flavor = "multi_thread")]
async fn test_load_pool_share_trustlines_multi_version() {
    let mut bucket_list = BucketList::new();

    let account_id = make_account_id([1u8; 32]);
    let issuer = [100u8; 32];
    let asset_to_search = make_asset("ast1", issuer);
    let asset2 = make_asset("ast2", issuer);

    let pool_id = make_pool_id(1);
    let pool = make_liquidity_pool(pool_id.clone(), asset_to_search.clone(), asset2);

    // Create initial trustline with balance 100
    let mut tl = make_pool_share_trustline(account_id.clone(), pool_id.clone());

    bucket_list
        .add_batch(
            1.into(),
            TEST_PROTOCOL,
            stellar_xdr::curr::BucketListType::Live,
            vec![pool, tl.clone()],
            vec![],
            vec![],
        )
        .unwrap();

    // Update trustline with new balance
    if let LedgerEntryData::Trustline(ref mut tl_data) = tl.data {
        tl_data.balance = 500_000; // New balance
    }
    tl.last_modified_ledger_seq = 2;

    bucket_list
        .add_batch(
            2.into(),
            TEST_PROTOCOL,
            stellar_xdr::curr::BucketListType::Live,
            vec![],
            vec![tl.clone()],
            vec![],
        )
        .unwrap();

    let searchable = create_searchable_snapshot(&bucket_list);
    let result = searchable
        .load_pool_share_trustlines_by_account_and_asset(&account_id, &asset_to_search)
        .unwrap();

    assert_eq!(result.len(), 1);

    // Verify the balance is the updated value
    if let LedgerEntryData::Trustline(tl_data) = &result[0].data {
        assert_eq!(tl_data.balance, 500_000);
    } else {
        panic!("Expected trustline entry");
    }
}

/// Test loading all trustlines for an account.
#[tokio::test(flavor = "multi_thread")]
async fn test_load_trustlines_for_account() {
    let mut bucket_list = BucketList::new();

    let account_id = make_account_id([1u8; 32]);
    let other_account = make_account_id([2u8; 32]);
    let issuer = [100u8; 32];

    // Create regular trustlines (non-pool share)
    let tl1 = LedgerEntry {
        last_modified_ledger_seq: 1,
        data: LedgerEntryData::Trustline(TrustLineEntry {
            account_id: account_id.clone(),
            asset: TrustLineAsset::CreditAlphanum4(AlphaNum4 {
                asset_code: AssetCode4(*b"USD\0"),
                issuer: make_account_id(issuer),
            }),
            balance: 100_000,
            limit: 1_000_000_000,
            flags: TrustLineFlags::AuthorizedFlag as u32,
            ext: TrustLineEntryExt::V0,
        }),
        ext: LedgerEntryExt::V0,
    };

    let tl2 = LedgerEntry {
        last_modified_ledger_seq: 1,
        data: LedgerEntryData::Trustline(TrustLineEntry {
            account_id: account_id.clone(),
            asset: TrustLineAsset::CreditAlphanum4(AlphaNum4 {
                asset_code: AssetCode4(*b"EUR\0"),
                issuer: make_account_id(issuer),
            }),
            balance: 200_000,
            limit: 1_000_000_000,
            flags: TrustLineFlags::AuthorizedFlag as u32,
            ext: TrustLineEntryExt::V0,
        }),
        ext: LedgerEntryExt::V0,
    };

    // Trustline for other account
    let tl3 = LedgerEntry {
        last_modified_ledger_seq: 1,
        data: LedgerEntryData::Trustline(TrustLineEntry {
            account_id: other_account.clone(),
            asset: TrustLineAsset::CreditAlphanum4(AlphaNum4 {
                asset_code: AssetCode4(*b"USD\0"),
                issuer: make_account_id(issuer),
            }),
            balance: 300_000,
            limit: 1_000_000_000,
            flags: TrustLineFlags::AuthorizedFlag as u32,
            ext: TrustLineEntryExt::V0,
        }),
        ext: LedgerEntryExt::V0,
    };

    bucket_list
        .add_batch(
            1.into(),
            TEST_PROTOCOL,
            stellar_xdr::curr::BucketListType::Live,
            vec![tl1, tl2, tl3],
            vec![],
            vec![],
        )
        .unwrap();

    let searchable = create_searchable_snapshot(&bucket_list);

    // Load trustlines for account_id
    let result = searchable.load_trustlines_for_account(&account_id).unwrap();
    assert_eq!(result.len(), 2);

    // Load trustlines for other_account
    let result = searchable
        .load_trustlines_for_account(&other_account)
        .unwrap();
    assert_eq!(result.len(), 1);
}

/// Test that pool entries with asset_b matching are also found.
#[tokio::test(flavor = "multi_thread")]
async fn test_load_pool_share_trustlines_asset_in_b_position() {
    let mut bucket_list = BucketList::new();

    let account_id = make_account_id([1u8; 32]);
    let issuer = [100u8; 32];
    let asset_to_search = make_asset("ast1", issuer);
    let asset2 = make_asset("ast2", issuer);

    // Create a pool where asset_to_search is in asset_b position
    let pool_id = make_pool_id(1);
    let pool = make_liquidity_pool(pool_id.clone(), asset2, asset_to_search.clone());
    let tl = make_pool_share_trustline(account_id.clone(), pool_id.clone());

    bucket_list
        .add_batch(
            1.into(),
            TEST_PROTOCOL,
            stellar_xdr::curr::BucketListType::Live,
            vec![pool, tl],
            vec![],
            vec![],
        )
        .unwrap();

    let searchable = create_searchable_snapshot(&bucket_list);
    let result = searchable
        .load_pool_share_trustlines_by_account_and_asset(&account_id, &asset_to_search)
        .unwrap();

    // Should find the trustline even though asset_to_search is in asset_b position
    assert_eq!(result.len(), 1);
}
