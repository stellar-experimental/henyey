//! Diagnostic / diff printers for offline execution verification.
//!
//! Leaf helpers called from `run::verify_single_ledger`'s hash-mismatch
//! diagnostic region. Moved verbatim from the former single-file module
//! (#3357 split); no logic change.

/// Returns a short description of a TransactionEnvelope for diagnostics:
/// (op_types_csv, declared_fee, soroban_resource_fee, soroban_resources_summary).
pub(super) fn describe_envelope(
    env: Option<&stellar_xdr::TransactionEnvelope>,
) -> (String, i64, i64, String) {
    use stellar_xdr::{
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
        use stellar_xdr::SorobanTransactionDataExt;
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
                stellar_xdr::FeeBumpTransactionInnerTx::Tx(inner) => {
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
    meta: Option<&stellar_xdr::TransactionMeta>,
) -> String {
    use stellar_xdr::{ScVal, TransactionMeta};
    let Some(meta) = meta else {
        return String::new();
    };
    let events: &[stellar_xdr::DiagnosticEvent] = match meta {
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
            stellar_xdr::ContractEventBody::V0(b) => b,
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
pub(super) fn summarize_cdp_meta_changes(meta: Option<&stellar_xdr::TransactionMeta>) -> String {
    use sha2::{Digest, Sha256};
    use stellar_xdr::{LedgerEntryChange, LedgerEntryData, TransactionMeta, WriteXdr};
    let Some(meta) = meta else {
        return String::new();
    };
    let mut parts: Vec<String> = Vec::new();
    let walk = |changes: &stellar_xdr::LedgerEntryChanges, parts: &mut Vec<String>, phase: &str| {
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
                .to_xdr(stellar_xdr::Limits::none())
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
pub(super) fn summarize_cdp_meta(meta: Option<&stellar_xdr::TransactionMeta>) -> (u32, u64) {
    use stellar_xdr::{LedgerEntryChange, LedgerEntryData, WriteXdr};
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
            if let Ok(bytes) = entry.to_xdr(stellar_xdr::Limits::none()) {
                total_bytes += bytes.len() as u64;
            }
        }
    });
    (count, total_bytes)
}

/// Returns a CSV summary of operation result discriminants for a transaction
/// result, e.g. "InvokeHostFunction:Trapped". Used to diagnose tx-level
/// success/fail divergence by exposing per-op outcomes.
pub(super) fn describe_op_results(r: &stellar_xdr::TransactionResultResult) -> String {
    use stellar_xdr::{InnerTransactionResultResult, OperationResult, TransactionResultResult};
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
pub(super) fn tx_result_code_name(r: &stellar_xdr::TransactionResultResult) -> String {
    use stellar_xdr::TransactionResultResult;
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
    our_ops: &[stellar_xdr::OperationResult],
    cdp_ops: &[stellar_xdr::OperationResult],
) {
    use stellar_xdr::WriteXdr;
    for (j, (our_op, cdp_op)) in our_ops.iter().zip(cdp_ops.iter()).enumerate() {
        let our_op_xdr = our_op
            .to_xdr(stellar_xdr::Limits::none())
            .unwrap_or_default();
        let cdp_op_xdr = cdp_op
            .to_xdr(stellar_xdr::Limits::none())
            .unwrap_or_default();
        if our_op_xdr != cdp_op_xdr {
            println!("          Op {} differs:", j);
            println!("            Ours: {:?}", our_op);
            println!("            CDP:  {:?}", cdp_op);
        }
    }
}

/// Prints all operations from a result slice with a label.
pub(super) fn print_all_ops(label: &str, ops: &[stellar_xdr::OperationResult]) {
    println!("        {} ops ({}):", label, ops.len());
    for (j, op) in ops.iter().enumerate() {
        println!("          Op {}: {:?}", j, op);
    }
}

/// Prints an exhaustive field-by-field comparison of two ledger headers.
pub(super) fn print_header_field_diffs(
    h: &stellar_xdr::LedgerHeader,
    c: &stellar_xdr::LedgerHeader,
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
    our_result: &stellar_xdr::TransactionResultResult,
    cdp_result: &stellar_xdr::TransactionResultResult,
) {
    use stellar_xdr::{InnerTransactionResultResult, TransactionResultResult};

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
    our_results: &[stellar_xdr::TransactionResultPair],
    cdp_results: &[stellar_xdr::TransactionResultPair],
) {
    use stellar_xdr::{TransactionResultResult, WriteXdr};
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
            .to_xdr(stellar_xdr::Limits::none())
            .unwrap_or_default();
        let cdp_xdr = cdp_tx
            .result
            .to_xdr(stellar_xdr::Limits::none())
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

/// Counts CDP entry changes from the per-transaction metas and the upgrade
/// metas. Returns `(creates, updates, deletes, upgrade_creates, upgrade_updates)`.
///
/// Pure helper hoisted from `run::print_eviction_and_entry_diagnostics`; no
/// printing. Mapping preserved exactly: `Created→creates`; `Updated` and
/// `Restored→updates`; `Removed→deletes`; `State` ignored. Upgrade metas count
/// only `Created`/`Updated`.
/// Classifies a single change-group, returning `(creates, updates, deletes)`.
///
/// Mapping preserved exactly: `Created→creates`; `Updated` and `Restored→updates`;
/// `Removed→deletes`; `State` ignored. Callers accumulate (`+=`) the returned
/// tuple — the counts fold across all change-groups and tx_metas.
fn count_changes(changes: &[stellar_xdr::LedgerEntryChange]) -> (u32, u32, u32) {
    let mut creates = 0u32;
    let mut updates = 0u32;
    let mut deletes = 0u32;
    for change in changes {
        match change {
            stellar_xdr::LedgerEntryChange::Created(_) => creates += 1,
            stellar_xdr::LedgerEntryChange::Updated(_) => updates += 1,
            stellar_xdr::LedgerEntryChange::Removed(_) => deletes += 1,
            stellar_xdr::LedgerEntryChange::Restored(_) => updates += 1,
            stellar_xdr::LedgerEntryChange::State(_) => {}
        }
    }
    (creates, updates, deletes)
}

pub(super) fn cdp_change_counts(
    tx_metas: &[stellar_xdr::TransactionMeta],
    upgrade_metas: &[stellar_xdr::UpgradeEntryMeta],
) -> (u32, u32, u32, u32, u32) {
    let mut cdp_creates = 0u32;
    let mut cdp_updates = 0u32;
    let mut cdp_deletes = 0u32;
    for tx_meta in tx_metas {
        henyey_common::meta_walk::for_each_change_group(tx_meta, |changes| {
            // Accumulate (`+=`, not reassign): counters fold across every
            // change-group callback within this tx_meta AND across all tx_metas.
            let (c, u, d) = count_changes(changes);
            cdp_creates += c;
            cdp_updates += u;
            cdp_deletes += d;
        });
    }

    let mut upgrade_creates = 0u32;
    let mut upgrade_updates = 0u32;
    for um in upgrade_metas {
        for change in um.changes.iter() {
            match change {
                stellar_xdr::LedgerEntryChange::Created(_) => upgrade_creates += 1,
                stellar_xdr::LedgerEntryChange::Updated(_) => upgrade_updates += 1,
                _ => {}
            }
        }
    }

    (
        cdp_creates,
        cdp_updates,
        cdp_deletes,
        upgrade_creates,
        upgrade_updates,
    )
}

/// Dumps the expected upgrade entries from CDP meta for comparison.
///
/// Verbatim move of the `if !cdp_upgrade_metas.is_empty()` block from
/// `run::print_eviction_and_entry_diagnostics`.
pub(super) fn print_cdp_upgrade_entries(upgrade_metas: &[stellar_xdr::UpgradeEntryMeta]) {
    if !upgrade_metas.is_empty() {
        use sha2::{Digest, Sha256};
        use stellar_xdr::WriteXdr;
        println!("    CDP upgrade entries (expected):");
        for (ui, um) in upgrade_metas.iter().enumerate() {
            for change in um.changes.iter() {
                match change {
                    stellar_xdr::LedgerEntryChange::Updated(entry) => {
                        let key_str = match &entry.data {
                            stellar_xdr::LedgerEntryData::ConfigSetting(cs) => {
                                format!("ConfigSetting({:?})", cs.discriminant())
                            }
                            other => format!("{:?}", std::mem::discriminant(other)),
                        };
                        let xdr_bytes = entry
                            .to_xdr(stellar_xdr::Limits::none())
                            .unwrap_or_default();
                        let xdr_size = xdr_bytes.len();
                        let hash = {
                            let mut h = Sha256::new();
                            h.update(&xdr_bytes);
                            let r = h.finalize();
                            format!("{:x}", r)
                        };
                        println!("      upgrade[{}] Updated: key={}, last_modified={}, xdr_size={}, xdr_hash={}",
                        ui, key_str, entry.last_modified_ledger_seq, xdr_size, hash);
                    }
                    stellar_xdr::LedgerEntryChange::Created(entry) => {
                        let key_str = match &entry.data {
                            stellar_xdr::LedgerEntryData::ConfigSetting(cs) => {
                                format!("ConfigSetting({:?})", cs.discriminant())
                            }
                            other => format!("{:?}", std::mem::discriminant(other)),
                        };
                        let xdr_bytes = entry
                            .to_xdr(stellar_xdr::Limits::none())
                            .unwrap_or_default();
                        let xdr_size = xdr_bytes.len();
                        let hash = {
                            let mut h = Sha256::new();
                            h.update(&xdr_bytes);
                            let r = h.finalize();
                            format!("{:x}", r)
                        };
                        println!("      upgrade[{}] Created: key={}, last_modified={}, xdr_size={}, xdr_hash={}",
                        ui, key_str, entry.last_modified_ledger_seq, xdr_size, hash);
                    }
                    stellar_xdr::LedgerEntryChange::State(entry) => {
                        let key_str = match &entry.data {
                            stellar_xdr::LedgerEntryData::ConfigSetting(cs) => {
                                format!("ConfigSetting({:?})", cs.discriminant())
                            }
                            other => format!("{:?}", std::mem::discriminant(other)),
                        };
                        println!("      upgrade[{}] State(before): key={}", ui, key_str);
                    }
                    _ => {}
                }
            }
        }
    }
}

/// Coalesces the expected final TX entries from CDP meta, keeping the last
/// `Updated`/`Created`/`Restored` entry per key and honoring `Removed`.
///
/// Pure helper hoisted from `run::print_eviction_and_entry_diagnostics`; no
/// printing. Includes ALL change sources: `fee_processing`,
/// `tx_apply_processing`, and `post_tx_apply_fee_processing` (V2 only).
pub(super) fn coalesce_cdp_final_entries(
    lcm: &stellar_xdr::LedgerCloseMeta,
) -> std::collections::HashMap<Vec<u8>, stellar_xdr::LedgerEntry> {
    use stellar_xdr::WriteXdr;
    // Coalesce: keep last Updated entry per key
    // Include ALL change sources: fee_processing, tx_apply_processing, and post_tx_apply_fee_processing
    let mut final_entries: std::collections::HashMap<Vec<u8>, stellar_xdr::LedgerEntry> =
        std::collections::HashMap::new();
    // Helper to process a slice of changes into the coalesced map
    let coalesce_changes =
        |changes: &[stellar_xdr::LedgerEntryChange],
         map: &mut std::collections::HashMap<Vec<u8>, stellar_xdr::LedgerEntry>| {
            for change in changes {
                match change {
                    stellar_xdr::LedgerEntryChange::Updated(entry)
                    | stellar_xdr::LedgerEntryChange::Created(entry)
                    | stellar_xdr::LedgerEntryChange::Restored(entry) => {
                        let key = henyey_common::entry_to_key(entry);
                        if let Ok(kb) = key.to_xdr(stellar_xdr::Limits::none()) {
                            map.insert(kb, entry.clone());
                        }
                    }
                    stellar_xdr::LedgerEntryChange::Removed(key) => {
                        if let Ok(kb) = key.to_xdr(stellar_xdr::Limits::none()) {
                            map.remove(&kb);
                        }
                    }
                    _ => {}
                }
            }
        };
    let coalesce_tx_meta =
        |meta: &stellar_xdr::TransactionMeta,
         map: &mut std::collections::HashMap<Vec<u8>, stellar_xdr::LedgerEntry>| {
            henyey_common::meta_walk::for_each_change_group(meta, |changes| {
                coalesce_changes(changes, map);
            });
        };
    // Process ALL change sources from LCM tx_processing
    match &lcm {
        stellar_xdr::LedgerCloseMeta::V0(v0) => {
            for tp in v0.tx_processing.iter() {
                coalesce_changes(&tp.fee_processing, &mut final_entries);
                coalesce_tx_meta(&tp.tx_apply_processing, &mut final_entries);
                // V0 TransactionResultMeta has no post_tx_apply_fee_processing
            }
        }
        stellar_xdr::LedgerCloseMeta::V1(v1) => {
            for tp in v1.tx_processing.iter() {
                coalesce_changes(&tp.fee_processing, &mut final_entries);
                coalesce_tx_meta(&tp.tx_apply_processing, &mut final_entries);
                // V1 TransactionResultMeta has no post_tx_apply_fee_processing
            }
        }
        stellar_xdr::LedgerCloseMeta::V2(v2) => {
            for tp in v2.tx_processing.iter() {
                coalesce_changes(&tp.fee_processing, &mut final_entries);
                coalesce_tx_meta(&tp.tx_apply_processing, &mut final_entries);
                coalesce_changes(&tp.post_tx_apply_fee_processing, &mut final_entries);
            }
        }
    }
    final_entries
}

/// Prints readable offer-entry diff details for a CDP/ours offer mismatch.
fn print_offer_diff_detail(cdp_o: &stellar_xdr::OfferEntry, our_o: &stellar_xdr::OfferEntry) {
    println!(
        "      CDP  offer: seller={:?} amount={} price={}/{}",
        hex::encode(
            &{
                let stellar_xdr::PublicKey::PublicKeyTypeEd25519(ref pk) = cdp_o.seller_id.0;
                pk.0
            }[..8]
        ),
        cdp_o.amount,
        cdp_o.price.n,
        cdp_o.price.d
    );
    println!(
        "      Ours offer: seller={:?} amount={} price={}/{}",
        hex::encode(
            &{
                let stellar_xdr::PublicKey::PublicKeyTypeEd25519(ref pk) = our_o.seller_id.0;
                pk.0
            }[..8]
        ),
        our_o.amount,
        our_o.price.n,
        our_o.price.d
    );
}

/// Prints readable account-entry diff details for a CDP/ours account mismatch.
fn print_account_diff_detail(cdp_a: &stellar_xdr::AccountEntry, our_a: &stellar_xdr::AccountEntry) {
    let cdp_pk = {
        let stellar_xdr::PublicKey::PublicKeyTypeEd25519(ref pk) = cdp_a.account_id.0;
        hex::encode(&pk.0[..16])
    };
    // Extract sponsorship counts from extensions
    let get_ext = |a: &stellar_xdr::AccountEntry| -> (u32, u32, u32) {
        match &a.ext {
            stellar_xdr::AccountEntryExt::V0 => (0, 0, 0),
            stellar_xdr::AccountEntryExt::V1(v1) => match &v1.ext {
                stellar_xdr::AccountEntryExtensionV1Ext::V0 => (0, 0, 0),
                stellar_xdr::AccountEntryExtensionV1Ext::V2(v2) => (
                    v2.num_sponsoring,
                    v2.num_sponsored,
                    v2.signer_sponsoring_i_ds.len() as u32,
                ),
            },
        }
    };
    let (cdp_ing, cdp_ed, cdp_sigs) = get_ext(cdp_a);
    let (our_ing, our_ed, our_sigs) = get_ext(our_a);
    println!("      CDP  account: id={} balance={} seq={} sub_entries={} flags={} num_sponsoring={} num_sponsored={} signer_sponsors={}",
    cdp_pk, cdp_a.balance, cdp_a.seq_num.0, cdp_a.num_sub_entries, cdp_a.flags, cdp_ing, cdp_ed, cdp_sigs);
    println!("      Ours account: id={} balance={} seq={} sub_entries={} flags={} num_sponsoring={} num_sponsored={} signer_sponsors={}",
    cdp_pk, our_a.balance, our_a.seq_num.0, our_a.num_sub_entries, our_a.flags, our_ing, our_ed, our_sigs);
    if cdp_a.balance != our_a.balance {
        println!(
            "      BALANCE DIFF: {} (ours - cdp)",
            our_a.balance - cdp_a.balance
        );
    }
    if cdp_a.num_sub_entries != our_a.num_sub_entries {
        println!(
            "      SUB_ENTRIES DIFF: {} (ours - cdp)",
            our_a.num_sub_entries as i64 - cdp_a.num_sub_entries as i64
        );
    }
    if cdp_ing != our_ing {
        println!(
            "      NUM_SPONSORING DIFF: {} (ours - cdp)",
            our_ing as i64 - cdp_ing as i64
        );
    }
    if cdp_ed != our_ed {
        println!(
            "      NUM_SPONSORED DIFF: {} (ours - cdp)",
            our_ed as i64 - cdp_ed as i64
        );
    }
}

/// Prints readable trustline-entry diff details for a CDP/ours trustline mismatch.
fn print_trustline_diff_detail(
    cdp_t: &stellar_xdr::TrustLineEntry,
    our_t: &stellar_xdr::TrustLineEntry,
) {
    println!(
        "      CDP  trustline: balance={} asset={:?}",
        cdp_t.balance, cdp_t.asset
    );
    println!(
        "      Ours trustline: balance={} asset={:?}",
        our_t.balance, our_t.asset
    );
}

/// Prints readable liquidity-pool-entry diff details for a CDP/ours pool mismatch.
fn print_pool_diff_detail(
    cdp_p: &stellar_xdr::LiquidityPoolEntry,
    our_p: &stellar_xdr::LiquidityPoolEntry,
) {
    let stellar_xdr::LiquidityPoolEntryBody::LiquidityPoolConstantProduct(ref cdp_cp) = cdp_p.body;
    let stellar_xdr::LiquidityPoolEntryBody::LiquidityPoolConstantProduct(ref our_cp) = our_p.body;
    println!(
        "      CDP  pool: reserve_a={} reserve_b={}",
        cdp_cp.reserve_a, cdp_cp.reserve_b
    );
    println!(
        "      Ours pool: reserve_a={} reserve_b={}",
        our_cp.reserve_a, our_cp.reserve_b
    );
}

/// Formats a short key description for a CDP entry that is truly missing from
/// our state. Returns the string; the caller emits the `println!`.
fn format_missing_entry_key(data: &stellar_xdr::LedgerEntryData) -> String {
    match data {
        stellar_xdr::LedgerEntryData::Account(a) => {
            let stellar_xdr::PublicKey::PublicKeyTypeEd25519(ref pk) = a.account_id.0;
            format!("Account({})", hex::encode(&pk.0[..8]))
        }
        stellar_xdr::LedgerEntryData::Trustline(t) => {
            let stellar_xdr::PublicKey::PublicKeyTypeEd25519(ref pk) = t.account_id.0;
            format!(
                "Trustline(acct={}, asset={:?}, balance={})",
                hex::encode(&pk.0[..8]),
                t.asset,
                t.balance
            )
        }
        stellar_xdr::LedgerEntryData::LiquidityPool(p) => {
            let stellar_xdr::LiquidityPoolEntryBody::LiquidityPoolConstantProduct(ref cp) = p.body;
            format!("Pool(ra={}, rb={})", cp.reserve_a, cp.reserve_b)
        }
        other => format!("{:?}", std::mem::discriminant(other)),
    }
}

/// Compares the coalesced CDP final entries against our bucket-list state,
/// printing per-entry DIFF/MISSING details and a final summary line.
///
/// Kept as ONE helper: the shared `diffs`/`missing` counters and the
/// `if diffs >= 20 { break }` cap must not be split (splitting would change
/// control flow). The `bucket_list()` `RwLockReadGuard` (`drop(bl)` ordering)
/// and the `offer_store_lock()` `MutexGuard` scopes stay inside this helper.
pub(super) fn print_cdp_entry_comparison(
    ctx: &super::VerifyContext,
    result_header: &stellar_xdr::LedgerHeader,
    final_entries: &std::collections::HashMap<Vec<u8>, stellar_xdr::LedgerEntry>,
) {
    use sha2::{Digest, Sha256};
    use stellar_xdr::WriteXdr;
    // Compare CDP entries with our bucket list state
    let bl = ctx.ledger_manager.bucket_list();
    let bl_snapshot = henyey_bucket::BucketListSnapshot::new(&bl, result_header.clone());
    drop(bl);
    let mut diffs = 0;
    let mut missing = 0;
    for (key_bytes, cdp_entry) in final_entries.iter() {
        use stellar_xdr::ReadXdr;
        if let Ok(key) =
            stellar_xdr::LedgerKey::from_xdr(key_bytes.as_slice(), stellar_xdr::Limits::none())
        {
            let cdp_xdr = cdp_entry
                .to_xdr(stellar_xdr::Limits::none())
                .unwrap_or_default();
            let cdp_hash = {
                let mut h = Sha256::new();
                h.update(&cdp_xdr);
                format!("{:x}", h.finalize())
            };
            match bl_snapshot.get(&key) {
                Some(our_entry) => {
                    let our_xdr = our_entry
                        .to_xdr(stellar_xdr::Limits::none())
                        .unwrap_or_default();
                    let our_hash = {
                        let mut h = Sha256::new();
                        h.update(&our_xdr);
                        format!("{:x}", h.finalize())
                    };
                    if our_hash != cdp_hash {
                        diffs += 1;
                        let key_str = format!("{:?}", std::mem::discriminant(&cdp_entry.data));
                        println!("    ENTRY DIFF #{}: key={:?}", diffs, key_str);
                        println!(
                            "      CDP:  lm={} hash={}",
                            cdp_entry.last_modified_ledger_seq, cdp_hash
                        );
                        println!(
                            "      Ours: lm={} hash={}",
                            our_entry.last_modified_ledger_seq, our_hash
                        );
                        println!(
                            "      CDP  xdr: {}",
                            hex::encode(&cdp_xdr[..cdp_xdr.len().min(200)])
                        );
                        println!(
                            "      Ours xdr: {}",
                            hex::encode(&our_xdr[..our_xdr.len().min(200)])
                        );
                        // For offers, show readable details
                        if let (
                            stellar_xdr::LedgerEntryData::Offer(cdp_o),
                            stellar_xdr::LedgerEntryData::Offer(our_o),
                        ) = (&cdp_entry.data, &our_entry.data)
                        {
                            print_offer_diff_detail(cdp_o, our_o);
                        }
                        if let (
                            stellar_xdr::LedgerEntryData::Account(cdp_a),
                            stellar_xdr::LedgerEntryData::Account(our_a),
                        ) = (&cdp_entry.data, &our_entry.data)
                        {
                            print_account_diff_detail(cdp_a, our_a);
                        }
                        if let (
                            stellar_xdr::LedgerEntryData::Trustline(cdp_t),
                            stellar_xdr::LedgerEntryData::Trustline(our_t),
                        ) = (&cdp_entry.data, &our_entry.data)
                        {
                            print_trustline_diff_detail(cdp_t, our_t);
                        }
                        if let (
                            stellar_xdr::LedgerEntryData::LiquidityPool(cdp_p),
                            stellar_xdr::LedgerEntryData::LiquidityPool(our_p),
                        ) = (&cdp_entry.data, &our_entry.data)
                        {
                            print_pool_diff_detail(cdp_p, our_p);
                        }
                        if diffs >= 20 {
                            break;
                        }
                    }
                }
                None => {
                    // For offers, try the offer_store instead of bucket list snapshot
                    // (offers are not indexed in bucket list snapshot)
                    if let stellar_xdr::LedgerEntryData::Offer(ref cdp_offer) = cdp_entry.data {
                        let offer_store = ctx.ledger_manager.offer_store_lock();
                        if let Some(our_entry) = offer_store
                            .get_ledger_entry_by_id(cdp_offer.offer_id)
                            .as_ref()
                        {
                            let our_xdr = our_entry
                                .to_xdr(stellar_xdr::Limits::none())
                                .unwrap_or_default();
                            let our_hash = {
                                let mut h = Sha256::new();
                                h.update(&our_xdr);
                                format!("{:x}", h.finalize())
                            };
                            if our_hash != cdp_hash {
                                diffs += 1;
                                if let stellar_xdr::LedgerEntryData::Offer(ref our_offer) =
                                    our_entry.data
                                {
                                    let cdp_seller = {
                                        let stellar_xdr::PublicKey::PublicKeyTypeEd25519(ref pk) =
                                            cdp_offer.seller_id.0;
                                        hex::encode(&pk.0[..8])
                                    };
                                    println!(
                                        "    OFFER DIFF #{}: id={} seller={}",
                                        diffs, cdp_offer.offer_id, cdp_seller
                                    );
                                    println!(
                                        "      CDP:  amount={} price={}/{} lm={}",
                                        cdp_offer.amount,
                                        cdp_offer.price.n,
                                        cdp_offer.price.d,
                                        cdp_entry.last_modified_ledger_seq
                                    );
                                    println!(
                                        "      Ours: amount={} price={}/{} lm={}",
                                        our_offer.amount,
                                        our_offer.price.n,
                                        our_offer.price.d,
                                        our_entry.last_modified_ledger_seq
                                    );
                                }
                            }
                            // else: offer matches, not a real diff
                        } else {
                            // Offer is truly missing from our state
                            missing += 1;
                            let cdp_seller = {
                                let stellar_xdr::PublicKey::PublicKeyTypeEd25519(ref pk) =
                                    cdp_offer.seller_id.0;
                                hex::encode(&pk.0)
                            };
                            println!("    TRULY MISSING offer: id={} seller={} amount={} price={}/{} cdp_lm={}",
                            cdp_offer.offer_id, cdp_seller, cdp_offer.amount,
                            cdp_offer.price.n, cdp_offer.price.d, cdp_entry.last_modified_ledger_seq);
                        }
                    } else {
                        // Non-offer entry truly missing
                        missing += 1;
                        let key_str = format_missing_entry_key(&cdp_entry.data);
                        println!(
                            "    MISSING in our state: {} cdp_lm={} hash={}",
                            key_str, cdp_entry.last_modified_ledger_seq, cdp_hash
                        );
                        println!(
                            "      cdp_xdr: {}",
                            hex::encode(&cdp_xdr[..cdp_xdr.len().min(200)])
                        );
                    }
                }
            }
        }
    }
    println!(
        "    Entry comparison: {} diffs, {} truly missing (out of {} CDP entries)",
        diffs,
        missing,
        final_entries.len()
    );
}

#[cfg(test)]
mod tests {
    use super::*;
    use stellar_xdr::{
        AccountEntry, AccountEntryExt, AccountId, ExtensionPoint, GeneralizedTransactionSet, Hash,
        LedgerCloseMeta, LedgerCloseMetaExt, LedgerCloseMetaV2, LedgerEntry, LedgerEntryChange,
        LedgerEntryData, LedgerEntryExt, LedgerHeader, LedgerHeaderExt, LedgerHeaderHistoryEntry,
        LedgerHeaderHistoryEntryExt, LedgerUpgrade, OperationMeta, PublicKey, SequenceNumber,
        StellarValue, StellarValueExt, String32, Thresholds, TimePoint, TransactionMeta,
        TransactionMetaV2, TransactionResult, TransactionResultExt, TransactionResultMetaV1,
        TransactionResultPair, TransactionResultResult, Uint256, UpgradeEntryMeta, VecM, WriteXdr,
    };

    /// Minimal `LedgerEntry` (Account) keyed by `id_byte` with the given balance.
    fn make_account_entry(id_byte: u8, balance: i64) -> LedgerEntry {
        LedgerEntry {
            last_modified_ledger_seq: 0,
            data: LedgerEntryData::Account(AccountEntry {
                account_id: AccountId(PublicKey::PublicKeyTypeEd25519(Uint256([id_byte; 32]))),
                balance,
                seq_num: SequenceNumber(0),
                num_sub_entries: 0,
                inflation_dest: None,
                flags: 0,
                home_domain: String32::default(),
                thresholds: Thresholds([0; 4]),
                signers: vec![].try_into().unwrap(),
                ext: AccountEntryExt::V0,
            }),
            ext: LedgerEntryExt::V0,
        }
    }

    // --- cdp_change_counts -------------------------------------------------

    #[test]
    fn test_cdp_change_counts_classifies_and_folds_upgrades() {
        // tx_metas: one V2 meta covering every change variant. The counting
        // mapping is Created→creates, Updated+Restored→updates, Removed→deletes,
        // State→ignored.
        let tx_meta = TransactionMeta::V2(TransactionMetaV2 {
            tx_changes_before: vec![
                LedgerEntryChange::Created(make_account_entry(1, 0)),
                LedgerEntryChange::State(make_account_entry(2, 0)), // ignored
            ]
            .try_into()
            .unwrap(),
            operations: vec![OperationMeta {
                changes: vec![
                    LedgerEntryChange::Updated(make_account_entry(3, 0)),
                    LedgerEntryChange::Restored(make_account_entry(4, 0)), // counts as update
                ]
                .try_into()
                .unwrap(),
            }]
            .try_into()
            .unwrap(),
            tx_changes_after: vec![LedgerEntryChange::Removed(henyey_common::entry_to_key(
                &make_account_entry(5, 0),
            ))]
            .try_into()
            .unwrap(),
        });

        // upgrade_metas: only Created/Updated are folded in; others ignored.
        let upgrade_metas = vec![UpgradeEntryMeta {
            upgrade: LedgerUpgrade::Version(0),
            changes: vec![
                LedgerEntryChange::Created(make_account_entry(6, 0)),
                LedgerEntryChange::Updated(make_account_entry(7, 0)),
                LedgerEntryChange::State(make_account_entry(8, 0)), // ignored
            ]
            .try_into()
            .unwrap(),
        }];

        let (creates, updates, deletes, upgrade_creates, upgrade_updates) =
            cdp_change_counts(&[tx_meta], &upgrade_metas);

        assert_eq!(creates, 1);
        assert_eq!(updates, 2); // one Updated + one Restored
        assert_eq!(deletes, 1);
        assert_eq!(upgrade_creates, 1);
        assert_eq!(upgrade_updates, 1);
    }

    // --- coalesce_cdp_final_entries ----------------------------------------

    /// Build a one-tx V2 `LedgerCloseMeta` whose `tx_apply_processing` and
    /// `post_tx_apply_fee_processing` carry the supplied change groups.
    fn make_v2_lcm(
        apply_changes: Vec<LedgerEntryChange>,
        post_fee_changes: Vec<LedgerEntryChange>,
    ) -> LedgerCloseMeta {
        let header = LedgerHeader {
            ledger_version: 24,
            previous_ledger_hash: Hash([0u8; 32]),
            scp_value: StellarValue {
                tx_set_hash: Hash([0u8; 32]),
                close_time: TimePoint(0),
                upgrades: VecM::default(),
                ext: StellarValueExt::Basic,
            },
            tx_set_result_hash: Hash([0u8; 32]),
            bucket_list_hash: Hash([0u8; 32]),
            ledger_seq: 1,
            total_coins: 0,
            fee_pool: 0,
            inflation_seq: 0,
            id_pool: 0,
            base_fee: 100,
            base_reserve: 100_000_000,
            max_tx_set_size: 100,
            skip_list: std::array::from_fn(|_| Hash([0u8; 32])),
            ext: LedgerHeaderExt::V0,
        };
        let tx_apply_processing = TransactionMeta::V2(TransactionMetaV2 {
            tx_changes_before: vec![].try_into().unwrap(),
            operations: vec![OperationMeta {
                changes: apply_changes.try_into().unwrap(),
            }]
            .try_into()
            .unwrap(),
            tx_changes_after: vec![].try_into().unwrap(),
        });
        let trm = TransactionResultMetaV1 {
            ext: ExtensionPoint::V0,
            result: TransactionResultPair {
                transaction_hash: Hash([0u8; 32]),
                result: TransactionResult {
                    fee_charged: 0,
                    result: TransactionResultResult::TxSuccess(vec![].try_into().unwrap()),
                    ext: TransactionResultExt::V0,
                },
            },
            fee_processing: vec![].try_into().unwrap(),
            tx_apply_processing,
            post_tx_apply_fee_processing: post_fee_changes.try_into().unwrap(),
        };
        LedgerCloseMeta::V2(LedgerCloseMetaV2 {
            ext: LedgerCloseMetaExt::V0,
            ledger_header: LedgerHeaderHistoryEntry {
                hash: Hash([0u8; 32]),
                header,
                ext: LedgerHeaderHistoryEntryExt::V0,
            },
            tx_set: GeneralizedTransactionSet::default(),
            tx_processing: vec![trm].try_into().unwrap(),
            upgrades_processing: VecM::default(),
            scp_info: VecM::default(),
            total_byte_size_of_live_soroban_state: 0,
            evicted_keys: VecM::default(),
        })
    }

    fn entry_key_bytes(entry: &LedgerEntry) -> Vec<u8> {
        henyey_common::entry_to_key(entry)
            .to_xdr(stellar_xdr::Limits::none())
            .unwrap()
    }

    #[test]
    fn test_coalesce_cdp_final_entries_last_write_wins() {
        // Account A: created (bal=1) then updated (bal=2) → survives with bal=2.
        // Account B: created then removed (in post_tx_apply_fee_processing) → gone.
        let a_v1 = make_account_entry(0xAA, 1);
        let a_v2 = make_account_entry(0xAA, 2);
        let b = make_account_entry(0xBB, 5);

        let lcm = make_v2_lcm(
            vec![
                LedgerEntryChange::Created(a_v1.clone()),
                LedgerEntryChange::Updated(a_v2.clone()),
                LedgerEntryChange::Created(b.clone()),
            ],
            vec![LedgerEntryChange::Removed(henyey_common::entry_to_key(&b))],
        );

        let coalesced = coalesce_cdp_final_entries(&lcm);

        // Only Account A survives, with the last-written balance.
        assert_eq!(coalesced.len(), 1);
        let a_key = entry_key_bytes(&a_v2);
        let surviving = coalesced.get(&a_key).expect("Account A should survive");
        match &surviving.data {
            LedgerEntryData::Account(acc) => assert_eq!(acc.balance, 2),
            other => panic!("expected Account, got {other:?}"),
        }
        // Account B was removed.
        assert!(!coalesced.contains_key(&entry_key_bytes(&b)));
    }
}
