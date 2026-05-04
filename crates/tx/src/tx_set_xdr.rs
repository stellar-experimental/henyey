//! Constructors for `ParallelTxsComponent` and parallel `TransactionPhase`.
//!
//! These functions enforce the stellar-core invariant from
//! `parallelPhaseToXdr()` (TxSetFrame.cpp:286-300): when `execution_stages`
//! is empty, `base_fee` must be `None`.
//!
//! Since `ParallelTxsComponent` is an external XDR type with public fields,
//! raw struct literals remain syntactically possible. Every remaining direct
//! construction should carry an `// Intentionally invalid` comment and exist
//! only in tests verifying validation of malformed inputs.

use stellar_xdr::curr::{ParallelTxExecutionStage, ParallelTxsComponent, TransactionPhase, VecM};

/// Construct a `ParallelTxsComponent` enforcing the base-fee/empty-stages
/// invariant (stellar-core `parallelPhaseToXdr()`, TxSetFrame.cpp:286-300).
///
/// # Panics
///
/// Panics if `execution_stages` is empty and `base_fee` is `Some(...)`.
///
/// # Scope
///
/// Enforces only the base-fee/empty-stages invariant. Does **not** validate
/// canonical ordering, non-empty stages/clusters, or fee sign — those are
/// enforced by `stages_to_xdr_phase()` (ordering) and
/// `validate_parallel_component()` (structure/fees).
pub fn new_parallel_txs_component(
    base_fee: Option<i64>,
    execution_stages: VecM<ParallelTxExecutionStage>,
) -> ParallelTxsComponent {
    assert!(
        !execution_stages.is_empty() || base_fee.is_none(),
        "empty execution_stages must not have Some(base_fee) \
         (stellar-core TxSetFrame.cpp:286-290)"
    );
    ParallelTxsComponent {
        base_fee,
        execution_stages,
    }
}

/// Build an empty parallel soroban phase (`base_fee: None`, no stages).
///
/// For protocol 23+ parallel execution phases only. Pre-parallel (protocol
/// 20–22) soroban phases use `TransactionPhase::V0`.
///
/// Matches stellar-core's behavior when no soroban transactions exist:
/// `parallelPhaseToXdr()` leaves `baseFee` as `nullopt` when
/// `inclusionFeeMap` is empty (TxSetFrame.cpp:286-290).
pub fn empty_soroban_phase() -> TransactionPhase {
    TransactionPhase::V1(new_parallel_txs_component(None, VecM::default()))
}

/// Build a parallel soroban phase with the given stages and optional base fee.
///
/// For protocol 23+ parallel execution phases only.
///
/// # Panics
///
/// Panics if `execution_stages` is empty — use [`empty_soroban_phase()`].
///
/// # Scope
///
/// Does **not** validate canonical ordering, non-empty stages/clusters, or
/// fee sign. Those are enforced by `stages_to_xdr_phase()` (ordering) and
/// `validate_parallel_component()` (structure/fees).
pub fn soroban_phase_with_stages(
    base_fee: Option<i64>,
    execution_stages: VecM<ParallelTxExecutionStage>,
) -> TransactionPhase {
    assert!(
        !execution_stages.is_empty(),
        "use empty_soroban_phase() for the zero-transaction case"
    );
    TransactionPhase::V1(new_parallel_txs_component(base_fee, execution_stages))
}

#[cfg(test)]
mod tests {
    use super::*;
    use stellar_xdr::curr::{DependentTxCluster, ParallelTxExecutionStage, TransactionEnvelope};

    fn make_dummy_stages() -> VecM<ParallelTxExecutionStage> {
        // A minimal non-empty execution_stages: one stage with one cluster
        // containing one (default) envelope.
        let envelope = TransactionEnvelope::default();
        let cluster = DependentTxCluster(vec![envelope].try_into().unwrap());
        let stage = ParallelTxExecutionStage(vec![cluster].try_into().unwrap());
        vec![stage].try_into().unwrap()
    }

    #[test]
    fn test_empty_soroban_phase() {
        let phase = empty_soroban_phase();
        match &phase {
            TransactionPhase::V1(component) => {
                assert!(component.base_fee.is_none());
                assert!(component.execution_stages.is_empty());
            }
            _ => panic!("expected V1 phase"),
        }
    }

    #[test]
    fn test_soroban_phase_with_stages_some_fee() {
        let stages = make_dummy_stages();
        let phase = soroban_phase_with_stages(Some(100), stages);
        match &phase {
            TransactionPhase::V1(component) => {
                assert_eq!(component.base_fee, Some(100));
                assert_eq!(component.execution_stages.len(), 1);
            }
            _ => panic!("expected V1 phase"),
        }
    }

    #[test]
    fn test_soroban_phase_with_stages_none_base_fee() {
        let stages = make_dummy_stages();
        let phase = soroban_phase_with_stages(None, stages);
        match &phase {
            TransactionPhase::V1(component) => {
                assert!(component.base_fee.is_none());
                assert_eq!(component.execution_stages.len(), 1);
            }
            _ => panic!("expected V1 phase"),
        }
    }

    #[test]
    #[should_panic(expected = "use empty_soroban_phase()")]
    fn test_soroban_phase_with_stages_panics_on_empty() {
        soroban_phase_with_stages(Some(100), VecM::default());
    }

    #[test]
    fn test_new_parallel_txs_component_empty_none() {
        let component = new_parallel_txs_component(None, VecM::default());
        assert!(component.base_fee.is_none());
        assert!(component.execution_stages.is_empty());
    }

    #[test]
    #[should_panic(expected = "empty execution_stages must not have Some(base_fee)")]
    fn test_new_parallel_txs_component_empty_some_panics() {
        new_parallel_txs_component(Some(100), VecM::default());
    }
}
