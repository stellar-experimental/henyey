//! Offline execution verification — replays ledgers against CDP data.

use std::sync::Arc;
use std::time::{Duration, Instant};

use stellar_xdr::curr::WriteXdr;

use henyey_app::AppConfig;
use henyey_bucket::{BucketList, BucketManager, HotArchiveBucketList, PendingMergeState};
use henyey_common::Hash256;
use henyey_history::cdp::{
    extract_ledger_close_data, extract_ledger_header, extract_transaction_results,
    CachedCdpDataLake,
};
use henyey_history::checkpoint;
use henyey_history::verify;
use henyey_ledger::{LedgerManager, LedgerManagerConfig};

pub(crate) struct VerifyExecutionOptions {
    pub from: Option<u32>,
    pub to: Option<u32>,
    pub stop_on_error: bool,
    pub show_diff: bool,
    pub cdp_url: Option<String>,
    pub cdp_date: Option<String>,
    pub cache_dir: Option<std::path::PathBuf>,
    pub no_cache: bool,
    pub quiet: bool,
}

/// Bundles all long-lived resources needed across verification phases.
pub(crate) struct VerifyContext {
    archive: henyey_history::HistoryArchive,
    cdp: CachedCdpDataLake,
    ledger_manager: LedgerManager,
    _bucket_manager: Arc<BucketManager>,
    // TempDir guards — dropping these would delete the temp directories.
    _cdp_dir_holder: Option<tempfile::TempDir>,
    _bucket_dir_holder: Option<tempfile::TempDir>,
    // Configuration
    start_ledger: u32,
    end_ledger: u32,
    init_checkpoint: u32,
    end_checkpoint: u32,
    init_header_hash: Hash256,
    stop_on_error: bool,
    show_diff: bool,
    quiet: bool,
}

/// Accumulator counters for the verification run.
#[derive(Default)]
pub(crate) struct VerifyStats {
    pub ledgers_verified: u32,
    pub ledgers_matched: u32,
    pub ledgers_mismatched: u32,
    pub header_mismatches: u32,
    pub tx_result_mismatches: u32,
    pub meta_mismatches: u32,
    total_close_us: u64,
    total_tx_exec_us: u64,
    total_commit_us: u64,
    total_add_batch_us: u64,
    total_eviction_us: u64,
    total_tx_count: usize,
    total_cache_hits: u64,
    total_cache_misses: u64,
    slowest_ledger_us: u64,
    slowest_ledger_seq: u32,
    slowest_txs: Vec<(u32, String, u64)>,
    peak_rss_bytes: u64,
}

/// Returns a short description of a TransactionEnvelope for diagnostics:
/// (op_types_csv, declared_fee, soroban_resource_fee, soroban_resources_summary).
fn describe_envelope(
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
fn extract_diagnostic_event_summary(meta: Option<&stellar_xdr::curr::TransactionMeta>) -> String {
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
fn summarize_cdp_meta_changes(meta: Option<&stellar_xdr::curr::TransactionMeta>) -> String {
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
fn summarize_cdp_meta(meta: Option<&stellar_xdr::curr::TransactionMeta>) -> (u32, u64) {
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
fn describe_op_results(r: &stellar_xdr::curr::TransactionResultResult) -> String {
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
fn tx_result_code_name(r: &stellar_xdr::curr::TransactionResultResult) -> String {
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
fn print_op_diffs(
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
fn print_all_ops(label: &str, ops: &[stellar_xdr::curr::OperationResult]) {
    println!("        {} ops ({}):", label, ops.len());
    for (j, op) in ops.iter().enumerate() {
        println!("          Op {}: {:?}", j, op);
    }
}

/// Prints an exhaustive field-by-field comparison of two ledger headers.
fn print_header_field_diffs(
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
fn print_fee_bump_inner_diffs(
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
fn print_tx_result_diffs(
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

/// Verifies transaction execution by comparing results against CDP metadata.
///
/// Restores bucket list state from a checkpoint, re-executes transactions via
/// `close_ledger`, and compares results against CDP-produced ledger close metadata.
pub(crate) async fn cmd_verify_execution(
    config: AppConfig,
    opts: VerifyExecutionOptions,
) -> anyhow::Result<()> {
    let mut ctx = setup(config, opts).await?;
    let (mut stats, elapsed) = run_verification_loop(&mut ctx).await?;
    print_summary(&mut stats, elapsed);
    if stats.ledgers_mismatched > 0 {
        anyhow::bail!(
            "Verification failed with {} mismatched ledgers",
            stats.ledgers_mismatched
        );
    }
    Ok(())
}

/// Phase 1-4: Parse config, create clients, download state, initialize LedgerManager.
async fn setup(config: AppConfig, opts: VerifyExecutionOptions) -> anyhow::Result<VerifyContext> {
    let VerifyExecutionOptions {
        from,
        to,
        stop_on_error,
        show_diff,
        cdp_url,
        cdp_date,
        cache_dir,
        no_cache,
        quiet,
    } = opts;

    let init_start = Instant::now();

    if !quiet {
        println!("Transaction Execution Verification");
        println!("===================================");
        println!("Executes transactions via close_ledger and compares against CDP.");
        println!();
    }

    // Determine network name
    let (network_name, is_mainnet) =
        if config.network.passphrase == "Test SDF Network ; September 2015" {
            ("testnet", false)
        } else {
            ("mainnet", true)
        };

    // Set network-specific CDP defaults
    let cdp_url = cdp_url.unwrap_or_else(|| {
        if is_mainnet {
            "https://aws-public-blockchain.s3.us-east-2.amazonaws.com/v1.1/stellar/ledgers/pubnet"
                .to_string()
        } else {
            "https://aws-public-blockchain.s3.us-east-2.amazonaws.com/v1.1/stellar/ledgers/testnet"
                .to_string()
        }
    });
    let cdp_date = cdp_date.unwrap_or_else(|| {
        if is_mainnet {
            String::new()
        } else {
            "2025-12-18".to_string()
        }
    });

    // Determine cache directory
    let cache_base = if no_cache {
        None
    } else {
        cache_dir.or_else(|| dirs::cache_dir().map(|p| p.join("rs-stellar-core")))
    };

    // Create archive client
    let archive = super::first_archive(&config)?;

    if !quiet {
        println!("Archive: {}", config.history.archives[0].url);
        let cdp_date_display = if cdp_date.is_empty() {
            "none (range-based)"
        } else {
            &cdp_date
        };
        println!("CDP: {} (date: {})", cdp_url, cdp_date_display);
        if let Some(ref cache) = cache_base {
            println!("Cache: {}", cache.display());
        } else {
            println!("Cache: disabled");
        }
    }

    // Get current ledger and calculate range
    let root_has = archive.fetch_root_has().await?;
    let current_ledger = root_has.current_ledger;

    let end_ledger = to.unwrap_or_else(|| {
        checkpoint::latest_checkpoint_before_or_at(current_ledger).unwrap_or(current_ledger)
    });
    let start_ledger = from.unwrap_or_else(|| {
        let freq = henyey_history::checkpoint_frequency();
        checkpoint::checkpoint_containing(end_ledger)
            .saturating_sub(4 * freq)
            .max(freq)
    });

    let freq = henyey_history::checkpoint_frequency();
    let init_checkpoint =
        checkpoint::latest_checkpoint_before_or_at(start_ledger.saturating_sub(1))
            .unwrap_or(freq - 1);
    let end_checkpoint = checkpoint::checkpoint_containing(end_ledger);

    if !quiet {
        println!("Ledger range: {} to {}", start_ledger, end_ledger);
        println!("Initial state: checkpoint {}", init_checkpoint);
        println!();
    }

    // Create CDP client with caching
    let (_cdp_dir_holder, cdp) = if let Some(ref cache) = cache_base {
        let cdp = CachedCdpDataLake::new(&cdp_url, &cdp_date, cache, network_name)?;
        (None, cdp)
    } else {
        let temp = tempfile::tempdir()?;
        let cdp = CachedCdpDataLake::new(&cdp_url, &cdp_date, temp.path(), network_name)?;
        (Some(temp), cdp)
    };

    // Setup bucket manager
    let (_bucket_dir_holder, bucket_path) = if let Some(ref cache) = cache_base {
        let path = cache.join("buckets").join(network_name);
        std::fs::create_dir_all(&path)?;
        (None, path)
    } else {
        let temp = tempfile::tempdir()?;
        let path = temp.path().to_path_buf();
        (Some(temp), path)
    };
    let bucket_manager = Arc::new(BucketManager::with_persist_index(
        bucket_path.clone(),
        true,
    )?);

    // Download initial state
    if !quiet {
        println!(
            "Downloading initial state at checkpoint {}...",
            init_checkpoint
        );
    }
    let init_has = archive.fetch_checkpoint_has(init_checkpoint).await?;

    // Extract bucket hashes
    let bucket_hashes = init_has.bucket_hash_pairs();

    let live_next_states: Vec<Option<PendingMergeState>> = init_has.live_next_states()?;

    // Extract hot archive bucket hashes (protocol 23+)
    let hot_archive_hashes = init_has.hot_archive_bucket_hash_pairs();

    let hot_archive_next_states: Option<Vec<Option<PendingMergeState>>> =
        init_has.hot_archive_next_states()?;

    // Collect all bucket hashes to download
    let mut all_hashes: Vec<Hash256> = Vec::new();
    for (curr, snap) in &bucket_hashes {
        all_hashes.push(*curr);
        all_hashes.push(*snap);
    }
    for merge_state in live_next_states.iter().flatten() {
        all_hashes.extend(merge_state.referenced_hashes().copied());
    }
    if let Some(ref ha_hashes) = hot_archive_hashes {
        for (curr, snap) in ha_hashes {
            all_hashes.push(*curr);
            all_hashes.push(*snap);
        }
    }
    if let Some(ref ha_states) = hot_archive_next_states {
        for merge_state in ha_states.iter().flatten() {
            all_hashes.extend(merge_state.referenced_hashes().copied());
        }
    }
    let all_hashes: Vec<&Hash256> = all_hashes.iter().filter(|h| !h.is_zero()).collect();

    // Download buckets
    let (cached, downloaded) =
        super::download_buckets_parallel(&archive, bucket_manager.clone(), all_hashes).await?;
    println!(
        "[INIT] Bucket download: {} cached, {} downloaded",
        cached, downloaded
    );

    // Restore bucket lists
    let mut bucket_list =
        BucketList::restore_from_has(&bucket_hashes, &live_next_states, |hash| {
            bucket_manager.load_bucket(hash).map(|b| (*b).clone())
        })?;
    bucket_list.set_bucket_dir(bucket_manager.bucket_dir().to_path_buf());

    let mut hot_archive_bucket_list = match (&hot_archive_hashes, &hot_archive_next_states) {
        (Some(ref hashes), Some(ref next_states)) => {
            HotArchiveBucketList::restore_from_has(hashes, next_states, |hash| {
                bucket_manager.load_hot_archive_bucket(hash)
            })?
        }
        _ => HotArchiveBucketList::new(),
    };

    // Get init header and restart merges
    let init_headers = archive.fetch_ledger_headers(init_checkpoint).await?;
    let init_header_entry = init_headers
        .iter()
        .find(|h| h.header.ledger_seq == init_checkpoint);
    let init_protocol_version = init_header_entry
        .map(|h| h.header.ledger_version)
        .unwrap_or(25);

    // Enable structure-based merge restarts to match stellar-core online mode behavior.
    //
    // Although stellar-core standalone offline commands skip restartMerges, we are comparing
    // our results against CDP headers produced by stellar-core in ONLINE mode. The stellar-core node
    // that produced those headers had full structure-based merge restarts enabled.
    //
    // In stellar-core online mode, restartMerges uses mLevels[i-1].getSnap() (the old snap
    // from HAS) to start merges. Without structure-based restarts, add_batch would
    // use snap() which returns the snapped curr (different input!).
    bucket_list
        .restart_merges_from_has(
            init_checkpoint,
            init_protocol_version,
            &live_next_states,
            |hash| bucket_manager.load_bucket_for_merge(hash),
            true, // restart_structure_based = true to match stellar-core online mode
        )
        .await?;

    if let Some(ref ha_next_states) = hot_archive_next_states {
        hot_archive_bucket_list.restart_merges_from_has(
            init_checkpoint,
            init_protocol_version,
            ha_next_states,
            |hash| bucket_manager.load_hot_archive_bucket_for_merge(hash),
            true, // restart_structure_based = true to match stellar-core online mode
        )?;
    }

    // Create and initialize LedgerManager
    let mut ledger_manager = LedgerManager::new(
        config.network.passphrase.clone(),
        LedgerManagerConfig {
            validate_bucket_hash: true,
            bucket_list_db: config.buckets.bucket_list_db.clone(),
            // Use single-threaded scan to reduce peak memory during initialization.
            // Verify-execution runs on memory-constrained CI runners where concurrent
            // level scans would cause OOM.
            scan_thread_count: 1,
            ..Default::default()
        },
    );

    // Wire merge map for bucket merge deduplication during replay.
    let finished_merges =
        std::sync::Arc::new(std::sync::RwLock::new(henyey_bucket::BucketMergeMap::new()));
    ledger_manager.set_merge_map(finished_merges);

    let init_header_entry = init_header_entry
        .ok_or_else(|| anyhow::anyhow!("No header found for checkpoint {}", init_checkpoint))?;
    let init_header_hash = verify::verify_ledger_header_history_entry(&init_header_entry)?;
    ledger_manager.initialize(
        bucket_list,
        hot_archive_bucket_list,
        init_header_entry.header.clone(),
        init_header_hash,
    )?;

    println!(
        "[INIT] TOTAL initialization: {:.2}s",
        init_start.elapsed().as_secs_f64()
    );

    Ok(VerifyContext {
        archive,
        cdp,
        ledger_manager,
        _bucket_manager: bucket_manager,
        _cdp_dir_holder,
        _bucket_dir_holder,
        start_ledger,
        end_ledger,
        init_checkpoint,
        end_checkpoint,
        init_header_hash,
        stop_on_error,
        show_diff,
        quiet,
    })
}

/// Phase 5: Main verification loop — iterate checkpoints and verify each ledger.
async fn run_verification_loop(ctx: &mut VerifyContext) -> anyhow::Result<(VerifyStats, Duration)> {
    let mut stats = VerifyStats::default();
    let mut prev_ledger_hash = ctx.init_header_hash;

    let verification_start = Instant::now();
    let process_from = ctx.init_checkpoint + 1;
    let process_from_cp = checkpoint::checkpoint_containing(process_from);

    let mut current_cp = process_from_cp;
    while current_cp <= ctx.end_checkpoint {
        let headers = ctx.archive.fetch_ledger_headers(current_cp).await?;

        for header_entry in &headers {
            verify_single_ledger(ctx, &mut stats, &mut prev_ledger_hash, header_entry).await?;
        }

        current_cp = checkpoint::next_checkpoint(current_cp);
    }

    let elapsed = verification_start.elapsed();
    Ok((stats, elapsed))
}

/// Disposition of a ledger relative to the verification range.
///
/// Drives the orchestrator's gating: whether a ledger is skipped (out of range),
/// in range but below the test window, or inside the test window. It also carries
/// the prev-hash chaining rule for the skip path so that decision can be unit-tested
/// directly, even though the actual `*prev_ledger_hash = …` write stays in the
/// orchestrator.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum LedgerDisposition {
    /// Out of range (`seq <= init_checkpoint || seq > end_ledger`). On this path the
    /// orchestrator returns early. `update_prev_hash` encodes the subtle rule that the
    /// running prev-hash is still advanced when `seq > init_checkpoint`, but NOT when
    /// `seq <= init_checkpoint`.
    OutOfRange { update_prev_hash: bool },
    /// In range but below the test window (`!(seq >= start_ledger && seq <= end_ledger)`
    /// while still in range). Processed but not counted toward test stats.
    InRangeBelowTest,
    /// Inside the test window (`seq >= start_ledger && seq <= end_ledger`).
    InTestRange,
}

/// Classify a ledger sequence against the verification range.
///
/// Pure mirror of the orchestrator's gating logic:
/// - skip when `seq <= init_checkpoint || seq > end_ledger`
/// - `in_test_range = seq >= start_ledger && seq <= end_ledger`
///
/// On the skip path, prev-hash is advanced only when `seq > init_checkpoint`.
fn classify_ledger(
    seq: u32,
    init_checkpoint: u32,
    start_ledger: u32,
    end_ledger: u32,
) -> LedgerDisposition {
    if seq <= init_checkpoint || seq > end_ledger {
        return LedgerDisposition::OutOfRange {
            update_prev_hash: seq > init_checkpoint,
        };
    }
    if seq >= start_ledger && seq <= end_ledger {
        LedgerDisposition::InTestRange
    } else {
        LedgerDisposition::InRangeBelowTest
    }
}

/// Whether the archive and CDP headers agree on the SCP close time.
///
/// Pure mirror of `header.scp_value.close_time.0 != cdp_header.scp_value.close_time.0`
/// (returns `true` when they are equal, i.e. the epoch matches).
fn epoch_matches(archive_close_time: u64, cdp_close_time: u64) -> bool {
    archive_close_time == cdp_close_time
}

/// The header/tx-result/meta comparison verdict for an in-test-range ledger.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct MatchVerdict {
    header_matches: bool,
    tx_result_matches: bool,
    meta_matches: bool,
    all_match: bool,
}

/// Compute the match verdict from the four comparison inputs.
///
/// Pure mirror of the orchestrator's comparison block, pinned AS-IS:
/// `meta_matches = meta_is_none || tx_result_matches` (a deliberate henyey-local
/// diagnostic simplification — NOT "improved" here) and
/// `all_match = header_matches && tx_result_matches && meta_matches`.
fn compare_results(
    our_header_hash: Hash256,
    expected_header_hash: Hash256,
    our_tx_result_hash: Hash256,
    expected_tx_result_hash: Hash256,
    meta_is_none: bool,
) -> MatchVerdict {
    let header_matches = our_header_hash == expected_header_hash;
    let tx_result_matches = our_tx_result_hash == expected_tx_result_hash;
    let meta_matches = meta_is_none || tx_result_matches;
    let all_match = header_matches && tx_result_matches && meta_matches;
    MatchVerdict {
        header_matches,
        tx_result_matches,
        meta_matches,
        all_match,
    }
}

/// Apply the match/mismatch counter increments for an in-test-range verdict.
///
/// Pure mirror of the orchestrator's `ledgers_matched` / `ledgers_mismatched` /
/// per-category mismatch increments.
fn record_match_counters(stats: &mut VerifyStats, v: &MatchVerdict) {
    if v.all_match {
        stats.ledgers_matched += 1;
    } else {
        stats.ledgers_mismatched += 1;
        if !v.header_matches {
            stats.header_mismatches += 1;
        }
        if !v.tx_result_matches {
            stats.tx_result_mismatches += 1;
        }
        if !v.meta_matches {
            stats.meta_mismatches += 1;
        }
    }
}

/// The 5-term commit-time sum used by both the perf accumulator and the per-64
/// print line. Centralized so the duplicated expression cannot drift.
fn commit_us(perf: &henyey_ledger::LedgerClosePerf) -> u64 {
    perf.commit_setup_us
        + perf.add_batch_us
        + perf.hot_archive_us
        + perf.header_us
        + perf.commit_close_us
}

/// Accumulate per-ledger performance metrics into the running stats.
///
/// Pure mirror of the orchestrator's perf-accumulation block (no prints).
fn record_perf(stats: &mut VerifyStats, perf: &henyey_ledger::LedgerClosePerf, seq: u32) {
    stats.total_close_us += perf.total_us;
    stats.total_tx_exec_us += perf.tx_exec_us;
    stats.total_commit_us += commit_us(perf);
    stats.total_add_batch_us += perf.add_batch_us;
    stats.total_eviction_us += perf.eviction_us;
    stats.total_tx_count += perf.tx_count;
    stats.total_cache_hits += perf.cache.hits;
    stats.total_cache_misses += perf.cache.misses;
    if perf.rss_after_bytes > stats.peak_rss_bytes {
        stats.peak_rss_bytes = perf.rss_after_bytes;
    }
    if perf.total_us > stats.slowest_ledger_us {
        stats.slowest_ledger_us = perf.total_us;
        stats.slowest_ledger_seq = seq;
    }
    // Track top slowest transactions across all ledgers
    for tx in &perf.tx_timings {
        stats
            .slowest_txs
            .push((seq, tx.hash_hex.clone(), tx.exec_us));
    }
}

/// Process a single ledger: fetch CDP data, execute close_ledger, compare results.
async fn verify_single_ledger(
    ctx: &mut VerifyContext,
    stats: &mut VerifyStats,
    prev_ledger_hash: &mut Hash256,
    header_entry: &stellar_xdr::curr::LedgerHeaderHistoryEntry,
) -> anyhow::Result<()> {
    let header = &header_entry.header;
    let seq = header.ledger_seq;
    let verified_header_hash = verify::verify_ledger_header_history_entry(header_entry)?;

    // Skip ledgers outside our range
    let disposition = classify_ledger(seq, ctx.init_checkpoint, ctx.start_ledger, ctx.end_ledger);
    if let LedgerDisposition::OutOfRange { update_prev_hash } = disposition {
        if update_prev_hash {
            *prev_ledger_hash = verified_header_hash;
        }
        return Ok(());
    }

    let in_test_range = disposition == LedgerDisposition::InTestRange;

    // Fetch CDP metadata
    let lcm = match ctx.cdp.fetch_ledger_close_meta(seq).await {
        Ok(lcm) => lcm,
        Err(e) => {
            if in_test_range {
                println!("  Ledger {}: CDP fetch failed: {}", seq, e);
            }
            *prev_ledger_hash = verified_header_hash;
            return Ok(());
        }
    };

    let cdp_header = extract_ledger_header(&lcm);

    // Validate CDP data matches archive
    if !epoch_matches(
        header.scp_value.close_time.0,
        cdp_header.scp_value.close_time.0,
    ) {
        if in_test_range {
            println!("  Ledger {}: EPOCH MISMATCH - skipping", seq);
        }
        if ctx.stop_on_error {
            anyhow::bail!("CDP epoch mismatch at ledger {}", seq);
        }
        *prev_ledger_hash = verified_header_hash;
        return Ok(());
    }

    // Create LedgerCloseData from CDP with expected header hash for pre-commit validation.
    let close_data = extract_ledger_close_data(&lcm, *prev_ledger_hash, Some(verified_header_hash));

    // Execute via close_ledger
    let result = match ctx.ledger_manager.close_ledger(close_data, None) {
        Ok(r) => r,
        Err(e) => {
            println!("  Ledger {}: close_ledger failed: {}", seq, e);
            // Print the CDP/mainnet expected header fields so the WARN
            // log emitted by manager.rs::commit (which prints OUR
            // computed fields) can be diffed against mainnet's directly
            // from CI output.
            tracing::warn!(
                target: "hash_mismatch_debug",
                ledger_seq = seq,
                expected_header_hash = %verified_header_hash.to_hex(),
                expected_bucket_list_hash = %Hash256::from(cdp_header.bucket_list_hash.0).to_hex(),
                expected_tx_result_hash = %Hash256::from(cdp_header.tx_set_result_hash.0).to_hex(),
                expected_total_coins = cdp_header.total_coins,
                expected_fee_pool = cdp_header.fee_pool,
                expected_inflation_seq = cdp_header.inflation_seq,
                expected_id_pool = cdp_header.id_pool,
                expected_base_fee = cdp_header.base_fee,
                expected_base_reserve = cdp_header.base_reserve,
                expected_max_tx_set_size = cdp_header.max_tx_set_size,
                expected_ledger_version = cdp_header.ledger_version,
                "Pre-commit hash mismatch (replay mode) - mainnet/CDP expected fields"
            );
            // Per-tx CDP result diagnostic: log each tx hash + fee_charged
            // from mainnet's recorded results so they can be diffed against
            // our per-tx WARN log emitted from manager.rs::commit.
            //
            // CRITICAL: cdp envelopes (from tx_set, canonical order) and
            // tx_processing (apply order) are NOT aligned by index. We MUST
            // align by tx hash via extract_transaction_processing.
            let network_id = stellar_xdr::curr::Hash(*ctx.ledger_manager.network_id().0.as_bytes());
            let cdp_processing =
                henyey_history::cdp::extract_transaction_processing(&lcm, &network_id);
            for (i, info) in cdp_processing.iter().enumerate() {
                let (op_types, declared_fee, soroban_resource_fee, soroban_resources) =
                    describe_envelope(Some(&info.envelope));
                let op_results = describe_op_results(&info.result.result.result);
                let (changes_count, changes_total_bytes) = summarize_cdp_meta(Some(&info.meta));
                let diag_events = extract_diagnostic_event_summary(Some(&info.meta));
                let changes_summary = summarize_cdp_meta_changes(Some(&info.meta));
                tracing::warn!(
                    target: "hash_mismatch_debug",
                    ledger_seq = seq,
                    tx_index = i,
                    tx_hash = %Hash256::from_bytes(info.result.transaction_hash.0).to_hex(),
                    fee_charged = info.result.result.fee_charged,
                    result_code = ?info.result.result.result.discriminant(),
                    op_results = %op_results,
                    declared_fee = declared_fee,
                    op_types = %op_types,
                    soroban_resource_fee = soroban_resource_fee,
                    soroban_resources = %soroban_resources,
                    cdp_meta_changes_count = changes_count,
                    cdp_meta_changes_total_bytes = changes_total_bytes,
                    diag_events = %diag_events,
                    changes = %changes_summary,
                    "Per-tx result (mainnet/CDP, hash-aligned)"
                );
            }
            if ctx.stop_on_error {
                anyhow::bail!("close_ledger failed at ledger {}: {}", seq, e);
            }
            *prev_ledger_hash = verified_header_hash;
            return Ok(());
        }
    };

    if in_test_range {
        stats.ledgers_verified += 1;

        // Compare header / tx-result / meta. `compare_results` mirrors the
        // (pinned-as-is) verdict logic, including
        // `meta_matches = meta.is_none() || tx_result_matches`.
        let expected_header_hash = verified_header_hash;
        let cdp_tx_results = extract_transaction_results(&lcm);
        let expected_tx_result_hash = Hash256::from(cdp_header.tx_set_result_hash.0);
        let our_tx_result_hash = result.tx_result_hash();
        let verdict = compare_results(
            result.header_hash,
            expected_header_hash,
            our_tx_result_hash,
            expected_tx_result_hash,
            result.meta.is_none(),
        );
        let MatchVerdict {
            header_matches,
            tx_result_matches,
            meta_matches: _,
            all_match,
        } = verdict;

        record_match_counters(stats, &verdict);

        if all_match {
            if !ctx.quiet {
                print!(".");
                if stats.ledgers_verified % 64 == 0 {
                    println!(" {}", seq);
                }
                std::io::Write::flush(&mut std::io::stdout()).ok();
            }
        } else {
            println!();
            println!("  Ledger {}: MISMATCH", seq);
            if !header_matches {
                println!(
                    "    Header hash: ours={} expected={}",
                    result.header_hash.to_hex(),
                    expected_header_hash.to_hex()
                );
                let bucket_levels = ctx.ledger_manager.bucket_list_levels();
                print_header_field_diffs(&result.header, &cdp_header, &bucket_levels);
            }
            if !tx_result_matches {
                println!(
                    "    TX result hash: ours={} expected={}",
                    our_tx_result_hash.to_hex(),
                    expected_tx_result_hash.to_hex()
                );
            }

            if ctx.show_diff && !tx_result_matches {
                print_tx_result_diffs(&result.tx_results, &cdp_tx_results);
            }

            // Compare eviction data when header mismatches but TX results match
            if !header_matches && tx_result_matches {
                print_eviction_and_entry_diagnostics(ctx, &lcm, &result.header);
            }

            if ctx.stop_on_error {
                anyhow::bail!("Mismatch at ledger {}", seq);
            }
        }

        // Collect and display performance metrics
        if let Some(ref perf) = result.perf {
            record_perf(stats, perf, seq);

            // Print per-ledger summary every 64 ledgers or if slow
            if !ctx.quiet && (stats.ledgers_verified % 64 == 0 || perf.total_us > 500_000) {
                let cache_rate = if perf.cache.hits + perf.cache.misses > 0 {
                    perf.cache.hits as f64 / (perf.cache.hits + perf.cache.misses) as f64 * 100.0
                } else {
                    0.0
                };
                println!(
                    "\n  [PERF L{}] total={:.1}ms tx_exec={:.1}ms commit={:.1}ms \
                 add_batch={:.1}ms eviction={:.1}ms txs={} cache={:.0}% \
                 rss={:.0}MB",
                    seq,
                    perf.total_us as f64 / 1000.0,
                    perf.tx_exec_us as f64 / 1000.0,
                    commit_us(perf) as f64 / 1000.0,
                    perf.add_batch_us as f64 / 1000.0,
                    perf.eviction_us as f64 / 1000.0,
                    perf.tx_count,
                    cache_rate,
                    perf.rss_after_bytes as f64 / (1024.0 * 1024.0),
                );
                // Show top 3 slowest txs for this ledger
                for tx in perf.tx_timings.iter().take(3) {
                    if tx.exec_us > 1000 {
                        println!(
                            "    tx[{}] {}..  {:.1}ms  ops={}  {}  {}",
                            tx.index,
                            &tx.hash_hex[..tx.hash_hex.len().min(12)],
                            tx.exec_us as f64 / 1000.0,
                            tx.op_count,
                            if tx.is_soroban { "soroban" } else { "classic" },
                            if tx.success { "ok" } else { "FAILED" },
                        );
                    }
                }
            }
        }
    }

    // Update prev hash for next ledger
    *prev_ledger_hash = result.header_hash;
    Ok(())
}

/// Phase 6: Print verification and performance summaries.
fn print_summary(stats: &mut VerifyStats, elapsed: Duration) {
    // Print summary
    println!();
    println!();
    println!("Verification Summary");
    println!("====================");
    println!("  Ledgers verified: {}", stats.ledgers_verified);
    println!("  Ledgers matched:  {}", stats.ledgers_matched);
    println!("  Ledgers with mismatches: {}", stats.ledgers_mismatched);
    if stats.ledgers_mismatched > 0 {
        println!("    - Header hash mismatches: {}", stats.header_mismatches);
        println!(
            "    - TX result hash mismatches: {}",
            stats.tx_result_mismatches
        );
        println!("    - Meta mismatches: {}", stats.meta_mismatches);
    }
    println!();
    println!("  Total time: {:.2}s", elapsed.as_secs_f64());
    if stats.ledgers_verified > 0 {
        println!(
            "  Average per ledger: {:.2}ms",
            elapsed.as_millis() as f64 / stats.ledgers_verified as f64
        );
    }

    // Performance summary
    println!();
    println!("Performance Summary");
    println!("====================");
    if stats.ledgers_verified > 0 {
        let avg_close_ms = stats.total_close_us as f64 / stats.ledgers_verified as f64 / 1000.0;
        let avg_tx_exec_ms = stats.total_tx_exec_us as f64 / stats.ledgers_verified as f64 / 1000.0;
        let avg_commit_ms = stats.total_commit_us as f64 / stats.ledgers_verified as f64 / 1000.0;
        println!("  Timing (averages per ledger):");
        println!("    close_ledger:  {:.2}ms", avg_close_ms);
        println!("    tx_exec:       {:.2}ms", avg_tx_exec_ms);
        println!("    commit:        {:.2}ms", avg_commit_ms);
        println!(
            "    add_batch:     {:.2}ms",
            stats.total_add_batch_us as f64 / stats.ledgers_verified as f64 / 1000.0
        );
        println!(
            "    eviction:      {:.2}ms",
            stats.total_eviction_us as f64 / stats.ledgers_verified as f64 / 1000.0
        );
        println!();
        println!("  Transactions:");
        println!("    total:         {}", stats.total_tx_count);
        println!(
            "    avg/ledger:    {:.1}",
            stats.total_tx_count as f64 / stats.ledgers_verified as f64
        );
        println!();
        println!("  Cache:");
        let overall_cache_rate = if stats.total_cache_hits + stats.total_cache_misses > 0 {
            stats.total_cache_hits as f64
                / (stats.total_cache_hits + stats.total_cache_misses) as f64
                * 100.0
        } else {
            0.0
        };
        println!("    hit rate:      {:.1}%", overall_cache_rate);
        println!("    total hits:    {}", stats.total_cache_hits);
        println!("    total misses:  {}", stats.total_cache_misses);
        println!();
        println!("  Memory:");
        println!(
            "    peak RSS:      {:.1}MB",
            stats.peak_rss_bytes as f64 / (1024.0 * 1024.0)
        );
        println!();
        println!(
            "  Slowest ledger:  {} ({:.1}ms)",
            stats.slowest_ledger_seq,
            stats.slowest_ledger_us as f64 / 1000.0
        );

        // Top 10 slowest transactions overall
        stats.slowest_txs.sort_by_key(|a| std::cmp::Reverse(a.2));
        println!();
        println!("  Top 10 slowest transactions:");
        for (i, (ledger, hash, us)) in stats.slowest_txs.iter().take(10).enumerate() {
            println!(
                "    {}. L{} {}..  {:.1}ms",
                i + 1,
                ledger,
                &hash[..hash.len().min(16)],
                *us as f64 / 1000.0
            );
        }
    }
}

/// Print detailed eviction and entry-level diagnostics for a mismatched ledger.
///
/// Called when the header hash mismatches but TX results match, indicating
/// the divergence is likely in bucket list state (eviction, upgrades, etc.).
fn print_eviction_and_entry_diagnostics(
    ctx: &VerifyContext,
    lcm: &stellar_xdr::curr::LedgerCloseMeta,
    result_header: &stellar_xdr::curr::LedgerHeader,
) {
    let cdp_evicted_keys = henyey_history::cdp::extract_evicted_keys(&lcm);
    let tx_metas = henyey_history::cdp::extract_transaction_metas(&lcm);
    let cdp_restored_keys = henyey_history::cdp::extract_restored_keys(&tx_metas);

    // Count CDP entry changes
    let mut cdp_creates = 0u32;
    let mut cdp_updates = 0u32;
    let mut cdp_deletes = 0u32;
    for tx_meta in &tx_metas {
        fn count_changes(
            changes: &[stellar_xdr::curr::LedgerEntryChange],
            creates: &mut u32,
            updates: &mut u32,
            deletes: &mut u32,
        ) {
            for change in changes {
                match change {
                    stellar_xdr::curr::LedgerEntryChange::Created(_) => *creates += 1,
                    stellar_xdr::curr::LedgerEntryChange::Updated(_) => *updates += 1,
                    stellar_xdr::curr::LedgerEntryChange::Removed(_) => *deletes += 1,
                    stellar_xdr::curr::LedgerEntryChange::Restored(_) => *updates += 1,
                    stellar_xdr::curr::LedgerEntryChange::State(_) => {}
                }
            }
        }
        henyey_common::meta_walk::for_each_change_group(tx_meta, |changes| {
            count_changes(
                changes,
                &mut cdp_creates,
                &mut cdp_updates,
                &mut cdp_deletes,
            );
        });
    }

    // Extract upgrade meta entry counts
    let cdp_upgrade_metas = henyey_history::cdp::extract_upgrade_metas(&lcm);
    let mut upgrade_creates = 0u32;
    let mut upgrade_updates = 0u32;
    for um in &cdp_upgrade_metas {
        for change in um.changes.iter() {
            match change {
                stellar_xdr::curr::LedgerEntryChange::Created(_) => upgrade_creates += 1,
                stellar_xdr::curr::LedgerEntryChange::Updated(_) => upgrade_updates += 1,
                _ => {}
            }
        }
    }

    println!("    CDP meta: creates={}, updates={}, deletes={}, evicted={}, restored={}, upgrade_creates={}, upgrade_updates={}",
    cdp_creates, cdp_updates, cdp_deletes, cdp_evicted_keys.len(), cdp_restored_keys.len(),
    upgrade_creates, upgrade_updates);

    // Dump expected upgrade entries from CDP meta for comparison
    if !cdp_upgrade_metas.is_empty() {
        use sha2::{Digest, Sha256};
        println!("    CDP upgrade entries (expected):");
        for (ui, um) in cdp_upgrade_metas.iter().enumerate() {
            for change in um.changes.iter() {
                match change {
                    stellar_xdr::curr::LedgerEntryChange::Updated(entry) => {
                        let key_str = match &entry.data {
                            stellar_xdr::curr::LedgerEntryData::ConfigSetting(cs) => {
                                format!("ConfigSetting({:?})", cs.discriminant())
                            }
                            other => format!("{:?}", std::mem::discriminant(other)),
                        };
                        let xdr_bytes = entry
                            .to_xdr(stellar_xdr::curr::Limits::none())
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
                    stellar_xdr::curr::LedgerEntryChange::Created(entry) => {
                        let key_str = match &entry.data {
                            stellar_xdr::curr::LedgerEntryData::ConfigSetting(cs) => {
                                format!("ConfigSetting({:?})", cs.discriminant())
                            }
                            other => format!("{:?}", std::mem::discriminant(other)),
                        };
                        let xdr_bytes = entry
                            .to_xdr(stellar_xdr::curr::Limits::none())
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
                    stellar_xdr::curr::LedgerEntryChange::State(entry) => {
                        let key_str = match &entry.data {
                            stellar_xdr::curr::LedgerEntryData::ConfigSetting(cs) => {
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

    // Also dump expected final TX entries from CDP meta
    {
        use sha2::{Digest, Sha256};
        // Coalesce: keep last Updated entry per key
        // Include ALL change sources: fee_processing, tx_apply_processing, and post_tx_apply_fee_processing
        let mut final_entries: std::collections::HashMap<Vec<u8>, stellar_xdr::curr::LedgerEntry> =
            std::collections::HashMap::new();
        // Helper to process a slice of changes into the coalesced map
        let coalesce_changes = |changes: &[stellar_xdr::curr::LedgerEntryChange],
                                map: &mut std::collections::HashMap<
            Vec<u8>,
            stellar_xdr::curr::LedgerEntry,
        >| {
            for change in changes {
                match change {
                    stellar_xdr::curr::LedgerEntryChange::Updated(entry)
                    | stellar_xdr::curr::LedgerEntryChange::Created(entry)
                    | stellar_xdr::curr::LedgerEntryChange::Restored(entry) => {
                        let key = henyey_common::entry_to_key(entry);
                        if let Ok(kb) = key.to_xdr(stellar_xdr::curr::Limits::none()) {
                            map.insert(kb, entry.clone());
                        }
                    }
                    stellar_xdr::curr::LedgerEntryChange::Removed(key) => {
                        if let Ok(kb) = key.to_xdr(stellar_xdr::curr::Limits::none()) {
                            map.remove(&kb);
                        }
                    }
                    _ => {}
                }
            }
        };
        let coalesce_tx_meta = |meta: &stellar_xdr::curr::TransactionMeta,
                                map: &mut std::collections::HashMap<
            Vec<u8>,
            stellar_xdr::curr::LedgerEntry,
        >| {
            henyey_common::meta_walk::for_each_change_group(meta, |changes| {
                coalesce_changes(changes, map);
            });
        };
        // Process ALL change sources from LCM tx_processing
        match &lcm {
            stellar_xdr::curr::LedgerCloseMeta::V0(v0) => {
                for tp in v0.tx_processing.iter() {
                    coalesce_changes(&tp.fee_processing, &mut final_entries);
                    coalesce_tx_meta(&tp.tx_apply_processing, &mut final_entries);
                    // V0 TransactionResultMeta has no post_tx_apply_fee_processing
                }
            }
            stellar_xdr::curr::LedgerCloseMeta::V1(v1) => {
                for tp in v1.tx_processing.iter() {
                    coalesce_changes(&tp.fee_processing, &mut final_entries);
                    coalesce_tx_meta(&tp.tx_apply_processing, &mut final_entries);
                    // V1 TransactionResultMeta has no post_tx_apply_fee_processing
                }
            }
            stellar_xdr::curr::LedgerCloseMeta::V2(v2) => {
                for tp in v2.tx_processing.iter() {
                    coalesce_changes(&tp.fee_processing, &mut final_entries);
                    coalesce_tx_meta(&tp.tx_apply_processing, &mut final_entries);
                    coalesce_changes(&tp.post_tx_apply_fee_processing, &mut final_entries);
                }
            }
        }
        println!(
            "    CDP TX final entries (coalesced, {} unique keys)",
            final_entries.len()
        );

        // Compare CDP entries with our bucket list state
        let bl = ctx.ledger_manager.bucket_list();
        let bl_snapshot = henyey_bucket::BucketListSnapshot::new(&bl, result_header.clone());
        drop(bl);
        let mut diffs = 0;
        let mut missing = 0;
        for (key_bytes, cdp_entry) in final_entries.iter() {
            use stellar_xdr::curr::ReadXdr;
            if let Ok(key) = stellar_xdr::curr::LedgerKey::from_xdr(
                key_bytes.as_slice(),
                stellar_xdr::curr::Limits::none(),
            ) {
                let cdp_xdr = cdp_entry
                    .to_xdr(stellar_xdr::curr::Limits::none())
                    .unwrap_or_default();
                let cdp_hash = {
                    let mut h = Sha256::new();
                    h.update(&cdp_xdr);
                    format!("{:x}", h.finalize())
                };
                match bl_snapshot.get(&key) {
                    Some(our_entry) => {
                        let our_xdr = our_entry
                            .to_xdr(stellar_xdr::curr::Limits::none())
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
                                stellar_xdr::curr::LedgerEntryData::Offer(cdp_o),
                                stellar_xdr::curr::LedgerEntryData::Offer(our_o),
                            ) = (&cdp_entry.data, &our_entry.data)
                            {
                                println!(
                                    "      CDP  offer: seller={:?} amount={} price={}/{}",
                                    hex::encode(
                                        &{
                                            let stellar_xdr::curr::PublicKey::PublicKeyTypeEd25519(
                                                ref pk,
                                            ) = cdp_o.seller_id.0;
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
                                            let stellar_xdr::curr::PublicKey::PublicKeyTypeEd25519(
                                                ref pk,
                                            ) = our_o.seller_id.0;
                                            pk.0
                                        }[..8]
                                    ),
                                    our_o.amount,
                                    our_o.price.n,
                                    our_o.price.d
                                );
                            }
                            if let (
                                stellar_xdr::curr::LedgerEntryData::Account(cdp_a),
                                stellar_xdr::curr::LedgerEntryData::Account(our_a),
                            ) = (&cdp_entry.data, &our_entry.data)
                            {
                                let cdp_pk = {
                                    let stellar_xdr::curr::PublicKey::PublicKeyTypeEd25519(ref pk) =
                                        cdp_a.account_id.0;
                                    hex::encode(&pk.0[..16])
                                };
                                // Extract sponsorship counts from extensions
                                let get_ext =
                                    |a: &stellar_xdr::curr::AccountEntry| -> (u32, u32, u32) {
                                        match &a.ext {
                                    stellar_xdr::curr::AccountEntryExt::V0 => (0, 0, 0),
                                    stellar_xdr::curr::AccountEntryExt::V1(v1) => match &v1.ext {
                                        stellar_xdr::curr::AccountEntryExtensionV1Ext::V0 => (0, 0, 0),
                                        stellar_xdr::curr::AccountEntryExtensionV1Ext::V2(v2) => (v2.num_sponsoring, v2.num_sponsored, v2.signer_sponsoring_i_ds.len() as u32),
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
                            if let (
                                stellar_xdr::curr::LedgerEntryData::Trustline(cdp_t),
                                stellar_xdr::curr::LedgerEntryData::Trustline(our_t),
                            ) = (&cdp_entry.data, &our_entry.data)
                            {
                                println!(
                                    "      CDP  trustline: balance={} asset={:?}",
                                    cdp_t.balance, cdp_t.asset
                                );
                                println!(
                                    "      Ours trustline: balance={} asset={:?}",
                                    our_t.balance, our_t.asset
                                );
                            }
                            if let (
                                stellar_xdr::curr::LedgerEntryData::LiquidityPool(cdp_p),
                                stellar_xdr::curr::LedgerEntryData::LiquidityPool(our_p),
                            ) = (&cdp_entry.data, &our_entry.data)
                            {
                                let stellar_xdr::curr::LiquidityPoolEntryBody::LiquidityPoolConstantProduct(ref cdp_cp) = cdp_p.body;
                                let stellar_xdr::curr::LiquidityPoolEntryBody::LiquidityPoolConstantProduct(ref our_cp) = our_p.body;
                                println!(
                                    "      CDP  pool: reserve_a={} reserve_b={}",
                                    cdp_cp.reserve_a, cdp_cp.reserve_b
                                );
                                println!(
                                    "      Ours pool: reserve_a={} reserve_b={}",
                                    our_cp.reserve_a, our_cp.reserve_b
                                );
                            }
                            if diffs >= 20 {
                                break;
                            }
                        }
                    }
                    None => {
                        // For offers, try the offer_store instead of bucket list snapshot
                        // (offers are not indexed in bucket list snapshot)
                        if let stellar_xdr::curr::LedgerEntryData::Offer(ref cdp_offer) =
                            cdp_entry.data
                        {
                            let offer_store = ctx.ledger_manager.offer_store_lock();
                            if let Some(our_entry) = offer_store
                                .get_ledger_entry_by_id(cdp_offer.offer_id)
                                .as_ref()
                            {
                                let our_xdr = our_entry
                                    .to_xdr(stellar_xdr::curr::Limits::none())
                                    .unwrap_or_default();
                                let our_hash = {
                                    let mut h = Sha256::new();
                                    h.update(&our_xdr);
                                    format!("{:x}", h.finalize())
                                };
                                if our_hash != cdp_hash {
                                    diffs += 1;
                                    if let stellar_xdr::curr::LedgerEntryData::Offer(
                                        ref our_offer,
                                    ) = our_entry.data
                                    {
                                        let cdp_seller = {
                                            let stellar_xdr::curr::PublicKey::PublicKeyTypeEd25519(
                                                ref pk,
                                            ) = cdp_offer.seller_id.0;
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
                                    let stellar_xdr::curr::PublicKey::PublicKeyTypeEd25519(ref pk) =
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
                            let key_str = match &cdp_entry.data {
                                stellar_xdr::curr::LedgerEntryData::Account(a) => {
                                    let stellar_xdr::curr::PublicKey::PublicKeyTypeEd25519(ref pk) =
                                        a.account_id.0;
                                    format!("Account({})", hex::encode(&pk.0[..8]))
                                }
                                stellar_xdr::curr::LedgerEntryData::Trustline(t) => {
                                    let stellar_xdr::curr::PublicKey::PublicKeyTypeEd25519(ref pk) =
                                        t.account_id.0;
                                    format!(
                                        "Trustline(acct={}, asset={:?}, balance={})",
                                        hex::encode(&pk.0[..8]),
                                        t.asset,
                                        t.balance
                                    )
                                }
                                stellar_xdr::curr::LedgerEntryData::LiquidityPool(p) => {
                                    let stellar_xdr::curr::LiquidityPoolEntryBody::LiquidityPoolConstantProduct(ref cp) = p.body;
                                    format!("Pool(ra={}, rb={})", cp.reserve_a, cp.reserve_b)
                                }
                                other => format!("{:?}", std::mem::discriminant(other)),
                            };
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
}

#[cfg(test)]
mod tests {
    use super::*;
    use henyey_ledger::{CachePerfStats, LedgerClosePerf, TxPerf};

    // ---- classify_ledger -------------------------------------------------

    #[test]
    fn test_classify_ledger_out_of_range_skips() {
        // init_checkpoint = 100, range = [120, 200], end = 200.
        // seq <= init_checkpoint -> skip, NO prev-hash update.
        assert_eq!(
            classify_ledger(50, 100, 120, 200),
            LedgerDisposition::OutOfRange {
                update_prev_hash: false
            }
        );
        assert_eq!(
            classify_ledger(100, 100, 120, 200),
            LedgerDisposition::OutOfRange {
                update_prev_hash: false
            }
        );
        // init_checkpoint < seq <= end but... only counts as skip when seq > end.
        // seq > end_ledger -> skip, prev-hash update (because seq > init_checkpoint).
        assert_eq!(
            classify_ledger(201, 100, 120, 200),
            LedgerDisposition::OutOfRange {
                update_prev_hash: true
            }
        );
        // The subtle case: seq in (init_checkpoint, start_ledger) i.e. above the
        // checkpoint but below the test window is NOT out-of-range; it is in-range
        // below-test (it falls through to the in_test_range check).
        assert_eq!(
            classify_ledger(110, 100, 120, 200),
            LedgerDisposition::InRangeBelowTest
        );
    }

    #[test]
    fn test_classify_ledger_in_test_range_boundaries() {
        // range = [120, 200], init_checkpoint = 100.
        assert_eq!(
            classify_ledger(120, 100, 120, 200),
            LedgerDisposition::InTestRange
        );
        assert_eq!(
            classify_ledger(200, 100, 120, 200),
            LedgerDisposition::InTestRange
        );
        assert_eq!(
            classify_ledger(160, 100, 120, 200),
            LedgerDisposition::InTestRange
        );
        // start_ledger - 1 is in-range (> init_checkpoint, <= end) but below test.
        assert_eq!(
            classify_ledger(119, 100, 120, 200),
            LedgerDisposition::InRangeBelowTest
        );
    }

    // ---- epoch_matches ---------------------------------------------------

    #[test]
    fn test_epoch_matches() {
        assert!(epoch_matches(1_700_000_000, 1_700_000_000));
        assert!(!epoch_matches(1_700_000_000, 1_700_000_001));
        assert!(epoch_matches(0, 0));
    }

    // ---- compare_results -------------------------------------------------

    fn h(byte: u8) -> Hash256 {
        Hash256::from_bytes([byte; 32])
    }

    #[test]
    fn test_compare_results_all_match() {
        // Equal header + equal tx_result + meta_is_none -> all four true.
        let v = compare_results(h(1), h(1), h(2), h(2), true);
        assert_eq!(
            v,
            MatchVerdict {
                header_matches: true,
                tx_result_matches: true,
                meta_matches: true,
                all_match: true,
            }
        );
    }

    #[test]
    fn test_compare_results_meta_matches_follows_tx_result() {
        // meta present (is_none = false): meta_matches == tx_result_matches.

        // Everything matches with meta present.
        let v = compare_results(h(1), h(1), h(2), h(2), false);
        assert_eq!(
            v,
            MatchVerdict {
                header_matches: true,
                tx_result_matches: true,
                meta_matches: true,
                all_match: true,
            }
        );

        // Header differs, tx_result matches: meta still matches (follows tx_result).
        let v = compare_results(h(1), h(9), h(2), h(2), false);
        assert_eq!(
            v,
            MatchVerdict {
                header_matches: false,
                tx_result_matches: true,
                meta_matches: true,
                all_match: false,
            }
        );

        // tx_result differs: meta_matches is false (follows tx_result).
        let v = compare_results(h(1), h(1), h(2), h(9), false);
        assert_eq!(
            v,
            MatchVerdict {
                header_matches: true,
                tx_result_matches: false,
                meta_matches: false,
                all_match: false,
            }
        );

        // With meta_is_none = true, tx_result mismatch still leaves meta_matches true.
        let v = compare_results(h(1), h(1), h(2), h(9), true);
        assert_eq!(
            v,
            MatchVerdict {
                header_matches: true,
                tx_result_matches: false,
                meta_matches: true,
                all_match: false,
            }
        );
    }

    // ---- record_match_counters ------------------------------------------

    #[test]
    fn test_record_match_counters_match() {
        let mut stats = VerifyStats::default();
        let v = MatchVerdict {
            header_matches: true,
            tx_result_matches: true,
            meta_matches: true,
            all_match: true,
        };
        record_match_counters(&mut stats, &v);
        assert_eq!(stats.ledgers_matched, 1);
        assert_eq!(stats.ledgers_mismatched, 0);
        assert_eq!(stats.header_mismatches, 0);
        assert_eq!(stats.tx_result_mismatches, 0);
        assert_eq!(stats.meta_mismatches, 0);
    }

    #[test]
    fn test_record_match_counters_mismatch() {
        // Header-only mismatch (tx_result matches, so meta matches too).
        let mut stats = VerifyStats::default();
        record_match_counters(
            &mut stats,
            &MatchVerdict {
                header_matches: false,
                tx_result_matches: true,
                meta_matches: true,
                all_match: false,
            },
        );
        assert_eq!(stats.ledgers_matched, 0);
        assert_eq!(stats.ledgers_mismatched, 1);
        assert_eq!(stats.header_mismatches, 1);
        assert_eq!(stats.tx_result_mismatches, 0);
        assert_eq!(stats.meta_mismatches, 0);

        // tx-only mismatch (meta follows tx_result -> also counts).
        let mut stats = VerifyStats::default();
        record_match_counters(
            &mut stats,
            &MatchVerdict {
                header_matches: true,
                tx_result_matches: false,
                meta_matches: false,
                all_match: false,
            },
        );
        assert_eq!(stats.ledgers_mismatched, 1);
        assert_eq!(stats.header_mismatches, 0);
        assert_eq!(stats.tx_result_mismatches, 1);
        assert_eq!(stats.meta_mismatches, 1);

        // Both header and tx mismatch.
        let mut stats = VerifyStats::default();
        record_match_counters(
            &mut stats,
            &MatchVerdict {
                header_matches: false,
                tx_result_matches: false,
                meta_matches: false,
                all_match: false,
            },
        );
        assert_eq!(stats.ledgers_mismatched, 1);
        assert_eq!(stats.header_mismatches, 1);
        assert_eq!(stats.tx_result_mismatches, 1);
        assert_eq!(stats.meta_mismatches, 1);
    }

    // ---- record_perf -----------------------------------------------------

    fn make_perf() -> LedgerClosePerf {
        let mut perf = LedgerClosePerf::default();
        perf.total_us = 700_000;
        perf.tx_exec_us = 250_000;
        perf.commit_setup_us = 10;
        perf.add_batch_us = 20;
        perf.hot_archive_us = 30;
        perf.header_us = 40;
        perf.commit_close_us = 50;
        perf.eviction_us = 5_000;
        perf.tx_count = 7;
        perf.cache = CachePerfStats {
            hits: 11,
            misses: 3,
            ..Default::default()
        };
        perf.rss_after_bytes = 4_000;
        perf.tx_timings = vec![
            TxPerf {
                index: 0,
                hash_hex: "aabbccdd".to_string(),
                success: true,
                op_count: 1,
                exec_us: 1_500,
                is_soroban: false,
            },
            TxPerf {
                index: 1,
                hash_hex: "11223344".to_string(),
                success: false,
                op_count: 2,
                exec_us: 900,
                is_soroban: true,
            },
        ];
        perf
    }

    #[test]
    fn test_record_perf_accumulates() {
        let perf = make_perf();
        let mut stats = VerifyStats::default();
        // Seed peak_rss below perf to verify the max update.
        stats.peak_rss_bytes = 1_000;

        record_perf(&mut stats, &perf, 555);

        assert_eq!(stats.total_close_us, 700_000);
        assert_eq!(stats.total_tx_exec_us, 250_000);
        // commit_us = 10 + 20 + 30 + 40 + 50 = 150
        assert_eq!(stats.total_commit_us, 150);
        assert_eq!(commit_us(&perf), 150);
        assert_eq!(stats.total_add_batch_us, 20);
        assert_eq!(stats.total_eviction_us, 5_000);
        assert_eq!(stats.total_tx_count, 7);
        assert_eq!(stats.total_cache_hits, 11);
        assert_eq!(stats.total_cache_misses, 3);
        assert_eq!(stats.peak_rss_bytes, 4_000);
        assert_eq!(stats.slowest_ledger_us, 700_000);
        assert_eq!(stats.slowest_ledger_seq, 555);
        assert_eq!(
            stats.slowest_txs,
            vec![
                (555, "aabbccdd".to_string(), 1_500),
                (555, "11223344".to_string(), 900),
            ]
        );
    }

    #[test]
    fn test_record_perf_peak_rss_and_slowest_not_lowered() {
        let mut perf = make_perf();
        perf.total_us = 100; // below an already-recorded slowest
        perf.rss_after_bytes = 100; // below an already-recorded peak
        let mut stats = VerifyStats::default();
        stats.peak_rss_bytes = 9_999;
        stats.slowest_ledger_us = 9_999;
        stats.slowest_ledger_seq = 42;

        record_perf(&mut stats, &perf, 555);

        // Neither peak_rss nor slowest_ledger is lowered.
        assert_eq!(stats.peak_rss_bytes, 9_999);
        assert_eq!(stats.slowest_ledger_us, 9_999);
        assert_eq!(stats.slowest_ledger_seq, 42);
    }

    // ---- pinning sequence test ------------------------------------------

    /// Models the orchestrator's per-branch flow over a fixed table of synthetic
    /// ledgers, asserting the cumulative `VerifyStats` AND the running
    /// `prev_ledger_hash` match a hardcoded snapshot. This pins "same verdict +
    /// same counters + same chaining" so a flipped verdict/counter/bail/prev-hash
    /// transition fails the build.
    ///
    /// The prev-hash transition per branch mirrors the orchestrator exactly:
    ///   - OutOfRange{update_prev_hash:false} (seq <= init_checkpoint): NO update
    ///   - OutOfRange{update_prev_hash:true}  (seq > end_ledger):       verified_header_hash
    ///   - CDP fetch fail:                                              verified_header_hash
    ///   - epoch mismatch:                                              verified_header_hash
    ///   - close_ledger fail:                                           verified_header_hash
    ///   - in-test success/mismatch (close ok):                         result.header_hash
    #[test]
    fn test_verify_disposition_sequence_pins_stats() {
        // Range config: init_checkpoint = 100, test window [120, 200], end = 200.
        let init_checkpoint = 100u32;
        let start_ledger = 120u32;
        let end_ledger = 200u32;

        // Outcome at the close_ledger step for in-test ledgers (the orchestrator's
        // branches that come *after* classify/epoch).
        enum CloseOutcome {
            FetchFail,
            EpochMismatch,
            // close_ledger succeeded; carries (our_header, expected_header,
            // our_tx_result, expected_tx_result, meta_is_none).
            Closed {
                our_header: Hash256,
                expected_header: Hash256,
                our_tx_result: Hash256,
                expected_tx_result: Hash256,
                meta_is_none: bool,
            },
        }

        // (seq, verified_header_hash, outcome-for-in-test-or-fetch).
        // verified_header_hash is the archive-verified header hash for that seq.
        let table: Vec<(u32, Hash256, CloseOutcome)> = vec![
            // out-of-range-low: seq <= init_checkpoint -> skip, no prev-hash update.
            (
                50,
                h(0x50),
                CloseOutcome::FetchFail, // unused on skip path
            ),
            // in-test, CDP fetch fail -> prev-hash = verified_header_hash, no stats.
            (130, h(0x30), CloseOutcome::FetchFail),
            // in-test, epoch mismatch -> prev-hash = verified_header_hash, no stats.
            (140, h(0x40), CloseOutcome::EpochMismatch),
            // in-test, close ok, full match -> prev-hash = result.header_hash, matched++.
            (
                150,
                h(0x5A),
                CloseOutcome::Closed {
                    our_header: h(0xAA),
                    expected_header: h(0xAA),
                    our_tx_result: h(0xBB),
                    expected_tx_result: h(0xBB),
                    meta_is_none: false,
                },
            ),
            // in-test, close ok, mismatch (header+tx differ) -> prev = result.header_hash, mismatched++.
            (
                160,
                h(0x5B),
                CloseOutcome::Closed {
                    our_header: h(0xCC),
                    expected_header: h(0xDD),
                    our_tx_result: h(0xEE),
                    expected_tx_result: h(0xFF),
                    meta_is_none: false,
                },
            ),
            // out-of-range-high: seq > end_ledger -> skip, prev-hash = verified_header_hash.
            (
                201,
                h(0x99),
                CloseOutcome::FetchFail, // unused on skip path
            ),
        ];

        let mut stats = VerifyStats::default();
        let mut prev_ledger_hash = h(0x00);

        for (seq, verified_header_hash, outcome) in &table {
            let seq = *seq;
            let verified_header_hash = *verified_header_hash;

            match classify_ledger(seq, init_checkpoint, start_ledger, end_ledger) {
                LedgerDisposition::OutOfRange { update_prev_hash } => {
                    // Orchestrator: skip path. prev-hash advances only when seq > init_checkpoint.
                    if update_prev_hash {
                        prev_ledger_hash = verified_header_hash;
                    }
                    continue;
                }
                LedgerDisposition::InRangeBelowTest => {
                    // Not exercised in this table; would process but not count test stats.
                }
                LedgerDisposition::InTestRange => {}
            }

            // CDP fetch step.
            match outcome {
                CloseOutcome::FetchFail => {
                    // Orchestrator: fetch failed -> prev-hash = verified_header_hash, return.
                    prev_ledger_hash = verified_header_hash;
                    continue;
                }
                CloseOutcome::EpochMismatch => {
                    // Orchestrator: !epoch_matches -> prev-hash = verified_header_hash, return.
                    // (Model the epoch check with unequal close times.)
                    assert!(!epoch_matches(1, 2));
                    prev_ledger_hash = verified_header_hash;
                    continue;
                }
                CloseOutcome::Closed {
                    our_header,
                    expected_header,
                    our_tx_result,
                    expected_tx_result,
                    meta_is_none,
                } => {
                    // Epoch matched, close_ledger succeeded.
                    assert!(epoch_matches(7, 7));
                    stats.ledgers_verified += 1;
                    let verdict = compare_results(
                        *our_header,
                        *expected_header,
                        *our_tx_result,
                        *expected_tx_result,
                        *meta_is_none,
                    );
                    record_match_counters(&mut stats, &verdict);
                    // Orchestrator success path: prev-hash = result.header_hash (== our_header).
                    prev_ledger_hash = *our_header;
                }
            }
        }

        // ---- Hardcoded expected snapshot ----
        // Two in-test ledgers reached close_ledger: 150 (match) and 160 (mismatch).
        assert_eq!(stats.ledgers_verified, 2);
        assert_eq!(stats.ledgers_matched, 1);
        assert_eq!(stats.ledgers_mismatched, 1);
        // Ledger 160: header differs AND tx_result differs -> both category counters,
        // and meta_matches follows tx_result (false) -> meta_mismatches too.
        assert_eq!(stats.header_mismatches, 1);
        assert_eq!(stats.tx_result_mismatches, 1);
        assert_eq!(stats.meta_mismatches, 1);

        // Final prev_ledger_hash chaining:
        //   start 0x00
        //   seq 50  : skip, no update            -> 0x00
        //   seq 130 : fetch fail -> verified 0x30 -> 0x30
        //   seq 140 : epoch mismatch -> verified 0x40 -> 0x40
        //   seq 150 : close ok -> result.header 0xAA -> 0xAA
        //   seq 160 : close ok -> result.header 0xCC -> 0xCC
        //   seq 201 : skip (seq>end), update -> verified 0x99 -> 0x99
        assert_eq!(prev_ledger_hash, h(0x99));
    }
}
