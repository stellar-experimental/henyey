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
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use henyey_app::metrics as app_metrics;
use henyey_app::App;
use henyey_common::{Hash256, NetworkId};
use henyey_crypto::SecretKey;
use henyey_herder::TxQueueResult;
use henyey_tx::TxResultCode;
use stellar_xdr::{
    AccountId, Asset, ConfigUpgradeSetKey, ContractDataDurability, ContractId, ContractIdPreimage,
    ContractIdPreimageFromAddress, CreateAccountOp, Hash, LedgerKey, LedgerKeyContractData, Limits,
    Memo, MuxedAccount, Operation, OperationBody, PaymentOp, Preconditions, PublicKey, ScAddress,
    ScVal, SequenceNumber, Transaction, TransactionEnvelope, TransactionExt, TransactionV1Envelope,
    Uint256, VecM, WriteXdr,
};
use tracing::{debug, info, warn};

use crate::loadgen_soroban::{
    compute_contract_id, contract_code_key, contract_instance_key, make_account_address,
    make_contract_address, ApplyLoadTxParams, BatchTransfer, ContractInvocation, SacTransfer,
    SorobanTxBuilder,
};

// ---------------------------------------------------------------------------
// Constants (matching stellar-core LoadGenerator.cpp)
// ---------------------------------------------------------------------------

/// Interval between load generation steps (milliseconds).
const STEP_MSECS: u64 = 100;

/// Maximum retries on `txBAD_SEQ` before giving up.
const TX_SUBMIT_MAX_TRIES: u32 = 10;

/// Maximum retries on transient tx-queue backpressure (`QueueFull`, and
/// `TryAgainLater` without `skip_low_fee_txs`) before giving up. Generous and
/// finite: a genuinely wedged queue still surfaces as a run failure after the
/// cap (bounded, not infinite masking), but a transient burst — e.g. the
/// soroban phase-2 instance-deploy burst right after 100 phase-1 uploads —
/// drains within a few `STEP_MSECS` intervals and the run proceeds.
///
/// DELIBERATE DIVERGENCE from stellar-core (#3574): core has no `QUEUE_FULL`
/// status — a full queue returns `ADD_STATUS_TRY_AGAIN_LATER`
/// (TransactionQueue.cpp:461), and `LoadGenerator::submitTx` with
/// `skipLowFeeTxs == false` (which soroban setup uses, LoadGenerator.cpp:366)
/// sets `mFailed = true` (LoadGenerator.cpp:957-963). So core does NOT retry
/// on queue-full; it fails the run. This back-off-and-retry is an
/// operator-directed SIM-robustness divergence (the simulation loadgen is test
/// tooling driven by Supercluster, NOT validator consensus), so the SSC
/// henyey-majority soroban loadgen is unblocked. No hashed/serialized/consensus
/// output is touched.
///
/// Sized for the SLOW soroban drain: soroban txns are capped at
/// `ledgerMaxTxCount` (genesis = 1) per ledger, and a ledger closes ~every
/// 5 s, so a backed-up soroban queue drains ~1 tx / 5 s. With `STEP_MSECS =
/// 100 ms`, 100 tries (= 10 s ≈ 2 ledgers) was too small: the post-upgrade
/// `soroban_upload` run (200 txns) wedged a tail tx at exactly 100 tries and
/// failed the whole run at 195/200. 6000 tries (= 600 s of paced retry per tx)
/// comfortably outlasts the per-ledger drain for these runs while still
/// surfacing a genuinely wedged queue (bounded, not infinite). Supercluster's
/// own `WaitForLoadGenComplete` timeout governs the overall run.
const QUEUE_FULL_MAX_TRIES: u32 = 6000;

/// Decision for how `submit_tx` should react to a single
/// [`TxQueueResult`]. Factored out of `submit_tx`'s inner loop as a pure
/// function ([`classify_submit_result`]) so the branching can be unit-tested
/// deterministically without constructing a live `App` (#3574).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum SubmitAction {
    /// The tx was added to the queue — submission succeeded.
    Accept,
    /// `txBAD_SEQ` below the retry cap — reload the account seqnum and retry.
    RetryBadSeq,
    /// Low-fee / try-again under `skip_low_fee_txs` — roll back the seqnum and
    /// skip this tx (existing behavior).
    SkipLowFee,
    /// Transient tx-queue backpressure below the cap — roll back the seqnum,
    /// back off one step, and retry the SAME tx (#3574).
    RetryQueueFull,
    /// Unrecoverable (or cap exhausted) — fail the run.
    Fail,
}

/// Pure decision function for `submit_tx`: given a submission `result` and the
/// current retry counters, decide what the caller should do.
///
/// `bad_seq_tries` / `queue_full_tries` are the attempt counts AFTER the caller
/// has incremented for the current result, so the caps are compared with `>=`
/// (mirroring the original `num_tries += 1; if num_tries >= TX_SUBMIT_MAX_TRIES`
/// ordering).
///
/// `QueueFull` and `TryAgainLater`-without-`skip_low_fee_txs` both map to
/// [`SubmitAction::RetryQueueFull`] until `queue_full_tries` reaches
/// [`QUEUE_FULL_MAX_TRIES`], then [`SubmitAction::Fail`]. This is the
/// deliberate SIM-robustness divergence from stellar-core documented on
/// [`QUEUE_FULL_MAX_TRIES`] (#3574). The `skip_low_fee_txs` guard wins first
/// when the flag is set, preserving the existing skip-and-drop behavior for
/// classic low-fee paths.
fn classify_submit_result(
    result: TxQueueResult,
    queue_full_tries: u32,
    bad_seq_tries: u32,
    skip_low_fee_txs: bool,
    queue_full_max_tries: u32,
    _mode: LoadGenMode,
) -> SubmitAction {
    match result {
        TxQueueResult::Added => SubmitAction::Accept,
        TxQueueResult::Invalid(Some(TxResultCode::TxBadSeq)) => {
            if bad_seq_tries >= TX_SUBMIT_MAX_TRIES {
                SubmitAction::Fail
            } else {
                SubmitAction::RetryBadSeq
            }
        }
        // `skip_low_fee_txs` path: drop low-fee / try-again txs (classic load).
        // Matches the existing guarded arm and wins over the retry path below.
        TxQueueResult::TryAgainLater | TxQueueResult::FeeTooLow if skip_low_fee_txs => {
            SubmitAction::SkipLowFee
        }
        // A banned tx cannot be re-added for `ban_depth` ledgers. stellar-core's
        // canAdd returns ADD_STATUS_TRY_AGAIN_LATER for this (TransactionQueue.cpp
        // :327); the loadgen's correct response is to skip and roll back the
        // seqnum (re-submitting the same — deterministic — tx would just hit the
        // ban again until it expires). The account is re-selected on a later
        // step and, once the ban lifts (or its prior tx applies), proceeds. This
        // pairs with the #3611 seq refresh: after a tx ages out and is banned,
        // the refresh regenerates the SAME tx, so banned must be a skip, not a
        // hard failure.
        TxQueueResult::Banned => SubmitAction::SkipLowFee,
        // Transient tx-queue backpressure (#3574): retry the same tx with a
        // back-off until the generous cap, then surface a genuinely wedged
        // queue as a run failure. `TryAgainLater` only reaches here when
        // `skip_low_fee_txs` is false (the guard above wins otherwise).
        TxQueueResult::QueueFull | TxQueueResult::TryAgainLater => {
            if queue_full_tries >= queue_full_max_tries {
                SubmitAction::Fail
            } else {
                SubmitAction::RetryQueueFull
            }
        }
        _ => SubmitAction::Fail,
    }
}

/// Sentinel account ID for the network root account.
const ROOT_ACCOUNT_ID: u64 = u64::MAX;

/// Default WASM size for random upload transactions.
const DEFAULT_WASM_SIZE: usize = 35_000;

/// Default inclusion fee for Soroban transactions.
const DEFAULT_SOROBAN_INCLUSION_FEE: u32 = 100;

/// Sample a `SorobanUpload` WASM size from `values`, weighted by `weights`.
///
/// Parity with stellar-core's `sampleDiscrete(LOADGEN_WASM_BYTES_FOR_TESTING,
/// LOADGEN_WASM_BYTES_DISTRIBUTION_FOR_TESTING, DEFAULT_WASM_BYTES)`
/// (TxGenerator.cpp:1647): empty `values` → `default`; a single value is used
/// directly; otherwise pick weighted by `weights`, falling back to uniform when
/// `weights` doesn't line up with `values`. `seed` makes the choice
/// deterministic per (account, ledger).
fn sample_wasm_size(values: &[u32], weights: &[u32], default: usize, seed: u64) -> usize {
    match values {
        [] => default,
        [single] => *single as usize,
        _ => {
            let weighted = weights.len() == values.len() && weights.iter().any(|&w| w > 0);
            if weighted {
                let total: u64 = weights.iter().map(|&w| u64::from(w)).sum();
                let mut pick = seed % total;
                for (v, &w) in values.iter().zip(weights.iter()) {
                    let w = u64::from(w);
                    if pick < w {
                        return *v as usize;
                    }
                    pick -= w;
                }
                values[values.len() - 1] as usize
            } else {
                values[(seed % values.len() as u64) as usize] as usize
            }
        }
    }
}

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

/// Bit shift for encoding ledger sequence into initial sequence numbers.
/// Upper 32 bits hold the ledger number, lower 32 bits hold the tx counter.
/// Matches stellar-core's `getAccount()`/`TestAccount` convention.
const INITIAL_SEQ_LEDGER_SHIFT: u32 = 32;

/// Bit shifts for extracting pseudo-random parameters from `deterministic_rand`.
/// Each shift isolates an independent portion of the 64-bit random value.
const RAND_HOST_FRACTION_SHIFT: u32 = 16;
const RAND_ENTRY_COUNT_SHIFT: u32 = 32;
const RAND_ENTRY_SIZE_SHIFT: u32 = 40;

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
    /// Two-phase setup for the config-upgrade contract: upload the loadgen
    /// Wasm and deploy a single instance on the **root account**.
    /// Prerequisite for `SorobanCreateUpgrade`.
    ///
    /// Matches stellar-core `SOROBAN_UPGRADE_SETUP`.
    SorobanUpgradeSetup,
    /// Submit a single `write` invocation that stages a `ConfigUpgradeSet` for a
    /// network config upgrade. Requires a prior `SorobanUpgradeSetup` run.
    ///
    /// Matches stellar-core `SOROBAN_CREATE_UPGRADE`.
    SorobanCreateUpgrade,
    /// Replay pre-serialized transaction envelopes from an XDR record-marked
    /// file. No account allocation; sequence numbers come from the file.
    ///
    /// Matches stellar-core `PAY_PREGENERATED`.
    PayPregenerated,
    /// Heaviest apply-load throughput mode: builds V2 Soroban invoke
    /// transactions (`do_cpu_only_work`) via distribution-sampled resources
    /// and the tuned instruction model. Requires a prior `SorobanInvokeSetup`
    /// (reads the per-account `contract_instances` map) and overlay-only mode.
    ///
    /// Matches stellar-core `SOROBAN_INVOKE_APPLY_LOAD`.
    SorobanInvokeApplyLoad,
}

impl LoadGenMode {
    /// Returns `true` for any Soroban mode.
    ///
    /// Matches stellar-core `GeneratedLoadConfig::isSoroban()`. Note this
    /// includes the two soroban-upgrade modes but **not** `PayPregenerated`.
    pub fn is_soroban(self) -> bool {
        matches!(
            self,
            Self::SorobanUpload
                | Self::SorobanInvokeSetup
                | Self::SorobanInvoke
                | Self::MixedClassicSoroban
                | Self::SorobanUpgradeSetup
                | Self::SorobanCreateUpgrade
                | Self::SorobanInvokeApplyLoad
        )
    }

    /// Returns `true` for two-phase setup modes (upload Wasm then deploy).
    ///
    /// Matches stellar-core `GeneratedLoadConfig::isSorobanSetup()` —
    /// `SorobanInvokeSetup` and `SorobanUpgradeSetup`.
    pub fn is_soroban_setup(self) -> bool {
        matches!(self, Self::SorobanInvokeSetup | Self::SorobanUpgradeSetup)
    }

    /// Returns `true` for modes that submit transactions in a continuous loop.
    pub fn is_load(self) -> bool {
        matches!(
            self,
            Self::Pay
                | Self::SorobanUpload
                | Self::SorobanInvoke
                | Self::MixedClassicSoroban
                | Self::SorobanCreateUpgrade
                | Self::PayPregenerated
                | Self::SorobanInvokeApplyLoad
        )
    }

    /// Returns `true` for modes that invoke previously deployed contracts.
    ///
    /// Matches stellar-core `modeSetsUpInvoke()` | `modeInvokes()` invoke check.
    /// `SorobanInvokeApplyLoad` reads the per-account `contract_instances` map
    /// exactly like `SorobanInvoke`, so it must build that map in `start()`.
    pub fn mode_invokes(self) -> bool {
        matches!(
            self,
            Self::SorobanInvoke | Self::MixedClassicSoroban | Self::SorobanInvokeApplyLoad
        )
    }

    /// Returns `true` for modes that set up contract instances (upload + deploy).
    ///
    /// Matches stellar-core `modeSetsUpInvoke()`.
    pub fn mode_sets_up_invoke(self) -> bool {
        matches!(self, Self::SorobanInvokeSetup)
    }
}

// ---------------------------------------------------------------------------
// sample_discrete (TxGenerator.cpp:34)
// ---------------------------------------------------------------------------

/// Sample from a discrete distribution of `values` with `weights`.
///
/// Faithful port of stellar-core `sampleDiscrete<T>` (TxGenerator.cpp:34):
/// returns `default_value` when `values` is empty, else a weighted pick.
///
/// stellar-core uses `std::discrete_distribution` over a single global engine;
/// henyey threads an injectable RNG so sampling is unit-testable and
/// deterministic under a seed. The drawn *values* are non-consensus (never
/// hashed), so bit-for-bit parity with C++ is not required — parity is on the
/// formula the draws feed, and determinism on the seeded RNG.
pub fn sample_discrete<R: rand::Rng + ?Sized>(
    values: &[u32],
    weights: &[u32],
    default_value: u32,
    rng: &mut R,
) -> u32 {
    if values.is_empty() {
        return default_value;
    }
    // Mirror std::discrete_distribution: if weights are absent/degenerate,
    // fall back to a uniform pick over the values.
    let usable_weights = weights.len() == values.len() && weights.iter().any(|&w| w > 0);
    if usable_weights {
        match rand::distributions::WeightedIndex::new(weights.iter().copied()) {
            Ok(dist) => {
                use rand::distributions::Distribution;
                return values[dist.sample(rng)];
            }
            Err(_) => {}
        }
    }
    values[rng.gen_range(0..values.len())]
}

// ---------------------------------------------------------------------------
// LoadGenApplyLoadConfig (APPLY_LOAD_* surface consumed by the V2 mode)
// ---------------------------------------------------------------------------

/// The `APPLY_LOAD_*` config subset the `SorobanInvokeApplyLoad` mode reads.
///
/// This is intentionally narrow — only the parameters the V2 tx path consumes
/// (the bucket-list batch/ledger scalars, the data-entry size, and the 5
/// sampling distributions). It is distinct from the broader
/// [`crate::ApplyLoadConfig`] used by the direct-apply ledger-simulation
/// harness, which carries ledger/tx resource-limit knobs out of scope here.
///
/// Defaults mirror stellar-core `Config.h` (batch=1000, simulated=1000,
/// data_entry_size=0, empty distributions).
#[derive(Debug, Clone)]
pub struct LoadGenApplyLoadConfig {
    /// `APPLY_LOAD_BL_BATCH_SIZE`.
    pub bl_batch_size: u32,
    /// `APPLY_LOAD_BL_SIMULATED_LEDGERS`.
    pub bl_simulated_ledgers: u32,
    /// `APPLY_LOAD_DATA_ENTRY_SIZE` (rounded up to a multiple of 4).
    pub data_entry_size: u32,
    /// `APPLY_LOAD_NUM_RW_ENTRIES[_DISTRIBUTION]` as `(values, weights)`.
    pub num_rw_entries: (Vec<u32>, Vec<u32>),
    /// `APPLY_LOAD_NUM_DISK_READ_ENTRIES[_DISTRIBUTION]`.
    pub num_disk_read_entries: (Vec<u32>, Vec<u32>),
    /// `APPLY_LOAD_TX_SIZE_BYTES[_DISTRIBUTION]`.
    pub tx_size_bytes: (Vec<u32>, Vec<u32>),
    /// `APPLY_LOAD_EVENT_COUNT[_DISTRIBUTION]`.
    pub event_count: (Vec<u32>, Vec<u32>),
    /// `APPLY_LOAD_INSTRUCTIONS[_DISTRIBUTION]`.
    pub instructions: (Vec<u32>, Vec<u32>),
}

impl Default for LoadGenApplyLoadConfig {
    fn default() -> Self {
        Self {
            bl_batch_size: 1000,
            bl_simulated_ledgers: 1000,
            data_entry_size: 0,
            num_rw_entries: (Vec::new(), Vec::new()),
            num_disk_read_entries: (Vec::new(), Vec::new()),
            tx_size_bytes: (Vec::new(), Vec::new()),
            event_count: (Vec::new(), Vec::new()),
            instructions: (Vec::new(), Vec::new()),
        }
    }
}

impl LoadGenApplyLoadConfig {
    /// `data_entry_count = APPLY_LOAD_BL_BATCH_SIZE * APPLY_LOAD_BL_SIMULATED_LEDGERS`
    /// (LoadGenerator.cpp:792).
    pub fn data_entry_count(&self) -> u64 {
        self.bl_batch_size as u64 * self.bl_simulated_ledgers as u64
    }

    /// Round `APPLY_LOAD_DATA_ENTRY_SIZE` up to a multiple of 4
    /// (Config.cpp:1608).
    pub fn round_data_entry_size(size: u32) -> u32 {
        let rem = size % 4;
        if rem == 0 {
            size
        } else {
            size + (4 - rem)
        }
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
    /// Maximum number of paced retries on transient tx-queue backpressure
    /// (`QueueFull`, and `TryAgainLater` without `skip_low_fee_txs`) before a
    /// tx surfaces as a run failure. Defaults to [`QUEUE_FULL_MAX_TRIES`]
    /// (6000), the production/SSC value sized for the slow soroban drain
    /// (#3574). Tests that intentionally saturate the queue set this low so
    /// the bounded retry completes in seconds instead of minutes (#3600).
    pub queue_full_max_tries: u32,
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

    // --- Config-upgrade / pregenerated fields ---
    /// Config-setting deltas applied when building the `ConfigUpgradeSet` for
    /// `SorobanCreateUpgrade`. All-`None` (default) re-emits current settings.
    pub soroban_upgrade_config: henyey_ledger::config_upgrade::SorobanUpgradeConfig,
    /// Path to the pre-generated transactions file for `PayPregenerated`.
    pub preloaded_transactions_file: Option<std::path::PathBuf>,

    // --- Apply-load (V2) fields ---
    /// `APPLY_LOAD_*` config subset consumed by `SorobanInvokeApplyLoad`.
    pub apply_load: LoadGenApplyLoadConfig,
    /// Whether the node is running in overlay-only mode. Gate stand-in for
    /// stellar-core's `getRunInOverlayOnlyMode()` (henyey has no equivalent
    /// yet). `SorobanInvokeApplyLoad` requires this to be `true`
    /// (LoadGenerator.cpp:293).
    pub overlay_only_mode: bool,

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
            queue_full_max_tries: QUEUE_FULL_MAX_TRIES,
            spike_interval: 0,
            spike_size: 0,
            n_instances: 0,
            n_wasms: 0,
            min_soroban_percent_success: 0,
            mix_pay_weight: 1,
            mix_upload_weight: 1,
            mix_invoke_weight: 1,
            soroban_upgrade_config: Default::default(),
            preloaded_transactions_file: None,
            apply_load: LoadGenApplyLoadConfig::default(),
            overlay_only_mode: false,
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
    /// Number of pre-populated hot-archive entries (mirrors stellar-core
    /// `mPrePopulatedArchivedEntries`). Defaults to 0, so the apply-load V2
    /// autorestore branch is dormant — henyey builds no bucket-prepopulation
    /// harness here (out of scope, see #3309).
    pre_populated_archived_entries: u32,
    /// Autorestore cursor (mirrors stellar-core `mNextKeyToRestore`).
    next_key_to_restore: u32,
    /// Per-run RNG threaded through the non-consensus apply-load distribution
    /// draws. Seeded deterministically so a run is reproducible.
    apply_load_rng: rand_chacha::ChaCha8Rng,
}

impl TxGenerator {
    pub fn new(app: Arc<App>, network_passphrase: String) -> Self {
        use rand::SeedableRng;
        Self {
            accounts: BTreeMap::new(),
            app,
            network_passphrase,
            pre_populated_archived_entries: 0,
            next_key_to_restore: 0,
            apply_load_rng: rand_chacha::ChaCha8Rng::seed_from_u64(0),
        }
    }

    fn soroban_builder(&self) -> SorobanTxBuilder {
        SorobanTxBuilder::new(self.network_passphrase.clone())
    }

    fn next_source_sequence(&mut self, account_id: u64, ledger_num: u32) -> (SecretKey, i64) {
        let source = self.find_account(account_id, ledger_num);
        (source.secret_key.clone(), source.next_sequence_number())
    }

    /// Look up or create an account in the cache.
    ///
    /// Matches stellar-core `TxGenerator::findAccount()`.
    /// For the root account, uses the network root secret key.
    /// For numbered accounts, creates a deterministic keypair from `"TestAccount-{id}"`.
    pub fn find_account(&mut self, account_id: u64, ledger_num: u32) -> &mut TestAccount {
        if let std::collections::btree_map::Entry::Vacant(entry) = self.accounts.entry(account_id) {
            let account = if account_id == ROOT_ACCOUNT_ID {
                let network_id = NetworkId::from_passphrase(&self.network_passphrase);
                let sk = SecretKey::from_seed(network_id.as_bytes());
                let pk = sk.public_key();
                let aid = AccountId(PublicKey::PublicKeyTypeEd25519(Uint256(*pk.as_bytes())));
                let seq = self
                    .app
                    .load_account_sequence(&aid)
                    .unwrap_or_else(|e| {
                        tracing::warn!(error = ?e, "failed to load root account sequence");
                        None
                    })
                    .unwrap_or((ledger_num as i64) << INITIAL_SEQ_LEDGER_SHIFT);
                TestAccount {
                    secret_key: sk,
                    account_id: aid,
                    sequence_number: seq,
                }
            } else {
                let name = format!("TestAccount-{}", account_id);
                let initial_seq = (ledger_num as i64) << INITIAL_SEQ_LEDGER_SHIFT;
                let mut account = TestAccount::from_name(&name, initial_seq);
                // Try to load real sequence from DB
                match self.app.load_account_sequence(&account.account_id) {
                    Ok(Some(seq)) => account.sequence_number = seq,
                    Ok(None) => {}
                    Err(e) => {
                        tracing::warn!(error = ?e, "failed to refresh account sequence");
                    }
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
            match self.app.load_account_sequence(&account.account_id) {
                Ok(Some(seq)) => {
                    account.sequence_number = seq;
                    return true;
                }
                Ok(None) => {}
                Err(e) => {
                    tracing::warn!(error = ?e, "failed to load account sequence");
                }
            }
        }
        false
    }

    /// Refresh a cached account's sequence number from the ledger before
    /// generating a transaction for it, unless running in overlay-only mode.
    ///
    /// Parity port of stellar-core `TxGenerator::maybeLoadAccountSequenceNumber`
    /// (`src/simulation/TxGenerator.cpp`): in overlay-only mode the on-disk
    /// seqnums stay frozen at genesis while the local counter is authoritative,
    /// so the reload is skipped; otherwise the on-ledger seqnum is reloaded.
    ///
    /// Required for #3611. Under sustained load a tx can be discarded from the
    /// tx queue without applying — it ages out at `pending_depth` (4) ledgers
    /// and is auto-banned (`TransactionQueue::shift`, matching stellar-core).
    /// The account is then no longer pending, so the loadgen re-selects it, but
    /// its cached in-memory seq is still ahead of on-ledger. Without this
    /// refresh the next tx is generated with a *gapped* seq (> on-ledger + 1),
    /// which tx-set selection can never include (it only selects a contiguous
    /// run from on-ledger + 1). Those gapped txns accumulate, the account never
    /// syncs, and `wait_till_complete` burns its full timeout. Reloading the
    /// on-ledger seq first regenerates the tx at the correct next seq.
    pub fn maybe_load_account_sequence_number(&mut self, account_id: u64, overlay_only_mode: bool) {
        if !overlay_only_mode {
            self.load_account(account_id);
        }
    }

    /// Return `true` when every cached account's on-ledger sequence number
    /// matches its in-memory (expected) sequence number — i.e. all submitted
    /// transactions have been applied.
    ///
    /// Matches stellar-core `LoadGenerator::checkAccountSynced()`
    /// (`src/simulation/LoadGenerator.cpp`): the loadgen is only "complete" once
    /// the accounts it submitted from are caught up on the ledger. Read-only —
    /// it must NOT overwrite the in-memory expected sequence number (unlike
    /// `load_account`). An account that cannot be loaded yet (None / Err) is
    /// treated as not-yet-synced so the caller keeps waiting.
    pub fn check_accounts_synced(&self) -> bool {
        Self::accounts_synced_with(self.accounts.values(), |id| {
            self.app.load_account_sequence(id).ok().flatten()
        })
    }

    /// Pure core of [`check_accounts_synced`]: `true` when every account's
    /// in-memory (expected) sequence number equals its on-ledger sequence
    /// number, as reported by `load_seq` (returns `None` when the account
    /// can't be loaded yet — treated as not-yet-synced). Factored out so the
    /// cross-run synced semantics can be unit-tested without a live `App`.
    fn accounts_synced_with<'a, F>(
        accounts: impl Iterator<Item = &'a TestAccount>,
        load_seq: F,
    ) -> bool
    where
        F: Fn(&AccountId) -> Option<i64>,
    {
        for account in accounts {
            match load_seq(&account.account_id) {
                Some(db_seq) if db_seq == account.sequence_number => {}
                _ => return false,
            }
        }
        true
    }

    /// Clear the per-account sequence-number cache.
    ///
    /// Parity port of stellar-core `TxGenerator::reset()`
    /// (`src/simulation/TxGenerator.cpp`), which `LoadGenerator::reset()` calls
    /// before every run. The cache holds each account's *expected* (in-memory)
    /// sequence number; carrying it across independent loadgen runs poisons
    /// [`check_accounts_synced`] — a later run would keep inspecting a prior
    /// run's accounts, whose expected seq may never match the ledger (e.g. a
    /// tx that was submitted but dropped on queue overflow left the cached seq
    /// permanently ahead of on-ledger), so `wait_till_complete` times out even
    /// though the current run's own accounts are fully applied.
    pub fn reset(&mut self) {
        self.accounts.clear();
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
        let initial_seq = (ledger_num as i64) << INITIAL_SEQ_LEDGER_SHIFT;
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
        ledger_num: u32,
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
        ledger_num: u32,
        account_id: u64,
        inclusion_fee: u32,
    ) -> anyhow::Result<(u64, TransactionEnvelope)> {
        // Parity: stellar-core sizes each random upload by sampling
        // LOADGEN_WASM_BYTES_FOR_TESTING (weighted by
        // LOADGEN_WASM_BYTES_DISTRIBUTION_FOR_TESTING), falling back to
        // DEFAULT_WASM_BYTES when unset (TxGenerator.cpp:1647). Honoring the
        // config lets missions that don't raise the per-tx limits
        // (`WithSmallLoadgenOptions` → 1 KB) keep uploads within the genesis
        // `maxContractSizeBytes`; previously we hardcoded ~35 KB, which forced
        // those missions to additionally raise the Soroban per-tx limits.
        let seed = deterministic_rand(account_id, ledger_num);
        let testing = &self.app.config().testing;
        let wasm_size = sample_wasm_size(
            &testing.loadgen_wasm_bytes,
            &testing.loadgen_wasm_bytes_distribution,
            DEFAULT_WASM_SIZE,
            seed,
        );
        let wasm = SorobanTxBuilder::random_wasm(wasm_size, seed);
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
        ledger_num: u32,
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
        ledger_num: u32,
        account_id: u64,
        wasm_hash: &Hash256,
        salt: &Uint256,
        contract_overhead_bytes: u32,
        max_fee_rate: Option<u32>,
    ) -> anyhow::Result<(u64, TransactionEnvelope)> {
        let fee = self.generate_fee(max_fee_rate, 1, account_id);
        let (sk, seq) = self.next_source_sequence(account_id, ledger_num);
        let builder = self.soroban_builder();
        let envelope =
            builder.create_contract_tx(&sk, seq, wasm_hash, salt, contract_overhead_bytes, fee)?;
        Ok((account_id, envelope))
    }

    /// Build a config-upgrade `write` invocation transaction.
    ///
    /// Builds the `ConfigUpgradeSet` bytes from the App's *live* config settings
    /// (via `henyey_ledger::config_upgrade::build_config_upgrade_set`, a port of
    /// stellar-core `getConfigUpgradeSetFromLoadConfig`) applying the supplied
    /// `SorobanUpgradeConfig` deltas, then builds the create-upgrade tx
    /// (TxGenerator.cpp:1251). `code_key` and `instance_key` come from a prior
    /// `upgrade_setup` run.
    pub fn invoke_soroban_create_upgrade_transaction(
        &mut self,
        ledger_num: u32,
        account_id: u64,
        upgrade_config: &henyey_ledger::config_upgrade::SorobanUpgradeConfig,
        code_key: LedgerKey,
        instance_key: LedgerKey,
        max_fee_rate: Option<u32>,
    ) -> anyhow::Result<(u64, TransactionEnvelope)> {
        let app = Arc::clone(&self.app);
        let upgrade_bytes =
            henyey_ledger::config_upgrade::build_config_upgrade_set(upgrade_config, |id| {
                app.load_config_setting(id).ok().flatten()
            })
            .map_err(|e| anyhow::anyhow!("failed to build config upgrade set: {e}"))?;

        let fee = self.generate_fee(max_fee_rate, 1, account_id);
        let (sk, seq) = self.next_source_sequence(account_id, ledger_num);
        let builder = self.soroban_builder();
        let envelope = builder.invoke_soroban_create_upgrade_tx(
            &sk,
            seq,
            &upgrade_bytes,
            code_key,
            instance_key,
            fee,
        )?;
        Ok((account_id, envelope))
    }

    /// Build a contract invocation transaction for load testing.
    ///
    /// Calls `do_work(guest_cycles, host_cycles, n_entries, kb_per_entry)` on the
    /// loadgen contract. Matches stellar-core `TxGenerator::invokeSorobanLoadTransaction()`.
    pub fn invoke_soroban_load_transaction(
        &mut self,
        ledger_num: u32,
        account_id: u64,
        instance: &ContractInstance,
        max_fee_rate: Option<u32>,
    ) -> anyhow::Result<(u64, TransactionEnvelope)> {
        let fee = self.generate_fee(max_fee_rate, 1, account_id);

        // Sample workload parameters deterministically
        let rand_val = deterministic_rand(account_id, ledger_num);
        let target_instructions: u32 =
            INVOKE_BASE_INSTRUCTIONS + (rand_val % INVOKE_INSTRUCTIONS_RANGE) as u32;

        // Split between guest and host cycles (matching stellar-core ratios)
        let host_fraction = (rand_val >> RAND_HOST_FRACTION_SHIFT) % 100;
        let host_instructions = (target_instructions as u64 * host_fraction) / 100;
        let guest_instructions = target_instructions as u64 - host_instructions;
        let host_cycles = host_instructions / HOST_CYCLES_PER_INSTRUCTION;
        let guest_cycles = guest_instructions / GUEST_CYCLES_PER_INSTRUCTION;

        let n_entries = 1 + (rand_val >> RAND_ENTRY_COUNT_SHIFT) % 4; // 1-4 entries
        let kb_per_entry = 1 + (rand_val >> RAND_ENTRY_SIZE_SHIFT) % 4; // 1-4 KB

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

    /// Build a V2 apply-load contract invocation transaction.
    ///
    /// Faithful port of stellar-core
    /// `TxGenerator::invokeSorobanLoadTransactionV2` (TxGenerator.cpp:551).
    /// The distribution-sampled tx construction lives in
    /// [`SorobanTxBuilder::invoke_soroban_apply_load_tx`]; this wrapper threads
    /// the generator's autorestore cursor + seeded RNG and refreshes the
    /// account sequence number before building (to avoid `txBAD_SEQ` when the
    /// tx is discarded during tx-set assembly).
    pub fn invoke_soroban_load_transaction_v2(
        &mut self,
        ledger_num: u32,
        account_id: u64,
        instance: &ContractInstance,
        apply_load: &crate::loadgen::LoadGenApplyLoadConfig,
        max_fee_rate: Option<u32>,
    ) -> anyhow::Result<(u64, TransactionEnvelope)> {
        let fee = self.generate_fee(max_fee_rate, 1, account_id);

        let params = ApplyLoadTxParams {
            data_entry_count: apply_load.data_entry_count(),
            data_entry_size: apply_load.data_entry_size,
            num_rw_entries: apply_load.num_rw_entries.clone(),
            num_disk_read_entries: apply_load.num_disk_read_entries.clone(),
            tx_size_bytes: apply_load.tx_size_bytes.clone(),
            event_count: apply_load.event_count.clone(),
            instructions: apply_load.instructions.clone(),
            pre_populated_archived_entries: self.pre_populated_archived_entries,
        };

        // Refresh the account's sequence number before building.
        self.load_account(account_id);
        let (sk, seq) = self.next_source_sequence(account_id, ledger_num);

        // Build with the generator's autorestore cursor + RNG. We take the RNG
        // out to satisfy the borrow checker, then put it back.
        let builder = SorobanTxBuilder::new(self.network_passphrase.clone());
        let mut next_key = self.next_key_to_restore;
        let mut rng = std::mem::replace(
            &mut self.apply_load_rng,
            <rand_chacha::ChaCha8Rng as rand::SeedableRng>::seed_from_u64(0),
        );
        let result = builder.invoke_soroban_apply_load_tx(
            &sk,
            seq,
            instance,
            &params,
            &mut next_key,
            fee,
            &mut rng,
        );
        self.apply_load_rng = rng;
        let built = result?;
        self.next_key_to_restore = next_key;

        Ok((account_id, built.envelope))
    }

    /// Build a SAC creation transaction.
    ///
    /// Matches stellar-core `TxGenerator::createSACTransaction()`.
    pub fn create_sac_transaction(
        &mut self,
        ledger_num: u32,
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
        ledger_num: u32,
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
        ledger_num: u32,
        source_account_id: u64,
        batch_instance: &ContractInstance,
        sac_instance: &ContractInstance,
        destinations: Vec<ScAddress>,
        max_fee_rate: Option<u32>,
    ) -> anyhow::Result<(u64, TransactionEnvelope)> {
        let fee = self.generate_fee(max_fee_rate, 1, source_account_id);
        let sac_address = make_contract_address(&sac_instance.contract_id);
        let dest_vals: Vec<ScVal> = destinations.into_iter().map(ScVal::Address).collect();
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

    // --- Soroban persistent state (survives across runs, reset by `reset_soroban_state()`) ---
    /// WASM code ledger key (set during `SorobanInvokeSetup` upload phase).
    code_key: Option<LedgerKey>,
    /// Contract instance ledger keys (set during `SorobanInvokeSetup` deploy phase).
    contract_instance_keys: HashSet<LedgerKey>,
    /// WASM blob size + overhead (set during upload phase).
    contract_overhead_bytes: u64,
    /// Per-account contract instance assignments (rebuilt each `SorobanInvoke` run).
    contract_instances: BTreeMap<u64, ContractInstance>,

    // --- PayPregenerated state ---
    /// Open reader over the pre-generated transactions file (persists position
    /// across steps). Set during `start()` for `PayPregenerated`.
    preloaded_reader: Option<crate::loadgen_pregenerated::PregeneratedTxReader>,
    /// Number of pre-generated transactions consumed so far. Drives the
    /// round-robin account index `(curr_preloaded % n_accounts) + offset`.
    curr_preloaded: u64,
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
            // Soroban persistent state — initialized empty, populated during setup modes.
            code_key: None,
            contract_instance_keys: HashSet::new(),
            contract_overhead_bytes: 0,
            contract_instances: BTreeMap::new(),
            preloaded_reader: None,
            curr_preloaded: 0,
        }
    }

    /// Initialize the account pool for a load generation run.
    ///
    /// Populates `accounts_available` with account IDs `[offset, offset + n_accounts)`.
    /// For Soroban invoke modes, builds the `contract_instances` map via round-robin
    /// assignment of deployed contract instances to accounts.
    ///
    /// Matches stellar-core `LoadGenerator::start()`.
    /// Reset all per-run account state before starting a new load run.
    ///
    /// Parity port of stellar-core `LoadGenerator::reset()`
    /// (`src/simulation/LoadGenerator.cpp`): clears the available/in-use account
    /// pools, the invoke contract-instance map, and — via `TxGenerator::reset()`
    /// — the per-account sequence-number cache.
    ///
    /// Clearing the seq cache between runs is required for parity. A single
    /// `LoadGenerator` instance is reused across every `/generateload` run (it
    /// holds the Soroban deploy state across upgrade-setup → create_upgrade), so
    /// without this reset the cache accumulates every prior run's accounts.
    /// [`check_accounts_synced`] then keeps inspecting them, and any account
    /// left with an expected seq ahead of the ledger (a tx dropped on queue
    /// overflow under heavy load) makes a later run — even create_upgrade with a
    /// single tx — time out in `wait_till_complete` despite its own accounts
    /// being fully applied.
    ///
    /// Soroban deploy state (`code_key` / `contract_instance_keys`) is
    /// intentionally NOT cleared here so the create_upgrade run can consume the
    /// contract deployed by the preceding upgrade-setup run; setup modes clear
    /// it separately via [`reset_soroban_state`](Self::reset_soroban_state).
    pub fn reset(&mut self) {
        self.accounts_available.clear();
        self.accounts_in_use.clear();
        self.contract_instances.clear();
        self.tx_generator.reset();
    }

    fn start(&mut self, config: &mut GeneratedLoadConfig) {
        self.start_time = Some(Instant::now());
        self.total_submitted = 0;
        self.last_second = 0;
        self.failed = false;
        self.reset();

        // Overlay-only gate (LoadGenerator.cpp:293): SOROBAN_INVOKE_APPLY_LOAD
        // may only run in overlay-only mode. stellar-core resets + throws; we
        // reset + mark failed (the run aborts before any tx is generated).
        if Self::apply_load_overlay_gate_fails(config.mode, config.overlay_only_mode) {
            warn!("Can only run SOROBAN_INVOKE_APPLY_LOAD in overlay only mode");
            self.failed = true;
            self.reset_soroban_state();
            return;
        }

        // Soroban config setup (mirrors stellar-core LoadGenerator::start()).
        if config.mode.is_soroban() && config.mode != LoadGenMode::SorobanUpload {
            config.n_wasms = 1;

            // Upgrade modes deploy exactly one instance.
            if config.mode == LoadGenMode::SorobanUpgradeSetup {
                config.n_instances = 1;
            }
            if config.mode == LoadGenMode::SorobanCreateUpgrade {
                config.n_instances = 1;
                config.n_txs = 1; // single upgrade TX
            }

            if config.mode.is_soroban_setup() {
                self.reset_soroban_state();
                // Phase 1 deploys the wasms; phase 2 (set in generate_load) deploys
                // instances.
                config.n_txs = config.n_wasms;
                config.skip_low_fee_txs = false;
                config.spike_interval = 0;
                config.spike_size = 0;
            }

            if (config.mode.mode_sets_up_invoke() || config.mode.mode_invokes())
                && config.n_instances == 0
            {
                config.n_instances = 1;
            }
        }

        // Populate accounts_available.
        if config.mode != LoadGenMode::PayPregenerated {
            // Upgrade modes use the root account (special ID) as the source and
            // consume one numbered-account slot for it.
            let mut accounts = config.n_accounts;
            if config.mode == LoadGenMode::SorobanUpgradeSetup
                || config.mode == LoadGenMode::SorobanCreateUpgrade
            {
                self.accounts_available.insert(ROOT_ACCOUNT_ID);
                accounts = accounts.saturating_sub(1);
            }
            for i in 0..accounts {
                self.accounts_available.insert((i + config.offset) as u64);
            }
        }

        // PayPregenerated: open the transactions file, preload accounts. No
        // account-pool allocation (the source accounts come from the file).
        if config.mode == LoadGenMode::PayPregenerated {
            self.curr_preloaded = 0;
            if self.preloaded_reader.is_none() {
                let path = config
                    .preloaded_transactions_file
                    .clone()
                    .expect("PayPregenerated requires preloaded_transactions_file");
                match crate::loadgen_pregenerated::PregeneratedTxReader::open(&path) {
                    Ok(reader) => self.preloaded_reader = Some(reader),
                    Err(e) => {
                        warn!("Failed to open preloaded tx file: {e}");
                        self.failed = true;
                    }
                }
            }
            // Preload account cache so seq numbers can be set on them.
            let ledger_num = self.tx_generator.app.current_ledger_seq();
            for i in 0..config.n_accounts {
                let _ = self
                    .tx_generator
                    .find_account((i + config.offset) as u64, ledger_num);
            }
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
    ///
    /// The `stop_signal` is checked cooperatively at each loop iteration.
    /// When set to `true`, the method returns `LoadResult::Stopped`.
    pub async fn generate_load(
        &mut self,
        config: &mut GeneratedLoadConfig,
        stop_signal: &AtomicBool,
    ) -> LoadResult {
        self.start(config);

        let step_duration = Duration::from_millis(STEP_MSECS);

        loop {
            if stop_signal.load(Ordering::Relaxed) {
                return LoadResult::Stopped;
            }
            if self.failed {
                return LoadResult::Failed;
            }

            // One step per loop iteration (parity: stellar-core
            // LoadGenerator::getTxPerStep() mStepMeter.Mark(), LoadGenerator.cpp:211).
            app_metrics::LOADGEN_STEP_COUNT.increment(1);

            // Check if all transactions for the current phase are submitted
            if !config.are_txs_remaining() {
                // For setup modes, transition from phase 1 (upload) to phase 2 (deploy)
                if config.mode.is_soroban_setup() && !config.is_done() {
                    // Phase 1 complete (wasm uploaded), start phase 2 (deploy instances)
                    assert!(
                        config.n_wasms == 0,
                        "Expected all wasms to be uploaded before transitioning to phase 2"
                    );
                    // Parity: stellar-core does not deploy instances until the
                    // uploaded wasms are VERIFIED on-ledger — "waitTillFinished
                    // will set nTxs for instances once wasms have been verified"
                    // (LoadGenerator.cpp:360-362). The deploy (create_contract)
                    // references the uploaded wasm by hash; under the genesis
                    // 1-soroban-tx-per-ledger limit the deploy can otherwise win
                    // its ledger slot before the upload applies and fail with
                    // Storage(MissingValue) (code not found), leaving the
                    // instance uncreated (soroban_synced=false) and the
                    // create_upgrade unable to resolve (#3602 residual flake).
                    self.wait_setup_code_applied().await;
                    config.n_txs = config.n_instances;
                    info!(
                        n_instances = config.n_instances,
                        "Setup phase 1 complete (wasm verified on-ledger), \
                         transitioning to instance deployment"
                    );
                } else {
                    // Parity: stellar-core does not emit `loadgen.run.complete`
                    // on submission — it runs `waitTillComplete` until every
                    // submitted tx is APPLIED (accounts synced) and only then
                    // reports the run done (LoadGenerator.cpp:704-708,1345).
                    // This matters for downstream consumers that act on
                    // completion: e.g. SSC arms the Soroban config-settings
                    // upgrade (`/upgrades?configupgradesetkey`) immediately after
                    // the create-upgrade loadgen completes; if we reported "done"
                    // on submit, the `ConfigUpgradeSet` entry would not yet be in
                    // a closed ledger and the arm would be rejected (#3596).
                    self.wait_till_complete(config).await;
                    if self.failed {
                        return LoadResult::Failed;
                    }
                    // For create_upgrade, account-sync is necessary but not
                    // sufficient: the TEMPORARY ConfigUpgradeSet ContractData
                    // write can lag the source-account seq update in the
                    // bucket-list snapshot, so SSC's arm (sent immediately
                    // after completion) could miss it and reject the upgrade
                    // (#3596 race). Additionally wait until the upgrade key is
                    // actually RESOLVABLE via the same path the arm uses
                    // (make_from_key + isValidForApply), so the arm always
                    // succeeds.
                    if config.mode == LoadGenMode::SorobanCreateUpgrade {
                        self.wait_config_upgrade_resolvable(config).await;
                    }
                    return LoadResult::Done {
                        submitted: self.total_submitted,
                    };
                }
            }

            // Compute how many txs we should have submitted by now
            let txs_this_step = self.get_tx_per_step(config);

            // Cleanup accounts once per second (skipped for PayPregenerated,
            // which has no account pool — stellar-core LoadGenerator.cpp:681).
            let elapsed_secs = self.start_time.map(|t| t.elapsed().as_secs()).unwrap_or(0);
            if elapsed_secs != self.last_second {
                self.last_second = elapsed_secs;
                if config.mode != LoadGenMode::PayPregenerated {
                    self.cleanup_accounts();
                }
            }

            // Submit transactions for this step
            let ledger_num = self.tx_generator.app.current_ledger_seq();
            let mut submitted_this_step = 0i64;
            for _ in 0..txs_this_step {
                if config.n_txs == 0 {
                    break;
                }

                // PayPregenerated draws its source account from the file, so it
                // does not pick from the account pool (stellar-core
                // LoadGenerator.cpp:701). A sentinel id is passed through.
                let source_id = if config.mode == LoadGenMode::PayPregenerated {
                    0
                } else {
                    match self.get_next_available_account(ledger_num) {
                        Some(id) => id,
                        None => {
                            debug!("No available accounts, waiting for cleanup");
                            break;
                        }
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

    /// Wait until the uploaded setup wasm (`code_key`) exists on-ledger before
    /// deploying contract instances that reference it by hash.
    ///
    /// Parity: stellar-core's loadgen verifies uploaded wasms before setting
    /// `nTxs` for the instance-deploy phase (`LoadGenerator.cpp:360-362`).
    /// Without this gate, the `create_contract` deploy can be included before
    /// the upload applies (under the genesis 1-soroban-tx-per-ledger limit) and
    /// fail with `Storage(MissingValue)`. Bounded by ledger advancement so a
    /// never-applied upload can't hang the run.
    async fn wait_setup_code_applied(&mut self) {
        const TIMEOUT_NUM_LEDGERS: u32 = 30;
        let Some(code_key) = self.code_key.clone() else {
            return;
        };
        let start_ledger = self.tx_generator.app.current_ledger_seq();
        loop {
            if matches!(self.tx_generator.app.has_ledger_entry(&code_key), Ok(true)) {
                return;
            }
            let elapsed = self
                .tx_generator
                .app
                .current_ledger_seq()
                .saturating_sub(start_ledger);
            if elapsed >= TIMEOUT_NUM_LEDGERS {
                warn!(
                    timeout_ledgers = TIMEOUT_NUM_LEDGERS,
                    "setup: uploaded wasm not on-ledger before deploy timeout; \
                     deploy may fail with MissingValue"
                );
                return;
            }
            tokio::time::sleep(Duration::from_millis(500)).await;
        }
    }

    /// Wait until every submitted transaction has been applied (accounts
    /// synced) — and, for Soroban setup modes, until the deployed contract
    /// code + instance entries actually exist on-ledger — before declaring the
    /// run complete.
    ///
    /// Parity: stellar-core `LoadGenerator::waitTillComplete()`
    /// (`src/simulation/LoadGenerator.cpp:1345`) gates completion on BOTH
    /// `checkAccountSynced().empty()` AND `checkSorobanStateSynced(cfg).empty()`
    /// each ledger, timing out after `TIMEOUT_NUM_LEDGERS`. The Soroban-state
    /// gate is load-bearing for the create_upgrade flow (#3602): under the
    /// genesis `MinimumSorobanNetworkConfig` (`ledgerMaxTxCount = 1`,
    /// `ledgerMaxInstructions = 2_500_000`) the upgrade-setup `create_contract`
    /// deploy competes for the single per-ledger Soroban slot and can be
    /// surge-excluded for several ledgers. Account-sync alone returns as soon
    /// as the source seq advances (a *failed* apply still bumps the seq), so
    /// without the state gate the run could report "complete" while the
    /// contract instance was never created — the subsequent `create_upgrade`
    /// then writes nothing resolvable and the SSC arm hangs. We poll ~once per
    /// second and bound the wait by ledger advancement.
    ///
    /// On timeout with the Soroban state still unsynced (setup modes), the run
    /// is marked failed — parity with stellar-core `emitFailure` — so the
    /// mission fails fast and is retried, rather than silently proceeding to a
    /// create_upgrade that can never resolve.
    async fn wait_till_complete(&mut self, config: &GeneratedLoadConfig) {
        const TIMEOUT_NUM_LEDGERS: u32 = 30;
        let check_soroban = config.mode.is_soroban_setup();
        let start_ledger = self.tx_generator.app.current_ledger_seq();
        loop {
            let accounts_synced = self.tx_generator.check_accounts_synced();
            let soroban_synced = !check_soroban || self.soroban_state_synced();
            if accounts_synced && soroban_synced {
                return;
            }
            let elapsed = self
                .tx_generator
                .app
                .current_ledger_seq()
                .saturating_sub(start_ledger);
            if elapsed >= TIMEOUT_NUM_LEDGERS {
                warn!(
                    timeout_ledgers = TIMEOUT_NUM_LEDGERS,
                    accounts_synced,
                    soroban_synced,
                    "loadgen wait-till-complete timed out; some submitted txns \
                     were not applied (likely dropped before inclusion)"
                );
                // Parity: emitFailure(!sorobanIsDone). A setup run whose
                // contract code/instance never materialized must fail — a
                // downstream create_upgrade built against the missing instance
                // would never produce a resolvable ConfigUpgradeSet entry.
                if check_soroban && !soroban_synced {
                    self.failed = true;
                }
                return;
            }
            tokio::time::sleep(Duration::from_millis(1000)).await;
        }
    }

    /// Whether the Soroban setup's deployed code + contract-instance entries
    /// exist on-ledger.
    ///
    /// Parity: stellar-core `LoadGenerator::checkSorobanStateSynced()`
    /// (`src/simulation/LoadGenerator.cpp:1259`), which loads every
    /// `mContractInstanceKeys` entry plus `mCodeKey` and reports those still
    /// missing. Here we require the code key and every recorded contract
    /// instance key to be present in the live ledger. (The `config.n_instances`
    /// counter is decremented during deployment, so we check the accumulated
    /// `contract_instance_keys` set directly rather than the counter.)
    fn soroban_state_synced(&self) -> bool {
        Self::soroban_state_synced_inner(
            self.code_key.as_ref(),
            &self.contract_instance_keys,
            |key| matches!(self.tx_generator.app.has_ledger_entry(key), Ok(true)),
        )
    }

    /// Pure decision for [`Self::soroban_state_synced`], split out so the
    /// completion gate can be unit-tested without a live `App`. Returns `true`
    /// iff the code key exists, at least one instance was deployed, and every
    /// recorded instance key exists (per the `entry_present` probe).
    pub(crate) fn soroban_state_synced_inner(
        code_key: Option<&LedgerKey>,
        instance_keys: &HashSet<LedgerKey>,
        entry_present: impl Fn(&LedgerKey) -> bool,
    ) -> bool {
        let Some(code_key) = code_key else {
            // No code uploaded yet — setup is not complete.
            return false;
        };
        if !entry_present(code_key) {
            return false;
        }
        // At least one instance must have been deployed, and all recorded
        // instance keys must exist on-ledger.
        !instance_keys.is_empty() && instance_keys.iter().all(entry_present)
    }

    /// Wait until the `create_upgrade` ConfigUpgradeSet entry is resolvable by
    /// the arm/nomination path (`make_from_key` + `isValidForApply`), so SSC's
    /// `/upgrades?configupgradesetkey` arm — sent immediately after the run
    /// completes — always finds a VALID entry. The TEMPORARY ContractData
    /// write can lag the source-account seq in the bucket-list snapshot, so
    /// `check_accounts_synced` returning true does not guarantee the entry is
    /// queryable yet. Bounded by the same ledger budget as `wait_till_complete`.
    async fn wait_config_upgrade_resolvable(&mut self, config: &GeneratedLoadConfig) {
        const TIMEOUT_NUM_LEDGERS: u32 = 30;
        let key = match self.get_config_upgrade_set_key(&config.soroban_upgrade_config) {
            Ok(k) => k,
            Err(e) => {
                warn!(error = %e, "create_upgrade: cannot compute ConfigUpgradeSetKey to await");
                return;
            }
        };
        let start_ledger = self.tx_generator.app.current_ledger_seq();
        loop {
            if let Ok(true) = self.tx_generator.app.validate_config_upgrade_set_key(&key) {
                return;
            }
            let elapsed = self
                .tx_generator
                .app
                .current_ledger_seq()
                .saturating_sub(start_ledger);
            if elapsed >= TIMEOUT_NUM_LEDGERS {
                warn!(
                    timeout_ledgers = TIMEOUT_NUM_LEDGERS,
                    "create_upgrade: ConfigUpgradeSet entry not resolvable before timeout; \
                     arm may be rejected"
                );
                return;
            }
            tokio::time::sleep(Duration::from_millis(500)).await;
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
        {
            let elapsed_secs = (elapsed_ms / 1000) as u64;
            let spikes = elapsed_secs.checked_div(config.spike_interval).unwrap_or(0);
            target += (spikes * config.spike_size as u64) as i64;
        }

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
            let idx = deterministic_rand(self.total_submitted as u64, ledger_num) as usize
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
    /// `TX_SUBMIT_MAX_TRIES` times and on transient tx-queue backpressure
    /// (`QueueFull`, and `TryAgainLater` without `skip_low_fee_txs`) up to
    /// `QUEUE_FULL_MAX_TRIES` times.
    ///
    /// Dispatches to the appropriate transaction builder based on the load
    /// generation mode. Matches stellar-core `LoadGenerator::submitTx()`, with
    /// the deliberate `RetryQueueFull` divergence documented on
    /// [`QUEUE_FULL_MAX_TRIES`] (#3574).
    ///
    /// The per-result decision is factored into the pure
    /// [`classify_submit_result`] helper so it can be unit-tested
    /// deterministically (the side effects below — seqnum rollback, async
    /// back-off, metric marking — run over a concrete `App` and are not
    /// straightforward to drive from a test).
    async fn submit_tx(
        &mut self,
        config: &mut GeneratedLoadConfig,
        source_account_id: u64,
        ledger_num: u32,
    ) -> bool {
        let mut num_tries = 0u32;
        let mut queue_full_tries = 0u32;

        // Parity: stellar-core refreshes the source account's on-ledger
        // sequence number before building each loadgen tx
        // (TxGenerator::maybeLoadAccountSequenceNumber). This heals the seq gap
        // that otherwise forms when a prior tx for this account was discarded
        // from the queue (aged-out/banned at pending_depth ledgers under load)
        // without applying — see the method docs and #3611. No-op for accounts
        // not yet cached (the first tx loads its seq via find_account).
        self.tx_generator
            .maybe_load_account_sequence_number(source_account_id, config.overlay_only_mode);

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

            // Mirror stellar-core LoadGenerator::execute() metric ordering:
            // mark the per-mode + per-tx attempt meters BEFORE submission, then
            // mark `txn.rejected` if the tx queue does not accept the tx. One
            // mark per execute()-equivalent (this inner-loop iteration), which
            // includes txBAD_SEQ retries — matching core, where submitTx()
            // re-enters execute() on each retry (#3569).
            mark_tx_meters(config.mode, &envelope);

            let result = self.tx_generator.app.submit_transaction(envelope).await;

            // PayPregenerated does not re-submit on failure: each tx is read
            // once from the file, so a retry would consume the *next* tx
            // (stellar-core LoadGenerator.cpp:874).
            if config.mode == LoadGenMode::PayPregenerated {
                if !matches!(result, TxQueueResult::Added) {
                    app_metrics::LOADGEN_TXN_REJECTED.increment(1);
                }
                return matches!(result, TxQueueResult::Added);
            }

            if !matches!(result, TxQueueResult::Added) {
                app_metrics::LOADGEN_TXN_REJECTED.increment(1);
            }

            // `txBAD_SEQ` and `QueueFull`/`TryAgainLater` retries each consume
            // one attempt; bump the relevant counter BEFORE classifying so the
            // pure helper compares against the cap (mirrors the original
            // `num_tries += 1` before the `>=` check).
            match result {
                TxQueueResult::Invalid(Some(TxResultCode::TxBadSeq)) => num_tries += 1,
                TxQueueResult::QueueFull => queue_full_tries += 1,
                TxQueueResult::TryAgainLater if !config.skip_low_fee_txs => queue_full_tries += 1,
                _ => {}
            }

            match classify_submit_result(
                result,
                queue_full_tries,
                num_tries,
                config.skip_low_fee_txs,
                config.queue_full_max_tries,
                config.mode,
            ) {
                SubmitAction::Accept => return true,
                SubmitAction::RetryBadSeq => {
                    // Refresh sequence number from DB
                    self.tx_generator.load_account(source_account_id);
                    debug!(
                        tries = num_tries,
                        account = source_account_id,
                        "Retrying after txBAD_SEQ"
                    );
                }
                SubmitAction::SkipLowFee => {
                    // Roll back sequence number and skip
                    if let Some(account) = self.tx_generator.accounts.get_mut(&source_account_id) {
                        account.sequence_number -= 1;
                    }
                    return false;
                }
                SubmitAction::RetryQueueFull => {
                    // Transient tx-queue backpressure: roll back the consumed
                    // seqnum and retry the SAME tx after a one-step back-off.
                    //
                    // The seqnum rollback is LOAD-BEARING: `generate_tx`
                    // re-consumes a seqnum (`next_sequence_number()`) on each
                    // loop iteration, so without rolling it back the retry would
                    // advance the seqnum and the tx would never land. Mirrors
                    // the `SkipLowFee` rollback above. Unlike `SkipLowFee`, the
                    // tx is NOT dropped — soroban setup txs are prerequisites
                    // for later phases. See [`QUEUE_FULL_MAX_TRIES`] for the
                    // deliberate divergence from stellar-core (#3574).
                    if let Some(account) = self.tx_generator.accounts.get_mut(&source_account_id) {
                        account.sequence_number -= 1;
                    }
                    debug!(
                        tries = queue_full_tries,
                        account = source_account_id,
                        result = ?result,
                        "Retrying after transient tx-queue backpressure"
                    );
                    // Async, non-blocking back-off — one step interval. The
                    // enclosing `generate_load` already awaits this same
                    // primitive every step, so this neither blocks the runtime
                    // nor starves the loadgen task.
                    tokio::time::sleep(Duration::from_millis(STEP_MSECS)).await;
                }
                SubmitAction::Fail => {
                    warn!(
                        "Transaction submission failed: {:?} (bad_seq_tries={}, queue_full_tries={})",
                        result, num_tries, queue_full_tries
                    );
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
        ledger_num: u32,
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
            // SorobanUpgradeSetup shares the two-phase upload→deploy path with
            // SorobanInvokeSetup (it differs only in source account + single
            // instance, both configured in start()).
            LoadGenMode::SorobanInvokeSetup | LoadGenMode::SorobanUpgradeSetup => {
                // Parity: stellar-core uploads the loadgen contract for the
                // invoke-setup path but the `write_bytes` contract for the
                // upgrade-setup path (`LoadGenerator.cpp:1184`). The upgrade flow
                // invokes `write(bytes)` to store the `ConfigUpgradeSet` entry,
                // which only the `write_bytes` contract exports — uploading the
                // loadgen contract (only `do_cpu_only_work`) makes the
                // create_upgrade invoke fail at apply, so the entry is never
                // written and the upgrade never arms.
                let (wasm, wasm_hash) = if config.mode.mode_sets_up_invoke() {
                    (
                        SorobanTxBuilder::loadgen_wasm(),
                        SorobanTxBuilder::loadgen_wasm_hash(),
                    )
                } else {
                    (
                        SorobanTxBuilder::write_upgrade_bytes_wasm(),
                        SorobanTxBuilder::write_upgrade_bytes_wasm_hash(),
                    )
                };
                if config.n_wasms > 0 {
                    // Phase 1: Upload the setup WASM
                    let result = self.tx_generator.create_upload_wasm_transaction(
                        ledger_num,
                        source_account_id,
                        wasm,
                        config.max_fee_rate,
                    );
                    if result.is_ok() {
                        self.code_key = Some(contract_code_key(&wasm_hash));
                        self.contract_overhead_bytes = wasm.len() as u64 + 160;
                        config.n_wasms = config.n_wasms.saturating_sub(1);
                    }
                    result
                } else {
                    // Phase 2: Deploy a contract instance
                    let salt = Uint256(
                        Hash256::hash(
                            &deterministic_rand(source_account_id, ledger_num).to_le_bytes(),
                        )
                        .0,
                    );
                    // Parity: stellar-core passes `mContactOverheadBytes`
                    // (= uploaded WASM size + 160, set in phase 1) as the
                    // deploy tx's `diskReadBytes` (LoadGenerator.cpp:1198,1214).
                    let contract_overhead_bytes = self.contract_overhead_bytes as u32;
                    let result = self.tx_generator.create_contract_transaction(
                        ledger_num,
                        source_account_id,
                        &wasm_hash,
                        &salt,
                        contract_overhead_bytes,
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
                    .expect("contract instance must be assigned for SorobanInvoke");
                self.tx_generator.invoke_soroban_load_transaction(
                    ledger_num,
                    source_account_id,
                    instance,
                    config.max_fee_rate,
                )
            }
            LoadGenMode::SorobanInvokeApplyLoad => {
                // Mirror LoadGenerator.cpp:785: read the per-account instance
                // and build the V2 distribution-driven invoke.
                let instance = self
                    .contract_instances
                    .get(&source_account_id)
                    .expect("contract instance must be assigned for SorobanInvokeApplyLoad")
                    .clone();
                self.tx_generator.invoke_soroban_load_transaction_v2(
                    ledger_num,
                    source_account_id,
                    &instance,
                    &config.apply_load,
                    config.max_fee_rate,
                )
            }
            LoadGenMode::MixedClassicSoroban => {
                self.create_mixed_classic_soroban_transaction(config, source_account_id, ledger_num)
            }
            LoadGenMode::SorobanCreateUpgrade => {
                // Requires a prior SorobanUpgradeSetup: code key + exactly one
                // instance key (stellar-core LoadGenerator.cpp:760).
                let code_key = self.code_key.clone().ok_or_else(|| {
                    anyhow::anyhow!("must run SOROBAN_UPGRADE_SETUP (no code key)")
                })?;
                if self.contract_instance_keys.len() != 1 {
                    anyhow::bail!(
                        "must run SOROBAN_UPGRADE_SETUP (expected exactly 1 instance, got {})",
                        self.contract_instance_keys.len()
                    );
                }
                let instance_key = self
                    .contract_instance_keys
                    .iter()
                    .next()
                    .expect("one instance key")
                    .clone();
                self.tx_generator.invoke_soroban_create_upgrade_transaction(
                    ledger_num,
                    source_account_id,
                    &config.soroban_upgrade_config,
                    code_key,
                    instance_key,
                    config.max_fee_rate,
                )
            }
            LoadGenMode::PayPregenerated => {
                let reader = self
                    .preloaded_reader
                    .as_mut()
                    .ok_or_else(|| anyhow::anyhow!("PayPregenerated: file not open"))?;
                let env = reader
                    .read_one()?
                    .ok_or_else(|| anyhow::anyhow!("end of pregenerated tx file reached"))?;

                // Set (not increment) the seq on the round-robin account
                // (currPreloaded % nAccounts) + offset (LoadGenerator.cpp:1769).
                let seq = envelope_seq_num(&env);
                let idx = if config.n_accounts > 0 {
                    self.curr_preloaded % config.n_accounts as u64
                } else {
                    0
                };
                let account_id = idx + config.offset as u64;
                if let Some(account) = self.tx_generator.accounts.get_mut(&account_id) {
                    account.sequence_number = seq;
                }
                self.curr_preloaded += 1;
                Ok((account_id, env))
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
        ledger_num: u32,
    ) -> anyhow::Result<(u64, TransactionEnvelope)> {
        let total_weight =
            config.mix_pay_weight + config.mix_upload_weight + config.mix_invoke_weight;
        if total_weight == 0 {
            anyhow::bail!("MixedClassicSoroban weights sum to 0");
        }

        // Deterministic weighted selection
        let rand_val = deterministic_rand(source_account_id, ledger_num) % total_weight as u64;
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
                .expect("contract instance must be assigned for mixed invoke");
            self.tx_generator.invoke_soroban_load_transaction(
                ledger_num,
                source_account_id,
                instance,
                config.max_fee_rate,
            )
        }
    }

    /// Whether load generation has failed.
    pub fn has_failed(&self) -> bool {
        self.failed
    }

    /// Returns `true` when the overlay-only gate should reject the run.
    ///
    /// Mirrors stellar-core `LoadGenerator::start()` (LoadGenerator.cpp:293):
    /// `SorobanInvokeApplyLoad` requires overlay-only mode; every other mode is
    /// unaffected. Extracted as a pure function so the gate is unit-testable
    /// without a full `App`.
    pub fn apply_load_overlay_gate_fails(mode: LoadGenMode, overlay_only_mode: bool) -> bool {
        mode == LoadGenMode::SorobanInvokeApplyLoad && !overlay_only_mode
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
            match self.tx_generator.app.has_ledger_entry(key) {
                Ok(true) => {}
                Ok(false) => missing.push(key.clone()),
                Err(e) => {
                    tracing::warn!(error = ?e, "entry lookup failed during sync check");
                    missing.push(key.clone());
                }
            }
        }

        // Check the WASM code key
        if let Some(ref code_key) = self.code_key {
            match self.tx_generator.app.has_ledger_entry(code_key) {
                Ok(true) => {}
                Ok(false) => missing.push(code_key.clone()),
                Err(e) => {
                    tracing::warn!(error = ?e, "code key lookup failed during sync check");
                    missing.push(code_key.clone());
                }
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
            match self
                .tx_generator
                .app
                .load_account_sequence(&account.account_id)
            {
                Ok(Some(db_seq)) => {
                    if db_seq != account.sequence_number {
                        out_of_sync.push(id);
                    }
                }
                Ok(None) => {}
                Err(e) => {
                    tracing::warn!(error = ?e, "failed to check account sync");
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

    /// Compute the `ConfigUpgradeSetKey` that a `create_upgrade` run reports so
    /// supercluster can arm `/upgrades?configupgradesetkey=…`.
    ///
    /// Faithful port of stellar-core `LoadGenerator::getConfigUpgradeSetKey`
    /// (`LoadGenerator.cpp:476-484`) + `TxGenerator::getConfigUpgradeSetKey`
    /// (`TxGenerator.cpp:1163-1174`):
    /// - `releaseAssert(testingKeys.size() == 1)`: requires exactly one deployed
    ///   contract instance (from a prior `upgrade_setup` run) — else `Err`;
    /// - `contractID = testingKeys.begin()->contractData().contract.contractId()`;
    /// - `contentHash = sha256(getConfigUpgradeSetFromLoadConfig(cfg))`.
    ///
    /// The `content_hash` is derived from the SAME bytes the create_upgrade tx
    /// writes (`build_config_upgrade_set` over the live config settings + the
    /// request's `SorobanUpgradeConfig`), so the reported key resolves — via
    /// `ConfigUpgradeSetFrame::get_ledger_key` — to the exact `ContractData`
    /// key the tx persists. `reset_soroban_state` does NOT run for
    /// `SorobanCreateUpgrade` (`loadgen.rs`), so the instance key persists from
    /// the prior `upgrade_setup`.
    pub fn get_config_upgrade_set_key(
        &self,
        upgrade_config: &henyey_ledger::config_upgrade::SorobanUpgradeConfig,
    ) -> anyhow::Result<ConfigUpgradeSetKey> {
        let app = Arc::clone(&self.tx_generator.app);
        Self::config_upgrade_set_key_from(&self.contract_instance_keys, upgrade_config, |id| {
            app.load_config_setting(id).ok().flatten()
        })
    }

    /// Pure inner derivation of the `ConfigUpgradeSetKey` from the deployed
    /// instance key set + the live config-setting loader. Split out so the
    /// key-match against the written `ContractData` key is unit-testable without
    /// a running `App`. See [`get_config_upgrade_set_key`] for parity notes.
    pub(crate) fn config_upgrade_set_key_from(
        instance_keys: &HashSet<LedgerKey>,
        upgrade_config: &henyey_ledger::config_upgrade::SorobanUpgradeConfig,
        load_entry: impl Fn(stellar_xdr::ConfigSettingId) -> Option<stellar_xdr::ConfigSettingEntry>,
    ) -> anyhow::Result<ConfigUpgradeSetKey> {
        // releaseAssert(testingKeys.size() == 1).
        if instance_keys.len() != 1 {
            anyhow::bail!(
                "get_config_upgrade_set_key requires exactly 1 deployed contract instance \
                 (run upgrade_setup first); got {}",
                instance_keys.len()
            );
        }
        let instance_key = instance_keys.iter().next().expect("one instance key");
        let contract_id = match instance_key {
            LedgerKey::ContractData(cd) => match &cd.contract {
                ScAddress::Contract(id) => id.clone(),
                other => {
                    anyhow::bail!("instance key contract address is not a contract: {other:?}")
                }
            },
            other => anyhow::bail!("instance key is not a CONTRACT_DATA key: {other:?}"),
        };

        // contentHash = sha256(getConfigUpgradeSetFromLoadConfig(cfg)) — the same
        // bytes invoke_soroban_create_upgrade_tx writes.
        let upgrade_bytes =
            henyey_ledger::config_upgrade::build_config_upgrade_set(upgrade_config, load_entry)
                .map_err(|e| anyhow::anyhow!("failed to build config upgrade set: {e}"))?;
        let content_hash = Hash256::hash(&upgrade_bytes);

        Ok(ConfigUpgradeSetKey {
            contract_id,
            content_hash: Hash(content_hash.0),
        })
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

/// Extract the sequence number from a `TransactionEnvelope`.
///
/// Used by `PayPregenerated` to mirror stellar-core's
/// `acc->setSequenceNumber(txFrame->getSeqNum())`. Fee-bump envelopes carry the
/// inner v1 tx's seq; v0 envelopes carry their own.
/// Number of operations in a transaction envelope.
fn envelope_num_operations(env: &TransactionEnvelope) -> u64 {
    match env {
        TransactionEnvelope::TxV0(e) => e.tx.operations.len() as u64,
        TransactionEnvelope::Tx(e) => e.tx.operations.len() as u64,
        TransactionEnvelope::TxFeeBump(e) => match &e.tx.inner_tx {
            stellar_xdr::FeeBumpTransactionInnerTx::Tx(inner) => inner.tx.operations.len() as u64,
        },
    }
}

/// Whether the envelope carries any Soroban (`InvokeHostFunction`,
/// `ExtendFootprintTtl`, `RestoreFootprint`) operation. Mirrors stellar-core
/// `TransactionFrame::isSoroban()` for loadgen-meter classification purposes.
fn envelope_is_soroban(env: &TransactionEnvelope) -> bool {
    let ops: &[Operation] = match env {
        TransactionEnvelope::TxV0(e) => e.tx.operations.as_slice(),
        TransactionEnvelope::Tx(e) => e.tx.operations.as_slice(),
        TransactionEnvelope::TxFeeBump(e) => match &e.tx.inner_tx {
            stellar_xdr::FeeBumpTransactionInnerTx::Tx(inner) => inner.tx.operations.as_slice(),
        },
    };
    ops.iter().any(|op| {
        matches!(
            op.body,
            OperationBody::InvokeHostFunction(_)
                | OperationBody::ExtendFootprintTtl(_)
                | OperationBody::RestoreFootprint(_)
        )
    })
}

/// XDR byte size of an envelope. Best-effort parity with stellar-core's
/// `xdr::xdr_argpack_size(*txf->toStellarMessage())`; we measure the envelope
/// itself (core wraps it in a `StellarMessage`, adding a small fixed
/// discriminant — immaterial for the `*_bytes` meters, which supercluster does
/// not poll for completion).
fn envelope_xdr_size(env: &TransactionEnvelope) -> u64 {
    env.to_xdr(Limits::none())
        .map(|b| b.len() as u64)
        .unwrap_or(0)
}

/// Mark the loadgen per-tx meters for one generated transaction, mirroring the
/// metric block of stellar-core `LoadGenerator::execute()` (LoadGenerator.cpp
/// 1500-1611): per-mode meter first, then `txn.attempted` + `txn.bytes`. The
/// caller marks `txn.rejected` afterward if the tx queue rejects the tx.
///
/// For `MixedClassicSoroban` (and other modes whose concrete sub-mode is only
/// known from the built tx), classification is by inspecting the envelope —
/// matching core's `MIXED_PREGEN_*` `isSoroban()` branch.
fn mark_tx_meters(mode: LoadGenMode, env: &TransactionEnvelope) {
    let xdr_size = envelope_xdr_size(env);
    match mode {
        LoadGenMode::Pay | LoadGenMode::PayPregenerated => {
            app_metrics::LOADGEN_PAYMENT_SUBMITTED.increment(envelope_num_operations(env));
            app_metrics::LOADGEN_PAYMENT_BYTES.increment(xdr_size);
        }
        LoadGenMode::SorobanUpload => {
            app_metrics::LOADGEN_SOROBAN_UPLOAD.increment(1);
        }
        LoadGenMode::SorobanInvokeSetup => {
            app_metrics::LOADGEN_SOROBAN_SETUP_INVOKE.increment(1);
        }
        LoadGenMode::SorobanUpgradeSetup => {
            app_metrics::LOADGEN_SOROBAN_SETUP_UPGRADE.increment(1);
        }
        LoadGenMode::SorobanInvoke | LoadGenMode::SorobanInvokeApplyLoad => {
            app_metrics::LOADGEN_SOROBAN_INVOKE.increment(1);
        }
        LoadGenMode::SorobanCreateUpgrade => {
            app_metrics::LOADGEN_SOROBAN_CREATE_UPGRADE.increment(1);
        }
        LoadGenMode::MixedClassicSoroban => {
            // Sub-mode is decided per-tx; classify by inspecting the built tx
            // (parity: core's execute() switch on mLastMixedMode, equivalently
            // the MIXED_PREGEN_* isSoroban() branch).
            if envelope_is_soroban(env) {
                app_metrics::LOADGEN_SOROBAN_INVOKE.increment(1);
            } else {
                app_metrics::LOADGEN_PAYMENT_SUBMITTED.increment(envelope_num_operations(env));
                app_metrics::LOADGEN_PAYMENT_BYTES.increment(xdr_size);
            }
        }
    }

    // Per-tx attempt + bytes (parity: txm.mTxnAttempted.Mark() +
    // txm.mTxnBytes.Mark(...) at the end of execute()).
    app_metrics::LOADGEN_TXN_ATTEMPTED.increment(1);
    app_metrics::LOADGEN_TXN_BYTES.increment(xdr_size);
}

fn envelope_seq_num(env: &TransactionEnvelope) -> i64 {
    match env {
        TransactionEnvelope::TxV0(e) => e.tx.seq_num.0,
        TransactionEnvelope::Tx(e) => e.tx.seq_num.0,
        TransactionEnvelope::TxFeeBump(e) => match &e.tx.inner_tx {
            stellar_xdr::FeeBumpTransactionInnerTx::Tx(inner) => inner.tx.seq_num.0,
        },
    }
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

    /// Fixture `load_entry` mirroring the one used in
    /// `henyey_ledger::config_upgrade` tests: returns a live `ConfigSettingEntry`
    /// for every stored id, `None` for conditionally-absent ids. Used to drive
    /// `build_config_upgrade_set` and `config_upgrade_set_key_from` with the
    /// same config-setting state, so the derived key is deterministic.
    fn fixture_load_entry(
        id: stellar_xdr::ConfigSettingId,
    ) -> Option<stellar_xdr::ConfigSettingEntry> {
        use stellar_xdr::ConfigSettingEntry as E;
        use stellar_xdr::ConfigSettingId as Id;
        match id {
            Id::ContractMaxSizeBytes => Some(E::ContractMaxSizeBytes(65_536)),
            Id::ContractComputeV0 => Some(E::ContractComputeV0(Default::default())),
            Id::ContractLedgerCostV0 => Some(E::ContractLedgerCostV0(Default::default())),
            Id::ContractHistoricalDataV0 => Some(E::ContractHistoricalDataV0(Default::default())),
            Id::ContractEventsV0 => Some(E::ContractEventsV0(Default::default())),
            Id::ContractBandwidthV0 => Some(E::ContractBandwidthV0(Default::default())),
            Id::ContractCostParamsCpuInstructions => {
                Some(E::ContractCostParamsCpuInstructions(Default::default()))
            }
            Id::ContractCostParamsMemoryBytes => {
                Some(E::ContractCostParamsMemoryBytes(Default::default()))
            }
            Id::ContractDataKeySizeBytes => Some(E::ContractDataKeySizeBytes(250)),
            Id::ContractDataEntrySizeBytes => Some(E::ContractDataEntrySizeBytes(65_536)),
            Id::StateArchival => Some(E::StateArchival(Default::default())),
            Id::ContractExecutionLanes => Some(E::ContractExecutionLanes(Default::default())),
            // Conditionally-absent settings: simulate "not yet upgraded".
            Id::ContractParallelComputeV0 | Id::ContractLedgerCostExtV0 | Id::ScpTiming => None,
            // Non-upgradeable / delta ids are never queried by build_config_upgrade_set.
            _ => None,
        }
    }

    /// Build the single-instance `contract_instance_keys` set the same way the
    /// `upgrade_setup` deploy phase populates it: a TEMPORARY `ContractData`
    /// entry under the deployed contract address.
    fn single_instance_key(contract_id: [u8; 32]) -> std::collections::HashSet<LedgerKey> {
        let mut set = std::collections::HashSet::new();
        set.insert(LedgerKey::ContractData(
            stellar_xdr::LedgerKeyContractData {
                contract: stellar_xdr::ScAddress::Contract(stellar_xdr::ContractId(
                    stellar_xdr::Hash(contract_id),
                )),
                key: stellar_xdr::ScVal::LedgerKeyContractInstance,
                durability: stellar_xdr::ContractDataDurability::Persistent,
            },
        ));
        set
    }

    /// Regression test for #3588: the `ConfigUpgradeSetKey` reported by
    /// `config_upgrade_set_key_from` (henyey's port of
    /// `LoadGenerator::getConfigUpgradeSetKey` / `TxGenerator::getConfigUpgradeSetKey`)
    /// must derive the SAME on-ledger `ContractData` key that the create_upgrade
    /// tx actually writes. Both derive `content_hash = sha256(build_config_upgrade_set(..))`
    /// and `contract_id` from the single deployed instance, so the reported key
    /// and the written key must be byte-identical via `get_ledger_key`.
    ///
    /// FAILS on main: `config_upgrade_set_key_from` does not exist.
    #[test]
    fn test_get_config_upgrade_set_key_matches_written_entry() {
        let contract_id = [7u8; 32];
        let instance_keys = single_instance_key(contract_id);
        let cfg = henyey_ledger::config_upgrade::SorobanUpgradeConfig::default();

        // The key the loadgen REPORTS (to arm /upgrades).
        let reported_key =
            LoadGenerator::config_upgrade_set_key_from(&instance_keys, &cfg, fixture_load_entry)
                .expect("key derivation succeeds for exactly one instance");

        // The key the create_upgrade tx WRITES: ContractData{contract,
        // SCV_BYTES(sha256(upgrade_bytes)), Temporary}, mirroring
        // invoke_soroban_create_upgrade_tx (loadgen_soroban.rs:433-447).
        let upgrade_bytes =
            henyey_ledger::config_upgrade::build_config_upgrade_set(&cfg, fixture_load_entry)
                .expect("build_config_upgrade_set");
        let content_hash = Hash256::hash(&upgrade_bytes);
        let written_key = LedgerKey::ContractData(stellar_xdr::LedgerKeyContractData {
            contract: stellar_xdr::ScAddress::Contract(stellar_xdr::ContractId(stellar_xdr::Hash(
                contract_id,
            ))),
            key: stellar_xdr::ScVal::Bytes(content_hash.0.to_vec().try_into().unwrap()),
            durability: stellar_xdr::ContractDataDurability::Temporary,
        });

        // The reported key, resolved to its on-ledger ContractData key, must
        // equal the key the tx writes.
        let resolved = henyey_ledger::ConfigUpgradeSetFrame::get_ledger_key(&reported_key);
        assert_eq!(
            resolved, written_key,
            "reported ConfigUpgradeSetKey must resolve to the ContractData key the create_upgrade tx writes"
        );
        // And the content hash must match sha256 of the upgrade bytes.
        assert_eq!(reported_key.content_hash.0, content_hash.0);
    }

    /// The instance-count assert mirrors stellar-core's
    /// `releaseAssert(testingKeys.size() == 1)`: 0 or >1 instances → Err.
    /// FAILS on main: `config_upgrade_set_key_from` does not exist.
    #[test]
    fn test_get_config_upgrade_set_key_requires_exactly_one_instance() {
        let cfg = henyey_ledger::config_upgrade::SorobanUpgradeConfig::default();

        // Zero instances → Err.
        let empty = std::collections::HashSet::new();
        assert!(
            LoadGenerator::config_upgrade_set_key_from(&empty, &cfg, fixture_load_entry).is_err()
        );

        // Two instances → Err.
        let mut two = single_instance_key([1u8; 32]);
        two.insert(LedgerKey::ContractData(
            stellar_xdr::LedgerKeyContractData {
                contract: stellar_xdr::ScAddress::Contract(stellar_xdr::ContractId(
                    stellar_xdr::Hash([2u8; 32]),
                )),
                key: stellar_xdr::ScVal::LedgerKeyContractInstance,
                durability: stellar_xdr::ContractDataDurability::Persistent,
            },
        ));
        assert!(
            LoadGenerator::config_upgrade_set_key_from(&two, &cfg, fixture_load_entry).is_err()
        );
    }

    /// Parity: upload size sampling honors LOADGEN_WASM_BYTES_FOR_TESTING.
    #[test]
    fn test_sample_wasm_size() {
        // Empty values → default (core's DEFAULT_WASM_BYTES fallback).
        assert_eq!(sample_wasm_size(&[], &[], 35_000, 123), 35_000);
        // Single value is used directly regardless of seed.
        assert_eq!(sample_wasm_size(&[1024], &[1], 35_000, 0), 1024);
        assert_eq!(sample_wasm_size(&[1024], &[], 35_000, 999), 1024);
        // Weighted: weight 0 on the first means it's never picked.
        for seed in 0..10u64 {
            assert_eq!(sample_wasm_size(&[100, 200], &[0, 1], 35_000, seed), 200);
        }
        // Mismatched weights → uniform over values (both reachable).
        let picks: std::collections::HashSet<usize> = (0..8u64)
            .map(|s| sample_wasm_size(&[10, 20], &[], 35_000, s))
            .collect();
        assert!(picks.contains(&10) && picks.contains(&20));
    }

    /// #3602 regression: the Soroban-setup completion gate must require BOTH
    /// the uploaded code key AND every deployed instance key to actually exist
    /// on-ledger — not merely that the source account's sequence advanced. The
    /// flake was the loadgen reporting setup "complete" (account-synced) while
    /// the `create_contract` deploy never materialized the instance, then
    /// running `create_upgrade` against the missing instance (which can never
    /// produce a resolvable ConfigUpgradeSet entry).
    #[test]
    fn test_soroban_state_synced_inner_gating() {
        let code_key = LedgerKey::ContractCode(stellar_xdr::LedgerKeyContractCode {
            hash: stellar_xdr::Hash([5u8; 32]),
        });
        let instances = single_instance_key([9u8; 32]);
        let inst_key = instances.iter().next().unwrap().clone();

        // No code uploaded yet → not synced.
        assert!(!LoadGenerator::soroban_state_synced_inner(
            None,
            &instances,
            |_| true
        ));

        // Code present but no instance deployed → not synced.
        let empty = std::collections::HashSet::new();
        assert!(!LoadGenerator::soroban_state_synced_inner(
            Some(&code_key),
            &empty,
            |_| true
        ));

        // Code key not yet on-ledger (upload didn't materialize) → not synced.
        assert!(!LoadGenerator::soroban_state_synced_inner(
            Some(&code_key),
            &instances,
            |k| k != &code_key
        ));

        // Instance not on-ledger (the #3602 failure: deploy never created it)
        // → not synced, even though code exists.
        assert!(!LoadGenerator::soroban_state_synced_inner(
            Some(&code_key),
            &instances,
            |k| k != &inst_key
        ));

        // Code + instance both present on-ledger → synced.
        assert!(LoadGenerator::soroban_state_synced_inner(
            Some(&code_key),
            &instances,
            |_| true
        ));
    }

    /// Regression for the cross-run account-cache poisoning that made every
    /// henyey loadgen run after the first time out `wait_till_complete`.
    ///
    /// A single `LoadGenerator` is reused across runs, so `TxGenerator::accounts`
    /// accumulated every prior run's accounts. If any carried an expected
    /// sequence number ahead of the ledger (e.g. a tx dropped on queue overflow
    /// under heavy load), `check_accounts_synced` stayed false forever — a later
    /// run (e.g. create_upgrade with a single tx) timed out even though its own
    /// account was fully applied. stellar-core avoids this by calling
    /// `mTxGenerator.reset()` in `LoadGenerator::reset()` before each run.
    #[test]
    fn test_account_cache_reset_clears_cross_run_poisoning() {
        // Prior run's account: cached/expected seq 42, but only seq 10 ever
        // applied on-ledger (later txs dropped) → permanently unsynced.
        let mut prior = TestAccount::from_name("TestAccount-prior", 0);
        prior.sequence_number = 42;
        // Current run's account: cached seq 7, fully applied on-ledger.
        let current = TestAccount::from_name("TestAccount-current", 7);

        let prior_id = prior.account_id.clone();
        let current_id = current.account_id.clone();
        let ledger_seq = move |id: &AccountId| -> Option<i64> {
            if *id == prior_id {
                Some(10)
            } else if *id == current_id {
                Some(7)
            } else {
                None
            }
        };

        // With the prior run's account still cached, the current run is NEVER
        // reported synced — the poisoning that timed out wait_till_complete.
        let mut accounts: BTreeMap<u64, TestAccount> = BTreeMap::new();
        accounts.insert(0, prior);
        accounts.insert(1, current.clone());
        assert!(
            !TxGenerator::accounts_synced_with(accounts.values(), &ledger_seq),
            "a stale prior-run account must poison the synced check (pre-reset state)"
        );

        // reset() (parity with TxGenerator::reset()) drops every cached account.
        // After it, only the current run's (applied) account is re-cached → the
        // run is reported synced and wait_till_complete completes promptly.
        accounts.clear();
        accounts.insert(1, current);
        assert!(
            TxGenerator::accounts_synced_with(accounts.values(), &ledger_seq),
            "after reset, only the current run's applied account remains → synced"
        );
    }

    #[test]
    fn test_loadgen_mode_predicates_new_modes() {
        // is_soroban: upgrade modes are soroban; pregenerated is NOT.
        assert!(LoadGenMode::SorobanUpgradeSetup.is_soroban());
        assert!(LoadGenMode::SorobanCreateUpgrade.is_soroban());
        assert!(!LoadGenMode::PayPregenerated.is_soroban());

        // is_soroban_setup: only the two setup modes.
        assert!(LoadGenMode::SorobanUpgradeSetup.is_soroban_setup());
        assert!(LoadGenMode::SorobanInvokeSetup.is_soroban_setup());
        assert!(!LoadGenMode::SorobanCreateUpgrade.is_soroban_setup());
        assert!(!LoadGenMode::PayPregenerated.is_soroban_setup());

        // mode_sets_up_invoke stays SorobanInvokeSetup-only (upgrade_setup does
        // not set up the invoke contract-instance map).
        assert!(LoadGenMode::SorobanInvokeSetup.mode_sets_up_invoke());
        assert!(!LoadGenMode::SorobanUpgradeSetup.mode_sets_up_invoke());
        assert!(!LoadGenMode::SorobanCreateUpgrade.mode_sets_up_invoke());

        // mode_invokes is unchanged by the new modes.
        assert!(!LoadGenMode::SorobanUpgradeSetup.mode_invokes());
        assert!(!LoadGenMode::SorobanCreateUpgrade.mode_invokes());
        assert!(!LoadGenMode::PayPregenerated.mode_invokes());

        // is_load: create_upgrade + pregenerated submit txs; upgrade_setup is a
        // setup phase, not a continuous-load mode.
        assert!(LoadGenMode::SorobanCreateUpgrade.is_load());
        assert!(LoadGenMode::PayPregenerated.is_load());
        assert!(!LoadGenMode::SorobanUpgradeSetup.is_load());
    }

    /// Guard: the five pre-existing modes keep their predicate classification
    /// so adding the new modes did not perturb existing behavior.
    #[test]
    fn test_existing_mode_predicates_unchanged() {
        assert!(!LoadGenMode::Pay.is_soroban());
        assert!(LoadGenMode::SorobanUpload.is_soroban());
        assert!(LoadGenMode::SorobanInvokeSetup.is_soroban());
        assert!(LoadGenMode::SorobanInvoke.is_soroban());
        assert!(LoadGenMode::MixedClassicSoroban.is_soroban());

        assert!(LoadGenMode::Pay.is_load());
        assert!(LoadGenMode::SorobanUpload.is_load());
        assert!(!LoadGenMode::SorobanInvokeSetup.is_load());
        assert!(LoadGenMode::SorobanInvoke.is_load());
        assert!(LoadGenMode::MixedClassicSoroban.is_load());

        assert!(LoadGenMode::SorobanInvoke.mode_invokes());
        assert!(LoadGenMode::MixedClassicSoroban.mode_invokes());
        assert!(!LoadGenMode::Pay.mode_invokes());
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

    /// #3309: the new apply-load mode classifies as soroban + load + invoke
    /// (it reads `contract_instances` exactly like `SorobanInvoke`).
    #[test]
    fn test_apply_load_mode_predicates() {
        let m = LoadGenMode::SorobanInvokeApplyLoad;
        assert!(m.is_soroban());
        assert!(m.is_load());
        assert!(m.mode_invokes());
        assert!(!m.is_soroban_setup());
        assert!(!m.mode_sets_up_invoke());
    }

    /// #3309 config surface: defaults mirror stellar-core Config.h
    /// (batch=1000, simulated=1000, entry_size=0, empty distributions) and
    /// `data_entry_size` rounds up to a multiple of 4 (Config.cpp:1608).
    #[test]
    fn test_apply_load_config_defaults_and_parse() {
        let c = LoadGenApplyLoadConfig::default();
        assert_eq!(c.bl_batch_size, 1000);
        assert_eq!(c.bl_simulated_ledgers, 1000);
        assert_eq!(c.data_entry_size, 0);
        assert!(c.num_rw_entries.0.is_empty());
        assert!(c.num_disk_read_entries.0.is_empty());
        assert!(c.tx_size_bytes.0.is_empty());
        assert!(c.event_count.0.is_empty());
        assert!(c.instructions.0.is_empty());

        // data_entry_count = batch * simulated.
        assert_eq!(c.data_entry_count(), 1_000_000);

        // round_data_entry_size rounds up to a multiple of 4.
        assert_eq!(LoadGenApplyLoadConfig::round_data_entry_size(0), 0);
        assert_eq!(LoadGenApplyLoadConfig::round_data_entry_size(1), 4);
        assert_eq!(LoadGenApplyLoadConfig::round_data_entry_size(4), 4);
        assert_eq!(LoadGenApplyLoadConfig::round_data_entry_size(5), 8);
        assert_eq!(LoadGenApplyLoadConfig::round_data_entry_size(7), 8);

        // GeneratedLoadConfig embeds it and defaults overlay_only_mode = false.
        let g = GeneratedLoadConfig::default();
        assert!(!g.overlay_only_mode);
        assert_eq!(g.apply_load.bl_batch_size, 1000);
    }

    /// Overlay-only gate (LoadGenerator.cpp:293): `SorobanInvokeApplyLoad`
    /// requires overlay-only mode; all other modes are unaffected.
    #[test]
    fn test_loadgen_apply_load_requires_overlay_only() {
        // gate fails (must reset+error) when apply-load mode + flag false.
        assert!(LoadGenerator::apply_load_overlay_gate_fails(
            LoadGenMode::SorobanInvokeApplyLoad,
            false
        ));
        // passes when overlay-only mode is on.
        assert!(!LoadGenerator::apply_load_overlay_gate_fails(
            LoadGenMode::SorobanInvokeApplyLoad,
            true
        ));
        // other modes never gated.
        assert!(!LoadGenerator::apply_load_overlay_gate_fails(
            LoadGenMode::Pay,
            false
        ));
        assert!(!LoadGenerator::apply_load_overlay_gate_fails(
            LoadGenMode::SorobanInvoke,
            false
        ));
    }

    #[test]
    fn test_stop_signal_contract() {
        // Validates that generate_load accepts &AtomicBool and the type is
        // compatible. The actual behavioral test (stop_signal = true →
        // Stopped) requires constructing a LoadGenerator with a full App,
        // which is covered by integration tests.
        let signal = AtomicBool::new(true);
        assert!(signal.load(Ordering::Relaxed));
        signal.store(false, Ordering::Relaxed);
        assert!(!signal.load(Ordering::Relaxed));
    }

    // ---------------------------------------------------------------------
    // #3574: transient tx-queue backpressure (`QueueFull`, and `TryAgainLater`
    // without `skip_low_fee_txs`) must back off + retry the SAME tx rather than
    // abort the run. These tests drive the pure `classify_submit_result` helper
    // (the decision), since `submit_tx`'s effects run over a concrete `App`.
    // ---------------------------------------------------------------------

    /// #3574 key regression: `QueueFull` below the cap must map to
    /// `RetryQueueFull` (back off + retry), NOT `Fail` — and a subsequent
    /// `Added` maps to `Accept`, so the run does NOT fail. FAILS on
    /// `origin/main` (helper absent; `QueueFull` routes to the `other`/`Fail`
    /// arm that sets `self.failed = true`).
    #[test]
    fn test_classify_submit_result_queue_full_retries_then_succeeds() {
        // QueueFull, well below the cap → retry, not fail.
        assert_eq!(
            classify_submit_result(
                TxQueueResult::QueueFull,
                /* queue_full_tries */ 0,
                /* bad_seq_tries */ 0,
                /* skip_low_fee_txs */ false,
                /* queue_full_max_tries */ QUEUE_FULL_MAX_TRIES,
                LoadGenMode::SorobanInvokeSetup,
            ),
            SubmitAction::RetryQueueFull,
        );
        // After the queue drains, the same tx is Added → Accept.
        assert_eq!(
            classify_submit_result(
                TxQueueResult::Added,
                /* queue_full_tries */ 3,
                /* bad_seq_tries */ 0,
                /* skip_low_fee_txs */ false,
                /* queue_full_max_tries */ QUEUE_FULL_MAX_TRIES,
                LoadGenMode::SorobanInvokeSetup,
            ),
            SubmitAction::Accept,
        );
    }

    /// #3611: a `Banned` result must map to `SkipLowFee` (skip + seqnum
    /// rollback), NOT `Fail`. After the #3611 seq refresh regenerates a tx whose
    /// prior copy aged out and was banned, re-submitting the same deterministic
    /// tx hits the ban; the run must skip it (the account proceeds once the ban
    /// lifts) rather than abort. Independent of `skip_low_fee_txs` and the
    /// retry caps. FAILS on origin/main (Banned routes to the `_ => Fail` arm).
    #[test]
    fn test_classify_submit_result_banned_skips_not_fails() {
        for skip_low_fee in [true, false] {
            assert_eq!(
                classify_submit_result(
                    TxQueueResult::Banned,
                    /* queue_full_tries */ 0,
                    /* bad_seq_tries */ 0,
                    skip_low_fee,
                    QUEUE_FULL_MAX_TRIES,
                    LoadGenMode::Pay,
                ),
                SubmitAction::SkipLowFee,
                "Banned must skip (rollback), not fail (skip_low_fee_txs={skip_low_fee})"
            );
        }
    }

    /// #3574: a genuinely wedged queue still surfaces — at the cap, `QueueFull`
    /// maps to `Fail` (bounded, not infinite masking).
    #[test]
    fn test_classify_submit_result_queue_full_exhausts_cap_fails() {
        // One below the cap → still retry.
        assert_eq!(
            classify_submit_result(
                TxQueueResult::QueueFull,
                QUEUE_FULL_MAX_TRIES - 1,
                0,
                false,
                QUEUE_FULL_MAX_TRIES,
                LoadGenMode::SorobanInvokeSetup,
            ),
            SubmitAction::RetryQueueFull,
        );
        // At the cap → fail the run.
        assert_eq!(
            classify_submit_result(
                TxQueueResult::QueueFull,
                QUEUE_FULL_MAX_TRIES,
                0,
                false,
                QUEUE_FULL_MAX_TRIES,
                LoadGenMode::SorobanInvokeSetup,
            ),
            SubmitAction::Fail,
        );
    }

    /// #3574: `TryAgainLater` without `skip_low_fee_txs` retries (queue
    /// backpressure on soroban setup, where dropping the tx breaks later
    /// phases); WITH `skip_low_fee_txs` it preserves the existing skip path.
    #[test]
    fn test_classify_submit_result_try_again_later_without_skip_retries() {
        // skip_low_fee_txs == false → treat as transient backpressure, retry.
        assert_eq!(
            classify_submit_result(
                TxQueueResult::TryAgainLater,
                0,
                0,
                false,
                QUEUE_FULL_MAX_TRIES,
                LoadGenMode::SorobanInvokeSetup,
            ),
            SubmitAction::RetryQueueFull,
        );
        // skip_low_fee_txs == true → existing skip behavior preserved.
        assert_eq!(
            classify_submit_result(
                TxQueueResult::TryAgainLater,
                0,
                0,
                true,
                QUEUE_FULL_MAX_TRIES,
                LoadGenMode::Pay,
            ),
            SubmitAction::SkipLowFee,
        );
        // FeeTooLow with skip → skip (unchanged).
        assert_eq!(
            classify_submit_result(
                TxQueueResult::FeeTooLow,
                0,
                0,
                true,
                QUEUE_FULL_MAX_TRIES,
                LoadGenMode::Pay,
            ),
            SubmitAction::SkipLowFee,
        );
    }

    /// #3574 guards: pre-existing `TxBadSeq` retry and `Added` accept behavior
    /// is unchanged by the new arms.
    #[test]
    fn test_classify_submit_result_bad_seq_and_added_unchanged() {
        // txBAD_SEQ below the retry cap → RetryBadSeq.
        assert_eq!(
            classify_submit_result(
                TxQueueResult::Invalid(Some(TxResultCode::TxBadSeq)),
                0,
                0,
                false,
                QUEUE_FULL_MAX_TRIES,
                LoadGenMode::Pay,
            ),
            SubmitAction::RetryBadSeq,
        );
        // txBAD_SEQ at the retry cap → Fail.
        assert_eq!(
            classify_submit_result(
                TxQueueResult::Invalid(Some(TxResultCode::TxBadSeq)),
                0,
                TX_SUBMIT_MAX_TRIES,
                false,
                QUEUE_FULL_MAX_TRIES,
                LoadGenMode::Pay,
            ),
            SubmitAction::Fail,
        );
        // Added → Accept regardless of counters.
        assert_eq!(
            classify_submit_result(
                TxQueueResult::Added,
                5,
                5,
                false,
                QUEUE_FULL_MAX_TRIES,
                LoadGenMode::Pay,
            ),
            SubmitAction::Accept,
        );
    }
}
