#!/usr/bin/env bash
#
# Integration test: run a henyey testnet validator that publishes history,
# then compare the published checkpoint against SDF's testnet archive.
#
# Usage:
#   ./scripts/test-history-publish.sh                   # build + run
#   ./scripts/test-history-publish.sh --no-build        # skip cargo build
#   ./scripts/test-history-publish.sh --timeout 600     # wait up to 10 min
#   ./scripts/test-history-publish.sh --checkpoint 63   # compare specific checkpoint
#   ./scripts/test-history-publish.sh --data-dir /tmp/x # use specific data directory
#
# Exit codes:
#   0 = checkpoint matches SDF archive
#   1 = mismatch or error
#
set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
PROJECT_ROOT="$(cd "$SCRIPT_DIR/.." && pwd)"
BINARY="$PROJECT_ROOT/target/release/henyey"

# Defaults
DO_BUILD=true
TIMEOUT=600        # 10 minutes max wait for first checkpoint
CHECKPOINT=""      # auto-detect from published HAS
KEEP_DATA=false
DATA_DIR_OVERRIDE=""  # if set, use this instead of auto-generated path

# SDF testnet reference archive
SDF_ARCHIVE="https://history.stellar.org/prd/core-testnet/core_testnet_001"

# Parse args
while [[ $# -gt 0 ]]; do
  case "$1" in
    --no-build)     DO_BUILD=false; shift ;;
    --timeout)      TIMEOUT="$2"; shift 2 ;;
    --checkpoint)   CHECKPOINT="$2"; shift 2 ;;
    --keep-data)    KEEP_DATA=true; shift ;;
    --data-dir)     DATA_DIR_OVERRIDE="$2"; KEEP_DATA=true; shift 2 ;;
    -h|--help)
      sed -n '3,14p' "$0" | sed 's/^# \?//'
      exit 0 ;;
    *) echo "Unknown arg: $1"; exit 1 ;;
  esac
done

# --- Data dirs ---
if [[ -n "$DATA_DIR_OVERRIDE" ]]; then
  DATA_DIR="$DATA_DIR_OVERRIDE"
else
  DATA_DIR="$PROJECT_ROOT/data/publish-test-$$"
fi
HISTORY_DIR="$DATA_DIR/history"
DB_PATH="$DATA_DIR/validator.db"
BUCKET_DIR="$DATA_DIR/buckets"
CONFIG_FILE="$DATA_DIR/validator.toml"
LOG_FILE="$DATA_DIR/validator.log"
NODE_PID=""

mkdir -p "$HISTORY_DIR" "$BUCKET_DIR"

cleanup() {
  if [[ -n "$NODE_PID" ]] && kill -0 "$NODE_PID" 2>/dev/null; then
    echo "Stopping validator (pid $NODE_PID)..."
    kill "$NODE_PID" 2>/dev/null || true
    wait "$NODE_PID" 2>/dev/null || true
  fi
  if [[ "$KEEP_DATA" == "false" ]]; then
    echo "Cleaning up $DATA_DIR"
    rm -rf "$DATA_DIR"
  else
    echo "Data kept at $DATA_DIR"
  fi
}
trap cleanup EXIT

# --- Build ---
if [[ "$DO_BUILD" == "true" ]]; then
  echo "Building henyey (release)..."
  cargo build --release --manifest-path "$PROJECT_ROOT/Cargo.toml" -p henyey 2>&1
  echo "Build complete."
  echo
fi

if [[ ! -x "$BINARY" ]]; then
  echo "ERROR: Binary not found at $BINARY"
  echo "Run with --no-build only if already built."
  exit 1
fi

# --- Generate ephemeral node seed ---
# We need a keypair for the validator. Generate one.
SEED_OUTPUT=$("$BINARY" new-keypair 2>&1)
NODE_SEED=$(echo "$SEED_OUTPUT" | grep -oP 'S[A-Z0-9]{55}' | head -1)
if [[ -z "$NODE_SEED" ]]; then
  echo "ERROR: Failed to generate node keypair"
  echo "$SEED_OUTPUT"
  exit 1
fi
echo "Generated ephemeral node seed: ${NODE_SEED:0:4}..."

# Pick a random high port for the overlay listener
PEER_PORT=$((30000 + RANDOM % 10000))
echo "Using overlay peer port: $PEER_PORT"

# --- Generate config ---
cat > "$CONFIG_FILE" <<EOF
[node]
name = "henyey-publish-test"
is_validator = true
node_seed = "$NODE_SEED"
max_tx_set_size = 100

[node.quorum_set]
threshold_percent = 67
validators = [
    "GDKXE2OZMJIPOSLNA6N6F2BVCI3O777I2OOC4BV7VOYUEHYX7RTRYA7Y",
    "GCUCJTIYXSOXKBSNFGNFWW5MUQ54HKRPGJUTQFJ5RQXZXNOLNXYDHRAP",
    "GC2V2EFSXN6SQTWVYA5EPJPBWWIMSD2XQNKUOHGEKB535AQE2I6IXV2Z"
]

[network]
passphrase = "Test SDF Network ; September 2015"

[database]
path = "$DB_PATH"
pool_size = 10

[buckets]
directory = "$BUCKET_DIR"

[history]
# SDF testnet archives for catchup
[[history.archives]]
name = "sdf1"
url = "https://history.stellar.org/prd/core-testnet/core_testnet_001"

[[history.archives]]
name = "sdf2"
url = "https://history.stellar.org/prd/core-testnet/core_testnet_002"

[[history.archives]]
name = "sdf3"
url = "https://history.stellar.org/prd/core-testnet/core_testnet_003"

# Local archive for publishing
[[history.archives]]
name = "local"
url = "file://$HISTORY_DIR"
get_enabled = true
put_enabled = true
put = "cp {0} $HISTORY_DIR/{1}"
mkdir = "mkdir -p $HISTORY_DIR/{0}"

[overlay]
# Use a random high port so peers don't reject us for advertising port 0
peer_port = $PEER_PORT
max_inbound_peers = 8
max_outbound_peers = 8
target_outbound_peers = 8
known_peers = [
    "core-testnet1.stellar.org:11625",
    "core-testnet2.stellar.org:11625",
    "core-testnet3.stellar.org:11625"
]

[http]
enabled = false

[compat_http]
enabled = false
EOF

echo "Config written to $CONFIG_FILE"
echo "History will be published to $HISTORY_DIR"
echo

# --- Initialize database ---
echo "Initializing database..."
"$BINARY" --config "$CONFIG_FILE" --testnet new-db 2>&1
echo "Database initialized."
echo

# --- Initialize local history archive ---
echo "Initializing local history archive..."
"$BINARY" --config "$CONFIG_FILE" --testnet new-hist local 2>&1
echo "Local history archive initialized."
echo

# --- Start validator ---
echo "Starting validator..."
"$BINARY" --config "$CONFIG_FILE" --testnet run --validator > "$LOG_FILE" 2>&1 &
NODE_PID=$!
echo "Validator started (pid $NODE_PID), logging to $LOG_FILE"
echo

# --- Poll for published checkpoint ---
HAS_FILE="$HISTORY_DIR/.well-known/stellar-history.json"
echo "Waiting for first published checkpoint (timeout: ${TIMEOUT}s)..."

START_TIME=$(date +%s)
while true; do
  ELAPSED=$(( $(date +%s) - START_TIME ))
  if [[ $ELAPSED -ge $TIMEOUT ]]; then
    echo "ERROR: Timed out after ${TIMEOUT}s waiting for checkpoint"
    echo "Last 50 lines of validator log:"
    tail -50 "$LOG_FILE"
    exit 1
  fi

  # Check if the process is still alive
  if ! kill -0 "$NODE_PID" 2>/dev/null; then
    echo "ERROR: Validator process died"
    echo "Last 50 lines of validator log:"
    tail -50 "$LOG_FILE"
    exit 1
  fi

  # Check for published HAS
  if [[ -f "$HAS_FILE" ]]; then
    # Read the currentLedger from the HAS
    CURRENT_LEDGER=$(jq -r '.currentLedger' "$HAS_FILE" 2>/dev/null || echo "0")
    if [[ "$CURRENT_LEDGER" -gt 0 ]]; then
      echo "Published checkpoint found! currentLedger=$CURRENT_LEDGER (after ${ELAPSED}s)"
      break
    fi
  fi

  # Print progress every 30 seconds
  if [[ $(( ELAPSED % 30 )) -eq 0 ]] && [[ $ELAPSED -gt 0 ]]; then
    echo "  Still waiting... (${ELAPSED}s elapsed)"
  fi

  sleep 5
done

echo

# --- Stop validator ---
echo "Stopping validator..."
kill "$NODE_PID" 2>/dev/null || true
wait "$NODE_PID" 2>/dev/null || true
NODE_PID=""
echo "Validator stopped."
echo

# --- Determine checkpoint to compare ---
if [[ -z "$CHECKPOINT" ]]; then
  CHECKPOINT="$CURRENT_LEDGER"
fi
echo "Comparing checkpoint $CHECKPOINT"
echo

# --- Run comparison ---
echo "Running compare-checkpoint..."
echo "  Local:     file://$HISTORY_DIR"
echo "  Reference: $SDF_ARCHIVE"
echo

"$BINARY" --testnet compare-checkpoint \
  --local-archive "file://$HISTORY_DIR" \
  --remote-archive "$SDF_ARCHIVE" \
  --checkpoint "$CHECKPOINT"

EXIT_CODE=$?

if [[ $EXIT_CODE -eq 0 ]]; then
  echo
  echo "SUCCESS: Checkpoint $CHECKPOINT matches SDF testnet archive"
else
  echo
  echo "FAILURE: Checkpoint $CHECKPOINT has mismatches (exit code $EXIT_CODE)"
fi

exit $EXIT_CODE
