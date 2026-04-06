//! Resource estimation and adjustment: CPU/memory budgets, read/write bytes
//! computation, fee estimation, resource bumping.

use soroban_env_host_p25 as soroban_host;
use stellar_xdr::curr::{LedgerKey, OperationBody, SorobanResources};

/// Multiplicative adjustment factor for refundable fees (soroban-simulation default).
const REFUNDABLE_FEE_ADJUSTMENT_FACTOR: f64 = 1.15;

/// Default additive leeway applied to CPU instructions (soroban-simulation default).
const DEFAULT_INSTRUCTION_LEEWAY: u32 = 50_000;

/// Multiplicative factor for CPU instruction adjustment (soroban-simulation default).
const INSTRUCTION_ADJUSTMENT_FACTOR: f64 = 1.04;

/// Multiplicative factor for transaction-size adjustment (soroban-simulation default).
const TX_SIZE_ADJUSTMENT_FACTOR: f64 = 1.1;

/// Additive factor for transaction-size adjustment (soroban-simulation default).
const TX_SIZE_ADDITIVE_ADJUSTMENT: u32 = 500;

/// Fallback transaction size when XDR encoding fails.
const FALLBACK_TX_SIZE: u32 = 300;

/// Estimated number of signatures on the transaction envelope for size estimation.
const MAX_SIGNATURES_ESTIMATE: usize = 20;

/// Length of a single Ed25519 signature in bytes.
const SIGNATURE_LENGTH: usize = 64;

// ---------------------------------------------------------------------------
// Resource adjustments
// ---------------------------------------------------------------------------

/// Apply soroban-simulation default adjustment: max(x + additive, floor(x * mult)).
pub(super) fn sim_adjust(value: u32, multiplicative: f64, additive: u32) -> u32 {
    if value == 0 {
        return 0;
    }
    let mult_adjusted = (value as f64 * multiplicative).floor() as u32;
    (value.saturating_add(additive)).max(mult_adjusted)
}

/// Apply resource adjustment factors matching soroban-simulation defaults.
///
/// `instruction_leeway` comes from the `resourceConfig.instructionLeeway` request param.
/// The effective additive factor is `max(DEFAULT_INSTRUCTION_LEEWAY, instruction_leeway)`.
/// `disk_read_bytes` and `write_bytes` use `(1.0, 0)` (no additive adjustment) per upstream.
pub(super) fn adjust_resources(resources: &mut SorobanResources, instruction_leeway: u32) {
    let additive = DEFAULT_INSTRUCTION_LEEWAY.max(instruction_leeway);
    resources.instructions = sim_adjust(
        resources.instructions,
        INSTRUCTION_ADJUSTMENT_FACTOR,
        additive,
    );
    resources.disk_read_bytes = sim_adjust(resources.disk_read_bytes, 1.0, 0);
    resources.write_bytes = sim_adjust(resources.write_bytes, 1.0, 0);
}

// ---------------------------------------------------------------------------
// Transaction size estimation
// ---------------------------------------------------------------------------

/// Estimate the XDR-encoded transaction size for fee computation.
///
/// Mirrors soroban-simulation: builds a max-size synthetic envelope with
/// 20 signatures and full preconditions, then applies the tx_size adjustment.
pub(super) fn estimate_tx_size_for_op(
    operation: &OperationBody,
    resources: &SorobanResources,
) -> u32 {
    use stellar_xdr::curr::*;

    let soroban_data = SorobanTransactionData {
        ext: SorobanTransactionDataExt::V0,
        resources: SorobanResources {
            footprint: resources.footprint.clone(),
            instructions: 0,
            disk_read_bytes: 0,
            write_bytes: 0,
        },
        resource_fee: 0,
    };

    let sig = DecoratedSignature {
        hint: SignatureHint([0u8; 4]),
        signature: Signature::try_from(vec![0u8; SIGNATURE_LENGTH]).unwrap_or_default(),
    };
    let sigs: Vec<DecoratedSignature> = (0..MAX_SIGNATURES_ESTIMATE).map(|_| sig.clone()).collect();

    let source = MuxedAccount::MuxedEd25519(MuxedAccountMed25519 {
        id: 0,
        ed25519: Uint256([0u8; 32]),
    });

    let tx = Transaction {
        source_account: source.clone(),
        fee: u32::MAX,
        seq_num: SequenceNumber(0),
        cond: Preconditions::V2(PreconditionsV2 {
            time_bounds: Some(TimeBounds {
                min_time: TimePoint(0),
                max_time: TimePoint(0),
            }),
            ledger_bounds: Some(LedgerBounds {
                min_ledger: 0,
                max_ledger: 0,
            }),
            min_seq_num: Some(SequenceNumber(0)),
            min_seq_age: Duration(0),
            min_seq_ledger_gap: 0,
            extra_signers: vec![
                SignerKey::Ed25519(Uint256([0u8; 32])),
                SignerKey::Ed25519(Uint256([0u8; 32])),
            ]
            .try_into()
            .unwrap_or_default(),
        }),
        memo: Memo::None,
        operations: vec![Operation {
            source_account: Some(source),
            body: operation.clone(),
        }]
        .try_into()
        .unwrap_or_default(),
        ext: TransactionExt::V1(soroban_data),
    };

    let envelope = TransactionEnvelope::Tx(TransactionV1Envelope {
        tx,
        signatures: sigs.try_into().unwrap_or_default(),
    });

    let raw_size = envelope
        .to_xdr(Limits::none())
        .map(|b| b.len() as u32)
        .unwrap_or(FALLBACK_TX_SIZE);

    sim_adjust(
        raw_size,
        TX_SIZE_ADJUSTMENT_FACTOR,
        TX_SIZE_ADDITIVE_ADJUSTMENT,
    )
}

// ---------------------------------------------------------------------------
// Fee computation
// ---------------------------------------------------------------------------

/// Build `FeeConfiguration` from `SorobanNetworkInfo`.
fn build_fee_config(
    soroban_info: &henyey_ledger::SorobanNetworkInfo,
) -> soroban_host::fees::FeeConfiguration {
    soroban_host::fees::FeeConfiguration {
        fee_per_instruction_increment: soroban_info.fee_rate_per_instructions_increment,
        fee_per_disk_read_entry: soroban_info.fee_read_ledger_entry,
        fee_per_write_entry: soroban_info.fee_write_ledger_entry,
        fee_per_disk_read_1kb: soroban_info.fee_read_1kb,
        fee_per_write_1kb: soroban_info.fee_write_1kb,
        fee_per_historical_1kb: soroban_info.fee_historical_1kb,
        fee_per_contract_event_1kb: soroban_info.fee_contract_events_size_1kb,
        fee_per_transaction_size_1kb: soroban_info.fee_transaction_size_1kb,
    }
}

/// Build `RentFeeConfiguration` from `SorobanNetworkInfo`.
fn build_rent_fee_config(
    soroban_info: &henyey_ledger::SorobanNetworkInfo,
) -> soroban_host::fees::RentFeeConfiguration {
    use soroban_host::fees::{RentFeeConfiguration, RentWriteFeeConfiguration};

    let rent_write_config = RentWriteFeeConfiguration {
        state_target_size_bytes: soroban_info.state_target_size_bytes,
        rent_fee_1kb_state_size_low: soroban_info.rent_fee_1kb_state_size_low,
        rent_fee_1kb_state_size_high: soroban_info.rent_fee_1kb_state_size_high,
        state_size_rent_fee_growth_factor: soroban_info.state_size_rent_fee_growth_factor,
    };
    let fee_per_rent_1kb = soroban_host::fees::compute_rent_write_fee_per_1kb(
        soroban_info.average_bucket_list_size as i64,
        &rent_write_config,
    );

    RentFeeConfiguration {
        fee_per_write_1kb: soroban_info.fee_write_1kb,
        fee_per_rent_1kb,
        fee_per_write_entry: soroban_info.fee_write_ledger_entry,
        persistent_rent_rate_denominator: soroban_info.persistent_rent_rate_denominator,
        temporary_rent_rate_denominator: soroban_info.temp_rent_rate_denominator,
    }
}

/// Compute resource fee for InvokeHostFunction (includes rent).
pub(super) fn compute_invoke_resource_fee(
    resources: &SorobanResources,
    rent_changes: &[soroban_host::fees::LedgerEntryRentChange],
    soroban_info: &henyey_ledger::SorobanNetworkInfo,
    current_ledger_seq: u32,
    contract_events_and_return_value_size: u32,
    tx_size: u32,
    restored_entry_count: u32,
) -> i64 {
    // Compute disk_read_entries the same way as upstream soroban-simulation:
    // only non-Soroban entries (accounts, trustlines etc.) count, since Soroban
    // entries (ContractData/ContractCode) are cached in memory and don't require
    // disk reads. Auto-restored entries also count.
    let mut disk_read_entries = 0u32;
    for k in resources.footprint.read_only.iter() {
        match k {
            LedgerKey::ContractData(_) | LedgerKey::ContractCode(_) => (),
            _ => disk_read_entries += 1,
        }
    }
    for k in resources.footprint.read_write.iter() {
        match k {
            LedgerKey::ContractData(_) | LedgerKey::ContractCode(_) => (),
            _ => disk_read_entries += 1,
        }
    }
    disk_read_entries += restored_entry_count;

    compute_resource_fee_core(
        resources,
        disk_read_entries,
        rent_changes,
        soroban_info,
        current_ledger_seq,
        contract_events_and_return_value_size,
        tx_size,
    )
}

/// Compute resource fee for ExtendTTL/Restore operations (rent-dominant).
///
/// These operations have no instructions and no contract events, but do have
/// rent fees that dominate the cost.
pub(super) fn compute_resource_fee_with_rent(
    resources: &SorobanResources,
    rent_changes: &[soroban_host::fees::LedgerEntryRentChange],
    soroban_info: &henyey_ledger::SorobanNetworkInfo,
    current_ledger_seq: u32,
    contract_events_size: u32,
    tx_size: u32,
) -> i64 {
    let disk_read_entries =
        resources.footprint.read_only.len() as u32 + resources.footprint.read_write.len() as u32;

    compute_resource_fee_core(
        resources,
        disk_read_entries,
        rent_changes,
        soroban_info,
        current_ledger_seq,
        contract_events_size,
        tx_size,
    )
}

/// Shared fee assembly: build resources → compute tx fee → compute rent → adjust.
fn compute_resource_fee_core(
    resources: &SorobanResources,
    disk_read_entries: u32,
    rent_changes: &[soroban_host::fees::LedgerEntryRentChange],
    soroban_info: &henyey_ledger::SorobanNetworkInfo,
    current_ledger_seq: u32,
    contract_events_size_bytes: u32,
    tx_size: u32,
) -> i64 {
    use soroban_host::fees::{
        compute_rent_fee, compute_transaction_resource_fee, TransactionResources,
    };

    let tx_resources = TransactionResources {
        instructions: resources.instructions,
        disk_read_entries,
        write_entries: resources.footprint.read_write.len() as u32,
        disk_read_bytes: resources.disk_read_bytes,
        write_bytes: resources.write_bytes,
        contract_events_size_bytes,
        transaction_size_bytes: tx_size,
    };

    let fee_config = build_fee_config(soroban_info);
    let (non_refundable, refundable) = compute_transaction_resource_fee(&tx_resources, &fee_config);

    let rent_fee = compute_rent_fee(
        rent_changes,
        &build_rent_fee_config(soroban_info),
        current_ledger_seq,
    );

    let total_refundable = refundable.saturating_add(rent_fee);
    let adjusted_refundable = if total_refundable > 0 {
        ((total_refundable as f64) * REFUNDABLE_FEE_ADJUSTMENT_FACTOR).floor() as i64
    } else {
        0
    };
    non_refundable.saturating_add(adjusted_refundable)
}

// ===========================================================================
// Tests
// ===========================================================================

#[cfg(test)]
mod tests {
    use super::*;
    use stellar_xdr::curr::*;

    fn test_soroban_network_info() -> henyey_ledger::SorobanNetworkInfo {
        henyey_ledger::SorobanNetworkInfo {
            max_contract_size: 65536,
            max_contract_data_key_size: 250,
            max_contract_data_entry_size: 65536,
            tx_max_instructions: 100_000_000,
            ledger_max_instructions: 2_500_000_000,
            fee_rate_per_instructions_increment: 25,
            tx_memory_limit: 41943040,
            ledger_max_read_ledger_entries: 200,
            ledger_max_read_bytes: 200_000,
            ledger_max_write_ledger_entries: 150,
            ledger_max_write_bytes: 65536,
            tx_max_read_ledger_entries: 40,
            tx_max_read_bytes: 200_000,
            tx_max_write_ledger_entries: 25,
            tx_max_write_bytes: 65536,
            fee_read_ledger_entry: 6250,
            fee_write_ledger_entry: 10000,
            fee_read_1kb: 1786,
            fee_write_1kb: 11800,
            fee_historical_1kb: 16235,
            tx_max_contract_events_size_bytes: 8198,
            fee_contract_events_size_1kb: 10000,
            ledger_max_tx_size_bytes: 71680,
            tx_max_size_bytes: 71680,
            fee_transaction_size_1kb: 1624,
            ledger_max_tx_count: 150,
            max_entry_ttl: 6_312_000,
            min_temporary_ttl: 17280,
            min_persistent_ttl: 120960,
            persistent_rent_rate_denominator: 2103840,
            temp_rent_rate_denominator: 4096,
            max_entries_to_archive: 100,
            bucketlist_size_window_sample_size: 30,
            eviction_scan_size: 100000,
            starting_eviction_scan_level: 7,
            average_bucket_list_size: 100_000_000,
            state_target_size_bytes: 134217728,
            rent_fee_1kb_state_size_low: 1000,
            rent_fee_1kb_state_size_high: 100000000,
            state_size_rent_fee_growth_factor: 1000,
            nomination_timeout_initial_ms: 1000,
            nomination_timeout_increment_ms: 500,
            ballot_timeout_initial_ms: 1000,
            ballot_timeout_increment_ms: 500,
        }
    }

    fn test_account_key(key_byte: u8) -> LedgerKey {
        LedgerKey::Account(LedgerKeyAccount {
            account_id: AccountId(PublicKey::PublicKeyTypeEd25519(Uint256([key_byte; 32]))),
        })
    }

    fn test_contract_data_key(contract_byte: u8) -> LedgerKey {
        LedgerKey::ContractData(LedgerKeyContractData {
            contract: ScAddress::Contract(ContractId(Hash([contract_byte; 32]))),
            key: ScVal::LedgerKeyContractInstance,
            durability: ContractDataDurability::Persistent,
        })
    }

    fn test_soroban_resources(ro: Vec<LedgerKey>, rw: Vec<LedgerKey>) -> SorobanResources {
        SorobanResources {
            footprint: LedgerFootprint {
                read_only: ro.try_into().unwrap_or_default(),
                read_write: rw.try_into().unwrap_or_default(),
            },
            instructions: 1_000_000,
            disk_read_bytes: 5000,
            write_bytes: 2000,
        }
    }

    // -----------------------------------------------------------------------
    // B1. sim_adjust (5 tests)
    // -----------------------------------------------------------------------

    #[test]
    fn test_sim_adjust_zero_returns_zero() {
        assert_eq!(sim_adjust(0, 1.04, 50_000), 0);
    }

    #[test]
    fn test_sim_adjust_additive_dominates() {
        // 100_000 + 50_000 = 150_000 vs floor(100_000 * 1.04) = 104_000
        // max(150_000, 104_000) = 150_000
        assert_eq!(sim_adjust(100_000, 1.04, 50_000), 150_000);
    }

    #[test]
    fn test_sim_adjust_multiplicative_dominates() {
        // 10_000_000 + 50_000 = 10_050_000 vs floor(10_000_000 * 1.04) = 10_400_000
        // max(10_050_000, 10_400_000) = 10_400_000
        assert_eq!(sim_adjust(10_000_000, 1.04, 50_000), 10_400_000);
    }

    #[test]
    fn test_sim_adjust_no_adjustment() {
        assert_eq!(sim_adjust(500, 1.0, 0), 500);
    }

    #[test]
    fn test_sim_adjust_saturating() {
        // Should saturate to u32::MAX instead of overflowing
        assert_eq!(sim_adjust(u32::MAX - 10, 1.04, 50_000), u32::MAX);
    }

    // -----------------------------------------------------------------------
    // B2. adjust_resources (3 tests)
    // -----------------------------------------------------------------------

    #[test]
    fn test_adjust_resources_default() {
        let mut resources = SorobanResources {
            footprint: LedgerFootprint {
                read_only: Default::default(),
                read_write: Default::default(),
            },
            instructions: 1_000_000,
            disk_read_bytes: 5000,
            write_bytes: 2000,
        };
        adjust_resources(&mut resources, 0);

        // instructions: max(1_000_000 + 50_000, floor(1_000_000 * 1.04)) = max(1_050_000, 1_040_000) = 1_050_000
        assert_eq!(resources.instructions, 1_050_000);
        // disk_read_bytes: sim_adjust(5000, 1.0, 0) = 5000
        assert_eq!(resources.disk_read_bytes, 5000);
        // write_bytes: sim_adjust(2000, 1.0, 0) = 2000
        assert_eq!(resources.write_bytes, 2000);
    }

    #[test]
    fn test_adjust_resources_custom_leeway() {
        let mut resources = SorobanResources {
            footprint: LedgerFootprint {
                read_only: Default::default(),
                read_write: Default::default(),
            },
            instructions: 1_000_000,
            disk_read_bytes: 0,
            write_bytes: 0,
        };
        adjust_resources(&mut resources, 200_000);

        // additive = max(50_000, 200_000) = 200_000
        // instructions: max(1_000_000 + 200_000, floor(1_000_000 * 1.04)) = max(1_200_000, 1_040_000) = 1_200_000
        assert_eq!(resources.instructions, 1_200_000);
    }

    #[test]
    fn test_adjust_resources_zero_values() {
        let mut resources = SorobanResources {
            footprint: LedgerFootprint {
                read_only: Default::default(),
                read_write: Default::default(),
            },
            instructions: 0,
            disk_read_bytes: 0,
            write_bytes: 0,
        };
        adjust_resources(&mut resources, 0);

        assert_eq!(resources.instructions, 0);
        assert_eq!(resources.disk_read_bytes, 0);
        assert_eq!(resources.write_bytes, 0);
    }

    // -----------------------------------------------------------------------
    // B11. compute_invoke_resource_fee (4 tests) [REGRESSION]
    // -----------------------------------------------------------------------

    #[test]
    fn test_disk_read_entries_excludes_soroban() {
        // 1 Account (RO) + 1 ContractData (RO) -> disk_read_entries = 1
        let resources = test_soroban_resources(
            vec![test_account_key(1), test_contract_data_key(0xAA)],
            vec![],
        );
        let info = test_soroban_network_info();
        let fee = compute_invoke_resource_fee(&resources, &[], &info, 100, 0, 1000, 0);
        // Fee should be > 0
        assert!(fee > 0);
        // The fee with only 1 disk read entry should be less than with 2
        let resources2 =
            test_soroban_resources(vec![test_account_key(1), test_account_key(2)], vec![]);
        let fee2 = compute_invoke_resource_fee(&resources2, &[], &info, 100, 0, 1000, 0);
        // 2 account entries = 2 disk reads, should cost more
        assert!(
            fee2 > fee,
            "2 account entries should cost more than 1 account + 1 contract"
        );
    }

    #[test]
    fn test_disk_read_entries_includes_restored() {
        let resources = test_soroban_resources(vec![], vec![]);
        let info = test_soroban_network_info();
        let fee_no_restore = compute_invoke_resource_fee(&resources, &[], &info, 100, 0, 1000, 0);
        let fee_with_restore = compute_invoke_resource_fee(&resources, &[], &info, 100, 0, 1000, 3);
        // 3 restored entries should add disk read cost
        assert!(fee_with_restore > fee_no_restore);
    }

    #[test]
    fn test_disk_read_entries_mixed() {
        // 2 accounts RO + 1 contract RW + 1 restored -> disk_read_entries = 2 + 0 + 1 = 3
        let resources = test_soroban_resources(
            vec![test_account_key(1), test_account_key(2)],
            vec![test_contract_data_key(0xCC)],
        );
        let info = test_soroban_network_info();
        let fee = compute_invoke_resource_fee(&resources, &[], &info, 100, 0, 1000, 1);
        assert!(fee > 0);
    }

    #[test]
    fn test_refundable_fee_adjustment() {
        // With non-zero rent changes, the refundable portion should be scaled by 1.15
        use soroban_host::fees::LedgerEntryRentChange;

        let resources = test_soroban_resources(vec![], vec![test_contract_data_key(0xAA)]);
        let info = test_soroban_network_info();

        let rent_changes = vec![LedgerEntryRentChange {
            is_persistent: true,
            is_code_entry: false,
            old_size_bytes: 100,
            new_size_bytes: 100,
            old_live_until_ledger: 100,
            new_live_until_ledger: 200,
        }];

        let fee = compute_invoke_resource_fee(&resources, &rent_changes, &info, 50, 0, 1000, 0);
        // Fee should be positive and include rent
        assert!(fee > 0);
    }

    // -----------------------------------------------------------------------
    // B12. estimate_tx_size (2 tests)
    // -----------------------------------------------------------------------

    #[test]
    fn test_tx_size_invoke_reasonable() {
        let op = OperationBody::InvokeHostFunction(InvokeHostFunctionOp {
            host_function: HostFunction::InvokeContract(InvokeContractArgs {
                contract_address: ScAddress::Contract(ContractId(Hash([0xAA; 32]))),
                function_name: ScSymbol("hello".try_into().unwrap()),
                args: Default::default(),
            }),
            auth: Default::default(),
        });
        let resources = test_soroban_resources(vec![], vec![]);
        let size = estimate_tx_size_for_op(&op, &resources);
        assert!(size > 0);
        // Reasonable: a minimal invoke tx with 20 sigs + preconditions should be > 1000 bytes
        assert!(
            size > 1000,
            "estimate should include 20 sigs overhead, got {size}"
        );
    }

    #[test]
    fn test_tx_size_extend_ttl() {
        let op = OperationBody::ExtendFootprintTtl(ExtendFootprintTtlOp {
            ext: ExtensionPoint::V0,
            extend_to: 5000,
        });
        let resources = test_soroban_resources(vec![test_contract_data_key(0xAA)], vec![]);
        let size = estimate_tx_size_for_op(&op, &resources);
        assert!(size > 0);
    }
}
