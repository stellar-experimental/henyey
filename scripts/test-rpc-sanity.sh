#!/usr/bin/env bash
#
# Smoke-test Stellar RPC endpoints using the stellar CLI + curl/jq.
# Defaults to public testnet; override with --rpc-url / --network-passphrase.
#
set -euo pipefail

# ── Defaults ──────────────────────────────────────────────────────────────────
RPC_URL=""
NETWORK_PASSPHRASE=""
CUSTOM_RPC=false
PASS_COUNT=0
FAIL_COUNT=0
FAILURES=()
ALICE_KEY="rpctest-alice-$$"
BOB_KEY="rpctest-bob-$$"

# ── Argument parsing ─────────────────────────────────────────────────────────
while [[ $# -gt 0 ]]; do
  case "$1" in
    --rpc-url)            RPC_URL="$2"; CUSTOM_RPC=true; shift 2 ;;
    --network-passphrase) NETWORK_PASSPHRASE="$2"; shift 2 ;;
    -h|--help)
      echo "Usage: $0 [--rpc-url URL] [--network-passphrase PASSPHRASE]"
      exit 0 ;;
    *) echo "Unknown arg: $1"; exit 1 ;;
  esac
done

# When using a custom RPC URL, --network-passphrase is required by the CLI.
# When no custom URL is given, we use --network testnet and the CLI resolves everything.
if $CUSTOM_RPC; then
  if [[ -z "$NETWORK_PASSPHRASE" ]]; then
    NETWORK_PASSPHRASE="Test SDF Network ; September 2015"
    echo "NOTE: No --network-passphrase given, defaulting to testnet passphrase."
  fi
  RPC_URL_FOR_CURL="$RPC_URL"
  # CLI args for stellar commands when using a custom RPC
  CLI_NET_ARGS=(--rpc-url "$RPC_URL" --network-passphrase "$NETWORK_PASSPHRASE")
else
  RPC_URL_FOR_CURL="https://soroban-testnet.stellar.org"
  CLI_NET_ARGS=(--network testnet)
fi

# ── Helpers ──────────────────────────────────────────────────────────────────
pass() {
  PASS_COUNT=$((PASS_COUNT + 1))
  echo "[PASS] $1"
}

fail() {
  FAIL_COUNT=$((FAIL_COUNT + 1))
  FAILURES+=("$1")
  if [[ -n "${2:-}" ]]; then
    echo "[FAIL] $1 - $2"
  else
    echo "[FAIL] $1"
  fi
}

# JSON-RPC helper — prints response body, returns 0/1 based on HTTP success.
rpc_call() {
  local method="$1"
  local params="${2:-null}"
  curl -sf -X POST "$RPC_URL_FOR_CURL" \
    -H 'Content-Type: application/json' \
    -d "{\"jsonrpc\":\"2.0\",\"id\":1,\"method\":\"$method\",\"params\":$params}"
}

cleanup() {
  echo ""
  echo "── Cleanup ────────────────────────────────────────────────────────────"
  stellar keys rm "$ALICE_KEY" 2>/dev/null && echo "Removed key $ALICE_KEY" || true
  stellar keys rm "$BOB_KEY"   2>/dev/null && echo "Removed key $BOB_KEY"   || true
}
trap cleanup EXIT

# ── Preflight checks ────────────────────────────────────────────────────────
echo "── Preflight ──────────────────────────────────────────────────────────"
for cmd in stellar curl jq; do
  if ! command -v "$cmd" &>/dev/null; then
    echo "ERROR: '$cmd' not found in PATH. Install it and retry."
    exit 1
  fi
done
echo "All prerequisites found."
echo "RPC URL: $RPC_URL_FOR_CURL"
echo ""

# ── 1. Direct JSON-RPC endpoint tests ───────────────────────────────────────
echo "── JSON-RPC endpoints ─────────────────────────────────────────────────"

# getHealth
if resp=$(rpc_call getHealth) && echo "$resp" | jq -e '.result.status' &>/dev/null; then
  pass "getHealth"
else
  fail "getHealth" "unhealthy or unreachable"
fi

# getNetwork
if resp=$(rpc_call getNetwork) && echo "$resp" | jq -e '.result.passphrase' &>/dev/null; then
  pass "getNetwork"
else
  fail "getNetwork" "no passphrase in response"
fi

# getLatestLedger
if resp=$(rpc_call getLatestLedger) && echo "$resp" | jq -e '.result.sequence' &>/dev/null; then
  LATEST_LEDGER=$(echo "$resp" | jq -r '.result.sequence')
  pass "getLatestLedger (seq=$LATEST_LEDGER)"
else
  fail "getLatestLedger" "no sequence in response"
fi

# getFeeStats
if resp=$(rpc_call getFeeStats) && echo "$resp" | jq -e '.result' &>/dev/null; then
  pass "getFeeStats"
else
  fail "getFeeStats" "empty result"
fi

# getVersionInfo
if resp=$(rpc_call getVersionInfo) && echo "$resp" | jq -e '.result.version' &>/dev/null; then
  VERSION=$(echo "$resp" | jq -r '.result.version')
  pass "getVersionInfo (version=$VERSION)"
else
  fail "getVersionInfo" "no version in response"
fi

echo ""

# ── 2. Setup accounts ───────────────────────────────────────────────────────
echo "── Account setup ────────────────────────────────────────────────────"

if stellar keys generate "$ALICE_KEY" --fund "${CLI_NET_ARGS[@]}" 2>/dev/null; then
  ALICE_ADDR=$(stellar keys address "$ALICE_KEY")
  echo "Alice: $ALICE_ADDR"
else
  fail "account setup (alice)" "key generation failed"
  echo "Cannot continue without accounts."
  echo ""
  echo "========"
  echo "$PASS_COUNT/$((PASS_COUNT + FAIL_COUNT)) passed"
  exit 1
fi

if stellar keys generate "$BOB_KEY" --fund "${CLI_NET_ARGS[@]}" 2>/dev/null; then
  BOB_ADDR=$(stellar keys address "$BOB_KEY")
  echo "Bob:   $BOB_ADDR"
else
  fail "account setup (bob)" "key generation failed"
  echo "Cannot continue without accounts."
  echo ""
  echo "========"
  echo "$PASS_COUNT/$((PASS_COUNT + FAIL_COUNT)) passed"
  exit 1
fi

pass "account setup"
echo ""

# ── 3. Deploy SAC for native XLM ────────────────────────────────────────────
echo "── Deploy SAC (native XLM) ──────────────────────────────────────────"

# The native SAC may already be deployed (it's a singleton per network).
# Try deploying first; if it already exists, just look up the ID.
if CONTRACT_ID=$(stellar contract asset deploy \
    --asset native \
    --source "$ALICE_KEY" \
    "${CLI_NET_ARGS[@]}" 2>/dev/null); then
  echo "Contract: $CONTRACT_ID"
  pass "contract asset deploy (SAC)"
else
  # Already exists — look up the contract ID
  if CONTRACT_ID=$(stellar contract id asset \
      --asset native \
      "${CLI_NET_ARGS[@]}" 2>/dev/null); then
    echo "Contract (already deployed): $CONTRACT_ID"
    pass "contract asset deploy (SAC, already exists)"
  else
    fail "contract asset deploy (SAC)" "deploy failed and could not look up ID"
    CONTRACT_ID=""
  fi
fi

echo ""

# ── 4. getLedgerEntries — fetch Alice's account ─────────────────────────────
echo "── getLedgerEntries ─────────────────────────────────────────────────"

# Encode Alice's account as an XDR LedgerKey (base64). The stellar CLI can do this.
if ALICE_LEDGER_KEY=$(stellar xdr encode --type LedgerKey \
    "{\"account\":{\"account_id\":\"$ALICE_ADDR\"}}" 2>/dev/null); then
  PARAMS="{\"keys\":[\"$ALICE_LEDGER_KEY\"]}"
  if resp=$(rpc_call getLedgerEntries "$PARAMS") && \
     echo "$resp" | jq -e '.result.entries[0].xdr' &>/dev/null; then
    pass "getLedgerEntries"
  else
    fail "getLedgerEntries" "no entry returned"
  fi
else
  # Fallback: try raw JSON-RPC with a known-good key encoding approach
  fail "getLedgerEntries" "could not encode ledger key"
fi

echo ""

# ── 5. Simulate + submit contract invocation ────────────────────────────────
echo "── Contract invoke (simulateTransaction + sendTransaction) ──────────"

TX_HASH=""

if [[ -n "$CONTRACT_ID" ]]; then
  # Simulate first
  if stellar contract invoke \
      --id "$CONTRACT_ID" \
      --source "$ALICE_KEY" \
      "${CLI_NET_ARGS[@]}" \
      --send no \
      -- transfer \
      --from "$ALICE_ADDR" \
      --to "$BOB_ADDR" \
      --amount 1000000000 &>/dev/null; then
    pass "simulateTransaction (via contract invoke --simulate-only)"
  else
    fail "simulateTransaction" "simulation failed"
  fi

  # Invoke for real — the CLI handles sendTransaction + polling getTransaction
  if INVOKE_OUTPUT=$(stellar contract invoke \
      --id "$CONTRACT_ID" \
      --source "$ALICE_KEY" \
      "${CLI_NET_ARGS[@]}" \
      -- transfer \
      --from "$ALICE_ADDR" \
      --to "$BOB_ADDR" \
      --amount 1000000000 2>&1); then
    pass "sendTransaction + getTransaction (via contract invoke)"
    # Try to extract the tx hash from stellar tx output
    TX_HASH=$(echo "$INVOKE_OUTPUT" | grep -oE '[a-f0-9]{64}' | head -1 || true)
  else
    fail "sendTransaction + getTransaction" "invoke failed"
    echo "  Output: $INVOKE_OUTPUT"
  fi
else
  fail "simulateTransaction" "skipped (no contract)"
  fail "sendTransaction + getTransaction" "skipped (no contract)"
fi

echo ""

# ── 6. getTransaction — fetch completed tx by hash ──────────────────────────
echo "── getTransaction (direct) ──────────────────────────────────────────"

TX_LEDGER=""
if [[ -n "$TX_HASH" ]]; then
  PARAMS="{\"hash\":\"$TX_HASH\"}"
  if resp=$(rpc_call getTransaction "$PARAMS") && \
     echo "$resp" | jq -e '.result.status' &>/dev/null; then
    TX_STATUS=$(echo "$resp" | jq -r '.result.status')
    TX_LEDGER=$(echo "$resp" | jq -r '.result.ledger // empty')
    pass "getTransaction (status=$TX_STATUS)"
  else
    fail "getTransaction" "no result for hash $TX_HASH"
  fi
else
  fail "getTransaction" "skipped (no tx hash available)"
fi

echo ""

# ── 7. getEvents — query transfer events ────────────────────────────────────
echo "── getEvents ────────────────────────────────────────────────────────"

if [[ -n "$CONTRACT_ID" && -n "$TX_LEDGER" && -n "$ALICE_ADDR" ]]; then
  # Encode filter topics: transfer symbol + Alice's address as ScVal
  TRANSFER_TOPIC=$(stellar xdr encode --type ScVal '{"symbol":"transfer"}' 2>/dev/null)
  ALICE_TOPIC=$(stellar xdr encode --type ScVal "{\"address\":\"$ALICE_ADDR\"}" 2>/dev/null)

  # Filter by contract + transfer topic + Alice as sender, starting from the tx ledger
  PARAMS=$(cat <<JSONEOF
{
  "startLedger": $TX_LEDGER,
  "filters": [{
    "type": "contract",
    "contractIds": ["$CONTRACT_ID"],
    "topics": [["$TRANSFER_TOPIC", "$ALICE_TOPIC", "*", "*"]]
  }],
  "pagination": {"limit": 10}
}
JSONEOF
)
  if resp=$(rpc_call getEvents "$PARAMS") && \
     echo "$resp" | jq -e '.result.events' &>/dev/null; then
    # Verify the events belong to our transaction
    MATCH_COUNT=$(echo "$resp" | jq --arg hash "$TX_HASH" \
      '[.result.events[] | select(.txHash == $hash)] | length')
    if [[ "$MATCH_COUNT" -gt 0 ]]; then
      pass "getEvents ($MATCH_COUNT transfer events from alice)"
    else
      EVENT_COUNT=$(echo "$resp" | jq '.result.events | length')
      fail "getEvents" "got $EVENT_COUNT events but none matched tx $TX_HASH"
    fi
  else
    fail "getEvents" "request failed"
  fi
elif [[ -n "$CONTRACT_ID" && -n "$LATEST_LEDGER" ]]; then
  # Fallback: no tx ledger known, use a broad window
  START_LEDGER=$((LATEST_LEDGER - 100))
  if [[ $START_LEDGER -lt 1 ]]; then START_LEDGER=1; fi
  PARAMS=$(cat <<JSONEOF
{
  "startLedger": $START_LEDGER,
  "filters": [{
    "type": "contract",
    "contractIds": ["$CONTRACT_ID"]
  }],
  "pagination": {"limit": 10}
}
JSONEOF
)
  if resp=$(rpc_call getEvents "$PARAMS") && \
     echo "$resp" | jq -e '.result.events[0]' &>/dev/null; then
    pass "getEvents (fallback, no tx hash to verify)"
  else
    fail "getEvents" "no events returned"
  fi
else
  fail "getEvents" "skipped (no contract or ledger info)"
fi

echo ""

# ── Summary ──────────────────────────────────────────────────────────────────
TOTAL=$((PASS_COUNT + FAIL_COUNT))
echo "========"
echo "$PASS_COUNT/$TOTAL passed"

if [[ ${#FAILURES[@]} -gt 0 ]]; then
  echo ""
  echo "Failures:"
  for f in "${FAILURES[@]}"; do
    echo "  - $f"
  done
  exit 1
fi
