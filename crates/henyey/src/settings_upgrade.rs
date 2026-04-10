//! Implementation of the `get-settings-upgrade-txs` CLI subcommand.
//!
//! This module ports stellar-core's `SettingsUpgradeUtils.cpp` and the relevant
//! parts of `CommandLine.cpp`/`dumpxdr.cpp` to produce the 4 transactions
//! needed for a Soroban config settings upgrade:
//!
//! 1. **Restore** the WASM contract code ledger entry (if archived)
//! 2. **Upload** the `write_upgrade_bytes` WASM
//! 3. **Create** the upgrade contract instance
//! 4. **Invoke** the contract with the serialized `ConfigUpgradeSet`
//!
//! When `--signtxs` is passed, each transaction is signed with a secret key
//! read from stdin, and the output is pairs of lines: base64 tx envelope +
//! hex tx hash. The final line is always the base64 `ConfigUpgradeSetKey`.

use base64::{engine::general_purpose::STANDARD as BASE64, Engine};
use stellar_xdr::curr::{
    ConfigUpgradeSet, ConfigUpgradeSetKey, ContractDataDurability, ContractExecutable, ContractId,
    ContractIdPreimage, ContractIdPreimageFromAddress, CreateContractArgs, DecoratedSignature,
    Hash, HashIdPreimage, HashIdPreimageContractId, HostFunction, InvokeContractArgs,
    LedgerFootprint, LedgerKey, LedgerKeyContractCode, LedgerKeyContractData, Memo, MuxedAccount,
    Operation, OperationBody, Preconditions, ReadXdr, ScAddress, ScSymbol, ScVal, SequenceNumber,
    SignatureHint, SorobanAuthorizationEntry, SorobanAuthorizedFunction,
    SorobanAuthorizedInvocation, SorobanCredentials, SorobanResources, SorobanTransactionData,
    SorobanTransactionDataExt, Transaction, TransactionEnvelope, TransactionExt,
    TransactionSignaturePayload, TransactionSignaturePayloadTaggedTransaction,
    TransactionV1Envelope, Uint256, VecM, WriteXdr,
};

use henyey_crypto::{sha256, SecretKey};

/// The pre-compiled `write_upgrade_bytes` WASM contract.
/// This is the same binary embedded in stellar-core via `soroban_test_wasms::WRITE_BYTES`.
const WRITE_BYTES_WASM: &[u8] = include_bytes!("../wasm/soroban_write_upgrade_bytes_contract.wasm");

fn single_operation(body: OperationBody) -> VecM<Operation, 100> {
    vec![Operation {
        source_account: None,
        body,
    }]
    .try_into()
    .unwrap()
}

/// Outputs of `get_create_tx` needed by `get_invoke_tx`.
struct ContractDeployment {
    /// The contract code ledger key (for the WASM upload entry).
    code_key: LedgerKey,
    /// The contract instance ledger key.
    source_ref_key: LedgerKey,
    /// The deployed contract ID.
    contract_id: Hash,
}

fn build_soroban_envelope(
    public_key: &Uint256,
    seq_num: i64,
    fee: i64,
    resource_fee: i64,
    operations: VecM<Operation, 100>,
    resources: SorobanResources,
) -> TransactionEnvelope {
    TransactionEnvelope::Tx(TransactionV1Envelope {
        tx: Transaction {
            source_account: MuxedAccount::Ed25519(public_key.clone()),
            fee: fee as u32,
            seq_num: SequenceNumber(seq_num),
            cond: Preconditions::None,
            memo: Memo::None,
            operations,
            ext: TransactionExt::V1(SorobanTransactionData {
                ext: SorobanTransactionDataExt::V0,
                resources,
                resource_fee,
            }),
        },
        signatures: VecM::default(),
    })
}

/// Build the WASM restore transaction (tx 1 of 4).
///
/// Restores the contract code ledger entry in case it has been archived.
fn get_wasm_restore_tx(
    public_key: &Uint256,
    seq_num: i64,
    add_resource_fee: i64,
) -> (TransactionEnvelope, LedgerKey) {
    let wasm_hash = sha256(WRITE_BYTES_WASM);

    let contract_code_key = LedgerKey::ContractCode(LedgerKeyContractCode {
        hash: Hash(wasm_hash.0),
    });

    let resources = SorobanResources {
        footprint: LedgerFootprint {
            read_only: VecM::default(),
            read_write: vec![contract_code_key.clone()].try_into().unwrap(),
        },
        instructions: 0,
        disk_read_bytes: 2000,
        write_bytes: 2000,
    };

    let resource_fee = 55_000_000 + add_resource_fee;
    let fee = 100_000_000 + resource_fee;

    let envelope = build_soroban_envelope(
        public_key,
        seq_num,
        fee,
        resource_fee,
        single_operation(OperationBody::RestoreFootprint(
            stellar_xdr::curr::RestoreFootprintOp {
                ext: stellar_xdr::curr::ExtensionPoint::V0,
            },
        )),
        resources,
    );

    (envelope, contract_code_key)
}

/// Build the WASM upload transaction (tx 2 of 4).
fn get_upload_tx(public_key: &Uint256, seq_num: i64) -> (TransactionEnvelope, LedgerKey) {
    let wasm = WRITE_BYTES_WASM.to_vec();
    let wasm_hash = sha256(&wasm);

    let contract_code_key = LedgerKey::ContractCode(LedgerKeyContractCode {
        hash: Hash(wasm_hash.0),
    });

    let resources = SorobanResources {
        footprint: LedgerFootprint {
            read_only: VecM::default(),
            read_write: vec![contract_code_key.clone()].try_into().unwrap(),
        },
        instructions: 2_000_000,
        disk_read_bytes: 2000,
        write_bytes: 2000,
    };

    // Note: stellar-core passes 0 for addResourceFee to getUploadTx
    let resource_fee: i64 = 55_000_000;
    let fee = 100_000_000 + resource_fee;

    let envelope = build_soroban_envelope(
        public_key,
        seq_num,
        fee,
        resource_fee,
        single_operation(OperationBody::InvokeHostFunction(
            stellar_xdr::curr::InvokeHostFunctionOp {
                host_function: HostFunction::UploadContractWasm(wasm.try_into().unwrap()),
                auth: VecM::default(),
            },
        )),
        resources,
    );

    (envelope, contract_code_key)
}

/// Build the contract creation transaction (tx 3 of 4).
fn get_create_tx(
    public_key: &Uint256,
    contract_code_key: &LedgerKey,
    network_passphrase: &str,
    seq_num: i64,
    add_resource_fee: i64,
) -> (TransactionEnvelope, ContractDeployment) {
    let wasm_hash = match contract_code_key {
        LedgerKey::ContractCode(k) => k.hash.clone(),
        _ => panic!("Expected ContractCode ledger key"),
    };

    // Generate a salt. stellar-core uses autocheck::generator<Hash>()(5) which
    // produces a pseudo-random hash. We use a fixed salt since the exact value
    // doesn't matter — it just needs to produce a unique contract ID.
    let salt = sha256(b"settings-upgrade-salt");

    // Build contract ID preimage
    let id_preimage = ContractIdPreimage::Address(ContractIdPreimageFromAddress {
        address: ScAddress::Account(stellar_xdr::curr::AccountId(
            stellar_xdr::curr::PublicKey::PublicKeyTypeEd25519(public_key.clone()),
        )),
        salt: Uint256(salt.0),
    });

    // Compute contract ID
    let network_id = sha256(network_passphrase.as_bytes());
    let full_preimage = HashIdPreimage::ContractId(HashIdPreimageContractId {
        network_id: Hash(network_id.0),
        contract_id_preimage: id_preimage.clone(),
    });

    let full_preimage_bytes = full_preimage
        .to_xdr(stellar_xdr::curr::Limits::none())
        .unwrap();
    let contract_id = Hash(sha256(&full_preimage_bytes).0);

    // Build create contract operation
    let create_args = CreateContractArgs {
        contract_id_preimage: id_preimage.clone(),
        executable: ContractExecutable::Wasm(wasm_hash.clone()),
    };

    // Build auth entry
    let auth = SorobanAuthorizationEntry {
        credentials: SorobanCredentials::SourceAccount,
        root_invocation: SorobanAuthorizedInvocation {
            function: SorobanAuthorizedFunction::CreateContractHostFn(CreateContractArgs {
                contract_id_preimage: id_preimage,
                executable: ContractExecutable::Wasm(wasm_hash),
            }),
            sub_invocations: VecM::default(),
        },
    };

    // Build contract source ref ledger key (CONTRACT_DATA for instance)
    let contract_source_ref_key = LedgerKey::ContractData(LedgerKeyContractData {
        contract: ScAddress::Contract(ContractId(contract_id.clone())),
        key: ScVal::LedgerKeyContractInstance,
        durability: ContractDataDurability::Persistent,
    });

    let resources = SorobanResources {
        footprint: LedgerFootprint {
            read_only: vec![contract_code_key.clone()].try_into().unwrap(),
            read_write: vec![contract_source_ref_key.clone()].try_into().unwrap(),
        },
        instructions: 2_000_000,
        disk_read_bytes: 2000,
        write_bytes: 120,
    };

    let resource_fee = 15_000_000 + add_resource_fee;
    let fee = 25_000_000 + resource_fee;

    let envelope = build_soroban_envelope(
        public_key,
        seq_num,
        fee,
        resource_fee,
        single_operation(OperationBody::InvokeHostFunction(
            stellar_xdr::curr::InvokeHostFunctionOp {
                host_function: HostFunction::CreateContract(create_args),
                auth: vec![auth].try_into().unwrap(),
            },
        )),
        resources,
    );

    (
        envelope,
        ContractDeployment {
            code_key: contract_code_key.clone(),
            source_ref_key: contract_source_ref_key,
            contract_id,
        },
    )
}

/// Build the invoke transaction (tx 4 of 4).
fn get_invoke_tx(
    public_key: &Uint256,
    deployment: &ContractDeployment,
    upgrade_set: &ConfigUpgradeSet,
    seq_num: i64,
    add_resource_fee: i64,
) -> (TransactionEnvelope, ConfigUpgradeSetKey) {
    let upgrade_set_bytes = upgrade_set
        .to_xdr(stellar_xdr::curr::Limits::none())
        .unwrap();

    // Build invoke contract args
    let addr = ScAddress::Contract(ContractId(deployment.contract_id.clone()));

    let function_name = ScSymbol("write".try_into().unwrap());

    let args_val = ScVal::Bytes(stellar_xdr::curr::ScBytes(
        upgrade_set_bytes.clone().try_into().unwrap(),
    ));

    let invoke_args = InvokeContractArgs {
        contract_address: addr.clone(),
        function_name,
        args: vec![args_val].try_into().unwrap(),
    };

    // Build the upgrade data ledger key
    let upgrade_hash = sha256(&upgrade_set_bytes);
    let upgrade_hash_val = ScVal::Bytes(stellar_xdr::curr::ScBytes(
        Hash(upgrade_hash.0)
            .to_xdr(stellar_xdr::curr::Limits::none())
            .unwrap()
            .try_into()
            .unwrap(),
    ));

    let upgrade_key = LedgerKey::ContractData(LedgerKeyContractData {
        contract: addr,
        key: upgrade_hash_val,
        durability: ContractDataDurability::Temporary,
    });

    let resources = SorobanResources {
        footprint: LedgerFootprint {
            read_only: vec![
                deployment.source_ref_key.clone(),
                deployment.code_key.clone(),
            ]
            .try_into()
            .unwrap(),
            read_write: vec![upgrade_key].try_into().unwrap(),
        },
        instructions: 2_000_000,
        disk_read_bytes: (WRITE_BYTES_WASM.len() as u32) + 300,
        write_bytes: (upgrade_set_bytes.len() as u32) + 200,
    };

    let resource_fee = 95_000_000 + add_resource_fee;
    let fee = 100_000_000 + resource_fee;

    let envelope = build_soroban_envelope(
        public_key,
        seq_num,
        fee,
        resource_fee,
        single_operation(OperationBody::InvokeHostFunction(
            stellar_xdr::curr::InvokeHostFunctionOp {
                host_function: HostFunction::InvokeContract(invoke_args),
                auth: VecM::default(),
            },
        )),
        resources,
    );

    let key = ConfigUpgradeSetKey {
        contract_id: ContractId(deployment.contract_id.clone()),
        content_hash: Hash(upgrade_hash.0),
    };

    (envelope, key)
}

/// Compute the transaction hash for a given transaction envelope and network passphrase.
fn compute_tx_hash(
    envelope: &TransactionEnvelope,
    network_passphrase: &str,
) -> henyey_common::Hash256 {
    let network_id = sha256(network_passphrase.as_bytes());

    let payload = match envelope {
        TransactionEnvelope::Tx(v1) => TransactionSignaturePayload {
            network_id: Hash(network_id.0),
            tagged_transaction: TransactionSignaturePayloadTaggedTransaction::Tx(v1.tx.clone()),
        },
        _ => panic!("Expected ENVELOPE_TYPE_TX"),
    };

    let payload_bytes = payload.to_xdr(stellar_xdr::curr::Limits::none()).unwrap();
    sha256(&payload_bytes)
}

/// Sign a transaction envelope with the given secret key.
fn sign_tx(
    envelope: &mut TransactionEnvelope,
    secret_key: &SecretKey,
    network_passphrase: &str,
) -> henyey_common::Hash256 {
    let tx_hash = compute_tx_hash(envelope, network_passphrase);

    let signature = secret_key.sign(&tx_hash.0);
    let pub_key = secret_key.public_key();
    let pub_key_bytes = pub_key.as_bytes();

    // Hint is the last 4 bytes of the public key
    let hint = SignatureHint([
        pub_key_bytes[28],
        pub_key_bytes[29],
        pub_key_bytes[30],
        pub_key_bytes[31],
    ]);

    let decorated = DecoratedSignature {
        hint,
        signature: stellar_xdr::curr::Signature(signature.as_bytes().to_vec().try_into().unwrap()),
    };

    match envelope {
        TransactionEnvelope::Tx(ref mut v1) => {
            let mut sigs: Vec<DecoratedSignature> = v1.signatures.to_vec();
            sigs.push(decorated);
            v1.signatures = sigs.try_into().unwrap();
        }
        _ => panic!("Expected ENVELOPE_TYPE_TX"),
    }

    tx_hash
}

/// Parameters for the `get-settings-upgrade-txs` CLI command.
pub struct SettingsUpgradeParams<'a> {
    pub public_key_str: &'a str,
    pub seq_num: i64,
    pub network_passphrase: &'a str,
    pub xdr_base64: &'a str,
    pub sign_txs: bool,
    pub add_resource_fee: i64,
}

/// Execute the `get-settings-upgrade-txs` command.
pub fn run(params: &SettingsUpgradeParams<'_>) -> anyhow::Result<()> {
    // Decode the ConfigUpgradeSet from base64 XDR
    let xdr_bytes = BASE64
        .decode(params.xdr_base64)
        .map_err(|e| anyhow::anyhow!("Failed to decode base64 XDR: {}", e))?;
    let upgrade_set = ConfigUpgradeSet::from_xdr(&xdr_bytes, stellar_xdr::curr::Limits::none())
        .map_err(|e| anyhow::anyhow!("Failed to decode ConfigUpgradeSet XDR: {}", e))?;

    // Parse public key from StrKey
    let pk = henyey_crypto::PublicKey::from_strkey(params.public_key_str)
        .map_err(|e| anyhow::anyhow!("Failed to parse public key: {}", e))?;
    let public_key = Uint256(*pk.as_bytes());

    // Build the 4 transactions
    let (mut restore_tx, _restore_key) =
        get_wasm_restore_tx(&public_key, params.seq_num + 1, params.add_resource_fee);

    // Note: stellar-core passes 0 for addResourceFee to getUploadTx
    let (mut upload_tx, contract_code_key) = get_upload_tx(&public_key, params.seq_num + 2);

    let (mut create_tx, deployment) = get_create_tx(
        &public_key,
        &contract_code_key,
        params.network_passphrase,
        params.seq_num + 3,
        params.add_resource_fee,
    );

    let (mut invoke_tx, upgrade_set_key) = get_invoke_tx(
        &public_key,
        &deployment,
        &upgrade_set,
        params.seq_num + 4,
        params.add_resource_fee,
    );

    if params.sign_txs {
        // Read secret key from stdin
        let secret_str = read_secret_from_stdin()?;
        let secret_key = SecretKey::from_strkey(&secret_str)
            .map_err(|e| anyhow::anyhow!("Failed to parse secret key: {}", e))?;

        // Sign and output each transaction (base64 envelope + hex tx hash)
        let txs = [
            &mut restore_tx,
            &mut upload_tx,
            &mut create_tx,
            &mut invoke_tx,
        ];

        for tx in txs {
            let tx_hash = sign_tx(tx, &secret_key, params.network_passphrase);
            let tx_bytes = tx.to_xdr(stellar_xdr::curr::Limits::none())?;
            println!("{}", BASE64.encode(&tx_bytes));
            println!("{}", hex::encode(tx_hash.0));
        }
    } else {
        // Output unsigned transactions with labels on stderr and tx hashes on stdout.
        // stellar-core outputs txsToSign[0..2] (restore, upload, create) with labels,
        // but does NOT output the invoke tx separately in unsigned mode.
        let txs = [&restore_tx, &upload_tx, &create_tx];

        let labels = [
            "Unsigned TransactionEnvelope to upload upgrade WASM",
            "Unsigned TransactionEnvelope to create upgrade contract",
            "Unsigned TransactionEnvelope to invoke contract with upgrade bytes",
        ];

        for (i, label) in labels.iter().enumerate() {
            eprintln!("{}", label);
            let tx_bytes = txs[i].to_xdr(stellar_xdr::curr::Limits::none())?;
            println!("{}", BASE64.encode(&tx_bytes));
            let tx_hash = compute_tx_hash(txs[i], params.network_passphrase);
            println!("{}", hex::encode(tx_hash.0));
        }
    }

    // Always output the ConfigUpgradeSetKey as the last line
    let key_bytes = upgrade_set_key.to_xdr(stellar_xdr::curr::Limits::none())?;
    println!("{}", BASE64.encode(&key_bytes));

    Ok(())
}

/// Read a secret key from stdin, trimming whitespace.
fn read_secret_from_stdin() -> anyhow::Result<String> {
    Ok(std::io::read_to_string(std::io::stdin())?
        .trim()
        .to_string())
}
