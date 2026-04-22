# Henyey Validator Dashboard Spec

## Overview

A single Grafana dashboard for monitoring the Henyey mainnet validator. Designed for both at-a-glance operational awareness (top rows always visible) and deep debugging (lower rows collapsed by default).

**Datasource:** Prometheus (uid `000000001`)
**Job filter:** `{job="henyey"}`
**Refresh:** 30s auto-refresh
**Default time range:** Last 6 hours

## Variables

| Variable | Type | Query / Values | Default |
|----------|------|---------------|---------|
| `instance` | query | `label_values(up{job="henyey"}, instance)` | `All` |

Since this is currently a single-node deployment, the instance variable exists for future multi-node support but will have one value today.

---

## Row 1: Health Summary

**Collapsed:** Never (always visible)
**Layout:** Single row of stat panels, equal width
**Purpose:** Immediate operational status at a glance. An operator should be able to tell if the validator is healthy in under 2 seconds.

### Panel 1.1: Up
- **Type:** stat
- **Query:** `up{job="henyey", instance=~"$instance"}`
- **Thresholds:** 1 = green, 0 = red
- **Value mapping:** 1 = "UP", 0 = "DOWN"
- **No-data state:** Red, display "NO DATA"

### Panel 1.2: Ledger Age
- **Type:** stat
- **Unit:** seconds (s)
- **Query:** `stellar_ledger_age_current_seconds{job="henyey", instance=~"$instance"}`
- **Thresholds:** <10 = green, <30 = yellow, >=30 = red
- **Description:** Seconds since the last ledger close. On a healthy mainnet validator this should hover around 5-7s. Sustained values above 30s indicate the node has fallen behind or lost sync.

### Panel 1.3: Ledger Seq
- **Type:** stat
- **Query:** `stellar_ledger_sequence{job="henyey", instance=~"$instance"}`
- **Unit:** none (plain integer)
- **Description:** Current ledger number. Useful for cross-referencing with network explorers and other validators.

### Panel 1.4: Herder
- **Type:** stat
- **Query:** `stellar_herder_state{job="henyey", instance=~"$instance"}`
- **Value mapping:** Numeric state codes to human-readable names if known; otherwise display raw value.
- **Description:** Internal herder state machine position. Indicates whether the node is tracking consensus, catching up, or in another state.

### Panel 1.5: Peers
- **Type:** stat
- **Query:** `stellar_peer_count{job="henyey", instance=~"$instance"}`
- **Thresholds:** <3 = red, <8 = yellow, >=8 = green
- **Description:** Total connected peers. Below 3 risks network partition; below 8 is degraded connectivity.

### Panel 1.6: Fail At
- **Type:** stat
- **Query:** `stellar_quorum_fail_at{job="henyey", instance=~"$instance"}`
- **Thresholds:** 0 = red, 1 = yellow, >=2 = green
- **Description:** Number of additional quorum nodes that can fail before this validator loses quorum. Zero means quorum loss is imminent. This is the single most critical safety metric.

### Panel 1.7: Missing
- **Type:** stat
- **Query:** `stellar_quorum_missing{job="henyey", instance=~"$instance"}`
- **Thresholds:** 0 = green, >=1 = red
- **Description:** Number of quorum peers not responding. Any non-zero value warrants investigation.

### Panel 1.8: Validator
- **Type:** stat
- **Query:** `stellar_is_validator{job="henyey", instance=~"$instance"}`
- **Value mapping:** 1 = "YES" (green), 0 = "NO" (red)
- **Description:** Whether this node is configured as a validator (participating in consensus, not just observing).

### Panel 1.9: Uptime
- **Type:** stat
- **Unit:** duration (auto, e.g. "3d 4h")
- **Query:** `stellar_uptime_seconds{job="henyey", instance=~"$instance"}`
- **Thresholds:** none
- **Description:** Time since the Henyey process started. Useful for spotting unexpected restarts.

### Panel 1.10: FD Usage
- **Type:** stat
- **Unit:** percentunit (0-1 range, displayed as percent)
- **Query:** `henyey_process_open_fds{job="henyey", instance=~"$instance"} / henyey_process_max_fds{job="henyey", instance=~"$instance"}`
- **Thresholds:** <0.6 = green, <0.8 = yellow, >=0.8 = red
- **Description:** File descriptor usage as percentage of ulimit. Approaching the limit causes connection failures and crashes.

---

## Row 2: Ledger Performance

**Collapsed:** No
**Purpose:** Detailed view of ledger close behavior, transaction throughput, and execution timing. This is where engineers spend most of their time during performance investigations.

### Panel 2.1: Ledger Age
- **Type:** timeseries
- **Query:** `stellar_ledger_age_current_seconds{job="henyey", instance=~"$instance"}`
- **Y-axis:** seconds
- **Description:** Time series view of ledger age. Spikes indicate slow closes or temporary desync. Complements the stat panel by showing patterns over time.

### Panel 2.2: Close Duration Percentiles
- **Type:** timeseries
- **Queries:**
  - `histogram_quantile(0.5, rate(stellar_ledger_close_duration_seconds_bucket{job="henyey", instance=~"$instance"}[$__rate_interval]))` — legend: "p50"
  - `histogram_quantile(0.99, rate(stellar_ledger_close_duration_seconds_bucket{job="henyey", instance=~"$instance"}[$__rate_interval]))` — legend: "p99"
  - `histogram_quantile(1.0, rate(stellar_ledger_close_duration_seconds_bucket{job="henyey", instance=~"$instance"}[$__rate_interval]))` — legend: "max"
- **Y-axis:** seconds
- **Description:** Distribution of ledger close durations. The gap between p50 and p99 reveals tail latency. If p99 is significantly higher than p50, some ledgers are taking much longer than average — investigate those time ranges via the heatmap.

### Panel 2.3: Close Duration Heatmap
- **Type:** heatmap
- **Query:** `sum(increase(stellar_ledger_close_duration_seconds_bucket{job="henyey", instance=~"$instance"}[$__rate_interval])) by (le)`
- **Y-axis:** seconds (bucket boundaries: 50ms, 100ms, 250ms, 500ms, 1s, 2s, 5s, 10s, 30s)
- **Color scheme:** green-to-red (low-to-high density)
- **Description:** Visual density of close durations over time. Highlights clusters and outliers that percentile lines can miss.

### Panel 2.4: Apply Time
- **Type:** timeseries
- **Query:** `stellar_ledger_apply_us{job="henyey", instance=~"$instance"} / 1000`
- **Y-axis:** milliseconds
- **Legend:** "apply time (ms)"
- **Description:** Total ledger apply time for the most recent close. This is the CPU-bound portion of the close (excludes consensus/network). Compare with close duration to see how much time is consensus vs execution.

### Panel 2.5: Classic vs Soroban Execution
- **Type:** timeseries (stacked area)
- **Queries:**
  - `henyey_ledger_classic_exec_us{job="henyey", instance=~"$instance"} / 1000` — legend: "Classic (ms)"
  - `henyey_ledger_soroban_exec_us{job="henyey", instance=~"$instance"} / 1000` — legend: "Soroban (ms)"
- **Y-axis:** milliseconds
- **Description:** Breakdown of ledger apply time between classic transaction execution and Soroban smart contract execution. Shows the relative cost and how it shifts over time. **Henyey-specific metric — not available in stellar-core.**

### Panel 2.6: Transaction & Operation Count
- **Type:** timeseries
- **Queries:**
  - `stellar_ledger_tx_count{job="henyey", instance=~"$instance"}` — legend: "Transactions"
  - `stellar_ledger_op_count{job="henyey", instance=~"$instance"}` — legend: "Operations"
- **Y-axis:** count
- **Dual Y-axis:** if op counts are significantly larger, use right axis for ops
- **Description:** Per-ledger transaction and operation counts. Correlate with apply time to understand throughput vs latency.

### Panel 2.7: TX Success vs Failure
- **Type:** timeseries (stacked area)
- **Queries:**
  - `stellar_ledger_tx_success_count{job="henyey", instance=~"$instance"}` — legend: "Success" (green)
  - `stellar_ledger_tx_failed_count{job="henyey", instance=~"$instance"}` — legend: "Failed" (red)
- **Y-axis:** count
- **Description:** Per-ledger success/failure breakdown. A sustained increase in failures may indicate network issues, fee market shifts, or application bugs.

### Panel 2.8: Ledger Entries Modified
- **Type:** timeseries (stacked area)
- **Queries:**
  - `stellar_ledger_entries_created{job="henyey", instance=~"$instance"}` — legend: "Created" (green)
  - `stellar_ledger_entries_updated{job="henyey", instance=~"$instance"}` — legend: "Updated" (blue)
  - `stellar_ledger_entries_deleted{job="henyey", instance=~"$instance"}` — legend: "Deleted" (orange)
- **Y-axis:** count
- **Description:** Ledger entry churn per close. High creation rates indicate growth; deletion spikes may correlate with TTL expiry or state archival.

### Panel 2.9: Bucket Cache Hit Ratio
- **Type:** timeseries
- **Query:** `henyey_bucket_cache_hit_ratio{job="henyey", instance=~"$instance"}`
- **Y-axis:** ratio (0-1), format as percent
- **Thresholds:** <0.5 = red line
- **Description:** Per-bucket RandomEvictionCache hit ratio (Account entries only). Low values are expected because the SnapshotHandle prefetch cache absorbs most lookups before they reach the bucket layer. **Henyey-specific metric.**

### Panel 2.9b: Snapshot Cache Hit Ratio
- **Type:** timeseries
- **Query:** `henyey_snapshot_cache_hit_ratio{job="henyey", instance=~"$instance"}`
- **Y-axis:** ratio (0-1), format as percent
- **Thresholds:** <0.5 = red line
- **Description:** Fraction of SnapshotHandle lookups served from local caches (snapshot + prefetch/read-through) without dispatching to the fallback lookup function. This is the primary cache effectiveness metric. **Henyey-specific metric.**

### Panel 2.9c: Snapshot Cache Fallback Lookups
- **Type:** timeseries
- **Query:** `henyey_snapshot_cache_fallback_lookups{job="henyey", instance=~"$instance"}`
- **Y-axis:** count
- **Description:** Number of SnapshotHandle lookups per ledger that were not served by local caches and had to be dispatched to the fallback (bucket list / Soroban state). **Henyey-specific metric.**

### Panel 2.10: Total Fees
- **Type:** timeseries
- **Query:** `stellar_ledger_total_fees{job="henyey", instance=~"$instance"}`
- **Y-axis:** stroops
- **Description:** Fees collected per ledger close. Proxy for network demand and fee market pressure.

---

## Row 3: Quorum & Consensus

**Collapsed:** No
**Purpose:** Quorum health and consensus timing. This section answers: "Is this validator participating in consensus correctly, and how fast?"

### Panel 3.1: Quorum Agree / Disagree
- **Type:** timeseries
- **Queries:**
  - `stellar_quorum_agree{job="henyey", instance=~"$instance"}` — legend: "Agree" (green)
  - `stellar_quorum_disagree{job="henyey", instance=~"$instance"}` — legend: "Disagree" (red)
- **Y-axis:** count
- **Description:** Number of quorum peers agreeing vs disagreeing with this validator. Any non-zero disagree warrants immediate investigation — it means validators in the quorum are seeing different ledger states.

### Panel 3.2: Quorum Fail At
- **Type:** timeseries
- **Query:** `stellar_quorum_fail_at{job="henyey", instance=~"$instance"}`
- **Y-axis:** count
- **Threshold line:** y=1 (dashed red) — below this line, one more failure causes quorum loss
- **Description:** Quorum failure tolerance over time. Drops toward zero are the highest-severity operational event for a validator.

### Panel 3.3: Quorum Missing
- **Type:** timeseries
- **Query:** `stellar_quorum_missing{job="henyey", instance=~"$instance"}`
- **Y-axis:** count
- **Description:** Missing quorum peers over time. Correlate with fail_at to understand safety margin.

### Panel 3.4: SCP Externalize Time
- **Type:** timeseries
- **Query:** `stellar_scp_timing_externalized_seconds{job="henyey", instance=~"$instance"}`
- **Y-axis:** seconds
- **Description:** Time taken to externalize (finalize) each consensus slot. This is the end-to-end consensus latency. Mainnet target is ~5s (one ledger interval). Values significantly above 5s mean this node is slow to reach agreement.

### Panel 3.5: SCP Nominate Time
- **Type:** timeseries
- **Query:** `stellar_scp_timing_nominated_seconds{job="henyey", instance=~"$instance"}`
- **Y-axis:** seconds
- **Description:** Time spent in the nomination phase of SCP. The gap between nomination and externalization is the ballot (voting) phase. If nomination is slow, the node may be slow to propose values. If externalization is slow but nomination is fast, the voting phase is the bottleneck.

### Panel 3.6: Externalize vs Nominate (overlay)
- **Type:** timeseries
- **Queries:**
  - `stellar_scp_timing_externalized_seconds{job="henyey", instance=~"$instance"}` — legend: "Externalize"
  - `stellar_scp_timing_nominated_seconds{job="henyey", instance=~"$instance"}` — legend: "Nominate"
- **Y-axis:** seconds
- **Description:** Both timings overlaid to visualize the nomination-to-externalization gap. The area between the lines represents ballot protocol duration.

### Panel 3.7: SCP Envelope Rates
- **Type:** timeseries
- **Queries:**
  - `rate(stellar_scp_envelope_emit_total{job="henyey", instance=~"$instance"}[$__rate_interval])` — legend: "Emit/s"
  - `rate(stellar_scp_envelope_receive_total{job="henyey", instance=~"$instance"}[$__rate_interval])` — legend: "Receive/s"
- **Y-axis:** envelopes/sec
- **Description:** Rate of SCP message emission and reception. A drop in receive rate may indicate network issues; a drop in emit rate may indicate this node is not participating actively.

### Panel 3.8: Herder Lost Sync
- **Type:** timeseries
- **Query:** `rate(stellar_herder_lost_sync_total{job="henyey", instance=~"$instance"}[$__rate_interval])`
- **Y-axis:** events/sec
- **Description:** Rate of sync loss events. Each event means the herder fell out of consensus and had to recover. Should be zero under normal operation.

---

## Row 4: SCP Verify Pipeline

**Collapsed:** Yes (default)
**Purpose:** Henyey-specific signature verification pipeline monitoring. Answers: "Is the SCP message verification keeping up with inbound traffic?"

### Panel 4.1: Verify Latency (average)
- **Type:** timeseries
- **Query:** `henyey_scp_verify_latency_us_sum{job="henyey", instance=~"$instance"} / henyey_scp_verify_latency_us_count{job="henyey", instance=~"$instance"}`
- **Y-axis:** microseconds
- **Description:** Average signature verification latency. Increases here indicate CPU pressure on the verify threads.

### Panel 4.2: Verify Backlogs
- **Type:** timeseries
- **Queries:**
  - `henyey_scp_verify_input_backlog{job="henyey", instance=~"$instance"}` — legend: "Input backlog"
  - `henyey_scp_verify_output_backlog{job="henyey", instance=~"$instance"}` — legend: "Output backlog"
- **Y-axis:** count
- **Description:** Queue depths before and after verification. Growing input backlog means messages arrive faster than they can be verified. Growing output backlog means verified messages aren't being consumed fast enough.

### Panel 4.3: Post-Verify Throughput & Drops
- **Type:** timeseries
- **Queries:**
  - `rate(henyey_scp_post_verify_total{job="henyey", instance=~"$instance"}[$__rate_interval])` — legend: "Verified/s" (green)
  - `rate(henyey_scp_post_verify_drops_total{job="henyey", instance=~"$instance"}[$__rate_interval])` — legend: "Dropped/s" (red)
- **Y-axis:** messages/sec
- **Description:** Throughput of the verify pipeline vs messages dropped post-verification. Drops indicate the pipeline is overwhelmed.

### Panel 4.4: Prefilter Reject Rate
- **Type:** timeseries
- **Query:** `rate(henyey_scp_prefilter_rejects_total{job="henyey", instance=~"$instance"}[$__rate_interval])`
- **Y-axis:** messages/sec
- **Description:** Rate of SCP messages rejected before verification (e.g., duplicate, wrong slot). High rates are normal under load but sustained spikes may indicate a misbehaving peer.

### Panel 4.5: Verifier Thread State
- **Type:** timeseries
- **Query:** `henyey_scp_verifier_thread_state{job="henyey", instance=~"$instance"}`
- **Y-axis:** state (integer)
- **Description:** Current state of the verifier thread pool. Interpretation depends on Henyey's internal state encoding.

---

## Row 5: Herder & TX Queue

**Collapsed:** Yes (default)
**Purpose:** Transaction lifecycle from submission through to consensus nomination.

### Panel 5.1: Herder Drift (percentiles)
- **Type:** timeseries
- **Queries:**
  - `henyey_herder_drift_min_seconds{job="henyey", instance=~"$instance"}` — legend: "min"
  - `henyey_herder_drift_median_seconds{job="henyey", instance=~"$instance"}` — legend: "median"
  - `henyey_herder_drift_p75_seconds{job="henyey", instance=~"$instance"}` — legend: "p75"
  - `henyey_herder_drift_max_seconds{job="henyey", instance=~"$instance"}` — legend: "max"
- **Y-axis:** seconds
- **Fill:** between min and max (band visualization)
- **Description:** Clock/consensus drift across percentiles relative to peers. Growing drift indicates this node is falling behind or ahead of the network. **Henyey-specific metric.**

### Panel 5.2: Pending Envelopes
- **Type:** timeseries
- **Query:** `stellar_herder_pending_envelopes{job="henyey", instance=~"$instance"}`
- **Y-axis:** count
- **Description:** SCP envelopes waiting to be processed by the herder. Sustained growth indicates the herder can't keep up.

### Panel 5.3: TX Queue Composition
- **Type:** timeseries (stacked area)
- **Queries:**
  - `stellar_herder_tx_queue_accounts{job="henyey", instance=~"$instance"}` — legend: "Accounts"
  - `stellar_herder_tx_queue_banned{job="henyey", instance=~"$instance"}` — legend: "Banned"
  - `stellar_herder_tx_queue_seen{job="henyey", instance=~"$instance"}` — legend: "Seen"
- **Y-axis:** count
- **Description:** Composition of the transaction queue. High "banned" counts may indicate spam or fee-related eviction.

### Panel 5.4: Pending TX Flow
- **Type:** timeseries
- **Queries:**
  - `rate(stellar_herder_pending_added_total{job="henyey", instance=~"$instance"}[$__rate_interval])` — legend: "Added/s" (green)
  - `rate(stellar_herder_pending_evicted_total{job="henyey", instance=~"$instance"}[$__rate_interval])` — legend: "Evicted/s" (orange)
  - `rate(stellar_herder_pending_released_total{job="henyey", instance=~"$instance"}[$__rate_interval])` — legend: "Released/s" (blue)
  - `rate(stellar_herder_pending_received_total{job="henyey", instance=~"$instance"}[$__rate_interval])` — legend: "Received/s" (gray)
- **Y-axis:** tx/sec
- **Description:** Transaction lifecycle flow rates. Released means included in a ledger. Evicted means dropped (too old or outbid). The ratio of released to added is effective throughput.

### Panel 5.5: Duplicates & Too Old
- **Type:** timeseries
- **Queries:**
  - `rate(stellar_herder_pending_duplicates_total{job="henyey", instance=~"$instance"}[$__rate_interval])` — legend: "Duplicates/s"
  - `rate(stellar_herder_pending_too_old_total{job="henyey", instance=~"$instance"}[$__rate_interval])` — legend: "Too old/s"
- **Y-axis:** tx/sec
- **Description:** Rate of duplicate and expired transactions. High duplicate rates indicate redundant submission or poor peer dedup. High too-old rates may indicate slow propagation.

### Panel 5.6: Pending Transactions (global)
- **Type:** timeseries
- **Query:** `stellar_pending_transactions{job="henyey", instance=~"$instance"}`
- **Y-axis:** count
- **Description:** Total pending (unconfirmed) transactions. Represents the current mempool pressure.

### Panel 5.7: Cached TX Sets
- **Type:** timeseries
- **Query:** `stellar_herder_cached_tx_sets{job="henyey", instance=~"$instance"}`
- **Y-axis:** count
- **Description:** Number of transaction sets cached by the herder for consensus rounds.

---

## Row 6: Overlay / Network

**Collapsed:** Yes (default)
**Purpose:** Network connectivity and message flow health.

### Panel 6.1: Peer Breakdown
- **Type:** timeseries
- **Queries:**
  - `stellar_overlay_inbound_authenticated{job="henyey", instance=~"$instance"}` — legend: "Inbound auth'd"
  - `stellar_overlay_outbound_authenticated{job="henyey", instance=~"$instance"}` — legend: "Outbound auth'd"
  - `stellar_overlay_inbound_pending{job="henyey", instance=~"$instance"}` — legend: "Inbound pending"
  - `stellar_overlay_outbound_pending{job="henyey", instance=~"$instance"}` — legend: "Outbound pending"
- **Y-axis:** count
- **Description:** Directional peer connection breakdown. If outbound drops but inbound is stable, this node may have network egress issues. If inbound drops, other nodes may be rejecting this node (auth issues, ban). Pending connections that don't resolve to authenticated indicate handshake failures.

### Panel 6.2: Message Rates
- **Type:** timeseries
- **Queries:**
  - `rate(stellar_overlay_message_read_total{job="henyey", instance=~"$instance"}[$__rate_interval])` — legend: "Read/s"
  - `rate(stellar_overlay_message_write_total{job="henyey", instance=~"$instance"}[$__rate_interval])` — legend: "Write/s"
  - `rate(stellar_overlay_message_broadcast_total{job="henyey", instance=~"$instance"}[$__rate_interval])` — legend: "Broadcast/s"
- **Y-axis:** messages/sec
- **Description:** Overall message I/O rates. Read and write should be roughly proportional. Broadcast rate shows how actively this node is disseminating information.

### Panel 6.3: Error Rates
- **Type:** timeseries
- **Queries:**
  - `rate(stellar_overlay_error_read_total{job="henyey", instance=~"$instance"}[$__rate_interval])` — legend: "Read errors/s" (red)
  - `rate(stellar_overlay_error_write_total{job="henyey", instance=~"$instance"}[$__rate_interval])` — legend: "Write errors/s" (orange)
- **Y-axis:** errors/sec
- **Description:** Network error rates. Sustained errors indicate connection instability. Correlate with peer count changes.

### Panel 6.4: Timeouts
- **Type:** timeseries
- **Queries:**
  - `rate(stellar_overlay_timeout_idle_total{job="henyey", instance=~"$instance"}[$__rate_interval])` — legend: "Idle timeout/s"
  - `rate(stellar_overlay_timeout_straggler_total{job="henyey", instance=~"$instance"}[$__rate_interval])` — legend: "Straggler timeout/s"
- **Y-axis:** events/sec
- **Description:** Peer timeout rates. Idle timeouts are normal peer lifecycle. Straggler timeouts indicate peers that are connected but too slow — these are more concerning as they suggest degraded peers consuming connection slots.

### Panel 6.5: Fetch Channel Depth
- **Type:** timeseries
- **Queries:**
  - `henyey_overlay_fetch_channel_depth{job="henyey", instance=~"$instance"}` — legend: "Current depth"
  - `henyey_overlay_fetch_channel_depth_max{job="henyey", instance=~"$instance"}` — legend: "Max depth"
- **Y-axis:** count
- **Description:** Internal fetch channel queue depth. High values indicate the node is backlogged on fetching data from peers. **Henyey-specific metric.**

---

## Row 7: Memory & Process

**Collapsed:** Yes (default)
**Purpose:** Process-level resource usage. Useful for capacity planning and debugging memory growth.

### Panel 7.1: Jemalloc Resident Memory
- **Type:** timeseries
- **Query:** `henyey_jemalloc_resident_bytes{job="henyey", instance=~"$instance"}`
- **Y-axis:** bytes (auto-scale to MB/GB)
- **Description:** Resident set size as reported by jemalloc. This is the actual physical memory used. **Henyey-specific metric.**

### Panel 7.2: Allocated vs Active
- **Type:** timeseries
- **Queries:**
  - `henyey_jemalloc_allocated_bytes{job="henyey", instance=~"$instance"}` — legend: "Allocated"
  - `henyey_jemalloc_active_bytes{job="henyey", instance=~"$instance"}` — legend: "Active"
- **Y-axis:** bytes (auto-scale)
- **Description:** Allocated = memory actively used by the application. Active = memory in active pages (includes internal fragmentation). A large gap between them indicates internal fragmentation within jemalloc arenas.

### Panel 7.3: Fragmentation
- **Type:** gauge (or stat)
- **Query:** `henyey_jemalloc_fragmentation_pct{job="henyey", instance=~"$instance"}`
- **Unit:** percent
- **Thresholds:** <15 = green, <30 = yellow, >=30 = red
- **Description:** Jemalloc fragmentation percentage. High fragmentation means the allocator is using more memory than the application needs due to allocation patterns.

### Panel 7.4: Mapped vs Retained
- **Type:** timeseries
- **Queries:**
  - `henyey_jemalloc_mapped_bytes{job="henyey", instance=~"$instance"}` — legend: "Mapped"
  - `henyey_jemalloc_retained_bytes{job="henyey", instance=~"$instance"}` — legend: "Retained"
- **Y-axis:** bytes (auto-scale)
- **Description:** Mapped = total virtual memory mapped by jemalloc. Retained = memory returned to the OS but still in the address space. Retained memory can be reclaimed by the OS under pressure.

### Panel 7.5: File Descriptors
- **Type:** timeseries
- **Queries:**
  - `henyey_process_open_fds{job="henyey", instance=~"$instance"}` — legend: "Open FDs"
  - `henyey_process_max_fds{job="henyey", instance=~"$instance"}` — legend: "Limit" (dashed line)
- **Y-axis:** count
- **Description:** Open file descriptors vs system limit. FD exhaustion causes "too many open files" errors, connection failures, and crashes. The limit line provides visual context.

---

## Row 8: Bucket Merges

**Collapsed:** Yes (default)
**Purpose:** Bucket merge activity. Relevant during high-throughput periods or after catchup.

### Panel 8.1: Merge Completed Rate
- **Type:** timeseries
- **Query:** `rate(stellar_bucket_merge_completed_total{job="henyey", instance=~"$instance"}[$__rate_interval])`
- **Y-axis:** merges/sec
- **Description:** Rate of bucket merge completions.

### Panel 8.2: Merge Time Rate
- **Type:** timeseries
- **Query:** `rate(stellar_bucket_merge_time_us_total{job="henyey", instance=~"$instance"}[$__rate_interval]) / 1000`
- **Y-axis:** ms/sec (milliseconds of merge time per second of wall time)
- **Description:** CPU time spent on bucket merges per second. Values approaching 1000 ms/s mean an entire core is saturated by merges.

### Panel 8.3: Merge Object Breakdown
- **Type:** timeseries (stacked area)
- **Queries:**
  - `rate(stellar_bucket_merge_new_live_total{job="henyey", instance=~"$instance"}[$__rate_interval])` — legend: "Live"
  - `rate(stellar_bucket_merge_new_dead_total{job="henyey", instance=~"$instance"}[$__rate_interval])` — legend: "Dead"
  - `rate(stellar_bucket_merge_new_init_total{job="henyey", instance=~"$instance"}[$__rate_interval])` — legend: "Init"
  - `rate(stellar_bucket_merge_new_meta_total{job="henyey", instance=~"$instance"}[$__rate_interval])` — legend: "Meta"
  - `rate(stellar_bucket_merge_shadowed_total{job="henyey", instance=~"$instance"}[$__rate_interval])` — legend: "Shadowed"
  - `rate(stellar_bucket_merge_annihilated_total{job="henyey", instance=~"$instance"}[$__rate_interval])` — legend: "Annihilated"
- **Y-axis:** objects/sec
- **Description:** Breakdown of objects processed during merges by type. High "annihilated" rates indicate effective deduplication. High "dead" rates correlate with state archival/expiry activity.

---

## Row 9: Soroban Network Config

**Collapsed:** Yes (default)
**Purpose:** Reference view of current Soroban network configuration values. Useful for verifying upgrades landed correctly.

### Panel 9.1: Soroban Config Table
- **Type:** table
- **Transformation:** `reduce` (mode: seriesToRows, reducer: lastNotNull) — collapses each series to a single Name/Value row
- **Queries:** One query per config metric, using `instant` mode and `time_series` format:
  - `stellar_soroban_config_ledger_max_tx_count{job="henyey", instance=~"$instance"}`
  - `stellar_soroban_config_ledger_max_cpu_insn{job="henyey", instance=~"$instance"}`
  - `stellar_soroban_config_ledger_max_txs_size_byte{job="henyey", instance=~"$instance"}`
  - `stellar_soroban_config_ledger_max_read_entry{job="henyey", instance=~"$instance"}`
  - `stellar_soroban_config_ledger_max_read_ledger_byte{job="henyey", instance=~"$instance"}`
  - `stellar_soroban_config_ledger_max_write_entry{job="henyey", instance=~"$instance"}`
  - `stellar_soroban_config_ledger_max_write_ledger_byte{job="henyey", instance=~"$instance"}`
  - `stellar_soroban_config_tx_max_cpu_insn{job="henyey", instance=~"$instance"}`
  - `stellar_soroban_config_tx_max_mem_byte{job="henyey", instance=~"$instance"}`
  - `stellar_soroban_config_tx_max_size_byte{job="henyey", instance=~"$instance"}`
  - `stellar_soroban_config_tx_max_read_entry{job="henyey", instance=~"$instance"}`
  - `stellar_soroban_config_tx_max_read_ledger_byte{job="henyey", instance=~"$instance"}`
  - `stellar_soroban_config_tx_max_write_entry{job="henyey", instance=~"$instance"}`
  - `stellar_soroban_config_tx_max_write_ledger_byte{job="henyey", instance=~"$instance"}`
  - `stellar_soroban_config_tx_max_emit_event_byte{job="henyey", instance=~"$instance"}`
  - `stellar_soroban_config_contract_max_rw_code_byte{job="henyey", instance=~"$instance"}`
  - `stellar_soroban_config_contract_max_rw_data_byte{job="henyey", instance=~"$instance"}`
  - `stellar_soroban_config_contract_max_rw_key_byte{job="henyey", instance=~"$instance"}`
  - `stellar_soroban_config_fee_write_1kb{job="henyey", instance=~"$instance"}`
  - `stellar_soroban_config_bucket_list_target_size_byte{job="henyey", instance=~"$instance"}`
- **Columns:** Metric name (cleaned, e.g. "Ledger Max TX Count"), Value
- **Sort:** Grouped by scope (ledger-level, tx-level, contract-level, fee)
- **Description:** Current Soroban configuration values as seen by this validator. Compare across validators or against expected values after an upgrade vote.

---

## Dashboard-Level Settings

- **Tags:** `henyey`, `validator`, `stellar`
- **Folder:** Create a "Henyey" folder
- **Editable:** true
- **Graph tooltip:** Shared crosshair (all panels show cursor position)
- **Timezone:** Browser
- **Panel defaults:**
  - Line width: 1
  - Fill opacity: 10 (for stacked areas: 50)
  - Point size: 5 (hidden by default)
  - Null values: connected
  - Legend: bottom, as table with min/max/avg/last
