//! Soroban transaction building utilities for load generation.
//!
//! Provides builders for constructing Soroban `TransactionEnvelope`s
//! (upload WASM, create contract, invoke contract) with correct
//! `SorobanTransactionData` extensions.

use henyey_common::{Hash256, NetworkId};
use henyey_crypto::{sign_hash, SecretKey};
use stellar_xdr::curr::{
    ContractDataDurability, ContractExecutable, ContractId, ContractIdPreimage,
    ContractIdPreimageFromAddress, CreateContractArgs, DecoratedSignature, Hash, HashIdPreimage,
    HashIdPreimageContractId, HostFunction, Int128Parts, InvokeContractArgs, InvokeHostFunctionOp,
    LedgerFootprint, LedgerKey, LedgerKeyContractCode, LedgerKeyContractData, Limits, Memo,
    MuxedAccount, Operation, OperationBody, Preconditions, ScAddress, ScSymbol, ScVal, ScVec,
    SequenceNumber, Signature, SignatureHint, SorobanAuthorizationEntry, SorobanAuthorizedFunction,
    SorobanAuthorizedInvocation, SorobanCredentials, SorobanResources, SorobanTransactionData,
    SorobanTransactionDataExt, Transaction, TransactionEnvelope, TransactionExt,
    TransactionV1Envelope, Uint256, VecM, WriteXdr,
};

/// Embedded loadgen test contract WASM (from stellar-core P21 test wasms).
///
/// This contract exposes `do_work(guest_cycles, host_cycles, n_entries, kb_per_entry)`
/// for CPU and IO load generation.
pub(crate) const LOADGEN_WASM: &[u8] = include_bytes!(
    "../../../stellar-core/src/rust/soroban/p21/soroban-test-wasms/wasm-workspace/opt/curr/loadgen.wasm"
);

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
            instructions: 2_500_000,
            disk_read_bytes: (wasm.len() as u32).saturating_add(500),
            write_bytes: (wasm.len() as u32).saturating_add(500),
        };

        // Resource fee estimate: generous default for simulation
        let resource_fee = 50_000_000i64;

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
        inclusion_fee: u32,
    ) -> anyhow::Result<TransactionEnvelope> {
        let deployer_address = ScAddress::Account(stellar_xdr::curr::AccountId(
            stellar_xdr::curr::PublicKey::PublicKeyTypeEd25519(Uint256(
                *source.public_key().as_bytes(),
            )),
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
        let instance_key = LedgerKey::ContractData(LedgerKeyContractData {
            contract: ScAddress::Contract(ContractId(Hash(contract_id.0))),
            key: ScVal::LedgerKeyContractInstance,
            durability: ContractDataDurability::Persistent,
        });

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
            instructions: 1_000_000,
            disk_read_bytes: 5000,
            write_bytes: 300,
        };

        let resource_fee = 10_000_000i64;
        self.build_soroban_envelope(source, sequence, op, resources, resource_fee, inclusion_fee)
    }

    /// Build a contract invocation transaction.
    ///
    /// Matches stellar-core `TxGenerator::invokeSorobanLoadTransaction()`.
    #[allow(clippy::too_many_arguments)]
    pub fn invoke_contract_tx(
        &self,
        source: &SecretKey,
        sequence: i64,
        contract_id: &Hash256,
        function_name: &str,
        args: Vec<ScVal>,
        read_only_keys: Vec<LedgerKey>,
        read_write_keys: Vec<LedgerKey>,
        instructions: u32,
        read_bytes: u32,
        write_bytes: u32,
        inclusion_fee: u32,
    ) -> anyhow::Result<TransactionEnvelope> {
        let contract_address = ScAddress::Contract(ContractId(Hash(contract_id.0)));

        let host_fn = HostFunction::InvokeContract(InvokeContractArgs {
            contract_address,
            function_name: ScSymbol(
                function_name
                    .try_into()
                    .map_err(|_| anyhow::anyhow!("function name too long"))?,
            ),
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
                read_only: read_only_keys.try_into().unwrap_or_default(),
                read_write: read_write_keys.try_into().unwrap_or_default(),
            },
            instructions,
            disk_read_bytes: read_bytes,
            write_bytes,
        };

        let resource_fee = 50_000_000i64;
        self.build_soroban_envelope(source, sequence, op, resources, resource_fee, inclusion_fee)
    }

    /// Build a SAC (Stellar Asset Contract) creation transaction.
    ///
    /// Matches stellar-core `TxGenerator::createSACTransaction()`.
    pub fn create_sac_tx(
        &self,
        source: &SecretKey,
        sequence: i64,
        asset: stellar_xdr::curr::Asset,
        inclusion_fee: u32,
    ) -> anyhow::Result<TransactionEnvelope> {
        let preimage = ContractIdPreimage::Asset(asset);

        let executable = ContractExecutable::StellarAsset;

        let contract_id = compute_contract_id(&preimage, &self.network_passphrase)?;

        let instance_key = LedgerKey::ContractData(LedgerKeyContractData {
            contract: ScAddress::Contract(ContractId(Hash(contract_id.0))),
            key: ScVal::LedgerKeyContractInstance,
            durability: ContractDataDurability::Persistent,
        });

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
            instructions: 1_000_000,
            disk_read_bytes: 5000,
            write_bytes: 300,
        };

        let resource_fee = 10_000_000i64;
        self.build_soroban_envelope(source, sequence, op, resources, resource_fee, inclusion_fee)
    }

    /// Build a SAC `transfer` invocation transaction.
    ///
    /// Matches stellar-core `TxGenerator::invokeSACPayment()`.
    #[allow(clippy::too_many_arguments)]
    pub fn invoke_sac_transfer_tx(
        &self,
        source: &SecretKey,
        sequence: i64,
        sac_contract_id: &Hash256,
        from_address: ScAddress,
        to_address: ScAddress,
        amount: i128,
        instance_keys: Vec<LedgerKey>,
        inclusion_fee: u32,
    ) -> anyhow::Result<TransactionEnvelope> {
        let args = vec![
            ScVal::Address(from_address.clone()),
            ScVal::Address(to_address.clone()),
            make_i128(amount),
        ];

        let auth = SorobanAuthorizationEntry {
            credentials: SorobanCredentials::SourceAccount,
            root_invocation: SorobanAuthorizedInvocation {
                function: SorobanAuthorizedFunction::ContractFn(InvokeContractArgs {
                    contract_address: ScAddress::Contract(ContractId(Hash(sac_contract_id.0))),
                    function_name: ScSymbol("transfer".try_into().unwrap()),
                    args: args.clone().try_into().unwrap_or_default(),
                }),
                sub_invocations: VecM::default(),
            },
        };

        let contract_address = ScAddress::Contract(ContractId(Hash(sac_contract_id.0)));

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
        let mut read_write_keys = Vec::new();

        // Source account key
        if let ScAddress::Account(ref aid) = from_address {
            read_write_keys.push(LedgerKey::Account(
                stellar_xdr::curr::LedgerKeyAccount {
                    account_id: aid.clone(),
                },
            ));
        }

        // Destination balance key (CONTRACT_DATA with Balance + to_address)
        match &to_address {
            ScAddress::Contract(_) => {
                read_write_keys.push(LedgerKey::ContractData(
                    stellar_xdr::curr::LedgerKeyContractData {
                        contract: ScAddress::Contract(ContractId(Hash(sac_contract_id.0))),
                        key: ScVal::Vec(Some(stellar_xdr::curr::ScVec(
                            vec![
                                ScVal::Symbol(ScSymbol("Balance".try_into().unwrap())),
                                ScVal::Address(to_address),
                            ]
                            .try_into()
                            .unwrap_or_default(),
                        ))),
                        durability: stellar_xdr::curr::ContractDataDurability::Persistent,
                    },
                ));
            }
            ScAddress::Account(ref aid) => {
                read_write_keys.push(LedgerKey::Account(
                    stellar_xdr::curr::LedgerKeyAccount {
                        account_id: aid.clone(),
                    },
                ));
            }
            _ => {} // MuxedAccount, ClaimableBalance, LiquidityPool not used in load test
        }

        let resources = SorobanResources {
            footprint: LedgerFootprint {
                read_only: instance_keys.try_into().unwrap_or_default(),
                read_write: read_write_keys.try_into().unwrap_or_default(),
            },
            // stellar-core uses 250K instructions but our non-typed host API (P25)
            // meters XDR deserialization, consuming ~263K+ for a SAC transfer.
            // Use 2M with generous I/O limits to avoid ResourceLimitExceeded in load tests.
            instructions: 2_000_000,
            disk_read_bytes: 10_000,
            write_bytes: 10_000,
        };

        let resource_fee = 10_000_000i64;
        self.build_soroban_envelope(source, sequence, op, resources, resource_fee, inclusion_fee)
    }

    /// Build a batch transfer invocation transaction.
    ///
    /// Matches stellar-core `TxGenerator::invokeBatchTransfer()`.
    #[allow(clippy::too_many_arguments)]
    pub fn invoke_batch_transfer_tx(
        &self,
        source: &SecretKey,
        sequence: i64,
        batch_contract_id: &Hash256,
        sac_address: ScVal,
        destinations: Vec<ScVal>,
        instance_keys: Vec<LedgerKey>,
        inclusion_fee: u32,
    ) -> anyhow::Result<TransactionEnvelope> {
        let batch_size = destinations.len() as u32;
        let dest_vec = ScVal::Vec(Some(ScVec(destinations.try_into().unwrap_or_default())));
        let args = vec![sac_address, dest_vec];

        let contract_address = ScAddress::Contract(ContractId(Hash(batch_contract_id.0)));

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
                read_only: instance_keys.try_into().unwrap_or_default(),
                read_write: VecM::default(),
            },
            instructions: 500_000 * batch_size,
            disk_read_bytes: 800 * batch_size,
            write_bytes: 800 * batch_size,
        };

        let resource_fee = 50_000_000i64;
        self.build_soroban_envelope(source, sequence, op, resources, resource_fee, inclusion_fee)
    }

    /// Get the embedded loadgen test contract WASM bytes.
    pub fn loadgen_wasm() -> &'static [u8] {
        LOADGEN_WASM
    }

    /// Compute the SHA-256 hash of the loadgen WASM.
    pub fn loadgen_wasm_hash() -> Hash256 {
        Hash256::hash(LOADGEN_WASM)
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
        let total_fee = inclusion_fee + resource_fee as u32;
        let source_muxed = MuxedAccount::Ed25519(Uint256(*source.public_key().as_bytes()));

        let soroban_data = SorobanTransactionData {
            ext: SorobanTransactionDataExt::V0,
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

/// Construct an `ScVal::I128` from a Rust `i128`.
pub fn make_i128(value: i128) -> ScVal {
    ScVal::I128(Int128Parts {
        hi: (value >> 64) as i64,
        lo: value as u64,
    })
}

/// Construct an `ScAddress::Account` from a public key.
pub fn make_account_address(public_key: &henyey_crypto::PublicKey) -> ScAddress {
    ScAddress::Account(stellar_xdr::curr::AccountId(
        stellar_xdr::curr::PublicKey::PublicKeyTypeEd25519(Uint256(*public_key.as_bytes())),
    ))
}

/// Construct an `ScAddress::Contract` from a contract hash.
pub fn make_contract_address(contract_id: &Hash256) -> ScAddress {
    ScAddress::Contract(ContractId(Hash(contract_id.0)))
}

/// Construct an `ScVal::U32`.
pub fn make_u32(value: u32) -> ScVal {
    ScVal::U32(value)
}

/// Construct an `ScVal::U64`.
pub fn make_u64(value: u64) -> ScVal {
    ScVal::U64(value)
}

/// Build a `LedgerKey` for a contract instance.
pub fn contract_instance_key(contract_id: &Hash256) -> LedgerKey {
    LedgerKey::ContractData(LedgerKeyContractData {
        contract: ScAddress::Contract(ContractId(Hash(contract_id.0))),
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
    let frame = henyey_tx::TransactionFrame::from_owned_with_network(envelope.clone(), network_id);
    let hash = frame.hash(&network_id)?;
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
        let preimage = ContractIdPreimage::Asset(stellar_xdr::curr::Asset::Native);
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
}
