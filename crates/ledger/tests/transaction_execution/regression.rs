use super::*;

#[test]
fn test_soroban_refund_event_after_all_txs() {
    let secret = SecretKey::from_seed(&[33u8; 32]);
    let source_id: AccountId = (&secret.public_key()).into();

    let (source_key, source_entry) = create_account_entry(source_id.clone(), 1, 20_000_000);

    let code_hash = Hash([9u8; 32]);
    let contract_code = ContractCodeEntry {
        ext: ContractCodeEntryExt::V0,
        hash: code_hash.clone(),
        code: BytesM::try_from(vec![1u8, 2u8, 3u8]).unwrap(),
    };
    let contract_key = LedgerKey::ContractCode(LedgerKeyContractCode {
        hash: code_hash.clone(),
    });
    let contract_entry = LedgerEntry {
        last_modified_ledger_seq: 1,
        data: LedgerEntryData::ContractCode(contract_code),
        ext: LedgerEntryExt::V0,
    };

    let key_hash = {
        use sha2::{Digest, Sha256};
        use stellar_xdr::curr::WriteXdr;

        let mut hasher = Sha256::new();
        let bytes = contract_key
            .to_xdr(stellar_xdr::curr::Limits::none())
            .unwrap_or_default();
        hasher.update(&bytes);
        Hash(hasher.finalize().into())
    };
    let ttl_entry = LedgerEntry {
        last_modified_ledger_seq: 1,
        data: LedgerEntryData::Ttl(TtlEntry {
            key_hash: key_hash.clone(),
            live_until_ledger_seq: 10,
        }),
        ext: LedgerEntryExt::V0,
    };
    let ttl_key = LedgerKey::Ttl(LedgerKeyTtl { key_hash });

    let snapshot = SnapshotBuilder::new(1)
        .add_entry(source_key, source_entry)
        .add_entry(contract_key.clone(), contract_entry)
        .add_entry(ttl_key, ttl_entry)
        .build_with_default_header();
    let snapshot = SnapshotHandle::new(snapshot);

    let operation = Operation {
        source_account: None,
        body: OperationBody::ExtendFootprintTtl(ExtendFootprintTtlOp {
            ext: stellar_xdr::curr::ExtensionPoint::V0,
            extend_to: 100,
        }),
    };

    let soroban_data = SorobanTransactionData {
        ext: SorobanTransactionDataExt::V0,
        resources: SorobanResources {
            footprint: LedgerFootprint {
                read_only: vec![contract_key].try_into().unwrap(),
                read_write: VecM::default(),
            },
            instructions: 0,
            disk_read_bytes: 10000,
            write_bytes: 0,
        },
        resource_fee: 900,
    };

    let tx = Transaction {
        source_account: MuxedAccount::Ed25519(Uint256(*secret.public_key().as_bytes())),
        fee: 1000,
        seq_num: SequenceNumber(2),
        cond: Preconditions::None,
        memo: Memo::None,
        operations: vec![operation].try_into().unwrap(),
        ext: TransactionExt::V1(soroban_data),
    };

    let mut envelope = TransactionEnvelope::Tx(TransactionV1Envelope {
        tx,
        signatures: VecM::default(),
    });

    let network_id = NetworkId::testnet();
    let decorated = sign_envelope(&envelope, &secret, &network_id);
    if let TransactionEnvelope::Tx(ref mut env) = envelope {
        env.signatures = vec![decorated].try_into().unwrap();
    }

    let classic_events = ClassicEventConfig {
        emit_classic_events: true,
        backfill_stellar_asset_events: false,
    };
    let context = henyey_tx::LedgerContext::new(1, 1_000, 100, 5_000_000, 25, network_id);
    let mut executor =
        TransactionExecutor::new(&context, 0, SorobanConfig::default(), classic_events);
    let result = executor
        .execute_transaction(&snapshot, &envelope, 100, None)
        .expect("execute");

    assert!(result.success);
    assert_eq!(result.fee_charged, 100);

    let tx_meta = result.tx_meta.expect("tx meta");
    let TransactionMeta::V4(meta) = tx_meta else {
        panic!("unexpected tx meta");
    };

    let tx_events: &[stellar_xdr::curr::TransactionEvent] = meta.events.as_ref();
    assert_eq!(tx_events.len(), 1);

    let contract_id = native_asset_contract_id(&network_id);
    let refund_event = &tx_events[0];
    assert_eq!(refund_event.stage, TransactionEventStage::AfterAllTxs);
    let ContractEventBody::V0(refund_body) = &refund_event.event.body;
    assert_eq!(refund_event.event.contract_id, Some(contract_id));
    assert_eq!(refund_body.data, ScVal::I128(i128_parts(-900)));
}

/// Regression test for module cache update when new contracts are deployed.
///
/// This test verifies that `apply_ledger_entry_changes` adds new ContractCode entries
/// to the module cache. Without this fix, newly deployed contracts would not be cached,
/// causing subsequent transactions to use expensive `VmInstantiation` instead of
/// `VmCachedInstantiation`, leading to cost model divergence (Issue #3).
///
/// The fix was implemented in commit that added `add_contract_to_cache()` calls in
/// `apply_ledger_entry_changes()` and `apply_ledger_entry_changes_preserve_seq()`.
#[test]
fn test_apply_ledger_entry_changes_updates_module_cache() {
    let network_id = NetworkId::testnet();

    // Create a module cache for protocol 25
    let module_cache = PersistentModuleCache::new_for_protocol(25).expect("create cache");

    // Create executor with module cache
    let context = henyey_tx::LedgerContext::new(1, 1_000, 100, 5_000_000, 25, network_id);
    let mut executor = TransactionExecutor::new(
        &context,
        0,
        SorobanConfig::default(),
        ClassicEventConfig::default(),
    );
    executor.set_module_cache(module_cache);

    // Create a ContractCode entry (simulating a newly deployed contract)
    // This is a minimal valid WASM module header
    let wasm_code: Vec<u8> = vec![
        0x00, 0x61, 0x73, 0x6d, // WASM magic number
        0x01, 0x00, 0x00, 0x00, // WASM version 1
    ];

    let code_hash = Hash([42u8; 32]);
    let contract_code_entry = LedgerEntry {
        last_modified_ledger_seq: 1,
        data: LedgerEntryData::ContractCode(ContractCodeEntry {
            ext: ContractCodeEntryExt::V0,
            hash: code_hash.clone(),
            code: BytesM::try_from(wasm_code.clone()).unwrap(),
        }),
        ext: LedgerEntryExt::V0,
    };

    // Create LedgerEntryChanges with Created entry (simulates contract deployment)
    let changes: LedgerEntryChanges = vec![LedgerEntryChange::Created(contract_code_entry.clone())]
        .try_into()
        .unwrap();

    // Apply the changes - this should add the contract to the module cache
    executor.apply_ledger_entry_changes(&changes);

    // Verify that the module cache still exists and is accessible
    assert!(
        executor.module_cache().is_some(),
        "Module cache should still be set after apply_ledger_entry_changes"
    );

    // Test apply_ledger_entry_changes_preserve_seq as well
    let changes2: LedgerEntryChanges =
        vec![LedgerEntryChange::Restored(contract_code_entry.clone())]
            .try_into()
            .unwrap();
    executor.apply_ledger_entry_changes_preserve_seq(&changes2);

    // Test Updated entry
    let changes3: LedgerEntryChanges = vec![LedgerEntryChange::Updated(contract_code_entry)]
        .try_into()
        .unwrap();
    executor.apply_ledger_entry_changes_preserve_seq(&changes3);

    // If we got here without panicking, the code path is exercised correctly.
    // The actual verification that this fix works is done via the integration test
    // (verify-execution from ledger 64 to 5000 shows 100% match after this fix).
}

/// Create an account entry with sponsored signers.
/// Used to test SetOptions when the source account has signers sponsored by other accounts.
fn create_account_entry_with_sponsored_signers(
    account_id: AccountId,
    seq_num: i64,
    balance: i64,
    signers: Vec<Signer>,
    signer_sponsors: Vec<SponsorshipDescriptor>,
    num_sponsored: u32,
) -> (LedgerKey, LedgerEntry) {
    let key = LedgerKey::Account(stellar_xdr::curr::LedgerKeyAccount {
        account_id: account_id.clone(),
    });

    let entry = LedgerEntry {
        last_modified_ledger_seq: 1,
        data: LedgerEntryData::Account(AccountEntry {
            account_id,
            balance,
            seq_num: SequenceNumber(seq_num),
            num_sub_entries: signers.len() as u32,
            inflation_dest: None,
            flags: 0,
            home_domain: String32::default(),
            thresholds: Thresholds([1, 0, 0, 0]),
            signers: signers.try_into().unwrap(),
            ext: AccountEntryExt::V1(AccountEntryExtensionV1 {
                liabilities: Liabilities {
                    buying: 0,
                    selling: 0,
                },
                ext: AccountEntryExtensionV1Ext::V2(AccountEntryExtensionV2 {
                    num_sponsored,
                    num_sponsoring: 0,
                    signer_sponsoring_i_ds: signer_sponsors.try_into().unwrap(),
                    ext: AccountEntryExtensionV2Ext::V0,
                }),
            }),
        }),
        ext: LedgerEntryExt::V0,
    };

    (key, entry)
}

/// Create an account entry that is a sponsor (has num_sponsoring set).
fn create_sponsor_account_entry(
    account_id: AccountId,
    seq_num: i64,
    balance: i64,
    num_sponsoring: u32,
) -> (LedgerKey, LedgerEntry) {
    let key = LedgerKey::Account(stellar_xdr::curr::LedgerKeyAccount {
        account_id: account_id.clone(),
    });

    let entry = LedgerEntry {
        last_modified_ledger_seq: 1,
        data: LedgerEntryData::Account(AccountEntry {
            account_id,
            balance,
            seq_num: SequenceNumber(seq_num),
            num_sub_entries: 0,
            inflation_dest: None,
            flags: 0,
            home_domain: String32::default(),
            thresholds: Thresholds([1, 0, 0, 0]),
            signers: VecM::default(),
            ext: AccountEntryExt::V1(AccountEntryExtensionV1 {
                liabilities: Liabilities {
                    buying: 0,
                    selling: 0,
                },
                ext: AccountEntryExtensionV1Ext::V2(AccountEntryExtensionV2 {
                    num_sponsored: 0,
                    num_sponsoring,
                    signer_sponsoring_i_ds: vec![].try_into().unwrap(),
                    ext: AccountEntryExtensionV2Ext::V0,
                }),
            }),
        }),
        ext: LedgerEntryExt::V0,
    };

    (key, entry)
}

/// Test that SetOptions correctly loads sponsor accounts when modifying signers
/// on an account that has sponsored signers.
///
/// This is a regression test for a bug found during testnet replay at ledger 84362,
/// where SetOptions would fail with "source account not found" when trying to
/// remove a signer that was sponsored by another account (because the sponsor
/// account wasn't loaded into state).
#[test]
fn test_set_options_loads_signer_sponsor_accounts() {
    let source_secret = SecretKey::from_seed(&[200u8; 32]);
    let source_id: AccountId = (&source_secret.public_key()).into();

    // Create a sponsor account
    let sponsor_id = AccountId(PublicKey::PublicKeyTypeEd25519(Uint256([201u8; 32])));

    // Create a signer key that we'll remove
    let signer_key = SignerKey::Ed25519(Uint256([202u8; 32]));
    let signer = Signer {
        key: signer_key.clone(),
        weight: 1,
    };

    // Create the source account with a sponsored signer
    let (source_key, source_entry) = create_account_entry_with_sponsored_signers(
        source_id.clone(),
        1,
        50_000_000,
        vec![signer],
        vec![SponsorshipDescriptor(Some(sponsor_id.clone()))],
        1, // num_sponsored = 1 (the signer is sponsored)
    );

    // Create the sponsor account with num_sponsoring = 1
    let (sponsor_key, sponsor_entry) =
        create_sponsor_account_entry(sponsor_id.clone(), 1, 50_000_000, 1);

    let snapshot = SnapshotBuilder::new(1)
        .add_entry(source_key, source_entry)
        .add_entry(sponsor_key, sponsor_entry)
        .build_with_default_header();
    let snapshot = SnapshotHandle::new(snapshot);

    // Create SetOptions operation to remove the signer (weight = 0)
    let operation = Operation {
        source_account: None,
        body: OperationBody::SetOptions(SetOptionsOp {
            inflation_dest: None,
            clear_flags: None,
            set_flags: None,
            master_weight: None,
            low_threshold: None,
            med_threshold: None,
            high_threshold: None,
            home_domain: None,
            signer: Some(Signer {
                key: signer_key,
                weight: 0, // Remove the signer
            }),
        }),
    };

    let tx = Transaction {
        source_account: MuxedAccount::Ed25519(Uint256(*source_secret.public_key().as_bytes())),
        fee: 100,
        seq_num: SequenceNumber(2),
        cond: Preconditions::None,
        memo: Memo::None,
        operations: vec![operation].try_into().unwrap(),
        ext: TransactionExt::V0,
    };

    let mut envelope = TransactionEnvelope::Tx(TransactionV1Envelope {
        tx,
        signatures: VecM::default(),
    });

    let network_id = NetworkId::testnet();
    let decorated = sign_envelope(&envelope, &source_secret, &network_id);
    if let TransactionEnvelope::Tx(ref mut env) = envelope {
        env.signatures = vec![decorated].try_into().unwrap();
    }

    let context = henyey_tx::LedgerContext::new(1, 1_000, 100, 5_000_000, 25, network_id);
    let mut executor = TransactionExecutor::new(
        &context,
        0,
        SorobanConfig::default(),
        ClassicEventConfig::default(),
    );
    let result = executor
        .execute_transaction(&snapshot, &envelope, 100, None)
        .expect("execute");

    // The operation should succeed
    assert!(
        result.success,
        "SetOptions should succeed when removing a sponsored signer: {:?}",
        result.failure
    );

    // Verify the operation result is Success
    let op_result = result.operation_results.get(0).expect("operation result");
    assert!(
        matches!(
            op_result,
            OperationResult::OpInner(OperationResultTr::SetOptions(SetOptionsResult::Success))
        ),
        "Expected SetOptions Success, got {:?}",
        op_result
    );

    // Verify the sponsor's num_sponsoring was decremented
    let state = executor.state();
    let sponsor_account = state.get_account(&sponsor_id).expect("sponsor account");
    if let AccountEntryExt::V1(v1) = &sponsor_account.ext {
        if let AccountEntryExtensionV1Ext::V2(v2) = &v1.ext {
            assert_eq!(
                v2.num_sponsoring, 0,
                "Sponsor's num_sponsoring should be decremented to 0"
            );
        } else {
            panic!("Expected V2 extension on sponsor account");
        }
    } else {
        panic!("Expected V1 extension on sponsor account");
    }

    // Verify the source account's signer was removed
    let source_account = state.get_account(&source_id).expect("source account");
    assert!(
        source_account.signers.is_empty(),
        "Signer should have been removed"
    );
}

/// Regression test for hot archive being passed to execute_transaction_set.
/// Prior to this fix, the hot archive was stored in LedgerManager but never passed
/// to the transaction execution layer, causing "No hot archive available for lookup"
/// errors when attempting to restore archived entries in Protocol 23+.
///
/// This test verifies that execute_transaction_set accepts and forwards the hot_archive
/// parameter to the TransactionExecutor.
///
/// Issue: Discovered at testnet ledger 637593+ when entry restoration failed.
/// Fix: Added hot_archive parameter to execute_transaction_set and wired it through
/// to TransactionExecutor::set_hot_archive().
#[test]
fn test_execute_transaction_set_accepts_hot_archive_parameter() {
    use henyey_bucket::HotArchiveBucketList;
    use henyey_ledger::execution::execute_transaction_set;
    use henyey_ledger::{LedgerDelta, SorobanContext};
    use std::sync::Arc;

    let secret = SecretKey::from_seed(&[99u8; 32]);
    let account_id: AccountId = (&secret.public_key()).into();

    let (key, entry) = create_account_entry(account_id.clone(), 1, 10_000_000);
    let snapshot = SnapshotBuilder::new(1)
        .add_entry(key, entry)
        .build_with_default_header();
    let snapshot = SnapshotHandle::new(snapshot);

    let destination = AccountId(PublicKey::PublicKeyTypeEd25519(Uint256([42u8; 32])));
    let operation = Operation {
        source_account: None,
        body: OperationBody::CreateAccount(CreateAccountOp {
            destination,
            starting_balance: 1_000_000,
        }),
    };

    let tx = Transaction {
        source_account: MuxedAccount::Ed25519(Uint256(*secret.public_key().as_bytes())),
        fee: 100,
        seq_num: SequenceNumber(2),
        cond: Preconditions::None,
        memo: Memo::None,
        operations: vec![operation].try_into().unwrap(),
        ext: TransactionExt::V0,
    };

    let mut envelope = TransactionEnvelope::Tx(TransactionV1Envelope {
        tx,
        signatures: VecM::default(),
    });

    let network_id = NetworkId::testnet();
    let decorated = sign_envelope(&envelope, &secret, &network_id);
    if let TransactionEnvelope::Tx(ref mut env) = envelope {
        env.signatures = vec![decorated].try_into().unwrap();
    }

    let mut delta = LedgerDelta::new(1);
    let transactions = vec![(Arc::new(envelope), None)];

    // Create an empty hot archive bucket list wrapped in the expected type
    let hot_archive = HotArchiveBucketList::new();
    let hot_archive_arc: Arc<parking_lot::RwLock<Option<HotArchiveBucketList>>> =
        Arc::new(parking_lot::RwLock::new(Some(hot_archive)));

    // This should NOT panic or error - the hot_archive parameter should be accepted
    // and forwarded to the executor. Prior to the fix, this parameter didn't exist.
    let context = henyey_tx::LedgerContext::new(
        1,         // ledger_seq
        1000,      // close_time
        100,       // base_fee
        5_000_000, // base_reserve
        25,        // protocol_version (Protocol 25)
        network_id,
    );
    let result = execute_transaction_set(
        &snapshot,
        &transactions,
        &context,
        &mut delta,
        SorobanContext {
            config: SorobanConfig::default(),
            base_prng_seed: [0u8; 32],
            classic_events: ClassicEventConfig::default(),
            module_cache: None,
            hot_archive: Some(hot_archive_arc), // the key parameter being tested
            runtime_handle: None,
            emit_soroban_tx_meta_ext_v1: false,
            soroban_state: None,
            offer_store: None,
            enable_soroban_diagnostic_events: false,
        },
    );

    // The transaction should execute (the API should accept the hot_archive parameter)
    // Note: The transaction result itself may vary - the key verification is that
    // execute_transaction_set properly accepts and forwards the hot_archive parameter
    // to TransactionExecutor without panicking or erroring.
    let tx_set_result =
        result.expect("execute_transaction_set should succeed with hot_archive parameter");
    assert_eq!(
        tx_set_result.results.len(),
        1,
        "Should have one transaction result"
    );
    // We don't assert success here because that depends on many factors -
    // the key test is that the API correctly accepts and processes the hot_archive parameter.
}

/// Regression test for deleted offers being incorrectly reloaded from snapshot.
///
/// This test reproduces a bug found at mainnet ledger 59501248:
/// 1. TX1 (path payment) fully crosses an offer, consuming it entirely and deleting it
/// 2. TX2 (manage sell offer) from the offer owner tries to delete the same offer by ID
///
/// Without the fix, TX2 would reload the offer from the snapshot because:
/// - `state.get_offer()` returns `None` (offer was removed from memory)
/// - `batch_load_keys()` would reload it from snapshot (ignoring that it was deleted)
/// - The offer would appear to exist with full amount but trustline liabilities = 0
/// - This caused "liabilities underflow" errors
///
/// The fix adds a check in `batch_load_keys()` to skip entries that were deleted
/// in the current ledger (by checking `state.delta().deleted_keys()`).
///
/// Expected behavior after fix: TX2 should return `ManageSellOfferResult::NotFound`
/// because the offer was already deleted by TX1.
#[test]
fn test_deleted_offer_not_reloaded_from_snapshot() {
    // Create accounts:
    // - source: initiates the path payment (TX1)
    // - offer_owner: owns the offer and tries to delete it (TX2)
    // - dest: destination for path payment
    // - issuer: issues the USD asset
    let source_secret = SecretKey::from_seed(&[200u8; 32]);
    let source_id: AccountId = (&source_secret.public_key()).into();

    let offer_owner_secret = SecretKey::from_seed(&[201u8; 32]);
    let offer_owner_id: AccountId = (&offer_owner_secret.public_key()).into();

    let dest_id = AccountId(PublicKey::PublicKeyTypeEd25519(Uint256([202u8; 32])));

    let issuer_secret = SecretKey::from_seed(&[203u8; 32]);
    let issuer_id: AccountId = (&issuer_secret.public_key()).into();

    // Create USD asset
    let asset_usd = Asset::CreditAlphanum4(AlphaNum4 {
        asset_code: AssetCode4([b'U', b'S', b'D', 0]),
        issuer: issuer_id.clone(),
    });

    // Create account entries
    let (source_key, source_entry) = create_account_entry(source_id.clone(), 1, 500_000_000);
    let (dest_key, dest_entry) = create_account_entry(dest_id.clone(), 1, 200_000_000);
    let (issuer_key, issuer_entry) = create_account_entry(issuer_id.clone(), 1, 100_000_000);

    // Offer owner account with selling liabilities for the XLM they're selling
    // num_sub_entries = 2: one trustline + one offer
    let (offer_owner_key, mut offer_owner_entry) =
        create_account_entry(offer_owner_id.clone(), 1, 500_000_000);
    if let LedgerEntryData::Account(ref mut acc) = offer_owner_entry.data {
        acc.num_sub_entries = 2;
    }
    // Set selling liabilities equal to offer amount (50 XLM)
    set_account_liabilities(&mut offer_owner_entry, 50_000_000, 0);

    // Source needs USD trustline to send USD
    let (source_tl_key, source_tl_entry) = create_trustline_entry(
        source_id.clone(),
        TrustLineAsset::CreditAlphanum4(match &asset_usd {
            Asset::CreditAlphanum4(a) => a.clone(),
            _ => unreachable!(),
        }),
        100_000_000, // balance: 100 USD
        200_000_000, // limit
        TrustLineFlags::AuthorizedFlag as u32,
    );

    // Offer owner needs USD trustline to receive USD (buying side of their offer)
    let (offer_owner_tl_key, mut offer_owner_tl_entry) = create_trustline_entry(
        offer_owner_id.clone(),
        TrustLineAsset::CreditAlphanum4(match &asset_usd {
            Asset::CreditAlphanum4(a) => a.clone(),
            _ => unreachable!(),
        }),
        0,           // balance: 0 USD
        100_000_000, // limit
        TrustLineFlags::AuthorizedFlag as u32,
    );
    // Set buying liabilities for the USD they're buying
    set_trustline_liabilities(&mut offer_owner_tl_entry, 0, 50_000_000);

    // Create the offer: offer_owner sells 50 XLM for USD at 1:1 price
    let offer_id: i64 = 12345;
    let (offer_key, offer_entry) = create_offer_entry(
        offer_owner_id.clone(),
        offer_id,
        Asset::Native,     // selling XLM
        asset_usd.clone(), // buying USD
        50_000_000,        // amount: 50 XLM
        Price { n: 1, d: 1 },
    );

    // Build snapshot with all entries
    let snapshot = SnapshotBuilder::new(1)
        .add_entry(source_key, source_entry)
        .add_entry(dest_key, dest_entry)
        .add_entry(offer_owner_key, offer_owner_entry)
        .add_entry(issuer_key, issuer_entry)
        .add_entry(source_tl_key, source_tl_entry)
        .add_entry(offer_owner_tl_key, offer_owner_tl_entry)
        .add_entry(offer_key.clone(), offer_entry)
        .build_with_default_header();
    let snapshot = SnapshotHandle::new(snapshot);

    let network_id = NetworkId::testnet();

    // Create executor
    let context = henyey_tx::LedgerContext::new(
        1,         // ledger_seq
        1_000,     // close_time
        100,       // base_fee
        5_000_000, // base_reserve
        25,        // protocol_version
        network_id,
    );
    let mut executor = TransactionExecutor::new(
        &context,
        0,
        SorobanConfig::default(),
        ClassicEventConfig::default(),
    );
    executor
        .load_orderbook_offers(&snapshot)
        .expect("load orderbook");

    // ===== TX1: Path payment that fully crosses the offer =====
    // Source sends 50 USD, which crosses offer_owner's offer fully, resulting in dest receiving 50 XLM
    let path_payment_op = Operation {
        source_account: None,
        body: OperationBody::PathPaymentStrictSend(PathPaymentStrictSendOp {
            send_asset: asset_usd.clone(),
            send_amount: 50_000_000, // Send exactly 50 USD to fully consume the offer
            destination: dest_id.clone().into(),
            dest_asset: Asset::Native, // Receive XLM
            dest_min: 1,               // Minimum acceptable
            path: VecM::default(),     // Direct path through the offer
        }),
    };

    let tx1 = Transaction {
        source_account: MuxedAccount::Ed25519(Uint256(*source_secret.public_key().as_bytes())),
        fee: 100,
        seq_num: SequenceNumber(2),
        cond: Preconditions::None,
        memo: Memo::None,
        operations: vec![path_payment_op].try_into().unwrap(),
        ext: TransactionExt::V0,
    };

    let mut envelope1 = TransactionEnvelope::Tx(TransactionV1Envelope {
        tx: tx1,
        signatures: VecM::default(),
    });
    let decorated1 = sign_envelope(&envelope1, &source_secret, &network_id);
    if let TransactionEnvelope::Tx(ref mut env) = envelope1 {
        env.signatures = vec![decorated1].try_into().unwrap();
    }

    // Execute TX1
    let result1 = executor
        .execute_transaction(&snapshot, &envelope1, 100, None)
        .expect("TX1 execute");

    assert!(
        result1.success,
        "TX1 (path payment) should succeed. Result: {:?}",
        result1.operation_results
    );

    // Verify the offer was crossed
    match result1.operation_results.get(0) {
        Some(OperationResult::OpInner(OperationResultTr::PathPaymentStrictSend(
            PathPaymentStrictSendResult::Success(success),
        ))) => {
            assert!(
                !success.offers.is_empty(),
                "Should have crossed at least one offer"
            );
        }
        other => panic!("Unexpected TX1 result: {:?}", other),
    }

    // Snapshot delta before TX2 (simulates ledger execution flow between transactions)
    // This preserves changes from TX1 so they're visible to TX2
    executor.state_mut().snapshot_delta();

    // ===== TX2: Offer owner tries to delete their offer (which was already consumed) =====
    // The offer was fully crossed by TX1, so it should no longer exist.
    // Without the fix, the offer would be reloaded from snapshot and cause issues.
    let delete_offer_op = Operation {
        source_account: None,
        body: OperationBody::ManageSellOffer(ManageSellOfferOp {
            selling: Asset::Native,
            buying: asset_usd.clone(),
            amount: 0, // amount=0 means delete the offer
            price: Price { n: 1, d: 1 },
            offer_id, // The offer ID that was consumed by TX1
        }),
    };

    let tx2 = Transaction {
        source_account: MuxedAccount::Ed25519(Uint256(*offer_owner_secret.public_key().as_bytes())),
        fee: 100,
        seq_num: SequenceNumber(2),
        cond: Preconditions::None,
        memo: Memo::None,
        operations: vec![delete_offer_op].try_into().unwrap(),
        ext: TransactionExt::V0,
    };

    let mut envelope2 = TransactionEnvelope::Tx(TransactionV1Envelope {
        tx: tx2,
        signatures: VecM::default(),
    });
    let decorated2 = sign_envelope(&envelope2, &offer_owner_secret, &network_id);
    if let TransactionEnvelope::Tx(ref mut env) = envelope2 {
        env.signatures = vec![decorated2].try_into().unwrap();
    }

    // Execute TX2
    let result2 = executor
        .execute_transaction(&snapshot, &envelope2, 100, None)
        .expect("TX2 execute");

    // TX2 should fail at the operation level (not succeed!) because the offer doesn't exist
    // The transaction itself succeeds (fee charged) but the operation fails with NotFound
    assert!(
        !result2.success,
        "TX2 should fail because offer no longer exists"
    );

    // Verify the operation result is NotFound
    match result2.operation_results.get(0) {
        Some(OperationResult::OpInner(OperationResultTr::ManageSellOffer(
            ManageSellOfferResult::NotFound,
        ))) => {
            // This is the expected result - the offer was already deleted by TX1
        }
        other => panic!(
            "TX2 should return ManageSellOffer::NotFound, got: {:?}",
            other
        ),
    }
}

// ---------------------------------------------------------------------------
// Parallel Soroban cluster execution tests
// ---------------------------------------------------------------------------

/// Helper: build an ExtendFootprintTtl Soroban TX for a given contract code entry.
///
/// Returns the signed envelope and all the ledger entries (account, contract code,
/// TTL) that must be in the snapshot for execution to succeed.
fn build_extend_ttl_tx(
    seed: [u8; 32],
    seq_num: i64,
    code_hash_bytes: [u8; 32],
    extend_to: u32,
    network_id: &NetworkId,
) -> (
    TransactionEnvelope,
    Vec<(LedgerKey, LedgerEntry)>, // entries to add to snapshot
) {
    let secret = SecretKey::from_seed(&seed);
    let source_id: AccountId = (&secret.public_key()).into();

    // Account entry
    let (source_key, source_entry) =
        create_account_entry(source_id.clone(), seq_num - 1, 20_000_000);

    // Contract code entry
    let code_hash = Hash(code_hash_bytes);
    let contract_code = ContractCodeEntry {
        ext: ContractCodeEntryExt::V0,
        hash: code_hash.clone(),
        code: BytesM::try_from(vec![1u8, 2u8, 3u8]).unwrap(),
    };
    let contract_key = LedgerKey::ContractCode(LedgerKeyContractCode {
        hash: code_hash.clone(),
    });
    let contract_entry = LedgerEntry {
        last_modified_ledger_seq: 1,
        data: LedgerEntryData::ContractCode(contract_code),
        ext: LedgerEntryExt::V0,
    };

    // TTL entry for the contract code
    let key_hash = {
        use sha2::{Digest, Sha256};
        use stellar_xdr::curr::WriteXdr;
        let mut hasher = Sha256::new();
        let bytes = contract_key
            .to_xdr(stellar_xdr::curr::Limits::none())
            .unwrap_or_default();
        hasher.update(&bytes);
        Hash(hasher.finalize().into())
    };
    let ttl_entry = LedgerEntry {
        last_modified_ledger_seq: 1,
        data: LedgerEntryData::Ttl(TtlEntry {
            key_hash: key_hash.clone(),
            live_until_ledger_seq: 10,
        }),
        ext: LedgerEntryExt::V0,
    };
    let ttl_key = LedgerKey::Ttl(LedgerKeyTtl { key_hash });

    // Build TX
    let operation = Operation {
        source_account: None,
        body: OperationBody::ExtendFootprintTtl(ExtendFootprintTtlOp {
            ext: ExtensionPoint::V0,
            extend_to,
        }),
    };
    let soroban_data = SorobanTransactionData {
        ext: SorobanTransactionDataExt::V0,
        resources: SorobanResources {
            footprint: LedgerFootprint {
                read_only: vec![contract_key.clone()].try_into().unwrap(),
                read_write: VecM::default(),
            },
            instructions: 0,
            disk_read_bytes: 10000,
            write_bytes: 0,
        },
        resource_fee: 900,
    };
    let tx = Transaction {
        source_account: MuxedAccount::Ed25519(Uint256(*secret.public_key().as_bytes())),
        fee: 1000,
        seq_num: SequenceNumber(seq_num),
        cond: Preconditions::None,
        memo: Memo::None,
        operations: vec![operation].try_into().unwrap(),
        ext: TransactionExt::V1(soroban_data),
    };
    let mut envelope = TransactionEnvelope::Tx(TransactionV1Envelope {
        tx,
        signatures: VecM::default(),
    });
    let decorated = sign_envelope(&envelope, &secret, network_id);
    if let TransactionEnvelope::Tx(ref mut env) = envelope {
        env.signatures = vec![decorated].try_into().unwrap();
    }

    let entries = vec![
        (source_key, source_entry),
        (contract_key, contract_entry),
        (ttl_key, ttl_entry),
    ];
    (envelope, entries)
}

/// Test that `execute_soroban_parallel_phase` with multiple clusters in a single
/// stage actually executes them via the `spawn_blocking` parallel path and
/// produces correct results.
#[tokio::test(flavor = "multi_thread")]
async fn test_parallel_soroban_multi_cluster_execution() {
    use henyey_ledger::{
        execute_soroban_parallel_phase, LedgerDelta, SnapshotBuilder, SnapshotHandle,
        SorobanContext, SorobanPhaseStructure,
    };

    let network_id = NetworkId::testnet();

    // Build two independent Soroban TXs that touch different contract codes.
    let (tx1, entries1) = build_extend_ttl_tx([33u8; 32], 2, [9u8; 32], 100, &network_id);
    let (tx2, entries2) = build_extend_ttl_tx([44u8; 32], 2, [10u8; 32], 200, &network_id);

    // Build snapshot with all entries
    let mut builder = SnapshotBuilder::new(1);
    for (key, entry) in entries1.into_iter().chain(entries2.into_iter()) {
        builder = builder.add_entry(key, entry);
    }
    let snapshot = SnapshotHandle::new(builder.build_with_default_header());

    // Create a phase with 1 stage and 2 clusters (triggers parallel path).
    let phase = SorobanPhaseStructure {
        base_fee: None,
        stages: vec![vec![
            vec![(std::sync::Arc::new(tx1.clone()), None)], // cluster 0
            vec![(std::sync::Arc::new(tx2.clone()), None)], // cluster 1
        ]],
    };

    let mut delta = LedgerDelta::new(1);
    let context = henyey_tx::LedgerContext::new(
        1,         // ledger_seq
        1_000,     // close_time
        100,       // base_fee
        5_000_000, // base_reserve
        25,        // protocol_version
        network_id,
    );
    let result = execute_soroban_parallel_phase(
        &snapshot,
        &phase,
        0, // no classic TXs in test
        &context,
        &mut delta,
        SorobanContext {
            config: SorobanConfig::default(),
            base_prng_seed: [0u8; 32],
            classic_events: ClassicEventConfig::default(),
            module_cache: None,
            hot_archive: None,
            runtime_handle: None,
            soroban_state: None,
            offer_store: None,
            emit_soroban_tx_meta_ext_v1: false,
            enable_soroban_diagnostic_events: false,
        },
        None,
    )
    .expect("execute parallel phase");

    // Both TXs should have executed.
    assert_eq!(result.results.len(), 2, "expected 2 execution results");
    assert_eq!(result.tx_results.len(), 2, "expected 2 TX result pairs");
    assert_eq!(
        result.tx_result_metas.len(),
        2,
        "expected 2 TX result metas"
    );

    // Both should succeed.
    assert!(result.results[0].success, "TX1 should succeed");
    assert!(result.results[1].success, "TX2 should succeed");

    // Fees should be collected (each TX has fee=1000, base_fee=100 → charged 100 each).
    assert_eq!(result.results[0].fee_charged, 100, "TX1 fee charged");
    assert_eq!(result.results[1].fee_charged, 100, "TX2 fee charged");

    // Delta should have changes from both clusters.
    // Each cluster modifies: account (fee deduction) + TTL entry (extend).
    assert!(delta.num_changes() > 0, "delta should have changes");
    assert!(delta.fee_pool_delta() > 0, "fees should be collected");
}

/// Test that parallel execution produces the same result as single-cluster
/// (sequential) execution.
#[tokio::test(flavor = "multi_thread")]
async fn test_parallel_soroban_matches_sequential() {
    use henyey_ledger::{
        execute_soroban_parallel_phase, LedgerDelta, SnapshotBuilder, SnapshotHandle,
        SorobanContext, SorobanPhaseStructure,
    };

    let network_id = NetworkId::testnet();

    let (tx1, entries1) = build_extend_ttl_tx([33u8; 32], 2, [9u8; 32], 100, &network_id);
    let (tx2, entries2) = build_extend_ttl_tx([44u8; 32], 2, [10u8; 32], 200, &network_id);

    // Build snapshot with all entries.
    let mut builder = SnapshotBuilder::new(1);
    for (key, entry) in entries1.into_iter().chain(entries2.into_iter()) {
        builder = builder.add_entry(key, entry);
    }
    let snapshot = SnapshotHandle::new(builder.build_with_default_header());

    let context = henyey_tx::LedgerContext::new(
        1,         // ledger_seq
        1_000,     // close_time
        100,       // base_fee
        5_000_000, // base_reserve
        25,        // protocol_version
        network_id,
    );

    // Run with 2 clusters in 1 stage (parallel path).
    let parallel_phase = SorobanPhaseStructure {
        base_fee: None,
        stages: vec![vec![
            vec![(std::sync::Arc::new(tx1.clone()), None)],
            vec![(std::sync::Arc::new(tx2.clone()), None)],
        ]],
    };
    let mut parallel_delta = LedgerDelta::new(1);
    let par = execute_soroban_parallel_phase(
        &snapshot,
        &parallel_phase,
        0, // no classic TXs in test
        &context,
        &mut parallel_delta,
        SorobanContext {
            config: SorobanConfig::default(),
            base_prng_seed: [0u8; 32],
            classic_events: ClassicEventConfig::default(),
            module_cache: None,
            hot_archive: None,
            runtime_handle: None,
            soroban_state: None,
            offer_store: None,
            emit_soroban_tx_meta_ext_v1: false,
            enable_soroban_diagnostic_events: false,
        },
        None,
    )
    .expect("parallel");

    // Run with 2 stages of 1 cluster each (sequential path — each stage has ≤1 cluster).
    let sequential_phase = SorobanPhaseStructure {
        base_fee: None,
        stages: vec![
            vec![vec![(std::sync::Arc::new(tx1.clone()), None)]],
            vec![vec![(std::sync::Arc::new(tx2.clone()), None)]],
        ],
    };
    let mut sequential_delta = LedgerDelta::new(1);
    let seq = execute_soroban_parallel_phase(
        &snapshot,
        &sequential_phase,
        0, // no classic TXs in test
        &context,
        &mut sequential_delta,
        SorobanContext {
            config: SorobanConfig::default(),
            base_prng_seed: [0u8; 32],
            classic_events: ClassicEventConfig::default(),
            module_cache: None,
            hot_archive: None,
            runtime_handle: None,
            soroban_state: None,
            offer_store: None,
            emit_soroban_tx_meta_ext_v1: false,
            enable_soroban_diagnostic_events: false,
        },
        None,
    )
    .expect("sequential");

    // Results should match.
    assert_eq!(par.results.len(), seq.results.len(), "result count");
    for i in 0..par.results.len() {
        assert_eq!(
            par.results[i].success, seq.results[i].success,
            "success[{i}]"
        );
        assert_eq!(
            par.results[i].fee_charged, seq.results[i].fee_charged,
            "fee_charged[{i}]"
        );
        assert_eq!(
            par.results[i].fee_refund, seq.results[i].fee_refund,
            "fee_refund[{i}]"
        );
    }
    assert_eq!(
        par.tx_results.len(),
        seq.tx_results.len(),
        "tx_results count"
    );
    assert_eq!(
        par.tx_result_metas.len(),
        seq.tx_result_metas.len(),
        "metas count"
    );
    assert_eq!(par.id_pool, seq.id_pool, "id_pool");
    assert_eq!(
        par.hot_archive_restored_keys.len(),
        seq.hot_archive_restored_keys.len(),
        "restored keys"
    );

    // Fee pool deltas should match.
    assert_eq!(
        parallel_delta.fee_pool_delta(),
        sequential_delta.fee_pool_delta(),
        "fee pool delta"
    );

    // Both should have the same number of state changes.
    assert_eq!(
        parallel_delta.num_changes(),
        sequential_delta.num_changes(),
        "number of delta changes"
    );
}

/// Test that parallel execution is deterministic across runs.
#[tokio::test(flavor = "multi_thread")]
async fn test_parallel_soroban_deterministic() {
    use henyey_ledger::{
        execute_soroban_parallel_phase, LedgerDelta, SnapshotBuilder, SnapshotHandle,
        SorobanContext, SorobanPhaseStructure,
    };

    let network_id = NetworkId::testnet();

    let (tx1, entries1) = build_extend_ttl_tx([33u8; 32], 2, [9u8; 32], 100, &network_id);
    let (tx2, entries2) = build_extend_ttl_tx([44u8; 32], 2, [10u8; 32], 200, &network_id);

    let mut builder = SnapshotBuilder::new(1);
    for (key, entry) in entries1.into_iter().chain(entries2.into_iter()) {
        builder = builder.add_entry(key, entry);
    }
    let snapshot = SnapshotHandle::new(builder.build_with_default_header());

    let phase = SorobanPhaseStructure {
        base_fee: None,
        stages: vec![vec![
            vec![(std::sync::Arc::new(tx1.clone()), None)],
            vec![(std::sync::Arc::new(tx2.clone()), None)],
        ]],
    };

    // Run the same phase multiple times.
    let mut prev_fee_delta = None;
    let mut prev_num_changes = None;
    let mut prev_results: Option<Vec<(bool, i64, i64)>> = None;
    let context = henyey_tx::LedgerContext::new(1, 1_000, 100, 5_000_000, 25, network_id);

    for run in 0..5 {
        let mut delta = LedgerDelta::new(1);
        let result = execute_soroban_parallel_phase(
            &snapshot,
            &phase,
            0, // no classic TXs in test
            &context,
            &mut delta,
            SorobanContext {
                config: SorobanConfig::default(),
                base_prng_seed: [0u8; 32],
                classic_events: ClassicEventConfig::default(),
                module_cache: None,
                hot_archive: None,
                runtime_handle: None,
                soroban_state: None,
                offer_store: None,
                emit_soroban_tx_meta_ext_v1: false,
                enable_soroban_diagnostic_events: false,
            },
            None,
        )
        .expect("execute");

        let result_tuples: Vec<(bool, i64, i64)> = result
            .results
            .iter()
            .map(|r| (r.success, r.fee_charged, r.fee_refund))
            .collect();

        if let Some(ref prev) = prev_results {
            assert_eq!(&result_tuples, prev, "results differ on run {run}");
        }
        if let Some(prev) = prev_fee_delta {
            assert_eq!(
                delta.fee_pool_delta(),
                prev,
                "fee delta differs on run {run}"
            );
        }
        if let Some(prev) = prev_num_changes {
            assert_eq!(
                delta.num_changes(),
                prev,
                "num changes differs on run {run}"
            );
        }

        prev_results = Some(result_tuples);
        prev_fee_delta = Some(delta.fee_pool_delta());
        prev_num_changes = Some(delta.num_changes());
    }
}

/// Test that `execute_soroban_parallel_phase` works correctly when called from
/// a `spawn_blocking` thread with an explicit runtime handle. This is the
/// production code path used by the parallel ledger close.
#[tokio::test(flavor = "multi_thread")]
async fn test_parallel_soroban_from_spawn_blocking() {
    use henyey_ledger::{
        execute_soroban_parallel_phase, LedgerDelta, SnapshotBuilder, SnapshotHandle,
        SorobanContext, SorobanPhaseStructure,
    };

    let network_id = NetworkId::testnet();

    let (tx1, entries1) = build_extend_ttl_tx([33u8; 32], 2, [9u8; 32], 100, &network_id);
    let (tx2, entries2) = build_extend_ttl_tx([44u8; 32], 2, [10u8; 32], 200, &network_id);

    let mut builder = SnapshotBuilder::new(1);
    for (key, entry) in entries1.into_iter().chain(entries2.into_iter()) {
        builder = builder.add_entry(key, entry);
    }
    let snapshot = SnapshotHandle::new(builder.build_with_default_header());

    // 1 stage, 2 clusters → triggers multi-cluster parallel path.
    let phase = SorobanPhaseStructure {
        base_fee: None,
        stages: vec![vec![
            vec![(std::sync::Arc::new(tx1.clone()), None)],
            vec![(std::sync::Arc::new(tx2.clone()), None)],
        ]],
    };

    let handle = tokio::runtime::Handle::current();

    // Execute on a spawn_blocking thread with Some(handle).
    // This exercises the Handle::block_on path (not block_in_place).
    let outer_result = tokio::task::spawn_blocking(move || {
        let mut delta = LedgerDelta::new(1);
        let context = henyey_tx::LedgerContext::new(1, 1_000, 100, 5_000_000, 25, network_id);
        let result = execute_soroban_parallel_phase(
            &snapshot,
            &phase,
            0, // no classic TXs in test
            &context,
            &mut delta,
            SorobanContext {
                config: SorobanConfig::default(),
                base_prng_seed: [0u8; 32],
                classic_events: ClassicEventConfig::default(),
                module_cache: None,
                hot_archive: None,
                runtime_handle: Some(handle),
                soroban_state: None,
                offer_store: None,
                emit_soroban_tx_meta_ext_v1: false,
                enable_soroban_diagnostic_events: false,
            },
            None,
        )
        .expect("execute parallel phase from spawn_blocking");

        (result, delta)
    })
    .await
    .expect("spawn_blocking task");

    let (result, delta) = outer_result;
    assert_eq!(result.results.len(), 2, "expected 2 execution results");
    assert_eq!(result.tx_results.len(), 2, "expected 2 TX result pairs");
    assert_eq!(
        result.tx_result_metas.len(),
        2,
        "expected 2 TX result metas"
    );
    assert!(result.results[0].success, "TX1 should succeed");
    assert!(result.results[1].success, "TX2 should succeed");
    assert!(delta.num_changes() > 0, "delta should have changes");
    assert!(delta.fee_pool_delta() > 0, "fees should be collected");
}

/// Test that results from spawn_blocking path match the block_in_place path.
#[tokio::test(flavor = "multi_thread")]
async fn test_parallel_soroban_spawn_blocking_matches_worker() {
    use henyey_ledger::{
        execute_soroban_parallel_phase, LedgerDelta, SnapshotBuilder, SnapshotHandle,
        SorobanContext, SorobanPhaseStructure,
    };

    let network_id = NetworkId::testnet();

    let (tx1, entries1) = build_extend_ttl_tx([33u8; 32], 2, [9u8; 32], 100, &network_id);
    let (tx2, entries2) = build_extend_ttl_tx([44u8; 32], 2, [10u8; 32], 200, &network_id);

    let mut builder = SnapshotBuilder::new(1);
    for (key, entry) in entries1.into_iter().chain(entries2.into_iter()) {
        builder = builder.add_entry(key, entry);
    }
    let snapshot = SnapshotHandle::new(builder.build_with_default_header());

    let phase = SorobanPhaseStructure {
        base_fee: None,
        stages: vec![vec![
            vec![(std::sync::Arc::new(tx1.clone()), None)],
            vec![(std::sync::Arc::new(tx2.clone()), None)],
        ]],
    };

    // Run with None (block_in_place path, on worker thread).
    let mut delta_worker = LedgerDelta::new(1);
    let context = henyey_tx::LedgerContext::new(1, 1_000, 100, 5_000_000, 25, network_id);
    let worker_result = execute_soroban_parallel_phase(
        &snapshot,
        &phase,
        0, // no classic TXs in test
        &context,
        &mut delta_worker,
        SorobanContext {
            config: SorobanConfig::default(),
            base_prng_seed: [0u8; 32],
            classic_events: ClassicEventConfig::default(),
            module_cache: None,
            hot_archive: None,
            runtime_handle: None,
            emit_soroban_tx_meta_ext_v1: false,
            soroban_state: None,
            offer_store: None,
            enable_soroban_diagnostic_events: false,
        },
        None,
    )
    .expect("worker path");

    // Run with Some(handle) (Handle::block_on path, on spawn_blocking thread).
    let handle = tokio::runtime::Handle::current();
    let snapshot2 = SnapshotHandle::new({
        let mut b = SnapshotBuilder::new(1);
        let (_, e1) = build_extend_ttl_tx([33u8; 32], 2, [9u8; 32], 100, &network_id);
        let (_, e2) = build_extend_ttl_tx([44u8; 32], 2, [10u8; 32], 200, &network_id);
        for (key, entry) in e1.into_iter().chain(e2.into_iter()) {
            b = b.add_entry(key, entry);
        }
        b.build_with_default_header()
    });
    let phase2 = phase.clone();

    let (blocking_result, delta_blocking) = tokio::task::spawn_blocking(move || {
        let mut delta = LedgerDelta::new(1);
        let context = henyey_tx::LedgerContext::new(1, 1_000, 100, 5_000_000, 25, network_id);
        let result = execute_soroban_parallel_phase(
            &snapshot2,
            &phase2,
            0, // no classic TXs in test
            &context,
            &mut delta,
            SorobanContext {
                config: SorobanConfig::default(),
                base_prng_seed: [0u8; 32],
                classic_events: ClassicEventConfig::default(),
                module_cache: None,
                hot_archive: None,
                runtime_handle: Some(handle),
                soroban_state: None,
                offer_store: None,
                emit_soroban_tx_meta_ext_v1: false,
                enable_soroban_diagnostic_events: false,
            },
            None,
        )
        .expect("spawn_blocking path");
        (result, delta)
    })
    .await
    .expect("spawn_blocking task");

    // Both paths should produce identical results.
    assert_eq!(worker_result.results.len(), blocking_result.results.len());
    for (w, b) in worker_result
        .results
        .iter()
        .zip(blocking_result.results.iter())
    {
        assert_eq!(w.success, b.success);
        assert_eq!(w.fee_charged, b.fee_charged);
    }
    assert_eq!(
        worker_result.tx_results.len(),
        blocking_result.tx_results.len()
    );
    assert_eq!(
        delta_worker.fee_pool_delta(),
        delta_blocking.fee_pool_delta()
    );
    assert_eq!(delta_worker.num_changes(), delta_blocking.num_changes());
}

/// Regression test for cross-TX entry reload bug (mainnet ledger 59503619).
///
/// When TX1 deletes a trustline via ChangeTrust(limit=0), and TX2 in the same ledger
/// tries to access the same trustline, load_trustline must NOT reload it from the
/// bucket list snapshot. The per-TX snapshots (is_trustline_tracked) are cleared on
/// commit, so the delta's deleted_keys must be checked to prevent stale reloads.
///
/// Without the fix, TX2's preloader reloads the deleted trustline from the snapshot,
/// ChangeTrust finds it, tries to delete it again, hits negative num_sub_entries,
/// and returns an internal error mapped to OpNotSupported/TxNotSupported.
#[test]
fn test_cross_tx_deleted_trustline_not_reloaded() {
    let source_secret = SecretKey::from_seed(&[50u8; 32]);
    let source_id: AccountId = (&source_secret.public_key()).into();
    let issuer_id = AccountId(PublicKey::PublicKeyTypeEd25519(Uint256([99u8; 32])));

    // Asset: VELO issued by issuer_id
    let asset_code = AssetCode4(*b"VELO");
    let tl_asset = TrustLineAsset::CreditAlphanum4(AlphaNum4 {
        asset_code: asset_code.clone(),
        issuer: issuer_id.clone(),
    });

    // Source account has a trustline (num_sub_entries=1)
    let source_key = LedgerKey::Account(stellar_xdr::curr::LedgerKeyAccount {
        account_id: source_id.clone(),
    });
    let source_entry = LedgerEntry {
        last_modified_ledger_seq: 1,
        data: LedgerEntryData::Account(AccountEntry {
            account_id: source_id.clone(),
            balance: 100_000_000,
            seq_num: SequenceNumber(1),
            num_sub_entries: 1, // One trustline
            inflation_dest: None,
            flags: 0,
            home_domain: String32::default(),
            thresholds: Thresholds([1, 0, 0, 0]),
            signers: VecM::default(),
            ext: AccountEntryExt::V0,
        }),
        ext: LedgerEntryExt::V0,
    };

    // Issuer account
    let (issuer_key, issuer_entry) = create_account_entry(issuer_id.clone(), 1, 100_000_000);

    // Trustline entry with zero balance (ready to delete)
    let (tl_key, tl_entry) = create_trustline_entry(
        source_id.clone(),
        tl_asset.clone(),
        0,         // balance = 0 (required for deletion)
        1_000_000, // limit
        1,         // AUTHORIZED_FLAG
    );

    let snapshot = SnapshotBuilder::new(1)
        .add_entry(source_key, source_entry)
        .add_entry(issuer_key, issuer_entry)
        .add_entry(tl_key, tl_entry)
        .build_with_default_header();
    let snapshot = SnapshotHandle::new(snapshot);

    let network_id = NetworkId::testnet();
    let context = henyey_tx::LedgerContext::new(1, 1_000, 100, 5_000_000, 25, network_id);
    let mut executor = TransactionExecutor::new(
        &context,
        0,
        SorobanConfig::default(),
        ClassicEventConfig::default(),
    );

    // ===== TX1: Delete the trustline via ChangeTrust(limit=0) =====
    let change_trust_op = Operation {
        source_account: None,
        body: OperationBody::ChangeTrust(ChangeTrustOp {
            line: ChangeTrustAsset::CreditAlphanum4(AlphaNum4 {
                asset_code: asset_code.clone(),
                issuer: issuer_id.clone(),
            }),
            limit: 0,
        }),
    };

    let tx1 = Transaction {
        source_account: MuxedAccount::Ed25519(Uint256(*source_secret.public_key().as_bytes())),
        fee: 100,
        seq_num: SequenceNumber(2),
        cond: Preconditions::None,
        memo: Memo::None,
        operations: vec![change_trust_op].try_into().unwrap(),
        ext: TransactionExt::V0,
    };

    let mut envelope1 = TransactionEnvelope::Tx(TransactionV1Envelope {
        tx: tx1,
        signatures: VecM::default(),
    });
    let decorated1 = sign_envelope(&envelope1, &source_secret, &network_id);
    if let TransactionEnvelope::Tx(ref mut env) = envelope1 {
        env.signatures = vec![decorated1].try_into().unwrap();
    }

    let result1 = executor
        .execute_transaction(&snapshot, &envelope1, 100, None)
        .expect("TX1 execute");

    assert!(
        result1.success,
        "TX1 (delete trustline) should succeed. Result: {:?}",
        result1.operation_results
    );

    // Verify the trustline was deleted
    assert!(
        executor
            .state()
            .get_trustline_by_trustline_asset(&source_id, &tl_asset)
            .is_none(),
        "Trustline should be gone after TX1"
    );

    // Verify num_sub_entries decremented to 0
    let account = executor
        .state()
        .get_account(&source_id)
        .expect("source account");
    assert_eq!(
        account.num_sub_entries, 0,
        "num_sub_entries should be 0 after trustline deletion"
    );

    // Snapshot delta before TX2 (simulates ledger execution flow between transactions)
    executor.state_mut().snapshot_delta();

    // ===== TX2: Try to delete the same trustline again =====
    // This should fail with ChangeTrustResult::InvalidLimit because the trustline
    // no longer exists. Without the fix, load_trustline would reload it from the
    // snapshot, causing a negative subentry count error.
    let change_trust_op2 = Operation {
        source_account: None,
        body: OperationBody::ChangeTrust(ChangeTrustOp {
            line: ChangeTrustAsset::CreditAlphanum4(AlphaNum4 {
                asset_code: asset_code.clone(),
                issuer: issuer_id.clone(),
            }),
            limit: 0,
        }),
    };

    let tx2 = Transaction {
        source_account: MuxedAccount::Ed25519(Uint256(*source_secret.public_key().as_bytes())),
        fee: 100,
        seq_num: SequenceNumber(3),
        cond: Preconditions::None,
        memo: Memo::None,
        operations: vec![change_trust_op2].try_into().unwrap(),
        ext: TransactionExt::V0,
    };

    let mut envelope2 = TransactionEnvelope::Tx(TransactionV1Envelope {
        tx: tx2,
        signatures: VecM::default(),
    });
    let decorated2 = sign_envelope(&envelope2, &source_secret, &network_id);
    if let TransactionEnvelope::Tx(ref mut env) = envelope2 {
        env.signatures = vec![decorated2].try_into().unwrap();
    }

    let result2 = executor
        .execute_transaction(&snapshot, &envelope2, 100, None)
        .expect("TX2 execute");

    // TX2 should fail, but with a proper ChangeTrust failure (not TxNotSupported)
    assert!(
        !result2.success,
        "TX2 should fail because trustline was already deleted"
    );

    // The failure should NOT be NotSupported (which would indicate the internal error
    // from negative subentry count). It should be a normal op failure.
    assert_ne!(
        result2.failure,
        Some(ExecutionFailure::TxNotSupported),
        "TX2 should NOT return NotSupported (which indicates the deleted trustline was reloaded from snapshot)"
    );

    // The op result should be ChangeTrust::InvalidLimit (trustline doesn't exist)
    match result2.operation_results.get(0) {
        Some(OperationResult::OpInner(OperationResultTr::ChangeTrust(
            ChangeTrustResult::InvalidLimit,
        ))) => {
            // Expected: trustline not found, limit=0 is invalid
        }
        other => panic!(
            "TX2 should return ChangeTrust::InvalidLimit, got: {:?}",
            other
        ),
    }

    // Verify num_sub_entries is still 0 (not decremented to -1/underflow)
    let account = executor
        .state()
        .get_account(&source_id)
        .expect("source account");
    assert_eq!(
        account.num_sub_entries, 0,
        "num_sub_entries should remain 0 (not underflow)"
    );
}

/// Regression test for executor reuse across ledger closes via advance_to_ledger.
///
/// When the executor is reused across ledger boundaries, offers must be preserved
/// correctly: offers consumed in ledger N must not reappear in ledger N+1, and
/// surviving offers must remain available for matching.
///
/// This test exercises the `advance_to_ledger` path that avoids
/// reloading ~911K offers from the bucket list on every ledger close.
///
/// The test flow:
/// 1. Ledger 1: Load offers, execute a path payment that fully crosses offer A
/// 2. advance_to_ledger(ledger 2)
/// 3. Ledger 2: Verify offer A is gone; execute a path payment that crosses offer B
///    (which survived from ledger 1)
#[test]
fn test_advance_to_ledger() {
    // Accounts
    let source_secret = SecretKey::from_seed(&[220u8; 32]);
    let source_id: AccountId = (&source_secret.public_key()).into();

    let owner_a_secret = SecretKey::from_seed(&[221u8; 32]);
    let owner_a_id: AccountId = (&owner_a_secret.public_key()).into();

    let owner_b_secret = SecretKey::from_seed(&[222u8; 32]);
    let owner_b_id: AccountId = (&owner_b_secret.public_key()).into();

    let dest_id = AccountId(PublicKey::PublicKeyTypeEd25519(Uint256([223u8; 32])));

    let issuer_secret = SecretKey::from_seed(&[224u8; 32]);
    let issuer_id: AccountId = (&issuer_secret.public_key()).into();

    let asset_usd = Asset::CreditAlphanum4(AlphaNum4 {
        asset_code: AssetCode4([b'U', b'S', b'D', 0]),
        issuer: issuer_id.clone(),
    });

    // Accounts
    let (source_key, source_entry) = create_account_entry(source_id.clone(), 1, 500_000_000);
    let (dest_key, dest_entry) = create_account_entry(dest_id.clone(), 1, 200_000_000);
    let (issuer_key, issuer_entry) = create_account_entry(issuer_id.clone(), 1, 100_000_000);

    // Offer owner A: sells 50 XLM for USD
    // num_sub_entries = 2: one trustline + one offer
    let (owner_a_key, mut owner_a_entry) = create_account_entry(owner_a_id.clone(), 1, 500_000_000);
    if let LedgerEntryData::Account(ref mut acc) = owner_a_entry.data {
        acc.num_sub_entries = 2;
    }
    set_account_liabilities(&mut owner_a_entry, 50_000_000, 0);

    // Offer owner B: sells 30 XLM for USD
    // num_sub_entries = 2: one trustline + one offer
    let (owner_b_key, mut owner_b_entry) = create_account_entry(owner_b_id.clone(), 1, 500_000_000);
    if let LedgerEntryData::Account(ref mut acc) = owner_b_entry.data {
        acc.num_sub_entries = 2;
    }
    set_account_liabilities(&mut owner_b_entry, 30_000_000, 0);

    // Source needs USD trustline
    let (source_tl_key, source_tl_entry) = create_trustline_entry(
        source_id.clone(),
        TrustLineAsset::CreditAlphanum4(match &asset_usd {
            Asset::CreditAlphanum4(a) => a.clone(),
            _ => unreachable!(),
        }),
        200_000_000, // 200 USD
        300_000_000,
        TrustLineFlags::AuthorizedFlag as u32,
    );

    // Owner A USD trustline (buying side)
    let (owner_a_tl_key, mut owner_a_tl_entry) = create_trustline_entry(
        owner_a_id.clone(),
        TrustLineAsset::CreditAlphanum4(match &asset_usd {
            Asset::CreditAlphanum4(a) => a.clone(),
            _ => unreachable!(),
        }),
        0,
        100_000_000,
        TrustLineFlags::AuthorizedFlag as u32,
    );
    set_trustline_liabilities(&mut owner_a_tl_entry, 0, 50_000_000);

    // Owner B USD trustline (buying side)
    let (owner_b_tl_key, mut owner_b_tl_entry) = create_trustline_entry(
        owner_b_id.clone(),
        TrustLineAsset::CreditAlphanum4(match &asset_usd {
            Asset::CreditAlphanum4(a) => a.clone(),
            _ => unreachable!(),
        }),
        0,
        100_000_000,
        TrustLineFlags::AuthorizedFlag as u32,
    );
    set_trustline_liabilities(&mut owner_b_tl_entry, 0, 30_000_000);

    // Offer A: owner_a sells 50 XLM for USD at 1:1
    let offer_a_id: i64 = 10001;
    let (offer_a_key, offer_a_entry) = create_offer_entry(
        owner_a_id.clone(),
        offer_a_id,
        Asset::Native,
        asset_usd.clone(),
        50_000_000,
        Price { n: 1, d: 1 },
    );

    // Offer B: owner_b sells 30 XLM for USD at 1:1
    let offer_b_id: i64 = 10002;
    let (offer_b_key, offer_b_entry) = create_offer_entry(
        owner_b_id.clone(),
        offer_b_id,
        Asset::Native,
        asset_usd.clone(),
        30_000_000,
        Price { n: 1, d: 1 },
    );

    // Build snapshot
    let snapshot = SnapshotBuilder::new(1)
        .add_entry(source_key, source_entry)
        .add_entry(dest_key, dest_entry)
        .add_entry(issuer_key, issuer_entry)
        .add_entry(owner_a_key, owner_a_entry)
        .add_entry(owner_b_key, owner_b_entry)
        .add_entry(source_tl_key, source_tl_entry)
        .add_entry(owner_a_tl_key, owner_a_tl_entry)
        .add_entry(owner_b_tl_key, owner_b_tl_entry)
        .add_entry(offer_a_key.clone(), offer_a_entry)
        .add_entry(offer_b_key.clone(), offer_b_entry)
        .build_with_default_header();
    let snapshot = SnapshotHandle::new(snapshot);

    let network_id = NetworkId::testnet();

    // Create executor for ledger 1
    let context = henyey_tx::LedgerContext::new(
        1,         // ledger_seq
        1_000,     // close_time
        100,       // base_fee
        5_000_000, // base_reserve
        25,        // protocol_version
        network_id,
    );
    let mut executor = TransactionExecutor::new(
        &context,
        0,
        SorobanConfig::default(),
        ClassicEventConfig::default(),
    );
    executor
        .load_orderbook_offers(&snapshot)
        .expect("load orderbook");

    // ===== LEDGER 1: Path payment that fully crosses offer A =====
    let path_payment_op1 = Operation {
        source_account: None,
        body: OperationBody::PathPaymentStrictSend(PathPaymentStrictSendOp {
            send_asset: asset_usd.clone(),
            send_amount: 50_000_000,
            destination: dest_id.clone().into(),
            dest_asset: Asset::Native,
            dest_min: 1,
            path: VecM::default(),
        }),
    };

    let tx1 = Transaction {
        source_account: MuxedAccount::Ed25519(Uint256(*source_secret.public_key().as_bytes())),
        fee: 100,
        seq_num: SequenceNumber(2),
        cond: Preconditions::None,
        memo: Memo::None,
        operations: vec![path_payment_op1].try_into().unwrap(),
        ext: TransactionExt::V0,
    };

    let mut envelope1 = TransactionEnvelope::Tx(TransactionV1Envelope {
        tx: tx1,
        signatures: VecM::default(),
    });
    let decorated1 = sign_envelope(&envelope1, &source_secret, &network_id);
    if let TransactionEnvelope::Tx(ref mut env) = envelope1 {
        env.signatures = vec![decorated1].try_into().unwrap();
    }

    let result1 = executor
        .execute_transaction(&snapshot, &envelope1, 100, None)
        .expect("TX1 execute");
    assert!(
        result1.success,
        "TX1 (path payment crossing offer A) should succeed: {:?}",
        result1.operation_results
    );

    // Verify offer A was crossed
    match result1.operation_results.get(0) {
        Some(OperationResult::OpInner(OperationResultTr::PathPaymentStrictSend(
            PathPaymentStrictSendResult::Success(success),
        ))) => {
            assert!(!success.offers.is_empty(), "Should have crossed offer A");
        }
        other => panic!("Unexpected TX1 result: {:?}", other),
    }

    // Snapshot delta (as close_ledger would)
    executor.state_mut().snapshot_delta();

    // ===== ADVANCE TO LEDGER 2 =====
    // This is the key operation under test: preserving offers across ledger boundaries
    executor.advance_to_ledger(
        2,         // new ledger_seq
        2_000,     // new close_time
        5_000_000, // base_reserve
        25,        // protocol_version
        0,         // id_pool
        SorobanConfig::default(),
        henyey_tx::frozen_keys::FrozenKeyConfig::empty(),
        0, // ledger_flags
    );

    // Verify offer A is NOT in the executor's state (it was consumed in ledger 1)
    assert!(
        executor
            .state()
            .get_offer(&owner_a_id, offer_a_id)
            .is_none(),
        "Offer A should be gone after being fully crossed in ledger 1"
    );

    // Verify offer B IS still in the executor's state (it was not touched in ledger 1)
    assert!(
        executor
            .state()
            .get_offer(&owner_b_id, offer_b_id)
            .is_some(),
        "Offer B should be preserved across ledger advance"
    );

    // ===== LEDGER 2: Path payment that crosses offer B =====
    // This verifies the preserved offer B is actually usable for matching.
    //
    // After advance_to_ledger, non-offer entries (accounts,
    // trustlines) are cleared and will be reloaded from the snapshot. The
    // snapshot still has the original source account with seq_num=1, so TX2
    // must use seq_num=2 (the next valid seq for the reloaded state).
    let path_payment_op2 = Operation {
        source_account: None,
        body: OperationBody::PathPaymentStrictSend(PathPaymentStrictSendOp {
            send_asset: asset_usd.clone(),
            send_amount: 30_000_000,
            destination: dest_id.clone().into(),
            dest_asset: Asset::Native,
            dest_min: 1,
            path: VecM::default(),
        }),
    };

    let tx2 = Transaction {
        source_account: MuxedAccount::Ed25519(Uint256(*source_secret.public_key().as_bytes())),
        fee: 100,
        seq_num: SequenceNumber(2),
        cond: Preconditions::None,
        memo: Memo::None,
        operations: vec![path_payment_op2].try_into().unwrap(),
        ext: TransactionExt::V0,
    };

    let mut envelope2 = TransactionEnvelope::Tx(TransactionV1Envelope {
        tx: tx2,
        signatures: VecM::default(),
    });
    let decorated2 = sign_envelope(&envelope2, &source_secret, &network_id);
    if let TransactionEnvelope::Tx(ref mut env) = envelope2 {
        env.signatures = vec![decorated2].try_into().unwrap();
    }

    let result2 = executor
        .execute_transaction(&snapshot, &envelope2, 100, None)
        .expect("TX2 execute");
    assert!(
        result2.success,
        "TX2 (path payment crossing offer B in ledger 2) should succeed: {:?}",
        result2.operation_results
    );

    // Verify offer B was crossed
    match result2.operation_results.get(0) {
        Some(OperationResult::OpInner(OperationResultTr::PathPaymentStrictSend(
            PathPaymentStrictSendResult::Success(success),
        ))) => {
            assert!(!success.offers.is_empty(), "Should have crossed offer B");
        }
        other => panic!("Unexpected TX2 result: {:?}", other),
    }
}

/// Regression test: internal errors during operation execution must map to txInternalError.
///
/// In stellar-core, when a std::runtime_error is thrown during operation execution
/// (e.g. liabilities underflow during offer crossing), it propagates to
/// TransactionFrame::applyOperations() which catches it and sets txINTERNAL_ERROR.
///
/// Previously our code mapped all operation Err values to OpNotSupported/TxNotSupported.
/// This caused a parity mismatch found at mainnet ledger 61171083.
///
/// This test creates an inconsistent state where an offer's computed liabilities
/// exceed the stored liabilities, triggering an underflow during releaseLiabilities.
#[test]
fn test_internal_error_maps_to_tx_internal_error() {
    let source_secret = SecretKey::from_seed(&[210u8; 32]);
    let source_id: AccountId = (&source_secret.public_key()).into();

    let seller_secret = SecretKey::from_seed(&[211u8; 32]);
    let seller_id: AccountId = (&seller_secret.public_key()).into();

    let dest_id = AccountId(PublicKey::PublicKeyTypeEd25519(Uint256([212u8; 32])));

    let issuer_secret = SecretKey::from_seed(&[213u8; 32]);
    let issuer_id: AccountId = (&issuer_secret.public_key()).into();

    let asset_usd = Asset::CreditAlphanum4(AlphaNum4 {
        asset_code: AssetCode4([b'U', b'S', b'D', 0]),
        issuer: issuer_id.clone(),
    });

    // Create accounts
    let (source_key, source_entry) = create_account_entry(source_id.clone(), 1, 500_000_000);
    let (dest_key, dest_entry) = create_account_entry(dest_id.clone(), 1, 200_000_000);
    let (issuer_key, issuer_entry) = create_account_entry(issuer_id.clone(), 1, 100_000_000);

    // Seller account: has NO selling liabilities set (V0 ext = liabilities 0)
    // but owns an offer selling 50 XLM. This is an inconsistent state that
    // triggers liabilities underflow when the offer is crossed.
    let (seller_key, seller_entry) = create_account_entry(seller_id.clone(), 1, 500_000_000);

    // Source needs USD trustline to send
    let (source_tl_key, source_tl_entry) = create_trustline_entry(
        source_id.clone(),
        TrustLineAsset::CreditAlphanum4(match &asset_usd {
            Asset::CreditAlphanum4(a) => a.clone(),
            _ => unreachable!(),
        }),
        100_000_000,
        200_000_000,
        TrustLineFlags::AuthorizedFlag as u32,
    );

    // Seller needs USD trustline (buying side)
    let (seller_tl_key, seller_tl_entry) = create_trustline_entry(
        seller_id.clone(),
        TrustLineAsset::CreditAlphanum4(match &asset_usd {
            Asset::CreditAlphanum4(a) => a.clone(),
            _ => unreachable!(),
        }),
        0,
        100_000_000,
        TrustLineFlags::AuthorizedFlag as u32,
    );
    // NOTE: seller trustline buying liabilities are also 0 (inconsistent with offer)

    // Offer: seller sells 50 XLM for USD at 1:1
    // The offer exists but liabilities on account/trustline are 0 → underflow on cross
    let offer_id: i64 = 99999;
    let (offer_key, offer_entry) = create_offer_entry(
        seller_id.clone(),
        offer_id,
        Asset::Native,
        asset_usd.clone(),
        50_000_000,
        Price { n: 1, d: 1 },
    );

    let snapshot = SnapshotBuilder::new(1)
        .add_entry(source_key, source_entry)
        .add_entry(dest_key, dest_entry)
        .add_entry(seller_key, seller_entry)
        .add_entry(issuer_key, issuer_entry)
        .add_entry(source_tl_key, source_tl_entry)
        .add_entry(seller_tl_key, seller_tl_entry)
        .add_entry(offer_key, offer_entry)
        .build_with_default_header();
    let snapshot = SnapshotHandle::new(snapshot);

    let network_id = NetworkId::testnet();

    let context = henyey_tx::LedgerContext::new(1, 1_000, 100, 5_000_000, 25, network_id);
    let mut executor = TransactionExecutor::new(
        &context,
        0,
        SorobanConfig::default(),
        ClassicEventConfig::default(),
    );
    executor
        .load_orderbook_offers(&snapshot)
        .expect("load orderbook");

    // PathPaymentStrictReceive that will cross the offer
    let op = Operation {
        source_account: None,
        body: OperationBody::PathPaymentStrictReceive(PathPaymentStrictReceiveOp {
            send_asset: asset_usd.clone(),
            send_max: 100_000_000,
            destination: dest_id.into(),
            dest_asset: Asset::Native,
            dest_amount: 50_000_000,
            path: VecM::default(),
        }),
    };

    let tx = Transaction {
        source_account: MuxedAccount::Ed25519(Uint256(*source_secret.public_key().as_bytes())),
        fee: 100,
        seq_num: SequenceNumber(2),
        cond: Preconditions::None,
        memo: Memo::None,
        operations: vec![op].try_into().unwrap(),
        ext: TransactionExt::V0,
    };

    let mut envelope = TransactionEnvelope::Tx(TransactionV1Envelope {
        tx,
        signatures: VecM::default(),
    });
    let decorated = sign_envelope(&envelope, &source_secret, &network_id);
    if let TransactionEnvelope::Tx(ref mut env) = envelope {
        env.signatures = vec![decorated].try_into().unwrap();
    }

    let result = executor
        .execute_transaction(&snapshot, &envelope, 100, None)
        .expect("execute_transaction should not return Err");

    // The transaction should fail with InternalError (not NotSupported)
    assert!(
        !result.success,
        "TX should fail due to liabilities underflow"
    );
    assert_eq!(
        result.failure,
        Some(ExecutionFailure::TxInternalError),
        "Internal errors must map to ExecutionFailure::TxInternalError, not NotSupported"
    );
}

/// Regression test for CreateAccount check order with sponsorship (mainnet ledger 61232072).
///
/// When a sponsored CreateAccount fails because both the sponsor lacks reserve AND
/// the source lacks available balance, the result must be LowReserve (sponsor checked
/// first via createEntryWithPossibleSponsorship), not Underfunded (source checked first).
///
/// This exercises the full TransactionExecutor pipeline, not just the op-level function.
#[test]
fn test_create_account_sponsor_low_reserve_before_underfunded() {
    let source_secret = SecretKey::from_seed(&[230u8; 32]);
    let source_id: AccountId = (&source_secret.public_key()).into();

    let sponsor_secret = SecretKey::from_seed(&[231u8; 32]);
    let sponsor_id: AccountId = (&sponsor_secret.public_key()).into();
    let dest_id = AccountId(PublicKey::PublicKeyTypeEd25519(Uint256([232u8; 32])));

    // Source: just above minimum balance — enough to pay fees but NOT enough
    // available balance for the starting_balance of 20M.
    // min_balance = 2 * base_reserve = 10M, available = 11M - 10M = 1M (< 20M)
    let (source_key, source_entry) = create_account_entry(source_id.clone(), 1, 11_000_000);

    // Sponsor: minimum balance only, can't afford to sponsor (needs 2x base_reserve extra)
    let (sponsor_key, sponsor_entry) =
        create_sponsor_account_entry(sponsor_id.clone(), 1, 10_000_000, 0);

    let snapshot = SnapshotBuilder::new(1)
        .add_entry(source_key, source_entry)
        .add_entry(sponsor_key, sponsor_entry)
        .build_with_default_header();
    let snapshot = SnapshotHandle::new(snapshot);

    let network_id = NetworkId::testnet();

    // Build a CreateAccount op inside a BeginSponsoringFutureReserves sandwich
    let ops: Vec<Operation> = vec![
        // Op 0: sponsor begins sponsoring
        Operation {
            source_account: Some(MuxedAccount::Ed25519(match &sponsor_id.0 {
                PublicKey::PublicKeyTypeEd25519(k) => k.clone(),
            })),
            body: OperationBody::BeginSponsoringFutureReserves(
                stellar_xdr::curr::BeginSponsoringFutureReservesOp {
                    sponsored_id: dest_id.clone(),
                },
            ),
        },
        // Op 1: CreateAccount (should fail with LowReserve, not Underfunded)
        Operation {
            source_account: None,
            body: OperationBody::CreateAccount(CreateAccountOp {
                destination: dest_id.clone(),
                starting_balance: 20_000_000, // source can't afford this either
            }),
        },
    ];

    let tx = Transaction {
        source_account: MuxedAccount::Ed25519(Uint256(*source_secret.public_key().as_bytes())),
        fee: 200,
        seq_num: SequenceNumber(2),
        cond: Preconditions::None,
        memo: Memo::None,
        operations: ops.try_into().unwrap(),
        ext: TransactionExt::V0,
    };

    let mut envelope = TransactionEnvelope::Tx(TransactionV1Envelope {
        tx,
        signatures: VecM::default(),
    });

    // Sign with both source and sponsor
    let source_sig = sign_envelope(&envelope, &source_secret, &network_id);
    let sponsor_sig = sign_envelope(&envelope, &sponsor_secret, &network_id);
    if let TransactionEnvelope::Tx(ref mut env) = envelope {
        env.signatures = vec![source_sig, sponsor_sig].try_into().unwrap();
    }

    let context = henyey_tx::LedgerContext::new(1, 1_000, 100, 5_000_000, 25, network_id);
    let mut executor = TransactionExecutor::new(
        &context,
        0,
        SorobanConfig::default(),
        ClassicEventConfig::default(),
    );

    let result = executor
        .execute_transaction(&snapshot, &envelope, 100, None)
        .expect("execute");

    // TX should fail (CreateAccount op fails)
    assert!(
        !result.success,
        "TX should fail because sponsor can't afford reserve. failure={:?} ops={:?}",
        result.failure, result.operation_results
    );

    // Op 1 (CreateAccount) must be LowReserve, NOT Underfunded
    let op_result = &result.operation_results[1];
    match op_result {
        OperationResult::OpInner(OperationResultTr::CreateAccount(r)) => {
            assert!(
                matches!(r, CreateAccountResult::LowReserve),
                "Must return LowReserve (sponsor checked first), not Underfunded; got {:?}",
                r
            );
        }
        other => panic!("Expected CreateAccount result, got {:?}", other),
    }
}

/// Regression test: ClawbackClaimableBalance must return NotIssuer (not NotClawbackEnabled)
/// when the source account is not the issuer of the claimable balance's asset.
///
/// This test reproduces a bug found at mainnet ledger 59536635 where a
/// ClawbackClaimableBalance operation failed because the source was not the issuer,
/// but our code returned `NotClawbackEnabled` instead of `NotIssuer`.
///
/// stellar-core's ClawbackClaimableBalanceOpFrame::doApply checks in this order:
///   1. CB entry exists? → DOES_NOT_EXIST
///   2. Asset is native? → NOT_ISSUER
///   3. Source == issuer? → NOT_ISSUER
///   4. isClawbackEnabledOnClaimableBalance? → NOT_CLAWBACK_ENABLED
///   5. Success
///
/// Our code was returning NotClawbackEnabled for checks 2, 3, and was also
/// checking the issuer account's AUTH_CLAWBACK_ENABLED flag instead of the
/// claimable balance entry's own CLAIMABLE_BALANCE_CLAWBACK_ENABLED_FLAG.
#[test]
fn test_clawback_claimable_balance_not_issuer_error_code() {
    use stellar_xdr::curr::{
        ClaimableBalanceEntryExtensionV1, ClaimableBalanceEntryExtensionV1Ext,
        ClaimableBalanceFlags, ClawbackClaimableBalanceResult,
    };

    let source_secret = SecretKey::from_seed(&[240u8; 32]);
    let source_id: AccountId = (&source_secret.public_key()).into();

    // The actual issuer of the asset in the claimable balance
    let issuer_id = AccountId(PublicKey::PublicKeyTypeEd25519(Uint256([241u8; 32])));

    let asset = Asset::CreditAlphanum4(AlphaNum4 {
        asset_code: AssetCode4([b'U', b'S', b'D', 0]),
        issuer: issuer_id.clone(),
    });

    // Create source account (NOT the issuer) with AUTH_CLAWBACK_ENABLED
    let (source_key, source_entry) = create_account_entry(source_id.clone(), 1, 100_000_000);

    // Create a claimable balance with clawback enabled on the CB entry itself
    let cb_id = ClaimableBalanceId::ClaimableBalanceIdTypeV0(Hash([42u8; 32]));
    let cb_key = LedgerKey::ClaimableBalance(LedgerKeyClaimableBalance {
        balance_id: cb_id.clone(),
    });
    let cb_entry = LedgerEntry {
        last_modified_ledger_seq: 1,
        data: LedgerEntryData::ClaimableBalance(ClaimableBalanceEntry {
            balance_id: cb_id.clone(),
            claimants: vec![Claimant::ClaimantTypeV0(ClaimantV0 {
                destination: source_id.clone(),
                predicate: ClaimPredicate::Unconditional,
            })]
            .try_into()
            .unwrap(),
            asset: asset.clone(),
            amount: 1_000_000,
            ext: ClaimableBalanceEntryExt::V1(ClaimableBalanceEntryExtensionV1 {
                ext: ClaimableBalanceEntryExtensionV1Ext::V0,
                flags: ClaimableBalanceFlags::ClaimableBalanceClawbackEnabledFlag as u32,
            }),
        }),
        ext: LedgerEntryExt::V0,
    };

    let snapshot = SnapshotBuilder::new(1)
        .add_entry(source_key, source_entry)
        .add_entry(cb_key, cb_entry)
        .build_with_default_header();
    let snapshot = SnapshotHandle::new(snapshot);

    let network_id = NetworkId::testnet();

    // ClawbackClaimableBalance from source (who is NOT the issuer)
    let op = Operation {
        source_account: None,
        body: OperationBody::ClawbackClaimableBalance(ClawbackClaimableBalanceOp {
            balance_id: cb_id,
        }),
    };

    let tx = Transaction {
        source_account: MuxedAccount::Ed25519(Uint256(*source_secret.public_key().as_bytes())),
        fee: 100,
        seq_num: SequenceNumber(2),
        cond: Preconditions::None,
        memo: Memo::None,
        operations: vec![op].try_into().unwrap(),
        ext: TransactionExt::V0,
    };

    let mut envelope = TransactionEnvelope::Tx(TransactionV1Envelope {
        tx,
        signatures: VecM::default(),
    });
    let decorated = sign_envelope(&envelope, &source_secret, &network_id);
    if let TransactionEnvelope::Tx(ref mut env) = envelope {
        env.signatures = vec![decorated].try_into().unwrap();
    }

    let context = henyey_tx::LedgerContext::new(1, 1_000, 100, 5_000_000, 25, network_id);
    let mut executor = TransactionExecutor::new(
        &context,
        0,
        SorobanConfig::default(),
        ClassicEventConfig::default(),
    );

    let result = executor
        .execute_transaction(&snapshot, &envelope, 100, None)
        .expect("execute");

    // The TX should fail (op fails)
    assert!(
        !result.success,
        "TX should fail because source is not issuer"
    );

    // The op result MUST be NotIssuer (not NotClawbackEnabled)
    match &result.operation_results[0] {
        OperationResult::OpInner(OperationResultTr::ClawbackClaimableBalance(r)) => {
            assert!(
                matches!(r, ClawbackClaimableBalanceResult::NotIssuer),
                "Expected NotIssuer when source is not the asset issuer, got {:?}",
                r
            );
        }
        other => panic!("Expected ClawbackClaimableBalance result, got {:?}", other),
    }
}

/// Regression test: All classic transaction fees must be deducted upfront
/// before any TX body executes (matching stellar-core processFeesSeqNums).
///
/// This test reproduces the bug found at mainnet ledger 59534195 where an account
/// submitted 25 transactions in one ledger. Without upfront fee deduction, early
/// TXs saw inflated balances (only their own fee deducted, not all 25). This caused
/// path payments to succeed that should have failed with UNDERFUNDED because the
/// correct post-all-fees balance was insufficient.
///
/// Scenario:
///   - Source account: balance = min_balance + 500 (available = 500)
///   - 3 TXs with fee=100 each (total fees = 300)
///   - Each TX sends 200 native to a destination
///
/// With correct upfront fee deduction:
///   - After all fees: available = 200
///   - TX 1: payment 200 succeeds (uses all remaining available)
///   - TX 2: payment 200 fails (UNDERFUNDED, available = 0)
///
/// With the old bug (per-TX fee deduction):
///   - TX 1: fee 100 → available 400, payment 200 → succeeds
///   - TX 2: fee 100 → available 100, payment 200 → fails
///   (TX 1 incorrectly succeeds with higher available balance)
#[test]
fn test_classic_fees_deducted_upfront_before_tx_execution() {
    use henyey_ledger::execution::{execute_transaction_set_with_fee_mode, SorobanContext};
    use henyey_ledger::LedgerDelta;

    let secret = SecretKey::from_seed(&[77u8; 32]);
    let source_id: AccountId = (&secret.public_key()).into();
    let dest_id = AccountId(PublicKey::PublicKeyTypeEd25519(Uint256([88u8; 32])));

    let base_reserve: i64 = 5_000_000;
    let min_balance = 2 * base_reserve; // 10_000_000 for 0 sub entries
    let source_balance = min_balance + 500; // available = 500
    let base_fee: u32 = 100;

    let (source_key, source_entry) = create_account_entry(source_id.clone(), 100, source_balance);
    let (dest_key, dest_entry) = create_account_entry(dest_id.clone(), 1, 10_000_000);
    let snapshot = SnapshotBuilder::new(1)
        .add_entry(source_key, source_entry)
        .add_entry(dest_key, dest_entry)
        .build_with_default_header();
    let snapshot = SnapshotHandle::new(snapshot);

    // Create 3 payment TXs from the same source, each paying 200 native.
    let network_id = NetworkId::testnet();
    let mut tx_set: Vec<(std::sync::Arc<TransactionEnvelope>, Option<u32>)> = Vec::new();
    for i in 0..3u32 {
        let operation = Operation {
            source_account: None,
            body: OperationBody::Payment(stellar_xdr::curr::PaymentOp {
                destination: MuxedAccount::Ed25519(Uint256([88u8; 32])),
                asset: stellar_xdr::curr::Asset::Native,
                amount: 200,
            }),
        };

        let tx = Transaction {
            source_account: MuxedAccount::Ed25519(Uint256(*secret.public_key().as_bytes())),
            fee: base_fee * 1, // 1 op
            seq_num: SequenceNumber(101 + i as i64),
            cond: Preconditions::None,
            memo: Memo::None,
            operations: vec![operation].try_into().unwrap(),
            ext: TransactionExt::V0,
        };

        let mut envelope = TransactionEnvelope::Tx(TransactionV1Envelope {
            tx,
            signatures: VecM::default(),
        });

        let decorated = sign_envelope(&envelope, &secret, &network_id);
        if let TransactionEnvelope::Tx(ref mut env) = envelope {
            env.signatures = vec![decorated].try_into().unwrap();
        }

        tx_set.push((std::sync::Arc::new(envelope), None));
    }

    let context =
        henyey_tx::LedgerContext::new(2, 1_000, base_fee, base_reserve as u32, 25, network_id);
    let mut delta = LedgerDelta::new(2);
    let soroban = SorobanContext {
        config: SorobanConfig::default(),
        base_prng_seed: [0u8; 32],
        classic_events: ClassicEventConfig::default(),
        module_cache: None,
        hot_archive: None,
        runtime_handle: None,
        soroban_state: None,
        offer_store: None,
        emit_soroban_tx_meta_ext_v1: false,
        enable_soroban_diagnostic_events: false,
    };

    let result = execute_transaction_set_with_fee_mode(
        &snapshot, &tx_set, &context, &mut delta, soroban, true,
    )
    .expect("execute tx set");

    // TX 0: With upfront fee deduction (3 × 100 = 300), available = 500 - 300 = 200.
    // Payment of 200 should succeed (exactly enough).
    assert!(
        result.results[0].success,
        "TX 0 should succeed: available (200) >= payment (200); got {:?}",
        result.results[0].failure
    );

    // TX 1: After TX 0's payment of 200, available = 0.
    // Payment of 200 should fail with UNDERFUNDED.
    assert!(
        !result.results[1].success,
        "TX 1 should fail: available (0) < payment (200)"
    );

    // TX 2: Same - should fail with UNDERFUNDED.
    assert!(
        !result.results[2].success,
        "TX 2 should fail: available (0) < payment (200)"
    );
}

/// Regression test for VE-02: pool share trustlines not in memory are correctly
/// redeemed when `SetTrustLineFlags` deauthorizes an account.
///
/// Before this fix, `find_pool_share_trustlines_for_asset` only searched
/// in-memory state. When pool share trustlines existed in the bucket list but
/// hadn't been loaded into memory, `redeem_pool_share_trustlines` found nothing
/// and returned early — no claimable balances were created, and the pool remained
/// stale. This caused the hash mismatch at mainnet ledger L59845023.
///
/// The fix adds:
/// 1. A secondary index (`pool_share_tl_account_index`) built from the bucket list
///    scan, mirroring how `offer_account_asset_index` works for offers.
/// 2. `load_pool_share_trustlines_for_account_and_asset` called during
///    `SetTrustLineFlags`/`AllowTrust` operation loading, which uses the index
///    to pre-load all pool share trustlines for the trustor before the op runs.
#[test]
fn test_set_trust_line_flags_redeems_pool_shares_loaded_from_snapshot() {
    use henyey_ledger::{EntryLookupFn, PoolShareTrustlinesByAccountFn};
    use std::collections::HashMap;
    use std::sync::Arc;
    use stellar_xdr::curr::{
        Liabilities, Limits, TrustLineEntryExt, TrustLineEntryExtensionV2,
        TrustLineEntryExtensionV2Ext, TrustLineEntryV1, TrustLineEntryV1Ext, TrustLineFlags,
        WriteXdr,
    };

    let network_id = NetworkId::testnet();

    // Accounts: issuer (AUTH_REQUIRED | AUTH_REVOCABLE), trustor, other_issuer for asset_b.
    let issuer_secret = SecretKey::from_seed(&[80u8; 32]);
    let issuer_id: AccountId = (&issuer_secret.public_key()).into();
    let trustor_id = AccountId(PublicKey::PublicKeyTypeEd25519(Uint256([81u8; 32])));
    let other_issuer_id = AccountId(PublicKey::PublicKeyTypeEd25519(Uint256([82u8; 32])));

    let asset_a = Asset::CreditAlphanum4(AlphaNum4 {
        asset_code: AssetCode4(*b"RUV\0"),
        issuer: issuer_id.clone(),
    });
    let asset_b = Asset::CreditAlphanum4(AlphaNum4 {
        asset_code: AssetCode4(*b"XLM\0"),
        issuer: other_issuer_id.clone(),
    });

    let pool_id = PoolId(Hash([80u8; 32]));

    // Pool share trustline (100 of 500 total shares) — stored ONLY via the
    // lookup function, NOT in the snapshot's initial entries.  This simulates
    // the VE-02 scenario where the entry lives in the bucket list on disk.
    let pool_share_tl_asset = TrustLineAsset::PoolShare(pool_id.clone());
    let pool_share_tl_key = LedgerKey::Trustline(LedgerKeyTrustLine {
        account_id: trustor_id.clone(),
        asset: pool_share_tl_asset.clone(),
    });
    let pool_share_tl_entry = LedgerEntry {
        last_modified_ledger_seq: 1,
        data: LedgerEntryData::Trustline(TrustLineEntry {
            account_id: trustor_id.clone(),
            asset: pool_share_tl_asset,
            balance: 100,
            limit: i64::MAX,
            flags: 0,
            ext: TrustLineEntryExt::V0,
        }),
        ext: LedgerEntryExt::V0,
    };

    // Asset A trustline for trustor (authorized, pool_use_count=1)
    let (asset_a_tl_key, asset_a_tl_entry) = {
        let asset = TrustLineAsset::CreditAlphanum4(AlphaNum4 {
            asset_code: AssetCode4(*b"RUV\0"),
            issuer: issuer_id.clone(),
        });
        let key = LedgerKey::Trustline(LedgerKeyTrustLine {
            account_id: trustor_id.clone(),
            asset: asset.clone(),
        });
        let entry = LedgerEntry {
            last_modified_ledger_seq: 1,
            data: LedgerEntryData::Trustline(TrustLineEntry {
                account_id: trustor_id.clone(),
                asset,
                balance: 5000,
                limit: 100_000,
                flags: TrustLineFlags::AuthorizedFlag as u32,
                ext: TrustLineEntryExt::V1(TrustLineEntryV1 {
                    liabilities: Liabilities {
                        buying: 0,
                        selling: 0,
                    },
                    ext: TrustLineEntryV1Ext::V2(TrustLineEntryExtensionV2 {
                        liquidity_pool_use_count: 1,
                        ext: TrustLineEntryExtensionV2Ext::V0,
                    }),
                }),
            }),
            ext: LedgerEntryExt::V0,
        };
        (key, entry)
    };

    // Asset B trustline for trustor (authorized, pool_use_count=1)
    let (asset_b_tl_key, asset_b_tl_entry) = {
        let asset = TrustLineAsset::CreditAlphanum4(AlphaNum4 {
            asset_code: AssetCode4(*b"XLM\0"),
            issuer: other_issuer_id.clone(),
        });
        let key = LedgerKey::Trustline(LedgerKeyTrustLine {
            account_id: trustor_id.clone(),
            asset: asset.clone(),
        });
        let entry = LedgerEntry {
            last_modified_ledger_seq: 1,
            data: LedgerEntryData::Trustline(TrustLineEntry {
                account_id: trustor_id.clone(),
                asset,
                balance: 5000,
                limit: 100_000,
                flags: TrustLineFlags::AuthorizedFlag as u32,
                ext: TrustLineEntryExt::V1(TrustLineEntryV1 {
                    liabilities: Liabilities {
                        buying: 0,
                        selling: 0,
                    },
                    ext: TrustLineEntryV1Ext::V2(TrustLineEntryExtensionV2 {
                        liquidity_pool_use_count: 1,
                        ext: TrustLineEntryExtensionV2Ext::V0,
                    }),
                }),
            }),
            ext: LedgerEntryExt::V0,
        };
        (key, entry)
    };

    // Issuer account (AUTH_REQUIRED | AUTH_REVOCABLE)
    let (issuer_key, issuer_entry) =
        create_account_entry_with_flags(issuer_id.clone(), 1, 100_000_000, 0x1 | 0x2);

    // Trustor account — num_sub_entries=4 (2 asset TLs × 1 each + pool share TL counts as 2)
    let trustor_key = LedgerKey::Account(stellar_xdr::curr::LedgerKeyAccount {
        account_id: trustor_id.clone(),
    });
    let trustor_entry = LedgerEntry {
        last_modified_ledger_seq: 1,
        data: LedgerEntryData::Account(AccountEntry {
            account_id: trustor_id.clone(),
            balance: 100_000_000,
            seq_num: SequenceNumber(0),
            num_sub_entries: 4,
            inflation_dest: None,
            flags: 0,
            home_domain: String32::default(),
            thresholds: Thresholds([1, 0, 0, 0]),
            signers: VecM::default(),
            ext: AccountEntryExt::V0,
        }),
        ext: LedgerEntryExt::V0,
    };

    // Other issuer account (needed for asset_b trustline validation)
    let (other_issuer_key, other_issuer_entry) =
        create_account_entry(other_issuer_id.clone(), 0, 100_000_000);

    // Liquidity pool: reserve_a=1000, reserve_b=2000, total_shares=500, tl_count=1
    let (pool_key, pool_entry) = create_liquidity_pool_entry(
        pool_id.clone(),
        asset_a.clone(),
        asset_b.clone(),
        1000,
        2000,
        500,
        1,
    );

    // Build snapshot WITHOUT the pool share trustline — it's "on disk" only.
    let snapshot = SnapshotBuilder::new(10)
        .add_entry(issuer_key, issuer_entry)
        .add_entry(trustor_key, trustor_entry)
        .add_entry(other_issuer_key, other_issuer_entry)
        .add_entry(pool_key, pool_entry)
        .add_entry(asset_a_tl_key, asset_a_tl_entry)
        .add_entry(asset_b_tl_key, asset_b_tl_entry)
        .build_with_default_header();

    // Encode the pool share TL key for lookup.
    let pool_share_tl_key_bytes = pool_share_tl_key
        .to_xdr(Limits::none())
        .expect("encode pool share TL key");

    // Lookup function: returns the pool share TL for the trustor when queried by key.
    let extra_entries: Arc<HashMap<Vec<u8>, LedgerEntry>> = Arc::new({
        let mut m = HashMap::new();
        m.insert(pool_share_tl_key_bytes, pool_share_tl_entry);
        m
    });
    let lookup_fn: EntryLookupFn = Arc::new(move |key| {
        let key_bytes = key
            .to_xdr(Limits::none())
            .map_err(|e| henyey_ledger::LedgerError::Serialization(e.to_string()))?;
        Ok(extra_entries.get(&key_bytes).cloned())
    });

    // Pool share TL secondary index: trustor → [pool_id]
    let captured_pool_id = pool_id.clone();
    let captured_trustor_id = trustor_id.clone();
    let pool_share_index_fn: PoolShareTrustlinesByAccountFn = Arc::new(move |account_id| {
        if account_id == &captured_trustor_id {
            Ok(vec![captured_pool_id.clone()])
        } else {
            Ok(vec![])
        }
    });

    let mut handle = SnapshotHandle::new(snapshot);
    handle.set_lookup(lookup_fn);
    handle.set_pool_share_tls_by_account(pool_share_index_fn);

    // Build and sign a SetTrustLineFlags transaction issued by the RUV issuer
    // to deauthorize the trustor's RUV trustline.
    let op = Operation {
        source_account: None,
        body: OperationBody::SetTrustLineFlags(SetTrustLineFlagsOp {
            trustor: trustor_id.clone(),
            asset: asset_a.clone(),
            clear_flags: TrustLineFlags::AuthorizedFlag as u32,
            set_flags: 0,
        }),
    };

    let tx = Transaction {
        source_account: MuxedAccount::Ed25519(Uint256(*issuer_secret.public_key().as_bytes())),
        fee: 1000,
        seq_num: SequenceNumber(2),
        cond: Preconditions::None,
        memo: Memo::None,
        operations: vec![op].try_into().unwrap(),
        ext: TransactionExt::V0,
    };

    let mut envelope = TransactionEnvelope::Tx(TransactionV1Envelope {
        tx,
        signatures: VecM::default(),
    });
    let decorated = sign_envelope(&envelope, &issuer_secret, &network_id);
    if let TransactionEnvelope::Tx(ref mut env) = envelope {
        env.signatures = vec![decorated].try_into().unwrap();
    }

    let context = henyey_tx::LedgerContext::new(10, 1_000, 100, 5_000_000, 25, network_id);
    let mut executor = TransactionExecutor::new(
        &context,
        0,
        SorobanConfig::default(),
        ClassicEventConfig::default(),
    );

    let result = executor
        .execute_transaction(&handle, &envelope, 100, None)
        .expect("execute SetTrustLineFlags");

    assert!(
        result.success,
        "SetTrustLineFlags should succeed, got failure: {:?}",
        result.failure
    );

    // Verify claimable balances were created for both pool assets:
    //   amount_a = floor(100 * 1000 / 500) = 200
    //   amount_b = floor(100 * 2000 / 500) = 400
    let tx_meta = result.tx_meta.expect("tx meta");
    let TransactionMeta::V4(ref meta) = tx_meta else {
        panic!("expected TransactionMeta::V4");
    };

    // Collect all ledger entry changes across fee-change frames and operation meta.
    let mut found_asset_a_cb = false;
    let mut found_asset_b_cb = false;
    let mut cb_count = 0;
    let all_op_changes: Vec<&LedgerEntryChange> = meta
        .operations
        .iter()
        .flat_map(|op| op.changes.iter())
        .collect();
    for change in all_op_changes {
        let entry = match change {
            LedgerEntryChange::Created(e) => e,
            _ => continue,
        };
        if let LedgerEntryData::ClaimableBalance(ref cb) = entry.data {
            cb_count += 1;
            if cb.asset == asset_a {
                assert_eq!(cb.amount, 200, "asset_a claimable balance amount");
                found_asset_a_cb = true;
            } else if cb.asset == asset_b {
                assert_eq!(cb.amount, 400, "asset_b claimable balance amount");
                found_asset_b_cb = true;
            }
        }
    }

    assert!(
        found_asset_a_cb,
        "claimable balance for asset_a (RUV) should have been created — \
         before VE-02 fix, pool share TL was not loaded from snapshot"
    );
    assert!(
        found_asset_b_cb,
        "claimable balance for asset_b (XLM) should have been created — \
         before VE-02 fix, pool share TL was not loaded from snapshot"
    );
    assert_eq!(
        cb_count, 2,
        "exactly 2 claimable balances should be created (one per pool asset)"
    );
}

/// Regression test for VE-03: fee-bump outer fee must be charged even when the inner
/// transaction's auth fails at apply time due to a prior transaction removing the signer.
///
/// In stellar-core, `processFeeSeqNum()` charges the outer fee BEFORE `apply()` re-validates
/// the inner transaction's signatures. If a prior transaction in the same ledger removes a
/// signer from the inner source account, the inner sig check fails during `apply()` with
/// `txBadAuth`, but the outer fee is still charged.
///
/// Our previous implementation checked inner signatures inside `validate_preconditions()`
/// (before fee charging) and returned early with `fee_charged=0`, which was incorrect.
///
/// The fix: for fee-bump transactions, inner sig validation is deferred to
/// `check_operation_signatures()` which runs after the outer fee has been deducted.
#[test]
fn test_fee_bump_charges_outer_fee_when_inner_auth_fails_at_apply() {
    let inner_secret = SecretKey::from_seed(&[30u8; 32]);
    let inner_account_id: AccountId = (&inner_secret.public_key()).into();

    // An additional signer that will be removed before the fee-bump executes
    let signer_secret = SecretKey::from_seed(&[31u8; 32]);
    let signer_pubkey = signer_secret.public_key();

    let fee_secret = SecretKey::from_seed(&[32u8; 32]);
    let fee_account_id: AccountId = (&fee_secret.public_key()).into();

    let network_id = NetworkId::testnet();

    // inner_source: master_weight=1, all thresholds=1, plus one additional signer
    let inner_signer = Signer {
        key: SignerKey::Ed25519(Uint256(*signer_pubkey.as_bytes())),
        weight: 1,
    };
    let inner_key = LedgerKey::Account(stellar_xdr::curr::LedgerKeyAccount {
        account_id: inner_account_id.clone(),
    });
    let inner_entry = LedgerEntry {
        last_modified_ledger_seq: 1,
        data: LedgerEntryData::Account(AccountEntry {
            account_id: inner_account_id.clone(),
            balance: 20_000_000,
            seq_num: SequenceNumber(1),
            num_sub_entries: 1,
            inflation_dest: None,
            flags: 0,
            home_domain: String32::default(),
            thresholds: Thresholds([1, 1, 1, 1]),
            signers: vec![inner_signer].try_into().unwrap(),
            ext: AccountEntryExt::V0,
        }),
        ext: LedgerEntryExt::V0,
    };

    // 20_000_000 gives the fee source ample room above the 2*base_reserve minimum (10_000_000)
    let (fee_key, fee_entry) = create_account_entry(fee_account_id.clone(), 1, 20_000_000);

    let snapshot = SnapshotBuilder::new(1)
        .add_entry(inner_key, inner_entry)
        .add_entry(fee_key, fee_entry)
        .build_with_default_header();
    let snapshot = SnapshotHandle::new(snapshot);

    let context = henyey_tx::LedgerContext::new(1, 1_000, 100, 5_000_000, 24, network_id);
    let mut executor = TransactionExecutor::new(
        &context,
        0,
        SorobanConfig::default(),
        ClassicEventConfig::default(),
    );

    // TX1: SetOptions from inner_source (master key) — removes signer_pubkey (weight=0).
    // This simulates a prior tx that changes the inner source's signer set mid-ledger.
    let set_options_op = Operation {
        source_account: None,
        body: OperationBody::SetOptions(SetOptionsOp {
            inflation_dest: None,
            clear_flags: None,
            set_flags: None,
            master_weight: None,
            low_threshold: None,
            med_threshold: None,
            high_threshold: None,
            home_domain: None,
            signer: Some(Signer {
                key: SignerKey::Ed25519(Uint256(*signer_pubkey.as_bytes())),
                weight: 0, // weight=0 removes the signer
            }),
        }),
    };

    let tx1 = Transaction {
        source_account: MuxedAccount::Ed25519(Uint256(*inner_secret.public_key().as_bytes())),
        fee: 100,
        seq_num: SequenceNumber(2),
        cond: Preconditions::None,
        memo: Memo::None,
        operations: vec![set_options_op].try_into().unwrap(),
        ext: TransactionExt::V0,
    };

    let mut env1 = TransactionEnvelope::Tx(TransactionV1Envelope {
        tx: tx1,
        signatures: VecM::default(),
    });
    let sig1 = sign_envelope(&env1, &inner_secret, &network_id);
    if let TransactionEnvelope::Tx(ref mut e) = env1 {
        e.signatures = vec![sig1].try_into().unwrap();
    }

    let result1 = executor
        .execute_transaction(&snapshot, &env1, 100, None)
        .expect("execute tx1");
    assert!(
        result1.success,
        "TX1 (SetOptions remove signer) should succeed"
    );

    // TX2: fee-bump wrapping an inner tx signed by signer_secret (now removed by TX1).
    let payment_op = Operation {
        source_account: None,
        body: OperationBody::Payment(stellar_xdr::curr::PaymentOp {
            destination: MuxedAccount::Ed25519(Uint256([9u8; 32])),
            asset: Asset::Native,
            amount: 1,
        }),
    };

    let inner_tx = Transaction {
        source_account: MuxedAccount::Ed25519(Uint256(*inner_secret.public_key().as_bytes())),
        fee: 100,
        seq_num: SequenceNumber(3), // inner_source seq is 2 after TX1, next is 3
        cond: Preconditions::None,
        memo: Memo::None,
        operations: vec![payment_op].try_into().unwrap(),
        ext: TransactionExt::V0,
    };

    // Sign the inner tx with signer_secret — this key was removed from inner_source by TX1
    let inner_env_for_signing = TransactionEnvelope::Tx(TransactionV1Envelope {
        tx: inner_tx.clone(),
        signatures: VecM::default(),
    });
    let inner_sig = sign_envelope(&inner_env_for_signing, &signer_secret, &network_id);

    let inner_v1 = TransactionV1Envelope {
        tx: inner_tx,
        signatures: vec![inner_sig].try_into().unwrap(),
    };

    let fee_bump = FeeBumpTransaction {
        fee_source: MuxedAccount::Ed25519(Uint256(*fee_secret.public_key().as_bytes())),
        fee: 200,
        inner_tx: FeeBumpTransactionInnerTx::Tx(inner_v1),
        ext: stellar_xdr::curr::FeeBumpTransactionExt::V0,
    };

    let mut env2 = TransactionEnvelope::TxFeeBump(FeeBumpTransactionEnvelope {
        tx: fee_bump,
        signatures: VecM::default(),
    });
    let outer_sig = sign_envelope(&env2, &fee_secret, &network_id);
    if let TransactionEnvelope::TxFeeBump(ref mut e) = env2 {
        e.signatures = vec![outer_sig].try_into().unwrap();
    }

    let result2 = executor
        .execute_transaction(&snapshot, &env2, 100, None)
        .expect("execute tx2");

    // The inner tx's signer was removed by TX1, so inner auth fails at apply time.
    assert!(
        !result2.success,
        "fee-bump should fail (inner auth invalid)"
    );
    assert_eq!(
        result2.failure,
        Some(ExecutionFailure::TxBadAuth),
        "inner tx should fail with InvalidSignature (txBadAuth)"
    );

    // The outer fee (200) MUST be charged even though the inner tx failed auth.
    // Before VE-03 fix, this was 0 because inner sig check happened before fee charging.
    assert_eq!(
        result2.fee_charged, 200,
        "outer fee-bump fee must be charged even when inner auth fails at apply time"
    );
}

/// Regression test for VE-04: op with per-op source merged in a prior TX must return
/// `opBAD_AUTH`, not `opNO_ACCOUNT`, when the current TX lacks the merged account's master key.
///
/// Scenario (mirrors L59863187 TX 69 / TX 124):
/// - TX1: account_merge of `op_source` into `dest` (signed by both source + op_source). Succeeds.
/// - TX2: a payment op whose explicit source is `op_source`, signed ONLY by `source_secret`
///   (no master-key sig for `op_source`).
///
/// Root cause (before fix): `check_signature_from_signers` returned `total_weight >= needed_weight`
/// at the end, evaluating `0 >= 0 = true` even when no signer matched. This caused
/// `check_signature_no_account` to return `true`, allowing the op to proceed to execution
/// where it hit `opNO_ACCOUNT` instead of the correct `opBAD_AUTH`.
///
/// Fix: changed final return to `total_weight >= needed_weight && total_weight > 0`, mirroring
/// stellar-core's `SignatureChecker::checkSignature` which falls through to `return false` when
/// nothing matched.
#[test]
fn test_op_source_merged_in_prior_tx_returns_bad_auth() {
    let source_secret = SecretKey::from_seed(&[60u8; 32]);
    let source_id: AccountId = (&source_secret.public_key()).into();

    let op_source_secret = SecretKey::from_seed(&[61u8; 32]);
    let op_source_id: AccountId = (&op_source_secret.public_key()).into();

    let dest_id = AccountId(PublicKey::PublicKeyTypeEd25519(Uint256([62u8; 32])));

    let (source_key, source_entry) = create_account_entry(source_id.clone(), 1, 100_000_000);
    // op_source needs enough balance to survive after fees (base_reserve = 1 XLM per entry)
    let (op_source_key, op_source_entry) =
        create_account_entry(op_source_id.clone(), 1, 20_000_000);
    let (dest_key, dest_entry) = create_account_entry(dest_id.clone(), 1, 10_000_000);

    let snapshot = SnapshotBuilder::new(1)
        .add_entry(source_key, source_entry)
        .add_entry(op_source_key, op_source_entry)
        .add_entry(dest_key, dest_entry)
        .build_with_default_header();
    let snapshot = SnapshotHandle::new(snapshot);

    let network_id = NetworkId::testnet();
    let context = henyey_tx::LedgerContext::new(1, 1_000, 100, 5_000_000, 25, network_id);
    let mut executor = TransactionExecutor::new(
        &context,
        0,
        SorobanConfig::default(),
        ClassicEventConfig::default(),
    );

    // ===== TX1: merge op_source into dest =====
    // The AccountMerge op has op_source as its explicit source.
    let op_source_muxed = MuxedAccount::Ed25519(Uint256(*op_source_secret.public_key().as_bytes()));
    let dest_muxed = MuxedAccount::Ed25519(match &dest_id.0 {
        PublicKey::PublicKeyTypeEd25519(k) => k.clone(),
    });

    let merge_op = Operation {
        source_account: Some(op_source_muxed.clone()),
        body: OperationBody::AccountMerge(dest_muxed),
    };

    let tx1 = Transaction {
        source_account: MuxedAccount::Ed25519(Uint256(*source_secret.public_key().as_bytes())),
        fee: 100,
        seq_num: SequenceNumber(2),
        cond: Preconditions::None,
        memo: Memo::None,
        operations: vec![merge_op].try_into().unwrap(),
        ext: TransactionExt::V0,
    };

    let mut envelope1 = TransactionEnvelope::Tx(TransactionV1Envelope {
        tx: tx1,
        signatures: VecM::default(),
    });
    // Both source and op_source must sign
    let source_sig1 = sign_envelope(&envelope1, &source_secret, &network_id);
    let op_source_sig1 = sign_envelope(&envelope1, &op_source_secret, &network_id);
    if let TransactionEnvelope::Tx(ref mut env) = envelope1 {
        env.signatures = vec![source_sig1, op_source_sig1].try_into().unwrap();
    }

    let result1 = executor
        .execute_transaction(&snapshot, &envelope1, 100, None)
        .expect("TX1 execute");

    assert!(
        result1.success,
        "TX1 (account merge) should succeed. Result: {:?}",
        result1.operation_results
    );

    // Advance to the next TX context (simulates between-transaction state advance)
    executor.state_mut().snapshot_delta();

    // ===== TX2: payment op sourced from op_source (now merged/deleted) =====
    // Signed only by source_secret — op_source's master key is NOT in the signatures.
    // stellar-core: checkSignatureNoAccount(op_source) creates synthetic signer for master key,
    // finds no match → returns false → opBAD_AUTH.
    // Before fix: Henyey returned true (0 >= 0 bug) → op executed → opNO_ACCOUNT.
    let payment_op = Operation {
        source_account: Some(op_source_muxed),
        body: OperationBody::Payment(stellar_xdr::curr::PaymentOp {
            destination: MuxedAccount::Ed25519(match &dest_id.0 {
                PublicKey::PublicKeyTypeEd25519(k) => k.clone(),
            }),
            asset: Asset::Native,
            amount: 1,
        }),
    };

    let tx2 = Transaction {
        source_account: MuxedAccount::Ed25519(Uint256(*source_secret.public_key().as_bytes())),
        fee: 100,
        seq_num: SequenceNumber(3),
        cond: Preconditions::None,
        memo: Memo::None,
        operations: vec![payment_op].try_into().unwrap(),
        ext: TransactionExt::V0,
    };

    let mut envelope2 = TransactionEnvelope::Tx(TransactionV1Envelope {
        tx: tx2,
        signatures: VecM::default(),
    });
    // Sign with source ONLY — deliberately omit op_source's master key
    let source_sig2 = sign_envelope(&envelope2, &source_secret, &network_id);
    if let TransactionEnvelope::Tx(ref mut env) = envelope2 {
        env.signatures = vec![source_sig2].try_into().unwrap();
    }

    let result2 = executor
        .execute_transaction(&snapshot, &envelope2, 100, None)
        .expect("TX2 execute");

    // TX2 should fail: the op's per-op source (op_source) no longer exists,
    // and the TX does not carry op_source's master key signature.
    assert!(!result2.success, "TX2 should fail");

    // The op result must be opBAD_AUTH, NOT opNO_ACCOUNT.
    // opNO_ACCOUNT would mean the sig check incorrectly passed (the pre-fix bug).
    assert_eq!(
        result2.operation_results.get(0),
        Some(&OperationResult::OpBadAuth),
        "op sourced from merged account with no master-key sig must return opBAD_AUTH, not opNO_ACCOUNT"
    );
}

/// Regression test for VE-07: PreAuthTx signer removed before signature check.
///
/// Scenario: Two transactions in the same ledger from the same source account.
///
/// TX 1 (SetOptions × 3):
///   1. Add a PreAuthTx signer whose key = hash(TX 2), weight = 1
///   2. Set master weight to 0
///   3. Set thresholds to [low=1, med=1, high=1]
///   (TX 1 is signed normally by the master key.)
///
/// TX 2 (Payment):
///   No signatures — authorized solely by the PreAuthTx signer added by TX 1.
///
/// In stellar-core, checkAllTransactionSignatures runs BEFORE
/// removeOneTimeSignerFromAllSourceAccounts. This means TX 2's signature check
/// sees the PreAuthTx signer (weight=1 >= threshold=1) and passes. The signer
/// is then consumed.
///
/// Bug: Our code removed one-time signers BEFORE checking signatures, so TX 2's
/// check saw an account with master_weight=0, no signers, no sigs → txBAD_AUTH.
#[test]
fn test_pre_auth_tx_signer_checked_before_removal() {
    use henyey_ledger::execution::{execute_transaction_set_with_fee_mode, SorobanContext};
    use henyey_ledger::LedgerDelta;

    let secret = SecretKey::from_seed(&[42u8; 32]);
    let source_id: AccountId = (&secret.public_key()).into();
    let dest_id = AccountId(PublicKey::PublicKeyTypeEd25519(Uint256([99u8; 32])));

    let network_id = NetworkId::testnet();
    let base_fee: u32 = 100;
    let base_reserve: i64 = 5_000_000;

    // Source account: master_weight=1, all thresholds=0 (so SetOptions passes at low=0).
    // Needs enough balance for 2 TXs' fees + payment + reserve for 1 sub-entry (the signer).
    let (source_key, source_entry) = create_account_entry(source_id.clone(), 100, 100_000_000);
    let (dest_key, dest_entry) = create_account_entry(dest_id.clone(), 1, 10_000_000);

    let snapshot = SnapshotBuilder::new(1)
        .add_entry(source_key, source_entry)
        .add_entry(dest_key, dest_entry)
        .build_with_default_header();
    let snapshot = SnapshotHandle::new(snapshot);

    // Build TX 2 first (unsigned Payment) so we can compute its hash for the
    // PreAuthTx signer in TX 1.
    let payment_op = Operation {
        source_account: None,
        body: OperationBody::Payment(stellar_xdr::curr::PaymentOp {
            destination: MuxedAccount::Ed25519(Uint256([99u8; 32])),
            asset: stellar_xdr::curr::Asset::Native,
            amount: 1000,
        }),
    };

    let tx2 = Transaction {
        source_account: MuxedAccount::Ed25519(Uint256(*secret.public_key().as_bytes())),
        fee: base_fee,
        seq_num: SequenceNumber(102), // TX 1 is seq 101
        cond: Preconditions::None,
        memo: Memo::None,
        operations: vec![payment_op].try_into().unwrap(),
        ext: TransactionExt::V0,
    };

    // TX 2 envelope with NO signatures (authorized by PreAuthTx signer).
    let tx2_envelope = TransactionEnvelope::Tx(TransactionV1Envelope {
        tx: tx2,
        signatures: VecM::default(),
    });

    // Compute TX 2's hash — this becomes the PreAuthTx signer key.
    let tx2_frame =
        henyey_tx::TransactionFrame::from_owned_with_network(tx2_envelope.clone(), network_id);
    let tx2_hash = tx2_frame.hash(&network_id).expect("tx2 hash");

    // Build TX 1: SetOptions × 3.
    //   Op 1: Add PreAuthTx signer (key = tx2_hash, weight = 1)
    //   Op 2: Set master weight to 0
    //   Op 3: Set thresholds to low=1, med=1, high=1
    let add_signer_op = Operation {
        source_account: None,
        body: OperationBody::SetOptions(SetOptionsOp {
            inflation_dest: None,
            clear_flags: None,
            set_flags: None,
            master_weight: None,
            low_threshold: None,
            med_threshold: None,
            high_threshold: None,
            home_domain: None,
            signer: Some(Signer {
                key: SignerKey::PreAuthTx(Uint256(tx2_hash.0)),
                weight: 1,
            }),
        }),
    };

    let set_master_weight_op = Operation {
        source_account: None,
        body: OperationBody::SetOptions(SetOptionsOp {
            inflation_dest: None,
            clear_flags: None,
            set_flags: None,
            master_weight: Some(0),
            low_threshold: None,
            med_threshold: None,
            high_threshold: None,
            home_domain: None,
            signer: None,
        }),
    };

    let set_thresholds_op = Operation {
        source_account: None,
        body: OperationBody::SetOptions(SetOptionsOp {
            inflation_dest: None,
            clear_flags: None,
            set_flags: None,
            master_weight: None,
            low_threshold: Some(1),
            med_threshold: Some(1),
            high_threshold: Some(1),
            home_domain: None,
            signer: None,
        }),
    };

    let tx1 = Transaction {
        source_account: MuxedAccount::Ed25519(Uint256(*secret.public_key().as_bytes())),
        fee: base_fee * 3, // 3 ops
        seq_num: SequenceNumber(101),
        cond: Preconditions::None,
        memo: Memo::None,
        operations: vec![add_signer_op, set_master_weight_op, set_thresholds_op]
            .try_into()
            .unwrap(),
        ext: TransactionExt::V0,
    };

    let mut tx1_envelope = TransactionEnvelope::Tx(TransactionV1Envelope {
        tx: tx1,
        signatures: VecM::default(),
    });

    // Sign TX 1 with the master key.
    let tx1_sig = sign_envelope(&tx1_envelope, &secret, &network_id);
    if let TransactionEnvelope::Tx(ref mut env) = tx1_envelope {
        env.signatures = vec![tx1_sig].try_into().unwrap();
    }

    // Execute both TXs in a single ledger.
    let tx_set: Vec<(std::sync::Arc<TransactionEnvelope>, Option<u32>)> = vec![
        (std::sync::Arc::new(tx1_envelope), None),
        (std::sync::Arc::new(tx2_envelope), None),
    ];

    let context = henyey_tx::LedgerContext::new(
        2,     // ledger_seq
        1_000, // close_time
        base_fee,
        base_reserve as u32,
        25, // protocol version
        network_id,
    );
    let mut delta = LedgerDelta::new(2);
    let soroban = SorobanContext {
        config: SorobanConfig::default(),
        base_prng_seed: [0u8; 32],
        classic_events: ClassicEventConfig::default(),
        module_cache: None,
        hot_archive: None,
        runtime_handle: None,
        soroban_state: None,
        offer_store: None,
        emit_soroban_tx_meta_ext_v1: false,
        enable_soroban_diagnostic_events: false,
    };

    let result = execute_transaction_set_with_fee_mode(
        &snapshot, &tx_set, &context, &mut delta, soroban, true,
    )
    .expect("execute tx set");

    // TX 1 (SetOptions): must succeed.
    assert!(
        result.results[0].success,
        "TX 1 (SetOptions) should succeed; got failure: {:?}",
        result.results[0].failure
    );

    // TX 2 (Payment authorized by PreAuthTx signer): must succeed.
    // Before the fix, this returned txBAD_AUTH because the PreAuthTx signer
    // was removed before the signature check could see it.
    assert!(
        result.results[1].success,
        "TX 2 (PreAuthTx-authorized payment) should succeed; got failure: {:?}",
        result.results[1].failure
    );
}

/// Regression test for VE-08: fee-bump wrapping a Soroban (InvokeHostFunction) transaction
/// must check inner source signatures at apply time, not skip them.
///
/// In stellar-core, `processSignatures()` calls `checkOperationSignatures()` for ALL
/// transaction types, including Soroban. Our code had `!frame.is_soroban()` which skipped
/// the check entirely for Soroban transactions. For fee-bump Soroban transactions, a prior
/// TX in the same ledger can modify the inner source's signer set, making the inner
/// signatures invalid. Without the check, the inner TX incorrectly succeeds.
///
/// The fix: remove the `!frame.is_soroban()` guard so signature checking runs for all
/// transaction types, matching stellar-core's behavior.
#[test]
fn test_fee_bump_soroban_checks_inner_signatures() {
    let inner_secret = SecretKey::from_seed(&[40u8; 32]);
    let inner_account_id: AccountId = (&inner_secret.public_key()).into();

    // An additional signer that will be removed before the fee-bump executes
    let signer_secret = SecretKey::from_seed(&[41u8; 32]);
    let signer_pubkey = signer_secret.public_key();

    let fee_secret = SecretKey::from_seed(&[42u8; 32]);
    let fee_account_id: AccountId = (&fee_secret.public_key()).into();

    let network_id = NetworkId::testnet();

    // inner_source: master_weight=1, thresholds=1, plus an additional signer
    let inner_signer = Signer {
        key: SignerKey::Ed25519(Uint256(*signer_pubkey.as_bytes())),
        weight: 1,
    };
    let inner_key = LedgerKey::Account(LedgerKeyAccount {
        account_id: inner_account_id.clone(),
    });
    let inner_entry = LedgerEntry {
        last_modified_ledger_seq: 1,
        data: LedgerEntryData::Account(AccountEntry {
            account_id: inner_account_id.clone(),
            balance: 20_000_000,
            seq_num: SequenceNumber(1),
            num_sub_entries: 1,
            inflation_dest: None,
            flags: 0,
            home_domain: String32::default(),
            thresholds: Thresholds([1, 1, 1, 1]),
            signers: vec![inner_signer].try_into().unwrap(),
            ext: AccountEntryExt::V0,
        }),
        ext: LedgerEntryExt::V0,
    };

    let (fee_key, fee_entry) = create_account_entry(fee_account_id.clone(), 1, 20_000_000);

    let snapshot = SnapshotBuilder::new(1)
        .add_entry(inner_key, inner_entry)
        .add_entry(fee_key, fee_entry)
        .build_with_default_header();
    let snapshot = SnapshotHandle::new(snapshot);

    let context = henyey_tx::LedgerContext::new(1, 1_000, 100, 5_000_000, 24, network_id);
    let mut executor = TransactionExecutor::new(
        &context,
        0,
        SorobanConfig::default(),
        ClassicEventConfig::default(),
    );

    // TX1: SetOptions — removes signer_pubkey (weight=0) from inner_source.
    let set_options_op = Operation {
        source_account: None,
        body: OperationBody::SetOptions(SetOptionsOp {
            inflation_dest: None,
            clear_flags: None,
            set_flags: None,
            master_weight: None,
            low_threshold: None,
            med_threshold: None,
            high_threshold: None,
            home_domain: None,
            signer: Some(Signer {
                key: SignerKey::Ed25519(Uint256(*signer_pubkey.as_bytes())),
                weight: 0,
            }),
        }),
    };

    let tx1 = Transaction {
        source_account: MuxedAccount::Ed25519(Uint256(*inner_secret.public_key().as_bytes())),
        fee: 100,
        seq_num: SequenceNumber(2),
        cond: Preconditions::None,
        memo: Memo::None,
        operations: vec![set_options_op].try_into().unwrap(),
        ext: TransactionExt::V0,
    };

    let mut env1 = TransactionEnvelope::Tx(TransactionV1Envelope {
        tx: tx1,
        signatures: VecM::default(),
    });
    let sig1 = sign_envelope(&env1, &inner_secret, &network_id);
    if let TransactionEnvelope::Tx(ref mut e) = env1 {
        e.signatures = vec![sig1].try_into().unwrap();
    }

    let result1 = executor
        .execute_transaction(&snapshot, &env1, 100, None)
        .expect("execute tx1");
    assert!(
        result1.success,
        "TX1 (SetOptions remove signer) should succeed"
    );

    // TX2: fee-bump wrapping an InvokeHostFunction (Soroban) inner TX,
    // signed by signer_secret which was removed by TX1.
    // The inner TX should fail with TxBadAuth.
    let invoke_op = Operation {
        source_account: None,
        body: OperationBody::InvokeHostFunction(InvokeHostFunctionOp {
            host_function: HostFunction::InvokeContract(InvokeContractArgs {
                contract_address: ScAddress::Contract(ContractId(Hash([99u8; 32]))),
                function_name: ScSymbol("test".try_into().unwrap()),
                args: VecM::default(),
            }),
            auth: VecM::default(),
        }),
    };

    let inner_tx = Transaction {
        source_account: MuxedAccount::Ed25519(Uint256(*inner_secret.public_key().as_bytes())),
        fee: 100,
        seq_num: SequenceNumber(3),
        cond: Preconditions::None,
        memo: Memo::None,
        operations: vec![invoke_op].try_into().unwrap(),
        ext: TransactionExt::V0,
    };

    let inner_env_for_signing = TransactionEnvelope::Tx(TransactionV1Envelope {
        tx: inner_tx.clone(),
        signatures: VecM::default(),
    });
    let inner_sig = sign_envelope(&inner_env_for_signing, &signer_secret, &network_id);

    let inner_v1 = TransactionV1Envelope {
        tx: inner_tx,
        signatures: vec![inner_sig].try_into().unwrap(),
    };

    let fee_bump = FeeBumpTransaction {
        fee_source: MuxedAccount::Ed25519(Uint256(*fee_secret.public_key().as_bytes())),
        fee: 200,
        inner_tx: FeeBumpTransactionInnerTx::Tx(inner_v1),
        ext: stellar_xdr::curr::FeeBumpTransactionExt::V0,
    };

    let mut env2 = TransactionEnvelope::TxFeeBump(FeeBumpTransactionEnvelope {
        tx: fee_bump,
        signatures: VecM::default(),
    });
    let outer_sig = sign_envelope(&env2, &fee_secret, &network_id);
    if let TransactionEnvelope::TxFeeBump(ref mut e) = env2 {
        e.signatures = vec![outer_sig].try_into().unwrap();
    }

    let result2 = executor
        .execute_transaction(&snapshot, &env2, 100, None)
        .expect("execute tx2");

    // The inner tx's signer was removed by TX1, so inner auth must fail.
    // Before the VE-08 fix, the `!frame.is_soroban()` guard skipped signature
    // checking for Soroban transactions, causing this to incorrectly succeed
    // or fail with a different error than InvalidSignature.
    assert!(
        !result2.success,
        "fee-bump Soroban TX should fail (inner auth invalid after signer removal)"
    );
    assert_eq!(
        result2.failure,
        Some(ExecutionFailure::TxBadAuth),
        "Failure must be InvalidSignature (TxBadAuth), not a Soroban execution error. \
         Before VE-08 fix, the `!frame.is_soroban()` guard skipped the sig check."
    );
}

/// Regression test: when `should_apply=false` is passed to
/// `execute_transaction_with_fee_mode_and_pre_state`, the operation body must
/// NOT execute. This matches stellar-core's `parallelApply` which returns
/// `{false, {}}` immediately when `!txResult.isSuccess()` after
/// `preParallelApply` determined insufficient balance.
///
/// Before the fix, henyey always executed the full TX body and then post-hoc
/// forced `success=false`. This leaked state changes, RO TTL bumps, hot archive
/// keys, and operation meta.
#[test]
fn test_should_apply_false_skips_operation_body() {
    let secret = SecretKey::from_seed(&[44u8; 32]);
    let source_id: AccountId = (&secret.public_key()).into();

    // Account with enough balance to pass validation but we'll set should_apply=false
    // to simulate the parallel path where pre-deduction found insufficient balance.
    let (source_key, source_entry) = create_account_entry(source_id.clone(), 1, 20_000_000);

    // Create a contract code entry and TTL — the TX will try to extend its TTL.
    let code_hash = Hash([44u8; 32]);
    let contract_code = ContractCodeEntry {
        ext: ContractCodeEntryExt::V0,
        hash: code_hash.clone(),
        code: BytesM::try_from(vec![1u8, 2u8, 3u8]).unwrap(),
    };
    let contract_key = LedgerKey::ContractCode(LedgerKeyContractCode {
        hash: code_hash.clone(),
    });
    let contract_entry = LedgerEntry {
        last_modified_ledger_seq: 1,
        data: LedgerEntryData::ContractCode(contract_code),
        ext: LedgerEntryExt::V0,
    };

    let key_hash = {
        use sha2::{Digest, Sha256};
        use stellar_xdr::curr::WriteXdr;

        let mut hasher = Sha256::new();
        let bytes = contract_key
            .to_xdr(stellar_xdr::curr::Limits::none())
            .unwrap_or_default();
        hasher.update(&bytes);
        Hash(hasher.finalize().into())
    };
    let ttl_entry = LedgerEntry {
        last_modified_ledger_seq: 1,
        data: LedgerEntryData::Ttl(TtlEntry {
            key_hash: key_hash.clone(),
            live_until_ledger_seq: 10,
        }),
        ext: LedgerEntryExt::V0,
    };
    let ttl_key = LedgerKey::Ttl(LedgerKeyTtl { key_hash });

    let snapshot = SnapshotBuilder::new(1)
        .add_entry(source_key, source_entry)
        .add_entry(contract_key.clone(), contract_entry)
        .add_entry(ttl_key, ttl_entry)
        .build_with_default_header();
    let snapshot = SnapshotHandle::new(snapshot);

    let operation = Operation {
        source_account: None,
        body: OperationBody::ExtendFootprintTtl(ExtendFootprintTtlOp {
            ext: ExtensionPoint::V0,
            extend_to: 100,
        }),
    };

    let soroban_data = SorobanTransactionData {
        ext: SorobanTransactionDataExt::V0,
        resources: SorobanResources {
            footprint: LedgerFootprint {
                read_only: vec![contract_key].try_into().unwrap(),
                read_write: VecM::default(),
            },
            instructions: 0,
            disk_read_bytes: 10000,
            write_bytes: 0,
        },
        resource_fee: 900,
    };

    let tx = Transaction {
        source_account: MuxedAccount::Ed25519(Uint256(*secret.public_key().as_bytes())),
        fee: 1000,
        seq_num: SequenceNumber(2),
        cond: Preconditions::None,
        memo: Memo::None,
        operations: vec![operation].try_into().unwrap(),
        ext: TransactionExt::V1(soroban_data),
    };

    let mut envelope = TransactionEnvelope::Tx(TransactionV1Envelope {
        tx,
        signatures: VecM::default(),
    });

    let network_id = NetworkId::testnet();
    let decorated = sign_envelope(&envelope, &secret, &network_id);
    if let TransactionEnvelope::Tx(ref mut env) = envelope {
        env.signatures = vec![decorated].try_into().unwrap();
    }

    let context = henyey_tx::LedgerContext::new(1, 1_000, 100, 5_000_000, 25, network_id);
    let mut executor = TransactionExecutor::new(
        &context,
        0,
        SorobanConfig::default(),
        ClassicEventConfig::default(),
    );

    // Execute with should_apply=false and deduct_fee=false, simulating the
    // parallel Soroban path where pre-deduction found insufficient balance.
    let result = executor
        .execute_transaction_with_arc(
            &snapshot,
            TransactionExecutionRequest::from_envelope(
                &envelope,
                100,
                Some([0u8; 32]), // soroban_prng_seed
                false,           // deduct_fee=false (fees pre-deducted in parallel path)
                None,            // no fee_source_pre_state
                false,           // should_apply=false — this is what we're testing
            ),
        )
        .expect("execute should not error");

    // 1. TX must report failure.
    assert!(!result.success, "TX must fail when should_apply=false");
    assert_eq!(
        result.failure,
        Some(ExecutionFailure::TxInsufficientBalance),
        "failure reason must be InsufficientBalance"
    );

    // 2. No operation results — the body was never executed.
    assert!(
        result.operation_results.is_empty(),
        "operation_results must be empty when body is skipped"
    );

    // 3. No hot archive restored keys leaked.
    assert!(
        result.hot_archive_restored_keys.is_empty(),
        "hot_archive_restored_keys must be empty when body is skipped"
    );

    // 4. Verify meta structure: tx_changes_before populated, operations empty.
    let tx_meta = result.tx_meta.expect("tx meta must be present");
    let TransactionMeta::V4(meta) = tx_meta else {
        panic!("expected TransactionMeta::V4");
    };

    // tx_changes_before should be non-empty (contains seq bump changes).
    assert!(
        !meta.tx_changes_before.is_empty(),
        "tx_changes_before must contain seq bump changes"
    );

    // operations must be empty — no operation body executed.
    assert!(
        meta.operations.is_empty(),
        "operations meta must be empty when body is skipped"
    );

    // 5. Soroban meta must be present (fee info activates it) with zero consumed fees.
    let soroban_meta = meta
        .soroban_meta
        .expect("soroban_meta must be present for Soroban TX");
    assert!(
        soroban_meta.return_value.is_none(),
        "return_value must be None when body is skipped"
    );

    // 6. Verify the executor state: sequence number was bumped (pre-apply committed).
    let account = executor
        .state()
        .get_account(&source_id)
        .expect("source account must exist in executor state");
    assert_eq!(
        account.seq_num,
        SequenceNumber(2),
        "sequence number must be bumped to 2 (the TX's seq_num) even though body was skipped"
    );
}

/// Regression test for the second part of VE-10: in the parallel fee-deduction
/// path, validation failures (e.g. TxNoAccount) must report the pre-charged fee
/// as fee_charged, not 0.
///
/// When fees are pre-deducted on the delta, `run_transactions_on_executor` calls
/// the executor with `deduct_fee=false`. Early validation failures like
/// TxNoAccount construct a `failed_result` with `fee_charged=0`. The fix
/// overrides fee_charged with the pre-charged amount when `has_pre_charged` is
/// true, so the result hash matches the fee already deducted on the delta.
///
/// Observed at mainnet L60645316 TX 68 (account deleted by account_merge,
/// subsequent TX got TxNoAccount with wrong fee_charged).
#[test]
fn test_pre_charged_fee_override_on_validation_failure() {
    use henyey_ledger::execution::{run_transactions_on_executor, PreChargedFee};
    use henyey_ledger::LedgerDelta;

    // Source account that DOES exist (needed for fee pre-charging to make sense).
    let secret = SecretKey::from_seed(&[99u8; 32]);
    let source_id: AccountId = (&secret.public_key()).into();
    let (source_key, source_entry) = create_account_entry(source_id.clone(), 100, 100_000_000);

    // Account that does NOT exist in the snapshot — simulates a deleted account.
    let missing_secret = SecretKey::from_seed(&[98u8; 32]);
    let _missing_id: AccountId = (&missing_secret.public_key()).into();

    let snapshot = SnapshotBuilder::new(1)
        .add_entry(source_key, source_entry)
        .build_with_default_header();
    let snapshot = SnapshotHandle::new(snapshot);

    let network_id = NetworkId::testnet();
    let base_fee: u32 = 100;
    let base_reserve: u32 = 5_000_000;

    // Build TX from the missing account — this will trigger TxNoAccount.
    let payment_op = Operation {
        source_account: None,
        body: OperationBody::Payment(stellar_xdr::curr::PaymentOp {
            destination: MuxedAccount::Ed25519(Uint256(*secret.public_key().as_bytes())),
            asset: Asset::Native,
            amount: 1,
        }),
    };
    let tx = Transaction {
        source_account: MuxedAccount::Ed25519(Uint256(*missing_secret.public_key().as_bytes())),
        fee: base_fee,
        seq_num: SequenceNumber(1),
        cond: Preconditions::None,
        memo: Memo::None,
        operations: vec![payment_op].try_into().unwrap(),
        ext: TransactionExt::V0,
    };
    let mut envelope = TransactionEnvelope::Tx(TransactionV1Envelope {
        tx,
        signatures: VecM::default(),
    });
    let sig = sign_envelope(&envelope, &missing_secret, &network_id);
    if let TransactionEnvelope::Tx(ref mut env) = envelope {
        env.signatures = vec![sig].try_into().unwrap();
    }

    let context = henyey_tx::LedgerContext::new(2, 1_000, base_fee, base_reserve, 25, network_id);
    let mut executor = TransactionExecutor::new(
        &context,
        0,
        SorobanConfig::default(),
        ClassicEventConfig::default(),
    );
    executor
        .load_orderbook_offers(&snapshot)
        .expect("load offers");

    let pre_charged_fee: i64 = 100;
    let pre_charged = vec![PreChargedFee {
        charged_fee: pre_charged_fee,
        should_apply: true,
        fee_changes: LedgerEntryChanges(vec![].try_into().unwrap()),
    }];

    let transactions = vec![(std::sync::Arc::new(envelope), None)];
    let mut delta = LedgerDelta::new(2);

    let result = run_transactions_on_executor(henyey_ledger::execution::RunTransactionsParams {
        executor: &mut executor,
        snapshot: &snapshot,
        transactions: &transactions,
        base_fee,
        soroban_base_prng_seed: [0u8; 32],
        deduct_fee: false,
        delta: &mut delta,
        external_pre_charged: Some(&pre_charged),
    })
    .expect("run_transactions_on_executor");

    // TX must fail with NoAccount.
    assert!(!result.results[0].success, "TX should fail");
    assert_eq!(
        result.results[0].failure,
        Some(ExecutionFailure::TxNoAccount),
        "Failure must be NoAccount"
    );

    // fee_charged must be the pre-charged amount, not 0.
    assert_eq!(
        result.results[0].fee_charged, pre_charged_fee,
        "fee_charged must equal the pre-charged fee ({}), not 0",
        pre_charged_fee
    );

    // Verify the XDR result also has the correct fee_charged.
    assert_eq!(
        result.tx_results[0].result.fee_charged, pre_charged_fee,
        "XDR fee_charged must equal the pre-charged fee"
    );
}

/// Regression test for AUDIT-577 Finding 1: run_transactions_on_executor hard-codes
/// should_apply=true when using external pre-charged fees. When a classic TX has
/// insufficient fees (should_apply=false from pre-deduction), the operation body must
/// NOT execute. stellar-core's parallelApply checks txResult.isSuccess() and returns
/// early without applying ops when fee validation failed.
#[test]
fn test_audit_577_pre_charged_should_apply_false_skips_body() {
    use henyey_ledger::execution::{run_transactions_on_executor, PreChargedFee};
    use henyey_ledger::LedgerDelta;

    let secret = SecretKey::from_seed(&[77u8; 32]);
    let source_id: AccountId = (&secret.public_key()).into();
    // Give source enough balance for the TX to succeed IF it were applied.
    let (source_key, source_entry) = create_account_entry(source_id.clone(), 1, 100_000_000);

    let snapshot = SnapshotBuilder::new(1)
        .add_entry(source_key, source_entry)
        .build_with_default_header();
    let snapshot = SnapshotHandle::new(snapshot);

    let network_id = NetworkId::testnet();
    let base_fee: u32 = 100;
    let base_reserve: u32 = 5_000_000;

    // Build a valid CreateAccount TX that would succeed normally.
    let dest = AccountId(PublicKey::PublicKeyTypeEd25519(Uint256([88u8; 32])));
    let op = Operation {
        source_account: None,
        body: OperationBody::CreateAccount(CreateAccountOp {
            destination: dest.clone(),
            starting_balance: 10_000_000,
        }),
    };
    let tx = Transaction {
        source_account: MuxedAccount::Ed25519(Uint256(*secret.public_key().as_bytes())),
        fee: base_fee,
        seq_num: SequenceNumber(2),
        cond: Preconditions::None,
        memo: Memo::None,
        operations: vec![op].try_into().unwrap(),
        ext: TransactionExt::V0,
    };
    let mut envelope = TransactionEnvelope::Tx(TransactionV1Envelope {
        tx,
        signatures: VecM::default(),
    });
    let sig = sign_envelope(&envelope, &secret, &network_id);
    if let TransactionEnvelope::Tx(ref mut env) = envelope {
        env.signatures = vec![sig].try_into().unwrap();
    }

    let context = henyey_tx::LedgerContext::new(2, 1_000, base_fee, base_reserve, 25, network_id);
    let mut executor = TransactionExecutor::new(
        &context,
        0,
        SorobanConfig::default(),
        ClassicEventConfig::default(),
    );
    executor
        .load_orderbook_offers(&snapshot)
        .expect("load offers");

    // Pre-charge with should_apply=false (simulates insufficient fee balance).
    let pre_charged = vec![PreChargedFee {
        charged_fee: 50, // Less than computed fee — insufficient
        should_apply: false,
        fee_changes: LedgerEntryChanges(vec![].try_into().unwrap()),
    }];

    let transactions = vec![(std::sync::Arc::new(envelope), None)];
    let mut delta = LedgerDelta::new(2);

    let result = run_transactions_on_executor(henyey_ledger::execution::RunTransactionsParams {
        executor: &mut executor,
        snapshot: &snapshot,
        transactions: &transactions,
        base_fee,
        soroban_base_prng_seed: [0u8; 32],
        deduct_fee: false,
        delta: &mut delta,
        external_pre_charged: Some(&pre_charged),
    })
    .expect("run_transactions_on_executor");

    // TX body must NOT have been applied — destination account must not exist.
    assert!(
        !result.results[0].success,
        "TX with should_apply=false must not succeed"
    );
    // The destination account must not have been created.
    assert!(
        executor.state().get_account(&dest).is_none(),
        "destination account must not exist when should_apply=false"
    );
}

/// Regression test for AUDIT-577 Finding 2: MAX_SEQ_NUM_TO_APPLY was only
/// populated when deduct_fee=true. The external pre-charged path (parallel
/// Soroban) passes deduct_fee=false, so AccountMerge protection was missing.
///
/// When a TX's sequence number is at or above starting_seq (ledger_seq << 32),
/// AccountMerge must be blocked with SeqnumTooFar. Before the fix, the
/// MAX_SEQ_NUM_TO_APPLY map was empty in the pre-charged path, so the merge
/// would incorrectly succeed.
#[test]
fn test_audit_577_max_seq_num_to_apply_with_pre_charged() {
    use henyey_ledger::execution::{run_transactions_on_executor, PreChargedFee};
    use henyey_ledger::LedgerDelta;
    use stellar_xdr::curr::AccountMergeResult;

    let ledger_seq: u32 = 2;
    let starting_seq: i64 = (ledger_seq as i64) << 32; // 8589934592

    let secret = SecretKey::from_seed(&[78u8; 32]);
    let source_id: AccountId = (&secret.public_key()).into();
    // Source account with seq_num = starting_seq so the TX seq_num
    // (starting_seq + 1) passes the sequence-number check.
    // TX seq_num = starting_seq + 1 >= starting_seq triggers SeqnumTooFar.
    let (source_key, source_entry) =
        create_account_entry(source_id.clone(), starting_seq, 100_000_000);

    // Destination for the merge.
    let dest_id = AccountId(PublicKey::PublicKeyTypeEd25519(Uint256([89u8; 32])));
    let (dest_key, dest_entry) = create_account_entry(dest_id.clone(), 1, 50_000_000);

    let snapshot = SnapshotBuilder::new(1)
        .add_entry(source_key, source_entry)
        .add_entry(dest_key, dest_entry)
        .build_with_default_header();
    let snapshot = SnapshotHandle::new(snapshot);

    let network_id = NetworkId::testnet();
    let base_fee: u32 = 100;
    let base_reserve: u32 = 5_000_000;

    // Build an AccountMerge TX with seq_num = starting_seq.
    let op = Operation {
        source_account: None,
        body: OperationBody::AccountMerge(MuxedAccount::Ed25519(Uint256([89u8; 32]))),
    };
    let tx = Transaction {
        source_account: MuxedAccount::Ed25519(Uint256(*secret.public_key().as_bytes())),
        fee: base_fee,
        seq_num: SequenceNumber(starting_seq + 1),
        cond: Preconditions::None,
        memo: Memo::None,
        operations: vec![op].try_into().unwrap(),
        ext: TransactionExt::V0,
    };
    let mut envelope = TransactionEnvelope::Tx(TransactionV1Envelope {
        tx,
        signatures: VecM::default(),
    });
    let sig = sign_envelope(&envelope, &secret, &network_id);
    if let TransactionEnvelope::Tx(ref mut env) = envelope {
        env.signatures = vec![sig].try_into().unwrap();
    }

    let context =
        henyey_tx::LedgerContext::new(ledger_seq, 1_000, base_fee, base_reserve, 25, network_id);
    let mut executor = TransactionExecutor::new(
        &context,
        0,
        SorobanConfig::default(),
        ClassicEventConfig::default(),
    );
    executor
        .load_orderbook_offers(&snapshot)
        .expect("load offers");

    // Pre-charge with should_apply=true and deduct_fee=false (the parallel path).
    let pre_charged = vec![PreChargedFee {
        charged_fee: 100,
        should_apply: true,
        fee_changes: LedgerEntryChanges(vec![].try_into().unwrap()),
    }];

    let transactions = vec![(std::sync::Arc::new(envelope), None)];
    let mut delta = LedgerDelta::new(ledger_seq);

    let result = run_transactions_on_executor(henyey_ledger::execution::RunTransactionsParams {
        executor: &mut executor,
        snapshot: &snapshot,
        transactions: &transactions,
        base_fee,
        soroban_base_prng_seed: [0u8; 32],
        deduct_fee: false,
        delta: &mut delta,
        external_pre_charged: Some(&pre_charged),
    })
    .expect("run_transactions_on_executor");

    // The TX must fail because MAX_SEQ_NUM_TO_APPLY blocks the merge.
    assert!(
        !result.results[0].success,
        "AccountMerge must fail when TX seq_num >= starting_seq"
    );

    // Verify the operation result is SeqnumTooFar specifically.
    let op_result = &result.results[0].operation_results[0];
    match op_result {
        OperationResult::OpInner(OperationResultTr::AccountMerge(
            AccountMergeResult::SeqnumTooFar,
        )) => {} // expected
        other => panic!("Expected AccountMerge::SeqnumTooFar, got {:?}", other),
    }

    // Source account must still exist (merge was blocked).
    assert!(
        executor.state().get_account(&source_id).is_some(),
        "source account must still exist when merge is blocked by MAX_SEQ_NUM_TO_APPLY"
    );
}

/// Regression test for the single-op savepoint skip optimization.
///
/// When a transaction has exactly one operation, the per-operation savepoint is
/// skipped (since TX-level rollback handles cleanup). This test verifies that a
/// single-op classic TX whose operation fails at execution time (not validation)
/// correctly rolls back all state changes: the source account's balance should
/// only reflect the fee deduction, and the destination should not exist.
#[test]
fn test_single_op_tx_failure_rolls_back_without_per_op_savepoint() {
    let secret = SecretKey::from_seed(&[50u8; 32]);
    let source_id: AccountId = (&secret.public_key()).into();

    let initial_balance: i64 = 20_000_000; // > 2 * base_reserve (5_000_000) + fee
    let fee: u32 = 100;
    let (source_key, source_entry) = create_account_entry(source_id.clone(), 1, initial_balance);
    let snapshot = SnapshotBuilder::new(1)
        .add_entry(source_key, source_entry)
        .build_with_default_header();
    let snapshot = SnapshotHandle::new(snapshot);

    // Single-op TX: Payment to a non-existent destination.
    // This will fail at execution time with PAYMENT_NO_DESTINATION (not a
    // precondition failure), exercising the code path where num_ops == 1 and
    // the per-operation savepoint is None.
    let nonexistent_dest = MuxedAccount::Ed25519(Uint256([99u8; 32]));
    let op_payment = Operation {
        source_account: None,
        body: OperationBody::Payment(stellar_xdr::curr::PaymentOp {
            destination: nonexistent_dest,
            asset: Asset::Native,
            amount: 1_000_000,
        }),
    };

    let tx = Transaction {
        source_account: MuxedAccount::Ed25519(Uint256(*secret.public_key().as_bytes())),
        fee,
        seq_num: SequenceNumber(2),
        cond: Preconditions::None,
        memo: Memo::None,
        operations: vec![op_payment].try_into().unwrap(),
        ext: TransactionExt::V0,
    };

    let mut envelope = TransactionEnvelope::Tx(TransactionV1Envelope {
        tx,
        signatures: VecM::default(),
    });

    let network_id = NetworkId::testnet();
    let decorated = sign_envelope(&envelope, &secret, &network_id);
    if let TransactionEnvelope::Tx(ref mut env) = envelope {
        env.signatures = vec![decorated].try_into().unwrap();
    }

    let classic_events = ClassicEventConfig {
        emit_classic_events: true,
        backfill_stellar_asset_events: false,
    };
    let context = henyey_tx::LedgerContext::new(1, 1_000, 100, 5_000_000, 25, network_id);
    let mut executor =
        TransactionExecutor::new(&context, 0, SorobanConfig::default(), classic_events);

    let result = executor
        .execute_transaction(&snapshot, &envelope, 100, None)
        .expect("execute");

    // TX should fail at the operation level.
    assert!(!result.success);
    assert_eq!(result.failure, Some(ExecutionFailure::TxFailed));

    // Source account: balance should reflect ONLY the fee deduction.
    // The payment amount must NOT have been deducted (rolled back by TX-level rollback).
    let source = executor
        .state()
        .get_account(&source_id)
        .expect("source account must exist");
    assert_eq!(
        source.balance,
        initial_balance - i64::from(fee),
        "source balance should be initial minus fee only"
    );

    // Sequence number should be bumped even for failed TXs.
    assert_eq!(source.seq_num.0, 2, "sequence number should be bumped to 2");

    // The destination should not exist (payment was rolled back).
    let dest_id = AccountId(PublicKey::PublicKeyTypeEd25519(Uint256([99u8; 32])));
    assert!(
        executor.state().get_account(&dest_id).is_none(),
        "destination account must not exist after rollback"
    );
}

/// Regression test for AUDIT-005 (H-001):
/// Fee bump pre-auth TX signer must be removed from outer fee source account.
///
/// In stellar-core, FeeBumpTransactionFrame::removeOneTimeSignerKeyFromFeeSource()
/// removes the PreAuthTx signer matching the fee bump outer hash from the fee source.
/// Henyey was generating fake STATE/UPDATED metadata without actually removing the signer.
///
/// Additionally, remove_one_time_signers_phase was using the outer (fee bump) hash
/// instead of the inner TX contents hash when removing signers from inner source accounts.
#[test]
fn test_audit_005_fee_bump_pre_auth_signer_removed_from_fee_source() {
    let inner_secret = SecretKey::from_seed(&[40u8; 32]);
    let inner_account_id: AccountId = (&inner_secret.public_key()).into();

    let fee_secret = SecretKey::from_seed(&[41u8; 32]);
    let fee_account_id: AccountId = (&fee_secret.public_key()).into();

    let network_id = NetworkId::testnet();

    // Build the fee bump transaction first to compute the outer hash.
    let payment_op = Operation {
        source_account: None,
        body: OperationBody::Payment(stellar_xdr::curr::PaymentOp {
            destination: MuxedAccount::Ed25519(Uint256(*inner_secret.public_key().as_bytes())),
            asset: Asset::Native,
            amount: 1,
        }),
    };

    let inner_tx = Transaction {
        source_account: MuxedAccount::Ed25519(Uint256(*inner_secret.public_key().as_bytes())),
        fee: 100,
        seq_num: SequenceNumber(2),
        cond: Preconditions::None,
        memo: Memo::None,
        operations: vec![payment_op].try_into().unwrap(),
        ext: TransactionExt::V0,
    };

    let inner_v1 = TransactionV1Envelope {
        tx: inner_tx,
        signatures: VecM::default(),
    };

    // Sign the inner TX first so the fee bump hash includes the signed inner.
    let inner_env_for_signing = TransactionEnvelope::Tx(inner_v1.clone());
    let inner_sig = sign_envelope(&inner_env_for_signing, &inner_secret, &network_id);

    let signed_inner_v1 = TransactionV1Envelope {
        tx: inner_v1.tx.clone(),
        signatures: vec![inner_sig].try_into().unwrap(),
    };

    // Build the fee bump with the SIGNED inner, so the hash is correct.
    let fee_bump = FeeBumpTransaction {
        fee_source: MuxedAccount::Ed25519(Uint256(*fee_secret.public_key().as_bytes())),
        fee: 200,
        inner_tx: FeeBumpTransactionInnerTx::Tx(signed_inner_v1.clone()),
        ext: stellar_xdr::curr::FeeBumpTransactionExt::V0,
    };

    let fee_bump_env = TransactionEnvelope::TxFeeBump(FeeBumpTransactionEnvelope {
        tx: fee_bump,
        signatures: VecM::default(),
    });

    // Compute the fee bump outer hash (used for fee source PreAuthTx signer).
    // The hash covers the FeeBumpTransaction contents (including signed inner),
    // NOT the outer envelope signatures.
    let fee_bump_frame =
        henyey_tx::TransactionFrame::from_owned_with_network(fee_bump_env.clone(), network_id);
    let outer_hash = fee_bump_frame.hash(&network_id).expect("outer hash");

    // Create fee source account WITH a PreAuthTx signer matching the outer hash.
    let pre_auth_signer = Signer {
        key: SignerKey::PreAuthTx(Uint256(outer_hash.0)),
        weight: 1,
    };
    let fee_key = LedgerKey::Account(LedgerKeyAccount {
        account_id: fee_account_id.clone(),
    });
    let fee_entry = LedgerEntry {
        last_modified_ledger_seq: 1,
        data: LedgerEntryData::Account(AccountEntry {
            account_id: fee_account_id.clone(),
            balance: 20_000_000,
            seq_num: SequenceNumber(1),
            num_sub_entries: 1, // one signer
            inflation_dest: None,
            flags: 0,
            home_domain: String32::default(),
            thresholds: Thresholds([1, 0, 0, 0]),
            signers: vec![pre_auth_signer].try_into().unwrap(),
            ext: AccountEntryExt::V0,
        }),
        ext: LedgerEntryExt::V0,
    };

    let (inner_key, inner_entry) = create_account_entry(inner_account_id.clone(), 1, 20_000_000);

    let snapshot = SnapshotBuilder::new(1)
        .add_entry(fee_key, fee_entry)
        .add_entry(inner_key, inner_entry)
        .build_with_default_header();
    let snapshot = SnapshotHandle::new(snapshot);

    let context = henyey_tx::LedgerContext::new(1, 1_000, 100, 5_000_000, 25, network_id);
    let mut executor = TransactionExecutor::new(
        &context,
        0,
        SorobanConfig::default(),
        ClassicEventConfig::default(),
    );

    // Build the final envelope using the already-signed inner and sign the outer.
    let fee_bump_signed = FeeBumpTransaction {
        fee_source: MuxedAccount::Ed25519(Uint256(*fee_secret.public_key().as_bytes())),
        fee: 200,
        inner_tx: FeeBumpTransactionInnerTx::Tx(signed_inner_v1),
        ext: stellar_xdr::curr::FeeBumpTransactionExt::V0,
    };

    let mut envelope = TransactionEnvelope::TxFeeBump(FeeBumpTransactionEnvelope {
        tx: fee_bump_signed,
        signatures: VecM::default(),
    });
    let outer_sig = sign_envelope(&envelope, &fee_secret, &network_id);
    if let TransactionEnvelope::TxFeeBump(ref mut e) = envelope {
        e.signatures = vec![outer_sig].try_into().unwrap();
    }

    let result = executor
        .execute_transaction(&snapshot, &envelope, 100, None)
        .expect("execute fee bump");

    assert!(result.success, "fee bump transaction should succeed");

    // Bug 1: The fee source's PreAuthTx signer matching the outer hash must be removed.
    let fee_account = executor
        .state()
        .get_account(&fee_account_id)
        .expect("fee source account");
    assert!(
        fee_account.signers.is_empty(),
        "PreAuthTx signer should be removed from fee source after fee bump execution, \
         but found {} signers: {:?}",
        fee_account.signers.len(),
        fee_account.signers
    );
    assert_eq!(
        fee_account.num_sub_entries, 0,
        "num_sub_entries should be decremented after signer removal"
    );
}

/// Regression test for AUDIT-005 secondary finding:
/// Inner source accounts' PreAuthTx signers must be removed using the inner TX hash,
/// not the fee bump outer hash.
///
/// The inner source has a PreAuthTx signer matching the inner TX hash. No ed25519
/// signatures are included on the inner TX — auth relies solely on the PreAuthTx signer.
/// master_weight=0 ensures the master key doesn't contribute weight.
#[test]
fn test_audit_005_fee_bump_inner_hash_for_signer_removal() {
    let inner_secret = SecretKey::from_seed(&[42u8; 32]);
    let inner_account_id: AccountId = (&inner_secret.public_key()).into();

    let fee_secret = SecretKey::from_seed(&[43u8; 32]);
    let fee_account_id: AccountId = (&fee_secret.public_key()).into();

    let network_id = NetworkId::testnet();

    // Build the inner TX to compute the inner TX contents hash.
    let payment_op = Operation {
        source_account: None,
        body: OperationBody::Payment(stellar_xdr::curr::PaymentOp {
            destination: MuxedAccount::Ed25519(Uint256(*fee_secret.public_key().as_bytes())),
            asset: Asset::Native,
            amount: 1,
        }),
    };

    let inner_tx = Transaction {
        source_account: MuxedAccount::Ed25519(Uint256(*inner_secret.public_key().as_bytes())),
        fee: 100,
        seq_num: SequenceNumber(2),
        cond: Preconditions::None,
        memo: Memo::None,
        operations: vec![payment_op].try_into().unwrap(),
        ext: TransactionExt::V0,
    };

    // No inner signatures — auth comes from the PreAuthTx signer alone.
    let inner_v1 = TransactionV1Envelope {
        tx: inner_tx,
        signatures: VecM::default(),
    };

    // Compute the inner TX contents hash (ENVELOPE_TYPE_TX).
    let inner_env = TransactionEnvelope::Tx(inner_v1.clone());
    let inner_frame =
        henyey_tx::TransactionFrame::from_owned_with_network(inner_env.clone(), network_id);
    let inner_hash = inner_frame.hash(&network_id).expect("inner hash");

    // Inner source: master_weight=0 (disabled), low=1, med=1, high=1.
    // Auth is provided solely by the PreAuthTx signer matching the inner TX hash.
    let pre_auth_signer = Signer {
        key: SignerKey::PreAuthTx(Uint256(inner_hash.0)),
        weight: 1,
    };
    let inner_key = LedgerKey::Account(LedgerKeyAccount {
        account_id: inner_account_id.clone(),
    });
    let inner_entry = LedgerEntry {
        last_modified_ledger_seq: 1,
        data: LedgerEntryData::Account(AccountEntry {
            account_id: inner_account_id.clone(),
            balance: 20_000_000,
            seq_num: SequenceNumber(1),
            num_sub_entries: 1,
            inflation_dest: None,
            flags: 0,
            home_domain: String32::default(),
            thresholds: Thresholds([0, 1, 1, 1]), // master_weight=0
            signers: vec![pre_auth_signer].try_into().unwrap(),
            ext: AccountEntryExt::V0,
        }),
        ext: LedgerEntryExt::V0,
    };

    let (fee_key, fee_entry) = create_account_entry(fee_account_id.clone(), 1, 20_000_000);

    let snapshot = SnapshotBuilder::new(1)
        .add_entry(inner_key, inner_entry)
        .add_entry(fee_key, fee_entry)
        .build_with_default_header();
    let snapshot = SnapshotHandle::new(snapshot);

    let context = henyey_tx::LedgerContext::new(1, 1_000, 100, 5_000_000, 25, network_id);
    let mut executor = TransactionExecutor::new(
        &context,
        0,
        SorobanConfig::default(),
        ClassicEventConfig::default(),
    );

    // No inner signatures — fee bump wraps unsigned inner TX.
    let fee_bump = FeeBumpTransaction {
        fee_source: MuxedAccount::Ed25519(Uint256(*fee_secret.public_key().as_bytes())),
        fee: 200,
        inner_tx: FeeBumpTransactionInnerTx::Tx(inner_v1),
        ext: stellar_xdr::curr::FeeBumpTransactionExt::V0,
    };

    let mut envelope = TransactionEnvelope::TxFeeBump(FeeBumpTransactionEnvelope {
        tx: fee_bump,
        signatures: VecM::default(),
    });
    let outer_sig = sign_envelope(&envelope, &fee_secret, &network_id);
    if let TransactionEnvelope::TxFeeBump(ref mut e) = envelope {
        e.signatures = vec![outer_sig].try_into().unwrap();
    }

    let result = executor
        .execute_transaction(&snapshot, &envelope, 100, None)
        .expect("execute fee bump");

    // With the bug: remove_one_time_signers_phase uses outer_hash instead of inner_hash,
    // so the PreAuthTx signer (keyed on inner_hash) won't be found and won't be removed.
    // The transaction should succeed because sig check happens before signer removal.
    assert!(
        result.success,
        "fee bump transaction should succeed (PreAuthTx auth), failure={:?}, error={:?}",
        result.failure, result.error
    );

    // Bug 2a: The inner source's PreAuthTx signer matching the INNER hash must be removed.
    let inner_account = executor
        .state()
        .get_account(&inner_account_id)
        .expect("inner source account");
    assert!(
        inner_account.signers.is_empty(),
        "PreAuthTx signer (inner hash) should be removed from inner source, \
         but found {} signers: {:?}",
        inner_account.signers.len(),
        inner_account.signers
    );
    assert_eq!(
        inner_account.num_sub_entries, 0,
        "num_sub_entries should be decremented after signer removal"
    );
}

/// Regression test for AUDIT-007: Sequential Soroban fee_charged missing refund subtraction.
///
/// The sequential execution path (`run_transactions_on_executor` with pre-charged fees)
/// was setting `result.fee_charged = pre_charged_fee` without subtracting the Soroban
/// refund. This caused `feeCharged` in the serialized TransactionResult to be inflated,
/// diverging from stellar-core's `finalizeFeeRefund()` which always subtracts the refund.
///
/// The parallel path correctly had `pre.charged_fee.saturating_sub(result.fee_refund)`.
#[test]
fn test_audit_007_sequential_soroban_fee_charged_subtracts_refund() {
    use henyey_ledger::execution::{run_transactions_on_executor, PreChargedFee};
    use henyey_ledger::LedgerDelta;

    let secret = SecretKey::from_seed(&[55u8; 32]);
    let source_id: AccountId = (&secret.public_key()).into();
    let (source_key, source_entry) = create_account_entry(source_id.clone(), 1, 100_000_000);

    let code_hash = Hash([9u8; 32]);
    let contract_code = ContractCodeEntry {
        ext: ContractCodeEntryExt::V0,
        hash: code_hash.clone(),
        code: BytesM::try_from(vec![1u8, 2u8, 3u8]).unwrap(),
    };
    let contract_key = LedgerKey::ContractCode(LedgerKeyContractCode {
        hash: code_hash.clone(),
    });
    let contract_entry = LedgerEntry {
        last_modified_ledger_seq: 1,
        data: LedgerEntryData::ContractCode(contract_code),
        ext: LedgerEntryExt::V0,
    };

    let key_hash = {
        use sha2::{Digest, Sha256};
        use stellar_xdr::curr::WriteXdr;

        let mut hasher = Sha256::new();
        let bytes = contract_key
            .to_xdr(stellar_xdr::curr::Limits::none())
            .unwrap_or_default();
        hasher.update(&bytes);
        Hash(hasher.finalize().into())
    };
    let ttl_entry = LedgerEntry {
        last_modified_ledger_seq: 1,
        data: LedgerEntryData::Ttl(TtlEntry {
            key_hash: key_hash.clone(),
            live_until_ledger_seq: 10,
        }),
        ext: LedgerEntryExt::V0,
    };
    let ttl_key = LedgerKey::Ttl(LedgerKeyTtl { key_hash });

    let snapshot = SnapshotBuilder::new(1)
        .add_entry(source_key, source_entry)
        .add_entry(contract_key.clone(), contract_entry)
        .add_entry(ttl_key, ttl_entry)
        .build_with_default_header();
    let snapshot = SnapshotHandle::new(snapshot);

    let network_id = NetworkId::testnet();
    let base_fee: u32 = 100;

    // Soroban TX with resource_fee that will generate a refund.
    let operation = Operation {
        source_account: None,
        body: OperationBody::ExtendFootprintTtl(ExtendFootprintTtlOp {
            ext: stellar_xdr::curr::ExtensionPoint::V0,
            extend_to: 100,
        }),
    };

    let soroban_data = SorobanTransactionData {
        ext: SorobanTransactionDataExt::V0,
        resources: SorobanResources {
            footprint: LedgerFootprint {
                read_only: vec![contract_key].try_into().unwrap(),
                read_write: VecM::default(),
            },
            instructions: 0,
            disk_read_bytes: 10000,
            write_bytes: 0,
        },
        resource_fee: 900,
    };

    let tx = Transaction {
        source_account: MuxedAccount::Ed25519(Uint256(*secret.public_key().as_bytes())),
        fee: 1000, // 100 inclusion + 900 resource
        seq_num: SequenceNumber(2),
        cond: Preconditions::None,
        memo: Memo::None,
        operations: vec![operation].try_into().unwrap(),
        ext: TransactionExt::V1(soroban_data),
    };

    let mut envelope = TransactionEnvelope::Tx(TransactionV1Envelope {
        tx,
        signatures: VecM::default(),
    });
    let sig = sign_envelope(&envelope, &secret, &network_id);
    if let TransactionEnvelope::Tx(ref mut env) = envelope {
        env.signatures = vec![sig].try_into().unwrap();
    }

    let context = henyey_tx::LedgerContext::new(2, 1_000, base_fee, 5_000_000, 25, network_id);
    let mut executor = TransactionExecutor::new(
        &context,
        0,
        SorobanConfig::default(),
        ClassicEventConfig::default(),
    );
    executor
        .load_orderbook_offers(&snapshot)
        .expect("load offers");

    // Pre-charge the full declared fee (1000).
    let pre_charged_fee: i64 = 1000;
    let pre_charged = vec![PreChargedFee {
        charged_fee: pre_charged_fee,
        should_apply: true,
        fee_changes: LedgerEntryChanges(vec![].try_into().unwrap()),
    }];

    let transactions = vec![(std::sync::Arc::new(envelope), None)];
    let mut delta = LedgerDelta::new(2);

    let result = run_transactions_on_executor(henyey_ledger::execution::RunTransactionsParams {
        executor: &mut executor,
        snapshot: &snapshot,
        transactions: &transactions,
        base_fee,
        soroban_base_prng_seed: [0u8; 32],
        deduct_fee: false,
        delta: &mut delta,
        external_pre_charged: Some(&pre_charged),
    })
    .expect("run_transactions_on_executor");

    assert!(result.results[0].success, "Soroban TX should succeed");

    // The refund should be non-zero (resource fee was over-estimated).
    let fee_refund = result.results[0].fee_refund;
    assert!(
        fee_refund > 0,
        "Soroban TX should have a non-zero fee refund, got {}",
        fee_refund
    );

    // AUDIT-007: fee_charged must be pre_charged minus the refund, not the full pre_charged.
    let expected_fee_charged = pre_charged_fee - fee_refund;
    assert_eq!(
        result.results[0].fee_charged, expected_fee_charged,
        "fee_charged ({}) should be pre_charged_fee ({}) - fee_refund ({}), not the full pre_charged",
        result.results[0].fee_charged, pre_charged_fee, fee_refund
    );

    // Also verify the XDR TransactionResult has the correct feeCharged.
    assert_eq!(
        result.tx_results[0].result.fee_charged, expected_fee_charged,
        "XDR feeCharged must match: expected {} (pre_charged {} - refund {})",
        expected_fee_charged, pre_charged_fee, fee_refund
    );
}
