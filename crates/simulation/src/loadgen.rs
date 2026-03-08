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
use henyey_common::{Hash256, NetworkId};
use henyey_crypto::{sign_hash, SecretKey};
use henyey_herder::TxQueueResult;
use henyey_tx::TxResultCode;
use stellar_xdr::curr::{
    AccountId, Asset, DecoratedSignature, Memo, MuxedAccount, Operation, OperationBody,
    PaymentOp, Preconditions, PublicKey, SequenceNumber, Signature, SignatureHint,
    Transaction, TransactionEnvelope, TransactionExt, TransactionV1Envelope, Uint256, VecM,
    CreateAccountOp,
};
use tracing::{debug, warn};

// ---------------------------------------------------------------------------
// Constants (matching stellar-core LoadGenerator.cpp)
// ---------------------------------------------------------------------------

/// Interval between load generation steps (milliseconds).
const STEP_MSECS: u64 = 100;

/// Maximum retries on `txBAD_SEQ` before giving up.
const TX_SUBMIT_MAX_TRIES: u32 = 10;

/// Sentinel account ID for the network root account.
const ROOT_ACCOUNT_ID: u64 = u64::MAX;

// ---------------------------------------------------------------------------
// LoadGenMode
// ---------------------------------------------------------------------------

/// Load generation mode.
///
/// Matches stellar-core `LoadGenMode`. Only `Pay` is currently supported;
/// Soroban modes are tracked as intentional omissions.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LoadGenMode {
    /// Classic payment transactions (1 stroop per tx).
    Pay,
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
        self.n_txs == 0
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

/// Deterministic seed derivation matching stellar-core `txtest::getAccount()`.
///
/// The name is right-padded with `.` to 32 bytes, then used as an ed25519 seed.
fn deterministic_seed(name: &str) -> [u8; 32] {
    let mut seed = [b'.'; 32];
    let len = name.len().min(32);
    seed[..len].copy_from_slice(&name.as_bytes()[..len]);
    seed
}

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
    app: Arc<App>,
    /// Network passphrase for transaction signing.
    network_passphrase: String,
}

impl TxGenerator {
    pub fn new(app: Arc<App>, network_passphrase: String) -> Self {
        Self {
            accounts: BTreeMap::new(),
            app,
            network_passphrase,
        }
    }

    /// Look up or create an account in the cache.
    ///
    /// Matches stellar-core `TxGenerator::findAccount()`.
    /// For the root account, uses the network root secret key.
    /// For numbered accounts, creates a deterministic keypair from `"TestAccount-{id}"`.
    pub fn find_account(&mut self, account_id: u64, ledger_num: u32) -> &mut TestAccount {
        if !self.accounts.contains_key(&account_id) {
            let account = if account_id == ROOT_ACCOUNT_ID {
                let network_id = NetworkId::from_passphrase(&self.network_passphrase);
                let sk = SecretKey::from_seed(network_id.as_bytes());
                let pk = sk.public_key();
                let aid =
                    AccountId(PublicKey::PublicKeyTypeEd25519(Uint256(*pk.as_bytes())));
                let seq = self
                    .app
                    .load_account_sequence(&aid)
                    .unwrap_or((ledger_num as i64) << 32);
                TestAccount {
                    secret_key: sk,
                    account_id: aid,
                    sequence_number: seq,
                }
            } else {
                let name = format!("TestAccount-{}", account_id);
                let initial_seq = (ledger_num as i64) << 32;
                let mut account = TestAccount::from_name(&name, initial_seq);
                // Try to load real sequence from DB
                if let Some(seq) = self.app.load_account_sequence(&account.account_id) {
                    account.sequence_number = seq;
                }
                account
            };
            self.accounts.insert(account_id, account);
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
        ledger_num: u32,
        balance: i64,
    ) -> Vec<Operation> {
        let mut ops = Vec::with_capacity(count as usize);
        let initial_seq = (ledger_num as i64) << 32;
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
        ledger_num: u32,
        source_account_id: u64,
    ) -> (u64, u64) {
        // Ensure source is cached
        let _ = self.find_account(source_account_id, ledger_num);
        // Pick a random destination
        let dest_id = if n_accounts > 1 {
            let raw = deterministic_rand(source_account_id, ledger_num) % (n_accounts as u64);
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
        ledger_num: u32,
        source_account_id: u64,
        max_fee_rate: Option<u32>,
    ) -> anyhow::Result<(u64, TransactionEnvelope)> {
        let (source_id, dest_id) =
            self.pick_account_pair(n_accounts, offset, ledger_num, source_account_id);

        let dest_account = self.find_account(dest_id, ledger_num);
        let dest_muxed = MuxedAccount::Ed25519(Uint256(
            *dest_account.secret_key.public_key().as_bytes(),
        ));

        let payment_op = Operation {
            source_account: None,
            body: OperationBody::Payment(PaymentOp {
                destination: dest_muxed,
                asset: Asset::Native,
                amount: 1, // 1 stroop, matching stellar-core
            }),
        };

        let fee = self.generate_fee(max_fee_rate, 1, source_account_id);
        let envelope = self.create_transaction_frame(source_id, vec![payment_op], fee, ledger_num)?;
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
        ledger_num: u32,
    ) -> anyhow::Result<TransactionEnvelope> {
        let source = self.find_account(source_id, ledger_num);
        let seq = source.next_sequence_number();
        let secret = source.secret_key.clone();
        let source_muxed =
            MuxedAccount::Ed25519(Uint256(*secret.public_key().as_bytes()));

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

        // Sign the envelope
        let network_id = NetworkId::from_passphrase(&self.network_passphrase);
        let frame =
            henyey_tx::TransactionFrame::with_network(envelope.clone(), network_id);
        let hash = frame.hash(&network_id)?;
        let signature = sign_hash(&secret, &hash);
        let public_key = secret.public_key();
        let pk_bytes = public_key.as_bytes();
        let hint =
            SignatureHint([pk_bytes[28], pk_bytes[29], pk_bytes[30], pk_bytes[31]]);
        let decorated = DecoratedSignature {
            hint,
            signature: Signature(signature.0.to_vec().try_into().unwrap_or_default()),
        };
        if let TransactionEnvelope::Tx(ref mut env) = envelope {
            env.signatures = vec![decorated].try_into().unwrap_or_default();
        }

        Ok(envelope)
    }

    /// Access the account cache.
    pub fn accounts(&self) -> &BTreeMap<u64, TestAccount> {
        &self.accounts
    }

    /// Access a cached account by ID.
    pub fn get_account(&self, id: u64) -> Option<&TestAccount> {
        self.accounts.get(&id)
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
    /// Reference to the app for submission.
    app: Arc<App>,
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
}

impl LoadGenerator {
    /// Create a new load generator for the given app.
    pub fn new(app: Arc<App>, network_passphrase: String) -> Self {
        Self {
            tx_generator: TxGenerator::new(Arc::clone(&app), network_passphrase),
            app,
            accounts_available: HashSet::new(),
            accounts_in_use: HashSet::new(),
            total_submitted: 0,
            start_time: None,
            last_second: 0,
            failed: false,
            stopped: false,
        }
    }

    /// Initialize the account pool for a load generation run.
    ///
    /// Populates `accounts_available` with account IDs `[offset, offset + n_accounts)`.
    fn start(&mut self, config: &GeneratedLoadConfig) {
        self.start_time = Some(Instant::now());
        self.total_submitted = 0;
        self.last_second = 0;
        self.failed = false;
        self.stopped = false;
        self.accounts_available.clear();
        self.accounts_in_use.clear();
        for i in 0..config.n_accounts {
            self.accounts_available
                .insert((i + config.offset) as u64);
        }
    }

    /// Run load generation: submit transactions at the configured rate.
    ///
    /// This is the main entry point matching stellar-core `LoadGenerator::generateLoad()`.
    /// It runs in a loop with `STEP_MSECS` intervals, using a cumulative-target
    /// rate limiter. Returns when all transactions have been submitted or on failure.
    pub async fn generate_load(
        &mut self,
        config: &mut GeneratedLoadConfig,
    ) -> LoadResult {
        self.start(config);

        let step_duration = Duration::from_millis(STEP_MSECS);

        loop {
            if self.stopped {
                return LoadResult::Stopped;
            }
            if self.failed {
                return LoadResult::Failed;
            }
            if config.is_done() {
                return LoadResult::Done {
                    submitted: self.total_submitted,
                };
            }

            // Compute how many txs we should have submitted by now
            let txs_this_step = self.get_tx_per_step(config);

            // Cleanup accounts once per second
            let elapsed_secs = self
                .start_time
                .map(|t| t.elapsed().as_secs())
                .unwrap_or(0);
            if elapsed_secs != self.last_second {
                self.last_second = elapsed_secs;
                self.cleanup_accounts();
            }

            // Submit transactions for this step
            let ledger_num = self.app.current_ledger_seq();
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
    fn get_tx_per_step(&self, config: &GeneratedLoadConfig) -> i64 {
        let Some(start) = self.start_time else {
            return 0;
        };
        let elapsed_ms = start.elapsed().as_millis() as i64;
        let target = elapsed_ms * config.tx_rate as i64 / 1000;
        let deficit = target - self.total_submitted;
        deficit.max(0)
    }

    /// Pick a random available account, move it to in-use, and ensure it
    /// has no pending transactions in the herder queue.
    ///
    /// Matches stellar-core `LoadGenerator::getNextAvailableAccount()`.
    fn get_next_available_account(&mut self, ledger_num: u32) -> Option<u64> {
        // Try up to `available.len()` times to find a non-pending account
        let max_attempts = self.accounts_available.len();
        for _ in 0..max_attempts {
            if self.accounts_available.is_empty() {
                return None;
            }

            // Pick deterministically using size-based index
            let idx = deterministic_rand(
                self.total_submitted as u64,
                ledger_num,
            ) as usize
                % self.accounts_available.len();

            let id = *self
                .accounts_available
                .iter()
                .nth(idx)
                .expect("idx within bounds");

            self.accounts_available.remove(&id);
            self.accounts_in_use.insert(id);

            // Check if account has pending txs
            let account = self.tx_generator.find_account(id, ledger_num);
            if !self.app.source_account_pending(&account.account_id) {
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
        let ledger_num = self.app.current_ledger_seq();
        let mut to_return = Vec::new();
        for &id in &self.accounts_in_use {
            if let Some(account) = self.tx_generator.get_account(id) {
                if !self.app.source_account_pending(&account.account_id) {
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
        let _ = ledger_num; // suppress unused warning
    }

    /// Submit a single transaction, retrying on `txBAD_SEQ` up to
    /// `TX_SUBMIT_MAX_TRIES` times.
    ///
    /// Matches stellar-core `LoadGenerator::submitTx()`.
    async fn submit_tx(
        &mut self,
        config: &GeneratedLoadConfig,
        source_account_id: u64,
        ledger_num: u32,
    ) -> bool {
        let mut num_tries = 0u32;

        loop {
            // Generate the transaction
            let tx_result = self.tx_generator.payment_transaction(
                config.n_accounts,
                config.offset,
                ledger_num,
                source_account_id,
                config.max_fee_rate,
            );

            let envelope = match tx_result {
                Ok((_source_id, env)) => env,
                Err(e) => {
                    warn!("Failed to build payment tx: {}", e);
                    self.failed = true;
                    return false;
                }
            };

            let result = self.app.submit_transaction(envelope).await;

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
                    if let Some(account) =
                        self.tx_generator.accounts.get_mut(&source_account_id)
                    {
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

    /// Check if load generation is complete.
    ///
    /// Matches stellar-core `LoadGenerator::isDone()`.
    pub fn is_done(&self, config: &GeneratedLoadConfig) -> bool {
        config.is_done()
    }

    /// Stop load generation.
    pub fn stop(&mut self) {
        self.stopped = true;
    }

    /// Whether load generation has failed.
    pub fn has_failed(&self) -> bool {
        self.failed
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
            if let Some(db_seq) = self.app.load_account_sequence(&account.account_id) {
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
    let hash = Hash256::hash(
        &[a.to_le_bytes().as_slice(), b.to_le_bytes().as_slice()].concat(),
    );
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
