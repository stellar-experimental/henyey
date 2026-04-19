use std::path::PathBuf;

use henyey_common::Hash256;
use henyey_crypto::PublicKey;
use serde_json::Value as JsonValue;
use stellar_xdr::curr::{
    ExtendFootprintTtlResult, Hash, InnerTransactionResult, InnerTransactionResultExt,
    InnerTransactionResultPair, InnerTransactionResultResult, InvokeHostFunctionResult,
    LedgerCloseValueSignature, LedgerHeader, LedgerHeaderExt, LedgerHeaderHistoryEntry,
    LedgerHeaderHistoryEntryExt, NodeId, OperationResult, OperationResultTr, PaymentResult,
    PublicKey as XdrPublicKey, RestoreFootprintResult, Signature, StellarValue, StellarValueExt,
    TimePoint, TransactionResult, TransactionResultExt, TransactionResultPair,
    TransactionResultResult, TransactionResultSet, Uint256, VecM,
};

fn testdata_path(name: &str) -> PathBuf {
    let mut path = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    path.push("..");
    path.push("..");
    path.push("testdata");
    path.push("ledger-close-meta");
    path.push(name);
    path
}

fn load_json(name: &str) -> JsonValue {
    let path = testdata_path(name);
    let payload =
        std::fs::read_to_string(&path).unwrap_or_else(|e| panic!("read {}: {}", path.display(), e));
    serde_json::from_str(&payload).unwrap_or_else(|e| panic!("parse {}: {}", path.display(), e))
}

fn parse_hash(value: &JsonValue, ctx: &str) -> Hash {
    let text = value
        .as_str()
        .unwrap_or_else(|| panic!("{}: expected hex string", ctx));
    let hash = Hash256::from_hex(text).unwrap_or_else(|e| panic!("{}: invalid hex: {}", ctx, e));
    Hash::from(hash)
}

fn parse_u32(value: &JsonValue, ctx: &str) -> u32 {
    value
        .as_u64()
        .unwrap_or_else(|| panic!("{}: expected u32", ctx))
        .try_into()
        .unwrap_or_else(|_| panic!("{}: out of range", ctx))
}

fn parse_u64(value: &JsonValue, ctx: &str) -> u64 {
    value
        .as_u64()
        .unwrap_or_else(|| panic!("{}: expected u64", ctx))
}

fn parse_i64(value: &JsonValue, ctx: &str) -> i64 {
    value
        .as_i64()
        .unwrap_or_else(|| panic!("{}: expected i64", ctx))
}

fn parse_stellar_value(value: &JsonValue) -> StellarValue {
    let obj = value
        .as_object()
        .unwrap_or_else(|| panic!("scpValue: expected object"));
    let tx_set_hash = parse_hash(
        obj.get("txSetHash")
            .unwrap_or_else(|| panic!("scpValue.txSetHash missing")),
        "scpValue.txSetHash",
    );
    let close_time = parse_u32(
        obj.get("closeTime")
            .unwrap_or_else(|| panic!("scpValue.closeTime missing")),
        "scpValue.closeTime",
    );
    let upgrades = VecM::default();
    let ext_value = obj
        .get("ext")
        .unwrap_or_else(|| panic!("scpValue.ext missing"));
    let ext = parse_stellar_value_ext(ext_value);

    StellarValue {
        tx_set_hash,
        close_time: TimePoint(close_time as u64),
        upgrades,
        ext,
    }
}

fn parse_stellar_value_ext(value: &JsonValue) -> StellarValueExt {
    let obj = value
        .as_object()
        .unwrap_or_else(|| panic!("scpValue.ext: expected object"));
    let tag = obj
        .get("v")
        .and_then(|v| v.as_str())
        .unwrap_or_else(|| panic!("scpValue.ext.v missing"));

    match tag {
        "STELLAR_VALUE_BASIC" => StellarValueExt::Basic,
        "STELLAR_VALUE_SIGNED" => {
            let sig_obj = obj
                .get("lcValueSignature")
                .unwrap_or_else(|| panic!("scpValue.ext.lcValueSignature missing"))
                .as_object()
                .unwrap_or_else(|| panic!("scpValue.ext.lcValueSignature: expected object"));
            let node_str = sig_obj
                .get("nodeID")
                .and_then(|v| v.as_str())
                .unwrap_or_else(|| panic!("lcValueSignature.nodeID missing"));
            let pk = PublicKey::from_strkey(node_str)
                .unwrap_or_else(|e| panic!("lcValueSignature.nodeID invalid: {}", e));
            let node_id = NodeId(XdrPublicKey::PublicKeyTypeEd25519(Uint256(*pk.as_bytes())));

            let sig_hex = sig_obj
                .get("signature")
                .and_then(|v| v.as_str())
                .unwrap_or_else(|| panic!("lcValueSignature.signature missing"));
            let sig_bytes = hex::decode(sig_hex)
                .unwrap_or_else(|e| panic!("lcValueSignature.signature invalid: {}", e));
            let signature = Signature(
                sig_bytes
                    .try_into()
                    .unwrap_or_else(|_| panic!("lcValueSignature.signature wrong length")),
            );

            StellarValueExt::Signed(LedgerCloseValueSignature { node_id, signature })
        }
        other => panic!("scpValue.ext.v unsupported: {}", other),
    }
}

fn parse_op_results(value: &JsonValue) -> VecM<OperationResult> {
    let items = value
        .as_array()
        .unwrap_or_else(|| panic!("tx result results: expected array"));
    let mut results = Vec::with_capacity(items.len());
    for item in items {
        let tr = item
            .get("tr")
            .unwrap_or_else(|| panic!("op result missing tr"))
            .as_object()
            .unwrap_or_else(|| panic!("op result tr: expected object"));
        let tr_type = tr
            .get("type")
            .and_then(|v| v.as_str())
            .unwrap_or_else(|| panic!("op result tr.type missing"));
        let op_result = match tr_type {
            "PAYMENT" => OperationResultTr::Payment(PaymentResult::Success),
            "INVOKE_HOST_FUNCTION" => {
                let result = tr
                    .get("invokeHostFunctionResult")
                    .unwrap_or_else(|| panic!("invokeHostFunctionResult missing"))
                    .as_object()
                    .unwrap_or_else(|| panic!("invokeHostFunctionResult: expected object"));
                let code = result
                    .get("code")
                    .and_then(|v| v.as_str())
                    .unwrap_or_else(|| panic!("invokeHostFunctionResult.code missing"));
                let invoke_result = match code {
                    "INVOKE_HOST_FUNCTION_SUCCESS" => {
                        let success = result
                            .get("success")
                            .unwrap_or_else(|| panic!("invokeHostFunctionResult.success missing"));
                        InvokeHostFunctionResult::Success(parse_hash(
                            success,
                            "invokeHostFunctionResult.success",
                        ))
                    }
                    "INVOKE_HOST_FUNCTION_RESOURCE_LIMIT_EXCEEDED" => {
                        InvokeHostFunctionResult::ResourceLimitExceeded
                    }
                    "INVOKE_HOST_FUNCTION_MALFORMED" => InvokeHostFunctionResult::Malformed,
                    "INVOKE_HOST_FUNCTION_TRAPPED" => InvokeHostFunctionResult::Trapped,
                    "INVOKE_HOST_FUNCTION_ENTRY_ARCHIVED" => {
                        InvokeHostFunctionResult::EntryArchived
                    }
                    "INVOKE_HOST_FUNCTION_INSUFFICIENT_REFUNDABLE_FEE" => {
                        InvokeHostFunctionResult::InsufficientRefundableFee
                    }
                    other => panic!("unsupported invokeHostFunctionResult code {}", other),
                };
                OperationResultTr::InvokeHostFunction(invoke_result)
            }
            "EXTEND_FOOTPRINT_TTL" => {
                let result = tr
                    .get("extendFootprintTTLResult")
                    .unwrap_or_else(|| panic!("extendFootprintTTLResult missing"))
                    .as_object()
                    .unwrap_or_else(|| panic!("extendFootprintTTLResult: expected object"));
                let code = result
                    .get("code")
                    .and_then(|v| v.as_str())
                    .unwrap_or_else(|| panic!("extendFootprintTTLResult.code missing"));
                let extend_result = match code {
                    "EXTEND_FOOTPRINT_TTL_SUCCESS" => ExtendFootprintTtlResult::Success,
                    "EXTEND_FOOTPRINT_TTL_MALFORMED" => ExtendFootprintTtlResult::Malformed,
                    "EXTEND_FOOTPRINT_TTL_RESOURCE_LIMIT_EXCEEDED" => {
                        ExtendFootprintTtlResult::ResourceLimitExceeded
                    }
                    "EXTEND_FOOTPRINT_TTL_INSUFFICIENT_REFUNDABLE_FEE" => {
                        ExtendFootprintTtlResult::InsufficientRefundableFee
                    }
                    other => panic!("unsupported extendFootprintTTLResult code {}", other),
                };
                OperationResultTr::ExtendFootprintTtl(extend_result)
            }
            "RESTORE_FOOTPRINT" => {
                let result = tr
                    .get("restoreFootprintResult")
                    .unwrap_or_else(|| panic!("restoreFootprintResult missing"))
                    .as_object()
                    .unwrap_or_else(|| panic!("restoreFootprintResult: expected object"));
                let code = result
                    .get("code")
                    .and_then(|v| v.as_str())
                    .unwrap_or_else(|| panic!("restoreFootprintResult.code missing"));
                let restore_result = match code {
                    "RESTORE_FOOTPRINT_SUCCESS" => RestoreFootprintResult::Success,
                    "RESTORE_FOOTPRINT_MALFORMED" => RestoreFootprintResult::Malformed,
                    "RESTORE_FOOTPRINT_RESOURCE_LIMIT_EXCEEDED" => {
                        RestoreFootprintResult::ResourceLimitExceeded
                    }
                    "RESTORE_FOOTPRINT_INSUFFICIENT_REFUNDABLE_FEE" => {
                        RestoreFootprintResult::InsufficientRefundableFee
                    }
                    other => panic!("unsupported restoreFootprintResult code {}", other),
                };
                OperationResultTr::RestoreFootprint(restore_result)
            }
            other => panic!("unsupported op result type {}", other),
        };
        results.push(OperationResult::OpInner(op_result));
    }
    results
        .try_into()
        .expect("tx result results VecM conversion failed")
}

fn parse_inner_transaction_result(value: &JsonValue) -> InnerTransactionResult {
    let obj = value
        .as_object()
        .unwrap_or_else(|| panic!("inner result: expected object"));
    let fee_charged = parse_i64(
        obj.get("feeCharged")
            .unwrap_or_else(|| panic!("inner result feeCharged missing")),
        "inner result feeCharged",
    );
    let result_obj = obj
        .get("result")
        .unwrap_or_else(|| panic!("inner result result missing"))
        .as_object()
        .unwrap_or_else(|| panic!("inner result result: expected object"));
    let code = result_obj
        .get("code")
        .and_then(|v| v.as_str())
        .unwrap_or_else(|| panic!("inner result code missing"));
    let result = match code {
        "txSUCCESS" => {
            let ops = parse_op_results(
                result_obj
                    .get("results")
                    .unwrap_or_else(|| panic!("inner result results missing")),
            );
            InnerTransactionResultResult::TxSuccess(ops)
        }
        "txFAILED" => {
            let ops = parse_op_results(
                result_obj
                    .get("results")
                    .unwrap_or_else(|| panic!("inner result results missing")),
            );
            InnerTransactionResultResult::TxFailed(ops)
        }
        other => panic!("unsupported inner tx result code {}", other),
    };
    InnerTransactionResult {
        fee_charged,
        result,
        ext: InnerTransactionResultExt::V0,
    }
}

fn parse_transaction_result(value: &JsonValue) -> TransactionResult {
    let obj = value
        .as_object()
        .unwrap_or_else(|| panic!("tx result: expected object"));
    let fee_charged = parse_i64(
        obj.get("feeCharged")
            .unwrap_or_else(|| panic!("tx result feeCharged missing")),
        "tx result feeCharged",
    );
    let result_obj = obj
        .get("result")
        .unwrap_or_else(|| panic!("tx result result missing"))
        .as_object()
        .unwrap_or_else(|| panic!("tx result result: expected object"));
    let code = result_obj
        .get("code")
        .and_then(|v| v.as_str())
        .unwrap_or_else(|| panic!("tx result code missing"));
    let result = match code {
        "txSUCCESS" => {
            let ops = parse_op_results(
                result_obj
                    .get("results")
                    .unwrap_or_else(|| panic!("tx result results missing")),
            );
            TransactionResultResult::TxSuccess(ops)
        }
        "txFAILED" => {
            let ops = parse_op_results(
                result_obj
                    .get("results")
                    .unwrap_or_else(|| panic!("tx result results missing")),
            );
            TransactionResultResult::TxFailed(ops)
        }
        "txFEE_BUMP_INNER_SUCCESS" | "txFEE_BUMP_INNER_FAILED" => {
            let inner_pair_obj = result_obj
                .get("innerResultPair")
                .unwrap_or_else(|| panic!("tx fee bump innerResultPair missing"))
                .as_object()
                .unwrap_or_else(|| panic!("innerResultPair: expected object"));
            let inner_hash = parse_hash(
                inner_pair_obj
                    .get("transactionHash")
                    .unwrap_or_else(|| panic!("innerResultPair.transactionHash missing")),
                "innerResultPair.transactionHash",
            );
            let inner_result = parse_inner_transaction_result(
                inner_pair_obj
                    .get("result")
                    .unwrap_or_else(|| panic!("innerResultPair.result missing")),
            );
            let inner_pair = InnerTransactionResultPair {
                transaction_hash: inner_hash,
                result: inner_result,
            };
            if code == "txFEE_BUMP_INNER_SUCCESS" {
                TransactionResultResult::TxFeeBumpInnerSuccess(inner_pair)
            } else {
                TransactionResultResult::TxFeeBumpInnerFailed(inner_pair)
            }
        }
        other => panic!("unsupported tx result code {}", other),
    };
    TransactionResult {
        fee_charged,
        result,
        ext: TransactionResultExt::V0,
    }
}

fn parse_tx_result_set(value: &JsonValue) -> TransactionResultSet {
    let items = value
        .as_array()
        .unwrap_or_else(|| panic!("txProcessing: expected array"));
    let mut pairs = Vec::with_capacity(items.len());
    for item in items {
        let obj = item
            .as_object()
            .unwrap_or_else(|| panic!("txProcessing entry: expected object"));
        let result_obj = obj
            .get("result")
            .unwrap_or_else(|| panic!("txProcessing.result missing"))
            .as_object()
            .unwrap_or_else(|| panic!("txProcessing.result: expected object"));
        let tx_hash = parse_hash(
            result_obj
                .get("transactionHash")
                .unwrap_or_else(|| panic!("txProcessing.result.transactionHash missing")),
            "txProcessing.result.transactionHash",
        );
        let tx_result = parse_transaction_result(
            result_obj
                .get("result")
                .unwrap_or_else(|| panic!("txProcessing.result.result missing")),
        );
        pairs.push(TransactionResultPair {
            transaction_hash: tx_hash,
            result: tx_result,
        });
    }
    TransactionResultSet {
        results: pairs
            .try_into()
            .expect("tx result pairs VecM conversion failed"),
    }
}

fn parse_ledger_header(value: &JsonValue) -> LedgerHeader {
    let obj = value
        .as_object()
        .unwrap_or_else(|| panic!("ledgerHeader.header: expected object"));

    let scp_value = parse_stellar_value(
        obj.get("scpValue")
            .unwrap_or_else(|| panic!("ledgerHeader.header.scpValue missing")),
    );

    let skip_list = obj
        .get("skipList")
        .and_then(|v| v.as_array())
        .unwrap_or_else(|| panic!("ledgerHeader.header.skipList missing"))
        .iter()
        .map(|item| parse_hash(item, "ledgerHeader.header.skipList"))
        .collect::<Vec<Hash>>();

    let skip_list: [Hash; 4] = skip_list
        .try_into()
        .unwrap_or_else(|_| panic!("ledgerHeader.header.skipList: expected 4 entries"));

    let ext = LedgerHeaderExt::V0;

    LedgerHeader {
        ledger_version: parse_u32(
            obj.get("ledgerVersion")
                .unwrap_or_else(|| panic!("ledgerHeader.header.ledgerVersion missing")),
            "ledgerHeader.header.ledgerVersion",
        ),
        previous_ledger_hash: parse_hash(
            obj.get("previousLedgerHash")
                .unwrap_or_else(|| panic!("ledgerHeader.header.previousLedgerHash missing")),
            "ledgerHeader.header.previousLedgerHash",
        ),
        scp_value,
        tx_set_result_hash: parse_hash(
            obj.get("txSetResultHash")
                .unwrap_or_else(|| panic!("ledgerHeader.header.txSetResultHash missing")),
            "ledgerHeader.header.txSetResultHash",
        ),
        bucket_list_hash: parse_hash(
            obj.get("bucketListHash")
                .unwrap_or_else(|| panic!("ledgerHeader.header.bucketListHash missing")),
            "ledgerHeader.header.bucketListHash",
        ),
        ledger_seq: parse_u32(
            obj.get("ledgerSeq")
                .unwrap_or_else(|| panic!("ledgerHeader.header.ledgerSeq missing")),
            "ledgerHeader.header.ledgerSeq",
        ),
        total_coins: parse_i64(
            obj.get("totalCoins")
                .unwrap_or_else(|| panic!("ledgerHeader.header.totalCoins missing")),
            "ledgerHeader.header.totalCoins",
        ),
        fee_pool: parse_i64(
            obj.get("feePool")
                .unwrap_or_else(|| panic!("ledgerHeader.header.feePool missing")),
            "ledgerHeader.header.feePool",
        ),
        inflation_seq: parse_u32(
            obj.get("inflationSeq")
                .unwrap_or_else(|| panic!("ledgerHeader.header.inflationSeq missing")),
            "ledgerHeader.header.inflationSeq",
        ),
        id_pool: parse_u64(
            obj.get("idPool")
                .unwrap_or_else(|| panic!("ledgerHeader.header.idPool missing")),
            "ledgerHeader.header.idPool",
        ),
        base_fee: parse_u32(
            obj.get("baseFee")
                .unwrap_or_else(|| panic!("ledgerHeader.header.baseFee missing")),
            "ledgerHeader.header.baseFee",
        ),
        base_reserve: parse_u32(
            obj.get("baseReserve")
                .unwrap_or_else(|| panic!("ledgerHeader.header.baseReserve missing")),
            "ledgerHeader.header.baseReserve",
        ),
        max_tx_set_size: parse_u32(
            obj.get("maxTxSetSize")
                .unwrap_or_else(|| panic!("ledgerHeader.header.maxTxSetSize missing")),
            "ledgerHeader.header.maxTxSetSize",
        ),
        skip_list,
        ext,
    }
}

fn v2_ledger_close_meta_files() -> &'static [&'static str] {
    &[
        "ledger-close-meta-v2-protocol-23.json",
        "ledger-close-meta-v2-protocol-23-soroban.json",
        "ledger-close-meta-v2-protocol-24.json",
        "ledger-close-meta-v2-protocol-24-soroban.json",
        "ledger-close-meta-v2-protocol-25.json",
        "ledger-close-meta-v2-protocol-25-soroban.json",
        "ledger-close-meta-v2-protocol-26.json",
        "ledger-close-meta-v2-protocol-26-soroban.json",
        "ledger-close-meta-enable-classic-events-v2-protocol-23.json",
        "ledger-close-meta-enable-classic-events-v2-protocol-23-soroban.json",
        "ledger-close-meta-enable-classic-events-v2-protocol-24.json",
        "ledger-close-meta-enable-classic-events-v2-protocol-24-soroban.json",
        "ledger-close-meta-enable-classic-events-v2-protocol-25.json",
        "ledger-close-meta-enable-classic-events-v2-protocol-25-soroban.json",
        "ledger-close-meta-enable-classic-events-v2-protocol-26.json",
        "ledger-close-meta-enable-classic-events-v2-protocol-26-soroban.json",
    ]
}

#[test]
fn ledger_close_meta_header_hash_vectors() {
    for name in v2_ledger_close_meta_files() {
        let root = load_json(name);
        let meta = root
            .get("LedgerCloseMeta")
            .unwrap_or_else(|| panic!("LedgerCloseMeta missing in {}", name));
        let meta_obj = meta
            .as_object()
            .unwrap_or_else(|| panic!("LedgerCloseMeta not an object in {}", name));
        let v2 = meta_obj
            .get("v2")
            .unwrap_or_else(|| panic!("LedgerCloseMeta.v2 missing in {}", name));
        let header_obj = v2
            .get("ledgerHeader")
            .unwrap_or_else(|| panic!("LedgerCloseMeta.v2.ledgerHeader missing in {}", name))
            .as_object()
            .unwrap_or_else(|| panic!("ledgerHeader not object in {}", name));
        let expected_hash = parse_hash(
            header_obj
                .get("hash")
                .unwrap_or_else(|| panic!("ledgerHeader.hash missing in {}", name)),
            "ledgerHeader.hash",
        );
        let header = parse_ledger_header(
            header_obj
                .get("header")
                .unwrap_or_else(|| panic!("ledgerHeader.header missing in {}", name)),
        );
        let entry = LedgerHeaderHistoryEntry {
            hash: expected_hash,
            header,
            ext: LedgerHeaderHistoryEntryExt::V0,
        };
        let expected = Hash256::from(entry.hash);
        let got = Hash256::hash_xdr(&entry.header);
        assert_eq!(got, expected, "header hash mismatch for {}", name);
    }
}

#[test]
fn ledger_close_meta_tx_result_hash_vectors() {
    for name in v2_ledger_close_meta_files() {
        let root = load_json(name);
        let meta = root
            .get("LedgerCloseMeta")
            .unwrap_or_else(|| panic!("LedgerCloseMeta missing in {}", name));
        let meta_obj = meta
            .as_object()
            .unwrap_or_else(|| panic!("LedgerCloseMeta not an object in {}", name));
        let v2 = meta_obj
            .get("v2")
            .unwrap_or_else(|| panic!("LedgerCloseMeta.v2 missing in {}", name));
        let header_obj = v2
            .get("ledgerHeader")
            .unwrap_or_else(|| panic!("LedgerCloseMeta.v2.ledgerHeader missing in {}", name))
            .as_object()
            .unwrap_or_else(|| panic!("ledgerHeader not object in {}", name));
        let expected_hash = parse_hash(
            header_obj
                .get("header")
                .unwrap_or_else(|| panic!("ledgerHeader.header missing in {}", name))
                .as_object()
                .unwrap_or_else(|| panic!("ledgerHeader.header not object in {}", name))
                .get("txSetResultHash")
                .unwrap_or_else(|| {
                    panic!("ledgerHeader.header.txSetResultHash missing in {}", name)
                }),
            "ledgerHeader.header.txSetResultHash",
        );
        let tx_result_set = parse_tx_result_set(
            v2.get("txProcessing")
                .unwrap_or_else(|| panic!("LedgerCloseMeta.v2.txProcessing missing in {}", name)),
        );
        let got = Hash256::hash_xdr(&tx_result_set);
        let expected = Hash256::from(expected_hash);
        assert_eq!(got, expected, "tx result hash mismatch for {}", name);
    }
}
