//! Transaction set types and wire-format parsing.
//!
//! Defines [`TransactionSet`] for carrying candidate and externalized
//! transaction sets, plus validation and conversion between the legacy
//! (v0) and generalized (v1) XDR wire formats.

use super::*;

pub(super) fn sort_txs_by_hash(txs: &mut [TransactionEnvelope]) {
    // Schwartzian transform: compute each hash once (O(N)), sort by cached
    // hashes (O(N log N)), then reorder the slice to match.
    let mut keyed: Vec<(Hash256, TransactionEnvelope)> = txs
        .iter()
        .map(|tx| (Hash256::hash_xdr(tx).unwrap_or_default(), tx.clone()))
        .collect();
    keyed.sort_by(|(ha, _), (hb, _)| ha.0.cmp(&hb.0));
    for (i, (_, tx)) in keyed.into_iter().enumerate() {
        txs[i] = tx;
    }
}

/// A set of transactions for a ledger.
#[derive(Debug, Clone)]
pub struct TransactionSet {
    /// Hash of this transaction set.
    pub hash: Hash256,
    /// Previous ledger hash.
    pub previous_ledger_hash: Hash256,
    /// Transactions in the set.
    pub transactions: Vec<TransactionEnvelope>,
    /// Generalized transaction set (protocol 20+), if available.
    pub generalized_tx_set: Option<GeneralizedTransactionSet>,
}

impl TransactionSet {
    /// Compute the legacy TransactionSet contents hash (non-generalized).
    pub fn compute_non_generalized_hash(
        previous_ledger_hash: Hash256,
        transactions: &[TransactionEnvelope],
    ) -> Option<Hash256> {
        let mut hasher = Sha256Hasher::new();
        hasher.update(&previous_ledger_hash.0);
        for tx in transactions {
            let bytes = tx.to_xdr(Limits::none()).ok()?;
            hasher.update(&bytes);
        }
        Some(hasher.finalize())
    }

    /// Create a new transaction set with computed hash (for legacy TransactionSet).
    pub fn new(previous_ledger_hash: Hash256, transactions: Vec<TransactionEnvelope>) -> Self {
        let mut transactions = transactions;
        sort_txs_by_hash(&mut transactions);
        let hash = Self::compute_non_generalized_hash(previous_ledger_hash, &transactions)
            .unwrap_or_default();

        Self {
            hash,
            previous_ledger_hash,
            transactions,
            generalized_tx_set: None,
        }
    }

    /// Create a transaction set with a pre-computed hash (for GeneralizedTransactionSet).
    /// The hash should be SHA-256 of the XDR-encoded GeneralizedTransactionSet.
    pub fn with_hash(
        previous_ledger_hash: Hash256,
        hash: Hash256,
        transactions: Vec<TransactionEnvelope>,
    ) -> Self {
        Self {
            hash,
            previous_ledger_hash,
            transactions,
            generalized_tx_set: None,
        }
    }

    pub fn with_generalized(
        previous_ledger_hash: Hash256,
        hash: Hash256,
        transactions: Vec<TransactionEnvelope>,
        generalized_tx_set: GeneralizedTransactionSet,
    ) -> Self {
        Self {
            hash,
            previous_ledger_hash,
            transactions,
            generalized_tx_set: Some(generalized_tx_set),
        }
    }

    /// Get the number of transactions.
    pub fn len(&self) -> usize {
        self.transactions.len()
    }

    /// Check if the set is empty.
    pub fn is_empty(&self) -> bool {
        self.transactions.is_empty()
    }

    /// Recompute the transaction set hash from its contents.
    pub fn recompute_hash(&self) -> Option<Hash256> {
        if let Some(gen) = &self.generalized_tx_set {
            let bytes = gen.to_xdr(stellar_xdr::curr::Limits::none()).ok()?;
            return Some(Hash256::hash(&bytes));
        }
        Self::compute_non_generalized_hash(self.previous_ledger_hash, &self.transactions)
    }

    /// Summarize the transaction set for logging/debugging.
    pub fn summary(&self) -> String {
        if self.transactions.is_empty() {
            return "empty tx set".to_string();
        }

        if let Some(gen) = &self.generalized_tx_set {
            return summary_generalized_tx_set(gen);
        }

        let tx_count = self.transactions.len();
        let op_count: i64 = self.transactions.iter().map(tx_operation_count).sum();
        let base_fee = self
            .transactions
            .iter()
            .map(tx_inclusion_fee)
            .zip(self.transactions.iter().map(tx_operation_count))
            .filter(|(_, ops)| *ops > 0)
            .map(|(fee, ops)| fee / ops)
            .min()
            .unwrap_or(0);

        format!("txs:{}, ops:{}, base_fee:{}", tx_count, op_count, base_fee)
    }

    /// Convert to StoredTransactionSet XDR for persistence.
    ///
    /// Uses the generalized format (v1) if available, otherwise falls back to legacy (v0).
    pub fn to_xdr_stored_set(&self) -> stellar_xdr::curr::StoredTransactionSet {
        use stellar_xdr::curr::StoredTransactionSet;

        if let Some(ref gen) = self.generalized_tx_set {
            StoredTransactionSet::V1(gen.clone())
        } else {
            // Build legacy TransactionSet
            let legacy = stellar_xdr::curr::TransactionSet {
                previous_ledger_hash: stellar_xdr::curr::Hash(self.previous_ledger_hash.0),
                txs: self.transactions.clone().try_into().unwrap_or_default(),
            };
            StoredTransactionSet::V0(legacy)
        }
    }

    /// Create from StoredTransactionSet XDR.
    ///
    /// # Errors
    ///
    /// Returns an error description if the transaction set cannot be decoded.
    pub fn from_xdr_stored_set(
        stored: &stellar_xdr::curr::StoredTransactionSet,
    ) -> std::result::Result<Self, String> {
        use stellar_xdr::curr::StoredTransactionSet;

        match stored {
            StoredTransactionSet::V0(legacy) => {
                let previous_ledger_hash = Hash256::from_bytes(legacy.previous_ledger_hash.0);
                let transactions: Vec<TransactionEnvelope> = legacy.txs.to_vec();

                // Compute hash
                let hash = Self::compute_non_generalized_hash(previous_ledger_hash, &transactions)
                    .ok_or_else(|| "Failed to compute tx set hash".to_string())?;

                Ok(Self {
                    hash,
                    previous_ledger_hash,
                    transactions,
                    generalized_tx_set: None,
                })
            }
            StoredTransactionSet::V1(gen) => {
                let previous_ledger_hash = match gen {
                    GeneralizedTransactionSet::V1(v1) => {
                        Hash256::from_bytes(v1.previous_ledger_hash.0)
                    }
                };

                // Extract transactions from phases
                let transactions = extract_transactions_from_generalized(gen);

                // Compute hash from generalized format
                let hash = gen
                    .to_xdr(Limits::none())
                    .map(|bytes| Hash256::hash(&bytes))
                    .map_err(|e| format!("Failed to encode generalized tx set: {}", e))?;

                Ok(Self {
                    hash,
                    previous_ledger_hash,
                    transactions,
                    generalized_tx_set: Some(gen.clone()),
                })
            }
        }
    }

    /// Prepare a transaction set for ledger application.
    ///
    /// This corresponds to upstream `TxSetXDRFrame::prepareForApply()`. It validates
    /// the XDR structure of the generalized transaction set, deserializes transaction
    /// envelopes, validates that each transaction has a valid fee structure, checks
    /// that transactions are properly sorted within components/clusters, and verifies
    /// that classic and Soroban transactions are in the correct phases.
    ///
    /// For legacy (non-generalized) transaction sets, only basic fee validation and
    /// sort order checks are performed.
    pub fn prepare_for_apply(&self, network_id: NetworkId) -> std::result::Result<Self, String> {
        if let Some(ref gen) = self.generalized_tx_set {
            Self::prepare_generalized_for_apply(gen, network_id)
        } else {
            Self::prepare_legacy_for_apply(
                self.previous_ledger_hash,
                &self.transactions,
                network_id,
            )
        }
    }

    /// Validate and prepare a generalized transaction set for application.
    fn prepare_generalized_for_apply(
        gen: &GeneralizedTransactionSet,
        network_id: NetworkId,
    ) -> std::result::Result<Self, String> {
        validate_generalized_tx_set_xdr_structure(gen)?;

        let GeneralizedTransactionSet::V1(v1) = gen;
        let mut all_transactions = Vec::new();

        for (phase_id, phase) in v1.phases.iter().enumerate() {
            let expect_soroban = phase_id == 1;
            match phase {
                TransactionPhase::V0(components) => {
                    for component in components.iter() {
                        match component {
                            TxSetComponent::TxsetCompTxsMaybeDiscountedFee(comp) => {
                                validate_wire_txs(&comp.txs, network_id, expect_soroban)?;
                                all_transactions.extend(comp.txs.iter().cloned());
                            }
                        }
                    }
                }
                TransactionPhase::V1(parallel) => {
                    for stage in parallel.execution_stages.iter() {
                        for cluster in stage.iter() {
                            validate_wire_txs(cluster.as_slice(), network_id, expect_soroban)?;
                            all_transactions.extend(cluster.iter().cloned());
                        }
                    }
                }
            }
        }

        // HERDER_SPEC §8.3 / §6.5: No two transactions across ALL phases may
        // share the same source account in a generalized transaction set.
        check_no_duplicate_source_accounts(&all_transactions)?;

        let hash = gen
            .to_xdr(Limits::none())
            .map(|bytes| Hash256::hash(&bytes))
            .map_err(|e| format!("Failed to encode generalized tx set: {}", e))?;

        let previous_ledger_hash = Hash256::from_bytes(v1.previous_ledger_hash.0);

        Ok(Self {
            hash,
            previous_ledger_hash,
            transactions: all_transactions,
            generalized_tx_set: Some(gen.clone()),
        })
    }

    /// Validate and prepare a legacy (non-generalized) transaction set for application.
    fn prepare_legacy_for_apply(
        previous_ledger_hash: Hash256,
        transactions: &[TransactionEnvelope],
        network_id: NetworkId,
    ) -> std::result::Result<Self, String> {
        for env in transactions {
            validate_tx_fee(env)?;
            let frame =
                henyey_tx::TransactionFrame::from_owned_with_network(env.clone(), network_id);
            if frame.is_soroban() {
                return Err("Legacy transaction set contains Soroban transaction".to_string());
            }
        }

        if !is_sorted_by_hash(transactions) {
            return Err("Legacy transaction set transactions are not sorted correctly".to_string());
        }

        let hash = Self::compute_non_generalized_hash(previous_ledger_hash, transactions)
            .ok_or_else(|| "Failed to compute tx set hash".to_string())?;

        Ok(Self {
            hash,
            previous_ledger_hash,
            transactions: transactions.to_vec(),
            generalized_tx_set: None,
        })
    }
}

/// Maximum allowed Soroban resource fee (2^50), matching upstream MAX_RESOURCE_FEE.
const MAX_RESOURCE_FEE: i64 = 1i64 << 50;

/// Validate the XDR structure of a GeneralizedTransactionSet.
///
/// Mirrors upstream `validateTxSetXDRStructure`.
fn validate_generalized_tx_set_xdr_structure(
    gen: &GeneralizedTransactionSet,
) -> std::result::Result<(), String> {
    let GeneralizedTransactionSet::V1(v1) = gen;

    if v1.phases.len() != 2 {
        return Err(format!(
            "Expected exactly 2 phases, got {}",
            v1.phases.len()
        ));
    }

    for (phase_id, phase) in v1.phases.iter().enumerate() {
        match phase {
            TransactionPhase::V0(components) => {
                validate_sequential_phase_xdr_structure(components.as_slice())?;
            }
            TransactionPhase::V1(parallel) => {
                if phase_id != 1 {
                    return Err(format!("Non-Soroban parallel phase at index {}", phase_id));
                }
                validate_parallel_component(parallel)?;
            }
        }
    }

    Ok(())
}

/// Validate the XDR structure of a sequential (V0) phase.
fn validate_sequential_phase_xdr_structure(
    components: &[TxSetComponent],
) -> std::result::Result<(), String> {
    let is_sorted = components.windows(2).all(|pair| {
        let fee_a = match &pair[0] {
            TxSetComponent::TxsetCompTxsMaybeDiscountedFee(c) => c.base_fee,
        };
        let fee_b = match &pair[1] {
            TxSetComponent::TxsetCompTxsMaybeDiscountedFee(c) => c.base_fee,
        };
        match (fee_a, fee_b) {
            (None, Some(_)) => true,
            (None, None) => false,
            (Some(_), None) => false,
            (Some(a), Some(b)) => a < b,
        }
    });
    if !is_sorted {
        return Err("Incorrect component order or duplicate base fees".to_string());
    }

    for component in components {
        match component {
            TxSetComponent::TxsetCompTxsMaybeDiscountedFee(comp) => {
                if comp.txs.is_empty() {
                    return Err("Empty component in sequential phase".to_string());
                }
                // Reject negative base fees (parity: TxSetFrame.cpp:1442)
                if let Some(fee) = comp.base_fee {
                    if fee < 0 {
                        return Err("Component has negative base fee".to_string());
                    }
                }
            }
        }
    }

    Ok(())
}

/// Validate the structure of a parallel (V1) phase component.
fn validate_parallel_component(
    parallel: &stellar_xdr::curr::ParallelTxsComponent,
) -> std::result::Result<(), String> {
    // Reject negative base fees (parity: TxSetFrame.cpp:1480)
    if let Some(fee) = parallel.base_fee {
        if fee < 0 {
            return Err("Parallel component has negative base fee".to_string());
        }
    }
    for stage in parallel.execution_stages.iter() {
        if stage.is_empty() {
            return Err("Empty stage in parallel phase".to_string());
        }
        for cluster in stage.iter() {
            if cluster.is_empty() {
                return Err("Empty cluster in parallel phase".to_string());
            }
        }
    }
    // HERDER_SPEC §7.7: Validate canonical ordering for parallel phases.
    // Clusters within each stage must be sorted by first-TX hash (ascending).
    // Stages must be sorted by first-TX-of-first-cluster hash (ascending).
    for stage in parallel.execution_stages.iter() {
        // Check clusters within this stage are sorted by first-TX hash
        let cluster_sorted = stage.windows(2).all(|pair| {
            let hash_a = Hash256::hash_xdr(&pair[0][0]).unwrap_or_default();
            let hash_b = Hash256::hash_xdr(&pair[1][0]).unwrap_or_default();
            hash_a.0 < hash_b.0
        });
        if !cluster_sorted {
            return Err(
                "Clusters within stage are not in canonical order (by first-TX hash)".to_string(),
            );
        }
    }
    // Check stages are sorted by first-TX-of-first-cluster hash
    let stages = &parallel.execution_stages;
    let stages_sorted = stages.windows(2).all(|pair| {
        let hash_a = Hash256::hash_xdr(&pair[0][0][0]).unwrap_or_default();
        let hash_b = Hash256::hash_xdr(&pair[1][0][0]).unwrap_or_default();
        hash_a.0 < hash_b.0
    });
    if !stages_sorted {
        return Err(
            "Stages are not in canonical order (by first-TX-of-first-cluster hash)".to_string(),
        );
    }

    Ok(())
}

/// Check that no two transactions share the same source account across all phases.
///
/// HERDER_SPEC §8.3 item 4 / §6.5: Generalized transaction sets MUST NOT contain
/// duplicate source accounts across phases.
fn check_no_duplicate_source_accounts(
    txs: &[TransactionEnvelope],
) -> std::result::Result<(), String> {
    let mut seen = HashSet::new();
    for env in txs {
        let source_key = source_account_ed25519(env);
        if !seen.insert(source_key) {
            return Err("Duplicate source account across phases in generalized tx set".to_string());
        }
    }
    Ok(())
}

/// Extract the ed25519 public key bytes from a transaction envelope's source account.
///
/// For fee-bump transactions, uses the *inner* transaction source (matching stellar-core's
/// `getSourceID()` which returns the inner source for fee bumps).
fn source_account_ed25519(env: &TransactionEnvelope) -> [u8; 32] {
    match env {
        TransactionEnvelope::TxV0(e) => e.tx.source_account_ed25519.0,
        TransactionEnvelope::Tx(e) => henyey_tx::muxed_to_ed25519(&e.tx.source_account).0,
        TransactionEnvelope::TxFeeBump(e) => match &e.tx.inner_tx {
            FeeBumpTransactionInnerTx::Tx(inner) => {
                henyey_tx::muxed_to_ed25519(&inner.tx.source_account).0
            }
        },
    }
}

/// Validate that a transaction envelope has a valid fee for inclusion in a tx set.
///
/// Mirrors upstream `XDRProvidesValidFee`.
fn validate_tx_fee(env: &TransactionEnvelope) -> std::result::Result<(), String> {
    let is_soroban = match env {
        TransactionEnvelope::TxV0(e) => e.tx.operations.iter().any(|op| {
            matches!(
                op.body,
                stellar_xdr::curr::OperationBody::InvokeHostFunction(_)
                    | stellar_xdr::curr::OperationBody::ExtendFootprintTtl(_)
                    | stellar_xdr::curr::OperationBody::RestoreFootprint(_)
            )
        }),
        TransactionEnvelope::Tx(e) => e.tx.operations.iter().any(|op| {
            matches!(
                op.body,
                stellar_xdr::curr::OperationBody::InvokeHostFunction(_)
                    | stellar_xdr::curr::OperationBody::ExtendFootprintTtl(_)
                    | stellar_xdr::curr::OperationBody::RestoreFootprint(_)
            )
        }),
        TransactionEnvelope::TxFeeBump(e) => match &e.tx.inner_tx {
            FeeBumpTransactionInnerTx::Tx(inner) => inner.tx.operations.iter().any(|op| {
                matches!(
                    op.body,
                    stellar_xdr::curr::OperationBody::InvokeHostFunction(_)
                        | stellar_xdr::curr::OperationBody::ExtendFootprintTtl(_)
                        | stellar_xdr::curr::OperationBody::RestoreFootprint(_)
                )
            }),
        },
    };

    if is_soroban {
        match env {
            TransactionEnvelope::TxV0(_) => {
                return Err("Soroban transaction uses TxV0 envelope".to_string());
            }
            TransactionEnvelope::Tx(e) => match &e.tx.ext {
                stellar_xdr::curr::TransactionExt::V0 => {
                    return Err("Soroban transaction missing SorobanTransactionData".to_string());
                }
                stellar_xdr::curr::TransactionExt::V1(data) => {
                    let resource_fee = data.resource_fee;
                    if resource_fee < 0 || resource_fee > MAX_RESOURCE_FEE {
                        return Err(format!(
                            "Soroban resource fee {} out of valid range",
                            resource_fee
                        ));
                    }
                }
            },
            TransactionEnvelope::TxFeeBump(e) => match &e.tx.inner_tx {
                FeeBumpTransactionInnerTx::Tx(inner) => match &inner.tx.ext {
                    stellar_xdr::curr::TransactionExt::V0 => {
                        return Err(
                            "Soroban fee-bump inner transaction missing SorobanTransactionData"
                                .to_string(),
                        );
                    }
                    stellar_xdr::curr::TransactionExt::V1(data) => {
                        let resource_fee = data.resource_fee;
                        if resource_fee < 0 || resource_fee > MAX_RESOURCE_FEE {
                            return Err(format!(
                                "Soroban resource fee {} out of valid range",
                                resource_fee
                            ));
                        }
                    }
                },
            },
        }
    }

    Ok(())
}

/// Check if a slice of transaction envelopes is sorted by hash.
fn is_sorted_by_hash(txs: &[TransactionEnvelope]) -> bool {
    txs.windows(2).all(|pair| {
        let hash_a = Hash256::hash_xdr(&pair[0]).unwrap_or_default();
        let hash_b = Hash256::hash_xdr(&pair[1]).unwrap_or_default();
        hash_a.0 <= hash_b.0
    })
}

/// Validate a set of wire-format transaction envelopes.
fn validate_wire_txs(
    txs: &[TransactionEnvelope],
    network_id: NetworkId,
    expect_soroban: bool,
) -> std::result::Result<(), String> {
    for env in txs {
        validate_tx_fee(env)?;

        let frame = henyey_tx::TransactionFrame::from_owned_with_network(env.clone(), network_id);
        if frame.is_soroban() != expect_soroban {
            if expect_soroban {
                return Err("Classic transaction found in Soroban phase".to_string());
            } else {
                return Err("Soroban transaction found in classic phase".to_string());
            }
        }
    }

    if !is_sorted_by_hash(txs) {
        return Err("Transactions are not sorted correctly within component".to_string());
    }

    Ok(())
}

/// Extract all transactions from a GeneralizedTransactionSet.
fn extract_transactions_from_generalized(
    gen: &GeneralizedTransactionSet,
) -> Vec<TransactionEnvelope> {
    let GeneralizedTransactionSet::V1(v1) = gen;
    let mut transactions = Vec::new();

    for phase in v1.phases.iter() {
        match phase {
            stellar_xdr::curr::TransactionPhase::V0(components) => {
                for component in components.iter() {
                    match component {
                        stellar_xdr::curr::TxSetComponent::TxsetCompTxsMaybeDiscountedFee(comp) => {
                            transactions.extend(comp.txs.iter().cloned());
                        }
                    }
                }
            }
            stellar_xdr::curr::TransactionPhase::V1(parallel) => {
                // V1 phase has execution_stages, which contains parallel stages
                for stage in parallel.execution_stages.iter() {
                    for cluster in stage.iter() {
                        transactions.extend(cluster.iter().cloned());
                    }
                }
            }
        }
    }

    transactions
}

fn tx_operation_count(envelope: &TransactionEnvelope) -> i64 {
    match envelope {
        TransactionEnvelope::TxV0(env) => env.tx.operations.len() as i64,
        TransactionEnvelope::Tx(env) => env.tx.operations.len() as i64,
        TransactionEnvelope::TxFeeBump(env) => match &env.tx.inner_tx {
            stellar_xdr::curr::FeeBumpTransactionInnerTx::Tx(inner) => {
                inner.tx.operations.len() as i64
            }
        },
    }
}

fn tx_inclusion_fee(envelope: &TransactionEnvelope) -> i64 {
    match envelope {
        TransactionEnvelope::TxV0(env) => env.tx.fee as i64,
        TransactionEnvelope::Tx(env) => env.tx.fee as i64,
        TransactionEnvelope::TxFeeBump(env) => env.tx.fee,
    }
}

fn summary_generalized_tx_set(gen: &GeneralizedTransactionSet) -> String {
    use std::collections::BTreeMap;

    let GeneralizedTransactionSet::V1(tx_set) = gen;
    if tx_set.phases.is_empty() {
        return "empty tx set".to_string();
    }

    let mut parts = Vec::new();
    for (phase_idx, phase) in tx_set.phases.iter().enumerate() {
        let mut component_stats: BTreeMap<Option<i64>, (i64, i64)> = BTreeMap::new();
        match phase {
            TransactionPhase::V0(components) => {
                for component in components.iter() {
                    let TxSetComponent::TxsetCompTxsMaybeDiscountedFee(comp) = component;
                    let base_fee = comp.base_fee;
                    for tx in comp.txs.iter() {
                        let entry = component_stats.entry(base_fee).or_insert((0, 0));
                        entry.0 += 1;
                        entry.1 += tx_operation_count(tx);
                    }
                }
            }
            TransactionPhase::V1(parallel) => {
                let base_fee = parallel.base_fee;
                for stage in parallel.execution_stages.iter() {
                    for cluster in stage.iter() {
                        for tx in cluster.0.iter() {
                            let entry = component_stats.entry(base_fee).or_insert((0, 0));
                            entry.0 += 1;
                            entry.1 += tx_operation_count(tx);
                        }
                    }
                }
            }
        }

        let mut component_parts = Vec::new();
        for (fee, stats) in component_stats.iter() {
            if let Some(base_fee) = fee {
                component_parts.push(format!(
                    "{{discounted txs:{}, ops:{}, base_fee:{}}}",
                    stats.0, stats.1, base_fee
                ));
            } else {
                component_parts.push(format!(
                    "{{non-discounted txs:{}, ops:{}}}",
                    stats.0, stats.1
                ));
            }
        }
        let phase_name = match phase_idx {
            0 => "classic",
            1 => "soroban",
            _ => "unknown",
        };
        parts.push(format!(
            "{} phase: {} component(s): [{}]",
            phase_name,
            component_stats.len(),
            component_parts.join(", ")
        ));
    }

    parts.join(", ")
}

#[cfg(test)]
mod tests {
    use super::*;
    use stellar_xdr::curr::{
        CreateAccountOp, DecoratedSignature, DependentTxCluster, FeeBumpTransaction,
        FeeBumpTransactionEnvelope, FeeBumpTransactionExt, FeeBumpTransactionInnerTx,
        GeneralizedTransactionSet, Memo, MuxedAccount, Operation, OperationBody,
        ParallelTxExecutionStage, ParallelTxsComponent, Preconditions, SequenceNumber,
        SignatureHint, Transaction, TransactionEnvelope, TransactionExt, TransactionPhase,
        TransactionSetV1, TransactionV1Envelope, TxSetComponent,
        TxSetComponentTxsMaybeDiscountedFee, Uint256,
    };

    fn make_tx_envelope(seed: u8, fee: u32) -> TransactionEnvelope {
        let source = MuxedAccount::Ed25519(Uint256([seed; 32]));
        let dest = stellar_xdr::curr::AccountId(
            stellar_xdr::curr::PublicKey::PublicKeyTypeEd25519(Uint256([seed.wrapping_add(1); 32])),
        );
        let tx = Transaction {
            source_account: source,
            fee,
            seq_num: SequenceNumber(seed as i64),
            cond: Preconditions::None,
            memo: Memo::None,
            operations: vec![Operation {
                source_account: None,
                body: OperationBody::CreateAccount(CreateAccountOp {
                    destination: dest,
                    starting_balance: 1_000_000_000,
                }),
            }]
            .try_into()
            .unwrap(),
            ext: TransactionExt::V0,
        };
        TransactionEnvelope::Tx(TransactionV1Envelope {
            tx,
            signatures: vec![DecoratedSignature {
                hint: SignatureHint([0u8; 4]),
                signature: stellar_xdr::curr::Signature(vec![0u8; 64].try_into().unwrap()),
            }]
            .try_into()
            .unwrap(),
        })
    }

    fn make_fee_bump_envelope(
        outer_source_seed: u8,
        inner_source_seed: u8,
        fee: i64,
    ) -> TransactionEnvelope {
        let inner_tx = make_tx_envelope(inner_source_seed, 100);
        let inner_v1 = match inner_tx {
            TransactionEnvelope::Tx(v1) => v1,
            _ => unreachable!(),
        };
        let outer_source = MuxedAccount::Ed25519(Uint256([outer_source_seed; 32]));
        TransactionEnvelope::TxFeeBump(FeeBumpTransactionEnvelope {
            tx: FeeBumpTransaction {
                fee_source: outer_source,
                fee,
                inner_tx: FeeBumpTransactionInnerTx::Tx(inner_v1),
                ext: FeeBumpTransactionExt::V0,
            },
            signatures: vec![DecoratedSignature {
                hint: SignatureHint([0u8; 4]),
                signature: stellar_xdr::curr::Signature(vec![0u8; 64].try_into().unwrap()),
            }]
            .try_into()
            .unwrap(),
        })
    }

    // =========================================================================
    // source_account_ed25519
    // =========================================================================

    #[test]
    fn test_source_account_ed25519_regular_tx() {
        let env = make_tx_envelope(42, 100);
        let result = source_account_ed25519(&env);
        assert_eq!(result, [42u8; 32]);
    }

    #[test]
    fn test_source_account_ed25519_fee_bump_returns_inner_source() {
        // For fee-bump, source_account_ed25519 should return the INNER tx source,
        // not the outer fee source.
        let env = make_fee_bump_envelope(99, 42, 200);
        let result = source_account_ed25519(&env);
        assert_eq!(
            result, [42u8; 32],
            "fee-bump should return inner source, not outer fee source"
        );
    }

    // =========================================================================
    // check_no_duplicate_source_accounts
    // =========================================================================

    #[test]
    fn test_check_no_duplicate_source_accounts_empty() {
        assert!(check_no_duplicate_source_accounts(&[]).is_ok());
    }

    #[test]
    fn test_check_no_duplicate_source_accounts_unique() {
        let txs = vec![make_tx_envelope(1, 100), make_tx_envelope(2, 100)];
        assert!(check_no_duplicate_source_accounts(&txs).is_ok());
    }

    #[test]
    fn test_check_no_duplicate_source_accounts_duplicate() {
        let txs = vec![make_tx_envelope(1, 100), make_tx_envelope(1, 200)];
        let result = check_no_duplicate_source_accounts(&txs);
        assert!(result.is_err());
        assert!(result.unwrap_err().contains("Duplicate source account"));
    }

    #[test]
    fn test_check_no_duplicate_source_accounts_fee_bump_uses_inner() {
        // Fee-bump with inner source = 1, regular tx with source = 1 → duplicate
        let txs = vec![make_fee_bump_envelope(99, 1, 200), make_tx_envelope(1, 100)];
        let result = check_no_duplicate_source_accounts(&txs);
        assert!(
            result.is_err(),
            "Fee-bump inner source duplicating regular source should be detected"
        );
    }

    #[test]
    fn test_check_no_duplicate_source_accounts_fee_bump_different_inner() {
        // Fee-bump with inner source = 2 and outer = 99, regular tx with source = 1 → OK
        let txs = vec![make_fee_bump_envelope(99, 2, 200), make_tx_envelope(1, 100)];
        assert!(check_no_duplicate_source_accounts(&txs).is_ok());
    }

    // =========================================================================
    // is_sorted_by_hash
    // =========================================================================

    #[test]
    fn test_is_sorted_by_hash_empty() {
        assert!(is_sorted_by_hash(&[]));
    }

    #[test]
    fn test_is_sorted_by_hash_single() {
        assert!(is_sorted_by_hash(&[make_tx_envelope(1, 100)]));
    }

    #[test]
    fn test_is_sorted_by_hash_sorted() {
        let mut txs = vec![
            make_tx_envelope(1, 100),
            make_tx_envelope(2, 100),
            make_tx_envelope(3, 100),
        ];
        sort_txs_by_hash(&mut txs);
        assert!(is_sorted_by_hash(&txs));
    }

    #[test]
    fn test_is_sorted_by_hash_unsorted() {
        let mut txs = vec![
            make_tx_envelope(1, 100),
            make_tx_envelope(2, 100),
            make_tx_envelope(3, 100),
        ];
        sort_txs_by_hash(&mut txs);
        // Reverse to guarantee unsorted
        txs.reverse();
        // Only fails if there were at least 2 distinct elements in non-ascending order
        if txs.len() >= 2 {
            // The reversed sorted order is descending, which is not ascending
            assert!(
                !is_sorted_by_hash(&txs),
                "Reversed sorted list should not be considered sorted"
            );
        }
    }

    #[test]
    fn test_is_sorted_by_hash_allows_equal_adjacent_hashes() {
        // is_sorted_by_hash uses <=, so equal adjacent hashes are acceptable
        let tx = make_tx_envelope(1, 100);
        let txs = vec![tx.clone(), tx];
        assert!(is_sorted_by_hash(&txs));
    }

    // =========================================================================
    // sort_txs_by_hash
    // =========================================================================

    #[test]
    fn test_sort_txs_by_hash_produces_sorted_output() {
        let mut txs = vec![
            make_tx_envelope(5, 100),
            make_tx_envelope(1, 100),
            make_tx_envelope(3, 100),
        ];
        sort_txs_by_hash(&mut txs);
        assert!(is_sorted_by_hash(&txs));
    }

    #[test]
    fn test_sort_txs_by_hash_is_idempotent() {
        let mut txs = vec![
            make_tx_envelope(5, 100),
            make_tx_envelope(1, 100),
            make_tx_envelope(3, 100),
        ];
        sort_txs_by_hash(&mut txs);
        let sorted_once: Vec<[u8; 32]> = txs
            .iter()
            .map(|t| Hash256::hash_xdr(t).unwrap_or_default().0)
            .collect();
        sort_txs_by_hash(&mut txs);
        let sorted_twice: Vec<[u8; 32]> = txs
            .iter()
            .map(|t| Hash256::hash_xdr(t).unwrap_or_default().0)
            .collect();
        assert_eq!(sorted_once, sorted_twice);
    }

    // =========================================================================
    // validate_generalized_tx_set_xdr_structure
    // =========================================================================

    fn make_classic_component(
        txs: Vec<TransactionEnvelope>,
        base_fee: Option<i64>,
    ) -> TxSetComponent {
        TxSetComponent::TxsetCompTxsMaybeDiscountedFee(TxSetComponentTxsMaybeDiscountedFee {
            base_fee,
            txs: txs.try_into().unwrap(),
        })
    }

    fn make_parallel_component(
        stages: Vec<Vec<Vec<TransactionEnvelope>>>,
        base_fee: Option<i64>,
    ) -> ParallelTxsComponent {
        let execution_stages: Vec<ParallelTxExecutionStage> = stages
            .into_iter()
            .map(|stage| {
                let clusters: Vec<DependentTxCluster> = stage
                    .into_iter()
                    .map(|cluster| DependentTxCluster(cluster.try_into().unwrap()))
                    .collect();
                ParallelTxExecutionStage(clusters.try_into().unwrap())
            })
            .collect();
        ParallelTxsComponent {
            base_fee,
            execution_stages: execution_stages.try_into().unwrap(),
        }
    }

    fn make_gen_tx_set(phases: Vec<TransactionPhase>) -> GeneralizedTransactionSet {
        GeneralizedTransactionSet::V1(TransactionSetV1 {
            previous_ledger_hash: stellar_xdr::curr::Hash([0u8; 32]),
            phases: phases.try_into().unwrap(),
        })
    }

    #[test]
    fn test_validate_generalized_tx_set_requires_two_phases() {
        // 1 phase → should fail
        let one_phase = make_gen_tx_set(vec![TransactionPhase::V0(
            vec![make_classic_component(vec![make_tx_envelope(1, 100)], None)]
                .try_into()
                .unwrap(),
        )]);
        let result = validate_generalized_tx_set_xdr_structure(&one_phase);
        assert!(result.is_err());
        assert!(result.unwrap_err().contains("2 phases"));

        // 3 phases → should fail
        let three_phases = make_gen_tx_set(vec![
            TransactionPhase::V0(
                vec![make_classic_component(vec![make_tx_envelope(1, 100)], None)]
                    .try_into()
                    .unwrap(),
            ),
            TransactionPhase::V0(
                vec![make_classic_component(vec![make_tx_envelope(2, 100)], None)]
                    .try_into()
                    .unwrap(),
            ),
            TransactionPhase::V0(
                vec![make_classic_component(vec![make_tx_envelope(3, 100)], None)]
                    .try_into()
                    .unwrap(),
            ),
        ]);
        let result = validate_generalized_tx_set_xdr_structure(&three_phases);
        assert!(result.is_err());
    }

    #[test]
    fn test_validate_generalized_tx_set_rejects_parallel_in_classic_phase() {
        // Phase 0 (classic) should not be V1 (parallel)
        let mut sorted_tx = vec![make_tx_envelope(1, 100)];
        sort_txs_by_hash(&mut sorted_tx);
        let gen = make_gen_tx_set(vec![
            TransactionPhase::V1(make_parallel_component(vec![vec![sorted_tx]], Some(100))),
            TransactionPhase::V0(
                vec![make_classic_component(vec![make_tx_envelope(2, 100)], None)]
                    .try_into()
                    .unwrap(),
            ),
        ]);
        let result = validate_generalized_tx_set_xdr_structure(&gen);
        assert!(result.is_err());
        assert!(result.unwrap_err().contains("Non-Soroban parallel phase"));
    }

    // =========================================================================
    // validate_parallel_component
    // =========================================================================

    #[test]
    fn test_validate_parallel_component_rejects_empty_stage() {
        let parallel = ParallelTxsComponent {
            base_fee: Some(100),
            execution_stages: vec![ParallelTxExecutionStage(vec![].try_into().unwrap())]
                .try_into()
                .unwrap(),
        };
        let result = validate_parallel_component(&parallel);
        assert!(result.is_err());
        assert!(result.unwrap_err().contains("Empty stage"));
    }

    #[test]
    fn test_validate_parallel_component_rejects_empty_cluster() {
        let parallel = ParallelTxsComponent {
            base_fee: Some(100),
            execution_stages: vec![ParallelTxExecutionStage(
                vec![DependentTxCluster(vec![].try_into().unwrap())]
                    .try_into()
                    .unwrap(),
            )]
            .try_into()
            .unwrap(),
        };
        let result = validate_parallel_component(&parallel);
        assert!(result.is_err());
        assert!(result.unwrap_err().contains("Empty cluster"));
    }

    #[test]
    fn test_validate_parallel_component_valid_single_stage() {
        let tx = make_tx_envelope(1, 100);
        let parallel = ParallelTxsComponent {
            base_fee: Some(100),
            execution_stages: vec![ParallelTxExecutionStage(
                vec![DependentTxCluster(vec![tx].try_into().unwrap())]
                    .try_into()
                    .unwrap(),
            )]
            .try_into()
            .unwrap(),
        };
        assert!(validate_parallel_component(&parallel).is_ok());
    }

    #[test]
    fn test_validate_parallel_component_rejects_unsorted_clusters() {
        let tx_a = make_tx_envelope(1, 100);
        let tx_b = make_tx_envelope(2, 100);
        let hash_a = Hash256::hash_xdr(&tx_a).unwrap_or_default();
        let hash_b = Hash256::hash_xdr(&tx_b).unwrap_or_default();

        let (first, second) = if hash_a.0 < hash_b.0 {
            (tx_b, tx_a)
        } else {
            (tx_a, tx_b)
        };

        let parallel = ParallelTxsComponent {
            base_fee: Some(100),
            execution_stages: vec![ParallelTxExecutionStage(
                vec![
                    DependentTxCluster(vec![first].try_into().unwrap()),
                    DependentTxCluster(vec![second].try_into().unwrap()),
                ]
                .try_into()
                .unwrap(),
            )]
            .try_into()
            .unwrap(),
        };

        let result = validate_parallel_component(&parallel);
        assert!(result.is_err());
        assert!(result
            .unwrap_err()
            .contains("Clusters within stage are not in canonical order"));
    }

    #[test]
    fn test_validate_parallel_component_rejects_unsorted_stages() {
        let tx_a = make_tx_envelope(1, 100);
        let tx_b = make_tx_envelope(2, 100);
        let hash_a = Hash256::hash_xdr(&tx_a).unwrap_or_default();
        let hash_b = Hash256::hash_xdr(&tx_b).unwrap_or_default();

        let (stage0_tx, stage1_tx) = if hash_a.0 < hash_b.0 {
            (tx_b, tx_a)
        } else {
            (tx_a, tx_b)
        };

        let parallel = ParallelTxsComponent {
            base_fee: Some(100),
            execution_stages: vec![
                ParallelTxExecutionStage(
                    vec![DependentTxCluster(vec![stage0_tx].try_into().unwrap())]
                        .try_into()
                        .unwrap(),
                ),
                ParallelTxExecutionStage(
                    vec![DependentTxCluster(vec![stage1_tx].try_into().unwrap())]
                        .try_into()
                        .unwrap(),
                ),
            ]
            .try_into()
            .unwrap(),
        };

        let result = validate_parallel_component(&parallel);
        assert!(result.is_err());
        assert!(result
            .unwrap_err()
            .contains("Stages are not in canonical order"));
    }

    // =========================================================================
    // TransactionSet construction and hash
    // =========================================================================

    #[test]
    fn test_transaction_set_new_sorts_transactions() {
        let tx1 = make_tx_envelope(5, 100);
        let tx2 = make_tx_envelope(1, 100);
        let tx3 = make_tx_envelope(3, 100);
        let tx_set = TransactionSet::new(Hash256::ZERO, vec![tx1, tx2, tx3]);

        // Verify transactions are sorted by hash after construction
        assert!(is_sorted_by_hash(&tx_set.transactions));
    }

    #[test]
    fn test_transaction_set_new_computes_deterministic_hash() {
        let tx1 = make_tx_envelope(1, 100);
        let tx2 = make_tx_envelope(2, 100);

        // Same inputs should produce same hash regardless of input order
        let set_a = TransactionSet::new(Hash256::ZERO, vec![tx1.clone(), tx2.clone()]);
        let set_b = TransactionSet::new(Hash256::ZERO, vec![tx2, tx1]);
        assert_eq!(set_a.hash, set_b.hash);
    }

    #[test]
    fn test_transaction_set_empty() {
        let tx_set = TransactionSet::new(Hash256::ZERO, vec![]);
        assert!(tx_set.is_empty());
        assert_eq!(tx_set.len(), 0);
    }

    #[test]
    fn test_transaction_set_recompute_hash_matches() {
        let tx_set = TransactionSet::new(
            Hash256::ZERO,
            vec![make_tx_envelope(1, 100), make_tx_envelope(2, 200)],
        );
        assert_eq!(tx_set.recompute_hash(), Some(tx_set.hash));
    }

    // --- Negative base fee rejection tests (parity: TxSetFrame.cpp:1442, 1480) ---

    #[test]
    fn test_validate_sequential_phase_rejects_negative_base_fee() {
        let components = vec![make_classic_component(
            vec![make_tx_envelope(1, 100)],
            Some(-1),
        )];
        let result = validate_sequential_phase_xdr_structure(&components);
        assert!(result.is_err());
        assert!(
            result.unwrap_err().contains("negative base fee"),
            "should reject negative base fee"
        );
    }

    #[test]
    fn test_validate_sequential_phase_accepts_zero_base_fee() {
        let components = vec![make_classic_component(
            vec![make_tx_envelope(1, 100)],
            Some(0),
        )];
        let result = validate_sequential_phase_xdr_structure(&components);
        assert!(result.is_ok());
    }

    #[test]
    fn test_validate_parallel_component_rejects_negative_base_fee() {
        let mut sorted_tx = vec![make_tx_envelope(1, 100)];
        sort_txs_by_hash(&mut sorted_tx);
        let parallel = make_parallel_component(vec![vec![sorted_tx]], Some(-5));
        let result = validate_parallel_component(&parallel);
        assert!(result.is_err());
        assert!(
            result.unwrap_err().contains("negative base fee"),
            "should reject negative base fee"
        );
    }

    #[test]
    fn test_validate_parallel_component_accepts_zero_base_fee() {
        let mut sorted_tx = vec![make_tx_envelope(1, 100)];
        sort_txs_by_hash(&mut sorted_tx);
        let parallel = make_parallel_component(vec![vec![sorted_tx]], Some(0));
        let result = validate_parallel_component(&parallel);
        assert!(result.is_ok());
    }

    #[test]
    fn test_validate_generalized_tx_set_rejects_negative_base_fee_in_classic_phase() {
        let mut sorted_tx = vec![make_tx_envelope(2, 200)];
        sort_txs_by_hash(&mut sorted_tx);
        let gen = make_gen_tx_set(vec![
            TransactionPhase::V0(
                vec![make_classic_component(
                    vec![make_tx_envelope(1, 100)],
                    Some(-10),
                )]
                .try_into()
                .unwrap(),
            ),
            TransactionPhase::V1(make_parallel_component(vec![vec![sorted_tx]], Some(100))),
        ]);
        let result = validate_generalized_tx_set_xdr_structure(&gen);
        assert!(result.is_err());
        assert!(result.unwrap_err().contains("negative base fee"));
    }
}
