//! Diagnostic / diff printers for offline execution verification.
//!
//! Leaf helpers called from `run::verify_single_ledger`'s hash-mismatch
//! diagnostic region. Moved verbatim from the former single-file module
//! (#3357 split); no logic change.

/// Returns a short description of a TransactionEnvelope for diagnostics:
/// (op_types_csv, declared_fee, soroban_resource_fee, soroban_resources_summary).
pub(super) fn describe_envelope(
    env: Option<&stellar_xdr::curr::TransactionEnvelope>,
) -> (String, i64, i64, String) {
    use stellar_xdr::curr::{
        Operation, OperationBody, SorobanTransactionData, TransactionEnvelope, TransactionExt,
    };
    fn describe_op(op: &Operation) -> &'static str {
        match op.body {
            OperationBody::CreateAccount(_) => "CreateAccount",
            OperationBody::Payment(_) => "Payment",
            OperationBody::PathPaymentStrictReceive(_) => "PathPayStrictRecv",
            OperationBody::ManageSellOffer(_) => "ManageSellOffer",
            OperationBody::CreatePassiveSellOffer(_) => "CreatePassiveSellOffer",
            OperationBody::SetOptions(_) => "SetOptions",
            OperationBody::ChangeTrust(_) => "ChangeTrust",
            OperationBody::AllowTrust(_) => "AllowTrust",
            OperationBody::AccountMerge(_) => "AccountMerge",
            OperationBody::Inflation => "Inflation",
            OperationBody::ManageData(_) => "ManageData",
            OperationBody::BumpSequence(_) => "BumpSequence",
            OperationBody::ManageBuyOffer(_) => "ManageBuyOffer",
            OperationBody::PathPaymentStrictSend(_) => "PathPayStrictSend",
            OperationBody::CreateClaimableBalance(_) => "CreateClaimableBalance",
            OperationBody::ClaimClaimableBalance(_) => "ClaimClaimableBalance",
            OperationBody::BeginSponsoringFutureReserves(_) => "BeginSponsoringFutureReserves",
            OperationBody::EndSponsoringFutureReserves => "EndSponsoringFutureReserves",
            OperationBody::RevokeSponsorship(_) => "RevokeSponsorship",
            OperationBody::Clawback(_) => "Clawback",
            OperationBody::ClawbackClaimableBalance(_) => "ClawbackClaimableBalance",
            OperationBody::SetTrustLineFlags(_) => "SetTrustLineFlags",
            OperationBody::LiquidityPoolDeposit(_) => "LiquidityPoolDeposit",
            OperationBody::LiquidityPoolWithdraw(_) => "LiquidityPoolWithdraw",
            OperationBody::InvokeHostFunction(_) => "InvokeHostFunction",
            OperationBody::ExtendFootprintTtl(_) => "ExtendFootprintTtl",
            OperationBody::RestoreFootprint(_) => "RestoreFootprint",
        }
    }
    fn summarize_resources(data: &SorobanTransactionData) -> String {
        use stellar_xdr::curr::SorobanTransactionDataExt;
        let r = &data.resources;
        let archived = match &data.ext {
            SorobanTransactionDataExt::V1(ext) => ext.archived_soroban_entries.len(),
            _ => 0,
        };
        // archived_idx: count of read-write footprint entries marked for
        // hot-archive autorestore (see CAP-66/77). Surfaced for diagnostic
        // parity with stellar-core's restore-then-meter pre-execution path.
        format!(
            "instr={},rro={},rrw={},wb={},drb={},archived_idx={}",
            r.instructions,
            r.footprint.read_only.len(),
            r.footprint.read_write.len(),
            r.write_bytes,
            r.disk_read_bytes,
            archived,
        )
    }
    let Some(env) = env else {
        return (String::from("?"), 0, 0, String::new());
    };
    match env {
        TransactionEnvelope::TxV0(v0) => {
            let ops: Vec<&str> = v0.tx.operations.iter().map(describe_op).collect();
            (ops.join(","), v0.tx.fee as i64, 0, String::new())
        }
        TransactionEnvelope::Tx(v1) => {
            let ops: Vec<&str> = v1.tx.operations.iter().map(describe_op).collect();
            let (sb_fee, sb_summary) = match &v1.tx.ext {
                TransactionExt::V1(data) => (data.resource_fee, summarize_resources(data)),
                _ => (0, String::new()),
            };
            (ops.join(","), v1.tx.fee as i64, sb_fee, sb_summary)
        }
        TransactionEnvelope::TxFeeBump(fb) => {
            let (inner_ops_str, inner_sb_fee, inner_sb_summary) = match &fb.tx.inner_tx {
                stellar_xdr::curr::FeeBumpTransactionInnerTx::Tx(inner) => {
                    let ops_str = inner
                        .tx
                        .operations
                        .iter()
                        .map(describe_op)
                        .collect::<Vec<_>>()
                        .join(",");
                    let (sb_fee, sb_summary) = match &inner.tx.ext {
                        TransactionExt::V1(data) => (data.resource_fee, summarize_resources(data)),
                        _ => (0, String::new()),
                    };
                    (ops_str, sb_fee, sb_summary)
                }
            };
            (
                format!("FB({inner_ops_str})"),
                fb.tx.fee,
                inner_sb_fee,
                inner_sb_summary,
            )
        }
    }
}

/// Extract a CSV summary of soroban diagnostic events from a transaction's
/// meta. Captures the SCEC_EXCEEDED_LIMIT errors that pinpoint which
/// stellar-core resource check fired (e.g.
/// "operation byte-write resources exceeds amount specified") — critical
/// for #2503 hash-mismatch divergence diagnosis.
pub(super) fn extract_diagnostic_event_summary(
    meta: Option<&stellar_xdr::curr::TransactionMeta>,
) -> String {
    use stellar_xdr::curr::{ScVal, TransactionMeta};
    let Some(meta) = meta else {
        return String::new();
    };
    let events: &[stellar_xdr::curr::DiagnosticEvent] = match meta {
        TransactionMeta::V3(m) => m
            .soroban_meta
            .as_ref()
            .map(|s| s.diagnostic_events.as_slice())
            .unwrap_or(&[]),
        TransactionMeta::V4(m) => m.diagnostic_events.as_slice(),
        _ => return String::new(),
    };
    let mut summaries = Vec::new();
    for event in events {
        let body = match &event.event.body {
            stellar_xdr::curr::ContractEventBody::V0(b) => b,
        };
        let mut parts: Vec<String> = Vec::new();
        for topic in body.topics.as_slice() {
            match topic {
                ScVal::Symbol(s) => parts.push(s.to_utf8_string_lossy()),
                ScVal::Error(e) => parts.push(format!("{:?}", e)),
                ScVal::String(s) => parts.push(s.to_utf8_string_lossy()),
                _ => parts.push(format!("{:?}", std::mem::discriminant(topic))),
            }
        }
        match &body.data {
            ScVal::Vec(Some(v)) => {
                let inner: Vec<String> =
                    v.0.as_slice()
                        .iter()
                        .filter_map(|x| match x {
                            ScVal::U64(n) => Some(n.to_string()),
                            ScVal::U32(n) => Some(n.to_string()),
                            ScVal::I64(n) => Some(n.to_string()),
                            ScVal::I32(n) => Some(n.to_string()),
                            ScVal::Symbol(s) => Some(s.to_utf8_string_lossy()),
                            ScVal::String(s) => Some(s.to_utf8_string_lossy()),
                            _ => None,
                        })
                        .collect();
                if !inner.is_empty() {
                    parts.push(format!("[{}]", inner.join(",")));
                }
            }
            ScVal::U64(n) => parts.push(n.to_string()),
            ScVal::String(s) => parts.push(s.to_utf8_string_lossy()),
            _ => {}
        }
        summaries.push(parts.join(":"));
    }
    let s = summaries.join(" | ");
    if s.len() > 2000 {
        format!("{}…", &s[..2000])
    } else {
        s
    }
}

/// Build a per-tx state-changes summary string mirroring what henyey logs from
/// manager.rs::commit on hash mismatch. Format: "phase:type:size:hash16 | …"
pub(super) fn summarize_cdp_meta_changes(
    meta: Option<&stellar_xdr::curr::TransactionMeta>,
) -> String {
    use sha2::{Digest, Sha256};
    use stellar_xdr::curr::{LedgerEntryChange, LedgerEntryData, TransactionMeta, WriteXdr};
    let Some(meta) = meta else {
        return String::new();
    };
    let mut parts: Vec<String> = Vec::new();
    let walk =
        |changes: &stellar_xdr::curr::LedgerEntryChanges, parts: &mut Vec<String>, phase: &str| {
            for change in changes.iter() {
                let entry = match change {
                    LedgerEntryChange::Created(e) => e,
                    LedgerEntryChange::Updated(e) => e,
                    LedgerEntryChange::Restored(e) => e,
                    _ => continue,
                };
                if matches!(entry.data, LedgerEntryData::Ttl(_)) {
                    continue;
                }
                let data_bytes = entry
                    .data
                    .to_xdr(stellar_xdr::curr::Limits::none())
                    .unwrap_or_default();
                let data_hash = format!("{:x}", Sha256::digest(&data_bytes));
                parts.push(format!(
                    "{}:t{:?}:{}:{}",
                    phase,
                    std::mem::discriminant(&entry.data),
                    data_bytes.len(),
                    &data_hash[..16],
                ));
            }
        };
    match meta {
        TransactionMeta::V3(m) => {
            walk(&m.tx_changes_before, &mut parts, "before");
            for op in m.operations.iter() {
                walk(&op.changes, &mut parts, "op");
            }
            walk(&m.tx_changes_after, &mut parts, "after");
        }
        TransactionMeta::V4(m) => {
            walk(&m.tx_changes_before, &mut parts, "before");
            for op in m.operations.iter() {
                walk(&op.changes, &mut parts, "op");
            }
            walk(&m.tx_changes_after, &mut parts, "after");
        }
        _ => {}
    }
    let mut sorted = parts.clone();
    sorted.sort();
    let set_str = sorted.join(" | ");
    let set_hash = format!("{:x}", Sha256::digest(set_str.as_bytes()));
    let s = parts.join(" | ");
    let truncated = if s.len() > 1000 {
        format!("{}…", &s[..1000])
    } else {
        s
    };
    format!(
        "count={} sethash={} {}",
        parts.len(),
        &set_hash[..16],
        truncated
    )
}

/// Returns (count, total_bytes) of non-TTL Created/Updated/Restored entries
/// across a transaction's tx_apply_processing change groups. Mirrors the
/// stellar-core write_bytes accounting (which excludes TTL entries) for
/// side-by-side comparison with henyey's `total_write_bytes`.
pub(super) fn summarize_cdp_meta(meta: Option<&stellar_xdr::curr::TransactionMeta>) -> (u32, u64) {
    use stellar_xdr::curr::{LedgerEntryChange, LedgerEntryData, WriteXdr};
    let Some(meta) = meta else {
        return (0, 0);
    };
    let mut count: u32 = 0;
    let mut total_bytes: u64 = 0;
    henyey_common::meta_walk::for_each_change_group(meta, |changes| {
        for change in changes {
            let entry = match change {
                LedgerEntryChange::Created(e) | LedgerEntryChange::Updated(e) => e,
                LedgerEntryChange::Restored(e) => e,
                _ => continue,
            };
            if matches!(entry.data, LedgerEntryData::Ttl(_)) {
                continue;
            }
            count += 1;
            if let Ok(bytes) = entry.to_xdr(stellar_xdr::curr::Limits::none()) {
                total_bytes += bytes.len() as u64;
            }
        }
    });
    (count, total_bytes)
}

/// Returns a CSV summary of operation result discriminants for a transaction
/// result, e.g. "InvokeHostFunction:Trapped". Used to diagnose tx-level
/// success/fail divergence by exposing per-op outcomes.
pub(super) fn describe_op_results(r: &stellar_xdr::curr::TransactionResultResult) -> String {
    use stellar_xdr::curr::{
        InnerTransactionResultResult, OperationResult, TransactionResultResult,
    };
    fn one_op(op: &OperationResult) -> String {
        // Print the full Debug repr (without payload values) so that
        // success/failure variants of inner result types like
        // InvokeHostFunctionResult::{Success,Trapped,ResourceLimitExceeded,…}
        // are visible. Truncate to keep log lines reasonable.
        let s = format!("{:?}", op);
        if s.len() > 200 {
            format!("{}…", &s[..200])
        } else {
            s
        }
    }
    fn ops_to_csv(ops: &[OperationResult]) -> String {
        ops.iter().map(one_op).collect::<Vec<_>>().join(",")
    }
    match r {
        TransactionResultResult::TxSuccess(ops) => ops_to_csv(ops.as_slice()),
        TransactionResultResult::TxFailed(ops) => ops_to_csv(ops.as_slice()),
        TransactionResultResult::TxFeeBumpInnerSuccess(pair) => {
            let inner_ops = match &pair.result.result {
                InnerTransactionResultResult::TxSuccess(ops) => ops_to_csv(ops.as_slice()),
                InnerTransactionResultResult::TxFailed(ops) => ops_to_csv(ops.as_slice()),
                other => format!("{:?}", other.discriminant()),
            };
            format!("FB+{inner_ops}")
        }
        TransactionResultResult::TxFeeBumpInnerFailed(pair) => {
            let inner_ops = match &pair.result.result {
                InnerTransactionResultResult::TxSuccess(ops) => ops_to_csv(ops.as_slice()),
                InnerTransactionResultResult::TxFailed(ops) => ops_to_csv(ops.as_slice()),
                other => format!("{:?}", other.discriminant()),
            };
            format!("FB-{inner_ops}")
        }
        other => format!("{:?}", other.discriminant()),
    }
}

/// Returns a human-readable name for a `TransactionResultResult` variant.
pub(super) fn tx_result_code_name(r: &stellar_xdr::curr::TransactionResultResult) -> String {
    use stellar_xdr::curr::TransactionResultResult;
    match r {
        TransactionResultResult::TxSuccess(_) => "txSuccess".to_string(),
        TransactionResultResult::TxFailed(_) => "txFailed".to_string(),
        TransactionResultResult::TxFeeBumpInnerSuccess(_) => "txFeeBumpInnerSuccess".to_string(),
        TransactionResultResult::TxFeeBumpInnerFailed(_) => "txFeeBumpInnerFailed".to_string(),
        other => format!("{:?}", other),
    }
}

/// Prints pairwise differences between two operation result slices.
pub(super) fn print_op_diffs(
    our_ops: &[stellar_xdr::curr::OperationResult],
    cdp_ops: &[stellar_xdr::curr::OperationResult],
) {
    use stellar_xdr::curr::WriteXdr;
    for (j, (our_op, cdp_op)) in our_ops.iter().zip(cdp_ops.iter()).enumerate() {
        let our_op_xdr = our_op
            .to_xdr(stellar_xdr::curr::Limits::none())
            .unwrap_or_default();
        let cdp_op_xdr = cdp_op
            .to_xdr(stellar_xdr::curr::Limits::none())
            .unwrap_or_default();
        if our_op_xdr != cdp_op_xdr {
            println!("          Op {} differs:", j);
            println!("            Ours: {:?}", our_op);
            println!("            CDP:  {:?}", cdp_op);
        }
    }
}

/// Prints all operations from a result slice with a label.
pub(super) fn print_all_ops(label: &str, ops: &[stellar_xdr::curr::OperationResult]) {
    println!("        {} ops ({}):", label, ops.len());
    for (j, op) in ops.iter().enumerate() {
        println!("          Op {}: {:?}", j, op);
    }
}

/// Prints an exhaustive field-by-field comparison of two ledger headers.
pub(super) fn print_header_field_diffs(
    h: &stellar_xdr::curr::LedgerHeader,
    c: &stellar_xdr::curr::LedgerHeader,
    bucket_levels: &[(henyey_common::Hash256, henyey_common::Hash256)],
) {
    use henyey_common::Hash256;
    if h.ledger_version != c.ledger_version {
        println!(
            "    DIFF ledger_version: ours={} expected={}",
            h.ledger_version, c.ledger_version
        );
    }
    if h.previous_ledger_hash != c.previous_ledger_hash {
        println!(
            "    DIFF previous_ledger_hash: ours={} expected={}",
            hex::encode(&h.previous_ledger_hash.0),
            hex::encode(&c.previous_ledger_hash.0)
        );
    }
    if h.scp_value != c.scp_value {
        println!("    DIFF scp_value");
        if h.scp_value.tx_set_hash != c.scp_value.tx_set_hash {
            println!(
                "      tx_set_hash: ours={} expected={}",
                hex::encode(&h.scp_value.tx_set_hash.0),
                hex::encode(&c.scp_value.tx_set_hash.0)
            );
        }
        if h.scp_value.close_time != c.scp_value.close_time {
            println!(
                "      close_time: ours={} expected={}",
                h.scp_value.close_time.0, c.scp_value.close_time.0
            );
        }
        if h.scp_value.upgrades != c.scp_value.upgrades {
            println!(
                "      upgrades: ours={:?} expected={:?}",
                h.scp_value.upgrades, c.scp_value.upgrades
            );
        }
        if h.scp_value.ext != c.scp_value.ext {
            println!("      ext: differs");
        }
    }
    let our_bl_hash = Hash256::from(h.bucket_list_hash.0);
    let expected_bl_hash = Hash256::from(c.bucket_list_hash.0);
    if our_bl_hash != expected_bl_hash {
        println!(
            "    DIFF bucket_list_hash: ours={} expected={}",
            our_bl_hash.to_hex(),
            expected_bl_hash.to_hex()
        );
        for (i, (curr_hash, snap_hash)) in bucket_levels.iter().enumerate() {
            println!(
                "      Level {}: curr={} snap={}",
                i,
                curr_hash.to_hex(),
                snap_hash.to_hex()
            );
        }
    }
    if h.tx_set_result_hash != c.tx_set_result_hash {
        println!(
            "    DIFF tx_set_result_hash: ours={} expected={}",
            hex::encode(&h.tx_set_result_hash.0),
            hex::encode(&c.tx_set_result_hash.0)
        );
    }
    if h.ledger_seq != c.ledger_seq {
        println!(
            "    DIFF ledger_seq: ours={} expected={}",
            h.ledger_seq, c.ledger_seq
        );
    }
    if h.total_coins != c.total_coins {
        println!(
            "    DIFF total_coins: ours={} expected={}",
            h.total_coins, c.total_coins
        );
    }
    if h.fee_pool != c.fee_pool {
        println!(
            "    DIFF fee_pool: ours={} expected={}",
            h.fee_pool, c.fee_pool
        );
    }
    if h.inflation_seq != c.inflation_seq {
        println!(
            "    DIFF inflation_seq: ours={} expected={}",
            h.inflation_seq, c.inflation_seq
        );
    }
    if h.id_pool != c.id_pool {
        println!(
            "    DIFF id_pool: ours={} expected={}",
            h.id_pool, c.id_pool
        );
    }
    if h.base_fee != c.base_fee {
        println!(
            "    DIFF base_fee: ours={} expected={}",
            h.base_fee, c.base_fee
        );
    }
    if h.base_reserve != c.base_reserve {
        println!(
            "    DIFF base_reserve: ours={} expected={}",
            h.base_reserve, c.base_reserve
        );
    }
    if h.max_tx_set_size != c.max_tx_set_size {
        println!(
            "    DIFF max_tx_set_size: ours={} expected={}",
            h.max_tx_set_size, c.max_tx_set_size
        );
    }
    if h.skip_list != c.skip_list {
        println!("    DIFF skip_list:");
        for (i, (ours, exp)) in h.skip_list.iter().zip(c.skip_list.iter()).enumerate() {
            if ours != exp {
                println!(
                    "      [{}]: ours={} expected={}",
                    i,
                    hex::encode(&ours.0),
                    hex::encode(&exp.0)
                );
            }
        }
    }
    if h.ext != c.ext {
        println!("    DIFF ext: ours={:?} expected={:?}", h.ext, c.ext);
    }
}

/// Compare fee bump inner results from both sides and print differences.
pub(super) fn print_fee_bump_inner_diffs(
    our_result: &stellar_xdr::curr::TransactionResultResult,
    cdp_result: &stellar_xdr::curr::TransactionResultResult,
) {
    use stellar_xdr::curr::{InnerTransactionResultResult, TransactionResultResult};

    match (our_result, cdp_result) {
        (
            TransactionResultResult::TxFeeBumpInnerFailed(our_inner),
            TransactionResultResult::TxFeeBumpInnerFailed(cdp_inner),
        ) => {
            println!(
                "        Inner fee: ours={} CDP={}",
                our_inner.result.fee_charged, cdp_inner.result.fee_charged
            );
            let our_inner_code = format!("{:?}", std::mem::discriminant(&our_inner.result.result));
            let cdp_inner_code = format!("{:?}", std::mem::discriminant(&cdp_inner.result.result));
            println!(
                "        Inner result type: ours={} CDP={}",
                our_inner_code, cdp_inner_code
            );
            if let (
                InnerTransactionResultResult::TxFailed(our_ops),
                InnerTransactionResultResult::TxFailed(cdp_ops),
            ) = (&our_inner.result.result, &cdp_inner.result.result)
            {
                print_op_diffs(our_ops, cdp_ops);
                if our_ops.len() != cdp_ops.len() {
                    println!(
                        "          Inner op count: ours={} CDP={}",
                        our_ops.len(),
                        cdp_ops.len()
                    );
                }
            } else {
                println!("        Inner result ours: {:?}", our_inner.result.result);
                println!("        Inner result CDP:  {:?}", cdp_inner.result.result);
            }
        }
        (
            TransactionResultResult::TxFeeBumpInnerSuccess(our_inner),
            TransactionResultResult::TxFeeBumpInnerSuccess(cdp_inner),
        ) => {
            println!(
                "        Inner fee: ours={} CDP={}",
                our_inner.result.fee_charged, cdp_inner.result.fee_charged
            );
            if let (
                InnerTransactionResultResult::TxSuccess(our_ops),
                InnerTransactionResultResult::TxSuccess(cdp_ops),
            ) = (&our_inner.result.result, &cdp_inner.result.result)
            {
                print_op_diffs(our_ops, cdp_ops);
            }
        }
        (
            TransactionResultResult::TxFeeBumpInnerSuccess(our_inner),
            TransactionResultResult::TxFeeBumpInnerFailed(cdp_inner),
        ) => {
            println!(
                "        Inner fee: ours={} CDP={}",
                our_inner.result.fee_charged, cdp_inner.result.fee_charged
            );
            if let InnerTransactionResultResult::TxSuccess(our_ops) = &our_inner.result.result {
                print_all_ops("Ours inner", our_ops);
            }
            if let InnerTransactionResultResult::TxFailed(cdp_ops) = &cdp_inner.result.result {
                print_all_ops("CDP inner", cdp_ops);
            } else {
                println!("        CDP inner result: {:?}", cdp_inner.result.result);
            }
        }
        (
            TransactionResultResult::TxFeeBumpInnerFailed(our_inner),
            TransactionResultResult::TxFeeBumpInnerSuccess(cdp_inner),
        ) => {
            println!(
                "        Inner fee: ours={} CDP={}",
                our_inner.result.fee_charged, cdp_inner.result.fee_charged
            );
            println!("        Ours inner result: {:?}", our_inner.result.result);
            if let InnerTransactionResultResult::TxSuccess(cdp_ops) = &cdp_inner.result.result {
                print_all_ops("CDP inner", cdp_ops);
            }
        }
        _ => {}
    }
}

/// Prints detailed per-TX result diffs between our results and CDP results.
///
/// Shows ordering differences, then does a TX-by-TX XDR comparison with
/// detailed operation-level diffs for all result variant combinations.
pub(super) fn print_tx_result_diffs(
    our_results: &[stellar_xdr::curr::TransactionResultPair],
    cdp_results: &[stellar_xdr::curr::TransactionResultPair],
) {
    use stellar_xdr::curr::{TransactionResultResult, WriteXdr};
    println!(
        "    TX count: ours={} CDP={}",
        our_results.len(),
        cdp_results.len()
    );

    // Check if TX ordering differs
    let mut order_diffs = 0;
    for (i, (our_tx, cdp_tx)) in our_results.iter().zip(cdp_results.iter()).enumerate() {
        if our_tx.transaction_hash != cdp_tx.transaction_hash {
            if order_diffs < 10 {
                println!(
                    "    ORDER DIFF at position {}: ours={} CDP={}",
                    i,
                    hex::encode(&our_tx.transaction_hash.0),
                    hex::encode(&cdp_tx.transaction_hash.0)
                );
            }
            order_diffs += 1;
        }
    }
    if order_diffs > 0 {
        println!("    Total TX ordering differences: {}", order_diffs);
    } else {
        println!("    TX ordering is IDENTICAL (same content hashes at every position)");
    }

    // Detailed TX-by-TX comparison using full XDR
    let mut diff_count = 0;
    for (i, (our_tx, cdp_tx)) in our_results.iter().zip(cdp_results.iter()).enumerate() {
        let our_xdr = our_tx
            .result
            .to_xdr(stellar_xdr::curr::Limits::none())
            .unwrap_or_default();
        let cdp_xdr = cdp_tx
            .result
            .to_xdr(stellar_xdr::curr::Limits::none())
            .unwrap_or_default();

        if our_xdr != cdp_xdr {
            diff_count += 1;
            let our_result = &our_tx.result.result;
            let cdp_result = &cdp_tx.result.result;

            println!("      TX {}: MISMATCH (XDR differs)", i);
            println!(
                "        Result: ours={} CDP={}",
                tx_result_code_name(our_result),
                tx_result_code_name(cdp_result)
            );
            println!(
                "        Fee: ours={} CDP={}",
                our_tx.result.fee_charged, cdp_tx.result.fee_charged
            );
            println!(
                "        TX hash: {}",
                hex::encode(&our_tx.transaction_hash.0)
            );

            // Compare operations for same-variant pairs
            match (our_result, cdp_result) {
                (
                    TransactionResultResult::TxFailed(our_ops),
                    TransactionResultResult::TxFailed(cdp_ops),
                )
                | (
                    TransactionResultResult::TxSuccess(our_ops),
                    TransactionResultResult::TxSuccess(cdp_ops),
                ) => {
                    print_op_diffs(our_ops, cdp_ops);
                }
                // One succeeds, other fails — show all ops from both sides
                (
                    TransactionResultResult::TxSuccess(our_ops),
                    TransactionResultResult::TxFailed(cdp_ops),
                )
                | (
                    TransactionResultResult::TxFailed(our_ops),
                    TransactionResultResult::TxSuccess(cdp_ops),
                ) => {
                    print_all_ops("Ours", our_ops);
                    print_all_ops("CDP", cdp_ops);
                }
                _ => {}
            }

            // Fee bump inner result details
            print_fee_bump_inner_diffs(our_result, cdp_result);

            // Show CDP ops when ours is TxNotSupported or other non-standard result
            if !matches!(
                our_result,
                TransactionResultResult::TxSuccess(_)
                    | TransactionResultResult::TxFailed(_)
                    | TransactionResultResult::TxFeeBumpInnerSuccess(_)
                    | TransactionResultResult::TxFeeBumpInnerFailed(_)
            ) {
                if let TransactionResultResult::TxFailed(cdp_ops) = cdp_result {
                    println!("        CDP txFailed ops ({}):", cdp_ops.len());
                    for (j, op) in cdp_ops.iter().enumerate() {
                        println!("          Op {}: {:?}", j, op);
                    }
                }
            }

            // Limit output to first 10 diffs
            if diff_count >= 10 {
                println!("      ... (showing first 10 of potentially more diffs)");
                break;
            }
        }
    }
    if diff_count > 0 {
        println!(
            "    Total TX diffs: {} out of {}",
            diff_count,
            our_results.len().min(cdp_results.len())
        );
    }
}
