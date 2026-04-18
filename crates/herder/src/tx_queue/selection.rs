//! Transaction set selection from the pending queue.
//!
//! Builds candidate transaction sets for consensus by selecting the
//! highest-fee transactions that fit within ledger capacity, applying
//! surge pricing and lane-based limits.

use super::*;

type AccountTransactions = HashMap<Vec<u8>, Vec<QueuedTransaction>>;

fn seed_account_queue(
    queue: &mut SurgePricingPriorityQueue,
    accounts: &AccountTransactions,
    positions: &mut HashMap<Vec<u8>, usize>,
    network_id: &NetworkId,
    ledger_version: u32,
) {
    for (account, txs) in accounts {
        if let Some(first) = txs.first() {
            queue.add(first.clone(), network_id, ledger_version);
            positions.insert(account.clone(), 0);
        }
    }
}

fn push_next_account_tx(
    queue: &mut SurgePricingPriorityQueue,
    accounts: &AccountTransactions,
    positions: &mut HashMap<Vec<u8>, usize>,
    account: &[u8],
    network_id: &NetworkId,
    ledger_version: u32,
) {
    let Some(txs) = accounts.get(account) else {
        return;
    };

    let next_index = positions
        .get(account)
        .copied()
        .unwrap_or(0)
        .saturating_add(1);
    if next_index < txs.len() {
        positions.insert(account.to_vec(), next_index);
        queue.add(txs[next_index].clone(), network_id, ledger_version);
    }
}

impl TransactionQueue {
    /// Get a transaction set for the next ledger.
    ///
    /// Returns the highest-fee transactions up to the specified limit.
    pub fn get_transaction_set(
        &self,
        previous_ledger_hash: Hash256,
        max_ops: usize,
    ) -> TransactionSet {
        let SelectedTxs { transactions, .. } =
            self.select_transactions_with_starting_seq(max_ops, None);
        TransactionSet::new(previous_ledger_hash, transactions)
    }

    pub fn get_transaction_set_with_starting_seq(
        &self,
        previous_ledger_hash: Hash256,
        max_ops: usize,
        starting_seq: Option<&HashMap<Vec<u8>, i64>>,
    ) -> TransactionSet {
        let SelectedTxs { transactions, .. } =
            self.select_transactions_with_starting_seq(max_ops, starting_seq);
        TransactionSet::new(previous_ledger_hash, transactions)
    }

    /// Build a GeneralizedTransactionSet (protocol 20+) and return it with the correct hash.
    ///
    /// The hash is SHA-256 of the XDR-encoded GeneralizedTransactionSet.
    pub fn build_generalized_tx_set(
        &self,
        previous_ledger_hash: Hash256,
        max_ops: usize,
    ) -> (TransactionSet, stellar_xdr::curr::GeneralizedTransactionSet) {
        self.build_generalized_tx_set_with_starting_seq(previous_ledger_hash, max_ops, None, 0)
    }

    pub fn build_generalized_tx_set_with_starting_seq(
        &self,
        previous_ledger_hash: Hash256,
        max_ops: usize,
        starting_seq: Option<&HashMap<Vec<u8>, i64>>,
        close_time_offset: u64,
    ) -> (TransactionSet, stellar_xdr::curr::GeneralizedTransactionSet) {
        self.build_generalized_tx_set_with_providers(
            previous_ledger_hash,
            max_ops,
            starting_seq,
            close_time_offset,
            None,
            None,
        )
    }

    /// Build a GeneralizedTransactionSet with caller-supplied snapshot providers.
    ///
    /// When `override_fee_provider` / `override_account_provider` are `Some`,
    /// they are used for the `trim_invalid_two_phase` pass instead of the
    /// queue's stored per-call providers. This avoids creating O(N) snapshots
    /// on batch paths (nomination, post-close re-validation).
    pub fn build_generalized_tx_set_with_providers(
        &self,
        previous_ledger_hash: Hash256,
        max_ops: usize,
        starting_seq: Option<&HashMap<Vec<u8>, i64>>,
        close_time_offset: u64,
        override_fee_provider: Option<&dyn FeeBalanceProvider>,
        override_account_provider: Option<&dyn AccountProvider>,
    ) -> (TransactionSet, stellar_xdr::curr::GeneralizedTransactionSet) {
        use stellar_xdr::curr::{GeneralizedTransactionSet, WriteXdr};

        let SelectedTxs {
            transactions,
            soroban_limited,
            dex_limited,
            classic_limited,
        } = self.select_transactions_with_starting_seq(max_ops, starting_seq);
        let base_fee = self.validation_context.read().base_fee as i64;
        let mut classic_txs = Vec::new();
        let mut soroban_txs = Vec::new();
        for tx in &transactions {
            let frame = henyey_tx::TransactionFrame::from_owned_with_network(
                tx.clone(),
                self.config.network_id,
            );
            if frame.is_soroban() {
                soroban_txs.push(tx.clone());
            } else {
                classic_txs.push(tx.clone());
            }
        }

        // Parity: stellar-core `makeTxSetFromTransactions` calls `trimInvalid` per-phase
        // with a shared `accountFeeMap` before surge pricing. For V26+, the fee map is
        // shared across Classic and Soroban phases so that a fee source appearing in both
        // phases has its total fees summed correctly.
        // (TxSetFrame.cpp:836-860)
        //
        // We only run this when a fee balance provider is available. Txs are already
        // individually validated at queue admission; the cross-phase trim catches the case
        // where cumulative fees across both phases exceed a source's balance.
        //
        // When override providers are supplied (batch paths like nomination), use them
        // directly — they wrap a single pre-built snapshot. Otherwise fall back to the
        // queue's stored per-call providers (admission path).
        let queue_fee = if override_fee_provider.is_none() {
            self.get_fee_balance_provider()
        } else {
            None
        };
        let queue_account = if override_account_provider.is_none() {
            self.get_account_provider()
        } else {
            None
        };
        let fee_ref: Option<&dyn FeeBalanceProvider> =
            override_fee_provider.or(queue_fee.as_deref());
        let account_ref: Option<&dyn AccountProvider> =
            override_account_provider.or(queue_account.as_deref());

        let (classic_txs, mut soroban_txs) = if fee_ref.is_some() || account_ref.is_some() {
            let ctx = {
                let vc = self.validation_context.read();
                crate::tx_set_utils::TxSetValidationContext {
                    next_ledger_seq: vc.ledger_seq + 1,
                    close_time: vc.close_time,
                    base_fee: vc.base_fee,
                    base_reserve: vc.base_reserve,
                    protocol_version: vc.protocol_version,
                    network_id: self.config.network_id,
                    ledger_flags: vc.ledger_flags,
                    max_contract_size_bytes: vc.max_contract_size_bytes,
                }
            };
            let close_time_bounds = crate::tx_set_utils::CloseTimeBounds::with_offsets(
                close_time_offset,
                close_time_offset,
            );
            crate::tx_set_utils::trim_invalid_two_phase(
                &classic_txs,
                &soroban_txs,
                &ctx,
                &close_time_bounds,
                fee_ref,
                account_ref,
            )
        } else {
            (classic_txs, soroban_txs)
        };

        sort_txs_by_hash(&mut soroban_txs);

        let classic_phase = build_classic_phase(
            classic_txs,
            classic_limited,
            dex_limited,
            base_fee,
            self.config.max_dex_ops.is_some(),
            self.config.network_id,
        );

        // Build Soroban phase first, then compute base fee from survivors.
        // This fixes #1477 (stale base fee after trim) and #1494 (missing
        // hadTxNotFittingLane feedback). stellar-core computes base fee after
        // building the parallel phase using computeLaneBaseFee().
        let (soroban_phase, _soroban_base_fee) =
            build_soroban_phase_with_base_fee(soroban_txs, soroban_limited, base_fee, &self.config);

        let trimmed_transactions = collect_phase_transactions(&classic_phase, &soroban_phase);

        let gen_tx_set = GeneralizedTransactionSet::V1(stellar_xdr::curr::TransactionSetV1 {
            previous_ledger_hash: stellar_xdr::curr::Hash(previous_ledger_hash.0),
            phases: vec![classic_phase, soroban_phase]
                .try_into()
                .unwrap_or_default(),
        });

        // Compute hash as SHA-256 of XDR-encoded GeneralizedTransactionSet
        let hash = if let Ok(xdr_bytes) = gen_tx_set.to_xdr(stellar_xdr::curr::Limits::none()) {
            Hash256::hash(&xdr_bytes)
        } else {
            Hash256::ZERO
        };

        let tx_set = TransactionSet::with_generalized(
            previous_ledger_hash,
            hash,
            trimmed_transactions,
            gen_tx_set.clone(),
        );
        (tx_set, gen_tx_set)
    }

    #[cfg(test)]
    pub(super) fn select_transactions(&self, max_ops: usize) -> SelectedTxs {
        self.select_transactions_with_starting_seq(max_ops, None)
    }

    fn select_transactions_with_starting_seq(
        &self,
        max_ops: usize,
        starting_seq: Option<&HashMap<Vec<u8>, i64>>,
    ) -> SelectedTxs {
        let seed = if cfg!(test) {
            0
        } else {
            rand::thread_rng().gen()
        };
        let store = self.store.read();
        let mut per_account: HashMap<Vec<u8>, Vec<QueuedTransaction>> = HashMap::new();

        for tx in store.values() {
            if tx.is_expired(self.config.max_age_secs) {
                continue;
            }
            let key = account_key(&tx.envelope);
            per_account.entry(key).or_default().push(tx.clone());
        }

        let mut layered: HashMap<Vec<u8>, Vec<QueuedTransaction>> = HashMap::new();
        for (account, mut txs) in per_account {
            txs.sort_by(|a, b| {
                a.sequence_number()
                    .cmp(&b.sequence_number())
                    .then_with(|| {
                        fee_rate_cmp(b.inclusion_fee, b.op_count, a.inclusion_fee, a.op_count)
                    })
                    .then_with(|| a.hash.0.cmp(&b.hash.0))
            });

            let mut deduped: HashMap<i64, QueuedTransaction> = HashMap::new();
            for tx in txs {
                let seq = tx.sequence_number();
                match deduped.get(&seq) {
                    None => {
                        deduped.insert(seq, tx);
                    }
                    Some(existing) => {
                        let replace = better_fee_ratio(&tx, existing);
                        if replace {
                            deduped.insert(seq, tx);
                        }
                    }
                }
            }

            let mut seqs: Vec<_> = deduped.keys().copied().collect();
            seqs.sort();
            let mut contiguous = Vec::new();
            let mut expected = starting_seq
                .and_then(|map| map.get(&account).copied())
                .map(|seq| seq.saturating_add(1));
            for seq in seqs {
                if let Some(exp) = expected {
                    if seq < exp {
                        continue;
                    }
                    if seq != exp {
                        break;
                    }
                }
                let Some(tx) = deduped.remove(&seq) else {
                    break;
                };
                contiguous.push(tx);
                expected = Some(seq.saturating_add(1));
            }

            if !contiguous.is_empty() {
                layered.insert(account, contiguous);
            }
        }

        let max_ops = u32::try_from(max_ops).unwrap_or(u32::MAX);
        let use_classic_bytes =
            self.config.max_classic_bytes.is_some() || self.config.max_dex_bytes.is_some();
        let ledger_version = self.validation_context.read().protocol_version;

        let (classic_accounts, soroban_accounts) = self.split_layered_accounts_by_phase(&layered);

        let classic_bytes = self
            .config
            .max_classic_bytes
            .unwrap_or(MAX_CLASSIC_BYTE_ALLOWANCE) as i64;
        let classic_limit = if use_classic_bytes {
            Resource::new(vec![max_ops as i64, classic_bytes])
        } else {
            Resource::new(vec![max_ops as i64])
        };
        let dex_limit = self.config.max_dex_ops.map(|dex_ops| {
            if use_classic_bytes {
                // stellar-core uses MAX_CLASSIC_BYTE_ALLOWANCE for the DEX lane byte limit.
                Resource::new(vec![dex_ops as i64, MAX_CLASSIC_BYTE_ALLOWANCE as i64])
            } else {
                Resource::new(vec![dex_ops as i64])
            }
        });

        let classic_lane_config = DexLimitingLaneConfig::new(classic_limit, dex_limit);
        let mut classic_had_not_fitting = Vec::new();
        let mut classic_queue = SurgePricingPriorityQueue::new(Box::new(classic_lane_config), seed);
        let mut classic_positions: HashMap<Vec<u8>, usize> = HashMap::new();
        seed_account_queue(
            &mut classic_queue,
            &classic_accounts,
            &mut classic_positions,
            &self.config.network_id,
            ledger_version,
        );

        let mut classic_selected = Vec::new();
        let lane_count = classic_queue.get_num_lanes();
        let mut classic_lane_left: Vec<Resource> = (0..lane_count)
            .map(|lane| classic_queue.lane_limits(lane))
            .collect();
        classic_had_not_fitting.resize(lane_count, false);
        while let Some((lane, entry)) = classic_queue.peek_top() {
            let frame = henyey_tx::TransactionFrame::from_owned_with_network(
                entry.tx.envelope.clone(),
                self.config.network_id,
            );
            let resources = classic_queue.tx_resources(&frame, ledger_version);
            let exceeds_lane = any_greater(&resources, &classic_lane_left[lane]);
            let exceeds_generic = any_greater(&resources, &classic_lane_left[GENERIC_LANE]);
            if exceeds_lane || exceeds_generic {
                if exceeds_lane {
                    classic_had_not_fitting[lane] = true;
                } else {
                    classic_had_not_fitting[GENERIC_LANE] = true;
                }
                classic_queue.remove_entry(lane, &entry, ledger_version, &self.config.network_id);
                continue;
            }

            classic_selected.push(entry.tx.clone());
            classic_lane_left[GENERIC_LANE] -= resources.clone();
            if lane != GENERIC_LANE {
                classic_lane_left[lane] -= resources;
            }

            classic_queue.remove_entry(lane, &entry, ledger_version, &self.config.network_id);
            let account = account_key(&entry.tx.envelope);
            push_next_account_tx(
                &mut classic_queue,
                &classic_accounts,
                &mut classic_positions,
                &account,
                &self.config.network_id,
                ledger_version,
            );
        }
        let classic_limited = classic_had_not_fitting
            .get(GENERIC_LANE)
            .copied()
            .unwrap_or(false);
        let dex_limited = classic_had_not_fitting
            .get(crate::surge_pricing::DEX_LANE)
            .copied()
            .unwrap_or(false);

        // Use 1x ledger-max Soroban limits for tx-set selection, not the 2x
        // queue-admission limits. Falls back to static config if not yet seeded.
        let mut soroban_limit = self.effective_selection_soroban_resources();
        if soroban_limit.is_none() {
            if let Some(byte_limit) = self.config.max_soroban_bytes {
                let mut values = vec![i64::MAX; NUM_SOROBAN_TX_RESOURCES];
                values[ResourceType::TxByteSize as usize] = byte_limit as i64;
                soroban_limit = Some(Resource::new(values));
            }
        }
        if let (Some(limit), Some(byte_limit)) =
            (soroban_limit.as_mut(), self.config.max_soroban_bytes)
        {
            let current = limit.get_val(ResourceType::TxByteSize);
            let clamped = (byte_limit as i64).min(current);
            limit.set_val(ResourceType::TxByteSize, clamped);
        }
        // When building Soroban tx sets with parallel execution support,
        // instructions are accounted for by the parallel builder logic, not by
        // the surge pricing config. Relax the instruction limit so surge pricing
        // doesn't prematurely reject transactions that could fit across stages.
        // Mirrors stellar-core TxSetFrame.cpp:513-524.
        let use_parallel = self.config.ledger_max_instructions > 0
            && self.config.ledger_max_dependent_tx_clusters > 0
            && self.config.soroban_phase_max_stage_count > 0;
        if use_parallel {
            if let Some(limit) = soroban_limit.as_mut() {
                limit.set_val(ResourceType::Instructions, i64::MAX);
            }
        }
        let (soroban_selected, soroban_limited) = if let Some(limit) = soroban_limit {
            let mut had_not_fitting = [false];
            let lane_config = SorobanGenericLaneConfig::new(limit);
            let mut queue = SurgePricingPriorityQueue::new(Box::new(lane_config), seed);
            let mut positions: HashMap<Vec<u8>, usize> = HashMap::new();
            seed_account_queue(
                &mut queue,
                &soroban_accounts,
                &mut positions,
                &self.config.network_id,
                ledger_version,
            );

            let mut selected = Vec::new();
            let mut lane_left = [queue.lane_limits(GENERIC_LANE)];
            while let Some((lane, entry)) = queue.peek_top() {
                let frame = henyey_tx::TransactionFrame::from_owned_with_network(
                    entry.tx.envelope.clone(),
                    self.config.network_id,
                );
                let resources = queue.tx_resources(&frame, ledger_version);
                let exceeds = any_greater(&resources, &lane_left[GENERIC_LANE]);
                if exceeds {
                    had_not_fitting[GENERIC_LANE] = true;
                    queue.remove_entry(lane, &entry, ledger_version, &self.config.network_id);
                    continue;
                }
                selected.push(entry.tx.clone());
                lane_left[GENERIC_LANE] -= resources;
                queue.remove_entry(lane, &entry, ledger_version, &self.config.network_id);
                let account = account_key(&entry.tx.envelope);
                push_next_account_tx(
                    &mut queue,
                    &soroban_accounts,
                    &mut positions,
                    &account,
                    &self.config.network_id,
                    ledger_version,
                );
            }
            let limited = had_not_fitting.get(GENERIC_LANE).copied().unwrap_or(false);
            (selected, limited)
        } else {
            let mut selected = Vec::new();
            let mut accounts: Vec<_> = soroban_accounts.keys().cloned().collect();
            accounts.sort();
            for account in accounts {
                if let Some(txs) = soroban_accounts.get(&account) {
                    selected.extend(txs.iter().cloned());
                }
            }
            (selected, false)
        };

        let mut transactions = Vec::new();
        transactions.extend(classic_selected.into_iter().map(|tx| tx.envelope));
        transactions.extend(soroban_selected.into_iter().map(|tx| tx.envelope));

        SelectedTxs {
            transactions,
            soroban_limited,
            dex_limited,
            classic_limited,
        }
    }

    fn split_layered_accounts_by_phase(
        &self,
        layered: &AccountTransactions,
    ) -> (AccountTransactions, AccountTransactions) {
        let mut classic_accounts: AccountTransactions = HashMap::new();
        let mut soroban_accounts: AccountTransactions = HashMap::new();
        let mut accounts: Vec<_> = layered.keys().cloned().collect();
        accounts.sort();

        for account in accounts {
            let Some(txs) = layered.get(&account) else {
                continue;
            };

            let mut seen_soroban = false;
            for tx in txs {
                let frame = henyey_tx::TransactionFrame::from_owned_with_network(
                    tx.envelope.clone(),
                    self.config.network_id,
                );
                let is_soroban = frame.is_soroban();
                if seen_soroban && !is_soroban {
                    break;
                }

                let target = if seen_soroban || is_soroban {
                    seen_soroban = true;
                    &mut soroban_accounts
                } else {
                    &mut classic_accounts
                };

                target.entry(account.clone()).or_default().push(tx.clone());
            }
        }

        (classic_accounts, soroban_accounts)
    }
}

/// Build the Soroban phase and compute base fee from surviving transactions.
///
/// Matches stellar-core's approach: build the parallel phase first, then derive
/// the base fee from the transactions that actually fit. This prevents stale
/// base fees from transactions dropped during stage packing.
///
/// Returns (phase, base_fee_used) — the base_fee_used is also embedded in the phase.
fn build_soroban_phase_with_base_fee(
    soroban_txs: Vec<TransactionEnvelope>,
    soroban_limited: bool,
    ledger_base_fee: i64,
    config: &super::TxQueueConfig,
) -> (stellar_xdr::curr::TransactionPhase, Option<i64>) {
    use stellar_xdr::curr::{
        DependentTxCluster, ParallelTxExecutionStage, ParallelTxsComponent, TransactionPhase, VecM,
    };

    if soroban_txs.is_empty() {
        let base_fee = if soroban_limited {
            Some(ledger_base_fee)
        } else {
            None
        };
        return (
            TransactionPhase::V1(ParallelTxsComponent {
                base_fee,
                execution_stages: VecM::default(),
            }),
            base_fee,
        );
    }

    let use_parallel = config.ledger_max_instructions > 0
        && config.ledger_max_dependent_tx_clusters > 0
        && config.soroban_phase_max_stage_count > 0;

    if use_parallel {
        let (stages, had_tx_not_fitting) =
            crate::parallel_tx_set_builder::build_parallel_soroban_phase(
                soroban_txs,
                config.network_id,
                config.ledger_max_instructions,
                config.ledger_max_dependent_tx_clusters,
                config.soroban_phase_min_stage_count,
                config.soroban_phase_max_stage_count,
            );

        // Compute base fee from surviving txs (post-build), not candidates.
        // stellar-core: computeLaneBaseFee uses hadTxNotFittingLane + lowest
        // fee of surviving txs.
        let soroban_base_fee = compute_soroban_base_fee(
            &stages,
            soroban_limited,
            had_tx_not_fitting,
            ledger_base_fee,
        );

        let phase = crate::parallel_tx_set_builder::stages_to_xdr_phase(stages, soroban_base_fee);
        (phase, soroban_base_fee)
    } else {
        // Non-parallel: all txs included, no drops possible.
        let soroban_base_fee = if soroban_limited {
            soroban_txs
                .iter()
                .filter_map(|tx| envelope_fee_per_op(tx).map(|(per_op, _, _)| per_op as i64))
                .min()
                .or(Some(ledger_base_fee))
        } else {
            Some(ledger_base_fee)
        };

        let cluster = DependentTxCluster(soroban_txs.try_into().unwrap_or_default());
        let stage = ParallelTxExecutionStage(vec![cluster].try_into().unwrap_or_default());
        let phase = TransactionPhase::V1(ParallelTxsComponent {
            base_fee: soroban_base_fee,
            execution_stages: vec![stage].try_into().unwrap_or_default(),
        });
        (phase, soroban_base_fee)
    }
}

/// Compute Soroban base fee from surviving transactions after parallel build.
///
/// Matches stellar-core's `computeLaneBaseFee` logic:
/// - If surge-priced (limited) or txs were dropped: use min fee-per-op of survivors
/// - Otherwise: use ledger base fee
fn compute_soroban_base_fee(
    stages: &[Vec<Vec<TransactionEnvelope>>],
    soroban_limited: bool,
    had_tx_not_fitting: bool,
    ledger_base_fee: i64,
) -> Option<i64> {
    if stages.is_empty() {
        return if soroban_limited || had_tx_not_fitting {
            Some(ledger_base_fee)
        } else {
            None
        };
    }

    if soroban_limited || had_tx_not_fitting {
        // Compute min fee-per-op from surviving txs
        let min_fee = stages
            .iter()
            .flat_map(|stage| stage.iter())
            .flat_map(|cluster| cluster.iter())
            .filter_map(|tx| envelope_fee_per_op(tx).map(|(per_op, _, _)| per_op as i64))
            .min();
        min_fee.or(Some(ledger_base_fee))
    } else {
        Some(ledger_base_fee)
    }
}

/// Collect all transaction envelopes from classic and soroban phases.
fn collect_phase_transactions(
    classic_phase: &stellar_xdr::curr::TransactionPhase,
    soroban_phase: &stellar_xdr::curr::TransactionPhase,
) -> Vec<TransactionEnvelope> {
    use stellar_xdr::curr::TransactionPhase;

    let mut all = Vec::new();
    if let TransactionPhase::V0(components) = classic_phase {
        for comp in components.iter() {
            match comp {
                stellar_xdr::curr::TxSetComponent::TxsetCompTxsMaybeDiscountedFee(c) => {
                    all.extend(c.txs.iter().cloned());
                }
            }
        }
    }
    if let TransactionPhase::V1(component) = soroban_phase {
        for stage in component.execution_stages.iter() {
            for cluster in stage.0.iter() {
                all.extend(cluster.0.iter().cloned());
            }
        }
    }
    all
}

fn build_classic_phase(
    classic_txs: Vec<TransactionEnvelope>,
    classic_limited: bool,
    dex_limited: bool,
    base_fee: i64,
    has_dex_lane: bool,
    network_id: NetworkId,
) -> stellar_xdr::curr::TransactionPhase {
    use stellar_xdr::curr::{
        TransactionPhase, TxSetComponent, TxSetComponentTxsMaybeDiscountedFee,
    };

    let mut classic_components: Vec<TxSetComponent> = Vec::new();
    if !classic_txs.is_empty() {
        let lane_count = if has_dex_lane { 2 } else { 1 };
        let mut lowest_lane_fee = vec![i64::MAX; lane_count];
        let mut lane_for_tx = Vec::with_capacity(classic_txs.len());

        for tx in &classic_txs {
            let frame =
                henyey_tx::TransactionFrame::from_owned_with_network(tx.clone(), network_id);
            let lane = if has_dex_lane && frame.has_dex_operations() {
                crate::surge_pricing::DEX_LANE
            } else {
                GENERIC_LANE
            };
            if let Some((per_op, _, _)) = envelope_fee_per_op(tx) {
                let lane_fee = &mut lowest_lane_fee[lane];
                let fee = per_op as i64;
                if fee < *lane_fee {
                    *lane_fee = fee;
                }
            }
            lane_for_tx.push(lane);
        }

        let min_lane_fee = lowest_lane_fee
            .iter()
            .copied()
            .filter(|fee| *fee != i64::MAX)
            .min()
            .unwrap_or(base_fee);
        let mut lane_base_fee = vec![base_fee; lane_count];
        if classic_limited {
            lane_base_fee.fill(min_lane_fee);
        }
        if has_dex_lane && dex_limited {
            let dex_fee = lowest_lane_fee[crate::surge_pricing::DEX_LANE];
            if dex_fee != i64::MAX {
                lane_base_fee[crate::surge_pricing::DEX_LANE] = dex_fee;
            }
        }

        let mut components_by_fee: BTreeMap<i64, Vec<TransactionEnvelope>> = BTreeMap::new();
        for (tx, lane) in classic_txs.into_iter().zip(lane_for_tx) {
            let fee = lane_base_fee[lane];
            components_by_fee.entry(fee).or_default().push(tx);
        }

        for (fee, mut txs) in components_by_fee {
            sort_txs_by_hash(&mut txs);
            classic_components.push(TxSetComponent::TxsetCompTxsMaybeDiscountedFee(
                TxSetComponentTxsMaybeDiscountedFee {
                    base_fee: Some(fee),
                    txs: txs.try_into().unwrap_or_default(),
                },
            ));
        }
    }
    TransactionPhase::V0(classic_components.try_into().unwrap_or_default())
}
