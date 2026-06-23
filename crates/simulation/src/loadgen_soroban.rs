//! Soroban transaction building utilities for load generation.
//!
//! Provides builders for constructing Soroban `TransactionEnvelope`s
//! (upload WASM, create contract, invoke contract) with correct
//! `SorobanTransactionData` extensions.

use henyey_common::{Hash256, NetworkId};
use henyey_crypto::{sign_hash, SecretKey};
use stellar_xdr::{
    ContractDataDurability, ContractExecutable, ContractId, ContractIdPreimage,
    ContractIdPreimageFromAddress, CreateContractArgs, DecoratedSignature, Hash, HashIdPreimage,
    HashIdPreimageContractId, HostFunction, Int128Parts, InvokeContractArgs, InvokeHostFunctionOp,
    LedgerFootprint, LedgerKey, LedgerKeyContractCode, LedgerKeyContractData, Limits, Memo,
    MuxedAccount, Operation, OperationBody, Preconditions, ScAddress, ScBytes, ScSymbol, ScVal,
    ScVec, SequenceNumber, Signature, SignatureHint, SorobanAuthorizationEntry,
    SorobanAuthorizedFunction, SorobanAuthorizedInvocation, SorobanCredentials, SorobanResources,
    SorobanResourcesExtV0, SorobanTransactionData, SorobanTransactionDataExt, Transaction,
    TransactionEnvelope, TransactionExt, TransactionV1Envelope, Uint256, VecM, WriteXdr,
};

use crate::loadgen::{sample_discrete, ContractInstance};
use rand::Rng;

/// Embedded loadgen test contract WASM (from stellar-core P21 test wasms).
///
/// This contract exposes `do_work(guest_cycles, host_cycles, n_entries, kb_per_entry)`
/// for CPU and IO load generation.
pub(crate) const LOADGEN_WASM: &[u8] = include_bytes!("../wasm/loadgen.wasm");

/// The `write_bytes` test contract (soroban-test-wasms `WRITE_BYTES`), used by
/// the config-upgrade setup. Its `write` function stores the passed bytes in a
/// TEMPORARY `ContractData` entry keyed by `SCV_BYTES(sha256(bytes))` — exactly
/// the `ConfigUpgradeSet` entry the create-upgrade flow then arms against.
///
/// Parity: stellar-core uploads `rust_bridge::get_write_bytes()` (not the
/// loadgen contract) for the upgrade-setup path (`LoadGenerator.cpp:1184`).
pub(crate) const WRITE_UPGRADE_BYTES_WASM: &[u8] =
    include_bytes!("../wasm/soroban_write_upgrade_bytes_contract.wasm");

// ---------------------------------------------------------------------------
// Resource estimate constants for Soroban transaction building.
// These are generous defaults matching stellar-core's `TxGenerator` estimates.
// ---------------------------------------------------------------------------

/// CPU instructions budget for uploading a WASM blob.
const UPLOAD_WASM_INSTRUCTIONS: u32 = 2_500_000;

/// Padding added to the raw WASM size for disk-read and write-byte estimates.
const WASM_SIZE_PADDING: u32 = 500;

/// Generous resource-fee estimate for WASM upload transactions (stroops).
const UPLOAD_WASM_RESOURCE_FEE: i64 = 50_000_000;

/// CPU instructions budget for contract creation.
const CREATE_CONTRACT_INSTRUCTIONS: u32 = 1_000_000;

/// Disk-read bytes estimate for contract creation.
const CREATE_CONTRACT_READ_BYTES: u32 = 5_000;

/// Write bytes estimate for contract creation.
const CREATE_CONTRACT_WRITE_BYTES: u32 = 300;

/// Generous resource-fee estimate for contract creation (stroops).
const CREATE_CONTRACT_RESOURCE_FEE: i64 = 10_000_000;

/// Generous resource-fee estimate for invoke / batch transactions (stroops).
const INVOKE_RESOURCE_FEE: i64 = 50_000_000;

/// CPU instructions budget for SAC transfer invocations.
///
/// stellar-core uses 250K instructions but our non-typed host API (P25)
/// meters XDR deserialization, consuming ~263K+ for a SAC transfer.
/// Use 2M to avoid ResourceLimitExceeded in load tests.
const SAC_TRANSFER_INSTRUCTIONS: u32 = 2_000_000;

/// Disk-read bytes estimate for SAC transfer invocations.
const SAC_TRANSFER_READ_BYTES: u32 = 10_000;

/// Write bytes estimate for SAC transfer invocations.
const SAC_TRANSFER_WRITE_BYTES: u32 = 10_000;

/// Per-transfer CPU instructions budget for batch transfers.
const BATCH_TRANSFER_INSTRUCTIONS_PER_ITEM: u32 = 500_000;

/// Per-transfer disk-read bytes for batch transfers.
const BATCH_TRANSFER_READ_BYTES_PER_ITEM: u32 = 800;

/// Per-transfer write bytes for batch transfers.
const BATCH_TRANSFER_WRITE_BYTES_PER_ITEM: u32 = 800;

/// CPU instructions budget for the config-upgrade `write` invocation.
///
/// Matches stellar-core `TxGenerator::invokeSorobanCreateUpgradeTransaction`'s
/// default `resources->instructions` (TxGenerator.cpp:1251).
const CREATE_UPGRADE_INSTRUCTIONS: u32 = 2_500_000;

/// Disk-read bytes for the config-upgrade `write` invocation
/// (stellar-core default `diskReadBytes`).
const CREATE_UPGRADE_READ_BYTES: u32 = 3_100;

/// Write bytes for the config-upgrade `write` invocation
/// (stellar-core default `writeBytes`).
const CREATE_UPGRADE_WRITE_BYTES: u32 = 3_100;

/// Fixed resource fee (stroops) for the config-upgrade `write` invocation.
///
/// stellar-core computes `sorobanResourceFee(app, resources, 1000, 40) +
/// 20'000'000` (TxGenerator.cpp:1251). henyey does not expose a public
/// `sorobanResourceFee`-equivalent over raw `SorobanResources` from the
/// simulation crate, so — consistent with the other generous fixed fee
/// constants in this module (`INVOKE_RESOURCE_FEE`, `UPLOAD_WASM_RESOURCE_FEE`)
/// — we use a generous fixed value that comfortably covers the resources above
/// plus the `+20'000'000` upgrade-tx headroom. The resource fee is a
/// fee-bound, not part of the upgrade-set content hash (the parity-critical
/// quantity), so an over-estimate is acceptable for load generation.
const CREATE_UPGRADE_RESOURCE_FEE: i64 = 50_000_000;

// ---------------------------------------------------------------------------
// Apply-load (V2) tuned instruction model.
//
// Faithful port of stellar-core `TxGenerator::invokeSorobanLoadTransactionV2`
// (TxGenerator.cpp:551). These constants are the load-bearing parity
// quantities — the random draws are non-consensus, but the instruction model
// the draws feed into must match stellar-core exactly.
// ---------------------------------------------------------------------------

/// Base instruction count for the V2 apply-load invocation
/// (`baseInstructionCount`, TxGenerator.cpp:558).
const APPLY_LOAD_BASE_INSTRUCTIONS: u32 = 737_119;

/// Baseline transaction size in bytes used to compute padding overhead
/// (`baselineTxSizeBytes`, TxGenerator.cpp:559).
const APPLY_LOAD_BASELINE_TX_SIZE_BYTES: u32 = 256;

/// Per-event size in bytes (`SOROBAN_LOAD_V2_EVENT_SIZE_BYTES`, TxGenerator.h:103).
const APPLY_LOAD_EVENT_SIZE_BYTES: u32 = 80;

/// Instructions modeled per guest CPU cycle (`instructionsPerGuestCycle`).
const APPLY_LOAD_INSTRUCTIONS_PER_GUEST_CYCLE: u32 = 40;

/// Instructions modeled per host CPU cycle (`instructionsPerHostCycle`).
/// Kept for parity documentation; the V2 path deliberately uses guest-only
/// cycles (see TxGenerator.cpp:702), so host cycles are always 0.
#[allow(dead_code)]
const APPLY_LOAD_INSTRUCTIONS_PER_HOST_CYCLE: u32 = 4_875;

/// Instructions modeled per byte of auth (padding) payload
/// (`instructionsPerAuthByte`).
const APPLY_LOAD_INSTRUCTIONS_PER_AUTH_BYTE: u32 = 35;

/// Instructions modeled per emitted event (`instructionsPerEvent`).
const APPLY_LOAD_INSTRUCTIONS_PER_EVENT: u32 = 8_500;

/// Instructions modeled per byte written to entries (`instructionsPerEntryByte`).
const APPLY_LOAD_INSTRUCTIONS_PER_ENTRY_BYTE: u32 = 44;

// Storage-instruction quadratic coefficients (TxGenerator.cpp:677):
// instructionsForEntries = 205*n^2 + 12000*n + 65485.
const APPLY_LOAD_STORAGE_QUADRATIC_A: u32 = 205;
const APPLY_LOAD_STORAGE_QUADRATIC_B: u32 = 12_000;
const APPLY_LOAD_STORAGE_QUADRATIC_C: u32 = 65_485;

// ---------------------------------------------------------------------------
// Apply-load (V2) tx construction inputs/outputs
// ---------------------------------------------------------------------------

/// Sampling inputs for [`SorobanTxBuilder::invoke_soroban_apply_load_tx`].
///
/// Each `(values, weights)` pair is a discrete distribution sampled via
/// [`crate::loadgen::sample_discrete`] (empty `values` ⇒ default `0`). The
/// scalar fields come from the `APPLY_LOAD_BL_*` / `APPLY_LOAD_DATA_ENTRY_SIZE`
/// config and the dispatch in `LoadGenerator` (LoadGenerator.cpp:785).
pub struct ApplyLoadTxParams {
    /// `APPLY_LOAD_BL_BATCH_SIZE * APPLY_LOAD_BL_SIMULATED_LEDGERS`.
    pub data_entry_count: u64,
    /// `APPLY_LOAD_DATA_ENTRY_SIZE` (already rounded to a multiple of 4).
    pub data_entry_size: u32,
    /// `APPLY_LOAD_NUM_RW_ENTRIES[_DISTRIBUTION]`.
    pub num_rw_entries: (Vec<u32>, Vec<u32>),
    /// `APPLY_LOAD_NUM_DISK_READ_ENTRIES[_DISTRIBUTION]`.
    pub num_disk_read_entries: (Vec<u32>, Vec<u32>),
    /// `APPLY_LOAD_TX_SIZE_BYTES[_DISTRIBUTION]`.
    pub tx_size_bytes: (Vec<u32>, Vec<u32>),
    /// `APPLY_LOAD_EVENT_COUNT[_DISTRIBUTION]`.
    pub event_count: (Vec<u32>, Vec<u32>),
    /// `APPLY_LOAD_INSTRUCTIONS[_DISTRIBUTION]`.
    pub instructions: (Vec<u32>, Vec<u32>),
    /// Number of pre-populated hot-archive entries (mirrors
    /// `TxGenerator::mPrePopulatedArchivedEntries`). When 0, the autorestore
    /// branch is dormant — exactly as stellar-core.
    pub pre_populated_archived_entries: u32,
}

/// Result of building a V2 apply-load tx.
pub struct ApplyLoadBuiltTx {
    /// The signed transaction envelope.
    pub envelope: TransactionEnvelope,
    /// Number of archived entries simulated for autorestore (0 when dormant).
    pub archived_entries_restored: u32,
}

// ---------------------------------------------------------------------------
// SorobanTxBuilder
// ---------------------------------------------------------------------------

/// Fluent builder for Soroban `TransactionEnvelope`s with correct
/// `SorobanTransactionData` extensions.
///
/// Mirrors the patterns in stellar-core's `TxGenerator` for constructing
/// Soroban transactions with proper footprints, resources, and fees.
pub struct SorobanTxBuilder {
    network_passphrase: String,
}

pub struct ContractInvocation {
    pub contract_id: Hash256,
    pub function_name: String,
    pub args: Vec<ScVal>,
    pub read_only_keys: Vec<LedgerKey>,
    pub read_write_keys: Vec<LedgerKey>,
    pub instructions: u32,
    pub read_bytes: u32,
    pub write_bytes: u32,
    pub inclusion_fee: u32,
}

pub struct SacTransfer {
    pub contract_id: Hash256,
    pub from_address: ScAddress,
    pub to_address: ScAddress,
    pub amount: i128,
    pub instance_keys: Vec<LedgerKey>,
    pub inclusion_fee: u32,
}

pub struct BatchTransfer {
    pub contract_id: Hash256,
    pub sac_address: ScVal,
    pub destinations: Vec<ScVal>,
    pub instance_keys: Vec<LedgerKey>,
    pub inclusion_fee: u32,
}

impl SorobanTxBuilder {
    pub fn new(network_passphrase: String) -> Self {
        Self { network_passphrase }
    }

    /// Build a WASM upload transaction.
    ///
    /// Matches stellar-core `TxGenerator::createUploadWasmTransaction()`.
    pub fn upload_wasm_tx(
        &self,
        source: &SecretKey,
        sequence: i64,
        wasm: &[u8],
        inclusion_fee: u32,
    ) -> anyhow::Result<TransactionEnvelope> {
        let wasm_hash = Hash256::hash(wasm);
        let code_key = LedgerKey::ContractCode(LedgerKeyContractCode {
            hash: Hash(wasm_hash.0),
        });

        let host_fn = HostFunction::UploadContractWasm(
            wasm.to_vec()
                .try_into()
                .map_err(|_| anyhow::anyhow!("wasm too large"))?,
        );

        let op = Operation {
            source_account: None,
            body: OperationBody::InvokeHostFunction(InvokeHostFunctionOp {
                host_function: host_fn,
                auth: VecM::default(),
            }),
        };

        let resources = SorobanResources {
            footprint: LedgerFootprint {
                read_only: VecM::default(),
                read_write: vec![code_key].try_into().unwrap_or_default(),
            },
            instructions: UPLOAD_WASM_INSTRUCTIONS,
            disk_read_bytes: (wasm.len() as u32).saturating_add(WASM_SIZE_PADDING),
            write_bytes: (wasm.len() as u32).saturating_add(WASM_SIZE_PADDING),
        };

        let resource_fee = UPLOAD_WASM_RESOURCE_FEE;

        self.build_soroban_envelope(source, sequence, op, resources, resource_fee, inclusion_fee)
    }

    /// Build a contract creation transaction.
    ///
    /// Matches stellar-core `TxGenerator::createContractTransaction()`.
    pub fn create_contract_tx(
        &self,
        source: &SecretKey,
        sequence: i64,
        wasm_hash: &Hash256,
        salt: &Uint256,
        contract_overhead_bytes: u32,
        inclusion_fee: u32,
    ) -> anyhow::Result<TransactionEnvelope> {
        let deployer_address = ScAddress::Account(stellar_xdr::AccountId(
            stellar_xdr::PublicKey::PublicKeyTypeEd25519(Uint256(*source.public_key().as_bytes())),
        ));

        let preimage = ContractIdPreimage::Address(ContractIdPreimageFromAddress {
            address: deployer_address.clone(),
            salt: salt.clone(),
        });

        let executable = ContractExecutable::Wasm(Hash(wasm_hash.0));

        let create_args = CreateContractArgs {
            contract_id_preimage: preimage.clone(),
            executable: executable.clone(),
        };

        let host_fn = HostFunction::CreateContract(create_args.clone());

        // Compute the contract ID for the footprint
        let contract_id = compute_contract_id(&preimage, &self.network_passphrase)?;

        let code_key = LedgerKey::ContractCode(LedgerKeyContractCode {
            hash: Hash(wasm_hash.0),
        });
        let instance_key = contract_instance_key(&contract_id);

        // Auth entry for the deployer
        let auth = SorobanAuthorizationEntry {
            credentials: SorobanCredentials::SourceAccount,
            root_invocation: SorobanAuthorizedInvocation {
                function: SorobanAuthorizedFunction::CreateContractHostFn(create_args),
                sub_invocations: VecM::default(),
            },
        };

        let op = Operation {
            source_account: None,
            body: OperationBody::InvokeHostFunction(InvokeHostFunctionOp {
                host_function: host_fn,
                auth: vec![auth].try_into().unwrap_or_default(),
            }),
        };

        let resources = SorobanResources {
            footprint: LedgerFootprint {
                read_only: vec![code_key].try_into().unwrap_or_default(),
                read_write: vec![instance_key].try_into().unwrap_or_default(),
            },
            instructions: CREATE_CONTRACT_INSTRUCTIONS,
            // Parity: stellar-core `createContractTransaction` sets
            // `diskReadBytes = contractOverheadBytes` (TxGenerator.cpp:333),
            // i.e. the uploaded WASM size + overhead (`mContactOverheadBytes =
            // wasmBytes.size() + 160`, LoadGenerator.cpp:1198). The deploy reads
            // the contract-code (WASM) entry, so the read budget must track the
            // actual WASM size. A fixed over-estimate (the old hardcoded 5000)
            // EXCEEDS the genesis Soroban `ledger_max_read_bytes` (3200), so the
            // deploy tx can never fit a tx set's Soroban lane — silently wedging
            // the root-account upgrade-setup sequence (deploy → create_upgrade).
            disk_read_bytes: contract_overhead_bytes,
            write_bytes: CREATE_CONTRACT_WRITE_BYTES,
        };

        let resource_fee = CREATE_CONTRACT_RESOURCE_FEE;
        self.build_soroban_envelope(source, sequence, op, resources, resource_fee, inclusion_fee)
    }

    /// Build a contract invocation transaction.
    ///
    /// Matches stellar-core `TxGenerator::invokeSorobanLoadTransaction()`.
    pub fn invoke_contract_tx(
        &self,
        source: &SecretKey,
        sequence: i64,
        invocation: ContractInvocation,
    ) -> anyhow::Result<TransactionEnvelope> {
        let contract_address = make_contract_address(&invocation.contract_id);

        let host_fn = HostFunction::InvokeContract(InvokeContractArgs {
            contract_address,
            function_name: ScSymbol(
                invocation
                    .function_name
                    .try_into()
                    .map_err(|_| anyhow::anyhow!("function name too long"))?,
            ),
            args: invocation.args.try_into().unwrap_or_default(),
        });

        let op = Operation {
            source_account: None,
            body: OperationBody::InvokeHostFunction(InvokeHostFunctionOp {
                host_function: host_fn,
                auth: VecM::default(),
            }),
        };

        let resources = SorobanResources {
            footprint: LedgerFootprint {
                read_only: invocation.read_only_keys.try_into().unwrap_or_default(),
                read_write: invocation.read_write_keys.try_into().unwrap_or_default(),
            },
            instructions: invocation.instructions,
            disk_read_bytes: invocation.read_bytes,
            write_bytes: invocation.write_bytes,
        };

        let resource_fee = INVOKE_RESOURCE_FEE;
        self.build_soroban_envelope(
            source,
            sequence,
            op,
            resources,
            resource_fee,
            invocation.inclusion_fee,
        )
    }

    /// Build a config-upgrade `write` invocation transaction.
    ///
    /// Faithful port of stellar-core
    /// `TxGenerator::invokeSorobanCreateUpgradeTransaction` (TxGenerator.cpp:1251):
    ///
    /// - op = `INVOKE_CONTRACT` on the upgrade contract, function `"write"`,
    ///   single arg `SCV_BYTES(upgradeBytes)`;
    /// - footprint readOnly = `[instanceKey, codeKey]` (order matters);
    /// - footprint readWrite = `[temp CONTRACT_DATA]` whose contract is the
    ///   upgrade contract and whose key is
    ///   `SCV_BYTES(xdr_to_opaque(sha256(upgradeBytes)))` — i.e. the raw 32-byte
    ///   content hash (XDR fixed-opaque encoding of a `Hash` is the 32 bytes
    ///   verbatim);
    /// - resources insns=2_500_000 / diskRead=3_100 / write=3_100.
    ///
    /// `code_key` and `instance_key` come from a prior `upgrade_setup` run.
    pub fn invoke_soroban_create_upgrade_tx(
        &self,
        source: &SecretKey,
        sequence: i64,
        upgrade_bytes: &[u8],
        code_key: LedgerKey,
        instance_key: LedgerKey,
        inclusion_fee: u32,
    ) -> anyhow::Result<TransactionEnvelope> {
        // The upgrade contract address is taken from the instance key.
        let contract_address = match &instance_key {
            LedgerKey::ContractData(cd) => cd.contract.clone(),
            _ => anyhow::bail!("instance_key must be a CONTRACT_DATA key"),
        };

        // Temp CONTRACT_DATA entry keyed by SCV_BYTES(xdr_to_opaque(sha256)).
        // XDR fixed-opaque encoding of a 32-byte `Hash` is the 32 bytes
        // verbatim, so the key bytes are the raw content hash.
        let upgrade_hash = Hash256::hash(upgrade_bytes);
        let key_bytes =
            ScVal::Bytes(
                upgrade_hash.0.to_vec().try_into().map_err(|_| {
                    anyhow::anyhow!("upgrade content hash exceeds SCV_BYTES capacity")
                })?,
            );
        let upgrade_lk = LedgerKey::ContractData(LedgerKeyContractData {
            contract: contract_address.clone(),
            key: key_bytes,
            durability: ContractDataDurability::Temporary,
        });

        let arg = ScVal::Bytes(
            upgrade_bytes
                .to_vec()
                .try_into()
                .map_err(|_| anyhow::anyhow!("upgrade bytes exceed SCV_BYTES capacity"))?,
        );

        let host_fn = HostFunction::InvokeContract(InvokeContractArgs {
            contract_address,
            function_name: ScSymbol("write".try_into().unwrap()),
            args: vec![arg].try_into().unwrap_or_default(),
        });

        let op = Operation {
            source_account: None,
            body: OperationBody::InvokeHostFunction(InvokeHostFunctionOp {
                host_function: host_fn,
                auth: VecM::default(),
            }),
        };

        let resources = SorobanResources {
            footprint: LedgerFootprint {
                // Order matters: [instance, code].
                read_only: vec![instance_key, code_key].try_into().unwrap_or_default(),
                read_write: vec![upgrade_lk].try_into().unwrap_or_default(),
            },
            instructions: CREATE_UPGRADE_INSTRUCTIONS,
            disk_read_bytes: CREATE_UPGRADE_READ_BYTES,
            write_bytes: CREATE_UPGRADE_WRITE_BYTES,
        };

        let resource_fee = CREATE_UPGRADE_RESOURCE_FEE;
        self.build_soroban_envelope(source, sequence, op, resources, resource_fee, inclusion_fee)
    }

    /// Build a V2 apply-load contract invocation transaction.
    ///
    /// Faithful port of stellar-core
    /// `TxGenerator::invokeSorobanLoadTransactionV2` (TxGenerator.cpp:551).
    /// Calls `do_cpu_only_work(u32 guest_cycles, u32 host_cycles, u32 event_count)`
    /// with explicit `SorobanResources` (footprint + instructions + write/disk
    /// bytes), op-size padding (`increaseOpSize`, TxGenerator.cpp:361), and the
    /// archived-entry index list in `SorobanResourcesExtV0` when non-empty.
    ///
    /// The RNG is threaded through all samples in stellar-core's exact order
    /// (disk-read → rw → entry-id loop → tx-size → event-count → instructions).
    /// The draws are non-consensus (never hashed), so parity is on the
    /// instruction *model*, with determinism guaranteed by the injected seeded
    /// RNG.
    ///
    /// `next_key_to_restore` is the autorestore cursor (`mNextKeyToRestore`);
    /// it is advanced by the number of restored entries. When
    /// `params.pre_populated_archived_entries == 0` the autorestore branch is
    /// dormant and the cursor is untouched.
    #[allow(clippy::too_many_arguments)]
    pub fn invoke_soroban_apply_load_tx<R: Rng + ?Sized>(
        &self,
        source: &SecretKey,
        sequence: i64,
        instance: &ContractInstance,
        params: &ApplyLoadTxParams,
        next_key_to_restore: &mut u32,
        inclusion_fee: u32,
        rng: &mut R,
    ) -> anyhow::Result<ApplyLoadBuiltTx> {
        let contract_address = make_contract_address(&instance.contract_id);

        // Simulate disk reads via autorestore. Dormant when there are no
        // pre-populated archived entries (TxGenerator.cpp:573).
        let mut archive_entries_to_restore = 0u32;
        if params.pre_populated_archived_entries != 0 {
            archive_entries_to_restore = sample_discrete(
                &params.num_disk_read_entries.0,
                &params.num_disk_read_entries.1,
                0,
                rng,
            );
        }

        // RW entries; restoration counts as a write, so subtract it (saturating).
        let mut rw_entries =
            sample_discrete(&params.num_rw_entries.0, &params.num_rw_entries.1, 0, rng);
        rw_entries = rw_entries.saturating_sub(archive_entries_to_restore);

        // Parity: stellar-core `releaseAssert(dataEntryCount > rwEntries)`
        // (TxGenerator.cpp:596). Keep it a hard abort — do NOT soften to a clamp.
        assert!(
            params.data_entry_count > rw_entries as u64,
            "APPLY_LOAD: data_entry_count ({}) must exceed rw_entries ({}); \
             increase APPLY_LOAD_BL_BATCH_SIZE * APPLY_LOAD_BL_SIMULATED_LEDGERS",
            params.data_entry_count,
            rw_entries
        );

        let mut read_write: Vec<LedgerKey> = Vec::new();
        let mut used_entries: std::collections::HashSet<u64> = std::collections::HashSet::new();
        // entryDist(0, dataEntryCount - 1); generate `entry_count` UNIQUE keys
        // with the `--i` retry on collision (TxGenerator.cpp:600).
        let mut generate_entries = |entry_count: u32, footprint: &mut Vec<LedgerKey>| {
            let mut i = 0u32;
            while i < entry_count {
                let entry_id = rng.gen_range(0..params.data_entry_count);
                if used_entries.insert(entry_id) {
                    footprint.push(LedgerKey::ContractData(LedgerKeyContractData {
                        contract: contract_address.clone(),
                        key: ScVal::U64(entry_id),
                        durability: ContractDataDurability::Persistent,
                    }));
                    i += 1;
                }
                // else: collision → retry (do not advance i).
            }
        };
        generate_entries(rw_entries, &mut read_write);

        // Archived autorestore entries appended to the RW footprint, recording
        // their RW indexes (TxGenerator.cpp:621).
        let mut archived_indexes: Vec<u32> = Vec::new();
        if archive_entries_to_restore > 0 {
            let end_index = *next_key_to_restore + archive_entries_to_restore;
            if end_index > params.pre_populated_archived_entries {
                anyhow::bail!(
                    "Ran out of hot archive entries: {} > {}",
                    end_index,
                    params.pre_populated_archived_entries
                );
            }
            while *next_key_to_restore < end_index {
                let lk = get_key_for_archived_entry(*next_key_to_restore as u64);
                read_write.push(lk);
                archived_indexes.push((read_write.len() - 1) as u32);
                *next_key_to_restore += 1;
            }
        }

        let mut resources = SorobanResources {
            footprint: LedgerFootprint {
                read_only: instance
                    .read_only_keys
                    .clone()
                    .try_into()
                    .unwrap_or_default(),
                read_write: read_write.try_into().unwrap_or_default(),
            },
            instructions: 0,
            disk_read_bytes: 0,
            write_bytes: 0,
        };

        // tx overhead = baseline + xdr_size(resources) (TxGenerator.cpp:643).
        let tx_overhead_bytes =
            APPLY_LOAD_BASELINE_TX_SIZE_BYTES.saturating_add(xdr_size(&resources) as u32);
        let desired_tx_bytes =
            sample_discrete(&params.tx_size_bytes.0, &params.tx_size_bytes.1, 0, rng);
        let padding_bytes = desired_tx_bytes.saturating_sub(tx_overhead_bytes);
        let entries_write_size = params
            .data_entry_size
            .saturating_mul(rw_entries + archive_entries_to_restore);

        let event_count = sample_discrete(&params.event_count.0, &params.event_count.1, 0, rng);
        let target_instructions =
            sample_discrete(&params.instructions.0, &params.instructions.1, 0, rng);

        resources.instructions = target_instructions;
        resources.write_bytes = entries_write_size;
        resources.disk_read_bytes = params
            .data_entry_size
            .saturating_mul(archive_entries_to_restore);

        let num_entries =
            rw_entries + archive_entries_to_restore + instance.read_only_keys.len() as u32;

        // Storage-instruction quadratic (TxGenerator.cpp:677).
        let instructions_for_entries = APPLY_LOAD_STORAGE_QUADRATIC_A
            .saturating_mul(num_entries.saturating_mul(num_entries))
            .saturating_add(APPLY_LOAD_STORAGE_QUADRATIC_B.saturating_mul(num_entries))
            .saturating_add(APPLY_LOAD_STORAGE_QUADRATIC_C);

        let instructions_without_cpu = APPLY_LOAD_BASE_INSTRUCTIONS
            .saturating_add(APPLY_LOAD_INSTRUCTIONS_PER_AUTH_BYTE.saturating_mul(padding_bytes))
            .saturating_add(
                APPLY_LOAD_INSTRUCTIONS_PER_ENTRY_BYTE.saturating_mul(entries_write_size),
            )
            .saturating_add(instructions_for_entries)
            .saturating_add(APPLY_LOAD_INSTRUCTIONS_PER_EVENT.saturating_mul(event_count));

        let mut cpu_target = target_instructions.saturating_sub(instructions_without_cpu);

        // Guest-only cycles (TxGenerator.cpp:702): host cycles deliberately 0.
        let guest_cycles = cpu_target / APPLY_LOAD_INSTRUCTIONS_PER_GUEST_CYCLE;
        cpu_target -= guest_cycles * APPLY_LOAD_INSTRUCTIONS_PER_GUEST_CYCLE;
        let _ = cpu_target;
        let host_cycles: u32 = 0;

        // do_cpu_only_work(makeU32(guest), makeU32(host), makeU32(events)) —
        // all 3 args U32, host_cycles still emitted (TxGenerator.cpp:710).
        let host_fn = HostFunction::InvokeContract(InvokeContractArgs {
            contract_address,
            function_name: ScSymbol("do_cpu_only_work".try_into().unwrap()),
            args: vec![
                ScVal::U32(guest_cycles),
                ScVal::U32(host_cycles),
                ScVal::U32(event_count),
            ]
            .try_into()
            .unwrap_or_default(),
        });

        let mut op = Operation {
            source_account: None,
            body: OperationBody::InvokeHostFunction(InvokeHostFunctionOp {
                host_function: host_fn,
                auth: VecM::default(),
            }),
        };

        increase_op_size(&mut op, padding_bytes);

        // Resource fee: henyey has no public `sorobanResourceFee` over raw
        // `SorobanResources` (same gap #3314 documented for create_upgrade);
        // the fee is a non-hashed bound, so a generous over-estimate is
        // acceptable. stellar-core feeds `eventSize * eventCount` of event
        // payload into the fee (TxGenerator.cpp:716); we fold that into the
        // over-estimate, then apply the `+1_000_000 + restored * 100_000`
        // buffer (TxGenerator.cpp:719) on top.
        let event_payload_bytes = (APPLY_LOAD_EVENT_SIZE_BYTES as i64) * (event_count as i64);
        let resource_fee = INVOKE_RESOURCE_FEE
            + event_payload_bytes
            + 1_000_000
            + (archive_entries_to_restore as i64) * 100_000;

        // Archived-index list lives in SorobanTransactionDataExt::V1 ↦
        // SorobanResourcesExtV0.archived_soroban_entries — only when non-empty
        // (else the default V0 ext), exactly stellar-core's
        // `archivedIndexes.empty() ? nullopt : ...`.
        let ext = if archived_indexes.is_empty() {
            SorobanTransactionDataExt::V0
        } else {
            SorobanTransactionDataExt::V1(SorobanResourcesExtV0 {
                archived_soroban_entries: archived_indexes.try_into().unwrap_or_default(),
            })
        };

        let envelope = self.build_soroban_envelope_with_ext(
            source,
            sequence,
            op,
            resources,
            ext,
            resource_fee,
            inclusion_fee,
        )?;

        Ok(ApplyLoadBuiltTx {
            envelope,
            archived_entries_restored: archive_entries_to_restore,
        })
    }

    /// Build a SAC (Stellar Asset Contract) creation transaction.
    ///
    /// Matches stellar-core `TxGenerator::createSACTransaction()`.
    pub fn create_sac_tx(
        &self,
        source: &SecretKey,
        sequence: i64,
        asset: stellar_xdr::Asset,
        inclusion_fee: u32,
    ) -> anyhow::Result<TransactionEnvelope> {
        let preimage = ContractIdPreimage::Asset(asset);

        let executable = ContractExecutable::StellarAsset;

        let contract_id = compute_contract_id(&preimage, &self.network_passphrase)?;

        let instance_key = contract_instance_key(&contract_id);

        let host_fn = HostFunction::CreateContract(CreateContractArgs {
            contract_id_preimage: preimage,
            executable,
        });

        let op = Operation {
            source_account: None,
            body: OperationBody::InvokeHostFunction(InvokeHostFunctionOp {
                host_function: host_fn,
                auth: VecM::default(),
            }),
        };

        let resources = SorobanResources {
            footprint: LedgerFootprint {
                read_only: VecM::default(),
                read_write: vec![instance_key].try_into().unwrap_or_default(),
            },
            instructions: CREATE_CONTRACT_INSTRUCTIONS,
            disk_read_bytes: CREATE_CONTRACT_READ_BYTES,
            write_bytes: CREATE_CONTRACT_WRITE_BYTES,
        };

        let resource_fee = CREATE_CONTRACT_RESOURCE_FEE;
        self.build_soroban_envelope(source, sequence, op, resources, resource_fee, inclusion_fee)
    }

    /// Build a SAC `transfer` invocation transaction.
    ///
    /// Matches stellar-core `TxGenerator::invokeSACPayment()`.
    pub fn invoke_sac_transfer_tx(
        &self,
        source: &SecretKey,
        sequence: i64,
        transfer: SacTransfer,
    ) -> anyhow::Result<TransactionEnvelope> {
        let args = vec![
            ScVal::Address(transfer.from_address.clone()),
            ScVal::Address(transfer.to_address.clone()),
            make_i128(transfer.amount),
        ];

        let auth = SorobanAuthorizationEntry {
            credentials: SorobanCredentials::SourceAccount,
            root_invocation: SorobanAuthorizedInvocation {
                function: SorobanAuthorizedFunction::ContractFn(InvokeContractArgs {
                    contract_address: make_contract_address(&transfer.contract_id),
                    function_name: ScSymbol("transfer".try_into().unwrap()),
                    args: args.clone().try_into().unwrap_or_default(),
                }),
                sub_invocations: VecM::default(),
            },
        };

        let contract_address = make_contract_address(&transfer.contract_id);

        let host_fn = HostFunction::InvokeContract(InvokeContractArgs {
            contract_address,
            function_name: ScSymbol("transfer".try_into().unwrap()),
            args: args.try_into().unwrap_or_default(),
        });

        let op = Operation {
            source_account: None,
            body: OperationBody::InvokeHostFunction(InvokeHostFunctionOp {
                host_function: host_fn,
                auth: vec![auth].try_into().unwrap_or_default(),
            }),
        };

        // Build read_write footprint entries matching stellar-core:
        // 1. Source account entry (for balance deduction)
        // 2. Destination balance CONTRACT_DATA entry (for SAC balance tracking)
        let read_write_keys = build_sac_transfer_rw_keys(
            &transfer.from_address,
            transfer.to_address,
            &transfer.contract_id,
        );

        let resources = SorobanResources {
            footprint: LedgerFootprint {
                read_only: transfer.instance_keys.try_into().unwrap_or_default(),
                read_write: read_write_keys.try_into().unwrap_or_default(),
            },
            // stellar-core uses 250K instructions but our non-typed host API (P25)
            // meters XDR deserialization, consuming ~263K+ for a SAC transfer.
            // Use 2M with generous I/O limits to avoid ResourceLimitExceeded in load tests.
            instructions: SAC_TRANSFER_INSTRUCTIONS,
            disk_read_bytes: SAC_TRANSFER_READ_BYTES,
            write_bytes: SAC_TRANSFER_WRITE_BYTES,
        };

        let resource_fee = CREATE_CONTRACT_RESOURCE_FEE;
        self.build_soroban_envelope(
            source,
            sequence,
            op,
            resources,
            resource_fee,
            transfer.inclusion_fee,
        )
    }

    /// Build a batch transfer invocation transaction.
    ///
    /// Matches stellar-core `TxGenerator::invokeBatchTransfer()`.
    pub fn invoke_batch_transfer_tx(
        &self,
        source: &SecretKey,
        sequence: i64,
        transfer: BatchTransfer,
    ) -> anyhow::Result<TransactionEnvelope> {
        let batch_size = transfer.destinations.len() as u32;
        let dest_vec = ScVal::Vec(Some(ScVec(
            transfer.destinations.try_into().unwrap_or_default(),
        )));
        let args = vec![transfer.sac_address, dest_vec];

        let contract_address = make_contract_address(&transfer.contract_id);

        let host_fn = HostFunction::InvokeContract(InvokeContractArgs {
            contract_address,
            function_name: ScSymbol("batch_transfer".try_into().unwrap()),
            args: args.try_into().unwrap_or_default(),
        });

        let op = Operation {
            source_account: None,
            body: OperationBody::InvokeHostFunction(InvokeHostFunctionOp {
                host_function: host_fn,
                auth: VecM::default(),
            }),
        };

        let resources = SorobanResources {
            footprint: LedgerFootprint {
                read_only: transfer.instance_keys.try_into().unwrap_or_default(),
                read_write: VecM::default(),
            },
            instructions: BATCH_TRANSFER_INSTRUCTIONS_PER_ITEM * batch_size,
            disk_read_bytes: BATCH_TRANSFER_READ_BYTES_PER_ITEM * batch_size,
            write_bytes: BATCH_TRANSFER_WRITE_BYTES_PER_ITEM * batch_size,
        };

        let resource_fee = INVOKE_RESOURCE_FEE;
        self.build_soroban_envelope(
            source,
            sequence,
            op,
            resources,
            resource_fee,
            transfer.inclusion_fee,
        )
    }

    /// Get the embedded loadgen test contract WASM bytes.
    pub fn loadgen_wasm() -> &'static [u8] {
        LOADGEN_WASM
    }

    /// Compute the SHA-256 hash of the loadgen WASM.
    pub fn loadgen_wasm_hash() -> Hash256 {
        Hash256::hash(LOADGEN_WASM)
    }

    /// Get the embedded `write_bytes` upgrade-setup contract WASM bytes.
    pub fn write_upgrade_bytes_wasm() -> &'static [u8] {
        WRITE_UPGRADE_BYTES_WASM
    }

    /// Compute the SHA-256 hash of the `write_bytes` upgrade-setup WASM.
    pub fn write_upgrade_bytes_wasm_hash() -> Hash256 {
        Hash256::hash(WRITE_UPGRADE_BYTES_WASM)
    }

    /// Generate random WASM bytes of approximately the given size.
    ///
    /// Produces a minimal valid WASM module padded to the desired size
    /// with a custom section.
    pub fn random_wasm(size: usize, seed: u64) -> Vec<u8> {
        // Minimal valid WASM module header
        let mut wasm = vec![
            0x00, 0x61, 0x73, 0x6d, // magic: \0asm
            0x01, 0x00, 0x00, 0x00, // version: 1
        ];

        // Add a custom section with padding to reach desired size
        if size > wasm.len() + 3 {
            let payload_size = size - wasm.len() - 3; // section id + 2 bytes for size (varuint)
            wasm.push(0x00); // custom section id
                             // LEB128 encode payload size (simplified for sizes < 16384)
            if payload_size < 128 {
                wasm.push(payload_size as u8);
            } else {
                wasm.push((payload_size & 0x7f) as u8 | 0x80);
                wasm.push((payload_size >> 7) as u8);
            }
            // Fill with deterministic pseudo-random bytes
            let hash = Hash256::hash(&seed.to_le_bytes());
            for i in 0..payload_size {
                wasm.push(hash.0[i % 32]);
            }
        }
        wasm
    }

    // --- Internal helpers ---

    fn build_soroban_envelope(
        &self,
        source: &SecretKey,
        sequence: i64,
        op: Operation,
        resources: SorobanResources,
        resource_fee: i64,
        inclusion_fee: u32,
    ) -> anyhow::Result<TransactionEnvelope> {
        self.build_soroban_envelope_with_ext(
            source,
            sequence,
            op,
            resources,
            SorobanTransactionDataExt::V0,
            resource_fee,
            inclusion_fee,
        )
    }

    #[allow(clippy::too_many_arguments)]
    fn build_soroban_envelope_with_ext(
        &self,
        source: &SecretKey,
        sequence: i64,
        op: Operation,
        resources: SorobanResources,
        ext: SorobanTransactionDataExt,
        resource_fee: i64,
        inclusion_fee: u32,
    ) -> anyhow::Result<TransactionEnvelope> {
        let total_fee = inclusion_fee + resource_fee as u32;
        let source_muxed = MuxedAccount::Ed25519(Uint256(*source.public_key().as_bytes()));

        let soroban_data = SorobanTransactionData {
            ext,
            resources,
            resource_fee,
        };

        let tx = Transaction {
            source_account: source_muxed,
            fee: total_fee,
            seq_num: SequenceNumber(sequence),
            cond: Preconditions::None,
            memo: Memo::None,
            operations: vec![op].try_into().unwrap_or_default(),
            ext: TransactionExt::V1(soroban_data),
        };

        let mut envelope = TransactionEnvelope::Tx(TransactionV1Envelope {
            tx,
            signatures: VecM::default(),
        });

        sign_envelope(&mut envelope, source, &self.network_passphrase)?;
        Ok(envelope)
    }
}

// ---------------------------------------------------------------------------
// Public helpers
// ---------------------------------------------------------------------------

/// Compute a contract ID from a `ContractIdPreimage`.
///
/// Hashes `HashIdPreimage::ContractId` with the network passphrase.
pub fn compute_contract_id(
    preimage: &ContractIdPreimage,
    network_passphrase: &str,
) -> anyhow::Result<Hash256> {
    let network_id = NetworkId::from_passphrase(network_passphrase);
    let hash_preimage = HashIdPreimage::ContractId(HashIdPreimageContractId {
        network_id: Hash(network_id.0 .0),
        contract_id_preimage: preimage.clone(),
    });
    let bytes = hash_preimage
        .to_xdr(Limits::none())
        .map_err(|e| anyhow::anyhow!("failed to encode contract ID preimage: {}", e))?;
    Ok(Hash256::hash(&bytes))
}

/// Returns the `LedgerKey` for a pre-populated hot-archive entry at `index`.
///
/// Faithful port of stellar-core `ApplyLoad::getKeyForArchivedEntry`
/// (ApplyLoad.cpp:409): a PERSISTENT `CONTRACT_DATA` key whose contract is
/// `sha256("archived-entry")` and whose key is `SCV_U64(index)`.
pub fn get_key_for_archived_entry(index: u64) -> LedgerKey {
    let contract_id = Hash256::hash(b"archived-entry");
    LedgerKey::ContractData(LedgerKeyContractData {
        contract: ScAddress::Contract(ContractId(Hash(contract_id.0))),
        key: ScVal::U64(index),
        durability: ContractDataDurability::Persistent,
    })
}

/// XDR-encoded size of a value, in bytes (parity helper for `xdr::xdr_size`).
fn xdr_size<T: WriteXdr>(v: &T) -> usize {
    v.to_xdr(Limits::none()).map(|b| b.len()).unwrap_or(0)
}

/// Pad an INVOKE_HOST_FUNCTION op up to `increase_up_to_bytes` by attaching a
/// source-account auth entry carrying an `SCV_BYTES` payload.
///
/// Faithful port of stellar-core `increaseOpSize` (TxGenerator.cpp:361): the
/// auth + empty-bytes overhead is subtracted first; if the overhead already
/// exceeds the target, no padding is added.
fn increase_op_size(op: &mut Operation, increase_up_to_bytes: u32) {
    if increase_up_to_bytes == 0 {
        return;
    }

    // SOROBAN_CREDENTIALS_SOURCE_ACCOUNT auth with an empty-bytes contract-fn arg.
    let mut auth = SorobanAuthorizationEntry {
        credentials: SorobanCredentials::SourceAccount,
        root_invocation: SorobanAuthorizedInvocation {
            function: SorobanAuthorizedFunction::ContractFn(InvokeContractArgs {
                contract_address: ScAddress::Contract(ContractId(Hash([0u8; 32]))),
                function_name: ScSymbol(Default::default()),
                args: VecM::default(),
            }),
            sub_invocations: VecM::default(),
        },
    };

    let empty_val = ScVal::Bytes(ScBytes(Default::default()));
    let overhead_bytes = (xdr_size(&auth) + xdr_size(&empty_val)) as u32;
    let payload_len = increase_up_to_bytes.saturating_sub(overhead_bytes);

    let val = ScVal::Bytes(ScBytes(
        vec![0u8; payload_len as usize]
            .try_into()
            .unwrap_or_default(),
    ));
    if let SorobanAuthorizedFunction::ContractFn(ref mut cf) = auth.root_invocation.function {
        cf.args = vec![val].try_into().unwrap_or_default();
    }
    if let OperationBody::InvokeHostFunction(ref mut ihf) = op.body {
        ihf.auth = vec![auth].try_into().unwrap_or_default();
    }
}

/// Construct an `ScVal::I128` from a Rust `i128`.
pub fn make_i128(value: i128) -> ScVal {
    ScVal::I128(Int128Parts {
        hi: (value >> 64) as i64,
        lo: value as u64,
    })
}

/// Construct an `ScAddress::Account` from a public key.
pub fn make_account_address(public_key: &henyey_crypto::PublicKey) -> ScAddress {
    ScAddress::Account(stellar_xdr::AccountId(
        stellar_xdr::PublicKey::PublicKeyTypeEd25519(Uint256(*public_key.as_bytes())),
    ))
}

/// Construct an `ScAddress::Contract` from a contract hash.
pub fn make_contract_address(contract_id: &Hash256) -> ScAddress {
    ScAddress::Contract(ContractId(Hash(contract_id.0)))
}

/// Build a `LedgerKey` for a contract instance.
pub fn contract_instance_key(contract_id: &Hash256) -> LedgerKey {
    LedgerKey::ContractData(LedgerKeyContractData {
        contract: make_contract_address(contract_id),
        key: ScVal::LedgerKeyContractInstance,
        durability: ContractDataDurability::Persistent,
    })
}

/// Build a `LedgerKey` for contract code.
pub fn contract_code_key(wasm_hash: &Hash256) -> LedgerKey {
    LedgerKey::ContractCode(LedgerKeyContractCode {
        hash: Hash(wasm_hash.0),
    })
}

/// Build the read-write footprint keys for a SAC `transfer` invocation.
///
/// Returns keys for:
/// 1. Source account (if the sender is an account, not a contract)
/// 2. Destination balance entry (CONTRACT_DATA for contracts, Account for accounts)
fn build_sac_transfer_rw_keys(
    from_address: &ScAddress,
    to_address: ScAddress,
    contract_id: &Hash256,
) -> Vec<LedgerKey> {
    let mut keys = Vec::new();

    // Source account key
    if let ScAddress::Account(ref aid) = from_address {
        keys.push(LedgerKey::Account(stellar_xdr::LedgerKeyAccount {
            account_id: aid.clone(),
        }));
    }

    // Destination balance key
    match &to_address {
        ScAddress::Contract(_) => {
            let balance_key = ScVal::Vec(Some(stellar_xdr::ScVec(
                vec![
                    ScVal::Symbol(ScSymbol("Balance".try_into().unwrap())),
                    ScVal::Address(to_address),
                ]
                .try_into()
                .unwrap_or_default(),
            )));
            keys.push(LedgerKey::ContractData(
                stellar_xdr::LedgerKeyContractData {
                    contract: make_contract_address(contract_id),
                    key: balance_key,
                    durability: stellar_xdr::ContractDataDurability::Persistent,
                },
            ));
        }
        ScAddress::Account(ref aid) => {
            keys.push(LedgerKey::Account(stellar_xdr::LedgerKeyAccount {
                account_id: aid.clone(),
            }));
        }
        _ => {} // MuxedAccount, ClaimableBalance, LiquidityPool not used in load test
    }

    keys
}

/// Sign a `TransactionEnvelope` and attach the signature.
///
/// This is the shared signing logic used by `Simulation`, `TxGenerator`,
/// and `SorobanTxBuilder` to avoid triple-duplicating the hash→sign→attach
/// sequence.
pub fn sign_envelope(
    envelope: &mut TransactionEnvelope,
    secret: &SecretKey,
    network_passphrase: &str,
) -> anyhow::Result<()> {
    let network_id = NetworkId::from_passphrase(network_passphrase);
    let hash = henyey_tx::TransactionFrame::hash_envelope(envelope, &network_id)?;
    let signature = sign_hash(secret, &hash);
    let public_key = secret.public_key();
    let pk_bytes = public_key.as_bytes();
    let hint = SignatureHint([pk_bytes[28], pk_bytes[29], pk_bytes[30], pk_bytes[31]]);
    let decorated = DecoratedSignature {
        hint,
        signature: Signature(signature.0.to_vec().try_into().unwrap_or_default()),
    };
    if let TransactionEnvelope::Tx(ref mut env) = envelope {
        env.signatures = vec![decorated].try_into().unwrap_or_default();
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_loadgen_wasm_is_valid() {
        let wasm = SorobanTxBuilder::loadgen_wasm();
        assert!(wasm.len() > 8);
        assert_eq!(&wasm[..4], b"\x00asm");
    }

    #[test]
    fn test_loadgen_wasm_hash_is_deterministic() {
        let h1 = SorobanTxBuilder::loadgen_wasm_hash();
        let h2 = SorobanTxBuilder::loadgen_wasm_hash();
        assert_eq!(h1, h2);
    }

    /// The bundled loadgen WASM must be the p26 blob that exports BOTH
    /// `do_work` (V1 invoke) and `do_cpu_only_work` (V2 apply-load invoke).
    /// henyey's pre-#3309 blob exported only `do_work`, which would make the
    /// V2 apply-load tx fail at apply (#3309 correctness blocker). The hash is
    /// re-pinned to the p26 blob.
    #[test]
    fn test_loadgen_wasm_exports_do_cpu_only_work() {
        let wasm = SorobanTxBuilder::loadgen_wasm();
        let exports = wasm_function_exports(wasm);
        assert!(
            exports.iter().any(|e| e == "do_work"),
            "loadgen wasm must export do_work, got {exports:?}"
        );
        assert!(
            exports.iter().any(|e| e == "do_cpu_only_work"),
            "loadgen wasm must export do_cpu_only_work (#3309), got {exports:?}"
        );
    }

    /// Re-pin: the bundled blob is the stellar-core p26 loadgen.wasm whose
    /// sha256 is `1a2ee5a8…`. Pinning the exact hash guards against an
    /// accidental swap back to a blob lacking `do_cpu_only_work`.
    #[test]
    fn test_loadgen_wasm_hash_is_pinned() {
        let h = SorobanTxBuilder::loadgen_wasm_hash();
        // sha256 of stellar-core p26 loadgen.wasm.
        let expected: [u8; 32] = [
            0x1a, 0x2e, 0xe5, 0xa8, 0x9d, 0xd2, 0x16, 0x2a, 0x20, 0xe7, 0x16, 0xb3, 0xe2, 0xde,
            0x70, 0xa2, 0xd6, 0x62, 0x48, 0x0a, 0x95, 0x65, 0x35, 0x03, 0x78, 0x66, 0x18, 0x71,
            0xbc, 0xf8, 0xe3, 0x98,
        ];
        assert_eq!(h.0, expected);
    }

    /// Parse the function-name exports out of a WASM export section.
    /// Test-only helper (no `wabt`/`wasmparser` dep needed).
    fn wasm_function_exports(d: &[u8]) -> Vec<String> {
        fn uleb(d: &[u8], i: &mut usize) -> u64 {
            let mut r = 0u64;
            let mut s = 0;
            loop {
                let b = d[*i];
                *i += 1;
                r |= ((b & 0x7f) as u64) << s;
                if b & 0x80 == 0 {
                    break;
                }
                s += 7;
            }
            r
        }
        assert_eq!(&d[..4], b"\x00asm");
        let mut i = 8usize;
        let mut out = Vec::new();
        while i < d.len() {
            let sid = d[i];
            i += 1;
            let size = uleb(d, &mut i) as usize;
            let end = i + size;
            if sid == 7 {
                let mut j = i;
                let cnt = uleb(d, &mut j);
                for _ in 0..cnt {
                    let nlen = uleb(d, &mut j) as usize;
                    let name = String::from_utf8_lossy(&d[j..j + nlen]).to_string();
                    j += nlen;
                    let kind = d[j];
                    j += 1;
                    let _idx = uleb(d, &mut j);
                    if kind == 0 {
                        out.push(name);
                    }
                }
            }
            i = end;
        }
        out
    }

    fn test_instance() -> crate::loadgen::ContractInstance {
        let contract_id = Hash256::hash(b"apply-load-contract");
        let code_key = contract_code_key(&Hash256::hash(b"code"));
        let instance_key = contract_instance_key(&contract_id);
        crate::loadgen::ContractInstance {
            read_only_keys: vec![code_key, instance_key],
            contract_id,
            contract_entries_size: 0,
        }
    }

    fn fixed_dist(value: u32) -> (Vec<u32>, Vec<u32>) {
        (vec![value], vec![1])
    }

    /// V2 op-shape: `do_cpu_only_work` with exactly 3 U32 args, RO = instance
    /// keys, RW = sampled SCV_U64 persistent contract-data keys, resources
    /// populated, padding-auth present when desired tx size > overhead.
    #[test]
    fn test_invoke_apply_load_v2_op_shape() {
        use rand::SeedableRng;
        let builder = SorobanTxBuilder::new("Test SDF Network ; September 2015".to_string());
        let instance = test_instance();
        let source = SecretKey::from_seed(&[7u8; 32]);
        let mut rng = rand_chacha::ChaCha8Rng::seed_from_u64(1);

        let params = ApplyLoadTxParams {
            data_entry_count: 1_000,
            data_entry_size: 200,
            num_rw_entries: fixed_dist(3),
            num_disk_read_entries: fixed_dist(0),
            tx_size_bytes: fixed_dist(10_000),
            event_count: fixed_dist(2),
            instructions: fixed_dist(50_000_000),
            pre_populated_archived_entries: 0,
        };
        let mut next_key_to_restore = 0u32;
        let built = builder
            .invoke_soroban_apply_load_tx(
                &source,
                100,
                &instance,
                &params,
                &mut next_key_to_restore,
                100,
                &mut rng,
            )
            .unwrap();

        let TransactionEnvelope::Tx(v1) = &built.envelope else {
            panic!("expected v1 envelope");
        };
        let op = &v1.tx.operations[0];
        let OperationBody::InvokeHostFunction(ihf) = &op.body else {
            panic!("expected InvokeHostFunction");
        };
        let HostFunction::InvokeContract(args) = &ihf.host_function else {
            panic!("expected InvokeContract");
        };
        assert_eq!(args.function_name.0.as_slice(), b"do_cpu_only_work");
        assert_eq!(args.args.len(), 3, "do_cpu_only_work takes 3 args");
        for a in args.args.iter() {
            assert!(
                matches!(a, ScVal::U32(_)),
                "all args must be U32, got {a:?}"
            );
        }

        let TransactionExt::V1(data) = &v1.tx.ext else {
            panic!("expected V1 soroban ext");
        };
        let res = &data.resources;
        // RO footprint = instance keys.
        assert_eq!(res.footprint.read_only.to_vec(), instance.read_only_keys);
        // RW footprint = 3 sampled persistent contract-data keys, all SCV_U64.
        let rw = res.footprint.read_write.to_vec();
        assert_eq!(rw.len(), 3);
        for k in &rw {
            let LedgerKey::ContractData(cd) = k else {
                panic!("expected CONTRACT_DATA rw key");
            };
            assert_eq!(cd.durability, ContractDataDurability::Persistent);
            assert!(matches!(cd.key, ScVal::U64(_)), "rw key must be SCV_U64");
        }
        assert_eq!(res.instructions, 50_000_000);
        assert_eq!(res.write_bytes, 200 * 3);
        assert_eq!(res.disk_read_bytes, 0);

        // padding auth present (desired 10_000 > overhead).
        assert_eq!(ihf.auth.len(), 1, "padding auth entry expected");

        // No archived entries → default ext.
        assert_eq!(
            data.ext,
            SorobanTransactionDataExt::V0,
            "no archived entries → V0 ext"
        );
        assert_eq!(built.archived_entries_restored, 0);
    }

    /// Instruction model: with cpu cycles deliberately guest-only and
    /// host_cycles always 0, the three U32 args are
    /// `(guest_cycles, 0, event_count)`, where guest_cycles is derived from the
    /// tuned model. Pins base=737_119, perGuest=40, perEvent=8_500, perAuth=35,
    /// entry-byte=44, and the storage quadratic 205n²+12000n+65485.
    #[test]
    fn test_invoke_apply_load_v2_instruction_model() {
        use rand::SeedableRng;
        let builder = SorobanTxBuilder::new("Test SDF Network ; September 2015".to_string());
        let instance = test_instance();
        let source = SecretKey::from_seed(&[8u8; 32]);
        let mut rng = rand_chacha::ChaCha8Rng::seed_from_u64(2);

        // No padding (tx_size 0 ⇒ paddingBytes 0), no events, no disk reads.
        let rw_entries = 2u32;
        let event_count = 0u32;
        let target = 100_000_000u32;
        let params = ApplyLoadTxParams {
            data_entry_count: 1_000,
            data_entry_size: 0,
            num_rw_entries: fixed_dist(rw_entries),
            num_disk_read_entries: fixed_dist(0),
            tx_size_bytes: fixed_dist(0),
            event_count: fixed_dist(event_count),
            instructions: fixed_dist(target),
            pre_populated_archived_entries: 0,
        };
        let mut next_key_to_restore = 0u32;
        let built = builder
            .invoke_soroban_apply_load_tx(
                &source,
                100,
                &instance,
                &params,
                &mut next_key_to_restore,
                100,
                &mut rng,
            )
            .unwrap();

        // Hand-computed expected guest_cycles.
        let num_entries = rw_entries + instance.read_only_keys.len() as u32; // = 4
        let instructions_for_entries =
            205 * num_entries * num_entries + 12_000 * num_entries + 65_485;
        let entries_write_size = 0u32; // data_entry_size = 0
        let padding = 0u32;
        let instructions_without_cpu = 737_119
            + 35 * padding
            + 44 * entries_write_size
            + instructions_for_entries
            + event_count * 8_500;
        let remaining = target - instructions_without_cpu;
        let expected_guest_cycles = remaining / 40;

        let TransactionEnvelope::Tx(v1) = &built.envelope else {
            panic!("expected v1 envelope");
        };
        let op = &v1.tx.operations[0];
        let OperationBody::InvokeHostFunction(ihf) = &op.body else {
            panic!("expected InvokeHostFunction");
        };
        let HostFunction::InvokeContract(args) = &ihf.host_function else {
            panic!("expected InvokeContract");
        };
        let g = match &args.args[0] {
            ScVal::U32(v) => *v,
            _ => panic!("guest_cycles arg must be U32"),
        };
        let h = match &args.args[1] {
            ScVal::U32(v) => *v,
            _ => panic!("host_cycles arg must be U32"),
        };
        let e = match &args.args[2] {
            ScVal::U32(v) => *v,
            _ => panic!("event_count arg must be U32"),
        };
        assert_eq!(g, expected_guest_cycles, "guest_cycles must match model");
        assert_eq!(h, 0, "host_cycles is always 0 in V2 (guest-only)");
        assert_eq!(e, event_count);
        // instructions resource is the unmodified sampled target.
        let TransactionExt::V1(data) = &v1.tx.ext else {
            panic!("expected V1 ext");
        };
        assert_eq!(data.resources.instructions, target);
    }

    /// Archived-entry key: contract = sha256("archived-entry"), key = SCV_U64,
    /// PERSISTENT (parity with ApplyLoad::getKeyForArchivedEntry).
    #[test]
    fn test_get_key_for_archived_entry() {
        let lk = get_key_for_archived_entry(7);
        let LedgerKey::ContractData(cd) = &lk else {
            panic!("expected CONTRACT_DATA");
        };
        let expected_contract = Hash256::hash(b"archived-entry");
        match &cd.contract {
            ScAddress::Contract(ContractId(Hash(bytes))) => {
                assert_eq!(bytes, &expected_contract.0)
            }
            other => panic!("expected contract address, got {other:?}"),
        }
        assert_eq!(cd.key, ScVal::U64(7));
        assert_eq!(cd.durability, ContractDataDurability::Persistent);
    }

    /// Autorestore is dormant when `pre_populated_archived_entries == 0`: no
    /// archived keys, no disk_read_bytes, V0 ext. With N>0 it appends exactly
    /// the restored keys, records their RW indexes in
    /// `SorobanResourcesExtV0.archived_soroban_entries`, sets disk_read_bytes,
    /// and errors when the cursor would exceed N.
    #[test]
    fn test_apply_load_autorestore() {
        use rand::SeedableRng;
        let builder = SorobanTxBuilder::new("Test SDF Network ; September 2015".to_string());
        let instance = test_instance();
        let source = SecretKey::from_seed(&[9u8; 32]);

        // N>0: 2 rw + 2 restored.
        let mut rng = rand_chacha::ChaCha8Rng::seed_from_u64(3);
        let params = ApplyLoadTxParams {
            data_entry_count: 1_000,
            data_entry_size: 100,
            num_rw_entries: fixed_dist(4), // 4 - 2 restored = 2 regular rw
            num_disk_read_entries: fixed_dist(2),
            tx_size_bytes: fixed_dist(0),
            event_count: fixed_dist(0),
            instructions: fixed_dist(50_000_000),
            pre_populated_archived_entries: 10,
        };
        let mut next_key_to_restore = 0u32;
        let built = builder
            .invoke_soroban_apply_load_tx(
                &source,
                100,
                &instance,
                &params,
                &mut next_key_to_restore,
                100,
                &mut rng,
            )
            .unwrap();
        assert_eq!(built.archived_entries_restored, 2);
        assert_eq!(next_key_to_restore, 2);

        let TransactionEnvelope::Tx(v1) = &built.envelope else {
            panic!("expected v1 envelope");
        };
        let TransactionExt::V1(data) = &v1.tx.ext else {
            panic!("expected V1 ext");
        };
        // disk_read_bytes = data_entry_size * restored.
        assert_eq!(data.resources.disk_read_bytes, 100 * 2);
        // RW footprint = 2 regular + 2 archived = 4.
        let rw = data.resources.footprint.read_write.to_vec();
        assert_eq!(rw.len(), 4);
        // Archived indexes recorded in ext V1.
        let SorobanTransactionDataExt::V1(ext) = &data.ext else {
            panic!("expected V1 ext with archived entries");
        };
        let idxs: Vec<u32> = ext.archived_soroban_entries.to_vec();
        assert_eq!(idxs, vec![2, 3], "archived entries at RW indexes 2,3");
        // The last 2 RW keys are archived-entry keys (sha256 contract).
        let expected_contract = Hash256::hash(b"archived-entry");
        for k in &rw[2..] {
            let LedgerKey::ContractData(cd) = k else {
                panic!("expected CONTRACT_DATA");
            };
            match &cd.contract {
                ScAddress::Contract(ContractId(Hash(b))) => {
                    assert_eq!(b, &expected_contract.0)
                }
                _ => panic!("archived key contract mismatch"),
            }
        }

        // Bounds: exceeding pre_populated count errors.
        let mut rng2 = rand_chacha::ChaCha8Rng::seed_from_u64(4);
        let mut cursor = 9u32; // 9 + 2 = 11 > 10
        let err = builder.invoke_soroban_apply_load_tx(
            &source,
            100,
            &instance,
            &params,
            &mut cursor,
            100,
            &mut rng2,
        );
        assert!(err.is_err(), "exceeding pre_populated entries must error");
    }

    /// Dormant when zero: no archived keys, V0 ext, disk_read_bytes = 0.
    #[test]
    fn test_apply_load_autorestore_dormant_when_zero() {
        use rand::SeedableRng;
        let builder = SorobanTxBuilder::new("Test SDF Network ; September 2015".to_string());
        let instance = test_instance();
        let source = SecretKey::from_seed(&[10u8; 32]);
        let mut rng = rand_chacha::ChaCha8Rng::seed_from_u64(5);
        let params = ApplyLoadTxParams {
            data_entry_count: 1_000,
            data_entry_size: 100,
            num_rw_entries: fixed_dist(3),
            num_disk_read_entries: fixed_dist(5), // ignored when prepopulated == 0
            tx_size_bytes: fixed_dist(0),
            event_count: fixed_dist(0),
            instructions: fixed_dist(50_000_000),
            pre_populated_archived_entries: 0,
        };
        let mut cursor = 0u32;
        let built = builder
            .invoke_soroban_apply_load_tx(
                &source,
                100,
                &instance,
                &params,
                &mut cursor,
                100,
                &mut rng,
            )
            .unwrap();
        assert_eq!(built.archived_entries_restored, 0);
        assert_eq!(cursor, 0, "cursor untouched when dormant");
        let TransactionEnvelope::Tx(v1) = &built.envelope else {
            panic!("expected v1 envelope");
        };
        let TransactionExt::V1(data) = &v1.tx.ext else {
            panic!("expected V1 ext");
        };
        assert_eq!(data.resources.disk_read_bytes, 0);
        assert_eq!(data.ext, SorobanTransactionDataExt::V0);
    }

    /// Dedup: the RW data-entry keys are unique even when collisions occur in
    /// sampling (the `--i` retry). Guards against an accidental switch to
    /// sampling-without-replacement.
    #[test]
    fn test_apply_load_generate_entries_dedup() {
        use rand::SeedableRng;
        let builder = SorobanTxBuilder::new("Test SDF Network ; September 2015".to_string());
        let instance = test_instance();
        let source = SecretKey::from_seed(&[11u8; 32]);
        let mut rng = rand_chacha::ChaCha8Rng::seed_from_u64(6);
        // Small data_entry_count forces collisions during sampling.
        let params = ApplyLoadTxParams {
            data_entry_count: 5,
            data_entry_size: 10,
            num_rw_entries: fixed_dist(4),
            num_disk_read_entries: fixed_dist(0),
            tx_size_bytes: fixed_dist(0),
            event_count: fixed_dist(0),
            instructions: fixed_dist(50_000_000),
            pre_populated_archived_entries: 0,
        };
        let mut cursor = 0u32;
        let built = builder
            .invoke_soroban_apply_load_tx(
                &source,
                100,
                &instance,
                &params,
                &mut cursor,
                100,
                &mut rng,
            )
            .unwrap();
        let TransactionEnvelope::Tx(v1) = &built.envelope else {
            panic!("expected v1 envelope");
        };
        let TransactionExt::V1(data) = &v1.tx.ext else {
            panic!("expected V1 ext");
        };
        let rw = data.resources.footprint.read_write.to_vec();
        assert_eq!(rw.len(), 4, "exactly 4 unique RW entries");
        let mut keys: Vec<u64> = rw
            .iter()
            .map(|k| match k {
                LedgerKey::ContractData(cd) => match cd.key {
                    ScVal::U64(v) => v,
                    _ => panic!("expected SCV_U64"),
                },
                _ => panic!("expected CONTRACT_DATA"),
            })
            .collect();
        keys.sort_unstable();
        keys.dedup();
        assert_eq!(keys.len(), 4, "RW entry keys must be unique");
    }

    /// sampleDiscrete parity: empty values ⇒ default; single value ⇒ that value;
    /// weighted pick is deterministic under a seeded RNG.
    #[test]
    fn test_sample_discrete_semantics() {
        use rand::SeedableRng;
        // empty ⇒ default.
        let mut rng = rand_chacha::ChaCha8Rng::seed_from_u64(0);
        assert_eq!(crate::loadgen::sample_discrete(&[], &[], 42, &mut rng), 42);
        // single value ⇒ that value.
        assert_eq!(
            crate::loadgen::sample_discrete(&[9u32], &[1], 0, &mut rng),
            9
        );
        // weighted pick deterministic for a fixed seed.
        let mut rng2 = rand_chacha::ChaCha8Rng::seed_from_u64(123);
        let values = [10u32, 20, 30];
        let weights = [1u32, 1, 1];
        let a: Vec<u32> = (0..8)
            .map(|_| crate::loadgen::sample_discrete(&values, &weights, 0, &mut rng2))
            .collect();
        let mut rng3 = rand_chacha::ChaCha8Rng::seed_from_u64(123);
        let b: Vec<u32> = (0..8)
            .map(|_| crate::loadgen::sample_discrete(&values, &weights, 0, &mut rng3))
            .collect();
        assert_eq!(a, b, "seeded sampling must be deterministic");
        // All draws are within the value set.
        assert!(a.iter().all(|v| values.contains(v)));
    }

    #[test]
    fn test_random_wasm_is_valid() {
        let wasm = SorobanTxBuilder::random_wasm(1024, 42);
        assert_eq!(&wasm[..4], b"\x00asm");
        assert!(wasm.len() >= 1024);
    }

    #[test]
    fn test_random_wasm_is_deterministic() {
        let a = SorobanTxBuilder::random_wasm(512, 99);
        let b = SorobanTxBuilder::random_wasm(512, 99);
        assert_eq!(a, b);
    }

    #[test]
    fn test_make_i128() {
        let val = make_i128(1_000_000);
        match val {
            ScVal::I128(parts) => {
                assert_eq!(parts.hi, 0);
                assert_eq!(parts.lo, 1_000_000);
            }
            _ => panic!("expected I128"),
        }
    }

    #[test]
    fn test_compute_contract_id_deterministic() {
        let preimage = ContractIdPreimage::Asset(stellar_xdr::Asset::Native);
        let id1 = compute_contract_id(&preimage, "Test SDF Network ; September 2015").unwrap();
        let id2 = compute_contract_id(&preimage, "Test SDF Network ; September 2015").unwrap();
        assert_eq!(id1, id2);
    }

    #[test]
    fn test_contract_instance_key_builds() {
        let id = Hash256::hash(b"test");
        let key = contract_instance_key(&id);
        assert!(matches!(key, LedgerKey::ContractData(_)));
    }

    /// The create-upgrade tx must match stellar-core
    /// `invokeSorobanCreateUpgradeTransaction` (TxGenerator.cpp:1251):
    /// INVOKE_CONTRACT `write`(SCV_BYTES(upgradeBytes)), footprint
    /// readOnly=[instance, code], readWrite=[temp CONTRACT_DATA keyed by
    /// SCV_BYTES(sha256(upgradeBytes))], fixed resources.
    #[test]
    fn test_invoke_soroban_create_upgrade_tx_op_shape() {
        let passphrase = "Test SDF Network ; September 2015".to_string();
        let builder = SorobanTxBuilder::new(passphrase);
        let source = SecretKey::from_seed(&[3u8; 32]);

        let wasm_hash = Hash256::hash(b"loadgen-wasm");
        let contract_id = Hash256::hash(b"upgrade-contract");
        let code_key = contract_code_key(&wasm_hash);
        let instance_key = contract_instance_key(&contract_id);

        let upgrade_bytes = vec![0xAB, 0xCD, 0xEF, 0x01, 0x02];
        let expected_hash = Hash256::hash(&upgrade_bytes);

        let env = builder
            .invoke_soroban_create_upgrade_tx(
                &source,
                42,
                &upgrade_bytes,
                code_key.clone(),
                instance_key.clone(),
                100,
            )
            .unwrap();

        let TransactionEnvelope::Tx(v1) = &env else {
            panic!("expected v1 envelope");
        };
        let tx = &v1.tx;
        assert_eq!(tx.seq_num.0, 42);

        // Operation: INVOKE_CONTRACT "write"(SCV_BYTES(upgradeBytes)).
        let op = &tx.operations[0];
        let OperationBody::InvokeHostFunction(ihf) = &op.body else {
            panic!("expected InvokeHostFunction");
        };
        let HostFunction::InvokeContract(args) = &ihf.host_function else {
            panic!("expected InvokeContract");
        };
        assert_eq!(args.function_name.0.as_slice(), b"write");
        assert_eq!(args.args.len(), 1);
        match &args.args[0] {
            ScVal::Bytes(b) => assert_eq!(b.0.as_slice(), upgrade_bytes.as_slice()),
            other => panic!("expected SCV_BYTES arg, got {other:?}"),
        }
        // Contract address is the upgrade contract instance's contract.
        match (&args.contract_address, &instance_key) {
            (ScAddress::Contract(a), LedgerKey::ContractData(cd)) => {
                assert_eq!(ScAddress::Contract(a.clone()), cd.contract);
            }
            _ => panic!("unexpected address shapes"),
        }

        // Footprint + resources live in the V1 ext SorobanTransactionData.
        let TransactionExt::V1(data) = &tx.ext else {
            panic!("expected V1 soroban ext");
        };
        let res = &data.resources;
        assert_eq!(res.instructions, CREATE_UPGRADE_INSTRUCTIONS);
        assert_eq!(res.disk_read_bytes, CREATE_UPGRADE_READ_BYTES);
        assert_eq!(res.write_bytes, CREATE_UPGRADE_WRITE_BYTES);

        // readOnly = [instance, code] (order matters).
        let ro: Vec<LedgerKey> = res.footprint.read_only.to_vec();
        assert_eq!(ro, vec![instance_key.clone(), code_key]);

        // readWrite = single temp CONTRACT_DATA keyed by sha256(upgradeBytes).
        let rw: Vec<LedgerKey> = res.footprint.read_write.to_vec();
        assert_eq!(rw.len(), 1);
        let LedgerKey::ContractData(rw_cd) = &rw[0] else {
            panic!("expected CONTRACT_DATA rw key");
        };
        assert_eq!(rw_cd.durability, ContractDataDurability::Temporary);
        match &rw_cd.key {
            ScVal::Bytes(b) => assert_eq!(b.0.as_slice(), &expected_hash.0),
            other => panic!("expected SCV_BYTES key, got {other:?}"),
        }
        // The rw entry's contract is the upgrade contract.
        if let LedgerKey::ContractData(inst_cd) = &instance_key {
            assert_eq!(rw_cd.contract, inst_cd.contract);
        }
    }

    /// Regression for the SSC mixed-image Soroban config-upgrade wedge (part 2).
    ///
    /// The create_upgrade tx invokes `write(bytes)` on the deployed contract to
    /// store the `ConfigUpgradeSet` entry. That function only exists on the
    /// `write_bytes` contract — the loadgen contract (`do_cpu_only_work`) does
    /// NOT export it. stellar-core uploads `get_write_bytes()` (not the loadgen
    /// contract) for the upgrade-setup path (`LoadGenerator.cpp:1184`). If the
    /// wrong WASM is uploaded, the `write` invoke fails at apply, the entry is
    /// never written, and the arm is rejected ("did not resolve to a VALID
    /// upgrade set"). This pins that the upgrade-setup WASM exports `write` and
    /// is distinct from the loadgen contract.
    #[test]
    fn test_upgrade_setup_wasm_exports_write_and_differs_from_loadgen() {
        let upgrade_wasm = SorobanTxBuilder::write_upgrade_bytes_wasm();

        assert!(
            !upgrade_wasm.is_empty(),
            "upgrade-setup WASM must be embedded"
        );
        assert_ne!(
            SorobanTxBuilder::write_upgrade_bytes_wasm_hash(),
            SorobanTxBuilder::loadgen_wasm_hash(),
            "upgrade-setup must use a different contract than invoke-setup"
        );

        // Parse the WASM export section (id 7) and collect exported names; assert
        // the create_upgrade-invoked function `write` is among them, and that the
        // loadgen contract does NOT export it (it only does `do_cpu_only_work`).
        let up = wasm_export_names(upgrade_wasm);
        let lg = wasm_export_names(SorobanTxBuilder::loadgen_wasm());
        assert!(
            up.iter().any(|n| n == "write"),
            "upgrade-setup WASM must export `write` (got exports {up:?})"
        );
        assert!(
            !lg.iter().any(|n| n == "write"),
            "loadgen contract must NOT export `write` (got exports {lg:?})"
        );
    }

    /// Minimal WASM export-section parser: returns the names in the export
    /// section (section id 7). Enough to assert which functions a contract
    /// exports without pulling in a full wasm crate.
    fn wasm_export_names(wasm: &[u8]) -> Vec<String> {
        fn leb(data: &[u8], pos: &mut usize) -> u32 {
            let mut result = 0u32;
            let mut shift = 0;
            loop {
                let byte = data[*pos];
                *pos += 1;
                result |= ((byte & 0x7f) as u32) << shift;
                if byte & 0x80 == 0 {
                    break;
                }
                shift += 7;
            }
            result
        }
        let mut names = Vec::new();
        // magic(4) + version(4)
        let mut pos = 8usize;
        while pos < wasm.len() {
            let section_id = wasm[pos];
            pos += 1;
            let section_len = leb(wasm, &mut pos) as usize;
            let section_end = pos + section_len;
            if section_id == 7 {
                let count = leb(wasm, &mut pos);
                for _ in 0..count {
                    let name_len = leb(wasm, &mut pos) as usize;
                    let name = String::from_utf8_lossy(&wasm[pos..pos + name_len]).into_owned();
                    pos += name_len;
                    let _kind = wasm[pos];
                    pos += 1;
                    let _index = leb(wasm, &mut pos);
                    names.push(name);
                }
                break;
            }
            pos = section_end;
        }
        names
    }

    /// Regression for the SSC mixed-image Soroban config-upgrade wedge.
    ///
    /// The deploy (`create_contract`) tx reads the uploaded contract-code (WASM)
    /// entry, so stellar-core sets `diskReadBytes = contractOverheadBytes`
    /// (`= wasmBytes.size() + 160`, TxGenerator.cpp:333 / LoadGenerator.cpp:1198).
    /// henyey previously hardcoded `disk_read_bytes = 5000`, which EXCEEDS the
    /// genesis Soroban `ledger_max_read_bytes` (3200): surge pricing relaxes only
    /// the instruction lane (not read/write bytes), so `any_greater` permanently
    /// excludes a 5000-read-byte tx from every tx set. The deploy tx could never
    /// be included, which (by sequence order on the shared root account) wedged
    /// the entire upgrade-setup chain (deploy → create_upgrade), so the config
    /// upgrade never armed/applied and `LedgerMaxInstructions` stayed at 2.5M.
    ///
    /// This pins the deploy tx's `disk_read_bytes` to the passed overhead and
    /// asserts it fits under the tiny genesis read-bytes limit. The old fixed
    /// 5000 fails the `<= GENESIS_LEDGER_MAX_READ_BYTES` assertion.
    #[test]
    fn test_create_contract_disk_read_bytes_tracks_overhead_and_fits_genesis() {
        let builder = SorobanTxBuilder::new("Test SDF Network ; September 2015".to_string());
        let source = SecretKey::from_seed(&[9u8; 32]);

        // Real loadgen WASM (1739 bytes) → overhead = wasm.len() + 160.
        let wasm = SorobanTxBuilder::loadgen_wasm();
        let wasm_hash = SorobanTxBuilder::loadgen_wasm_hash();
        let overhead = wasm.len() as u32 + 160;
        let salt = Uint256([3u8; 32]);

        let env = builder
            .create_contract_tx(&source, 100, &wasm_hash, &salt, overhead, 100)
            .unwrap();

        let TransactionEnvelope::Tx(v1) = &env else {
            panic!("expected v1 envelope");
        };
        let TransactionExt::V1(data) = &v1.tx.ext else {
            panic!("expected V1 soroban ext");
        };

        // diskReadBytes must equal contractOverheadBytes (parity).
        assert_eq!(
            data.resources.disk_read_bytes, overhead,
            "deploy diskReadBytes must equal contractOverheadBytes (TxGenerator.cpp:333)"
        );

        // ... and must fit under the genesis Soroban ledger read-bytes limit, or
        // the deploy tx can never be included in a tx set's Soroban lane.
        const GENESIS_LEDGER_MAX_READ_BYTES: u32 = 3200;
        assert!(
            data.resources.disk_read_bytes <= GENESIS_LEDGER_MAX_READ_BYTES,
            "deploy diskReadBytes {} exceeds genesis ledger_max_read_bytes {} — \
             tx would be silently excluded from every tx set (the pre-fix bug used 5000)",
            data.resources.disk_read_bytes,
            GENESIS_LEDGER_MAX_READ_BYTES
        );
    }
}
