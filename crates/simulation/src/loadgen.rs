//! Deterministic load and transaction generation for simulation workloads.
//!
//! This module provides two levels of load generation:
//!
//! 1. **Simple stateless API** (`LoadGenerator::step_plan`, `TxGenerator::payment_series`):
//!    Pre-computes transaction batches for deterministic manual-close simulations.
//!
//! 2. **Rich stateful API** (mirroring stellar-core's `LoadGenerator`/`TxGenerator`):
//!    Manages account pools, cumulative-rate-limited submission, sequence number
//!    refresh, and `txBAD_SEQ` retry logic for long-running consensus simulations.

use std::collections::{BTreeMap, HashSet};
use std::sync::Arc;
use std::time::{Duration, Instant};

use henyey_app::App;
use henyey_common::{Hash256, LedgerSeq, NetworkId};
use henyey_crypto::SecretKey;
use henyey_herder::TxQueueResult;
use henyey_tx::TxResultCode;
use stellar_xdr::curr::{
    AccountId, Asset, ContractDataDurability, ContractId, ContractIdPreimage,
    ContractIdPreimageFromAddress, CreateAccountOp, Hash, LedgerKey, LedgerKeyContractData, Memo,
    MuxedAccount, Operation, OperationBody, PaymentOp, Preconditions, PublicKey, ScAddress, ScVal,
    SequenceNumber, Transaction, TransactionEnvelope, TransactionExt, TransactionV1Envelope,
    Uint256, VecM,
};
use tracing::{debug, info, warn};

use crate::loadgen_soroban::{
    compute_contract_id, contract_code_key, contract_instance_key, make_account_address,
    make_contract_address, BatchTransfer, ContractInvocation, SacTransfer, SorobanTxBuilder,
};

// ---------------------------------------------------------------------------
// Constants (matching stellar-core LoadGenerator.cpp)
// ---------------------------------------------------------------------------

/// Interval between load generation steps (milliseconds).
const STEP_MSECS: u64 = 100;

/// Maximum retries on `txBAD_SEQ` before giving up.
const TX_SUBMIT_MAX_TRIES: u32 = 10;

/// Sentinel account ID for the network root account.
const ROOT_ACCOUNT_ID: u64 = u64::MAX;

/// Default WASM size for random upload transactions.
const DEFAULT_WASM_SIZE: usize = 35_000;

/// Default inclusion fee for Soroban transactions.
const DEFAULT_SOROBAN_INCLUSION_FEE: u32 = 100;

/// Base CPU instruction budget for contract invocations.
const INVOKE_BASE_INSTRUCTIONS: u32 = 2_000_000;

/// Random range added on top of `INVOKE_BASE_INSTRUCTIONS`.
const INVOKE_INSTRUCTIONS_RANGE: u64 = 1_000_000;

/// Guest CPU cycles per instruction (stellar-core ratio).
const GUEST_CYCLES_PER_INSTRUCTION: u64 = 80;

/// Host CPU cycles per instruction (stellar-core ratio).
const HOST_CYCLES_PER_INSTRUCTION: u64 = 5030;

/// Base disk-read bytes for contract invocations (before adding entry sizes).
const INVOKE_BASE_READ_BYTES: u32 = 5_000;

// ---------------------------------------------------------------------------
// ContractInstance (Soroban)
// ---------------------------------------------------------------------------

/// Deployed contract instance metadata for load generation.
///
/// Matches stellar-core `TxGenerator::ContractInstance`.
#[derive(Debug, Clone)]
pub struct ContractInstance {
    /// Read-only ledger keys: `[contract_code, contract_instance]`.
    pub read_only_keys: Vec<LedgerKey>,
    /// Contract address.
    pub contract_id: Hash256,
    /// Estimated size of contract entries in bytes.
    pub contract_entries_size: u32,
}

// ---------------------------------------------------------------------------
// LoadGenMode
// ---------------------------------------------------------------------------

/// Load generation mode.
///
/// Matches stellar-core `LoadGenMode`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LoadGenMode {
    /// Classic payment transactions (1 stroop per tx).
    Pay,
    /// Deploy random Wasm blobs (overlay/herder stress testing).
    SorobanUpload,
    /// Two-phase setup: upload test Wasm, then deploy N contract instances.
    /// Prerequisite for `SorobanInvoke`.
    SorobanInvokeSetup,
    /// Invoke resource-intensive contract transactions on instances created
    /// by `SorobanInvokeSetup`.
    SorobanInvoke,
    /// Blend of Pay, SorobanUpload, and SorobanInvoke at configurable weights.
    MixedClassicSoroban,
}

impl LoadGenMode {
    /// Returns `true` for any Soroban mode.
    pub fn is_soroban(self) -> bool {
        matches!(
            self,
            Self::SorobanUpload
                | Self::SorobanInvokeSetup
                | Self::SorobanInvoke
                | Self::MixedClassicSoroban
        )
    }

    /// Returns `true` for setup-only modes (no ongoing tx submission).
    pub fn is_soroban_setup(self) -> bool {
        matches!(self, Self::SorobanInvokeSetup)
    }

    /// Returns `true` for modes that submit transactions in a continuous loop.
    pub fn is_load(self) -> bool {
        matches!(
            self,
            Self::Pay | Self::SorobanUpload | Self::SorobanInvoke | Self::MixedClassicSoroban
        )
    }

    /// Returns `true` for modes that invoke previously deployed contracts.
    ///
    /// Matches stellar-core `modeSetsUpInvoke()` | `modeInvokes()` invoke check.
    pub fn mode_invokes(self) -> bool {
        matches!(self, Self::SorobanInvoke | Self::MixedClassicSoroban)
    }

    /// Returns `true` for modes that set up contract instances (upload + deploy).
    ///
    /// Matches stellar-core `modeSetsUpInvoke()`.
    pub fn mode_sets_up_invoke(self) -> bool {
        matches!(self, Self::SorobanInvokeSetup)
    }
}

// ---------------------------------------------------------------------------
// GeneratedLoadConfig (enriched)
// ---------------------------------------------------------------------------

/// Configuration for a load generation run.
///
/// Matches stellar-core `GeneratedLoadConfig` (Pay-mode fields).
#[derive(Debug, Clone)]
pub struct GeneratedLoadConfig {
    /// Load generation mode.
    pub mode: LoadGenMode,
    /// Number of source accounts in the pool.
    pub n_accounts: u32,
    /// Account ID offset (accounts are numbered `offset..offset+n_accounts`).
    pub offset: u32,
    /// Remaining transactions to submit.
    pub n_txs: u32,
    /// Target transaction rate (transactions per second).
    pub tx_rate: u32,
    /// Optional maximum fee rate (random fee in `[base_fee, max_fee_rate]`).
    pub max_fee_rate: Option<u32>,
    /// Whether to skip transactions rejected for low fee instead of failing.
    pub skip_low_fee_txs: bool,
    /// Spike interval in seconds (0 = no spikes). Every `spike_interval`
    /// seconds, an additional burst of `spike_size` transactions is injected.
    ///
    /// Matches stellar-core `GeneratedLoadConfig::spikeInterval` / `spikeSize`.
    pub spike_interval: u64,
    /// Number of extra transactions per spike burst.
    pub spike_size: u32,

    // --- Soroban-specific fields ---
    /// Number of contract instances to deploy (for `SorobanInvokeSetup`).
    pub n_instances: u32,
    /// Number of Wasm blobs to upload (for `SorobanInvokeSetup`).
    pub n_wasms: u32,
    /// Minimum Soroban success percentage (0-100).
    pub min_soroban_percent_success: u32,
    /// Weight for Pay mode in `MixedClassicSoroban`.
    pub mix_pay_weight: u32,
    /// Weight for SorobanUpload in `MixedClassicSoroban`.
    pub mix_upload_weight: u32,
    /// Weight for SorobanInvoke in `MixedClassicSoroban`.
    pub mix_invoke_weight: u32,

    // --- Legacy simple-mode fields (backward compat) ---
    /// Account names for simple step_plan mode.
    pub accounts: Vec<String>,
    /// Transactions per step in simple mode.
    pub txs_per_step: usize,
    /// Number of steps in simple mode.
    pub steps: usize,
    /// Fixed fee bid for simple mode.
    pub fee_bid: u32,
    /// Payment amount for simple mode.
    pub amount: i64,
}

impl Default for GeneratedLoadConfig {
    fn default() -> Self {
        Self {
            mode: LoadGenMode::Pay,
            n_accounts: 100,
            offset: 0,
            n_txs: 0,
            tx_rate: 10,
            max_fee_rate: None,
            skip_low_fee_txs: false,
            spike_interval: 0,
            spike_size: 0,
            n_instances: 0,
            n_wasms: 0,
            min_soroban_percent_success: 0,
            mix_pay_weight: 1,
            mix_upload_weight: 1,
            mix_invoke_weight: 1,
            accounts: Vec::new(),
            txs_per_step: 0,
            steps: 0,
            fee_bid: 100,
            amount: 1,
        }
    }
}

impl GeneratedLoadConfig {
    /// Create a Pay-mode load config.
    pub fn tx_load(
        n_accounts: u32,
        n_txs: u32,
        tx_rate: u32,
        offset: u32,
        max_fee_rate: Option<u32>,
    ) -> Self {
        Self {
            mode: LoadGenMode::Pay,
            n_accounts,
            offset,
            n_txs,
            tx_rate,
            max_fee_rate,
            ..Default::default()
        }
    }

    /// Returns `true` when all transactions have been submitted.
    ///
    /// Matches stellar-core `GeneratedLoadConfig::isDone()`.
    pub fn is_done(&self) -> bool {
        if self.mode.is_soroban_setup() {
            self.n_instances == 0
        } else {
            self.n_txs == 0
        }
    }

    /// Returns `true` when there are still transactions to submit.
    ///
    /// Matches stellar-core `GeneratedLoadConfig::areTxsRemaining()`.
    pub fn are_txs_remaining(&self) -> bool {
        self.n_txs != 0
    }
}

// ---------------------------------------------------------------------------
// TestAccount (account cache entry)
// ---------------------------------------------------------------------------

/// Cached account with a deterministic keypair and mutable sequence number.
///
/// Matches stellar-core `TestAccount`.
#[derive(Debug, Clone)]
pub struct TestAccount {
    pub secret_key: SecretKey,
    pub account_id: AccountId,
    pub sequence_number: i64,
}

impl TestAccount {
    /// Create from a deterministic name (padded to 32 bytes as seed).
    fn from_name(name: &str, initial_seq: i64) -> Self {
        let seed = deterministic_seed(name);
        let sk = SecretKey::from_seed(&seed);
        let pk = sk.public_key();
        let account_id = AccountId(PublicKey::PublicKeyTypeEd25519(Uint256(*pk.as_bytes())));
        Self {
            secret_key: sk,
            account_id,
            sequence_number: initial_seq,
        }
    }

    /// Increment and return the next sequence number.
    pub fn next_sequence_number(&mut self) -> i64 {
        self.sequence_number += 1;
        self.sequence_number
    }
}

pub(crate) use henyey_common::deterministic_seed;

// ---------------------------------------------------------------------------
// TxGenerator (enriched)
// ---------------------------------------------------------------------------

/// Transaction generator with an account cache.
///
/// Matches stellar-core `TxGenerator`.
pub struct TxGenerator {
    /// Cached accounts: numeric ID → TestAccount.
    accounts: BTreeMap<u64, TestAccount>,
    /// Reference to the app (for DB lookups and fee queries).
    pub(crate) app: Arc<App>,
    /// Network passphrase for transaction signing.
    pub(crate) network_passphrase: String,
}

impl TxGenerator {
    pub fn new(app: Arc<App>, network_passphrase: String) -> Self {
        Self {
            accounts: BTreeMap::new(),
            app,
            network_passphrase,
        }
    }

    fn soroban_builder(&self) -> SorobanTxBuilder {
        SorobanTxBuilder::new(self.network_passphrase.clone())
    }

    fn next_source_sequence(&mut self, account_id: u64, ledger_num: LedgerSeq) -> (SecretKey, i64) {
        let source = self.find_account(account_id, ledger_num);
        (source.secret_key.clone(), source.next_sequence_number())
    }

    /// Look up or create an account in the cache.
    ///
    /// Matches stellar-core `TxGenerator::findAccount()`.
    /// For the root account, uses the network root secret key.
    /// For numbered accounts, creates a deterministic keypair from `"TestAccount-{id}"`.
    pub fn find_account(&mut self, account_id: u64, ledger_num: LedgerSeq) -> &mut TestAccount {
        if let std::collections::btree_map::Entry::Vacant(entry) = self.accounts.entry(account_id) {
            let account = if account_id == ROOT_ACCOUNT_ID {
                let network_id = NetworkId::from_passphrase(&self.network_passphrase);
                let sk = SecretKey::from_seed(network_id.as_bytes());
                let pk = sk.public_key();
                let aid = AccountId(PublicKey::PublicKeyTypeEd25519(Uint256(*pk.as_bytes())));
                let seq = self
                    .app
                    .load_account_sequence(&aid)
                    .unwrap_or((ledger_num.get() as i64) << 32);
                TestAccount {
                    secret_key: sk,
                    account_id: aid,
                    sequence_number: seq,
                }
            } else {
                let name = format!("TestAccount-{}", account_id);
                let initial_seq = (ledger_num.get() as i64) << 32;
                let mut account = TestAccount::from_name(&name, initial_seq);
                // Try to load real sequence from DB
                if let Some(seq) = self.app.load_account_sequence(&account.account_id) {
                    account.sequence_number = seq;
                }
                account
            };
            entry.insert(account);
        }
        self.accounts.get_mut(&account_id).unwrap()
    }

    /// Reload the account's sequence number from the DB.
    ///
    /// Returns `true` if the account was found.
    /// Matches stellar-core `TxGenerator::loadAccount()`.
    pub fn load_account(&mut self, account_id: u64) -> bool {
        if let Some(account) = self.accounts.get_mut(&account_id) {
            if let Some(seq) = self.app.load_account_sequence(&account.account_id) {
                account.sequence_number = seq;
                return true;
            }
        }
        false
    }

    /// Build CreateAccount operations for a range of accounts.
    ///
    /// Matches stellar-core `TxGenerator::createAccounts()`.
    /// Each account gets `balance` stroops.
    pub fn create_accounts(
        &mut self,
        start: u64,
        count: u64,
        ledger_num: LedgerSeq,
        balance: i64,
    ) -> Vec<Operation> {
        let mut ops = Vec::with_capacity(count as usize);
        let initial_seq = (ledger_num.get() as i64) << 32;
        for i in start..start + count {
            let name = format!("TestAccount-{}", i);
            let account = TestAccount::from_name(&name, initial_seq);
            let destination = account.account_id.clone();
            self.accounts.insert(i, account);
            ops.push(Operation {
                source_account: None,
                body: OperationBody::CreateAccount(CreateAccountOp {
                    destination,
                    starting_balance: balance,
                }),
            });
        }
        ops
    }

    /// Pick a random source+destination pair from the account pool.
    ///
    /// Matches stellar-core `TxGenerator::pickAccountPair()`.
    pub fn pick_account_pair(
        &mut self,
        n_accounts: u32,
        offset: u32,
        ledger_num: LedgerSeq,
        source_account_id: u64,
    ) -> (u64, u64) {
        // Ensure source is cached
        let _ = self.find_account(source_account_id, ledger_num);
        // Pick a random destination
        let dest_id = if n_accounts > 1 {
            let raw = deterministic_rand(source_account_id, ledger_num.get()) % (n_accounts as u64);
            raw + offset as u64
        } else {
            offset as u64
        };
        (source_account_id, dest_id)
    }

    /// Generate a random fee in `[base_fee, max_fee_rate]`.
    ///
    /// Matches stellar-core `TxGenerator::generateFee()`.
    pub fn generate_fee(
        &self,
        max_fee_rate: Option<u32>,
        ops_count: usize,
        source_account_id: u64,
    ) -> u32 {
        let base_fee = self.app.base_fee();
        match max_fee_rate {
            Some(max_rate) if max_rate > base_fee => {
                let range = max_rate - base_fee;
                let r = deterministic_rand(source_account_id, ops_count as u32);
                let fee_rate = base_fee + (r % range as u64) as u32;
                fee_rate * ops_count as u32
            }
            _ => base_fee * ops_count as u32,
        }
    }

    /// Build a signed payment transaction (1 stroop).
    ///
    /// Matches stellar-core `TxGenerator::paymentTransaction()`.
    pub fn payment_transaction(
        &mut self,
        n_accounts: u32,
        offset: u32,
        ledger_num: LedgerSeq,
        source_account_id: u64,
        max_fee_rate: Option<u32>,
    ) -> anyhow::Result<(u64, TransactionEnvelope)> {
        let (source_id, dest_id) =
            self.pick_account_pair(n_accounts, offset, ledger_num, source_account_id);

        let dest_account = self.find_account(dest_id, ledger_num);
        let dest_muxed =
            MuxedAccount::Ed25519(Uint256(*dest_account.secret_key.public_key().as_bytes()));

        let payment_op = Operation {
            source_account: None,
            body: OperationBody::Payment(PaymentOp {
                destination: dest_muxed,
                asset: Asset::Native,
                amount: 1, // 1 stroop, matching stellar-core
            }),
        };

        let fee = self.generate_fee(max_fee_rate, 1, source_account_id);
        let envelope =
            self.create_transaction_frame(source_id, vec![payment_op], fee, ledger_num)?;
        Ok((source_id, envelope))
    }

    /// Build and sign a `TransactionEnvelope` from a source account and
    /// operations.
    ///
    /// Matches stellar-core `TxGenerator::createTransactionFramePtr()`.
    pub fn create_transaction_frame(
        &mut self,
        source_id: u64,
        ops: Vec<Operation>,
        fee: u32,
        ledger_num: LedgerSeq,
    ) -> anyhow::Result<TransactionEnvelope> {
        let source = self.find_account(source_id, ledger_num);
        let seq = source.next_sequence_number();
        let secret = source.secret_key.clone();
        let source_muxed = MuxedAccount::Ed25519(Uint256(*secret.public_key().as_bytes()));

        let tx = Transaction {
            source_account: source_muxed,
            fee,
            seq_num: SequenceNumber(seq),
            cond: Preconditions::None,
            memo: Memo::None,
            operations: ops.try_into().unwrap_or_default(),
            ext: TransactionExt::V0,
        };

        let mut envelope = TransactionEnvelope::Tx(TransactionV1Envelope {
            tx,
            signatures: VecM::default(),
        });

        crate::loadgen_soroban::sign_envelope(&mut envelope, &secret, &self.network_passphrase)?;
        Ok(envelope)
    }

    /// Access the account cache.
    pub fn accounts(&self) -> &BTreeMap<u64, TestAccount> {
        &self.accounts
    }

    /// Mutable access to the accounts map (for cache warming).
    pub fn accounts_mut(&mut self) -> &mut BTreeMap<u64, TestAccount> {
        &mut self.accounts
    }

    /// Access a cached account by ID.
    pub fn get_account(&self, id: u64) -> Option<&TestAccount> {
        self.accounts.get(&id)
    }

    // --- Soroban transaction builders ---

    /// Build a random WASM upload transaction.
    ///
    /// Matches stellar-core `TxGenerator::sorobanRandomWasmTransaction()`.
    pub fn soroban_random_wasm_transaction(
        &mut self,
        ledger_num: LedgerSeq,
        account_id: u64,
        inclusion_fee: u32,
    ) -> anyhow::Result<(u64, TransactionEnvelope)> {
        let wasm_size = DEFAULT_WASM_SIZE;
        let wasm =
            SorobanTxBuilder::random_wasm(wasm_size, deterministic_rand(account_id, ledger_num.get()));
        let (sk, seq) = self.next_source_sequence(account_id, ledger_num);
        let builder = self.soroban_builder();
        let envelope = builder.upload_wasm_tx(&sk, seq, &wasm, inclusion_fee)?;
        Ok((account_id, envelope))
    }

    /// Build a WASM upload transaction for the loadgen test contract.
    ///
    /// Matches stellar-core `TxGenerator::createUploadWasmTransaction()`.
    pub fn create_upload_wasm_transaction(
        &mut self,
        ledger_num: LedgerSeq,
        account_id: u64,
        wasm: &[u8],
        max_fee_rate: Option<u32>,
    ) -> anyhow::Result<(u64, TransactionEnvelope)> {
        let fee = self.generate_fee(max_fee_rate, 1, account_id);
        let (sk, seq) = self.next_source_sequence(account_id, ledger_num);
        let builder = self.soroban_builder();
        let envelope = builder.upload_wasm_tx(&sk, seq, wasm, fee)?;
        Ok((account_id, envelope))
    }

    /// Build a contract creation transaction.
    ///
    /// Matches stellar-core `TxGenerator::createContractTransaction()`.
    pub fn create_contract_transaction(
        &mut self,
        ledger_num: LedgerSeq,
        account_id: u64,
        wasm_hash: &Hash256,
        salt: &Uint256,
        max_fee_rate: Option<u32>,
    ) -> anyhow::Result<(u64, TransactionEnvelope)> {
        let fee = self.generate_fee(max_fee_rate, 1, account_id);
        let (sk, seq) = self.next_source_sequence(account_id, ledger_num);
        let builder = self.soroban_builder();
        let envelope = builder.create_contract_tx(&sk, seq, wasm_hash, salt, fee)?;
        Ok((account_id, envelope))
    }

    /// Build a contract invocation transaction for load testing.
    ///
    /// Calls `do_work(guest_cycles, host_cycles, n_entries, kb_per_entry)` on the
    /// loadgen contract. Matches stellar-core `TxGenerator::invokeSorobanLoadTransaction()`.
    pub fn invoke_soroban_load_transaction(
        &mut self,
        ledger_num: LedgerSeq,
        account_id: u64,
        instance: &ContractInstance,
        max_fee_rate: Option<u32>,
    ) -> anyhow::Result<(u64, TransactionEnvelope)> {
        let fee = self.generate_fee(max_fee_rate, 1, account_id);

        // Sample workload parameters deterministically
        let rand_val = deterministic_rand(account_id, ledger_num.get());
        let target_instructions: u32 =
            INVOKE_BASE_INSTRUCTIONS + (rand_val % INVOKE_INSTRUCTIONS_RANGE) as u32;

        // Split between guest and host cycles (matching stellar-core ratios)
        let host_fraction = (rand_val >> 16) % 100;
        let host_instructions = (target_instructions as u64 * host_fraction) / 100;
        let guest_instructions = target_instructions as u64 - host_instructions;
        let host_cycles = host_instructions / HOST_CYCLES_PER_INSTRUCTION;
        let guest_cycles = guest_instructions / GUEST_CYCLES_PER_INSTRUCTION;

        let n_entries = 1 + (rand_val >> 32) % 4; // 1-4 entries
        let kb_per_entry = 1 + (rand_val >> 40) % 4; // 1-4 KB

        let args = vec![
            ScVal::U64(guest_cycles),
            ScVal::U64(host_cycles),
            ScVal::U32(n_entries as u32),
            ScVal::U32(kb_per_entry as u32),
        ];

        // Build read-write keys for contract data entries
        let mut rw_keys = Vec::new();
        for i in 0..n_entries {
            rw_keys.push(LedgerKey::ContractData(LedgerKeyContractData {
                contract: ScAddress::Contract(ContractId(Hash(instance.contract_id.0))),
                key: ScVal::U32(i as u32),
                durability: ContractDataDurability::Persistent,
            }));
        }

        // Refresh the account's sequence number before building (matching stellar-core)
        self.load_account(account_id);
        let (sk, seq) = self.next_source_sequence(account_id, ledger_num);
        let builder = self.soroban_builder();
        let envelope = builder.invoke_contract_tx(
            &sk,
            seq,
            ContractInvocation {
                contract_id: instance.contract_id,
                function_name: "do_work".to_string(),
                args,
                read_only_keys: instance.read_only_keys.clone(),
                read_write_keys: rw_keys,
                instructions: target_instructions,
                read_bytes: INVOKE_BASE_READ_BYTES + instance.contract_entries_size,
                write_bytes: (n_entries as u32) * (kb_per_entry as u32) * 1024,
                inclusion_fee: fee,
            },
        )?;
        Ok((account_id, envelope))
    }

    /// Build a SAC creation transaction.
    ///
    /// Matches stellar-core `TxGenerator::createSACTransaction()`.
    pub fn create_sac_transaction(
        &mut self,
        ledger_num: LedgerSeq,
        account_id: Option<u64>,
        asset: Asset,
        max_fee_rate: Option<u32>,
    ) -> anyhow::Result<(u64, TransactionEnvelope)> {
        let id = account_id.unwrap_or(ROOT_ACCOUNT_ID);
        let fee = self.generate_fee(max_fee_rate, 1, id);
        let (sk, seq) = self.next_source_sequence(id, ledger_num);
        let builder = self.soroban_builder();
        let envelope = builder.create_sac_tx(&sk, seq, asset, fee)?;
        Ok((id, envelope))
    }

    /// Build a SAC transfer invocation transaction.
    ///
    /// Matches stellar-core `TxGenerator::invokeSACPayment()`.
    pub fn invoke_sac_payment(
        &mut self,
        ledger_num: LedgerSeq,
        from_account_id: u64,
        to_address: ScAddress,
        instance: &ContractInstance,
        amount: u64,
        max_fee_rate: Option<u32>,
    ) -> anyhow::Result<(u64, TransactionEnvelope)> {
        let fee = self.generate_fee(max_fee_rate, 1, from_account_id);
        let source = self.find_account(from_account_id, ledger_num);
        let from_address = make_account_address(&source.secret_key.public_key());
        let (sk, seq) = (source.secret_key.clone(), source.next_sequence_number());
        let builder = self.soroban_builder();
        let envelope = builder.invoke_sac_transfer_tx(
            &sk,
            seq,
            SacTransfer {
                contract_id: instance.contract_id,
                from_address,
                to_address,
                amount: amount as i128,
                instance_keys: instance.read_only_keys.clone(),
                inclusion_fee: fee,
            },
        )?;
        Ok((from_account_id, envelope))
    }

    /// Build a batch transfer invocation transaction.
    ///
    /// Matches stellar-core `TxGenerator::invokeBatchTransfer()`.
    pub fn invoke_batch_transfer(
        &mut self,
        ledger_num: LedgerSeq,
        source_account_id: u64,
        batch_instance: &ContractInstance,
        sac_instance: &ContractInstance,
        destinations: Vec<ScAddress>,
        max_fee_rate: Option<u32>,
    ) -> anyhow::Result<(u64, TransactionEnvelope)> {
        let fee = self.generate_fee(max_fee_rate, 1, source_account_id);
        let sac_address = make_contract_address(&sac_instance.contract_id);
        let dest_vals: Vec<ScVal> = destinations
            .into_iter()
            .map(|a| ScVal::Address(a))
            .collect();
        let (sk, seq) = self.next_source_sequence(source_account_id, ledger_num);
        let builder = self.soroban_builder();
        let envelope = builder.invoke_batch_transfer_tx(
            &sk,
            seq,
            BatchTransfer {
                contract_id: batch_instance.contract_id,
                sac_address: ScVal::Address(sac_address),
                destinations: dest_vals,
                instance_keys: batch_instance.read_only_keys.clone(),
                inclusion_fee: fee,
            },
        )?;
        Ok((source_account_id, envelope))
    }

    // --- Legacy stateless API (backward compat) ---

    /// Generate a deterministic series of payment transactions.
    ///
    /// This is the original simple stateless API.
    pub fn payment_series(
        accounts: &[String],
        start_sequence: u64,
        tx_count: usize,
        fee_bid: u32,
        amount: i64,
    ) -> Vec<GeneratedTransaction> {
        if accounts.len() < 2 || tx_count == 0 {
            return Vec::new();
        }

        let mut txs = Vec::with_capacity(tx_count);
        for i in 0..tx_count {
            let source = accounts[i % accounts.len()].clone();
            let destination = accounts[(i + 1) % accounts.len()].clone();
            let sequence = start_sequence + i as u64;
            let nonce =
                Hash256::hash(format!("{}:{}:{}", source, destination, sequence).as_bytes());
            txs.push(GeneratedTransaction {
                source,
                destination,
                sequence,
                fee_bid,
                amount,
                nonce,
            });
        }
        txs
    }
}

// ---------------------------------------------------------------------------
// LoadGenerator (enriched)
// ---------------------------------------------------------------------------

/// Load generator with account pool management, rate limiting, and retry logic.
///
/// Matches stellar-core `LoadGenerator`.
pub struct LoadGenerator {
    /// Transaction generator with account cache.
    tx_generator: TxGenerator,
    /// Accounts available for use (not currently in-flight).
    accounts_available: HashSet<u64>,
    /// Accounts currently referenced by pending transactions.
    accounts_in_use: HashSet<u64>,
    /// Cumulative count of transactions submitted.
    total_submitted: i64,
    /// Start time of the current load generation run.
    start_time: Option<Instant>,
    /// Last second at which cleanup was performed.
    last_second: u64,
    /// Whether load generation has failed.
    failed: bool,
    /// Whether load generation has been stopped.
    stopped: bool,

    // --- Soroban persistent state (survives across runs, reset by `reset_soroban_state()`) ---
    /// WASM code ledger key (set during `SorobanInvokeSetup` upload phase).
    code_key: Option<LedgerKey>,
    /// Contract instance ledger keys (set during `SorobanInvokeSetup` deploy phase).
    contract_instance_keys: HashSet<LedgerKey>,
    /// WASM blob size + overhead (set during upload phase).
    contract_overhead_bytes: u64,
    /// Per-account contract instance assignments (rebuilt each `SorobanInvoke` run).
    contract_instances: BTreeMap<u64, ContractInstance>,
    /// Number of WASM uploads completed in current setup run.
    wasms_uploaded: u32,
}

impl LoadGenerator {
    /// Create a new load generator for the given app.
    pub fn new(app: Arc<App>, network_passphrase: String) -> Self {
        Self {
            tx_generator: TxGenerator::new(app, network_passphrase),
            accounts_available: HashSet::new(),
            accounts_in_use: HashSet::new(),
            total_submitted: 0,
            start_time: None,
            last_second: 0,
            failed: false,
            stopped: false,
            // Soroban persistent state — initialized empty, populated during setup modes.
            code_key: None,
            contract_instance_keys: HashSet::new(),
            contract_overhead_bytes: 0,
            contract_instances: BTreeMap::new(),
            wasms_uploaded: 0,
        }
    }

    /// Initialize the account pool for a load generation run.
    ///
    /// Populates `accounts_available` with account IDs `[offset, offset + n_accounts)`.
    /// For Soroban invoke modes, builds the `contract_instances` map via round-robin
    /// assignment of deployed contract instances to accounts.
    ///
    /// Matches stellar-core `LoadGenerator::start()`.
    fn start(&mut self, config: &mut GeneratedLoadConfig) {
        self.start_time = Some(Instant::now());
        self.total_submitted = 0;
        self.last_second = 0;
        self.failed = false;
        self.stopped = false;
        self.accounts_available.clear();
        self.accounts_in_use.clear();
        self.contract_instances.clear();

        // Soroban config setup
        if config.mode.is_soroban() && config.mode != LoadGenMode::SorobanUpload {
            if config.n_wasms == 0 {
                config.n_wasms = 1;
            }

            if config.mode.is_soroban_setup() {
                self.reset_soroban_state();
                config.n_txs = config.n_wasms;
                config.skip_low_fee_txs = false;
                config.spike_interval = 0;
                config.spike_size = 0;
            }

            if config.mode.mode_sets_up_invoke() || config.mode.mode_invokes() {
                if config.n_instances == 0 {
                    config.n_instances = 1;
                }
            }
        }

        // Populate accounts_available
        for i in 0..config.n_accounts {
            self.accounts_available.insert((i + config.offset) as u64);
        }

        // Build contract_instances for invoke modes (round-robin assignment)
        if config.mode.mode_invokes() {
            assert!(
                self.code_key.is_some(),
                "Must run SorobanInvokeSetup before SorobanInvoke"
            );
            assert!(
                config.n_accounts as usize >= config.n_instances as usize,
                "n_accounts must be >= n_instances"
            );
            assert!(
                self.contract_instance_keys.len() >= config.n_instances as usize,
                "Not enough contract instances deployed"
            );

            let instance_keys: Vec<&LedgerKey> = self.contract_instance_keys.iter().collect();
            let code_key = self.code_key.clone().unwrap();

            let mut account_iter = self.accounts_available.iter();
            for i in 0..config.n_accounts as usize {
                let instance_key = instance_keys[i % config.n_instances as usize];

                // Extract contract ID from the instance key
                let contract_id = match instance_key {
                    LedgerKey::ContractData(cd) => match &cd.contract {
                        ScAddress::Contract(ContractId(Hash(bytes))) => Hash256(*bytes),
                        _ => panic!("unexpected contract address type"),
                    },
                    _ => panic!("unexpected instance key type"),
                };

                let instance = ContractInstance {
                    read_only_keys: vec![code_key.clone(), instance_key.clone()],
                    contract_id,
                    contract_entries_size: self.contract_overhead_bytes as u32,
                };

                let account_id = *account_iter.next().expect("enough accounts");
                self.contract_instances.insert(account_id, instance);
            }
        }
    }

    /// Run load generation: submit transactions at the configured rate.
    ///
    /// This is the main entry point matching stellar-core `LoadGenerator::generateLoad()`.
    /// It runs in a loop with `STEP_MSECS` intervals, using a cumulative-target
    /// rate limiter. Returns when all transactions have been submitted or on failure.
    ///
    /// For `SorobanInvokeSetup`, this implements a two-phase approach:
    /// - Phase 1: Upload WASM (n_txs = n_wasms)
    /// - Phase 2: Deploy contract instances (n_txs = n_instances)
    pub async fn generate_load(&mut self, config: &mut GeneratedLoadConfig) -> LoadResult {
        self.start(config);

        let step_duration = Duration::from_millis(STEP_MSECS);

        loop {
            if self.stopped {
                return LoadResult::Stopped;
            }
            if self.failed {
                return LoadResult::Failed;
            }

            // Check if all transactions for the current phase are submitted
            if !config.are_txs_remaining() {
                // For setup modes, transition from phase 1 (upload) to phase 2 (deploy)
                if config.mode.is_soroban_setup() && !config.is_done() {
                    // Phase 1 complete (wasm uploaded), start phase 2 (deploy instances)
                    assert!(
                        config.n_wasms == 0,
                        "Expected all wasms to be uploaded before transitioning to phase 2"
                    );
                    config.n_txs = config.n_instances;
                    info!(
                        n_instances = config.n_instances,
                        "Setup phase 1 complete, transitioning to instance deployment"
                    );
                } else {
                    return LoadResult::Done {
                        submitted: self.total_submitted,
                    };
                }
            }

            // Compute how many txs we should have submitted by now
            let txs_this_step = self.get_tx_per_step(config);

            // Cleanup accounts once per second
            let elapsed_secs = self.start_time.map(|t| t.elapsed().as_secs()).unwrap_or(0);
            if elapsed_secs != self.last_second {
                self.last_second = elapsed_secs;
                self.cleanup_accounts();
            }

            // Submit transactions for this step
            let ledger_num = self.tx_generator.app.current_ledger_seq();
            let mut submitted_this_step = 0i64;
            for _ in 0..txs_this_step {
                if config.n_txs == 0 {
                    break;
                }

                let source_id = match self.get_next_available_account(ledger_num) {
                    Some(id) => id,
                    None => {
                        debug!("No available accounts, waiting for cleanup");
                        break;
                    }
                };

                let ok = self.submit_tx(config, source_id, ledger_num).await;
                if ok {
                    config.n_txs = config.n_txs.saturating_sub(1);
                    submitted_this_step += 1;
                } else if self.failed {
                    return LoadResult::Failed;
                }
            }
            self.total_submitted += submitted_this_step;

            tokio::time::sleep(step_duration).await;
        }
    }

    /// Compute how many transactions to submit this step using the
    /// cumulative-target rate limiter.
    ///
    /// Matches stellar-core `LoadGenerator::getTxPerStep()`.
    /// Includes spike interval logic: every `spike_interval` seconds, an
    /// additional `spike_size` transactions are added to the target.
    fn get_tx_per_step(&self, config: &GeneratedLoadConfig) -> i64 {
        let Some(start) = self.start_time else {
            return 0;
        };
        let elapsed_ms = start.elapsed().as_millis() as i64;
        let mut target = elapsed_ms * config.tx_rate as i64 / 1000;

        // Add spike contribution
        if config.spike_interval > 0 {
            let elapsed_secs = (elapsed_ms / 1000) as u64;
            let spikes = elapsed_secs / config.spike_interval;
            target += (spikes * config.spike_size as u64) as i64;
        }

        let deficit = target - self.total_submitted;
        deficit.max(0)
    }

    /// Pick a random available account, move it to in-use, and ensure it
    /// has no pending transactions in the herder queue.
    ///
    /// Matches stellar-core `LoadGenerator::getNextAvailableAccount()`.
    fn get_next_available_account(&mut self, ledger_num: LedgerSeq) -> Option<u64> {
        // Try up to `available.len()` times to find a non-pending account
        let max_attempts = self.accounts_available.len();
        for _ in 0..max_attempts {
            if self.accounts_available.is_empty() {
                return None;
            }

            // Pick deterministically using size-based index
            let idx = deterministic_rand(self.total_submitted as u64, ledger_num.get()) as usize
                % self.accounts_available.len();

            let id = *self
                .accounts_available
                .iter()
                .nth(idx)
                .expect("idx within bounds");

            self.accounts_available.remove(&id);
            self.accounts_in_use.insert(id);

            // Check if account has pending txs
            let account_id = self
                .tx_generator
                .find_account(id, ledger_num)
                .account_id
                .clone();
            if !self.tx_generator.app.source_account_pending(&account_id) {
                return Some(id);
            }
            // If pending, it stays in accounts_in_use and we try another
        }
        None
    }

    /// Move accounts from in-use back to available when they no longer have
    /// pending transactions.
    ///
    /// Matches stellar-core `LoadGenerator::cleanupAccounts()`.
    pub fn cleanup_accounts(&mut self) {
        let mut to_return = Vec::new();
        for &id in &self.accounts_in_use {
            if let Some(account) = self.tx_generator.get_account(id) {
                if !self
                    .tx_generator
                    .app
                    .source_account_pending(&account.account_id)
                {
                    to_return.push(id);
                }
            } else {
                // Account not in cache — shouldn't happen, but reclaim it
                to_return.push(id);
            }
        }
        for id in to_return {
            self.accounts_in_use.remove(&id);
            self.accounts_available.insert(id);
        }
    }

    /// Submit a single transaction, retrying on `txBAD_SEQ` up to
    /// `TX_SUBMIT_MAX_TRIES` times.
    ///
    /// Dispatches to the appropriate transaction builder based on the load
    /// generation mode. Matches stellar-core `LoadGenerator::submitTx()`.
    async fn submit_tx(
        &mut self,
        config: &mut GeneratedLoadConfig,
        source_account_id: u64,
        ledger_num: LedgerSeq,
    ) -> bool {
        let mut num_tries = 0u32;

        loop {
            // Generate the transaction based on mode
            let tx_result = self.generate_tx(config, source_account_id, ledger_num);

            let envelope = match tx_result {
                Ok((_source_id, env)) => env,
                Err(e) => {
                    warn!("Failed to build tx (mode={:?}): {}", config.mode, e);
                    self.failed = true;
                    return false;
                }
            };

            let result = self.tx_generator.app.submit_transaction(envelope).await;

            match result {
                TxQueueResult::Added => return true,
                TxQueueResult::Invalid(Some(TxResultCode::TxBadSeq)) => {
                    num_tries += 1;
                    if num_tries >= TX_SUBMIT_MAX_TRIES {
                        warn!(
                            "Failed to submit tx after {} retries (txBAD_SEQ)",
                            num_tries
                        );
                        self.failed = true;
                        return false;
                    }
                    // Refresh sequence number from DB
                    self.tx_generator.load_account(source_account_id);
                    debug!(
                        tries = num_tries,
                        account = source_account_id,
                        "Retrying after txBAD_SEQ"
                    );
                }
                TxQueueResult::TryAgainLater | TxQueueResult::FeeTooLow
                    if config.skip_low_fee_txs =>
                {
                    // Roll back sequence number and skip
                    if let Some(account) = self.tx_generator.accounts.get_mut(&source_account_id) {
                        account.sequence_number -= 1;
                    }
                    return false;
                }
                other => {
                    warn!("Transaction submission failed: {:?}", other);
                    self.failed = true;
                    return false;
                }
            }
        }
    }

    /// Generate a transaction based on the current load generation mode.
    ///
    /// This is the mode-dispatch logic that stellar-core implements as a lambda
    /// in `generateLoad()`.
    fn generate_tx(
        &mut self,
        config: &mut GeneratedLoadConfig,
        source_account_id: u64,
        ledger_num: LedgerSeq,
    ) -> anyhow::Result<(u64, TransactionEnvelope)> {
        match config.mode {
            LoadGenMode::Pay => self.tx_generator.payment_transaction(
                config.n_accounts,
                config.offset,
                ledger_num,
                source_account_id,
                config.max_fee_rate,
            ),
            LoadGenMode::SorobanUpload => self.tx_generator.soroban_random_wasm_transaction(
                ledger_num,
                source_account_id,
                DEFAULT_SOROBAN_INCLUSION_FEE,
            ),
            LoadGenMode::SorobanInvokeSetup => {
                if config.n_wasms > 0 {
                    // Phase 1: Upload the loadgen WASM
                    let wasm = SorobanTxBuilder::loadgen_wasm();
                    let result = self.tx_generator.create_upload_wasm_transaction(
                        ledger_num,
                        source_account_id,
                        wasm,
                        config.max_fee_rate,
                    );
                    if result.is_ok() {
                        let wasm_hash = SorobanTxBuilder::loadgen_wasm_hash();
                        self.code_key = Some(contract_code_key(&wasm_hash));
                        self.contract_overhead_bytes = wasm.len() as u64 + 160;
                        self.wasms_uploaded += 1;
                        config.n_wasms = config.n_wasms.saturating_sub(1);
                    }
                    result
                } else {
                    // Phase 2: Deploy a contract instance
                    let wasm_hash = SorobanTxBuilder::loadgen_wasm_hash();
                    let salt = Uint256(
                        Hash256::hash(
                            &deterministic_rand(source_account_id, ledger_num.get()).to_le_bytes(),
                        )
                        .0,
                    );
                    let result = self.tx_generator.create_contract_transaction(
                        ledger_num,
                        source_account_id,
                        &wasm_hash,
                        &salt,
                        config.max_fee_rate,
                    );
                    if result.is_ok() {
                        // Compute the contract ID and store the instance key
                        let source_account = self
                            .tx_generator
                            .get_account(source_account_id)
                            .expect("source account must exist");
                        let source_pk = source_account.account_id.clone();
                        let preimage = ContractIdPreimage::Address(ContractIdPreimageFromAddress {
                            address: ScAddress::Account(source_pk),
                            salt: salt.clone(),
                        });
                        let contract_id =
                            compute_contract_id(&preimage, &self.tx_generator.network_passphrase)
                                .expect("contract ID computation");
                        let instance_key = contract_instance_key(&contract_id);
                        self.contract_instance_keys.insert(instance_key);
                        config.n_instances = config.n_instances.saturating_sub(1);
                    }
                    result
                }
            }
            LoadGenMode::SorobanInvoke => {
                let instance = self
                    .contract_instances
                    .get(&source_account_id)
                    .expect("contract instance must be assigned for SorobanInvoke")
                    .clone();
                self.tx_generator.invoke_soroban_load_transaction(
                    ledger_num,
                    source_account_id,
                    &instance,
                    config.max_fee_rate,
                )
            }
            LoadGenMode::MixedClassicSoroban => {
                self.create_mixed_classic_soroban_transaction(config, source_account_id, ledger_num)
            }
        }
    }

    /// Generate a transaction for `MixedClassicSoroban` mode using weighted
    /// random selection among Pay, SorobanUpload, and SorobanInvoke.
    ///
    /// Matches stellar-core `LoadGenerator::createMixedClassicSorobanTransaction()`.
    fn create_mixed_classic_soroban_transaction(
        &mut self,
        config: &GeneratedLoadConfig,
        source_account_id: u64,
        ledger_num: LedgerSeq,
    ) -> anyhow::Result<(u64, TransactionEnvelope)> {
        let total_weight =
            config.mix_pay_weight + config.mix_upload_weight + config.mix_invoke_weight;
        if total_weight == 0 {
            anyhow::bail!("MixedClassicSoroban weights sum to 0");
        }

        // Deterministic weighted selection
        let rand_val = deterministic_rand(source_account_id, ledger_num.get()) % total_weight as u64;
        let pay_threshold = config.mix_pay_weight as u64;
        let upload_threshold = pay_threshold + config.mix_upload_weight as u64;

        if rand_val < pay_threshold {
            // Pay mode
            self.tx_generator.payment_transaction(
                config.n_accounts,
                config.offset,
                ledger_num,
                source_account_id,
                config.max_fee_rate,
            )
        } else if rand_val < upload_threshold {
            // SorobanUpload mode
            self.tx_generator.soroban_random_wasm_transaction(
                ledger_num,
                source_account_id,
                DEFAULT_SOROBAN_INCLUSION_FEE,
            )
        } else {
            // SorobanInvoke mode
            let instance = self
                .contract_instances
                .get(&source_account_id)
                .expect("contract instance must be assigned for mixed invoke")
                .clone();
            self.tx_generator.invoke_soroban_load_transaction(
                ledger_num,
                source_account_id,
                &instance,
                config.max_fee_rate,
            )
        }
    }

    /// Stop load generation.
    pub fn stop(&mut self) {
        self.stopped = true;
    }

    /// Whether load generation has failed.
    pub fn has_failed(&self) -> bool {
        self.failed
    }

    /// Clear persistent Soroban state (contract keys, code key, overhead).
    ///
    /// Called at the start of setup modes and on certain failures.
    /// Matches stellar-core `LoadGenerator::resetSorobanState()`.
    pub fn reset_soroban_state(&mut self) {
        self.contract_instance_keys.clear();
        self.code_key = None;
        self.contract_overhead_bytes = 0;
    }

    /// Check that all deployed Soroban contract entries exist in the current
    /// ledger state.
    ///
    /// Returns ledger keys that are missing from the ledger snapshot.
    /// An empty return value means all state is synced.
    ///
    /// Matches stellar-core `LoadGenerator::checkSorobanStateSynced()`.
    pub fn check_soroban_state_synced(&self, config: &GeneratedLoadConfig) -> Vec<LedgerKey> {
        // Only applies to Soroban modes other than upload-only
        if !config.mode.is_soroban() || config.mode == LoadGenMode::SorobanUpload {
            return Vec::new();
        }

        let mut missing = Vec::new();

        // Check all contract instance keys
        for key in &self.contract_instance_keys {
            if !self.tx_generator.app.has_ledger_entry(key) {
                missing.push(key.clone());
            }
        }

        // Check the WASM code key
        if let Some(ref code_key) = self.code_key {
            if !self.tx_generator.app.has_ledger_entry(code_key) {
                missing.push(code_key.clone());
            }
        }

        missing
    }

    /// Check that the Soroban success rate meets the configured minimum.
    ///
    /// Returns `true` if the success percentage is at or above
    /// `min_soroban_percent_success`, or if the mode is not Soroban.
    ///
    /// Matches stellar-core `LoadGenerator::checkMinimumSorobanSuccess()`.
    pub fn check_minimum_soroban_success(
        &self,
        config: &GeneratedLoadConfig,
        success_count: u64,
        failure_count: u64,
    ) -> bool {
        if !config.mode.is_soroban() {
            return true;
        }
        let total = success_count + failure_count;
        if total == 0 {
            return true;
        }
        (success_count * 100) / total >= config.min_soroban_percent_success as u64
    }

    /// Total transactions submitted so far.
    pub fn total_submitted(&self) -> i64 {
        self.total_submitted
    }

    /// Check all cached accounts against the DB and return those with
    /// mismatched sequence numbers.
    ///
    /// Matches stellar-core `LoadGenerator::checkAccountSynced()`.
    pub fn check_account_synced(&self) -> Vec<u64> {
        let mut out_of_sync = Vec::new();
        for (&id, account) in self.tx_generator.accounts() {
            if id == ROOT_ACCOUNT_ID {
                continue;
            }
            if let Some(db_seq) = self
                .tx_generator
                .app
                .load_account_sequence(&account.account_id)
            {
                if db_seq != account.sequence_number {
                    out_of_sync.push(id);
                }
            }
        }
        out_of_sync
    }

    /// Access the underlying transaction generator.
    pub fn tx_generator(&self) -> &TxGenerator {
        &self.tx_generator
    }

    /// Mutable access to the underlying transaction generator.
    pub fn tx_generator_mut(&mut self) -> &mut TxGenerator {
        &mut self.tx_generator
    }

    // --- Legacy stateless API (backward compat) ---

    /// Pre-compute a load plan as a series of steps.
    ///
    /// This is the original simple stateless API.
    pub fn step_plan(config: &GeneratedLoadConfig) -> Vec<LoadStep> {
        let mut steps = Vec::with_capacity(config.steps);
        let mut next_sequence = 1u64;
        for step_index in 0..config.steps {
            let transactions = TxGenerator::payment_series(
                &config.accounts,
                next_sequence,
                config.txs_per_step,
                config.fee_bid,
                config.amount,
            );
            next_sequence += transactions.len() as u64;
            steps.push(LoadStep {
                step_index,
                transactions,
            });
        }
        steps
    }

    /// Summarize a pre-computed load plan.
    pub fn summarize(steps: &[LoadStep]) -> LoadReport {
        LoadReport {
            total_steps: steps.len(),
            total_transactions: steps.iter().map(|s| s.transactions.len()).sum(),
        }
    }
}

// ---------------------------------------------------------------------------
// Result types
// ---------------------------------------------------------------------------

/// Result of a load generation run.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LoadResult {
    /// All transactions submitted successfully.
    Done { submitted: i64 },
    /// Load generation was stopped by the user.
    Stopped,
    /// Load generation failed (submission error or too many retries).
    Failed,
}

// ---------------------------------------------------------------------------
// Legacy types (backward compat)
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GeneratedTransaction {
    pub source: String,
    pub destination: String,
    pub sequence: u64,
    pub fee_bid: u32,
    pub amount: i64,
    pub nonce: Hash256,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LoadStep {
    pub step_index: usize,
    pub transactions: Vec<GeneratedTransaction>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LoadReport {
    pub total_steps: usize,
    pub total_transactions: usize,
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

/// Simple deterministic pseudo-random function for load generation.
///
/// Not cryptographic — just needs to produce varied but repeatable values.
fn deterministic_rand(a: u64, b: u32) -> u64 {
    let hash = Hash256::hash(&[a.to_le_bytes().as_slice(), b.to_le_bytes().as_slice()].concat());
    u64::from_le_bytes(hash.0[..8].try_into().unwrap())
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn payment_series_is_deterministic() {
        let accounts = vec!["a".to_string(), "b".to_string(), "c".to_string()];
        let a = TxGenerator::payment_series(&accounts, 1, 5, 100, 10);
        let b = TxGenerator::payment_series(&accounts, 1, 5, 100, 10);
        assert_eq!(a, b);
    }

    #[test]
    fn step_plan_counts_transactions() {
        let config = GeneratedLoadConfig {
            accounts: vec!["a".to_string(), "b".to_string()],
            txs_per_step: 3,
            steps: 4,
            fee_bid: 100,
            amount: 10,
            ..Default::default()
        };
        let steps = LoadGenerator::step_plan(&config);
        let report = LoadGenerator::summarize(&steps);
        assert_eq!(report.total_steps, 4);
        assert_eq!(report.total_transactions, 12);
    }

    #[test]
    fn deterministic_seed_padding() {
        let seed = deterministic_seed("TestAccount-0");
        assert_eq!(seed.len(), 32);
        assert_eq!(&seed[..14], b"TestAccount-0.");
        assert!(seed[14..].iter().all(|&b| b == b'.'));
    }

    #[test]
    fn test_account_from_name() {
        let a1 = TestAccount::from_name("TestAccount-0", 0);
        let a2 = TestAccount::from_name("TestAccount-0", 0);
        assert_eq!(
            a1.secret_key.public_key().as_bytes(),
            a2.secret_key.public_key().as_bytes()
        );
    }

    #[test]
    fn generated_load_config_is_done() {
        let mut config = GeneratedLoadConfig::tx_load(10, 5, 10, 0, None);
        assert!(!config.is_done());
        assert!(config.are_txs_remaining());
        config.n_txs = 0;
        assert!(config.is_done());
        assert!(!config.are_txs_remaining());
    }

    #[test]
    fn deterministic_rand_is_stable() {
        let a = deterministic_rand(42, 7);
        let b = deterministic_rand(42, 7);
        assert_eq!(a, b);
        let c = deterministic_rand(42, 8);
        assert_ne!(a, c);
    }

    #[test]
    fn load_gen_mode_default() {
        let config = GeneratedLoadConfig::default();
        assert_eq!(config.mode, LoadGenMode::Pay);
    }
}
