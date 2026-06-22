//! Phase 5/6: the per-ledger verification loop, its pure helpers, the
//! `verify_single_ledger` orchestrator, and the run/eviction diagnostics.
//!
//! Moved verbatim from the former single-file module (#3357 split); no logic
//! change. The `hash_mismatch_debug` WARN blocks are byte-identical to the
//! pre-split source.

use std::time::{Duration, Instant};

use henyey_common::Hash256;
use henyey_history::cdp::{
    extract_ledger_close_data, extract_ledger_header, extract_transaction_results,
};
use henyey_history::checkpoint;
use henyey_history::verify;

use super::diffs::*;
use super::{VerifyContext, VerifyStats};

/// Phase 5: Main verification loop — iterate checkpoints and verify each ledger.
pub(super) async fn run_verification_loop(
    ctx: &mut VerifyContext,
) -> anyhow::Result<(VerifyStats, Duration)> {
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
    header_entry: &stellar_xdr::LedgerHeaderHistoryEntry,
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
            log_close_ledger_failure_diag(ctx, seq, verified_header_hash, &cdp_header, &lcm);
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
            print_mismatch_details(
                ctx,
                seq,
                &result.header,
                result.header_hash,
                expected_header_hash,
                our_tx_result_hash,
                expected_tx_result_hash,
                header_matches,
                tx_result_matches,
                &result.tx_results,
                &cdp_tx_results,
                &cdp_header,
                &lcm,
            );

            if ctx.stop_on_error {
                anyhow::bail!("Mismatch at ledger {}", seq);
            }
        }

        // Collect and display performance metrics
        if let Some(ref perf) = result.perf {
            record_perf(stats, perf, seq);
            print_perf_line(seq, perf, ctx.quiet, stats.ledgers_verified);
        }
    }

    // Update prev hash for next ledger
    *prev_ledger_hash = result.header_hash;
    Ok(())
}

/// Logs the CDP/mainnet expected header fields and per-tx CDP results after a
/// `close_ledger` failure, so they can be diffed against the WARN log emitted by
/// `manager.rs::commit` (which prints OUR computed fields) directly from CI output.
///
/// Print-only: extracted verbatim from `verify_single_ledger`'s close-failure arm.
/// All control flow (the `stop_on_error` bail) stays in the caller.
fn log_close_ledger_failure_diag(
    ctx: &VerifyContext,
    seq: u32,
    verified_header_hash: Hash256,
    cdp_header: &stellar_xdr::LedgerHeader,
    lcm: &stellar_xdr::LedgerCloseMeta,
) {
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
    let network_id = stellar_xdr::Hash(*ctx.ledger_manager.network_id().0.as_bytes());
    let cdp_processing = henyey_history::cdp::extract_transaction_processing(lcm, &network_id);
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
}

/// Prints the per-ledger MISMATCH diagnostics (header-field diffs, tx-result
/// hash diffs, and eviction/entry diagnostics).
///
/// Print-only: extracted verbatim from `verify_single_ledger`'s mismatch arm.
/// The `stop_on_error` bail stays in the caller.
#[allow(clippy::too_many_arguments)]
fn print_mismatch_details(
    ctx: &VerifyContext,
    seq: u32,
    our_header: &stellar_xdr::LedgerHeader,
    our_header_hash: Hash256,
    expected_header_hash: Hash256,
    our_tx_result_hash: Hash256,
    expected_tx_result_hash: Hash256,
    header_matches: bool,
    tx_result_matches: bool,
    our_tx_results: &[stellar_xdr::TransactionResultPair],
    cdp_tx_results: &[stellar_xdr::TransactionResultPair],
    cdp_header: &stellar_xdr::LedgerHeader,
    lcm: &stellar_xdr::LedgerCloseMeta,
) {
    println!();
    println!("  Ledger {}: MISMATCH", seq);
    if !header_matches {
        println!(
            "    Header hash: ours={} expected={}",
            our_header_hash.to_hex(),
            expected_header_hash.to_hex()
        );
        let bucket_levels = ctx.ledger_manager.bucket_list_levels();
        print_header_field_diffs(our_header, cdp_header, &bucket_levels);
    }
    if !tx_result_matches {
        println!(
            "    TX result hash: ours={} expected={}",
            our_tx_result_hash.to_hex(),
            expected_tx_result_hash.to_hex()
        );
    }

    if ctx.show_diff && !tx_result_matches {
        print_tx_result_diffs(our_tx_results, cdp_tx_results);
    }

    // Compare eviction data when header mismatches but TX results match
    if !header_matches && tx_result_matches {
        print_eviction_and_entry_diagnostics(ctx, lcm, our_header);
    }
}

/// Prints the per-ledger performance summary line (every 64 ledgers or when slow),
/// including cache-rate math and the top-3-slowest-tx tail.
///
/// Print-only: extracted verbatim from `verify_single_ledger`'s perf block. The
/// `record_perf` stats mutation stays in the caller.
fn print_perf_line(
    seq: u32,
    perf: &henyey_ledger::LedgerClosePerf,
    quiet: bool,
    ledgers_verified: u32,
) {
    // Print per-ledger summary every 64 ledgers or if slow
    if !quiet && (ledgers_verified % 64 == 0 || perf.total_us > 500_000) {
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

/// Phase 6: Print verification and performance summaries.
pub(super) fn print_summary(stats: &mut VerifyStats, elapsed: Duration) {
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
    lcm: &stellar_xdr::LedgerCloseMeta,
    result_header: &stellar_xdr::LedgerHeader,
) {
    let cdp_evicted_keys = henyey_history::cdp::extract_evicted_keys(lcm);
    let tx_metas = henyey_history::cdp::extract_transaction_metas(lcm);
    let cdp_restored_keys = henyey_history::cdp::extract_restored_keys(&tx_metas);

    // Count CDP entry changes (including upgrade-meta create/update counts).
    let cdp_upgrade_metas = henyey_history::cdp::extract_upgrade_metas(lcm);
    let (cdp_creates, cdp_updates, cdp_deletes, upgrade_creates, upgrade_updates) =
        cdp_change_counts(&tx_metas, &cdp_upgrade_metas);

    println!("    CDP meta: creates={}, updates={}, deletes={}, evicted={}, restored={}, upgrade_creates={}, upgrade_updates={}",
    cdp_creates, cdp_updates, cdp_deletes, cdp_evicted_keys.len(), cdp_restored_keys.len(),
    upgrade_creates, upgrade_updates);

    // Dump expected upgrade entries from CDP meta for comparison
    print_cdp_upgrade_entries(&cdp_upgrade_metas);

    // Also dump expected final TX entries from CDP meta
    let final_entries = coalesce_cdp_final_entries(&lcm);
    println!(
        "    CDP TX final entries (coalesced, {} unique keys)",
        final_entries.len()
    );

    // Compare CDP entries with our bucket list state
    print_cdp_entry_comparison(ctx, result_header, &final_entries);
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
