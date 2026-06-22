use henyey_common::entry_to_key;
use henyey_common::normalize_transaction_meta;
use henyey_crypto::{seed, xdr_compute_hash};
use stellar_xdr::{
    AccountEntry, AccountEntryExt, ExtensionPoint, LedgerEntry, LedgerEntryChange,
    LedgerEntryChanges, LedgerEntryData, LedgerEntryExt, OperationMetaV2, PublicKey, String32,
    Thresholds, TransactionMeta, TransactionMetaV4, Uint256, VecM,
};

fn account_entry(id_byte: u8, balance: i64) -> LedgerEntry {
    let account_id = PublicKey::PublicKeyTypeEd25519(Uint256([id_byte; 32]));
    let account = AccountEntry {
        account_id: stellar_xdr::AccountId(account_id),
        balance,
        seq_num: stellar_xdr::SequenceNumber(1),
        num_sub_entries: 0,
        inflation_dest: None,
        flags: 0,
        home_domain: String32::default(),
        thresholds: Thresholds([1, 0, 0, 0]),
        signers: VecM::default(),
        ext: AccountEntryExt::V0,
    };

    LedgerEntry {
        last_modified_ledger_seq: 1,
        data: LedgerEntryData::Account(account),
        ext: LedgerEntryExt::V0,
    }
}

fn ledger_entry_changes(changes: Vec<LedgerEntryChange>) -> LedgerEntryChanges {
    LedgerEntryChanges(changes.try_into().unwrap_or_default())
}

fn tx_meta_with_changes(
    tx_changes_before: LedgerEntryChanges,
    op_changes: Vec<LedgerEntryChanges>,
) -> TransactionMeta {
    let operations: Vec<OperationMetaV2> = op_changes
        .into_iter()
        .map(|changes| OperationMetaV2 {
            ext: ExtensionPoint::V0,
            changes,
            events: VecM::default(),
        })
        .collect();

    TransactionMeta::V4(TransactionMetaV4 {
        ext: ExtensionPoint::V0,
        tx_changes_before,
        operations: operations.try_into().unwrap_or_default(),
        tx_changes_after: LedgerEntryChanges(VecM::default()),
        soroban_meta: None,
        events: VecM::default(),
        diagnostic_events: VecM::default(),
    })
}

fn tx_meta_hash(meta: &TransactionMeta) -> u64 {
    let mut meta = meta.clone();
    normalize_transaction_meta(&mut meta).expect("normalize tx meta");
    xdr_compute_hash(&meta).expect("hash tx meta")
}

#[test]
fn tx_meta_hash_vectors() {
    seed(7).expect("seed short hash");

    let entry_a = account_entry(0x11, 100);
    let entry_b = account_entry(0x22, 200);

    let tx_changes_before = ledger_entry_changes(vec![
        LedgerEntryChange::Updated(entry_b.clone()),
        LedgerEntryChange::Created(entry_a.clone()),
    ]);

    let op_changes = vec![
        ledger_entry_changes(vec![
            LedgerEntryChange::Removed(entry_to_key(&entry_a)),
            LedgerEntryChange::Updated(entry_b.clone()),
        ]),
        ledger_entry_changes(vec![LedgerEntryChange::Created(entry_b.clone())]),
    ];

    let meta_one = tx_meta_with_changes(tx_changes_before, op_changes);
    let meta_two = tx_meta_with_changes(
        ledger_entry_changes(vec![LedgerEntryChange::Created(entry_b)]),
        vec![ledger_entry_changes(vec![LedgerEntryChange::Created(
            entry_a,
        )])],
    );

    let got = vec![tx_meta_hash(&meta_one), tx_meta_hash(&meta_two)];
    let expected = vec![16722020423653793170, 2868878751995401193];
    assert_eq!(got, expected);
}

#[test]
#[ignore = "used to regenerate expected vectors"]
fn dump_tx_meta_hash_vectors() {
    seed(7).expect("seed short hash");

    let entry_a = account_entry(0x11, 100);
    let entry_b = account_entry(0x22, 200);

    let tx_changes_before = ledger_entry_changes(vec![
        LedgerEntryChange::Updated(entry_b.clone()),
        LedgerEntryChange::Created(entry_a.clone()),
    ]);

    let op_changes = vec![
        ledger_entry_changes(vec![
            LedgerEntryChange::Removed(entry_to_key(&entry_a)),
            LedgerEntryChange::Updated(entry_b.clone()),
        ]),
        ledger_entry_changes(vec![LedgerEntryChange::Created(entry_b.clone())]),
    ];

    let meta_one = tx_meta_with_changes(tx_changes_before, op_changes);
    let meta_two = tx_meta_with_changes(
        ledger_entry_changes(vec![LedgerEntryChange::Created(entry_b)]),
        vec![ledger_entry_changes(vec![LedgerEntryChange::Created(
            entry_a,
        )])],
    );

    println!(
        "{:?}",
        vec![tx_meta_hash(&meta_one), tx_meta_hash(&meta_two)]
    );
}
