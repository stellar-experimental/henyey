#!/usr/bin/env bash
# Security audit script — runs Claude Code on each Rust source file in CTF mode.
# Inspired by https://mtlynch.io/claude-code-found-linux-vulnerability
#
# Usage:
#   ./scripts/audit.sh                      # audit all crates
#   ./scripts/audit.sh --crate crypto       # audit one crate
#   AUDIT_JOBS=2 ./scripts/audit.sh         # control parallelism
#   AUDIT_MODEL=sonnet ./scripts/audit.sh   # override model
set -euo pipefail

REPO="$(cd "$(dirname "$0")/.." && pwd)"
REPORT_DIR="$REPO/reports/audit"
JOBS="${AUDIT_JOBS:-4}"
MODEL="${AUDIT_MODEL:-opus}"
SUMMARY_MODEL="${AUDIT_SUMMARY_MODEL:-opus}"

# --- Internal: audit a single file (called via --audit-file) ---
if [[ "${1:-}" == "--audit-file" ]]; then
  file="$2"
  crate=$(echo "$file" | sed "s|${REPO}/crates/||" | cut -d/ -f1)
  relpath=$(echo "$file" | sed "s|${REPO}/crates/${crate}/src/||; s|/|__|g")
  report="${REPORT_DIR}/${crate}__${relpath}.md"

  # Resume: skip if already done
  if [[ -s "$report" ]]; then
    echo "SKIP ${crate}/${relpath}"
    exit 0
  fi

  # Crate context
  case "$crate" in
    crypto)      ctx="Ed25519 signatures, Curve25519 key exchange, sealed boxes, SHA-256 hashing for a blockchain validator." ;;
    scp)         ctx="Stellar Consensus Protocol (FBA): ballot protocol, nomination, quorum set management. Consensus bugs cause network forks." ;;
    overlay)     ctx="P2P networking: TCP connections, HMAC-authenticated framing, message routing, flood control, peer management. Handles untrusted network input." ;;
    tx)          ctx="Transaction parsing, validation, and execution for all Stellar operation types including Soroban smart contracts. Financial logic lives here." ;;
    ledger)      ctx="Ledger close sequencing, transaction batch execution, state commitment hashing. Determinism bugs here cause chain splits." ;;
    bucket)      ctx="Persistent state storage using a log-structured merge tree (bucket list). Handles disk I/O, bucket merges, and hot archive." ;;
    rpc)         ctx="JSON-RPC server exposing Soroban endpoints (simulate, send_transaction, get_events). Accepts untrusted external input." ;;
    herder)      ctx="Coordinates SCP consensus with transaction queue management. Bridges overlay messages to consensus and ledger close." ;;
    app)         ctx="Main application orchestration: startup, catchup, configuration parsing, shutdown." ;;
    *)           ctx="Infrastructure/support crate for a Stellar blockchain validator." ;;
  esac

  # Read file content; truncate very large files
  content=$(head -c 300000 "$file")
  file_size=$(wc -c < "$file")
  if [[ "$file_size" -gt 300000 ]]; then
    content="${content}

... (file truncated at 300KB, original size: ${file_size} bytes) ..."
  fi

  prompt=$(cat <<PROMPT
You are a security researcher in a CTF competition. Your target is a Rust
implementation of a Stellar blockchain validator (stellar-core equivalent).

Audit this file: ${file}
This file belongs to the "${crate}" crate.
Crate context: ${ctx}

\`\`\`rust
${content}
\`\`\`

Look for these vulnerability classes, ordered by severity:

CRITICAL:
- Consensus safety: equivocation, fork attacks, quorum manipulation, vote
  replay that could cause network splits or double-spends
- Transaction validation bypasses: missing checks that allow unauthorized
  operations, balance manipulation, or fee evasion
- Cryptographic misuse: timing side-channels in signature verification,
  weak/predictable randomness, key material leaks, nonce reuse
- Determinism violations: any code path where two honest nodes processing
  the same ledger could reach different states (floating point, HashMap
  iteration order, system clock usage, thread-dependent ordering)

HIGH:
- Integer overflow/underflow in financial calculations (balances, fees,
  offers, liquidity pools) -- even with Rust's default overflow checks,
  look for wrapping ops, casts, or checked arithmetic that silently
  saturates
- Network/overlay attacks: eclipse attacks, amplification DoS, malformed
  message injection, authentication bypass, unbounded allocations from
  peer data
- Race conditions in async/concurrent code: TOCTOU bugs, lock ordering
  issues, missing atomicity in multi-step state mutations
- Unsafe Rust: memory safety violations, unsound abstractions

MEDIUM:
- RPC input validation: injection, unbounded queries, information leaks
- Resource exhaustion: unbounded Vec/HashMap growth from external input,
  missing size limits on deserialized XDR
- Error handling: panics on untrusted input, swallowed errors that hide
  corruption, unwrap() on fallible operations in non-test code
- Logic bugs: off-by-one in protocol-critical ranges, missing edge cases

CONTEXT: This is a production blockchain validator handling real funds.
A consensus or determinism bug could halt the network or cause a chain
split. A transaction validation bug could allow theft of funds.

Output format -- report ONLY confirmed or high-confidence findings:

For each finding, write:

## [SEVERITY] Short title

**Location**: function_name (line ~N)
**Category**: (from the list above)
**Description**: What the bug is, concretely.
**Exploit scenario**: How an attacker would trigger it.
**Suggested fix**: One-liner or short description.

If you find nothing notable, write: "No significant findings."
Do not report style issues, missing docs, or test coverage gaps.
PROMPT
)

  echo "AUDIT ${crate}/${relpath}"
  if claude --print --model "$MODEL" --dangerously-skip-permissions "$prompt" > "$report" 2>/dev/null; then
    if [[ ! -s "$report" ]]; then
      rm -f "$report"
      echo "FAIL ${crate}/${relpath} (empty output)"
      exit 1
    fi
    echo "DONE ${crate}/${relpath}"
  else
    rm -f "$report"
    echo "FAIL ${crate}/${relpath}"
    exit 1
  fi
  exit 0
fi

# --- Main entry point ---

# Parse --crate flag
CRATE_FILTER=""
while [[ $# -gt 0 ]]; do
  case "$1" in
    --crate) CRATE_FILTER="$2"; shift 2 ;;
    *) echo "Unknown option: $1"; exit 1 ;;
  esac
done

# Crate priority order (highest risk first)
PRIORITY=(
  crypto scp overlay
  tx ledger bucket
  rpc herder app
  history historywork db common work simulation clock henyey
)

mkdir -p "$REPORT_DIR"

# Count files
file_count=0
for crate in "${PRIORITY[@]}"; do
  if [[ -n "$CRATE_FILTER" && "$crate" != "$CRATE_FILTER" ]]; then
    continue
  fi
  if [[ -d "$REPO/crates/$crate/src" ]]; then
    n=$(find "$REPO/crates/$crate/src" -name '*.rs' -not -path '*/tests/*' -type f 2>/dev/null | wc -l)
    file_count=$((file_count + n))
  fi
done

echo "=== Henyey Security Audit ==="
echo "Model: $MODEL | Jobs: $JOBS | Files: $file_count"
if [[ -n "$CRATE_FILTER" ]]; then
  echo "Crate filter: $CRATE_FILTER"
fi
echo "Reports: $REPORT_DIR/"
echo ""

SELF="$(realpath "$0")"

# Build file list in priority order, run in parallel
for crate in "${PRIORITY[@]}"; do
  if [[ -n "$CRATE_FILTER" && "$crate" != "$CRATE_FILTER" ]]; then
    continue
  fi
  if [[ -d "$REPO/crates/$crate/src" ]]; then
    find "$REPO/crates/$crate/src" -name '*.rs' -not -path '*/tests/*' -type f 2>/dev/null | sort
  fi
done | xargs -P "$JOBS" -I{} "$SELF" --audit-file {}

# Summary step: collect findings, deduplicate with opus
echo ""
echo "=== Generating Summary ==="

findings_file=$(mktemp)
trap 'rm -f "$findings_file"' EXIT

finding_count=0
for report in "$REPORT_DIR"/*.md; do
  [[ -f "$report" ]] || continue
  [[ "$(basename "$report")" == "SUMMARY.md" ]] && continue
  # Include reports that do NOT contain "No significant findings"
  if ! grep -q "No significant findings" "$report" 2>/dev/null; then
    echo "--- $(basename "$report" .md) ---" >> "$findings_file"
    cat "$report" >> "$findings_file"
    echo "" >> "$findings_file"
    finding_count=$((finding_count + 1))
  fi
done

if [[ "$finding_count" -gt 0 ]]; then
  echo "Files with findings: $finding_count"
  summary_prompt="You are a senior security auditor reviewing findings from a CTF-style audit
of a Rust blockchain validator (Stellar Core reimplementation called henyey).

Below are the raw findings from individual file audits. Your job:

1. Remove exact duplicates (same bug reported from different files)
2. Remove false positives and low-confidence findings
3. Rank remaining findings by severity: CRITICAL > HIGH > MEDIUM
4. For each unique finding, note all affected files
5. Add a brief executive summary at the top

Output a clean final report in markdown.

Raw findings:

$(cat "$findings_file")"

  claude --print --model "$SUMMARY_MODEL" --dangerously-skip-permissions "$summary_prompt" \
    > "$REPORT_DIR/SUMMARY.md" 2>/dev/null

  echo "Summary written to: $REPORT_DIR/SUMMARY.md"
else
  echo "No findings to summarize."
  echo "# Audit Summary" > "$REPORT_DIR/SUMMARY.md"
  echo "" >> "$REPORT_DIR/SUMMARY.md"
  echo "No significant findings across all audited files." >> "$REPORT_DIR/SUMMARY.md"
fi

echo ""
echo "=== Audit Complete ==="
echo "Reports: $REPORT_DIR/"
echo "Summary: $REPORT_DIR/SUMMARY.md"
