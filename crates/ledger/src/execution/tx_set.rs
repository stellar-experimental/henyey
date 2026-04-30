//! Transaction set execution orchestration.
//!
//! Coordinates the execution of an entire transaction set within a ledger
//! close, including parallel Soroban phase execution via `tokio::spawn_blocking`,
//! fee pre-deduction for Soroban transactions, and sequential classic phase
//! processing.

use std::sync::Arc;

use super::*;

/// Result of a parallel cluster execution: (result, elapsed_us, cluster_index).
type ClusterThreadResult = (Result<(TxSetResult, LedgerDelta, i64)>, u64, usize);

/// How the tx-set orchestrator sources fee information before executing transaction bodies.
///
/// # Invariants
/// - `PreChargeInternally`: The orchestrator calls `process_fee_only` for each TX
///   and records the fee pool delta itself. The executor runs with `FeeMode::Skip`.
/// - `ExternallyPrecharged`: The slice length MUST equal the transaction count and
///   be in the same order. Validated at runtime (returns error on mismatch).
///   The caller MUST have already deducted fees on the delta and recorded the fee
///   pool delta. The executor runs with `FeeMode::Skip`.
/// - `NoFees`: No fee deduction occurs anywhere. The executor runs with `FeeMode::Skip`.
///   Valid for test harnesses and offline replay contexts that skip fee processing.
#[derive(Clone, Debug)]
pub enum FeeStrategy<'a> {
    /// Pre-charge fees internally via process_fee_only before executing bodies.
    PreChargeInternally,
    /// Fees were already deducted externally; use the provided records.
    ExternallyPrecharged(&'a [PreChargedFee]),
    /// No fee charging — executor runs with FeeMode::Skip.
    NoFees,
}

/// Fee source for the Soroban parallel phase.
#[derive(Clone, Debug)]
pub enum SorobanFeeSource {
    /// Fees already deducted externally; use provided records.
    ExternallyPrecharged(Vec<PreChargedFee>),
    /// Pre-deduct Soroban fees internally from the delta.
    DeductInternally,
}

pub struct RunTransactionsParams<'a> {
    pub executor: &'a mut TransactionExecutor,
    pub snapshot: &'a SnapshotHandle,
    pub transactions: &'a [TxWithFee],
    pub base_fee: u32,
    pub soroban_base_prng_seed: [u8; 32],
    pub fee_strategy: FeeStrategy<'a>,
    pub delta: &'a mut LedgerDelta,
}

/// Execute a full transaction set.
///
/// # Arguments
///
/// * `soroban` - Soroban execution context (config, PRNG seed, module cache, etc.)
pub fn execute_transaction_set(
    snapshot: &SnapshotHandle,
    transactions: &[TxWithFee],
    context: &LedgerContext,
    delta: &mut LedgerDelta,
    soroban: SorobanContext<'_>,
) -> Result<TxSetResult> {
    let id_pool = snapshot.header().id_pool;
    let mut executor =
        TransactionExecutor::new(context, id_pool, soroban.config, soroban.classic_events);
    executor.set_meta_flags(
        soroban.emit_soroban_tx_meta_ext_v1,
        soroban.enable_soroban_diagnostic_events,
    );
    // Set the module cache if provided for better Soroban performance
    if let Some(cache) = soroban.module_cache {
        executor.set_module_cache(cache.clone());
    }
    // Set the hot archive for Protocol 23+ entry restoration
    if let Some(ha) = soroban.hot_archive {
        executor.set_hot_archive(ha);
    }

    // Set shared offer store for classic phase execution
    if let Some(ref offer_store) = soroban.offer_store {
        executor.set_offer_store(offer_store.clone());
    }

    run_transactions_on_executor(RunTransactionsParams {
        executor: &mut executor,
        snapshot,
        transactions,
        base_fee: context.base_fee,
        soroban_base_prng_seed: soroban.base_prng_seed,
        fee_strategy: FeeStrategy::PreChargeInternally,
        delta,
    })
}

/// Execute transactions on a pre-configured executor, apply results to delta.
///
/// This is the core transaction execution loop, separated from executor
/// creation/setup so it can be used both by `execute_transaction_set`
/// (which creates a fresh executor) and by `LedgerCloseContext::apply_transactions`
/// (which reuses a persistent executor across ledger closes to avoid reloading ~911K offers).
///
/// The `fee_strategy` field determines how fees are sourced:
/// - `PreChargeInternally`: fees are pre-charged via `process_fee_only` before body execution.
/// - `ExternallyPrecharged`: fees were already deducted on the delta by caller.
/// - `NoFees`: no fee processing occurs.
pub fn run_transactions_on_executor(params: RunTransactionsParams<'_>) -> Result<TxSetResult> {
    let RunTransactionsParams {
        executor,
        snapshot,
        transactions,
        base_fee,
        soroban_base_prng_seed,
        fee_strategy,
        delta,
    } = params;
    let ledger_seq = executor.ledger_seq;
    let protocol_version = executor.protocol_version;

    prefetch_classic_keys(executor, snapshot, transactions)?;

    // Pre-deduct fees before executing any TX body.
    let pre_fee_results: Vec<PreChargedFee> = match fee_strategy {
        FeeStrategy::ExternallyPrecharged(ext) => {
            if ext.len() != transactions.len() {
                return Err(crate::LedgerError::Internal(format!(
                    "ExternallyPrecharged fees count ({}) != transaction count ({})",
                    ext.len(),
                    transactions.len()
                )));
            }
            ext.to_vec()
        }
        FeeStrategy::PreChargeInternally => {
            let mut results = Vec::with_capacity(transactions.len());
            for (tx, tx_base_fee) in transactions.iter() {
                let tx_fee = tx_base_fee.unwrap_or(base_fee);
                let (fee_changes, charged_fee) = executor.process_fee_only(snapshot, tx, tx_fee)?;
                results.push(PreChargedFee {
                    charged_fee,
                    fee_changes,
                });
            }
            results
        }
        FeeStrategy::NoFees => Vec::new(),
    };
    let has_pre_charged = !pre_fee_results.is_empty();

    compute_max_seq_num_to_apply(executor, transactions);

    let mut results = Vec::with_capacity(transactions.len());
    let mut tx_results = Vec::with_capacity(transactions.len());
    let mut tx_result_metas = Vec::with_capacity(transactions.len());

    for (tx_index, (tx, tx_base_fee)) in transactions.iter().enumerate() {
        // Flush pending RO TTL bumps for keys in this TX's write footprint
        // (matches stellar-core's flushRoTTLBumpsInTxWriteFootprint).
        if let Some(write_keys) = soroban_write_footprint(tx) {
            executor
                .state_mut()
                .flush_ro_ttl_bumps_for_write_footprint(&write_keys);
        }

        // Snapshot the delta before starting each transaction.
        // This preserves committed changes from previous transactions so they're
        // not lost if this transaction fails and rolls back.
        executor.state.snapshot_delta();

        let tx_fee = tx_base_fee.unwrap_or(base_fee);
        // Compute per-transaction PRNG seed: subSha256(basePrngSeed, txIndex)
        let tx_prng_seed = sub_sha256(&soroban_base_prng_seed, tx_index as u32);
        // Execute with FeeMode::Skip — fees were already pre-deducted above
        // (PreChargeInternally/ExternallyPrecharged) or not needed (NoFees).
        // For protocol 24+, the body always executes after fee pre-deduction,
        // matching stellar-core's behavior where feeToPay=0 in applying mode.
        let mut result = executor.execute_transaction_with_arc(
            snapshot,
            TransactionExecutionRequest {
                tx_envelope: Arc::clone(tx),
                base_fee: tx_fee,
                soroban_prng_seed: Some(tx_prng_seed),
                fee_mode: FeeMode::Skip,
            },
        )?;

        // When fees were pre-charged, the executor runs with
        // FeeMode::Skip so early validation failures (e.g. TxNoAccount)
        // report fee_charged=0. Override with the actual pre-charged amount
        // so the result matches the fee already deducted on the delta.
        if has_pre_charged {
            result.fee_charged = pre_fee_results[tx_index]
                .charged_fee
                .saturating_sub(result.fee_refund);
        }

        let frame = TransactionFrame::with_network(Arc::clone(tx), executor.network_id);

        let tx_result = build_tx_result_pair(
            &frame,
            &executor.network_id,
            &result,
            tx_fee as i64,
            protocol_version,
        )?;
        let tx_meta = result.tx_meta.take().unwrap_or_else(empty_transaction_meta);
        // Use pre-captured fee_changes from the upfront fee deduction pass
        // (either internal or external), or the per-TX result if fees were
        // not pre-deducted.
        let fee_changes = if has_pre_charged {
            pre_fee_results[tx_index].fee_changes.clone()
        } else {
            result
                .fee_changes
                .take()
                .unwrap_or_else(empty_entry_changes)
        };
        let post_fee_changes = result
            .post_fee_changes
            .take()
            .unwrap_or_else(empty_entry_changes);
        let tx_result_meta = TransactionResultMetaV1 {
            ext: ExtensionPoint::V0,
            result: tx_result.clone(),
            fee_processing: fee_changes,
            tx_apply_processing: tx_meta,
            post_tx_apply_fee_processing: post_fee_changes,
        };

        debug!(
            success = result.success,
            fee = result.fee_charged,
            fee_refund = result.fee_refund,
            ops = result.operation_results.len(),
            ledger_seq = ledger_seq,
            tx_index = tx_index,
            "Executed transaction"
        );

        // Record hot-archive restored keys from successful TXs so subsequent
        // TXs see them. Only successful TXs contribute (failed TXs are rolled back).
        if result.success {
            executor.record_hot_archive_restores(&result.hot_archive_restored_keys);
        }

        results.push(result);
        tx_results.push(tx_result);
        tx_result_metas.push(tx_result_meta);
    }

    // Protocol 23+: Apply Soroban fee refunds after ALL transactions
    if has_pre_charged {
        let post_fee_changes_list =
            apply_soroban_fee_refunds(executor, transactions, &results, ledger_seq);
        // Patch post_tx_apply_fee_processing in the per-tx metas
        for (tx_idx, changes) in post_fee_changes_list {
            if tx_idx < tx_result_metas.len() {
                tx_result_metas[tx_idx].post_tx_apply_fee_processing = changes;
            }
        }
    }

    // Flush deferred read-only TTL bumps to the delta before applying to bucket list.
    // These are TTL updates for read-only entries that were NOT included in transaction
    // meta but MUST be written to the bucket list.
    executor.state_mut().flush_deferred_ro_ttl_bumps();

    // Apply all changes to the delta
    executor.apply_to_delta(delta)?;

    // Add fees to fee pool.
    // When fees were externally precharged, the caller already recorded the
    // fee pool delta on the main delta, so we only record for internal fees.
    if matches!(fee_strategy, FeeStrategy::PreChargeInternally) {
        let total_fees = executor.total_fees();
        delta.record_fee_pool_delta(total_fees);
    }

    // Collect hot archive restored keys from SUCCESSFUL transactions only.
    // Failed transactions are rolled back and must not contribute
    // HOT_ARCHIVE_LIVE tombstones.
    let mut all_hot_archive_restored_keys: Vec<LedgerKey> = Vec::new();
    for result in &results {
        if result.success {
            all_hot_archive_restored_keys.extend(result.hot_archive_restored_keys.iter().cloned());
        }
    }

    Ok(TxSetResult {
        results,
        tx_results,
        tx_result_metas,
        id_pool: executor.id_pool(),
        hot_archive_restored_keys: all_hot_archive_restored_keys,
    })
}

/// Prefetch all statically-known keys for the transaction set.
fn prefetch_classic_keys(
    _executor: &TransactionExecutor,
    snapshot: &SnapshotHandle,
    transactions: &[TxWithFee],
) -> Result<()> {
    let prefetch_start = std::time::Instant::now();
    let mut all_keys = std::collections::HashSet::new();
    for (tx, _) in transactions {
        let frame = TransactionFrame::new(Arc::clone(tx));
        for key in frame.keys_for_fee_processing() {
            all_keys.insert(key);
        }
        all_keys.extend(frame.keys_for_apply());
    }
    let keys_vec: Vec<LedgerKey> = all_keys.into_iter().collect();
    let stats = snapshot.prefetch(&keys_vec)?;
    tracing::debug!(
        requested = stats.requested,
        loaded = stats.loaded,
        elapsed_ms = prefetch_start.elapsed().as_millis() as u64,
        "Prefetched ledger keys for classic phase"
    );
    Ok(())
}

/// Compute max sequence number per source account for AccountMerge protection.
fn compute_max_seq_num_to_apply(executor: &mut TransactionExecutor, transactions: &[TxWithFee]) {
    let mut merge_seen = false;
    let mut acc_to_max_seq: HashMap<AccountId, i64> = HashMap::new();
    for (tx, _) in transactions.iter() {
        let frame = TransactionFrame::new(Arc::clone(tx));
        let source_id = frame.source_account_id();
        let seq = frame.sequence_number();
        acc_to_max_seq
            .entry(source_id)
            .and_modify(|e| *e = (*e).max(seq))
            .or_insert(seq);
        if !merge_seen {
            for op in frame.operations() {
                if matches!(op.body, OperationBody::AccountMerge(_)) {
                    merge_seen = true;
                    break;
                }
            }
        }
    }
    if merge_seen {
        executor
            .state_mut()
            .set_max_seq_num_to_apply(acc_to_max_seq);
    }
}

/// Apply Soroban fee refunds after all transactions (protocol 23+).
/// Apply fee refunds and return per-tx post_fee_changes for meta emission.
///
/// Returns a Vec of (tx_index, LedgerEntryChanges) for transactions that had
/// non-zero refunds successfully applied. Matches stellar-core's
/// processPostTxSetApply which captures ltxInner.getChanges() for each refund.
fn apply_soroban_fee_refunds(
    executor: &mut TransactionExecutor,
    transactions: &[TxWithFee],
    results: &[TransactionExecutionResult],
    ledger_seq: u32,
) -> Vec<(usize, stellar_xdr::curr::LedgerEntryChanges)> {
    use stellar_xdr::curr::{LedgerEntryChange, LedgerEntryChanges, LedgerKeyAccount};

    let mut total_refunds = 0i64;
    let mut per_tx_changes = Vec::new();

    for (idx, (tx, _)) in transactions.iter().enumerate() {
        let refund = results[idx].fee_refund;
        if refund > 0 {
            let frame = TransactionFrame::with_network(Arc::clone(tx), executor.network_id);
            let fee_source_id = henyey_tx::muxed_to_account_id(&frame.fee_source_account());

            // Snapshot pre-refund entry for meta emission
            let account_key = LedgerKey::Account(LedgerKeyAccount {
                account_id: fee_source_id.clone(),
            });
            let pre_entry = executor.state.get_entry(&account_key);

            if executor.state.apply_refund_to_delta(&fee_source_id, refund) {
                executor.state.delta_mut().add_fee(-refund);
                total_refunds += refund;

                // Build STATE/UPDATED changes from pre/post entries
                if let Some(pre) = pre_entry {
                    if let Some(post) = executor.state.get_entry(&account_key) {
                        let changes: LedgerEntryChanges = vec![
                            LedgerEntryChange::State(pre),
                            LedgerEntryChange::Updated(post),
                        ]
                        .try_into()
                        .unwrap_or_default();
                        per_tx_changes.push((idx, changes));
                    }
                }

                tracing::debug!(
                    ledger_seq = ledger_seq,
                    tx_index = idx,
                    refund = refund,
                    fee_source = %account_id_to_strkey(&fee_source_id),
                    "Applied P23+ Soroban fee refund"
                );
            } else {
                tracing::debug!(
                    ledger_seq = ledger_seq,
                    tx_index = idx,
                    refund = refund,
                    fee_source = %account_id_to_strkey(&fee_source_id),
                    "Skipped Soroban fee refund (account merged or balance overflow)"
                );
            }
        }
    }
    if total_refunds > 0 {
        tracing::debug!(
            ledger_seq = ledger_seq,
            total_refunds = total_refunds,
            tx_count = transactions.len(),
            "P23+ Soroban fee refunds applied"
        );
    }
    per_tx_changes
}

/// Result of a per-TX global pre-parallel-apply pass.
///
/// Matches stellar-core's `preParallelApply` which runs validation, fee charging,
/// signer removal, and sequence bumping for ALL Soroban transactions on shared
/// state before any cluster starts parallel execution.
pub(super) enum PreParallelResult {
    /// Pre-apply succeeded — carry the `PreApplyResult` to `apply_body` in the cluster.
    Success(PreApplyResult),
    /// Pre-apply failed (validation error, insufficient balance, etc.) — use this
    /// result directly in the cluster without running `apply_body`.
    Failed(TransactionExecutionResult),
}

/// Run `preParallelApply` for ALL Soroban transactions on a shared executor.
///
/// This matches stellar-core's `preParallelApplyAndCollectModifiedClassicEntries`
/// in `ParallelApplyUtils.cpp:327-385`. The function:
///
/// 1. Creates a single `TransactionExecutor` using the main delta's state.
/// 2. For each Soroban TX (across all stages/clusters, in flattened order):
///    runs `pre_apply_arc` with `FeeMode::Skip` (fees already on delta).
/// 3. Transfers all classic mutations (sequence bumps, signer removals) to
///    the main delta via `apply_to_delta`.
/// 4. Returns per-TX results consumed by `execute_single_cluster`.
///
/// After this call, the delta contains all pre-apply mutations. Subsequent
/// Parameters for the pre-parallel apply phase.
struct PreParallelParams<'a> {
    pre_charged_fees: &'a [PreChargedFee],
    classic_tx_count: usize,
    soroban_base_prng_seed: [u8; 32],
}

/// `PriorStageState::from_delta` calls will see bumped sequences and removed
/// signers, so peer clusters observe correct state.
fn pre_parallel_apply(
    snapshot: &SnapshotHandle,
    phase: &crate::close::SorobanPhaseStructure,
    context: &LedgerContext,
    soroban: &SorobanContext<'_>,
    delta: &mut LedgerDelta,
    params: &PreParallelParams<'_>,
) -> Result<Vec<PreParallelResult>> {
    let start = std::time::Instant::now();

    // Create a shared executor operating on the main delta's state.
    let mut executor = TransactionExecutor::new(
        context,
        snapshot.header().id_pool,
        soroban.config.clone(),
        soroban.classic_events,
    );
    executor.set_meta_flags(
        soroban.emit_soroban_tx_meta_ext_v1,
        soroban.enable_soroban_diagnostic_events,
    );
    if let Some(cache) = soroban.module_cache {
        executor.set_module_cache(cache.clone());
    }
    if let Some(ref ha) = soroban.hot_archive {
        executor.set_hot_archive(ha.clone());
    }
    if let Some(ref ss) = soroban.soroban_state {
        executor.set_soroban_state(ss.clone());
    }

    // Load current fee-deducted account state from the delta so the executor
    // sees balances AFTER fee pre-deduction. This is critical because
    // pre_apply_arc's validation checks (e.g., available balance for sig
    // weight, sequence number) must operate on post-fee-deduction state.
    for entry in delta.current_entries() {
        executor.state.load_entry(entry);
    }
    // Only propagate Soroban key deletions — classic deletions are irrelevant
    // for parallel Soroban execution (classic entries are loaded from snapshot).
    for key in delta
        .dead_entries()
        .iter()
        .filter(|k| super::is_soroban_key(k))
    {
        executor.state.mark_entry_deleted(key);
    }

    let total_txs: usize = phase
        .stages
        .iter()
        .flat_map(|s| s.iter())
        .map(|c| c.len())
        .sum();
    let mut results = Vec::with_capacity(total_txs);
    let mut flat_idx: usize = 0;
    let mut global_tx_offset: usize = params.classic_tx_count;

    for stage in &phase.stages {
        for cluster in stage {
            for (tx, tx_base_fee) in cluster {
                let tx_fee = tx_base_fee.unwrap_or(context.base_fee);
                let tx_prng_seed =
                    sub_sha256(&params.soroban_base_prng_seed, global_tx_offset as u32);

                let pre_result = executor.pre_apply_arc(
                    snapshot,
                    tx,
                    tx_fee,
                    Some(tx_prng_seed),
                    FeeMode::Skip, // fees already on delta
                )?;

                match pre_result {
                    Ok(pre) => results.push(PreParallelResult::Success(pre)),
                    Err(early_result) => results.push(PreParallelResult::Failed(early_result)),
                }

                flat_idx += 1;
                global_tx_offset += 1;
            }
        }
    }
    debug_assert_eq!(flat_idx, total_txs);
    debug_assert_eq!(flat_idx, params.pre_charged_fees.len());

    // Transfer all pre-apply mutations (sequence bumps, signer removals)
    // to the main delta so PriorStageState::from_delta picks them up.
    executor.apply_to_delta(delta)?;

    let elapsed_us = start.elapsed().as_micros() as u64;
    tracing::debug!(
        ledger_seq = context.sequence,
        total_txs,
        elapsed_us,
        "PROFILE pre_parallel_apply"
    );

    Ok(results)
}

/// Execute a full Soroban parallel phase (all stages sequentially,
/// clusters within each stage in parallel).
///
/// The classic phase must be executed separately before calling this function.
///
/// When `fee_source` is `SorobanFeeSource::ExternallyPrecharged`, fees have
/// already been pre-deducted on the delta by `pre_deduct_all_fees_on_delta`.
/// The internal fee deduction is skipped and the provided fee changes are used.
pub fn execute_soroban_parallel_phase(
    snapshot: &SnapshotHandle,
    phase: &crate::close::SorobanPhaseStructure,
    classic_tx_count: usize,
    context: &LedgerContext,
    delta: &mut LedgerDelta,
    soroban: SorobanContext<'_>,
    fee_source: SorobanFeeSource,
) -> Result<TxSetResult> {
    let mut all_results: Vec<TransactionExecutionResult> = Vec::new();
    let mut all_tx_results: Vec<TransactionResultPair> = Vec::new();
    let mut all_tx_result_metas: Vec<TransactionResultMetaV1> = Vec::new();
    let mut all_hot_archive_restored_keys: Vec<LedgerKey> = Vec::new();
    // Accumulate hot-archive restored keys across stages for cross-stage propagation.
    // Each stage's clusters see the accumulated set from prior stages to prevent
    // re-restoration. Mirrors stellar-core's `mPreviouslyRestoredEntries`.
    let mut accumulated_restored_keys: std::collections::HashSet<LedgerKey> =
        std::collections::HashSet::new();
    let mut id_pool = snapshot.header().id_pool;
    // Global TX offset tracks the canonical position for PRNG seed computation.
    // In stellar-core, phases are applied in order [Classic (phase 0), Soroban (phase 1)],
    // with a shared index counter starting at 0. Classic TXs get indexes 0..N-1,
    // and Soroban TXs get indexes N..N+M-1 (see LedgerManagerImpl::applyTransactions).
    let mut global_tx_offset: usize = classic_tx_count;

    // Use externally pre-charged fees if provided (from unified fee pass),
    // otherwise pre-deduct Soroban fees from the delta internally.
    let pre_charged_fees = match fee_source {
        SorobanFeeSource::ExternallyPrecharged(ext) => {
            // Fees already deducted on delta and fee pool already recorded by caller.
            ext
        }
        SorobanFeeSource::DeductInternally => {
            let (fees, total_pre_deducted) = pre_deduct_soroban_fees(
                snapshot,
                phase,
                context.base_fee,
                context.network_id,
                context.sequence,
                delta,
            )?;
            if total_pre_deducted != 0 {
                delta.record_fee_pool_delta(total_pre_deducted);
            }
            fees
        }
    };
    let soroban_base_prng_seed = soroban.base_prng_seed;

    // Prefetch fee source accounts for all Soroban TXs.
    // Soroban operations themselves return empty from collect_prefetch_keys (their
    // state is in-memory via InMemorySorobanState), but fee source account loading
    // in each cluster benefits from the prefetch cache.
    let prefetch_us;
    {
        let prefetch_start = std::time::Instant::now();
        let mut all_keys = std::collections::HashSet::new();
        for stage in &phase.stages {
            for cluster in stage {
                for (tx, _) in cluster {
                    let frame = TransactionFrame::new(Arc::clone(tx));
                    for key in frame.keys_for_fee_processing() {
                        all_keys.insert(key);
                    }
                    all_keys.extend(frame.keys_for_apply());
                }
            }
        }
        let keys_vec: Vec<LedgerKey> = all_keys.into_iter().collect();
        let stats = snapshot.prefetch(&keys_vec)?;
        prefetch_us = prefetch_start.elapsed().as_micros() as u64;
        tracing::debug!(
            requested = stats.requested,
            loaded = stats.loaded,
            prefetch_us,
            "Prefetched ledger keys for Soroban parallel phase"
        );
    }

    // Run pre-parallel-apply for ALL Soroban TXs on a shared executor.
    // This matches stellar-core's `preParallelApplyAndCollectModifiedClassicEntries`
    // which runs validation + sequence bump + signer removal for every Soroban TX
    // BEFORE any cluster starts. Classic mutations (bumped seqs, removed signers)
    // are transferred to the main delta so PriorStageState picks them up.
    let mut pre_parallel_results = pre_parallel_apply(
        snapshot,
        phase,
        context,
        &SorobanContext {
            config: soroban.config.clone(),
            base_prng_seed: soroban_base_prng_seed,
            classic_events: soroban.classic_events,
            module_cache: soroban.module_cache,
            hot_archive: soroban.hot_archive.clone(),
            runtime_handle: soroban.runtime_handle.clone(),
            soroban_state: soroban.soroban_state.clone(),
            offer_store: None,
            emit_soroban_tx_meta_ext_v1: soroban.emit_soroban_tx_meta_ext_v1,
            enable_soroban_diagnostic_events: soroban.enable_soroban_diagnostic_events,
        },
        delta,
        &PreParallelParams {
            pre_charged_fees: &pre_charged_fees,
            classic_tx_count,
            soroban_base_prng_seed,
        },
    )?;

    // Track position in the flattened pre_charged_fees vector.
    let mut pre_charge_offset: usize = 0;

    let mut total_prior_stage_us: u64 = 0;
    let mut total_stage_clusters_us: u64 = 0;
    let mut total_result_merge_us: u64 = 0;

    for (stage_idx, stage) in phase.stages.iter().enumerate() {
        if stage.is_empty() {
            continue;
        }

        // Capture live entries and deletions from the delta so clusters in
        // this stage can see changes from prior stages AND classic TX changes.
        // In stellar-core, GlobalParallelApplyLedgerState wraps the main
        // LedgerTxn which already has classic TX changes committed, so even
        // stage 0 clusters see classic modifications (e.g., fee deductions on
        // shared accounts).  PriorStageState::from_delta bundles both
        // current_entries() and dead_entries() to match this behavior.
        let prior_stage_start = std::time::Instant::now();
        let prior_stage = PriorStageState::from_delta(delta, accumulated_restored_keys.clone());
        total_prior_stage_us += prior_stage_start.elapsed().as_micros() as u64;

        // Slice pre_charged_fees for this stage's clusters.
        let stage_tx_count: usize = stage.iter().map(|c| c.len()).sum();
        let stage_pre_charged =
            &pre_charged_fees[pre_charge_offset..pre_charge_offset + stage_tx_count];

        // Drain pre_parallel results for this stage (moves ownership).
        let stage_pre_parallel: Vec<PreParallelResult> =
            pre_parallel_results.drain(..stage_tx_count).collect();

        // Execute each cluster with its own executor, then merge results.
        // Clusters within a stage are independent (no footprint conflicts)
        // so they can be executed with isolated state.
        let stage_clusters_start = std::time::Instant::now();
        let cluster_results = execute_stage_clusters(
            snapshot,
            StageInput {
                clusters: stage,
                global_tx_offset,
                pre_parallel: stage_pre_parallel,
            },
            context,
            &SorobanContext {
                config: soroban.config.clone(),
                base_prng_seed: soroban_base_prng_seed,
                classic_events: soroban.classic_events,
                module_cache: soroban.module_cache,
                hot_archive: soroban.hot_archive.clone(),
                runtime_handle: soroban.runtime_handle.clone(),
                soroban_state: soroban.soroban_state.clone(),
                offer_store: None, // Soroban clusters don't touch offers
                emit_soroban_tx_meta_ext_v1: soroban.emit_soroban_tx_meta_ext_v1,
                enable_soroban_diagnostic_events: soroban.enable_soroban_diagnostic_events,
            },
            delta,
            &ClusterParams {
                id_pool,
                prior_stage: &prior_stage,
                pre_charged_fees: stage_pre_charged,
            },
        )?;
        total_stage_clusters_us += stage_clusters_start.elapsed().as_micros() as u64;

        // Merge cluster results. Use max id_pool across clusters.
        let result_merge_start = std::time::Instant::now();
        for cr in &cluster_results {
            if cr.id_pool > id_pool {
                id_pool = cr.id_pool;
            }
            all_results.extend(cr.results.iter().cloned());
            all_tx_results.extend(cr.tx_results.iter().cloned());
            all_tx_result_metas.extend(cr.tx_result_metas.iter().cloned());
            all_hot_archive_restored_keys.extend(cr.hot_archive_restored_keys.iter().cloned());
        }
        // Merge this stage's restored keys into the cross-stage accumulator.
        for cr in &cluster_results {
            accumulated_restored_keys.extend(cr.hot_archive_restored_keys.iter().cloned());
        }
        total_result_merge_us += result_merge_start.elapsed().as_micros() as u64;

        // Advance global TX offset and pre_charge_offset for next stage.
        global_tx_offset += stage_tx_count;
        pre_charge_offset += stage_tx_count;

        tracing::debug!(
            ledger_seq = context.sequence,
            stage_idx = stage_idx,
            clusters = stage.len(),
            stage_tx_count = stage_tx_count,
            "Completed parallel stage"
        );
    }

    // Apply fee refunds after ALL transactions (matching stellar-core processPostTxSetApply).
    // Account entries are already in the main delta from cluster merges, so we modify
    // them directly rather than using a separate executor.
    let refund_start = std::time::Instant::now();
    let flat_txs: Vec<&TransactionEnvelope> = phase
        .stages
        .iter()
        .flat_map(|s| s.iter())
        .flat_map(|c| c.iter())
        .map(|(tx, _)| tx.as_ref())
        .collect();

    // Apply Soroban fee refunds: stellar-core processPostTxSetApply() calls
    // processRefund() which applies refund to both the account balance
    // (via LedgerTxn) and the fee pool (feePool -= refund).
    // Only adjust fee pool if the refund was actually credited.
    // Also capture STATE/UPDATED changes for post_tx_apply_fee_processing meta.
    use stellar_xdr::curr::{LedgerEntryChange, LedgerEntryChanges};
    let mut total_refunds = 0i64;
    for (idx, result) in all_results.iter().enumerate() {
        let refund = result.fee_refund;
        if refund > 0 && idx < flat_txs.len() {
            let source = fee_source_account_id(flat_txs[idx]);
            let account_key = LedgerKey::Account(stellar_xdr::curr::LedgerKeyAccount {
                account_id: source.clone(),
            });
            let pre_entry = delta.get_current_entry(&account_key);
            if delta.apply_refund_to_account(&source, refund)? {
                total_refunds += refund;
                // Build STATE/UPDATED meta for this refund
                if let Some(pre) = pre_entry {
                    if let Some(post) = delta.get_current_entry(&account_key) {
                        let changes: LedgerEntryChanges = vec![
                            LedgerEntryChange::State(pre),
                            LedgerEntryChange::Updated(post),
                        ]
                        .try_into()
                        .unwrap_or_default();
                        if idx < all_tx_result_metas.len() {
                            all_tx_result_metas[idx].post_tx_apply_fee_processing = changes;
                        }
                    }
                }
            }
        }
    }
    if total_refunds > 0 {
        delta.record_fee_pool_delta(-total_refunds);
    }
    let refund_us = refund_start.elapsed().as_micros() as u64;

    tracing::debug!(
        ledger_seq = context.sequence,
        stages = phase.stages.len(),
        prefetch_us,
        prior_stage_us = total_prior_stage_us,
        stage_clusters_us = total_stage_clusters_us,
        result_merge_us = total_result_merge_us,
        refund_us,
        "PROFILE soroban_phase"
    );

    Ok(TxSetResult {
        results: all_results,
        tx_results: all_tx_results,
        tx_result_metas: all_tx_result_metas,
        id_pool,
        hot_archive_restored_keys: all_hot_archive_restored_keys,
    })
}

/// Pre-deduct ALL fees (classic + Soroban) on the delta in a single pass.
///
/// This matches stellar-core's `processFeesSeqNums()` which processes fees for
/// ALL transactions across both phases before any transaction body executes.
/// The processing order is: classic phase first, then Soroban phase (matching
/// stellar-core's phase iteration order).
///
/// Returns `(classic_pre_charged, soroban_pre_charged, total_fee_pool)`.
pub(crate) fn pre_deduct_all_fees_on_delta(
    classic_txs: &[TxWithFee],
    soroban_phase: &crate::close::SorobanPhaseStructure,
    base_fee: u32,
    network_id: NetworkId,
    ledger_seq: u32,
    delta: &mut LedgerDelta,
    snapshot: &SnapshotHandle,
) -> Result<(Vec<PreChargedFee>, Vec<PreChargedFee>, i64)> {
    // Batch-prefetch all fee source accounts before processing fees.
    // Without this, each deduct_fee_from_account call falls through to an individual
    // bucket list lookup. Prefetching once populates the snapshot cache so all
    // subsequent lookups are O(1) cache hits.
    {
        let mut fee_keys: Vec<LedgerKey> = Vec::with_capacity(
            classic_txs.len()
                + soroban_phase
                    .stages
                    .iter()
                    .map(|s| s.iter().map(|c| c.len()).sum::<usize>())
                    .sum::<usize>(),
        );
        for (tx, _) in classic_txs {
            fee_keys.push(LedgerKey::Account(stellar_xdr::curr::LedgerKeyAccount {
                account_id: fee_source_account_id(tx),
            }));
        }
        for stage in &soroban_phase.stages {
            for cluster in stage {
                for (tx, _) in cluster {
                    fee_keys.push(LedgerKey::Account(stellar_xdr::curr::LedgerKeyAccount {
                        account_id: fee_source_account_id(tx),
                    }));
                }
            }
        }
        snapshot.prefetch(&fee_keys)?;
    }

    let mut total_fee_pool = 0i64;

    // Phase 0: Classic fees (in apply order)
    let mut classic_pre_charged = Vec::with_capacity(classic_txs.len());
    for (tx, tx_base_fee) in classic_txs {
        let tx_fee = tx_base_fee.unwrap_or(base_fee);
        let frame = TransactionFrame::with_network(Arc::clone(tx), network_id);
        let fee_source = fee_source_account_id(tx);

        let computed_fee = frame.fee_to_charge(tx_fee as i64);

        let (charged_fee, fee_changes) =
            delta.deduct_fee_from_account(&fee_source, computed_fee, snapshot, ledger_seq)?;
        total_fee_pool = total_fee_pool
            .checked_add(charged_fee)
            .expect("total_fee_pool overflow");
        classic_pre_charged.push(PreChargedFee {
            charged_fee,
            fee_changes,
        });
    }

    // Phase 1: Soroban fees (in stage/cluster/tx order)
    let mut soroban_pre_charged = Vec::new();
    for stage in &soroban_phase.stages {
        for cluster in stage {
            for (tx, tx_base_fee) in cluster {
                let tx_fee = tx_base_fee.unwrap_or(base_fee);
                let frame = TransactionFrame::with_network(Arc::clone(tx), network_id);
                let fee_source = fee_source_account_id(tx);

                let computed_fee = frame.fee_to_charge(tx_fee as i64);

                let (charged_fee, fee_changes) = delta.deduct_fee_from_account(
                    &fee_source,
                    computed_fee,
                    snapshot,
                    ledger_seq,
                )?;
                total_fee_pool = total_fee_pool
                    .checked_add(charged_fee)
                    .expect("total_fee_pool overflow");
                soroban_pre_charged.push(PreChargedFee {
                    charged_fee,
                    fee_changes,
                });
            }
        }
    }

    Ok((classic_pre_charged, soroban_pre_charged, total_fee_pool))
}

/// Extract the read-write footprint keys from a Soroban transaction envelope.
fn soroban_write_footprint(tx: &TransactionEnvelope) -> Option<Vec<LedgerKey>> {
    let data = match tx {
        TransactionEnvelope::Tx(env) => match &env.tx.ext {
            stellar_xdr::curr::TransactionExt::V1(data) => Some(data),
            _ => None,
        },
        TransactionEnvelope::TxFeeBump(env) => match &env.tx.inner_tx {
            stellar_xdr::curr::FeeBumpTransactionInnerTx::Tx(inner) => match &inner.tx.ext {
                stellar_xdr::curr::TransactionExt::V1(data) => Some(data),
                _ => None,
            },
        },
        _ => None,
    };
    data.map(|d| d.resources.footprint.read_write.to_vec())
}

/// Execute a single cluster of transactions independently.
///
/// Creates its own `TransactionExecutor` and `LedgerDelta`.
/// Fees are NOT deducted by the executor (FeeMode::Skip) because they
/// were pre-deducted from the main delta by `pre_deduct_soroban_fees`.
/// Returns `(TxSetResult, per_cluster_delta, total_fees)`.
pub(super) fn execute_single_cluster(
    snapshot: &SnapshotHandle,
    cluster: &[TxWithFee],
    cluster_offset: usize,
    context: &LedgerContext,
    soroban: &SorobanContext<'_>,
    params: &ClusterParams<'_>,
    pre_parallel: Option<Vec<PreParallelResult>>,
) -> Result<(TxSetResult, LedgerDelta, i64)> {
    let cluster_start = std::time::Instant::now();

    let executor_setup_start = std::time::Instant::now();
    let mut executor = TransactionExecutor::new(
        context,
        params.id_pool,
        soroban.config.clone(),
        soroban.classic_events,
    );
    executor.set_meta_flags(
        soroban.emit_soroban_tx_meta_ext_v1,
        soroban.enable_soroban_diagnostic_events,
    );
    if let Some(cache) = soroban.module_cache {
        executor.set_module_cache(cache.clone());
    }
    if let Some(ref ha) = soroban.hot_archive {
        executor.set_hot_archive(ha.clone());
    }
    if let Some(ref ss) = soroban.soroban_state {
        executor.set_soroban_state(ss.clone());
    }
    let executor_setup_us = executor_setup_start.elapsed().as_micros() as u64;

    // Pre-load entries from prior stages so this cluster's executor sees
    // Pre-load entries and deletions from prior stages so this cluster's
    // executor sees restorations, modifications, and deletions made by
    // earlier stages.  This matches stellar-core
    // `collectClusterFootprintEntriesFromGlobal`.
    let prior_load_start = std::time::Instant::now();
    for entry in &params.prior_stage.entries {
        executor.state.load_entry(entry.clone());
    }
    for key in &params.prior_stage.deleted_keys {
        executor.state.mark_entry_deleted(key);
    }
    // Seed hot-archive restored keys from prior stages so this cluster
    // won't re-restore entries that were already restored and then deleted.
    executor.seed_hot_archive_restored_keys(&params.prior_stage.hot_archive_restored_keys);
    let prior_load_us = prior_load_start.elapsed().as_micros() as u64;

    let mut results = Vec::with_capacity(cluster.len());
    let mut tx_results = Vec::with_capacity(cluster.len());
    let mut tx_result_metas = Vec::with_capacity(cluster.len());

    tracing::debug!(
        ledger_seq = context.sequence,
        cluster_offset,
        cluster_len = cluster.len(),
        "Starting cluster execution"
    );

    let mut agg_flush_ttl_us: u64 = 0;
    let mut agg_snapshot_delta_us: u64 = 0;
    let mut agg_execute_us: u64 = 0;
    let mut agg_result_build_us: u64 = 0;
    let mut agg_validation_us: u64 = 0;
    let mut agg_fee_seq_us: u64 = 0;
    let mut agg_footprint_us: u64 = 0;
    let mut agg_ops_us: u64 = 0;
    let mut agg_meta_us: u64 = 0;
    let mut agg_val_account_load_us: u64 = 0;
    let mut agg_val_tx_hash_us: u64 = 0;
    let mut agg_val_ed25519_us: u64 = 0;
    let mut agg_val_other_us: u64 = 0;
    let mut agg_meta_commit_us: u64 = 0;
    let mut agg_meta_fee_refund_us: u64 = 0;
    let mut agg_meta_build_phase_us: u64 = 0;

    // Consume pre-parallel results in order (if provided).
    let mut pre_parallel_iter = pre_parallel.map(|v| v.into_iter());

    for (local_idx, (tx, tx_base_fee)) in cluster.iter().enumerate() {
        // Flush pending RO TTL bumps for keys in this TX's write footprint.
        // This matches stellar-core's flushRoTTLBumpsInTxWriteFootprint:
        // bumped TTL values from earlier TXs' read-only bumps must be visible
        // to write TXs for correct rent fee calculation. Must happen BEFORE
        // snapshot_delta so flushed values are not rolled back on TX failure.
        let flush_start = std::time::Instant::now();
        if let Some(write_keys) = soroban_write_footprint(tx) {
            executor
                .state_mut()
                .flush_ro_ttl_bumps_for_write_footprint(&write_keys);
        }
        agg_flush_ttl_us += flush_start.elapsed().as_micros() as u64;

        let snap_start = std::time::Instant::now();
        executor.state.snapshot_delta();
        agg_snapshot_delta_us += snap_start.elapsed().as_micros() as u64;

        let tx_fee = tx_base_fee.unwrap_or(context.base_fee);

        // Execute TX: use pre-parallel result if available (global preParallelApply
        // already ran validation + seq bump + signer removal), otherwise fall back
        // to the full execute_transaction_with_arc path.
        let pre = &params.pre_charged_fees[local_idx];
        let exec_start = std::time::Instant::now();
        let mut result = if let Some(ref mut iter) = pre_parallel_iter {
            let pp = iter.next().expect("pre_parallel_results length mismatch");
            match pp {
                PreParallelResult::Success(pre_apply) => {
                    // Pre-apply succeeded globally; run only the operation body.
                    executor.execute_with_pre_apply_result(snapshot, pre_apply)?
                }
                PreParallelResult::Failed(early_result) => {
                    // Pre-apply failed globally (validation error); use result directly.
                    early_result
                }
            }
        } else {
            // Legacy path: no pre-parallel apply (shouldn't happen for Soroban
            // parallel phase, but kept for safety).
            let tx_fee = tx_base_fee.unwrap_or(context.base_fee);
            let global_idx = cluster_offset + local_idx;
            let tx_prng_seed = sub_sha256(&soroban.base_prng_seed, global_idx as u32);
            executor.execute_transaction_with_arc(
                snapshot,
                TransactionExecutionRequest {
                    tx_envelope: Arc::clone(tx),
                    base_fee: tx_fee,
                    soroban_prng_seed: Some(tx_prng_seed),
                    fee_mode: FeeMode::Skip,
                },
            )?
        };
        agg_execute_us += exec_start.elapsed().as_micros() as u64;
        agg_validation_us += result.timings.validation_us;
        agg_fee_seq_us += result.timings.fee_seq_us;
        agg_footprint_us += result.timings.footprint_us;
        agg_ops_us += result.timings.ops_us;
        agg_meta_us += result.timings.meta_build_us;
        agg_val_account_load_us += result.timings.val_account_load_us;
        agg_val_tx_hash_us += result.timings.val_tx_hash_us;
        agg_val_ed25519_us += result.timings.val_ed25519_us;
        agg_val_other_us += result.timings.val_other_us;
        agg_meta_commit_us += result.timings.meta_commit_us;
        agg_meta_fee_refund_us += result.timings.meta_fee_refund_us;
        agg_meta_build_phase_us += result.timings.meta_build_phase_us;

        // Override fee_charged and fee_changes from pre-deduction.
        // The executor computed fee_refund correctly (based on resource consumption),
        // but fee_charged=0 because FeeMode::Skip. We use the pre-charged values.
        let result_build_start = std::time::Instant::now();
        result.fee_charged = pre.charged_fee.saturating_sub(result.fee_refund);
        result.fee_changes = Some(pre.fee_changes.clone());

        let frame = TransactionFrame::with_network(Arc::clone(tx), executor.network_id);
        let tx_result = build_tx_result_pair(
            &frame,
            &executor.network_id,
            &result,
            tx_fee as i64,
            context.protocol_version,
        )?;
        let tx_meta = result.tx_meta.take().unwrap_or_else(empty_transaction_meta);
        let fee_changes = result
            .fee_changes
            .take()
            .unwrap_or_else(empty_entry_changes);
        let post_fee_changes = result
            .post_fee_changes
            .take()
            .unwrap_or_else(empty_entry_changes);
        let tx_result_meta = TransactionResultMetaV1 {
            ext: ExtensionPoint::V0,
            result: tx_result.clone(),
            fee_processing: fee_changes,
            tx_apply_processing: tx_meta,
            post_tx_apply_fee_processing: post_fee_changes,
        };

        // Record hot-archive restored keys from successful TXs so subsequent
        // TXs in this cluster see them and won't re-restore entries that were
        // restored and then deleted. Only successful TXs contribute.
        if result.success {
            executor.record_hot_archive_restores(&result.hot_archive_restored_keys);
        }

        results.push(result);
        tx_results.push(tx_result);
        tx_result_metas.push(tx_result_meta);
        agg_result_build_us += result_build_start.elapsed().as_micros() as u64;
    }

    // Flush deferred RO TTL bumps within this cluster.
    let flush_final_start = std::time::Instant::now();
    executor.state_mut().flush_deferred_ro_ttl_bumps();
    let flush_final_us = flush_final_start.elapsed().as_micros() as u64;

    // Collect hot archive restored keys from SUCCESSFUL transactions only.
    // Failed transactions' hot archive keys must not propagate as they would
    // produce spurious HOT_ARCHIVE_LIVE tombstones (VE-06 class bug).
    let mut restored_keys: Vec<LedgerKey> = Vec::new();
    for r in &results {
        if r.success {
            restored_keys.extend(r.hot_archive_restored_keys.iter().cloned());
        }
    }

    let total_fees = executor.total_fees();
    let final_id_pool = executor.id_pool();

    // Apply this cluster's state changes to a local delta.
    let apply_delta_start = std::time::Instant::now();
    let mut cluster_delta = LedgerDelta::new(context.sequence);
    executor.apply_to_delta(&mut cluster_delta)?;
    let apply_delta_us = apply_delta_start.elapsed().as_micros() as u64;

    let cluster_total_us = cluster_start.elapsed().as_micros() as u64;
    tracing::debug!(
        ledger_seq = context.sequence,
        cluster_offset,
        tx_count = cluster.len(),
        cluster_total_us,
        executor_setup_us,
        prior_load_us,
        flush_ttl_us = agg_flush_ttl_us,
        snapshot_delta_us = agg_snapshot_delta_us,
        execute_us = agg_execute_us,
        validation_us = agg_validation_us,
        fee_seq_us = agg_fee_seq_us,
        footprint_us = agg_footprint_us,
        ops_us = agg_ops_us,
        meta_us = agg_meta_us,
        val_acct_load_us = agg_val_account_load_us,
        val_hash_us = agg_val_tx_hash_us,
        val_ed25519_us = agg_val_ed25519_us,
        val_other_us = agg_val_other_us,
        meta_commit_us = agg_meta_commit_us,
        meta_fee_refund_us = agg_meta_fee_refund_us,
        meta_build_phase_us = agg_meta_build_phase_us,
        result_build_us = agg_result_build_us,
        flush_final_us,
        apply_delta_us,
        "PROFILE cluster"
    );

    Ok((
        TxSetResult {
            results,
            tx_results,
            tx_result_metas,
            id_pool: final_id_pool,
            hot_archive_restored_keys: restored_keys,
        },
        cluster_delta,
        total_fees,
    ))
}

/// Execute all clusters within a stage.
///
/// Each cluster gets its own `TransactionExecutor` with isolated state.
/// The executor's state changes are applied to the main delta via `apply_to_delta`.
/// Soroban TXs don't use the orderbook so `load_orderbook_offers` is skipped.
///
/// Input for a single stage of parallel cluster execution.
pub(super) struct StageInput<'a> {
    pub clusters: &'a [Vec<TxWithFee>],
    pub global_tx_offset: usize,
    pub pre_parallel: Vec<PreParallelResult>,
}

/// When a stage has multiple clusters, they are executed in parallel using
/// `tokio::task::spawn_blocking` (one blocking task per cluster). Results are
/// merged into `delta` in deterministic cluster order.
pub(super) fn execute_stage_clusters(
    snapshot: &SnapshotHandle,
    stage: StageInput<'_>,
    context: &LedgerContext,
    soroban: &SorobanContext<'_>,
    delta: &mut LedgerDelta,
    params: &ClusterParams<'_>,
) -> Result<Vec<TxSetResult>> {
    let clusters = stage.clusters;
    let global_tx_offset = stage.global_tx_offset;
    let stage_pre_parallel = stage.pre_parallel;
    // Compute per-cluster global offsets and pre_charged_fees slicing.
    let mut offsets = Vec::with_capacity(clusters.len());
    let mut pre_charge_offsets = Vec::with_capacity(clusters.len());
    let mut offset = global_tx_offset;
    let mut pc_offset = 0usize;
    for cluster in clusters {
        offsets.push(offset);
        pre_charge_offsets.push(pc_offset);
        offset += cluster.len();
        pc_offset += cluster.len();
    }

    // Split pre_parallel results by cluster (consuming the Vec).
    let mut per_cluster_pre_parallel: Vec<Vec<PreParallelResult>> =
        Vec::with_capacity(clusters.len());
    {
        let mut remaining = stage_pre_parallel;
        for cluster in clusters {
            let rest = remaining.split_off(cluster.len());
            per_cluster_pre_parallel.push(remaining);
            remaining = rest;
        }
        debug_assert!(remaining.is_empty());
    }

    tracing::debug!(
        ledger_seq = context.sequence,
        num_clusters = clusters.len(),
        prior_entries = params.prior_stage.entries.len(),
        "execute_stage_clusters: starting"
    );

    // Single-cluster fast path: execute inline, no thread overhead.
    if clusters.len() <= 1 {
        let mut cluster_results = Vec::with_capacity(clusters.len());
        let mut pp_iter = per_cluster_pre_parallel.into_iter();
        for (cluster_idx, cluster) in clusters.iter().enumerate() {
            let cluster_pc = &params.pre_charged_fees
                [pre_charge_offsets[cluster_idx]..pre_charge_offsets[cluster_idx] + cluster.len()];
            let cluster_pp = pp_iter.next().unwrap_or_default();
            let (cr, cluster_delta, total_fees) = execute_single_cluster(
                snapshot,
                cluster,
                offsets[cluster_idx],
                context,
                soroban,
                &ClusterParams {
                    id_pool: params.id_pool,
                    prior_stage: params.prior_stage,
                    pre_charged_fees: cluster_pc,
                },
                Some(cluster_pp),
            )?;
            delta.merge(cluster_delta)?;
            if total_fees != 0 {
                delta.record_fee_pool_delta(total_fees);
            }
            cluster_results.push(cr);
        }
        return Ok(cluster_results);
    }

    // Multi-cluster: spawn one blocking task per cluster on Tokio's thread pool.
    // Clone shared data for 'static closures (all clones are cheap / Arc-based).
    let clone_start = std::time::Instant::now();
    let snapshot = snapshot.clone();
    let context = context.clone();
    let soroban_config = soroban.config.clone();
    let soroban_base_prng_seed = soroban.base_prng_seed;
    let classic_events = soroban.classic_events;
    let module_cache = soroban.module_cache.cloned();
    let hot_archive = soroban.hot_archive.clone();
    let soroban_state_arc = soroban.soroban_state.clone();
    let runtime_handle = soroban.runtime_handle.clone();
    let emit_soroban_tx_meta_ext_v1 = soroban.emit_soroban_tx_meta_ext_v1;
    let enable_soroban_diagnostic_events = soroban.enable_soroban_diagnostic_events;
    let id_pool = params.id_pool;
    let clusters: std::sync::Arc<Vec<Vec<TxWithFee>>> = std::sync::Arc::new(clusters.to_vec());
    let prior_stage: std::sync::Arc<PriorStageState> = std::sync::Arc::new(PriorStageState {
        entries: params.prior_stage.entries.clone(),
        deleted_keys: params.prior_stage.deleted_keys.clone(),
        hot_archive_restored_keys: params.prior_stage.hot_archive_restored_keys.clone(),
    });
    // For multi-cluster parallel execution, we need to split pre_charged_fees per cluster.
    // Each cluster gets its own Vec since the spawn_blocking closure needs 'static data.
    let per_cluster_fees: Vec<std::sync::Arc<Vec<PreChargedFee>>> = {
        let mut result = Vec::with_capacity(clusters.len());
        for (idx, cluster) in clusters.iter().enumerate() {
            let start = pre_charge_offsets[idx];
            let end = start + cluster.len();
            let fees: Vec<PreChargedFee> = params.pre_charged_fees[start..end]
                .iter()
                .map(|f| PreChargedFee {
                    charged_fee: f.charged_fee,
                    fee_changes: f.fee_changes.clone(),
                })
                .collect();
            result.push(std::sync::Arc::new(fees));
        }
        result
    };
    let clone_setup_us = clone_start.elapsed().as_micros() as u64;

    // Wrap per-cluster pre-parallel results for moving into spawn_blocking closures.
    // Each closure takes ownership of its cluster's Vec.
    let per_cluster_pp: Vec<Option<Vec<PreParallelResult>>> =
        per_cluster_pre_parallel.into_iter().map(Some).collect();
    let per_cluster_pp = std::sync::Arc::new(parking_lot::Mutex::new(per_cluster_pp));

    // Build the async future that spawns per-cluster tasks and collects results.
    let spawn_start = std::time::Instant::now();
    let spawn_and_collect = async {
        let mut tasks = Vec::with_capacity(clusters.len());
        for idx in 0..clusters.len() {
            let snapshot = snapshot.clone();
            let context = context.clone();
            let config = soroban_config.clone();
            let cache = module_cache.clone();
            let ha = hot_archive.clone();
            let ss = soroban_state_arc.clone();
            let clusters = clusters.clone();
            let prior_stage = prior_stage.clone();
            let cluster_offset = offsets[idx];
            let cluster_fees = per_cluster_fees[idx].clone();
            let cluster_pp = per_cluster_pp.lock()[idx].take().unwrap_or_default();

            tasks.push(tokio::task::spawn_blocking(move || {
                let soroban = SorobanContext {
                    config: config,
                    base_prng_seed: soroban_base_prng_seed,
                    classic_events,
                    module_cache: cache.as_ref(),
                    hot_archive: ha,
                    soroban_state: ss,
                    offer_store: None, // Soroban clusters don't touch offers
                    runtime_handle: None,
                    emit_soroban_tx_meta_ext_v1,
                    enable_soroban_diagnostic_events,
                };
                let cluster_wall_start = std::time::Instant::now();
                let result = execute_single_cluster(
                    &snapshot,
                    &clusters[idx],
                    cluster_offset,
                    &context,
                    &soroban,
                    &ClusterParams {
                        id_pool,
                        prior_stage: &prior_stage,
                        pre_charged_fees: &cluster_fees,
                    },
                    Some(cluster_pp),
                );
                let wall_us = cluster_wall_start.elapsed().as_micros() as u64;
                let tx_count = clusters[idx].len();
                (result, wall_us, tx_count)
            }));
        }

        // Collect all results (preserving cluster order).
        let mut results = Vec::with_capacity(tasks.len());
        for task in tasks {
            results.push(task.await.expect("cluster task panicked"));
        }
        results
    };

    // When called from a spawn_blocking thread (runtime_handle is Some),
    // use Handle::block_on directly. When called from a tokio worker thread
    // (runtime_handle is None), use block_in_place to safely enter a blocking
    // context before calling block_on.
    let thread_results: Vec<ClusterThreadResult> = if let Some(handle) = runtime_handle {
        handle.block_on(spawn_and_collect)
    } else {
        tokio::task::block_in_place(|| {
            tokio::runtime::Handle::current().block_on(spawn_and_collect)
        })
    };
    let spawn_collect_us = spawn_start.elapsed().as_micros() as u64;

    // Merge results in cluster order (deterministic).
    let merge_start = std::time::Instant::now();
    let mut cluster_results = Vec::with_capacity(thread_results.len());
    let mut cluster_wall_times: Vec<(u64, usize)> = Vec::with_capacity(thread_results.len());
    for (result, wall_us, tx_count) in thread_results {
        let (cr, cluster_delta, total_fees) = result?;
        delta.merge(cluster_delta)?;
        if total_fees != 0 {
            delta.record_fee_pool_delta(total_fees);
        }
        cluster_results.push(cr);
        cluster_wall_times.push((wall_us, tx_count));
    }
    let delta_merge_us = merge_start.elapsed().as_micros() as u64;

    // Log per-cluster wall times for straggler analysis.
    let wall_times_us: Vec<u64> = cluster_wall_times.iter().map(|(w, _)| *w).collect();
    let min_us = wall_times_us.iter().copied().min().unwrap_or(0);
    let max_us = wall_times_us.iter().copied().max().unwrap_or(0);
    let avg_us = if wall_times_us.is_empty() {
        0
    } else {
        wall_times_us.iter().sum::<u64>() / wall_times_us.len() as u64
    };
    let per_cluster_str: String = cluster_wall_times
        .iter()
        .enumerate()
        .map(|(i, (w, n))| format!("c{}:{:.1}ms/{}tx", i, *w as f64 / 1000.0, n))
        .collect::<Vec<_>>()
        .join(" ");

    tracing::debug!(
        ledger_seq = context.sequence,
        num_clusters = cluster_results.len(),
        clone_setup_us,
        spawn_collect_us,
        delta_merge_us,
        min_cluster_us = min_us,
        max_cluster_us = max_us,
        avg_cluster_us = avg_us,
        clusters = %per_cluster_str,
        "PROFILE stage_clusters"
    );

    Ok(cluster_results)
}

/// Compute the state size window update entry for a ledger close.
///
/// This implements the stellar-core `maybeSnapshotSorobanStateSize` logic, which updates the
/// `LiveSorobanStateSizeWindow` config setting on each sample period.
///
/// # Arguments
///
/// * `seq` - Current ledger sequence number
/// * `protocol_version` - Current protocol version
/// * `bucket_list` - Bucket list to read current window state from
/// * `soroban_state_size` - Total size of Soroban state in bytes (contracts + data)
///
/// # Returns
///
/// The updated window entry if a change is needed, or None if no update is required.
pub fn compute_state_size_window_entry(
    seq: u32,
    protocol_version: u32,
    bucket_list: &henyey_bucket::BucketList,
    soroban_state_size: u64,
    sample_period: u32,
    sample_size: u32,
) -> Option<LedgerEntry> {
    use henyey_common::protocol::MIN_SOROBAN_PROTOCOL_VERSION;
    use stellar_xdr::curr::{
        ConfigSettingEntry, ConfigSettingId, LedgerEntryData, LedgerEntryExt, LedgerKey,
        LedgerKeyConfigSetting, VecM,
    };

    if protocol_version < MIN_SOROBAN_PROTOCOL_VERSION {
        return None;
    }

    let sample_size = sample_size as usize;
    if sample_period == 0 || sample_size == 0 {
        return None;
    }

    // Load current window state
    let window_key = LedgerKey::ConfigSetting(LedgerKeyConfigSetting {
        config_setting_id: ConfigSettingId::LiveSorobanStateSizeWindow,
    });
    let window_entry = bucket_list.get(&window_key).ok()??;
    let LedgerEntryData::ConfigSetting(ConfigSettingEntry::LiveSorobanStateSizeWindow(window)) =
        window_entry.data
    else {
        return None;
    };

    let mut window_vec: Vec<u64> = window.into();
    if window_vec.is_empty() {
        return None;
    }

    // Check if window size needs to be adjusted
    let mut changed = false;
    if window_vec.len() != sample_size {
        if sample_size < window_vec.len() {
            let remove_count = window_vec.len() - sample_size;
            window_vec.drain(0..remove_count);
        } else {
            let oldest = window_vec[0];
            let insert_count = sample_size - window_vec.len();
            for _ in 0..insert_count {
                window_vec.insert(0, oldest);
            }
        }
        changed = true;
    }

    // Update window on sample ledgers
    if seq % sample_period == 0 && !window_vec.is_empty() {
        window_vec.remove(0);
        window_vec.push(soroban_state_size);
        changed = true;
    }

    if !changed {
        return None;
    }

    let window_vecm: VecM<u64> = window_vec.try_into().ok()?;

    Some(LedgerEntry {
        last_modified_ledger_seq: seq,
        data: LedgerEntryData::ConfigSetting(ConfigSettingEntry::LiveSorobanStateSizeWindow(
            window_vecm,
        )),
        ext: LedgerEntryExt::V0,
    })
}

/// Extract (buying, selling) asset pairs from DEX operations in the TX set.
///
/// Scans all TX operations for offer management and path payment operations
/// to determine which order books will be accessed during execution.
#[cfg(test)]
fn collect_dex_asset_pairs(transactions: &[TxWithFee]) -> HashSet<(Asset, Asset)> {
    let mut pairs = HashSet::new();
    for (tx, _) in transactions {
        let frame = TransactionFrame::new(Arc::clone(tx));
        for op in frame.operations() {
            match &op.body {
                OperationBody::ManageSellOffer(o) => {
                    pairs.insert((o.buying.clone(), o.selling.clone()));
                }
                OperationBody::CreatePassiveSellOffer(o) => {
                    pairs.insert((o.buying.clone(), o.selling.clone()));
                }
                OperationBody::ManageBuyOffer(o) => {
                    pairs.insert((o.buying.clone(), o.selling.clone()));
                }
                OperationBody::PathPaymentStrictReceive(o) => {
                    // Path: send_asset → path[0] → ... → dest_asset; crossed as (next, prev).
                    let mut chain: Vec<&Asset> = vec![&o.send_asset];
                    chain.extend(o.path.iter());
                    chain.push(&o.dest_asset);
                    for w in chain.windows(2) {
                        pairs.insert((w[1].clone(), w[0].clone()));
                    }
                }
                OperationBody::PathPaymentStrictSend(o) => {
                    let mut chain: Vec<&Asset> = vec![&o.send_asset];
                    chain.extend(o.path.iter());
                    chain.push(&o.dest_asset);
                    for w in chain.windows(2) {
                        pairs.insert((w[1].clone(), w[0].clone()));
                    }
                }
                _ => {}
            }
        }
    }
    pairs
}

#[cfg(test)]
mod tests {
    use super::*;
    use stellar_xdr::curr::{
        AccountId, AlphaNum4, Asset, AssetCode4, ManageSellOfferOp, Memo, MuxedAccount,
        PathPaymentStrictReceiveOp, Preconditions, PublicKey, SequenceNumber, Transaction,
        TransactionExt, TransactionV1Envelope, Uint256, VecM,
    };

    fn make_account_id(seed: u8) -> AccountId {
        let mut bytes = [0u8; 32];
        bytes[0] = seed;
        AccountId(PublicKey::PublicKeyTypeEd25519(Uint256(bytes)))
    }

    fn credit(code: &[u8; 4], issuer_seed: u8) -> Asset {
        Asset::CreditAlphanum4(AlphaNum4 {
            asset_code: AssetCode4(*code),
            issuer: make_account_id(issuer_seed),
        })
    }

    fn make_tx_with_ops(ops: Vec<Operation>) -> TxWithFee {
        let ops_vec: VecM<Operation, 100> = ops.try_into().unwrap();
        let tx = Transaction {
            source_account: MuxedAccount::Ed25519(Uint256([1u8; 32])),
            fee: 100,
            seq_num: SequenceNumber(1),
            cond: Preconditions::None,
            memo: Memo::None,
            operations: ops_vec,
            ext: TransactionExt::V0,
        };
        (
            Arc::new(TransactionEnvelope::Tx(TransactionV1Envelope {
                tx,
                signatures: VecM::default(),
            })),
            None,
        )
    }

    fn op(body: OperationBody) -> Operation {
        Operation {
            source_account: None,
            body,
        }
    }

    /// collect_dex_asset_pairs extracts (buying, selling) from manage sell/buy offer ops.
    #[test]
    fn test_collect_dex_asset_pairs_manage_offers() {
        let usd = credit(b"USD\0", 10);
        let eur = credit(b"EUR\0", 11);

        let tx = make_tx_with_ops(vec![op(OperationBody::ManageSellOffer(
            ManageSellOfferOp {
                selling: Asset::Native,
                buying: usd.clone(),
                amount: 100,
                price: stellar_xdr::curr::Price { n: 1, d: 1 },
                offer_id: 0,
            },
        ))]);

        let pairs = collect_dex_asset_pairs(&[tx]);
        assert_eq!(pairs.len(), 1);
        assert!(pairs.contains(&(usd.clone(), Asset::Native)));

        // ManageBuyOffer also captured
        let tx2 = make_tx_with_ops(vec![op(OperationBody::ManageBuyOffer(
            stellar_xdr::curr::ManageBuyOfferOp {
                selling: eur.clone(),
                buying: usd.clone(),
                buy_amount: 100,
                price: stellar_xdr::curr::Price { n: 2, d: 1 },
                offer_id: 0,
            },
        ))]);
        let pairs2 = collect_dex_asset_pairs(&[tx2]);
        assert!(pairs2.contains(&(usd.clone(), eur.clone())));
    }

    /// collect_dex_asset_pairs extracts all hop pairs from path payments.
    #[test]
    fn test_collect_dex_asset_pairs_path_payment() {
        let usd = credit(b"USD\0", 10);
        let eur = credit(b"EUR\0", 11);
        let btc = credit(b"BTC\0", 12);

        // PathPaymentStrictReceive: native → EUR → USD → BTC
        // Hops: (EUR, native), (USD, EUR), (BTC, USD)
        let path: VecM<Asset, 5> = vec![eur.clone(), usd.clone()].try_into().unwrap();
        let tx = make_tx_with_ops(vec![op(OperationBody::PathPaymentStrictReceive(
            PathPaymentStrictReceiveOp {
                send_asset: Asset::Native,
                send_max: 1000,
                destination: MuxedAccount::Ed25519(Uint256([2u8; 32])),
                dest_asset: btc.clone(),
                dest_amount: 500,
                path,
            },
        ))]);

        let pairs = collect_dex_asset_pairs(&[tx]);
        assert_eq!(pairs.len(), 3, "expected 3 hop pairs, got {:?}", pairs);
        assert!(pairs.contains(&(eur.clone(), Asset::Native)));
        assert!(pairs.contains(&(usd.clone(), eur.clone())));
        assert!(pairs.contains(&(btc.clone(), usd.clone())));
    }
}
