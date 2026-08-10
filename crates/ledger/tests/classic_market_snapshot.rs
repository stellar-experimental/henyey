use std::collections::HashMap;

use henyey_bucket::{BucketList, HotArchiveBucketList};
use henyey_common::{entry_to_key, Hash256};
use henyey_ledger::{LedgerManager, LedgerManagerConfig};
use stellar_xdr::{
    AccountId, AlphaNum4, Asset, AssetCode4, BucketListType, Hash, LedgerEntry, LedgerEntryData,
    LedgerEntryExt, LedgerHeader, LiquidityPoolConstantProductParameters, LiquidityPoolEntry,
    LiquidityPoolEntryBody, LiquidityPoolEntryConstantProduct, OfferEntry, OfferEntryExt, PoolId,
    Price, PublicKey, SponsorshipDescriptor, Uint256, LIQUIDITY_POOL_FEE_V18,
};

fn account(seed: u8) -> AccountId {
    AccountId(PublicKey::PublicKeyTypeEd25519(Uint256([seed; 32])))
}

fn usd() -> Asset {
    Asset::CreditAlphanum4(AlphaNum4 {
        asset_code: AssetCode4(*b"USD\0"),
        issuer: account(9),
    })
}

fn offer_entry(id: i64) -> LedgerEntry {
    LedgerEntry {
        last_modified_ledger_seq: 5,
        data: LedgerEntryData::Offer(OfferEntry {
            seller_id: account(id as u8),
            offer_id: id,
            selling: usd(),
            buying: Asset::Native,
            amount: 100,
            price: Price { n: 1, d: 1 },
            flags: 0,
            ext: OfferEntryExt::V0,
        }),
        ext: LedgerEntryExt::V0,
    }
}

fn pool_entry() -> LedgerEntry {
    LedgerEntry {
        last_modified_ledger_seq: 5,
        data: LedgerEntryData::LiquidityPool(LiquidityPoolEntry {
            liquidity_pool_id: PoolId(Hash([4; 32])),
            body: LiquidityPoolEntryBody::LiquidityPoolConstantProduct(
                LiquidityPoolEntryConstantProduct {
                    params: LiquidityPoolConstantProductParameters {
                        asset_a: Asset::Native,
                        asset_b: usd(),
                        fee: LIQUIDITY_POOL_FEE_V18 as i32,
                    },
                    reserve_a: 1_000,
                    reserve_b: 2_000,
                    total_pool_shares: 500,
                    pool_shares_trust_line_count: 1,
                },
            ),
        }),
        ext: LedgerEntryExt::V0,
    }
}

#[test]
fn committed_market_snapshot_ignores_live_offer_store_mutation() {
    let mut bucket_list = BucketList::new();
    let mut sponsored_offer = offer_entry(1);
    sponsored_offer.ext = LedgerEntryExt::V1(stellar_xdr::LedgerEntryExtensionV1 {
        sponsoring_id: SponsorshipDescriptor(Some(account(7))),
        ext: stellar_xdr::LedgerEntryExtensionV1Ext::V0,
    });
    bucket_list
        .add_batch(
            5,
            0,
            BucketListType::Live,
            vec![sponsored_offer, offer_entry(2), pool_entry()],
            Vec::new(),
            Vec::new(),
        )
        .unwrap();

    let mut header = LedgerHeader::default();
    header.ledger_seq = 5;
    header.ledger_version = 0;
    header.bucket_list_hash = Hash(bucket_list.hash().0);
    let header_hash = Hash256::from_bytes([8; 32]);
    let manager = LedgerManager::new(
        "Standalone Network ; February 2017".to_string(),
        LedgerManagerConfig::default(),
    );
    manager
        .initialize(
            bucket_list,
            HotArchiveBucketList::new(),
            header,
            header_hash,
        )
        .unwrap();

    let snapshot = manager.create_snapshot().unwrap();
    manager
        .offer_store_lock()
        .insert_from_ledger_entry(&offer_entry(99));

    let market = snapshot.classic_market_snapshot().unwrap();
    assert_eq!(market.ledger_seq(), 5);
    assert_eq!(market.ledger_hash(), header_hash);
    assert_eq!(market.protocol_version(), 0);
    assert_eq!(market.offers().len(), 2);
    assert!(market.offers().iter().all(|offer| offer.offer_id != 99));
    assert_eq!(market.liquidity_pools().len(), 1);

    let live_offer_entries = snapshot.all_entries().unwrap();
    assert_eq!(live_offer_entries.len(), 3);
    assert!(live_offer_entries.iter().any(|entry| {
        matches!(&entry.data, LedgerEntryData::Offer(offer) if offer.offer_id == 99)
    }));

    let frozen_offer_entries = snapshot.frozen_offer_entries().unwrap();
    assert_eq!(frozen_offer_entries.len(), 2);
    assert!(frozen_offer_entries.iter().all(|entry| {
        matches!(&entry.data, LedgerEntryData::Offer(offer) if offer.offer_id != 99)
    }));

    let filtered = snapshot
        .filtered_offer_entries(&[(Asset::Native, usd())])
        .unwrap();
    assert_eq!(filtered.len(), 2);
    assert!(filtered
        .iter()
        .all(|entry| entry.last_modified_ledger_seq == 5));
    assert!(filtered
        .iter()
        .any(|entry| matches!(&entry.ext, LedgerEntryExt::V1(ext) if ext.sponsoring_id.0 == Some(account(7)))));
    assert!(snapshot
        .filtered_offer_entries(&[(usd(), Asset::Native)])
        .unwrap()
        .is_empty());

    let mut replacement_offer = offer_entry(2);
    let LedgerEntryData::Offer(offer) = &mut replacement_offer.data else {
        unreachable!()
    };
    offer.amount = 777;
    let added_offer = offer_entry(3);
    let overlay = snapshot.with_overrides(HashMap::from([
        (entry_to_key(&offer_entry(1)), None),
        (entry_to_key(&replacement_offer), Some(replacement_offer)),
        (entry_to_key(&added_offer), Some(added_offer)),
        (entry_to_key(&pool_entry()), None),
    ]));
    let overlaid_market = overlay.classic_market_snapshot().unwrap();
    assert_eq!(
        overlaid_market
            .offers()
            .iter()
            .map(|offer| (offer.offer_id, offer.amount))
            .collect::<Vec<_>>(),
        vec![(2, 777), (3, 100)]
    );
    assert!(overlaid_market.liquidity_pools().is_empty());

    let mut replacement_pool = pool_entry();
    let LedgerEntryData::LiquidityPool(pool) = &mut replacement_pool.data else {
        unreachable!()
    };
    let LiquidityPoolEntryBody::LiquidityPoolConstantProduct(body) = &mut pool.body;
    body.reserve_a = 9_999;
    let pool_overlay = snapshot.with_overrides(HashMap::from([(
        entry_to_key(&replacement_pool),
        Some(replacement_pool),
    )]));
    let replaced_market = pool_overlay.classic_market_snapshot().unwrap();
    let LiquidityPoolEntryBody::LiquidityPoolConstantProduct(body) =
        &replaced_market.liquidity_pools()[0].body;
    assert_eq!(body.reserve_a, 9_999);
}
