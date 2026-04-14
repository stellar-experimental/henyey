#!/usr/bin/env bash
#
# Smoke-test Stellar Horizon REST endpoints using the stellar CLI + curl/jq.
# Defaults to standalone network on localhost; override with --horizon-url etc.
#
set -euo pipefail

# ── Defaults ──────────────────────────────────────────────────────────────────
HORIZON_URL=""
RPC_URL=""
NETWORK_PASSPHRASE=""
CUSTOM=false
PASS_COUNT=0
FAIL_COUNT=0
FAILURES=()
ALICE_KEY="horizontest-alice-$$"
BOB_KEY="horizontest-bob-$$"

# ── Argument parsing ─────────────────────────────────────────────────────────
while [[ $# -gt 0 ]]; do
  case "$1" in
    --horizon-url)        HORIZON_URL="$2"; CUSTOM=true; shift 2 ;;
    --rpc-url)            RPC_URL="$2"; shift 2 ;;
    --network-passphrase) NETWORK_PASSPHRASE="$2"; shift 2 ;;
    -h|--help)
      echo "Usage: $0 [--horizon-url URL] [--rpc-url URL] [--network-passphrase PASSPHRASE]"
      exit 0 ;;
    *) echo "Unknown arg: $1"; exit 1 ;;
  esac
done

if $CUSTOM; then
  if [[ -z "$NETWORK_PASSPHRASE" ]]; then
    NETWORK_PASSPHRASE="Standalone Network ; February 2017"
    echo "NOTE: No --network-passphrase given, defaulting to standalone passphrase."
  fi
  if [[ -z "$HORIZON_URL" ]]; then
    HORIZON_URL="http://localhost:8000"
  fi
  if [[ -z "$RPC_URL" ]]; then
    RPC_URL="http://localhost:8000/soroban/rpc"
  fi
  CLI_NET_ARGS=(--rpc-url "$RPC_URL" --network-passphrase "$NETWORK_PASSPHRASE")
else
  HORIZON_URL="http://localhost:8000"
  RPC_URL="http://localhost:8000/soroban/rpc"
  NETWORK_PASSPHRASE="Standalone Network ; February 2017"
  CLI_NET_ARGS=(--rpc-url "$RPC_URL" --network-passphrase "$NETWORK_PASSPHRASE")
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

# GET helper — prints response body.
horizon_get() {
  curl -sf "$HORIZON_URL$1"
}

# Retry a horizon GET until a jq expression succeeds, up to $2 seconds (default 30).
horizon_poll() {
  local path="$1"
  local jq_expr="$2"
  local timeout="${3:-30}"
  local interval=2
  local elapsed=0
  local resp
  while [[ $elapsed -lt $timeout ]]; do
    if resp=$(horizon_get "$path") && echo "$resp" | jq -e "$jq_expr" &>/dev/null; then
      echo "$resp"
      return 0
    fi
    sleep $interval
    elapsed=$((elapsed + interval))
  done
  return 1
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
echo "Horizon URL: $HORIZON_URL"
echo "RPC URL:     $RPC_URL"
echo ""

# ── 0. Wait for Horizon ingestion ───────────────────────────────────────────
echo "── Waiting for Horizon to start ingesting (up to 120s) ────────────"
INGEST_TIMEOUT=120
INGEST_ELAPSED=0
while [[ $INGEST_ELAPSED -lt $INGEST_TIMEOUT ]]; do
  if resp=$(horizon_get "/ledgers?order=desc&limit=1") && \
     echo "$resp" | jq -e '._embedded.records[0].sequence' &>/dev/null; then
    INGEST_SEQ=$(echo "$resp" | jq -r '._embedded.records[0].sequence')
    echo "Horizon ingesting (latest ledger=$INGEST_SEQ, waited ${INGEST_ELAPSED}s)"
    break
  fi
  sleep 2
  INGEST_ELAPSED=$((INGEST_ELAPSED + 2))
done
if [[ $INGEST_ELAPSED -ge $INGEST_TIMEOUT ]]; then
  echo "WARNING: Horizon not ingesting after ${INGEST_TIMEOUT}s, continuing anyway"
fi
echo ""

# ── 1. Horizon health checks ────────────────────────────────────────────────
echo "── Horizon health checks ────────────────────────────────────────────"

# GET / — root endpoint
if resp=$(horizon_get "/") && \
   echo "$resp" | jq -e '.core_latest_ledger > 0' &>/dev/null; then
  CORE_VERSION=$(echo "$resp" | jq -r '.core_version // "unknown"')
  HORIZON_VERSION=$(echo "$resp" | jq -r '.horizon_version // "unknown"')
  pass "Horizon root (core_version=$CORE_VERSION, horizon_version=$HORIZON_VERSION)"
else
  fail "Horizon root" "unreachable or core_latest_ledger is 0"
fi

# GET /fee_stats
if resp=$(horizon_get "/fee_stats") && \
   echo "$resp" | jq -e '.last_ledger' &>/dev/null; then
  pass "fee_stats"
else
  fail "fee_stats" "no last_ledger in response"
fi

# GET /ledgers?order=desc&limit=1
if resp=$(horizon_get "/ledgers?order=desc&limit=1") && \
   echo "$resp" | jq -e '._embedded.records[0].sequence' &>/dev/null; then
  LATEST_SEQ=$(echo "$resp" | jq -r '._embedded.records[0].sequence')
  pass "latest ledger (seq=$LATEST_SEQ)"
else
  fail "latest ledger" "no ledger returned"
fi

echo ""

# ── 2. Fund accounts via friendbot ──────────────────────────────────────────
echo "── Fund accounts ────────────────────────────────────────────────────"

# Generate keypairs (no --fund: generates locally without funding)
stellar keys generate "$ALICE_KEY" --overwrite 2>/dev/null
ALICE_ADDR=$(stellar keys address "$ALICE_KEY")
stellar keys generate "$BOB_KEY" --overwrite 2>/dev/null
BOB_ADDR=$(stellar keys address "$BOB_KEY")

echo "Alice: $ALICE_ADDR"
echo "Bob:   $BOB_ADDR"

# Fund via friendbot (retry up to 60s — friendbot may need time to become ready)
friendbot_fund() {
  local addr="$1"
  local timeout=60
  local elapsed=0
  while [[ $elapsed -lt $timeout ]]; do
    if resp=$(curl -sf "$HORIZON_URL/friendbot?addr=$addr" 2>/dev/null); then
      echo "$resp"
      return 0
    fi
    sleep 2
    elapsed=$((elapsed + 2))
  done
  return 1
}

if resp=$(friendbot_fund "$ALICE_ADDR"); then
  pass "friendbot fund alice"
else
  fail "friendbot fund alice" "friendbot request failed"
fi

if resp=$(friendbot_fund "$BOB_ADDR"); then
  pass "friendbot fund bob"
else
  fail "friendbot fund bob" "friendbot request failed"
fi

# Verify account exists
if resp=$(horizon_get "/accounts/$ALICE_ADDR") && \
   echo "$resp" | jq -e '.balances[0].balance' &>/dev/null; then
  ALICE_BALANCE=$(echo "$resp" | jq -r '.balances[] | select(.asset_type == "native") | .balance')
  pass "account lookup alice (balance=$ALICE_BALANCE)"
else
  fail "account lookup alice" "account not found or no balances"
fi

echo ""

# ── 3. Build + submit classic payment ───────────────────────────────────────
echo "── Classic payment tx ───────────────────────────────────────────────"

TX_HASH=""

# Build the payment transaction using stellar CLI (uses RPC for sequence number)
if TX_XDR=$(stellar tx new payment \
    --source-account "$ALICE_KEY" \
    --destination "$BOB_ADDR" \
    --asset native \
    --amount 10000000 \
    --build-only \
    "${CLI_NET_ARGS[@]}" 2>/dev/null); then

  # Sign the transaction (pass XDR as positional arg, not via pipe)
  if SIGNED_XDR=$(stellar tx sign \
      --sign-with-key "$ALICE_KEY" \
      "${CLI_NET_ARGS[@]}" \
      "$TX_XDR" 2>/dev/null); then
    TX_XDR="$SIGNED_XDR"
  fi

  # Submit directly to Horizon (--data-urlencode to handle base64 '+' chars)
  if resp=$(curl -s -X POST "$HORIZON_URL/transactions" \
      --data-urlencode "tx=$TX_XDR"); then
    TX_HASH=$(echo "$resp" | jq -r '.hash // empty')
    TX_SUCCESS=$(echo "$resp" | jq -r '.successful // false')
    if [[ "$TX_SUCCESS" == "true" && -n "$TX_HASH" ]]; then
      pass "submit payment tx (hash=$TX_HASH)"
    else
      RESULT_CODES=$(echo "$resp" | jq -r '.extras.result_codes // empty' 2>/dev/null || true)
      fail "submit payment tx" "successful=$TX_SUCCESS result_codes=$RESULT_CODES"
    fi
  else
    fail "submit payment tx" "HTTP request failed"
  fi
else
  fail "submit payment tx" "tx build failed"
fi

echo ""

# ── 4. Verify Horizon indexing ──────────────────────────────────────────────
echo "── Verify Horizon indexing ──────────────────────────────────────────"

if [[ -n "$TX_HASH" ]]; then
  # GET /transactions/{hash} — poll until indexed
  if resp=$(horizon_poll "/transactions/$TX_HASH" '.successful'); then
    TX_SUCCESSFUL=$(echo "$resp" | jq -r '.successful')
    pass "GET /transactions/$TX_HASH (successful=$TX_SUCCESSFUL)"
  else
    fail "GET /transactions/$TX_HASH" "not indexed within timeout"
  fi

  # GET /transactions/{hash}/operations
  if resp=$(horizon_poll "/transactions/$TX_HASH/operations" \
      '._embedded.records[0].type'); then
    OP_TYPE=$(echo "$resp" | jq -r '._embedded.records[0].type')
    OP_AMOUNT=$(echo "$resp" | jq -r '._embedded.records[0].amount // empty')
    pass "GET /transactions/$TX_HASH/operations (type=$OP_TYPE, amount=$OP_AMOUNT)"
  else
    fail "GET /transactions/$TX_HASH/operations" "no operations returned"
  fi

  # GET /accounts/{bob}/payments — verify bob received the payment
  if resp=$(horizon_poll "/accounts/$BOB_ADDR/payments?order=desc&limit=1" \
      '._embedded.records[0].type'); then
    PAY_TYPE=$(echo "$resp" | jq -r '._embedded.records[0].type')
    PAY_FROM=$(echo "$resp" | jq -r '._embedded.records[0].from // empty')
    pass "GET /accounts/$BOB_ADDR/payments (type=$PAY_TYPE, from=$PAY_FROM)"
  else
    fail "GET /accounts/$BOB_ADDR/payments" "no payments found for bob"
  fi
else
  fail "GET /transactions/{hash}" "skipped (no tx hash)"
  fail "GET /transactions/{hash}/operations" "skipped (no tx hash)"
  fail "GET /accounts/{bob}/payments" "skipped (no tx hash)"
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
