//! Isolated, read-only Classic transaction execution.
//!
//! This API is **Classic-only**: envelopes containing any Soroban operation
//! (`InvokeHostFunction`, `ExtendFootprintTtl`, `RestoreFootprint`) are
//! rejected with [`IsolatedClassicSimulationError::SorobanUnsupported`]. Real
//! Soroban execution requires the live network `SorobanConfig`, module cache,
//! and per-transaction PRNG seed, none of which are available here; executing
//! with placeholders would produce plausible-but-wrong results.
//!
//! # Exactness model
//!
//! The simulated transaction runs as if it were the next ledger after the
//! committed snapshot (LCL): `ledger_seq = LCL + 1`, `close_time =
//! LCL close_time + 1`, and the header's `base_fee`, unless overridden via
//! [`IsolatedClassicSimulationRequest::close_time`] /
//! [`IsolatedClassicSimulationRequest::base_fee`]. Fees are processed with the
//! same two-phase flow as a real ledger close (fee pre-charge, then body
//! execution with fees skipped), so `fee_charged` and the fee debit are exact
//! even when the body fails apply-time validation (e.g. `txBAD_SEQ`).
//!
//! Byte-identical meta parity with an event-emitting node additionally
//! requires passing that node's live classic-event flags via
//! [`IsolatedClassicSimulationRequest::classic_events`].

use std::collections::{BTreeMap, HashMap};
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::sync::Arc;

use henyey_common::{entry_to_key, Hash256, NetworkId};
use henyey_tx::state::offer_store::{ImmutableOfferStoreBase, OfferStore};
use henyey_tx::{ClassicEventConfig, LedgerContext, TransactionFrame};
use stellar_xdr::{
    LedgerEntry, LedgerEntryChange, LedgerEntryData, LedgerHeaderExt, LedgerKey, OperationResult,
    TransactionEnvelope, TransactionMeta, TransactionResultPair,
};

use super::{
    build_tx_result_pair, load_frozen_key_config, FeeMode, TransactionExecutionRequest,
    TransactionExecutor,
};
use crate::{LedgerError, SnapshotHandle};

/// One isolated transaction plus ledger entries to overlay on the committed snapshot.
///
/// Overrides are private to this simulation. They may create synthetic accounts,
/// mask committed entries, or replace committed entries without mutating canonical
/// state or caches owned by the source snapshot.
///
/// Offers are controlled **solely** via the frozen offer base supplied to
/// [`IsolatedClassicSimulationBase::prepare`] (plus the per-simulation overlay
/// the executor builds on top of it). `LedgerKey::Offer` keys in
/// `ledger_overrides` are rejected with
/// [`IsolatedClassicSimulationError::OfferOverrideUnsupported`] because offer
/// reads route exclusively through the offer store and would silently ignore
/// such overrides.
#[derive(Debug, Clone)]
pub struct IsolatedClassicSimulationRequest {
    /// Signed Classic envelope to execute. Soroban envelopes are rejected.
    pub envelope: TransactionEnvelope,
    /// Non-offer ledger entries to mask/replace/create over the snapshot.
    pub ledger_overrides: HashMap<LedgerKey, Option<LedgerEntry>>,
    /// Classic event emission flags. Meta parity with a live node requires
    /// passing that node's config (`emit_classic_events`,
    /// `backfill_stellar_asset_events`); the default (both false) matches a
    /// node with events disabled.
    pub classic_events: ClassicEventConfig,
    /// Close time of the simulated ledger. Defaults to the committed header's
    /// close time + 1 (the LCL+1 model). Pass the expected real close time to
    /// match time-bound (`maxTime`) validity of the actual next ledger.
    pub close_time: Option<u64>,
    /// Effective base fee of the simulated ledger. Defaults to the committed
    /// header's `base_fee`. Pass a surge-priced base fee to match
    /// `fee_charged` of the actual next ledger.
    pub base_fee: Option<u32>,
}

impl IsolatedClassicSimulationRequest {
    /// Request with no overrides and default (LCL+1, events-off) parameters.
    pub fn new(envelope: TransactionEnvelope) -> Self {
        Self {
            envelope,
            ledger_overrides: HashMap::new(),
            classic_events: ClassicEventConfig::default(),
            close_time: None,
            base_fee: None,
        }
    }
}

/// One ledger key's final simulated state relative to the committed snapshot.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NetLedgerChange {
    pub key: LedgerKey,
    pub before: Option<LedgerEntry>,
    pub after: Option<LedgerEntry>,
}

/// Exact result of executing one signed Classic transaction in isolation.
#[derive(Debug, Clone)]
pub struct IsolatedClassicSimulation {
    pub committed_ledger_hash: Hash256,
    pub committed_ledger: u32,
    pub target_ledger: u32,
    pub success: bool,
    pub error: Option<String>,
    pub fee_charged: i64,
    pub result: TransactionResultPair,
    pub operation_results: Vec<OperationResult>,
    pub transaction_meta: Option<TransactionMeta>,
    pub net_ledger_changes: Vec<NetLedgerChange>,
}

/// Instrumentation for one prepared simulation base.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct IsolatedClassicSimulationStats {
    pub preparations: u64,
    pub frozen_offers: usize,
    pub simulations: u64,
    pub max_transaction_overlay_offers: usize,
}

/// Exact immutable offer base and committed snapshot reused by simulations.
///
/// The caller supplies the complete, metadata-preserving offer entries that may
/// be crossed. This keeps route selection and offer-index policy outside henyey
/// while allowing many simulations to share one frozen base cheaply.
pub struct IsolatedClassicSimulationBase {
    snapshot: SnapshotHandle,
    network_id: NetworkId,
    offers: Arc<ImmutableOfferStoreBase>,
    frozen_offers: usize,
    simulations: AtomicU64,
    max_transaction_overlay_offers: AtomicUsize,
}

#[derive(Debug, thiserror::Error)]
pub enum IsolatedClassicSimulationError {
    #[error("committed ledger sequence cannot be advanced")]
    LedgerSequenceOverflow,
    #[error("simulation offer set contains a non-offer ledger entry")]
    NonOfferEntry,
    #[error("duplicate offer id {0} in simulation offer set")]
    DuplicateOfferId(i64),
    #[error("offer id {0} exceeds the committed header id_pool {1}")]
    OfferIdExceedsIdPool(i64, u64),
    #[error("Soroban transactions are not supported by isolated Classic simulation")]
    SorobanUnsupported,
    #[error("offer ledger overrides are unsupported; supply offers via the frozen offer base")]
    OfferOverrideUnsupported,
    #[error(transparent)]
    Ledger(#[from] LedgerError),
}

/// Prepare and execute one isolated Classic transaction.
pub fn simulate_classic_transaction(
    snapshot: &SnapshotHandle,
    network_id: NetworkId,
    offer_entries: Vec<LedgerEntry>,
    request: &IsolatedClassicSimulationRequest,
) -> Result<IsolatedClassicSimulation, IsolatedClassicSimulationError> {
    IsolatedClassicSimulationBase::prepare(snapshot, network_id, offer_entries)?.simulate(request)
}

impl IsolatedClassicSimulationBase {
    /// Freeze the supplied complete offer records for reuse by many simulations.
    pub fn prepare(
        snapshot: &SnapshotHandle,
        network_id: NetworkId,
        offer_entries: Vec<LedgerEntry>,
    ) -> Result<Self, IsolatedClassicSimulationError> {
        snapshot
            .ledger_seq()
            .checked_add(1)
            .ok_or(IsolatedClassicSimulationError::LedgerSequenceOverflow)?;

        // Validate against the committed header's id_pool. The header is not a
        // ledger entry, so per-request `ledger_overrides` cannot change it —
        // the snapshot header is always the effective header. A synthetic
        // offer above id_pool would collide with the first offer a simulated
        // transaction creates (which allocates id_pool + 1).
        let id_pool = snapshot.header().id_pool;
        let mut frozen_offers = HashMap::with_capacity(offer_entries.len());
        for entry in offer_entries {
            let LedgerEntryData::Offer(offer) = &entry.data else {
                return Err(IsolatedClassicSimulationError::NonOfferEntry);
            };
            let offer_id = offer.offer_id;
            if offer_id < 0 || offer_id as u64 > id_pool {
                return Err(IsolatedClassicSimulationError::OfferIdExceedsIdPool(
                    offer_id, id_pool,
                ));
            }
            if frozen_offers.insert(offer_id, entry).is_some() {
                return Err(IsolatedClassicSimulationError::DuplicateOfferId(offer_id));
            }
        }
        let frozen_offer_count = frozen_offers.len();
        metrics::counter!("henyey_isolated_classic_base_preparations_total").increment(1);
        metrics::counter!("henyey_isolated_classic_base_offers_total")
            .increment(frozen_offer_count as u64);
        Ok(Self {
            snapshot: snapshot.clone(),
            network_id,
            offers: OfferStore::from_bucket_list_entries(frozen_offers).into_immutable_base(),
            frozen_offers: frozen_offer_count,
            simulations: AtomicU64::new(0),
            max_transaction_overlay_offers: AtomicUsize::new(0),
        })
    }

    /// Execute one signed envelope with a fresh ledger and offer overlay.
    ///
    /// The envelope must be Classic (no Soroban operations) and its fee source
    /// account must exist in the committed snapshot or `ledger_overrides`,
    /// mirroring the inclusion requirements of a real ledger close.
    pub fn simulate(
        &self,
        request: &IsolatedClassicSimulationRequest,
    ) -> Result<IsolatedClassicSimulation, IsolatedClassicSimulationError> {
        // Classic-only: Soroban execution requires the live network config,
        // module cache, and PRNG seed; placeholders would yield wrong results.
        if henyey_tx::envelope_utils::is_soroban_envelope(&request.envelope) {
            return Err(IsolatedClassicSimulationError::SorobanUnsupported);
        }
        // Offer reads route exclusively through the frozen offer base and the
        // per-simulation overlay; an offer ledger override would be silently
        // ignored, so reject it up front.
        if request
            .ledger_overrides
            .keys()
            .any(|key| matches!(key, LedgerKey::Offer(_)))
        {
            return Err(IsolatedClassicSimulationError::OfferOverrideUnsupported);
        }

        let target_ledger = self
            .snapshot
            .ledger_seq()
            .checked_add(1)
            .ok_or(IsolatedClassicSimulationError::LedgerSequenceOverflow)?;
        let overlay = self
            .snapshot
            .with_overrides(request.ledger_overrides.clone());
        let offer_store = Arc::new(parking_lot::Mutex::new(OfferStore::with_immutable_base(
            Arc::clone(&self.offers),
        )));

        let header = self.snapshot.header();
        let close_time = request
            .close_time
            .unwrap_or_else(|| header.scp_value.close_time.0.saturating_add(1));
        let base_fee = request.base_fee.unwrap_or(header.base_fee);
        let mut context = LedgerContext::new(
            target_ledger,
            close_time,
            base_fee,
            header.base_reserve,
            header.ledger_version,
            self.network_id,
        );
        context.ledger_flags = match &header.ext {
            LedgerHeaderExt::V0 => 0,
            LedgerHeaderExt::V1(ext) => ext.flags,
        };
        context.frozen_key_config = load_frozen_key_config(&overlay, header.ledger_version)?;

        let envelope = Arc::new(request.envelope.clone());
        let frame = TransactionFrame::with_network(Arc::clone(&envelope), self.network_id);
        let mut executor = TransactionExecutor::new(
            &context,
            header.id_pool,
            Default::default(),
            request.classic_events,
        );
        executor.set_offer_store(Arc::clone(&offer_store));

        // Mirror the production ledger-close flow (run_transactions_on_executor
        // with FeeStrategy::PreChargeInternally): pre-charge the fee for the
        // included transaction, then execute the body with FeeMode::Skip. This
        // keeps fee_charged and the fee debit exact even when the body fails
        // apply-time validation (e.g. txBAD_SEQ).
        let (fee_changes, charged_fee) =
            executor.process_fee_only(&overlay, &envelope, base_fee)?;
        executor.state.snapshot_delta();
        let mut execution = executor.execute_transaction_with_arc(
            &overlay,
            TransactionExecutionRequest {
                tx_envelope: Arc::clone(&envelope),
                base_fee,
                soroban_prng_seed: None,
                fee_mode: FeeMode::Skip,
            },
        )?;
        // With FeeMode::Skip, early validation failures report fee_charged=0.
        // Override with the pre-charged amount, exactly as tx_set.rs does.
        execution.fee_charged = charged_fee.saturating_sub(execution.fee_refund);
        execution.fee_changes = Some(fee_changes);
        let transaction_overlay_offers = offer_store.lock().overlay_len();
        self.simulations.fetch_add(1, Ordering::Relaxed);
        self.max_transaction_overlay_offers
            .fetch_max(transaction_overlay_offers, Ordering::Relaxed);
        metrics::histogram!("henyey_isolated_classic_transaction_overlay_offers")
            .record(transaction_overlay_offers as f64);

        let result = build_tx_result_pair(
            &frame,
            &self.network_id,
            &execution,
            base_fee as i64,
            header.ledger_version,
        )?;
        let net_ledger_changes = collect_net_changes(
            execution
                .fee_changes
                .as_ref()
                .map(|changes| changes.as_slice()),
            execution.tx_meta.as_ref(),
            execution
                .post_fee_changes
                .as_ref()
                .map(|changes| changes.as_slice()),
        );

        Ok(IsolatedClassicSimulation {
            committed_ledger_hash: self.snapshot.header_hash(),
            committed_ledger: self.snapshot.ledger_seq(),
            target_ledger,
            success: execution.success,
            error: execution.error,
            fee_charged: execution.fee_charged,
            result,
            operation_results: execution.operation_results,
            transaction_meta: execution.tx_meta,
            net_ledger_changes,
        })
    }

    pub fn stats(&self) -> IsolatedClassicSimulationStats {
        IsolatedClassicSimulationStats {
            preparations: 1,
            frozen_offers: self.frozen_offers,
            simulations: self.simulations.load(Ordering::Relaxed),
            max_transaction_overlay_offers: self
                .max_transaction_overlay_offers
                .load(Ordering::Relaxed),
        }
    }
}

fn collect_net_changes(
    fee: Option<&[LedgerEntryChange]>,
    meta: Option<&TransactionMeta>,
    post_fee: Option<&[LedgerEntryChange]>,
) -> Vec<NetLedgerChange> {
    let mut changes: BTreeMap<LedgerKey, (Option<LedgerEntry>, Option<LedgerEntry>)> =
        BTreeMap::new();
    let mut apply = |change: &LedgerEntryChange| match change {
        LedgerEntryChange::State(entry) => {
            let key = entry_to_key(entry);
            changes
                .entry(key)
                .or_insert_with(|| (Some(entry.clone()), Some(entry.clone())));
        }
        LedgerEntryChange::Created(entry) | LedgerEntryChange::Updated(entry) => {
            let key = entry_to_key(entry);
            changes.entry(key).or_insert((None, None)).1 = Some(entry.clone());
        }
        LedgerEntryChange::Removed(key) => {
            changes.entry(key.clone()).or_insert((None, None)).1 = None;
        }
        LedgerEntryChange::Restored(_) => {
            // Hot-archive restoration is Soroban-only; Soroban envelopes are
            // rejected in simulate(), so this arm is unreachable for isolated
            // Classic simulation. Keep it explicit rather than silently
            // dropping a change kind.
            debug_assert!(
                false,
                "Restored ledger change in Classic-only isolated simulation"
            );
        }
    };
    if let Some(fee) = fee {
        fee.iter().for_each(&mut apply);
    }
    if let Some(meta) = meta {
        henyey_common::meta_walk::for_each_change(std::slice::from_ref(meta), &mut apply);
    }
    if let Some(post_fee) = post_fee {
        post_fee.iter().for_each(&mut apply);
    }
    changes
        .into_iter()
        .filter_map(|(key, (before, after))| {
            (before != after).then_some(NetLedgerChange { key, before, after })
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use henyey_crypto::{sha256, SecretKey};
    use stellar_xdr::{
        AccountEntry, AccountEntryExt, AccountEntryExtensionV1, AccountEntryExtensionV1Ext,
        AccountId, AlphaNum4, Asset, AssetCode4, DecoratedSignature, LedgerEntryExt,
        LedgerKeyAccount, Liabilities, Memo, MuxedAccount, OfferEntry, OfferEntryExt, Operation,
        OperationBody, PathPaymentStrictSendOp, PathPaymentStrictSendResult, Preconditions, Price,
        PublicKey, SequenceNumber, Signature, SignatureHint, String32, Thresholds, Transaction,
        TransactionExt, TransactionV1Envelope, TrustLineAsset, TrustLineEntry, TrustLineEntryExt,
        TrustLineEntryV1, TrustLineEntryV1Ext, TrustLineFlags, Uint256, VecM,
    };

    use crate::SnapshotBuilder;

    fn id(seed: u8) -> AccountId {
        AccountId(PublicKey::PublicKeyTypeEd25519(Uint256([seed; 32])))
    }

    fn usd() -> Asset {
        Asset::CreditAlphanum4(AlphaNum4 {
            asset_code: AssetCode4(*b"USD\0"),
            issuer: id(9),
        })
    }

    fn account(
        account_id: AccountId,
        balance: i64,
        subentries: u32,
        buying: i64,
        selling: i64,
    ) -> LedgerEntry {
        LedgerEntry {
            last_modified_ledger_seq: 42,
            data: LedgerEntryData::Account(AccountEntry {
                account_id,
                balance,
                seq_num: SequenceNumber(0),
                num_sub_entries: subentries,
                inflation_dest: None,
                flags: 0,
                home_domain: String32::default(),
                thresholds: Thresholds([1, 1, 1, 1]),
                signers: VecM::default(),
                ext: AccountEntryExt::V1(AccountEntryExtensionV1 {
                    liabilities: Liabilities { buying, selling },
                    ext: AccountEntryExtensionV1Ext::V0,
                }),
            }),
            ext: LedgerEntryExt::V0,
        }
    }

    fn trustline(account_id: AccountId, balance: i64, buying: i64, selling: i64) -> LedgerEntry {
        let Asset::CreditAlphanum4(asset) = usd() else {
            unreachable!()
        };
        LedgerEntry {
            last_modified_ledger_seq: 42,
            data: LedgerEntryData::Trustline(TrustLineEntry {
                account_id,
                asset: TrustLineAsset::CreditAlphanum4(asset),
                balance,
                limit: 1_000_000,
                flags: TrustLineFlags::AuthorizedFlag as u32,
                ext: TrustLineEntryExt::V1(TrustLineEntryV1 {
                    liabilities: Liabilities { buying, selling },
                    ext: TrustLineEntryV1Ext::V0,
                }),
            }),
            ext: LedgerEntryExt::V0,
        }
    }

    fn offer(
        seller_id: AccountId,
        offer_id: i64,
        selling: Asset,
        buying: Asset,
        amount: i64,
        price: Price,
    ) -> LedgerEntry {
        LedgerEntry {
            last_modified_ledger_seq: 42,
            data: LedgerEntryData::Offer(OfferEntry {
                seller_id,
                offer_id,
                selling,
                buying,
                amount,
                price,
                flags: 0,
                ext: OfferEntryExt::V0,
            }),
            ext: LedgerEntryExt::V0,
        }
    }

    fn fixture() -> (SnapshotHandle, Vec<LedgerEntry>) {
        let outbound = id(1);
        let returning = id(2);
        let mut header = stellar_xdr::LedgerHeader::default();
        header.ledger_seq = 42;
        header.ledger_version = 24;
        header.base_fee = 100;
        header.base_reserve = 5_000_000;
        header.scp_value.close_time.0 = 1_000;
        header.id_pool = 100;
        let offers = vec![
            offer(
                outbound.clone(),
                1,
                usd(),
                Asset::Native,
                100,
                Price { n: 1, d: 1 },
            ),
            offer(
                returning.clone(),
                2,
                Asset::Native,
                usd(),
                1_000,
                Price { n: 1, d: 20 },
            ),
        ];
        let mut entries = vec![
            account(outbound.clone(), 1_000_000_000, 2, 100, 0),
            account(returning.clone(), 1_000_000_000, 2, 0, 1_000),
            account(id(9), 1_000_000_000, 0, 0, 0),
            trustline(outbound, 100, 0, 100),
            trustline(returning, 0, 50, 0),
        ];
        entries.extend(offers.clone());
        let snapshot = SnapshotHandle::new(
            SnapshotBuilder::new(42)
                .with_header(header, Hash256::from_bytes([7; 32]))
                .add_entries(
                    entries
                        .into_iter()
                        .map(|entry| (entry_to_key(&entry), entry)),
                )
                .build()
                .unwrap(),
        );
        (snapshot, offers)
    }

    fn signed_envelope(fee: u32, cond: Preconditions) -> (LedgerKey, TransactionEnvelope) {
        let network_id = NetworkId::testnet();
        let secret =
            SecretKey::from_seed(sha256(b"henyey isolated classic simulation test").as_bytes());
        let public_key = Uint256(*secret.public_key().as_bytes());
        let account_id = AccountId(PublicKey::PublicKeyTypeEd25519(public_key.clone()));
        let source_key = LedgerKey::Account(LedgerKeyAccount { account_id });
        let tx = Transaction {
            source_account: MuxedAccount::Ed25519(public_key.clone()),
            fee,
            seq_num: SequenceNumber(1),
            cond,
            memo: Memo::None,
            operations: vec![Operation {
                source_account: None,
                body: OperationBody::PathPaymentStrictSend(PathPaymentStrictSendOp {
                    send_asset: Asset::Native,
                    send_amount: 10,
                    destination: MuxedAccount::Ed25519(public_key),
                    dest_asset: Asset::Native,
                    dest_min: 200,
                    path: vec![usd()].try_into().unwrap(),
                }),
            }]
            .try_into()
            .unwrap(),
            ext: TransactionExt::V0,
        };
        let mut envelope = TransactionEnvelope::Tx(TransactionV1Envelope {
            tx,
            signatures: VecM::default(),
        });
        let frame = TransactionFrame::with_network(Arc::new(envelope.clone()), network_id);
        let hash = frame.hash(&network_id).unwrap();
        let signature = secret.sign(hash.as_bytes());
        let key = secret.public_key();
        let TransactionEnvelope::Tx(inner) = &mut envelope else {
            unreachable!()
        };
        inner.signatures = vec![DecoratedSignature {
            hint: SignatureHint(key.as_bytes()[28..].try_into().unwrap()),
            signature: Signature(signature.as_bytes().to_vec().try_into().unwrap()),
        }]
        .try_into()
        .unwrap();
        (source_key, envelope)
    }

    fn request(snapshot: &SnapshotHandle) -> (LedgerKey, IsolatedClassicSimulationRequest) {
        let (source_key, envelope) = signed_envelope(snapshot.base_fee(), Preconditions::None);
        let LedgerKey::Account(key) = &source_key else {
            unreachable!()
        };
        let source = account(key.account_id.clone(), 20_000_000, 0, 0, 0);
        let mut request = IsolatedClassicSimulationRequest::new(envelope);
        request.ledger_overrides = HashMap::from([(source_key.clone(), Some(source))]);
        (source_key, request)
    }

    #[test]
    fn executes_signed_transaction_without_mutating_committed_snapshot() {
        let (snapshot, offers) = fixture();
        let (source_key, request) = request(&snapshot);
        assert!(snapshot.get_entry(&source_key).unwrap().is_none());
        let base = IsolatedClassicSimulationBase::prepare(&snapshot, NetworkId::testnet(), offers)
            .unwrap();

        let result = base.simulate(&request).unwrap();
        assert!(result.success, "{:?}", result.error);
        assert_eq!(result.committed_ledger, 42);
        assert_eq!(result.target_ledger, 43);
        assert!(matches!(
            result.operation_results.first(),
            Some(stellar_xdr::OperationResult::OpInner(
                stellar_xdr::OperationResultTr::PathPaymentStrictSend(
                    PathPaymentStrictSendResult::Success(_)
                )
            ))
        ));
        assert!(!result.net_ledger_changes.is_empty());
        assert!(snapshot.get_entry(&source_key).unwrap().is_none());
        assert_eq!(
            base.stats(),
            IsolatedClassicSimulationStats {
                preparations: 1,
                frozen_offers: 2,
                simulations: 1,
                max_transaction_overlay_offers: 2,
            }
        );
    }

    #[test]
    fn rejects_non_offer_and_duplicate_offer_inputs() {
        let (snapshot, offers) = fixture();
        assert!(matches!(
            IsolatedClassicSimulationBase::prepare(
                &snapshot,
                NetworkId::testnet(),
                vec![account(id(3), 100, 0, 0, 0)],
            ),
            Err(IsolatedClassicSimulationError::NonOfferEntry)
        ));
        assert!(matches!(
            IsolatedClassicSimulationBase::prepare(
                &snapshot,
                NetworkId::testnet(),
                vec![offers[0].clone(), offers[0].clone()],
            ),
            Err(IsolatedClassicSimulationError::DuplicateOfferId(1))
        ));
    }

    #[test]
    fn rejects_soroban_envelopes() {
        use stellar_xdr::{
            ContractId, Hash, HostFunction, InvokeContractArgs, InvokeHostFunctionOp, ScAddress,
            ScSymbol,
        };
        let (snapshot, offers) = fixture();
        let base = IsolatedClassicSimulationBase::prepare(&snapshot, NetworkId::testnet(), offers)
            .unwrap();
        let tx = Transaction {
            source_account: MuxedAccount::Ed25519(Uint256([5; 32])),
            fee: 100,
            seq_num: SequenceNumber(1),
            cond: Preconditions::None,
            memo: Memo::None,
            operations: vec![Operation {
                source_account: None,
                body: OperationBody::InvokeHostFunction(InvokeHostFunctionOp {
                    host_function: HostFunction::InvokeContract(InvokeContractArgs {
                        contract_address: ScAddress::Contract(ContractId(Hash([0u8; 32]))),
                        function_name: ScSymbol("test".try_into().unwrap()),
                        args: VecM::default(),
                    }),
                    auth: VecM::default(),
                }),
            }]
            .try_into()
            .unwrap(),
            ext: TransactionExt::V0,
        };
        let request =
            IsolatedClassicSimulationRequest::new(TransactionEnvelope::Tx(TransactionV1Envelope {
                tx,
                signatures: VecM::default(),
            }));
        assert!(matches!(
            base.simulate(&request),
            Err(IsolatedClassicSimulationError::SorobanUnsupported)
        ));
    }

    #[test]
    fn rejects_offer_ledger_overrides() {
        let (snapshot, offers) = fixture();
        let (_, mut request) = request(&snapshot);
        request.ledger_overrides.insert(
            LedgerKey::Offer(stellar_xdr::LedgerKeyOffer {
                seller_id: id(1),
                offer_id: 1,
            }),
            None,
        );
        let base = IsolatedClassicSimulationBase::prepare(&snapshot, NetworkId::testnet(), offers)
            .unwrap();
        assert!(matches!(
            base.simulate(&request),
            Err(IsolatedClassicSimulationError::OfferOverrideUnsupported)
        ));
    }

    #[test]
    fn rejects_offer_id_above_committed_id_pool() {
        let (snapshot, _) = fixture();
        // fixture header has id_pool = 100; offer id 101 would collide with
        // the first offer a simulated transaction creates.
        assert!(matches!(
            IsolatedClassicSimulationBase::prepare(
                &snapshot,
                NetworkId::testnet(),
                vec![offer(
                    id(1),
                    101,
                    usd(),
                    Asset::Native,
                    100,
                    Price { n: 1, d: 1 },
                )],
            ),
            Err(IsolatedClassicSimulationError::OfferIdExceedsIdPool(
                101, 100
            ))
        ));
    }

    #[test]
    fn bad_seq_transaction_still_charges_full_fee_and_debits_balance() {
        let (snapshot, offers) = fixture();
        let (source_key, mut request) = request(&snapshot);
        // Make the account's sequence far ahead of the envelope's seq_num 1 so
        // apply-time validation fails with txBAD_SEQ.
        let LedgerKey::Account(key) = &source_key else {
            unreachable!()
        };
        let mut source = account(key.account_id.clone(), 20_000_000, 0, 0, 0);
        let LedgerEntryData::Account(acc) = &mut source.data else {
            unreachable!()
        };
        acc.seq_num = SequenceNumber(500);
        request
            .ledger_overrides
            .insert(source_key.clone(), Some(source));

        let base = IsolatedClassicSimulationBase::prepare(&snapshot, NetworkId::testnet(), offers)
            .unwrap();
        let result = base.simulate(&request).unwrap();
        assert!(!result.success);
        assert!(matches!(
            result.result.result.result,
            stellar_xdr::TransactionResultResult::TxBadSeq
        ));
        // Production pre-deducts the fee for every included transaction and
        // reports it even on validation failure.
        assert_eq!(result.fee_charged, 100);
        assert_eq!(result.result.result.fee_charged, 100);
        let change = result
            .net_ledger_changes
            .iter()
            .find(|change| change.key == source_key)
            .expect("fee debit must appear in net changes");
        let LedgerEntryData::Account(before) = &change.before.as_ref().unwrap().data else {
            unreachable!()
        };
        let LedgerEntryData::Account(after) = &change.after.as_ref().unwrap().data else {
            unreachable!()
        };
        assert_eq!(before.balance, 20_000_000);
        assert_eq!(after.balance, 19_999_900);
        // txBAD_SEQ does not consume a sequence number.
        assert_eq!(after.seq_num, SequenceNumber(500));
    }

    #[test]
    fn close_time_override_controls_time_bound_validity() {
        use stellar_xdr::{TimeBounds, TimePoint};
        let (snapshot, offers) = fixture();
        let (source_key, envelope) = signed_envelope(
            snapshot.base_fee(),
            Preconditions::Time(TimeBounds {
                min_time: TimePoint(0),
                max_time: TimePoint(1_500),
            }),
        );
        let LedgerKey::Account(key) = &source_key else {
            unreachable!()
        };
        let source = account(key.account_id.clone(), 20_000_000, 0, 0, 0);
        let mut request = IsolatedClassicSimulationRequest::new(envelope);
        request.ledger_overrides = HashMap::from([(source_key, Some(source))]);
        let base = IsolatedClassicSimulationBase::prepare(&snapshot, NetworkId::testnet(), offers)
            .unwrap();

        // Default LCL+1 close time (1_001) is within the bounds.
        let on_time = base.simulate(&request).unwrap();
        assert!(on_time.success, "{:?}", on_time.error);

        // Overriding the close time past maxTime must fail with txTOO_LATE.
        request.close_time = Some(2_000);
        let late = base.simulate(&request).unwrap();
        assert!(!late.success);
        assert!(matches!(
            late.result.result.result,
            stellar_xdr::TransactionResultResult::TxTooLate
        ));
        // The fee is still charged, as for any included transaction.
        assert_eq!(late.fee_charged, 100);
    }

    #[test]
    fn base_fee_override_controls_fee_charged() {
        let (snapshot, offers) = fixture();
        let (source_key, envelope) = signed_envelope(1_000, Preconditions::None);
        let LedgerKey::Account(key) = &source_key else {
            unreachable!()
        };
        let source = account(key.account_id.clone(), 20_000_000, 0, 0, 0);
        let mut request = IsolatedClassicSimulationRequest::new(envelope);
        request.ledger_overrides = HashMap::from([(source_key, Some(source))]);
        let base = IsolatedClassicSimulationBase::prepare(&snapshot, NetworkId::testnet(), offers)
            .unwrap();

        // Default: header base_fee (100) is charged.
        let default_fee = base.simulate(&request).unwrap();
        assert_eq!(default_fee.fee_charged, 100);
        assert_eq!(default_fee.result.result.fee_charged, 100);

        // Surge-priced ledger: effective base fee 250 is charged.
        request.base_fee = Some(250);
        let surged = base.simulate(&request).unwrap();
        assert_eq!(surged.fee_charged, 250);
        assert_eq!(surged.result.result.fee_charged, 250);
    }

    #[test]
    fn classic_event_config_changes_emitted_meta() {
        let (snapshot, offers) = fixture();
        let (_, mut request) = request(&snapshot);
        let base = IsolatedClassicSimulationBase::prepare(&snapshot, NetworkId::testnet(), offers)
            .unwrap();

        let without_events = base.simulate(&request).unwrap();
        assert!(without_events.success, "{:?}", without_events.error);

        request.classic_events = ClassicEventConfig {
            emit_classic_events: true,
            backfill_stellar_asset_events: false,
        };
        let with_events = base.simulate(&request).unwrap();
        assert!(with_events.success, "{:?}", with_events.error);
        assert_ne!(
            without_events.transaction_meta, with_events.transaction_meta,
            "enabling classic events must change the emitted meta"
        );
    }

    #[test]
    fn independent_simulations_do_not_share_offer_mutations() {
        let (snapshot, offers) = fixture();
        let (_, request) = request(&snapshot);
        let base = IsolatedClassicSimulationBase::prepare(&snapshot, NetworkId::testnet(), offers)
            .unwrap();
        let first = base.simulate(&request).unwrap();
        let second = base.simulate(&request).unwrap();
        assert_eq!(first.success, second.success);
        assert_eq!(first.operation_results, second.operation_results);
        assert_eq!(base.stats().simulations, 2);
    }
}
