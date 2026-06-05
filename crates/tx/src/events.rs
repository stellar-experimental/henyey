//! Classic event emission for Stellar Asset Contract (SAC) events.
//!
//! This module handles emission of SEP-0041 (Stellar Token Standard) compatible
//! events for classic Stellar operations involving assets. These events provide
//! a unified interface for tracking asset movements regardless of whether they
//! occur through classic operations or Soroban contracts.
//!
//! # Event Types
//!
//! - **transfer**: Asset moved between two non-issuer accounts
//! - **mint**: Asset issued from the issuer to a recipient
//! - **burn**: Asset returned to the issuer
//! - **clawback**: Asset forcibly returned to the issuer
//! - **set_authorized**: Trustline authorization status changed
//! - **fee**: Transaction fee payment (at transaction level)
//!
//! # Protocol Versioning
//!
//! - Protocol 23+: Classic events are natively emitted
//! - Pre-Protocol 23: Events can be backfilled for historical analysis
//!
//! # Key Types
//!
//! - [`OpEventManager`]: Manages events for a single operation
//! - [`TxEventManager`]: Manages transaction-level events (fees)
//! - [`ClassicEventConfig`]: Configuration for event emission behavior
//! - [`EventManagerHierarchy`]: Composes all event managers for a transaction
//!
//! # Parity
//!
//! This module provides full parity with stellar-core's EventManager hierarchy:
//! - `OpEventManager` matches stellar-core `OpEventManager` with issuer-aware transfer logic
//! - `TxEventManager` matches stellar-core `TxEventManager` for fee events
//! - `EventManagerHierarchy` composes managers like stellar-core `TransactionMetaBuilder`
//! - Support for `insertAtBeginning` flag in mint events for lumen reconciliation

use henyey_common::NetworkId;
use henyey_crypto::PublicKey as StrKeyPublicKey;
use stellar_xdr::curr::{
    AccountId, Asset, ClaimableBalanceId, ContractDataDurability, ContractEvent, ContractEventBody,
    ContractEventType, ContractEventV0, ContractId, ContractIdPreimage, Hash, HashIdPreimage,
    HashIdPreimageContractId, Int128Parts, LedgerEntry, LedgerEntryData, LedgerKey, Limits, Memo,
    MuxedAccount, MuxedEd25519Account, PublicKey as XdrPublicKey, ReadXdr, ScAddress, ScMap,
    ScMapEntry, ScString, ScSymbol, ScVal, StringM, TransactionEvent, TransactionEventStage,
};

#[derive(Debug, Clone, Copy, Default)]
pub struct ClassicEventConfig {
    pub emit_classic_events: bool,
    pub backfill_stellar_asset_events: bool,
}

impl ClassicEventConfig {
    pub fn events_enabled(self, _protocol_version: u32) -> bool {
        self.emit_classic_events
    }

    /// Whether SAC mint/burn event backfill (the P23 corruption reconciler) is
    /// active for this ledger.
    ///
    /// Mirrors stellar-core's gating: the reconciler only runs when
    /// `BACKFILL_STELLAR_ASSET_EVENTS` is enabled AND the protocol version is
    /// **exactly** 23 (`protocolVersionEquals(pv, ProtocolVersion::V_23)` in
    /// `Protocol23CorruptionEventReconciler::getSACReconciliationEventAndTrackDiff`,
    /// `P23HotArchiveBug.cpp:530`). The equality (not `>=`) is load-bearing —
    /// the corruption only occurred during protocol 23.
    pub fn backfill_to_protocol23(self, protocol_version: u32) -> bool {
        self.backfill_stellar_asset_events && protocol_version == 23
    }
}

/// Manages contract events for a single operation.
///
/// `OpEventManager` accumulates contract events during operation execution and
/// provides smart event generation that handles issuer-aware transfer logic
/// (mint vs burn vs transfer).
///
/// # Lifecycle
///
/// 1. Create with [`OpEventManager::new`]
/// 2. Call event generation methods during operation execution
/// 3. Call [`OpEventManager::finalize`] to extract events
///
/// After finalization, no more events can be added.
///
/// # Issuer-Aware Transfer Logic
///
/// When using [`OpEventManager::event_for_transfer_with_issuer_check`]:
/// - If both parties are the issuer: emits transfer event
/// - If sender is issuer: emits mint event
/// - If receiver is issuer: emits burn event
/// - Otherwise: emits transfer event
pub struct OpEventManager {
    enabled: bool,
    backfill_to_protocol23: bool,
    events: Vec<ContractEvent>,
    network_id: NetworkId,
    memo: Memo,
    /// Guard to prevent mutations after finalization
    finalized: bool,
}

impl OpEventManager {
    /// Create a new operation event manager.
    ///
    /// # Parameters
    ///
    /// - `meta_enabled`: Whether metadata building is enabled
    /// - `is_soroban`: Whether this is a Soroban operation
    /// - `protocol_version`: Current protocol version
    /// - `network_id`: Network identifier for contract ID computation
    /// - `memo`: Transaction memo for muxed transfer events
    /// - `config`: Classic event emission configuration
    pub fn new(
        meta_enabled: bool,
        is_soroban: bool,
        protocol_version: u32,
        network_id: NetworkId,
        memo: Memo,
        config: ClassicEventConfig,
    ) -> Self {
        let enabled = meta_enabled && (is_soroban || config.events_enabled(protocol_version));
        let backfill_to_protocol23 = config.backfill_to_protocol23(protocol_version);
        Self {
            enabled,
            backfill_to_protocol23,
            events: Vec::new(),
            network_id,
            memo,
            finalized: false,
        }
    }

    /// Create a disabled event manager (no-op for all methods).
    pub fn disabled() -> Self {
        Self {
            enabled: false,
            backfill_to_protocol23: false,
            events: Vec::new(),
            network_id: NetworkId::testnet(),
            memo: Memo::None,
            finalized: false,
        }
    }

    /// Check if this manager is enabled.
    pub fn is_enabled(&self) -> bool {
        self.enabled
    }

    /// Check if this manager has been finalized.
    pub fn is_finalized(&self) -> bool {
        self.finalized
    }

    /// Get the number of accumulated events.
    pub fn event_count(&self) -> usize {
        self.events.len()
    }

    pub fn events_for_claim_atoms(
        &mut self,
        source: &MuxedAccount,
        claim_atoms: &[stellar_xdr::curr::ClaimAtom],
    ) {
        if !self.enabled || self.finalized {
            return;
        }
        let source_addr = make_muxed_account_address(source);
        for atom in claim_atoms {
            match atom {
                stellar_xdr::curr::ClaimAtom::OrderBook(claim) => {
                    let seller = make_account_address(&claim.seller_id);
                    self.event_for_transfer_with_issuer_check(
                        &claim.asset_bought,
                        &source_addr,
                        &seller,
                        claim.amount_bought,
                        false,
                    );
                    self.event_for_transfer_with_issuer_check(
                        &claim.asset_sold,
                        &seller,
                        &source_addr,
                        claim.amount_sold,
                        false,
                    );
                }
                stellar_xdr::curr::ClaimAtom::LiquidityPool(claim) => {
                    let pool = ScAddress::LiquidityPool(claim.liquidity_pool_id.clone());
                    self.event_for_transfer_with_issuer_check(
                        &claim.asset_bought,
                        &source_addr,
                        &pool,
                        claim.amount_bought,
                        false,
                    );
                    self.event_for_transfer_with_issuer_check(
                        &claim.asset_sold,
                        &pool,
                        &source_addr,
                        claim.amount_sold,
                        false,
                    );
                }
                stellar_xdr::curr::ClaimAtom::V0(claim) => {
                    let seller = ScAddress::Account(AccountId::from(
                        XdrPublicKey::PublicKeyTypeEd25519(claim.seller_ed25519.clone()),
                    ));
                    self.event_for_transfer_with_issuer_check(
                        &claim.asset_bought,
                        &source_addr,
                        &seller,
                        claim.amount_bought,
                        false,
                    );
                    self.event_for_transfer_with_issuer_check(
                        &claim.asset_sold,
                        &seller,
                        &source_addr,
                        claim.amount_sold,
                        false,
                    );
                }
            }
        }
    }

    pub fn event_for_transfer_with_issuer_check(
        &mut self,
        asset: &Asset,
        from: &ScAddress,
        to: &ScAddress,
        amount: i64,
        allow_muxed_id_or_memo: bool,
    ) {
        if !self.enabled || self.finalized {
            return;
        }

        let from_is_issuer = is_issuer(from, asset);
        let to_is_issuer = is_issuer(to, asset);

        if from_is_issuer && to_is_issuer {
            self.new_transfer_event(asset, from, to, amount, allow_muxed_id_or_memo);
        } else if from_is_issuer {
            self.new_mint_event(asset, to, amount, allow_muxed_id_or_memo);
        } else if to_is_issuer {
            self.new_burn_event(asset, from, amount);
        } else {
            self.new_transfer_event(asset, from, to, amount, allow_muxed_id_or_memo);
        }
    }

    pub fn new_transfer_event(
        &mut self,
        asset: &Asset,
        from: &ScAddress,
        to: &ScAddress,
        amount: i64,
        allow_muxed_id_or_memo: bool,
    ) {
        if !self.enabled || self.finalized {
            return;
        }
        let contract_id = get_asset_contract_id(&self.network_id, asset);
        let topics = vec![
            make_symbol_scval("transfer"),
            ScVal::Address(get_address_with_dropped_muxed_info(from)),
            ScVal::Address(get_address_with_dropped_muxed_info(to)),
            make_sep0011_asset_string_scval(asset),
        ];
        let data = make_possible_muxed_data(to, amount, &self.memo, allow_muxed_id_or_memo);
        self.events.push(make_event(contract_id, topics, data));
    }

    /// Emit a mint event for asset issuance.
    ///
    /// # Parameters
    ///
    /// - `asset`: The asset being minted
    /// - `to`: Recipient address
    /// - `amount`: Amount minted (in stroops for XLM, smallest unit for other assets)
    /// - `allow_muxed_id_or_memo`: Whether to include muxed ID or memo in event data
    pub fn new_mint_event(
        &mut self,
        asset: &Asset,
        to: &ScAddress,
        amount: i64,
        allow_muxed_id_or_memo: bool,
    ) {
        self.new_mint_event_internal(asset, to, amount, allow_muxed_id_or_memo, false);
    }

    /// Emit a mint event at the beginning of the event list.
    ///
    /// This is used by the [`LumenEventReconciler`](crate::lumen_reconciler::LumenEventReconciler)
    /// to insert synthetic mint events for pre-protocol 8 XLM reconciliation.
    /// Inserting at the beginning ensures correct event ordering for historical replay.
    ///
    /// # Parameters
    ///
    /// - `asset`: The asset being minted
    /// - `to`: Recipient address
    /// - `amount`: Amount minted
    pub fn new_mint_event_at_beginning(&mut self, asset: &Asset, to: &ScAddress, amount: i64) {
        self.new_mint_event_internal(asset, to, amount, false, true);
    }

    /// Internal mint event implementation with insert position control.
    fn new_mint_event_internal(
        &mut self,
        asset: &Asset,
        to: &ScAddress,
        amount: i64,
        allow_muxed_id_or_memo: bool,
        insert_at_beginning: bool,
    ) {
        if !self.enabled || self.finalized {
            return;
        }
        let contract_id = get_asset_contract_id(&self.network_id, asset);
        let topics = vec![
            make_symbol_scval("mint"),
            ScVal::Address(get_address_with_dropped_muxed_info(to)),
            make_sep0011_asset_string_scval(asset),
        ];
        let data = make_possible_muxed_data(to, amount, &self.memo, allow_muxed_id_or_memo);
        let event = make_event(contract_id, topics, data);

        if insert_at_beginning {
            self.events.insert(0, event);
        } else {
            self.events.push(event);
        }
    }

    /// Emit a burn event for asset destruction.
    ///
    /// # Parameters
    ///
    /// - `asset`: The asset being burned
    /// - `from`: Address from which asset is burned
    /// - `amount`: Amount burned
    pub fn new_burn_event(&mut self, asset: &Asset, from: &ScAddress, amount: i64) {
        if !self.enabled || self.finalized {
            return;
        }
        let contract_id = get_asset_contract_id(&self.network_id, asset);
        let topics = vec![
            make_symbol_scval("burn"),
            ScVal::Address(get_address_with_dropped_muxed_info(from)),
            make_sep0011_asset_string_scval(asset),
        ];
        let data = make_i128_scval(amount);
        self.events.push(make_event(contract_id, topics, data));
    }

    /// Emit a clawback event for forcible asset recovery.
    ///
    /// # Parameters
    ///
    /// - `asset`: The asset being clawed back
    /// - `from`: Address from which asset is clawed back
    /// - `amount`: Amount clawed back
    pub fn new_clawback_event(&mut self, asset: &Asset, from: &ScAddress, amount: i64) {
        if !self.enabled || self.finalized {
            return;
        }
        let contract_id = get_asset_contract_id(&self.network_id, asset);
        let topics = vec![
            make_symbol_scval("clawback"),
            ScVal::Address(get_address_with_dropped_muxed_info(from)),
            make_sep0011_asset_string_scval(asset),
        ];
        let data = make_i128_scval(amount);
        self.events.push(make_event(contract_id, topics, data));
    }

    /// Emit a set_authorized event for trustline authorization changes.
    ///
    /// # Parameters
    ///
    /// - `asset`: The asset for which authorization is changing
    /// - `account`: Account whose authorization is changing
    /// - `authorize`: New authorization status (true = authorized, false = not authorized)
    pub fn new_set_authorized_event(
        &mut self,
        asset: &Asset,
        account: &AccountId,
        authorize: bool,
    ) {
        if !self.enabled || self.finalized {
            return;
        }
        let contract_id = get_asset_contract_id(&self.network_id, asset);
        let topics = vec![
            make_symbol_scval("set_authorized"),
            ScVal::Address(ScAddress::Account(account.clone())),
            make_sep0011_asset_string_scval(asset),
        ];
        let data = ScVal::Bool(authorize);
        self.events.push(make_event(contract_id, topics, data));
    }

    /// Set events from an external source (e.g., Soroban contract execution).
    ///
    /// This is incompatible with event generation methods - use one or the other.
    /// If `backfill_to_protocol23` is enabled, events will be transformed to
    /// the protocol 23 format.
    pub fn set_events(&mut self, mut events: Vec<ContractEvent>) {
        if !self.enabled || self.finalized {
            return;
        }
        if self.backfill_to_protocol23 {
            for event in events.iter_mut() {
                backfill_event(event, &self.network_id);
            }
        }
        self.events = events;
    }

    /// Finalize and return all accumulated events.
    ///
    /// After calling this method, no more events can be added.
    /// Subsequent calls return an empty vector.
    pub fn finalize(&mut self) -> Vec<ContractEvent> {
        self.finalized = true;
        std::mem::take(&mut self.events)
    }

    /// Consume and finalize, returning all events.
    ///
    /// This is the consuming variant of [`OpEventManager::finalize`].
    pub fn into_events(mut self) -> Vec<ContractEvent> {
        self.finalize()
    }
}

/// Manages transaction-level events (currently only fee events).
///
/// `TxEventManager` handles events that occur at the transaction level rather
/// than the operation level. Currently, this is limited to fee events that
/// track XLM charged or refunded for transaction fees.
///
/// # Lifecycle
///
/// 1. Create with [`TxEventManager::new`]
/// 2. Call [`TxEventManager::new_fee_event`] for fee charges/refunds
/// 3. Call [`TxEventManager::finalize`] to extract events
///
/// After finalization, no more events can be added.
pub struct TxEventManager {
    enabled: bool,
    events: Vec<TransactionEvent>,
    network_id: NetworkId,
    /// Guard to prevent mutations after finalization
    finalized: bool,
}

impl TxEventManager {
    /// Create a new transaction event manager.
    ///
    /// # Parameters
    ///
    /// - `meta_enabled`: Whether metadata building is enabled
    /// - `protocol_version`: Current protocol version
    /// - `network_id`: Network identifier for contract ID computation
    /// - `config`: Classic event emission configuration
    pub fn new(
        meta_enabled: bool,
        protocol_version: u32,
        network_id: NetworkId,
        config: ClassicEventConfig,
    ) -> Self {
        let enabled = meta_enabled && config.events_enabled(protocol_version);
        Self {
            enabled,
            events: Vec::new(),
            network_id,
            finalized: false,
        }
    }

    /// Create a disabled event manager (no-op for all methods).
    pub fn disabled() -> Self {
        Self {
            enabled: false,
            events: Vec::new(),
            network_id: NetworkId::testnet(),
            finalized: false,
        }
    }

    /// Check if this manager is enabled.
    pub fn is_enabled(&self) -> bool {
        self.enabled
    }

    /// Check if this manager has been finalized.
    pub fn is_finalized(&self) -> bool {
        self.finalized
    }

    /// Get the number of accumulated events.
    pub fn event_count(&self) -> usize {
        self.events.len()
    }

    /// Emit a fee event.
    ///
    /// Fee events track XLM charged or refunded for transaction fees.
    ///
    /// # Parameters
    ///
    /// - `fee_source`: Account that paid the fee
    /// - `amount`: Amount charged (negative) or refunded (positive)
    /// - `stage`: Transaction lifecycle stage when the fee event occurred
    ///
    /// # Note
    ///
    /// Zero-amount fee events are skipped (no-op).
    pub fn new_fee_event(
        &mut self,
        fee_source: &AccountId,
        amount: i64,
        stage: TransactionEventStage,
    ) {
        if !self.enabled || self.finalized || amount == 0 {
            return;
        }
        let contract_id = get_asset_contract_id(&self.network_id, &Asset::Native);
        let topics = vec![
            make_symbol_scval("fee"),
            ScVal::Address(ScAddress::Account(fee_source.clone())),
        ];
        let data = make_i128_scval(amount);
        let event = make_event(contract_id, topics, data);
        self.events.push(TransactionEvent { stage, event });
    }

    /// Emit a fee charge event (negative amount).
    ///
    /// Convenience method for charging fees.
    pub fn charge_fee(
        &mut self,
        fee_source: &AccountId,
        amount: i64,
        stage: TransactionEventStage,
    ) {
        self.new_fee_event(fee_source, -amount.saturating_abs(), stage);
    }

    /// Emit a fee refund event (positive amount).
    ///
    /// Convenience method for refunding fees.
    pub fn refund_fee(
        &mut self,
        fee_source: &AccountId,
        amount: i64,
        stage: TransactionEventStage,
    ) {
        self.new_fee_event(fee_source, amount.saturating_abs(), stage);
    }

    /// Finalize and return all accumulated events.
    ///
    /// After calling this method, no more events can be added.
    /// Subsequent calls return an empty vector.
    pub fn finalize(&mut self) -> Vec<TransactionEvent> {
        self.finalized = true;
        std::mem::take(&mut self.events)
    }

    /// Consume and finalize, returning all events.
    pub fn into_events(mut self) -> Vec<TransactionEvent> {
        self.finalize()
    }
}

/// Composes all event managers for a transaction.
///
/// `EventManagerHierarchy` provides a unified interface for managing events
/// at both the operation and transaction levels. It matches the stellar-core
/// architecture where `TransactionMetaBuilder` composes `TxEventManager` and
/// multiple `OpEventManager` instances.
///
/// # Structure
///
/// - One `TxEventManager` for transaction-level fee events
/// - One `OpEventManager` per operation for operation-level events
///
/// # Usage
///
/// ```ignore
/// let mut hierarchy = EventManagerHierarchy::new(
///     true,   // meta_enabled
///     false,  // is_soroban
///     21,     // protocol_version
///     NetworkId::testnet(),
///     Memo::None,
///     ClassicEventConfig::default(),
///     2,      // operation_count
/// );
///
/// // Access operation event managers
/// hierarchy.op_event_manager(0).new_transfer_event(...);
///
/// // Access transaction event manager
/// hierarchy.tx_event_manager().new_fee_event(...);
///
/// // Finalize all managers
/// let (op_events, tx_events) = hierarchy.finalize();
/// ```
pub struct EventManagerHierarchy {
    tx_manager: TxEventManager,
    op_managers: Vec<OpEventManager>,
}

impl EventManagerHierarchy {
    /// Create a new event manager hierarchy.
    ///
    /// # Parameters
    ///
    /// - `meta_enabled`: Whether metadata building is enabled
    /// - `is_soroban`: Whether this is a Soroban transaction
    /// - `protocol_version`: Current protocol version
    /// - `network_id`: Network identifier
    /// - `memo`: Transaction memo
    /// - `config`: Classic event configuration
    /// - `operation_count`: Number of operations in the transaction
    pub fn new(
        meta_enabled: bool,
        is_soroban: bool,
        protocol_version: u32,
        network_id: NetworkId,
        memo: Memo,
        config: ClassicEventConfig,
        operation_count: usize,
    ) -> Self {
        let tx_manager = TxEventManager::new(meta_enabled, protocol_version, network_id, config);

        let op_managers = (0..operation_count)
            .map(|_| {
                OpEventManager::new(
                    meta_enabled,
                    is_soroban,
                    protocol_version,
                    network_id,
                    memo.clone(),
                    config,
                )
            })
            .collect();

        Self {
            tx_manager,
            op_managers,
        }
    }

    /// Create a disabled hierarchy (no-op for all methods).
    pub fn disabled(operation_count: usize) -> Self {
        Self {
            tx_manager: TxEventManager::disabled(),
            op_managers: (0..operation_count)
                .map(|_| OpEventManager::disabled())
                .collect(),
        }
    }

    /// Get a mutable reference to the transaction event manager.
    pub fn tx_event_manager(&mut self) -> &mut TxEventManager {
        &mut self.tx_manager
    }

    /// Get a reference to the transaction event manager.
    pub fn tx_event_manager_ref(&self) -> &TxEventManager {
        &self.tx_manager
    }

    /// Get a mutable reference to an operation's event manager.
    ///
    /// # Panics
    ///
    /// Panics if `index` is out of bounds.
    pub fn op_event_manager(&mut self, index: usize) -> &mut OpEventManager {
        &mut self.op_managers[index]
    }

    /// Get a reference to an operation's event manager.
    ///
    /// # Panics
    ///
    /// Panics if `index` is out of bounds.
    pub fn op_event_manager_ref(&self, index: usize) -> &OpEventManager {
        &self.op_managers[index]
    }

    /// Get the number of operation event managers.
    pub fn operation_count(&self) -> usize {
        self.op_managers.len()
    }

    /// Finalize all event managers and return the events.
    ///
    /// Returns a tuple of:
    /// - Vector of operation event vectors (one per operation)
    /// - Vector of transaction events
    pub fn finalize(&mut self) -> (Vec<Vec<ContractEvent>>, Vec<TransactionEvent>) {
        let op_events: Vec<Vec<ContractEvent>> =
            self.op_managers.iter_mut().map(|m| m.finalize()).collect();
        let tx_events = self.tx_manager.finalize();
        (op_events, tx_events)
    }

    /// Consume and finalize all managers.
    pub fn into_events(mut self) -> (Vec<Vec<ContractEvent>>, Vec<TransactionEvent>) {
        self.finalize()
    }
}

pub fn make_muxed_account_address(muxed: &MuxedAccount) -> ScAddress {
    match muxed {
        MuxedAccount::Ed25519(pk) => ScAddress::Account(AccountId::from(
            XdrPublicKey::PublicKeyTypeEd25519(pk.clone()),
        )),
        MuxedAccount::MuxedEd25519(m) => ScAddress::MuxedAccount(MuxedEd25519Account {
            id: m.id,
            ed25519: m.ed25519.clone(),
        }),
    }
}

pub fn make_account_address(account: &AccountId) -> ScAddress {
    ScAddress::Account(account.clone())
}

pub fn make_claimable_balance_address(balance_id: &ClaimableBalanceId) -> ScAddress {
    ScAddress::ClaimableBalance(balance_id.clone())
}

fn make_event(contract_id: ContractId, topics: Vec<ScVal>, data: ScVal) -> ContractEvent {
    let topics: Vec<ScVal> = topics;
    ContractEvent {
        ext: stellar_xdr::curr::ExtensionPoint::V0,
        contract_id: Some(contract_id),
        type_: ContractEventType::Contract,
        body: ContractEventBody::V0(ContractEventV0 {
            topics: topics.try_into().unwrap_or_default(),
            data,
        }),
    }
}

fn get_address_with_dropped_muxed_info(address: &ScAddress) -> ScAddress {
    match address {
        ScAddress::MuxedAccount(muxed) => ScAddress::Account(AccountId::from(
            XdrPublicKey::PublicKeyTypeEd25519(muxed.ed25519.clone()),
        )),
        _ => address.clone(),
    }
}

fn make_sep0011_asset_string_scval(asset: &Asset) -> ScVal {
    let asset_str = match asset {
        Asset::Native => "native".to_string(),
        Asset::CreditAlphanum4(a) => format!(
            "{}:{}",
            asset_code_to_string(&a.asset_code.0),
            account_id_to_strkey(&a.issuer).unwrap_or_default()
        ),
        Asset::CreditAlphanum12(a) => format!(
            "{}:{}",
            asset_code_to_string(&a.asset_code.0),
            account_id_to_strkey(&a.issuer).unwrap_or_default()
        ),
    };
    ScVal::String(ScString(StringM::try_from(asset_str).unwrap_or_default()))
}

fn account_id_to_strkey(account_id: &AccountId) -> Option<String> {
    let public_key = match account_id {
        AccountId(pk) => StrKeyPublicKey::try_from(pk).ok()?,
    };
    Some(public_key.to_strkey())
}

fn asset_code_to_string(bytes: &[u8]) -> String {
    let end = bytes.iter().position(|b| *b == 0).unwrap_or(bytes.len());
    String::from_utf8_lossy(&bytes[..end]).into_owned()
}

use crate::scval_utils::{make_string_scval, make_symbol_scval};

fn make_i128_scval(amount: i64) -> ScVal {
    let value = amount as i128;
    ScVal::I128(Int128Parts {
        hi: (value >> 64) as i64,
        lo: value as u64,
    })
}

fn make_u64_scval(value: u64) -> ScVal {
    ScVal::U64(value)
}

fn make_bytes_scval(bytes: &[u8]) -> ScVal {
    ScVal::Bytes(bytes.to_vec().try_into().unwrap_or_default())
}

fn make_classic_memo_scval(memo: &Memo) -> ScVal {
    match memo {
        Memo::None => panic!("memo type cannot be None for classic memo encoding"),
        Memo::Text(text) => {
            let value = std::str::from_utf8(text.as_ref()).unwrap_or("");
            make_string_scval(value)
        }
        Memo::Id(id) => make_u64_scval(*id),
        Memo::Hash(hash) => make_bytes_scval(&hash.0),
        Memo::Return(ret) => make_bytes_scval(&ret.0),
    }
}

fn make_possible_muxed_data(
    to: &ScAddress,
    amount: i64,
    memo: &Memo,
    allow_muxed_id_or_memo: bool,
) -> ScVal {
    let is_to_muxed = matches!(to, ScAddress::MuxedAccount(_));
    let has_memo = !matches!(memo, Memo::None);

    if !allow_muxed_id_or_memo || (!is_to_muxed && !has_memo) {
        return make_i128_scval(amount);
    }

    let mut map = Vec::new();
    map.push(ScMapEntry {
        key: make_symbol_scval("amount"),
        val: make_i128_scval(amount),
    });
    let muxed_val = if let ScAddress::MuxedAccount(muxed) = to {
        make_u64_scval(muxed.id)
    } else {
        make_classic_memo_scval(memo)
    };
    map.push(ScMapEntry {
        key: make_symbol_scval("to_muxed_id"),
        val: muxed_val,
    });
    ScVal::Map(Some(ScMap(map.try_into().unwrap_or_default())))
}

fn is_issuer(address: &ScAddress, asset: &Asset) -> bool {
    let account = match address {
        ScAddress::Account(account) => account,
        _ => return false,
    };
    match asset {
        Asset::Native => false,
        Asset::CreditAlphanum4(a) => &a.issuer == account,
        Asset::CreditAlphanum12(a) => &a.issuer == account,
    }
}

fn get_asset_contract_id(network_id: &NetworkId, asset: &Asset) -> ContractId {
    let preimage = HashIdPreimage::ContractId(HashIdPreimageContractId {
        network_id: Hash::from(network_id.0),
        contract_id_preimage: ContractIdPreimage::Asset(asset.clone()),
    });
    let hash = henyey_common::Hash256::hash_xdr(&preimage);
    ContractId(Hash::from(hash))
}

// ============================================================================
// Protocol 23 SAC mint/burn event reconciler (issue #3126).
//
// Port of stellar-core's `Protocol23CorruptionEventReconciler`
// (`stellar-core/src/ledger/P23HotArchiveBug.cpp:456-615`).
//
// During protocol-23 catchup, when a corrupted SAC (Stellar Asset Contract)
// balance entry is auto-restored from the hot archive, stellar-core prepends a
// synthetic mint/burn reconciliation event to the InvokeHostFunction operation
// meta so the off-chain event stream balances out the corruption. This is
// observability-only: the events are added to op-meta AFTER the success
// preimage is hashed (`InvokeHostFunctionOpFrame.cpp:821` hashes `success`
// before `:825` `setEvents` prepends the reconciliation events), so they have
// ZERO effect on `bucketListHash`, the tx-result hash, or the
// `InvokeHostFunctionSuccessPreImage` hash. The whole mechanism is gated on
// `BACKFILL_STELLAR_ASSET_EVENTS` and off by default.
//
// Intentional divergences from stellar-core, both parity-safe:
//   * We omit `mReconciliationAmounts` / `hasReconciliationAmount` tracking —
//     its only upstream consumer is the unimplemented
//     `EventsAreConsistentWithEntryDiffs` invariant.
//   * We drop stellar-core's `std::mutex` (henyey replays single-threaded for
//     the reconciler input).
// ============================================================================

/// A single reconciliation event produced by [`P23SacReconciler`].
///
/// Mirrors `Protocol23CorruptionEventReconciler::SACReconciliationInfo`. A
/// positive `amount` is a mint to `mint_or_burn_address`; a negative `amount`
/// is a burn from that address (emitting `-amount` as a positive number).
#[derive(Debug, Clone)]
pub struct SacReconciliationInfo {
    /// The affected SAC asset.
    pub asset: Asset,
    /// The balance owner address that mints (diff > 0) or burns (diff < 0).
    pub mint_or_burn_address: ScAddress,
    /// `restored_balance - correct_balance` (never zero — equal balances yield
    /// no event).
    pub amount: i64,
}

/// Reconciler for the protocol-23 hot-archive SAC corruption.
///
/// Built once (decoding the hardcoded data table is non-trivial), then queried
/// for each hot-archive restore during a protocol-23 ledger.
pub struct P23SacReconciler {
    /// SAC contract id → affected `Asset`. Built from the 12-entry
    /// `P23_CORRUPTED_AFFECTED_ASSETS` array via [`get_asset_contract_id`].
    sac_asset_map: std::collections::HashMap<ScAddress, Asset>,
    /// Corrupted-entry `LedgerKey` → (correct entry, corrupted entry), built
    /// from the 478 corrupted/correct pairs.
    key_to_entries: std::collections::HashMap<LedgerKey, (LedgerEntry, LedgerEntry)>,
}

impl P23SacReconciler {
    /// Construct the reconciler from the hardcoded base64-XDR data table.
    ///
    /// `affected_assets` are the base64-XDR `Asset` literals
    /// (`P23_CORRUPTED_AFFECTED_ASSETS`); `corrupted_correct_pairs` are
    /// `(corrupted, correct)` base64-XDR `LedgerEntry` literals (the 478 pairs
    /// from the #3061 data table, in `(P23_CORRUPTED_HOT_ARCHIVE_ENTRIES[i],
    /// P23_CORRUPTED_HOT_ARCHIVE_ENTRY_CORRECT_STATE[i])` order).
    ///
    /// Mirrors `Protocol23CorruptionEventReconciler::Protocol23CorruptionEventReconciler`
    /// (`P23HotArchiveBug.cpp:456-482`).
    ///
    /// # Errors
    /// Returns `Err` if any literal fails to decode, or if two affected assets
    /// map to the same SAC contract id (mirrors stellar-core's
    /// `releaseAssert(inserted)`).
    pub fn new(
        network_id: &NetworkId,
        affected_assets: &[&str],
        corrupted_correct_pairs: &[(&str, &str)],
    ) -> Result<Self, String> {
        let mut sac_asset_map = std::collections::HashMap::new();
        for (i, encoded) in affected_assets.iter().enumerate() {
            let asset = Asset::from_xdr_base64(encoded, Limits::none())
                .map_err(|e| format!("P23 reconciler: failed to decode affected asset {i}: {e}"))?;
            let address = ScAddress::Contract(get_asset_contract_id(network_id, &asset));
            if sac_asset_map.insert(address, asset).is_some() {
                return Err(format!(
                    "P23 reconciler: duplicate SAC contract id for affected asset {i}"
                ));
            }
        }

        let mut key_to_entries = std::collections::HashMap::new();
        for (i, (corrupted_b64, correct_b64)) in corrupted_correct_pairs.iter().enumerate() {
            let corrupted =
                LedgerEntry::from_xdr_base64(corrupted_b64, Limits::none()).map_err(|e| {
                    format!("P23 reconciler: failed to decode corrupted entry {i}: {e}")
                })?;
            let correct = LedgerEntry::from_xdr_base64(correct_b64, Limits::none())
                .map_err(|e| format!("P23 reconciler: failed to decode correct entry {i}: {e}"))?;
            let key = henyey_common::entry_to_key(&corrupted);
            key_to_entries.insert(key, (correct, corrupted));
        }

        Ok(Self {
            sac_asset_map,
            key_to_entries,
        })
    }

    /// Compute the reconciliation event for a single hot-archive restore.
    ///
    /// Mirrors `getSACReconciliationEventAndTrackDiff`
    /// (`P23HotArchiveBug.cpp:524-588`). Returns `None` (no event) when:
    ///   * `protocol_version != 23` (equality gate),
    ///   * the restored key is not in the corrupted-entry table,
    ///   * the restored entry is not `CONTRACT_DATA`,
    ///   * the restored entry's contract is not one of the 12 affected SACs, or
    ///   * the restored balance equals the correct balance.
    ///
    /// The restored entry MUST byte-match the hardcoded corrupted entry
    /// (stellar-core `releaseAssert(restoredEntry == corruptedEntry)`); a
    /// mismatch is a data/usage error and returns `Err`.
    pub fn reconciliation_event(
        &self,
        restored_key: &LedgerKey,
        restored_entry: &LedgerEntry,
        protocol_version: u32,
    ) -> Result<Option<SacReconciliationInfo>, String> {
        if protocol_version != 23 {
            return Ok(None);
        }

        let Some((correct_entry, corrupted_entry)) = self.key_to_entries.get(restored_key) else {
            return Ok(None);
        };

        let LedgerEntryData::ContractData(cd) = &restored_entry.data else {
            return Ok(None);
        };

        let Some(asset) = self.sac_asset_map.get(&cd.contract) else {
            return Ok(None);
        };

        let (restored_balance, restored_owner) = get_sac_balance(restored_entry)?;

        // The restored entry must be exactly the hardcoded corrupted entry.
        if restored_entry != corrupted_entry {
            return Err(
                "P23 reconciler: restored entry does not match the hardcoded corrupted entry"
                    .to_string(),
            );
        }

        let (correct_balance, correct_owner) = get_sac_balance(correct_entry)?;

        // The balance addresses must match.
        if correct_owner != restored_owner {
            return Err("P23 reconciler: correct/restored balance owner mismatch".to_string());
        }

        if correct_balance == restored_balance {
            // No change in amount, no reconciliation event.
            return Ok(None);
        }

        let amount = restored_balance - correct_balance;
        Ok(Some(SacReconciliationInfo {
            asset: asset.clone(),
            mint_or_burn_address: correct_owner,
            amount,
        }))
    }

    /// Build the raw `ContractEvent`s for a batch of restored entries, in
    /// restore order.
    ///
    /// Each non-`None` reconciliation produces a mint event (amount > 0) or a
    /// burn event (amount < 0, emitting `-amount`). Mirrors the prepend loop in
    /// `InvokeHostFunctionOpFrame::setEvents` (`InvokeHostFunctionOpFrame.cpp:772-815`).
    ///
    /// The returned events carry no muxed-id/memo data
    /// (`allow_muxed_id_or_memo = false`), matching `makeMintEvent(.., false)` /
    /// `makeBurnEvent(..)` upstream.
    pub fn reconciliation_events_for_restores(
        &self,
        network_id: &NetworkId,
        restores: impl IntoIterator<Item = (LedgerKey, LedgerEntry)>,
        protocol_version: u32,
    ) -> Result<Vec<ContractEvent>, String> {
        let mut events = Vec::new();
        for (key, entry) in restores {
            if let Some(info) = self.reconciliation_event(&key, &entry, protocol_version)? {
                // getSACReconciliationEventAndTrackDiff never returns a zero diff.
                debug_assert_ne!(info.amount, 0);
                let address = get_address_with_dropped_muxed_info(&info.mint_or_burn_address);
                let event = if info.amount > 0 {
                    make_sac_reconciliation_event(
                        network_id,
                        "mint",
                        &info.asset,
                        &address,
                        info.amount,
                    )
                } else {
                    make_sac_reconciliation_event(
                        network_id,
                        "burn",
                        &info.asset,
                        &address,
                        -info.amount,
                    )
                };
                events.push(event);
            }
        }
        Ok(events)
    }
}

/// Build a raw mint/burn `ContractEvent` for SAC reconciliation, matching
/// `OpEventManager::makeMintEvent` / `makeBurnEvent` (no muxed-id/memo data).
fn make_sac_reconciliation_event(
    network_id: &NetworkId,
    topic: &str,
    asset: &Asset,
    address: &ScAddress,
    amount: i64,
) -> ContractEvent {
    let contract_id = get_asset_contract_id(network_id, asset);
    let topics = vec![
        make_symbol_scval(topic),
        ScVal::Address(address.clone()),
        make_sep0011_asset_string_scval(asset),
    ];
    make_event(contract_id, topics, make_i128_scval(amount))
}

/// Extract `(balance, owner)` from a SAC balance `CONTRACT_DATA` entry.
///
/// Port of `getSACBalance` (`P23HotArchiveBug.cpp:485-522`). For the affected
/// SACs the balance always fits in `[0, i64::MAX]`; the function enforces every
/// structural assumption as a hard error (matching stellar-core's
/// `releaseAssert`s), since the only entries reaching it are the hardcoded
/// corrupted/correct SAC balances.
fn get_sac_balance(le: &LedgerEntry) -> Result<(i64, ScAddress), String> {
    let LedgerEntryData::ContractData(cd) = &le.data else {
        return Err("P23 getSACBalance: entry is not CONTRACT_DATA".to_string());
    };

    if cd.durability != ContractDataDurability::Persistent {
        return Err("P23 getSACBalance: durability is not PERSISTENT".to_string());
    }

    let ScVal::Vec(Some(key_vec)) = &cd.key else {
        return Err("P23 getSACBalance: key is not a non-null SCVec".to_string());
    };
    if key_vec.0.len() != 2 {
        return Err("P23 getSACBalance: key SCVec is not length 2".to_string());
    }

    // The "Balance" symbol must be the first entry in the SCVec.
    let balance_symbol = ScVal::Symbol(
        ScSymbol::try_from("Balance".to_string()).expect("\"Balance\" is a valid SCSymbol"),
    );
    if key_vec.0[0] != balance_symbol {
        return Err("P23 getSACBalance: first key entry is not the \"Balance\" symbol".to_string());
    }

    let ScVal::Address(balance_owner) = &key_vec.0[1] else {
        return Err("P23 getSACBalance: second key entry is not an address".to_string());
    };

    let ScVal::Map(Some(val_map)) = &cd.val else {
        return Err("P23 getSACBalance: val is not a non-null SCMap".to_string());
    };
    if val_map.0.len() != 3 {
        return Err("P23 getSACBalance: val SCMap is not length 3".to_string());
    }

    let amount_symbol = ScVal::Symbol(
        ScSymbol::try_from("amount".to_string()).expect("\"amount\" is a valid SCSymbol"),
    );
    let amount_entry = &val_map.0[0];
    if amount_entry.key != amount_symbol {
        return Err(
            "P23 getSACBalance: first val entry key is not the \"amount\" symbol".to_string(),
        );
    }
    let ScVal::I128(parts) = &amount_entry.val else {
        return Err("P23 getSACBalance: amount value is not SCV_I128".to_string());
    };

    // For the range in question, hi is always 0 and lo fits in i64.
    if parts.hi != 0 {
        return Err("P23 getSACBalance: amount hi is not 0".to_string());
    }
    if parts.lo > i64::MAX as u64 {
        return Err("P23 getSACBalance: amount lo exceeds i64::MAX".to_string());
    }

    Ok((parts.lo as i64, balance_owner.clone()))
}

fn scval_symbol_bytes(value: &ScVal) -> Option<Vec<u8>> {
    match value {
        ScVal::Symbol(sym) => {
            let bytes: &[u8] = sym.0.as_ref();
            Some(bytes.to_vec())
        }
        _ => None,
    }
}

fn get_asset_from_event(event: &ContractEvent, network_id: &NetworkId) -> Option<Asset> {
    let contract_id = event.contract_id.as_ref()?;
    let ContractEventBody::V0(body) = &event.body;
    let asset_val = body.topics.last()?;
    let asset_str = match asset_val {
        ScVal::String(s) => std::str::from_utf8(s.0.as_ref()).ok()?,
        _ => return None,
    };

    let asset = if asset_str == "native" {
        Asset::Native
    } else if let Some((code, issuer_str)) = asset_str.split_once(':') {
        let issuer_pk = StrKeyPublicKey::from_strkey(issuer_str).ok()?;
        let issuer = AccountId::from(XdrPublicKey::PublicKeyTypeEd25519(
            stellar_xdr::curr::Uint256(*issuer_pk.as_bytes()),
        ));
        let code_bytes = code.as_bytes();
        if code_bytes.len() <= 4 {
            let mut buf = [0u8; 4];
            buf[..code_bytes.len()].copy_from_slice(code_bytes);
            Asset::CreditAlphanum4(stellar_xdr::curr::AlphaNum4 {
                asset_code: stellar_xdr::curr::AssetCode4(buf),
                issuer,
            })
        } else if code_bytes.len() <= 12 {
            let mut buf = [0u8; 12];
            buf[..code_bytes.len()].copy_from_slice(code_bytes);
            Asset::CreditAlphanum12(stellar_xdr::curr::AlphaNum12 {
                asset_code: stellar_xdr::curr::AssetCode12(buf),
                issuer,
            })
        } else {
            return None;
        }
    } else {
        return None;
    };

    let expected = get_asset_contract_id(network_id, &asset);
    if &expected != contract_id {
        return None;
    }
    Some(asset)
}

/// Backfill a single contract event to protocol-23 format.
///
/// Transforms transfer events involving an asset issuer into mint/burn events
/// and strips the admin topic from mint/clawback/set_authorized events.
fn backfill_event(event: &mut ContractEvent, network_id: &NetworkId) {
    let Some(asset) = get_asset_from_event(event, network_id) else {
        return;
    };

    let ContractEventBody::V0(body) = &mut event.body;
    let topics = body.topics.clone();
    if topics.is_empty() {
        return;
    }
    let Some(name) = scval_symbol_bytes(&topics[0]) else {
        return;
    };

    match name.as_slice() {
        b"transfer" => {
            if topics.len() != 4 {
                return;
            }
            let from = match &topics[1] {
                ScVal::Address(addr) => addr,
                _ => return,
            };
            let to = match &topics[2] {
                ScVal::Address(addr) => addr,
                _ => return,
            };
            let from_is_issuer = is_issuer(from, &asset);
            let to_is_issuer = is_issuer(to, &asset);
            if (from_is_issuer && to_is_issuer) || (!from_is_issuer && !to_is_issuer) {
                return;
            }
            let mut topics_vec: Vec<ScVal> = Vec::from(topics);
            if from_is_issuer {
                topics_vec[0] = make_symbol_scval("mint");
                topics_vec.remove(1);
            } else {
                topics_vec[0] = make_symbol_scval("burn");
                topics_vec.remove(2);
            }
            body.topics = topics_vec.try_into().unwrap_or_default();
        }
        b"mint" | b"clawback" | b"set_authorized" => {
            if topics.len() == 4 {
                let mut topics_vec: Vec<ScVal> = Vec::from(topics);
                topics_vec.remove(1);
                body.topics = topics_vec.try_into().unwrap_or_default();
            }
        }
        _ => {}
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use stellar_xdr::curr::{MuxedAccountMed25519, PublicKey, Uint256};

    fn test_account_id(seed: u8) -> AccountId {
        AccountId(PublicKey::PublicKeyTypeEd25519(Uint256([seed; 32])))
    }

    fn test_asset_alphanum4(seed: u8) -> Asset {
        Asset::CreditAlphanum4(stellar_xdr::curr::AlphaNum4 {
            asset_code: stellar_xdr::curr::AssetCode4([b'U', b'S', b'D', 0]),
            issuer: test_account_id(seed),
        })
    }

    fn test_asset_alphanum12(seed: u8) -> Asset {
        Asset::CreditAlphanum12(stellar_xdr::curr::AlphaNum12 {
            asset_code: stellar_xdr::curr::AssetCode12([
                b'L', b'O', b'N', b'G', b'A', b'S', b'S', b'E', b'T', 0, 0, 0,
            ]),
            issuer: test_account_id(seed),
        })
    }

    // === ClassicEventConfig tests ===

    #[test]
    fn test_classic_event_config_default() {
        let config = ClassicEventConfig::default();
        assert!(!config.emit_classic_events);
        assert!(!config.backfill_stellar_asset_events);
    }

    #[test]
    fn test_classic_event_config_events_enabled() {
        let config = ClassicEventConfig {
            emit_classic_events: true,
            backfill_stellar_asset_events: false,
        };
        assert!(config.events_enabled(25));
        assert!(config.events_enabled(23));

        let disabled = ClassicEventConfig {
            emit_classic_events: false,
            backfill_stellar_asset_events: false,
        };
        assert!(!disabled.events_enabled(25));
    }

    #[test]
    fn test_classic_event_config_backfill() {
        let config = ClassicEventConfig {
            emit_classic_events: true,
            backfill_stellar_asset_events: true,
        };
        // SAC-event backfill (the P23 reconciler) is active ONLY on protocol 23
        // (equality gate, issue #3126), and only when the flag is set.
        assert!(config.backfill_to_protocol23(23));
        assert!(!config.backfill_to_protocol23(22));
        assert!(!config.backfill_to_protocol23(24));

        // Flag off → never active, even on protocol 23 (off by default).
        let off = ClassicEventConfig {
            emit_classic_events: true,
            backfill_stellar_asset_events: false,
        };
        assert!(!off.backfill_to_protocol23(23));
    }

    // === OpEventManager tests ===

    #[test]
    fn test_op_event_manager_disabled() {
        let manager = OpEventManager::disabled();
        assert!(!manager.is_enabled());
        assert!(!manager.is_finalized());
        assert_eq!(manager.event_count(), 0);
    }

    #[test]
    fn test_op_event_manager_new_enabled() {
        let config = ClassicEventConfig {
            emit_classic_events: true,
            backfill_stellar_asset_events: false,
        };
        let manager = OpEventManager::new(
            true,  // meta_enabled
            false, // is_soroban
            25,    // protocol_version
            NetworkId::testnet(),
            Memo::None,
            config,
        );
        assert!(manager.is_enabled());
        assert!(!manager.is_finalized());
        assert_eq!(manager.event_count(), 0);
    }

    #[test]
    fn test_op_event_manager_new_disabled_no_meta() {
        let config = ClassicEventConfig {
            emit_classic_events: true,
            backfill_stellar_asset_events: false,
        };
        let manager = OpEventManager::new(
            false, // meta_enabled = false
            false,
            25,
            NetworkId::testnet(),
            Memo::None,
            config,
        );
        assert!(!manager.is_enabled());
    }

    #[test]
    fn test_op_event_manager_new_soroban_enabled() {
        let config = ClassicEventConfig {
            emit_classic_events: false, // classic events disabled
            backfill_stellar_asset_events: false,
        };
        let manager = OpEventManager::new(
            true,
            true, // is_soroban = true
            25,
            NetworkId::testnet(),
            Memo::None,
            config,
        );
        // Soroban operations enable events even without classic events config
        assert!(manager.is_enabled());
    }

    #[test]
    fn test_op_event_manager_finalize() {
        let config = ClassicEventConfig {
            emit_classic_events: true,
            backfill_stellar_asset_events: false,
        };
        let mut manager =
            OpEventManager::new(true, false, 25, NetworkId::testnet(), Memo::None, config);

        assert!(!manager.is_finalized());
        let events = manager.finalize();
        assert!(manager.is_finalized());
        assert!(events.is_empty());

        // Second finalize returns empty
        let events2 = manager.finalize();
        assert!(events2.is_empty());
    }

    #[test]
    fn test_op_event_manager_into_events() {
        let config = ClassicEventConfig {
            emit_classic_events: true,
            backfill_stellar_asset_events: false,
        };
        let manager =
            OpEventManager::new(true, false, 25, NetworkId::testnet(), Memo::None, config);
        let events = manager.into_events();
        assert!(events.is_empty());
    }

    #[test]
    fn test_op_event_manager_transfer_event() {
        let config = ClassicEventConfig {
            emit_classic_events: true,
            backfill_stellar_asset_events: false,
        };
        let mut manager =
            OpEventManager::new(true, false, 25, NetworkId::testnet(), Memo::None, config);

        let from = ScAddress::Account(test_account_id(1));
        let to = ScAddress::Account(test_account_id(2));
        manager.new_transfer_event(&Asset::Native, &from, &to, 1000, false);

        assert_eq!(manager.event_count(), 1);
        let events = manager.finalize();
        assert_eq!(events.len(), 1);

        // Verify event structure
        let event = &events[0];
        assert!(event.contract_id.is_some());
        assert_eq!(event.type_, ContractEventType::Contract);
    }

    #[test]
    fn test_op_event_manager_mint_event() {
        let config = ClassicEventConfig {
            emit_classic_events: true,
            backfill_stellar_asset_events: false,
        };
        let mut manager =
            OpEventManager::new(true, false, 25, NetworkId::testnet(), Memo::None, config);

        let to = ScAddress::Account(test_account_id(1));
        manager.new_mint_event(&Asset::Native, &to, 5000, false);

        assert_eq!(manager.event_count(), 1);
    }

    #[test]
    fn test_op_event_manager_mint_event_at_beginning() {
        let config = ClassicEventConfig {
            emit_classic_events: true,
            backfill_stellar_asset_events: false,
        };
        let mut manager =
            OpEventManager::new(true, false, 25, NetworkId::testnet(), Memo::None, config);

        let to1 = ScAddress::Account(test_account_id(1));
        let to2 = ScAddress::Account(test_account_id(2));

        // Add first mint event normally
        manager.new_mint_event(&Asset::Native, &to1, 1000, false);
        // Add second at beginning
        manager.new_mint_event_at_beginning(&Asset::Native, &to2, 2000);

        assert_eq!(manager.event_count(), 2);
    }

    #[test]
    fn test_op_event_manager_burn_event() {
        let config = ClassicEventConfig {
            emit_classic_events: true,
            backfill_stellar_asset_events: false,
        };
        let mut manager =
            OpEventManager::new(true, false, 25, NetworkId::testnet(), Memo::None, config);

        let from = ScAddress::Account(test_account_id(1));
        manager.new_burn_event(&Asset::Native, &from, 500);

        assert_eq!(manager.event_count(), 1);
    }

    #[test]
    fn test_op_event_manager_clawback_event() {
        let config = ClassicEventConfig {
            emit_classic_events: true,
            backfill_stellar_asset_events: false,
        };
        let mut manager =
            OpEventManager::new(true, false, 25, NetworkId::testnet(), Memo::None, config);

        let from = ScAddress::Account(test_account_id(1));
        manager.new_clawback_event(&test_asset_alphanum4(10), &from, 100);

        assert_eq!(manager.event_count(), 1);
    }

    #[test]
    fn test_op_event_manager_set_authorized_event() {
        let config = ClassicEventConfig {
            emit_classic_events: true,
            backfill_stellar_asset_events: false,
        };
        let mut manager =
            OpEventManager::new(true, false, 25, NetworkId::testnet(), Memo::None, config);

        let account = test_account_id(1);
        manager.new_set_authorized_event(&test_asset_alphanum4(10), &account, true);

        assert_eq!(manager.event_count(), 1);
    }

    #[test]
    fn test_op_event_manager_disabled_no_events() {
        let mut manager = OpEventManager::disabled();

        let from = ScAddress::Account(test_account_id(1));
        let to = ScAddress::Account(test_account_id(2));

        manager.new_transfer_event(&Asset::Native, &from, &to, 1000, false);
        manager.new_mint_event(&Asset::Native, &to, 500, false);
        manager.new_burn_event(&Asset::Native, &from, 200);

        // No events should be added when disabled
        assert_eq!(manager.event_count(), 0);
    }

    #[test]
    fn test_op_event_manager_no_events_after_finalize() {
        let config = ClassicEventConfig {
            emit_classic_events: true,
            backfill_stellar_asset_events: false,
        };
        let mut manager =
            OpEventManager::new(true, false, 25, NetworkId::testnet(), Memo::None, config);

        manager.finalize();
        assert!(manager.is_finalized());

        // Events after finalize should be ignored
        let to = ScAddress::Account(test_account_id(1));
        manager.new_mint_event(&Asset::Native, &to, 1000, false);
        manager.new_burn_event(&Asset::Native, &to, 500);

        assert_eq!(manager.event_count(), 0);
    }

    #[test]
    fn test_op_event_manager_transfer_with_issuer_check_mint() {
        let config = ClassicEventConfig {
            emit_classic_events: true,
            backfill_stellar_asset_events: false,
        };
        let mut manager =
            OpEventManager::new(true, false, 25, NetworkId::testnet(), Memo::None, config);

        let issuer_id = test_account_id(10);
        let asset = test_asset_alphanum4(10); // uses issuer_id as issuer
        let issuer = ScAddress::Account(issuer_id.clone());
        let receiver = ScAddress::Account(test_account_id(2));

        // From issuer to non-issuer = mint
        manager.event_for_transfer_with_issuer_check(&asset, &issuer, &receiver, 1000, false);

        assert_eq!(manager.event_count(), 1);
    }

    #[test]
    fn test_op_event_manager_transfer_with_issuer_check_burn() {
        let config = ClassicEventConfig {
            emit_classic_events: true,
            backfill_stellar_asset_events: false,
        };
        let mut manager =
            OpEventManager::new(true, false, 25, NetworkId::testnet(), Memo::None, config);

        let issuer_id = test_account_id(10);
        let asset = test_asset_alphanum4(10);
        let issuer = ScAddress::Account(issuer_id.clone());
        let sender = ScAddress::Account(test_account_id(2));

        // From non-issuer to issuer = burn
        manager.event_for_transfer_with_issuer_check(&asset, &sender, &issuer, 500, false);

        assert_eq!(manager.event_count(), 1);
    }

    #[test]
    fn test_op_event_manager_transfer_with_issuer_check_regular() {
        let config = ClassicEventConfig {
            emit_classic_events: true,
            backfill_stellar_asset_events: false,
        };
        let mut manager =
            OpEventManager::new(true, false, 25, NetworkId::testnet(), Memo::None, config);

        let asset = test_asset_alphanum4(10);
        let from = ScAddress::Account(test_account_id(1));
        let to = ScAddress::Account(test_account_id(2));

        // Neither is issuer = regular transfer
        manager.event_for_transfer_with_issuer_check(&asset, &from, &to, 1000, false);

        assert_eq!(manager.event_count(), 1);
    }

    #[test]
    fn test_op_event_manager_set_events() {
        let config = ClassicEventConfig {
            emit_classic_events: true,
            backfill_stellar_asset_events: false,
        };
        let mut manager = OpEventManager::new(
            true,
            true, // Soroban
            25,
            NetworkId::testnet(),
            Memo::None,
            config,
        );

        let event = ContractEvent {
            ext: stellar_xdr::curr::ExtensionPoint::V0,
            contract_id: None,
            type_: ContractEventType::Contract,
            body: ContractEventBody::V0(ContractEventV0 {
                topics: vec![].try_into().unwrap(),
                data: ScVal::Void,
            }),
        };

        manager.set_events(vec![event]);
        assert_eq!(manager.event_count(), 1);
    }

    // === TxEventManager tests ===

    #[test]
    fn test_tx_event_manager_disabled() {
        let manager = TxEventManager::disabled();
        assert!(!manager.is_enabled());
        assert!(!manager.is_finalized());
        assert_eq!(manager.event_count(), 0);
    }

    #[test]
    fn test_tx_event_manager_new_enabled() {
        let config = ClassicEventConfig {
            emit_classic_events: true,
            backfill_stellar_asset_events: false,
        };
        let manager = TxEventManager::new(true, 25, NetworkId::testnet(), config);
        assert!(manager.is_enabled());
        assert!(!manager.is_finalized());
    }

    #[test]
    fn test_tx_event_manager_fee_event() {
        let config = ClassicEventConfig {
            emit_classic_events: true,
            backfill_stellar_asset_events: false,
        };
        let mut manager = TxEventManager::new(true, 25, NetworkId::testnet(), config);

        let fee_source = test_account_id(1);
        manager.new_fee_event(&fee_source, -100, TransactionEventStage::BeforeAllTxs);

        assert_eq!(manager.event_count(), 1);
    }

    #[test]
    fn test_tx_event_manager_zero_fee_skipped() {
        let config = ClassicEventConfig {
            emit_classic_events: true,
            backfill_stellar_asset_events: false,
        };
        let mut manager = TxEventManager::new(true, 25, NetworkId::testnet(), config);

        let fee_source = test_account_id(1);
        manager.new_fee_event(&fee_source, 0, TransactionEventStage::BeforeAllTxs);

        // Zero amount should be skipped
        assert_eq!(manager.event_count(), 0);
    }

    #[test]
    fn test_tx_event_manager_charge_fee() {
        let config = ClassicEventConfig {
            emit_classic_events: true,
            backfill_stellar_asset_events: false,
        };
        let mut manager = TxEventManager::new(true, 25, NetworkId::testnet(), config);

        let fee_source = test_account_id(1);
        manager.charge_fee(&fee_source, 200, TransactionEventStage::BeforeAllTxs);

        assert_eq!(manager.event_count(), 1);
    }

    #[test]
    fn test_tx_event_manager_refund_fee() {
        let config = ClassicEventConfig {
            emit_classic_events: true,
            backfill_stellar_asset_events: false,
        };
        let mut manager = TxEventManager::new(true, 25, NetworkId::testnet(), config);

        let fee_source = test_account_id(1);
        manager.refund_fee(&fee_source, 50, TransactionEventStage::BeforeAllTxs);

        assert_eq!(manager.event_count(), 1);
    }

    #[test]
    fn test_tx_event_manager_finalize() {
        let config = ClassicEventConfig {
            emit_classic_events: true,
            backfill_stellar_asset_events: false,
        };
        let mut manager = TxEventManager::new(true, 25, NetworkId::testnet(), config);

        let fee_source = test_account_id(1);
        manager.new_fee_event(&fee_source, -100, TransactionEventStage::BeforeAllTxs);

        let events = manager.finalize();
        assert!(manager.is_finalized());
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].stage, TransactionEventStage::BeforeAllTxs);
    }

    #[test]
    fn test_tx_event_manager_into_events() {
        let config = ClassicEventConfig {
            emit_classic_events: true,
            backfill_stellar_asset_events: false,
        };
        let mut manager = TxEventManager::new(true, 25, NetworkId::testnet(), config);

        let fee_source = test_account_id(1);
        manager.charge_fee(&fee_source, 100, TransactionEventStage::BeforeAllTxs);

        let events = manager.into_events();
        assert_eq!(events.len(), 1);
    }

    // === EventManagerHierarchy tests ===

    #[test]
    fn test_event_manager_hierarchy_new() {
        let config = ClassicEventConfig {
            emit_classic_events: true,
            backfill_stellar_asset_events: false,
        };
        let hierarchy = EventManagerHierarchy::new(
            true,
            false,
            25,
            NetworkId::testnet(),
            Memo::None,
            config,
            3, // 3 operations
        );

        assert_eq!(hierarchy.operation_count(), 3);
        assert!(hierarchy.tx_event_manager_ref().is_enabled());
        assert!(hierarchy.op_event_manager_ref(0).is_enabled());
        assert!(hierarchy.op_event_manager_ref(1).is_enabled());
        assert!(hierarchy.op_event_manager_ref(2).is_enabled());
    }

    #[test]
    fn test_event_manager_hierarchy_disabled() {
        let hierarchy = EventManagerHierarchy::disabled(2);

        assert_eq!(hierarchy.operation_count(), 2);
        assert!(!hierarchy.tx_event_manager_ref().is_enabled());
        assert!(!hierarchy.op_event_manager_ref(0).is_enabled());
        assert!(!hierarchy.op_event_manager_ref(1).is_enabled());
    }

    #[test]
    fn test_event_manager_hierarchy_finalize() {
        let config = ClassicEventConfig {
            emit_classic_events: true,
            backfill_stellar_asset_events: false,
        };
        let mut hierarchy = EventManagerHierarchy::new(
            true,
            false,
            25,
            NetworkId::testnet(),
            Memo::None,
            config,
            2,
        );

        // Add events to op managers
        let to = ScAddress::Account(test_account_id(1));
        hierarchy
            .op_event_manager(0)
            .new_mint_event(&Asset::Native, &to, 1000, false);
        hierarchy
            .op_event_manager(1)
            .new_burn_event(&Asset::Native, &to, 500);

        // Add fee event
        hierarchy.tx_event_manager().charge_fee(
            &test_account_id(2),
            100,
            TransactionEventStage::BeforeAllTxs,
        );

        let (op_events, tx_events) = hierarchy.finalize();

        assert_eq!(op_events.len(), 2);
        assert_eq!(op_events[0].len(), 1);
        assert_eq!(op_events[1].len(), 1);
        assert_eq!(tx_events.len(), 1);
    }

    #[test]
    fn test_event_manager_hierarchy_into_events() {
        let config = ClassicEventConfig {
            emit_classic_events: true,
            backfill_stellar_asset_events: false,
        };
        let mut hierarchy = EventManagerHierarchy::new(
            true,
            false,
            25,
            NetworkId::testnet(),
            Memo::None,
            config,
            1,
        );

        let to = ScAddress::Account(test_account_id(1));
        hierarchy
            .op_event_manager(0)
            .new_mint_event(&Asset::Native, &to, 1000, false);

        let (op_events, tx_events) = hierarchy.into_events();
        assert_eq!(op_events.len(), 1);
        assert_eq!(op_events[0].len(), 1);
        assert!(tx_events.is_empty());
    }

    // === Helper function tests ===

    #[test]
    fn test_make_muxed_account_address_ed25519() {
        let pk = Uint256([1; 32]);
        let muxed = MuxedAccount::Ed25519(pk.clone());
        let addr = make_muxed_account_address(&muxed);

        match addr {
            ScAddress::Account(account) => {
                assert!(matches!(account.0, PublicKey::PublicKeyTypeEd25519(_)));
            }
            _ => panic!("Expected Account address"),
        }
    }

    #[test]
    fn test_make_muxed_account_address_muxed() {
        let muxed_account = MuxedAccountMed25519 {
            id: 12345,
            ed25519: Uint256([2; 32]),
        };
        let muxed = MuxedAccount::MuxedEd25519(muxed_account);
        let addr = make_muxed_account_address(&muxed);

        match addr {
            ScAddress::MuxedAccount(m) => {
                assert_eq!(m.id, 12345);
            }
            _ => panic!("Expected MuxedAccount address"),
        }
    }

    #[test]
    fn test_make_account_address() {
        let account = test_account_id(5);
        let addr = make_account_address(&account);

        match addr {
            ScAddress::Account(a) => {
                assert_eq!(a, account);
            }
            _ => panic!("Expected Account address"),
        }
    }

    #[test]
    fn test_make_claimable_balance_address() {
        let balance_id = ClaimableBalanceId::ClaimableBalanceIdTypeV0(Hash([42; 32]));
        let addr = make_claimable_balance_address(&balance_id);

        match addr {
            ScAddress::ClaimableBalance(b) => {
                assert_eq!(b, balance_id);
            }
            _ => panic!("Expected ClaimableBalance address"),
        }
    }

    #[test]
    fn test_get_address_with_dropped_muxed_info() {
        // Test with regular account - should return unchanged
        let account_addr = ScAddress::Account(test_account_id(1));
        let result = get_address_with_dropped_muxed_info(&account_addr);
        assert!(matches!(result, ScAddress::Account(_)));

        // Test with muxed account - should return regular account
        let muxed_addr = ScAddress::MuxedAccount(MuxedEd25519Account {
            id: 12345,
            ed25519: Uint256([3; 32]),
        });
        let result = get_address_with_dropped_muxed_info(&muxed_addr);
        match result {
            ScAddress::Account(_) => {}
            _ => panic!("Expected Account address after dropping muxed info"),
        }
    }

    #[test]
    fn test_is_issuer_native() {
        let addr = ScAddress::Account(test_account_id(1));
        // Native asset has no issuer
        assert!(!is_issuer(&addr, &Asset::Native));
    }

    #[test]
    fn test_is_issuer_alphanum4() {
        let issuer_id = test_account_id(10);
        let issuer_addr = ScAddress::Account(issuer_id.clone());
        let non_issuer_addr = ScAddress::Account(test_account_id(5));
        let asset = test_asset_alphanum4(10);

        assert!(is_issuer(&issuer_addr, &asset));
        assert!(!is_issuer(&non_issuer_addr, &asset));
    }

    #[test]
    fn test_is_issuer_alphanum12() {
        let issuer_id = test_account_id(20);
        let issuer_addr = ScAddress::Account(issuer_id.clone());
        let non_issuer_addr = ScAddress::Account(test_account_id(5));
        let asset = test_asset_alphanum12(20);

        assert!(is_issuer(&issuer_addr, &asset));
        assert!(!is_issuer(&non_issuer_addr, &asset));
    }

    #[test]
    fn test_is_issuer_non_account() {
        // Non-account addresses can't be issuers
        let pool_addr = ScAddress::LiquidityPool(Hash([0; 32]).into());
        let asset = test_asset_alphanum4(10);

        assert!(!is_issuer(&pool_addr, &asset));
    }

    #[test]
    fn test_make_symbol_scval() {
        let val = make_symbol_scval("transfer");
        match val {
            ScVal::Symbol(sym) => {
                let bytes: &[u8] = sym.0.as_ref();
                assert_eq!(bytes, b"transfer");
            }
            _ => panic!("Expected Symbol"),
        }
    }

    #[test]
    fn test_make_string_scval() {
        let val = make_string_scval("hello");
        match val {
            ScVal::String(s) => {
                let bytes: &[u8] = s.0.as_ref();
                assert_eq!(bytes, b"hello");
            }
            _ => panic!("Expected String"),
        }
    }

    #[test]
    fn test_make_i128_scval() {
        let val = make_i128_scval(1000);
        match val {
            ScVal::I128(parts) => {
                assert_eq!(parts.hi, 0);
                assert_eq!(parts.lo, 1000);
            }
            _ => panic!("Expected I128"),
        }

        // Test negative value
        let val_neg = make_i128_scval(-500);
        match val_neg {
            ScVal::I128(parts) => {
                // -500 as i128 has all bits set in high part
                assert_eq!(parts.hi, -1);
                // lo is the lower 64 bits of -500 as i128
                let expected_lo = -500i128 as u64;
                assert_eq!(parts.lo, expected_lo);
            }
            _ => panic!("Expected I128"),
        }
    }

    #[test]
    fn test_make_u64_scval() {
        let val = make_u64_scval(999);
        match val {
            ScVal::U64(v) => assert_eq!(v, 999),
            _ => panic!("Expected U64"),
        }
    }

    #[test]
    fn test_make_bytes_scval() {
        let bytes = vec![1, 2, 3, 4];
        let val = make_bytes_scval(&bytes);
        match val {
            ScVal::Bytes(b) => {
                let b_slice: &[u8] = b.as_ref();
                assert_eq!(b_slice, &[1, 2, 3, 4]);
            }
            _ => panic!("Expected Bytes"),
        }
    }

    #[test]
    fn test_asset_code_to_string() {
        // Test 4-char asset code
        let code4 = [b'U', b'S', b'D', 0];
        assert_eq!(asset_code_to_string(&code4), "USD");

        // Test 4-char full code
        let code4_full = [b'A', b'B', b'C', b'D'];
        assert_eq!(asset_code_to_string(&code4_full), "ABCD");

        // Test 12-char asset code with nulls
        let code12 = [b'L', b'O', b'N', b'G', 0, 0, 0, 0, 0, 0, 0, 0];
        assert_eq!(asset_code_to_string(&code12), "LONG");
    }

    #[test]
    fn test_make_sep0011_asset_string_scval_native() {
        let val = make_sep0011_asset_string_scval(&Asset::Native);
        match val {
            ScVal::String(s) => {
                let bytes: &[u8] = s.0.as_ref();
                assert_eq!(bytes, b"native");
            }
            _ => panic!("Expected String"),
        }
    }

    #[test]
    fn test_make_sep0011_asset_string_scval_alphanum4() {
        let asset = test_asset_alphanum4(1);
        let val = make_sep0011_asset_string_scval(&asset);
        match val {
            ScVal::String(s) => {
                let str_val = std::str::from_utf8(s.0.as_ref()).unwrap();
                assert!(str_val.starts_with("USD:"));
            }
            _ => panic!("Expected String"),
        }
    }

    #[test]
    fn test_get_asset_contract_id() {
        let network_id = NetworkId::testnet();
        let contract_id = get_asset_contract_id(&network_id, &Asset::Native);

        // Just verify it returns a valid contract ID
        assert!(!contract_id.0 .0.is_empty());
    }

    #[test]
    fn test_scval_symbol_bytes() {
        let symbol = make_symbol_scval("test");
        let bytes = scval_symbol_bytes(&symbol);
        assert_eq!(bytes, Some(b"test".to_vec()));

        // Non-symbol should return None
        let non_symbol = ScVal::Void;
        assert_eq!(scval_symbol_bytes(&non_symbol), None);
    }

    #[test]
    fn test_make_possible_muxed_data_simple() {
        let to = ScAddress::Account(test_account_id(1));
        let data = make_possible_muxed_data(&to, 1000, &Memo::None, false);

        // Without muxed or memo, should be simple i128
        match data {
            ScVal::I128(_) => {}
            _ => panic!("Expected I128 for simple case"),
        }
    }

    #[test]
    fn test_make_possible_muxed_data_with_muxed() {
        let to = ScAddress::MuxedAccount(MuxedEd25519Account {
            id: 12345,
            ed25519: Uint256([1; 32]),
        });
        let data = make_possible_muxed_data(&to, 1000, &Memo::None, true);

        // With muxed account and allow_muxed_id_or_memo, should be a Map
        match data {
            ScVal::Map(Some(_)) => {}
            _ => panic!("Expected Map for muxed case"),
        }
    }

    #[test]
    fn test_make_possible_muxed_data_with_memo() {
        let to = ScAddress::Account(test_account_id(1));
        let memo = Memo::Id(999);
        let data = make_possible_muxed_data(&to, 1000, &memo, true);

        // With memo and allow_muxed_id_or_memo, should be a Map
        match data {
            ScVal::Map(Some(_)) => {}
            _ => panic!("Expected Map for memo case"),
        }
    }

    #[test]
    fn test_make_classic_memo_scval_text() {
        let memo = Memo::Text(b"hello".to_vec().try_into().unwrap());
        let val = make_classic_memo_scval(&memo);
        match val {
            ScVal::String(s) => {
                let bytes: &[u8] = s.0.as_ref();
                assert_eq!(bytes, b"hello");
            }
            _ => panic!("Expected String"),
        }
    }

    #[test]
    fn test_make_classic_memo_scval_id() {
        let memo = Memo::Id(12345);
        let val = make_classic_memo_scval(&memo);
        match val {
            ScVal::U64(v) => assert_eq!(v, 12345),
            _ => panic!("Expected U64"),
        }
    }

    #[test]
    fn test_make_classic_memo_scval_hash() {
        let memo = Memo::Hash(Hash([42; 32]));
        let val = make_classic_memo_scval(&memo);
        match val {
            ScVal::Bytes(b) => {
                let bytes: &[u8] = b.as_ref();
                assert_eq!(bytes, &[42; 32]);
            }
            _ => panic!("Expected Bytes"),
        }
    }

    #[test]
    fn test_make_classic_memo_scval_return() {
        let memo = Memo::Return(Hash([99; 32]));
        let val = make_classic_memo_scval(&memo);
        match val {
            ScVal::Bytes(b) => {
                let bytes: &[u8] = b.as_ref();
                assert_eq!(bytes, &[99; 32]);
            }
            _ => panic!("Expected Bytes"),
        }
    }

    #[test]
    #[should_panic(expected = "memo type cannot be None")]
    fn test_make_classic_memo_scval_none_panics() {
        make_classic_memo_scval(&Memo::None);
    }

    // ====================================================================
    // Protocol 23 SAC mint/burn event reconciler tests (issue #3126).
    // ====================================================================

    use stellar_xdr::curr::{
        ContractDataEntry, ExtensionPoint, LedgerEntryExt, ScSymbol as XdrScSymbol, ScVec, WriteXdr,
    };

    /// Build a SAC-balance `CONTRACT_DATA` LedgerEntry for the given SAC
    /// contract id, owner, and amount, mirroring the structure `getSACBalance`
    /// expects (key = ["Balance", owner]; val = {amount, authorized, clawback}).
    fn sac_balance_entry(contract_id: ContractId, owner: &ScAddress, amount: i64) -> LedgerEntry {
        let key = ScVal::Vec(Some(ScVec(
            vec![
                ScVal::Symbol(XdrScSymbol::try_from("Balance").unwrap()),
                ScVal::Address(owner.clone()),
            ]
            .try_into()
            .unwrap(),
        )));
        let val = ScVal::Map(Some(ScMap(
            vec![
                ScMapEntry {
                    key: ScVal::Symbol(XdrScSymbol::try_from("amount").unwrap()),
                    val: make_i128_scval(amount),
                },
                ScMapEntry {
                    key: ScVal::Symbol(XdrScSymbol::try_from("authorized").unwrap()),
                    val: ScVal::Bool(true),
                },
                ScMapEntry {
                    key: ScVal::Symbol(XdrScSymbol::try_from("clawback").unwrap()),
                    val: ScVal::Bool(false),
                },
            ]
            .try_into()
            .unwrap(),
        )));
        LedgerEntry {
            last_modified_ledger_seq: 0,
            data: LedgerEntryData::ContractData(ContractDataEntry {
                ext: ExtensionPoint::V0,
                contract: ScAddress::Contract(contract_id),
                key,
                durability: ContractDataDurability::Persistent,
                val,
            }),
            ext: LedgerEntryExt::V0,
        }
    }

    fn b64(entry: &LedgerEntry) -> String {
        entry.to_xdr_base64(Limits::none()).unwrap()
    }

    fn b64_asset(asset: &Asset) -> String {
        asset.to_xdr_base64(Limits::none()).unwrap()
    }

    /// Build a reconciler whose single affected asset is `asset`, with one
    /// corrupted/correct pair: a SAC balance owned by `owner` that was restored
    /// as `corrupted_amount` but should have been `correct_amount`.
    fn single_pair_reconciler(
        network_id: &NetworkId,
        asset: &Asset,
        owner: &ScAddress,
        corrupted_amount: i64,
        correct_amount: i64,
    ) -> (P23SacReconciler, LedgerKey, LedgerEntry) {
        let contract_id = get_asset_contract_id(network_id, asset);
        let corrupted = sac_balance_entry(contract_id.clone(), owner, corrupted_amount);
        let correct = sac_balance_entry(contract_id, owner, correct_amount);
        let key = henyey_common::entry_to_key(&corrupted);

        let asset_b64 = b64_asset(asset);
        let corrupted_b64 = b64(&corrupted);
        let correct_b64 = b64(&correct);
        let reconciler = P23SacReconciler::new(
            network_id,
            &[asset_b64.as_str()],
            &[(corrupted_b64.as_str(), correct_b64.as_str())],
        )
        .unwrap();
        (reconciler, key, corrupted)
    }

    fn contract_owner(seed: u8) -> ScAddress {
        ScAddress::Contract(ContractId(Hash([seed; 32])))
    }

    #[test]
    fn test_p23_reconciler_mint_on_positive_diff() {
        let net = NetworkId::testnet();
        let asset = test_asset_alphanum4(7);
        let owner = contract_owner(9);
        // restored (corrupted) = 1000, correct = 600 → diff +400 → mint.
        let (rec, key, corrupted) = single_pair_reconciler(&net, &asset, &owner, 1000, 600);
        let info = rec
            .reconciliation_event(&key, &corrupted, 23)
            .unwrap()
            .expect("expected a reconciliation event");
        assert_eq!(info.asset, asset);
        assert_eq!(info.mint_or_burn_address, owner);
        assert_eq!(info.amount, 400);

        // Built event is a mint for +400.
        let events = rec
            .reconciliation_events_for_restores(&net, [(key, corrupted)], 23)
            .unwrap();
        assert_eq!(events.len(), 1);
        let ContractEventBody::V0(body) = &events[0].body;
        assert_eq!(body.topics.first().unwrap(), &make_symbol_scval("mint"));
        assert_eq!(body.data, make_i128_scval(400));
    }

    #[test]
    fn test_p23_reconciler_burn_on_negative_diff() {
        let net = NetworkId::testnet();
        let asset = test_asset_alphanum12(11);
        let owner = contract_owner(3);
        // restored = 250, correct = 900 → diff -650 → burn of 650.
        let (rec, key, corrupted) = single_pair_reconciler(&net, &asset, &owner, 250, 900);
        let info = rec
            .reconciliation_event(&key, &corrupted, 23)
            .unwrap()
            .expect("expected a reconciliation event");
        assert_eq!(info.amount, -650);

        let events = rec
            .reconciliation_events_for_restores(&net, [(key, corrupted)], 23)
            .unwrap();
        assert_eq!(events.len(), 1);
        let ContractEventBody::V0(body) = &events[0].body;
        assert_eq!(body.topics.first().unwrap(), &make_symbol_scval("burn"));
        // A burn emits the positive magnitude.
        assert_eq!(body.data, make_i128_scval(650));
    }

    #[test]
    fn test_p23_reconciler_none_on_equal_balance() {
        let net = NetworkId::testnet();
        let asset = test_asset_alphanum4(7);
        let owner = contract_owner(9);
        let (rec, key, corrupted) = single_pair_reconciler(&net, &asset, &owner, 500, 500);
        assert!(rec
            .reconciliation_event(&key, &corrupted, 23)
            .unwrap()
            .is_none());
    }

    #[test]
    fn test_p23_reconciler_none_for_non_contract_data() {
        let net = NetworkId::testnet();
        let asset = test_asset_alphanum4(7);
        let owner = contract_owner(9);
        let (rec, key, _corrupted) = single_pair_reconciler(&net, &asset, &owner, 1000, 600);
        // An account entry is not CONTRACT_DATA → None even if the key matched.
        let account_entry = LedgerEntry {
            last_modified_ledger_seq: 0,
            data: LedgerEntryData::Account(stellar_xdr::curr::AccountEntry {
                account_id: test_account_id(1),
                balance: 0,
                seq_num: stellar_xdr::curr::SequenceNumber(0),
                num_sub_entries: 0,
                inflation_dest: None,
                flags: 0,
                home_domain: Default::default(),
                thresholds: stellar_xdr::curr::Thresholds([0; 4]),
                signers: Default::default(),
                ext: stellar_xdr::curr::AccountEntryExt::V0,
            }),
            ext: LedgerEntryExt::V0,
        };
        assert!(rec
            .reconciliation_event(&key, &account_entry, 23)
            .unwrap()
            .is_none());
    }

    #[test]
    fn test_p23_reconciler_none_for_key_not_in_corrupted_table() {
        let net = NetworkId::testnet();
        let asset = test_asset_alphanum4(7);
        let owner = contract_owner(9);
        let (rec, _key, corrupted) = single_pair_reconciler(&net, &asset, &owner, 1000, 600);
        // A different owner → a different SAC balance key not in the table.
        let other_owner = contract_owner(42);
        let contract_id = get_asset_contract_id(&net, &asset);
        let other_entry = sac_balance_entry(contract_id, &other_owner, 1000);
        let other_key = henyey_common::entry_to_key(&other_entry);
        assert_ne!(other_key, henyey_common::entry_to_key(&corrupted));
        assert!(rec
            .reconciliation_event(&other_key, &other_entry, 23)
            .unwrap()
            .is_none());
    }

    #[test]
    fn test_p23_reconciler_none_for_unaffected_asset() {
        let net = NetworkId::testnet();
        // The reconciler knows about asset A, but the restored entry belongs to
        // asset B's SAC contract → not in sac_asset_map → None.
        let asset_a = test_asset_alphanum4(7);
        let owner = contract_owner(9);
        let (rec, _key, _corrupted) = single_pair_reconciler(&net, &asset_a, &owner, 1000, 600);

        let asset_b = test_asset_alphanum4(8);
        let contract_b = get_asset_contract_id(&net, &asset_b);
        let entry_b = sac_balance_entry(contract_b, &owner, 1000);
        // Re-key against the corrupted table by reusing asset A's key is not
        // possible; the entry's own key won't be in the table, so None.
        let key_b = henyey_common::entry_to_key(&entry_b);
        assert!(rec
            .reconciliation_event(&key_b, &entry_b, 23)
            .unwrap()
            .is_none());
    }

    #[test]
    fn test_p23_reconciler_none_off_protocol_23() {
        let net = NetworkId::testnet();
        let asset = test_asset_alphanum4(7);
        let owner = contract_owner(9);
        let (rec, key, corrupted) = single_pair_reconciler(&net, &asset, &owner, 1000, 600);
        // Same data that mints at p23, but p22/p24 → equality gate → None.
        assert!(rec
            .reconciliation_event(&key, &corrupted, 22)
            .unwrap()
            .is_none());
        assert!(rec
            .reconciliation_event(&key, &corrupted, 24)
            .unwrap()
            .is_none());
    }

    #[test]
    fn test_backfill_to_protocol23_gating() {
        let on = ClassicEventConfig {
            emit_classic_events: true,
            backfill_stellar_asset_events: true,
        };
        assert!(on.backfill_to_protocol23(23));
        assert!(!on.backfill_to_protocol23(22));
        assert!(!on.backfill_to_protocol23(24));

        let off = ClassicEventConfig {
            emit_classic_events: true,
            backfill_stellar_asset_events: false,
        };
        assert!(!off.backfill_to_protocol23(23));
        assert!(!ClassicEventConfig::default().backfill_to_protocol23(23));
    }

    /// The 12 affected-asset base64 literals, transcribed verbatim from
    /// `crates/ledger/src/p23_hot_archive_bug_data.rs`
    /// (`P23_CORRUPTED_AFFECTED_ASSETS`, in turn from
    /// `stellar-core/src/ledger/P23HotArchiveBugData.cpp:10040-10063`). Inlined
    /// here because `henyey-tx` cannot depend on `henyey-ledger` (cycle); the
    /// ledger crate owns the canonical array and feeds it to `P23SacReconciler`
    /// at runtime. `henyey-ledger` independently asserts the count is 12.
    const P23_AFFECTED_ASSETS_B64: [&str; 12] = [
        "AAAAAA==",
        "AAAAAVVTREMAAAAAO5kROA7+mIugqJAOsc/kTzZvfb6Ua+0HckD39iTfFcU=",
        "AAAAAlVTVFJZAAAAAAAAAAAAAACjihh9bUETXn31yZR0SCigSeDgfCEzu07aIZmcZ+LBNg==",
        "AAAAAUJMTkQAAAAA0kPMJPZPS8rFR7mhiMujiCWd5dqlc77zlNPybSXzIdI=",
        "AAAAAUFRVUEAAAAAW5QuU6wzyP0KgMx8GxqF19g4qcQZd6rRizrwV/jjPfA=",
        "AAAAAVNIWAAAAAAA5TjI9zmT/KKxDcmCdh/l6VFSRFEDH9BHJShFf036PqQ=",
        "AAAAAVNUSwAAAAAAfW7IaowF6psp0uDj0Z0IMxUWYHOCFHiZgj3bIdtXwQw=",
        "AAAAAU5MVAAAAAAAL67mruJq8g1bj8lCiNVHbidAzKmg3NSrNjUbTCiAbaw=",
        "AAAAAkxJQlJFAAAAAAAAAAAAAAAwIVlEE0w4nxTdJxIEerrFsVIN8WutTUzlhQJHaLR/Mg==",
        "AAAAAUtQT1AAAAAA43Oh2jfBBl1sJz15HPylcD8Gbbh9XTWsf6/BAUfUlcc=",
        "AAAAAUtBTEUAAAAAR1vypFiHKHeKgnE2nuA5VhED/841SUAs4KR5zr8bCfU=",
        "AAAAAVNCSQAAAAAApjwUrepRcx7ax1frLCxCibZizKJrumi41+2I+RW8EAo=",
    ];

    #[test]
    fn test_p23_affected_assets_array_decodes() {
        // All 12 base64 strings decode to a valid Asset, and the 12 SAC contract
        // ids are pairwise distinct (mirrors stellar-core's
        // releaseAssert(inserted) in the reconciler constructor).
        let net = NetworkId::mainnet();
        assert_eq!(P23_AFFECTED_ASSETS_B64.len(), 12);
        let mut ids = std::collections::HashSet::new();
        for (i, s) in P23_AFFECTED_ASSETS_B64.iter().enumerate() {
            let asset = Asset::from_xdr_base64(s, Limits::none())
                .unwrap_or_else(|e| panic!("affected asset {i} failed to decode: {e}"));
            let id = get_asset_contract_id(&net, &asset);
            assert!(ids.insert(id), "duplicate SAC contract id at index {i}");
        }
        assert_eq!(ids.len(), 12);
    }

    #[test]
    fn test_p23_reconciler_constructor_rejects_duplicate_asset_ids() {
        // Two identical assets → same SAC contract id → constructor errors,
        // mirroring stellar-core's releaseAssert(inserted).
        let net = NetworkId::testnet();
        let asset = b64_asset(&test_asset_alphanum4(7));
        let err = P23SacReconciler::new(&net, &[asset.as_str(), asset.as_str()], &[]);
        assert!(err.is_err());
    }
}
