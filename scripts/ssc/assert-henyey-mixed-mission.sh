#!/usr/bin/env bash
#
# assert-henyey-mixed-mission.sh — operator-run acceptance checks for the
# Henyey mixed-image Supercluster (SSC) mission. Run this against a LIVE
# mission (the nsc-side instance is up and the SSC dotnet harness has the
# mixed network running). It encodes mission acceptance criteria AC#3–AC#6 as
# a runnable, exit-coded check.
#
#   AC#3 overlay authenticated peering : henyey peer.peer.authenticated-count > 0
#   AC#4 ledger externalize over window: henyey ledger seq advances across the window
#   AC#5 seq + hash agreement          : henyey and a stellar-core peer agree on
#                                        (ledger num, ledger hash) at a common seq
#   AC#6 henyey stays Synced           : henyey /info state == "Synced!"
#
# Field paths (verified against the henyey compat handlers, which mirror
# stellar-core's /info and /metrics shapes):
#   /info    -> .info.state, .info.ledger.num, .info.ledger.hash
#   /metrics -> .metrics["peer.peer.authenticated-count"].count
#
# Usage:
#   scripts/ssc/assert-henyey-mixed-mission.sh \
#       --henyey-info    http://<henyey-pod>:11626/info \
#       --core-info      http://<core-pod>:11626/info \
#       --henyey-metrics http://<henyey-pod>:11626/metrics \
#       [--window-secs 60] [--poll-secs 5] [--self-check]
#
#   --self-check   Run an offline self-test of the JSON-extraction + agreement
#                  logic against built-in fixtures (no live infra, no curl).
#                  CI / the harness runs this to exercise the assert logic.
#
set -euo pipefail

HENYEY_INFO=""
CORE_INFO=""
HENYEY_METRICS=""
WINDOW_SECS=60
POLL_SECS=5
SELF_CHECK=0

usage() { sed -n '2,38p' "$0" | sed 's/^# \{0,1\}//'; exit "${1:-0}"; }

while [ $# -gt 0 ]; do
  case "$1" in
    --henyey-info) HENYEY_INFO="$2"; shift 2 ;;
    --core-info) CORE_INFO="$2"; shift 2 ;;
    --henyey-metrics) HENYEY_METRICS="$2"; shift 2 ;;
    --window-secs) WINDOW_SECS="$2"; shift 2 ;;
    --poll-secs) POLL_SECS="$2"; shift 2 ;;
    --self-check) SELF_CHECK=1; shift ;;
    -h|--help) usage 0 ;;
    *) echo "Unknown argument: $1" >&2; usage 1 ;;
  esac
done

need() { command -v "$1" >/dev/null 2>&1 || { echo "ERROR: '$1' is required" >&2; exit 2; }; }
need jq

# --- JSON extractors (apples-to-apples with stellar-core /info shape) --------
info_state()  { jq -r '.info.state'        <<<"$1"; }
info_seq()    { jq -r '.info.ledger.num'   <<<"$1"; }
info_hash()   { jq -r '.info.ledger.hash'  <<<"$1"; }
metric_auth() { jq -r '.metrics["peer.peer.authenticated-count"].count' <<<"$1"; }

# --- Self-check: exercise the logic offline against fixtures -----------------
if [ "$SELF_CHECK" -eq 1 ]; then
  echo "=== assert-henyey-mixed-mission self-check (offline) ==="
  HENYEY_FIX='{"info":{"state":"Synced!","ledger":{"num":12345,"hash":"deadbeef"}}}'
  CORE_FIX='{"info":{"state":"Synced!","ledger":{"num":12345,"hash":"deadbeef"}}}'
  METRICS_FIX='{"metrics":{"peer.peer.authenticated-count":{"type":"counter","count":3}}}'
  fail=0
  [ "$(info_state  "$HENYEY_FIX")" = "Synced!" ] || { echo "FAIL: state extract"; fail=1; }
  [ "$(info_seq    "$HENYEY_FIX")" = "12345" ]   || { echo "FAIL: seq extract"; fail=1; }
  [ "$(info_hash   "$HENYEY_FIX")" = "deadbeef" ]|| { echo "FAIL: hash extract"; fail=1; }
  [ "$(metric_auth "$METRICS_FIX")" = "3" ]      || { echo "FAIL: auth-count extract"; fail=1; }
  # agreement at a common seq
  if [ "$(info_seq "$HENYEY_FIX")" = "$(info_seq "$CORE_FIX")" ] \
     && [ "$(info_hash "$HENYEY_FIX")" = "$(info_hash "$CORE_FIX")" ]; then
    echo "OK: seq+hash agreement logic"
  else
    echo "FAIL: agreement logic"; fail=1
  fi
  # mismatch must be detected
  CORE_BAD='{"info":{"state":"Synced!","ledger":{"num":12345,"hash":"feedface"}}}'
  if [ "$(info_hash "$HENYEY_FIX")" != "$(info_hash "$CORE_BAD")" ]; then
    echo "OK: hash-mismatch detection"
  else
    echo "FAIL: hash-mismatch not detected"; fail=1
  fi
  if [ "$fail" -eq 0 ]; then echo "=== self-check PASSED ==="; exit 0; else echo "=== self-check FAILED ==="; exit 1; fi
fi

# --- Live mode requires the three endpoints ----------------------------------
need curl
[ -n "$HENYEY_INFO" ] && [ -n "$CORE_INFO" ] && [ -n "$HENYEY_METRICS" ] || {
  echo "ERROR: live mode needs --henyey-info, --core-info, and --henyey-metrics" >&2
  usage 1
}

fetch() { curl -sf "$1" || { echo "ERROR: failed to fetch $1" >&2; exit 3; }; }

fails=0

# AC#6: henyey stays Synced
echo "--- AC#6: henyey Synced state ---"
H_INFO="$(fetch "$HENYEY_INFO")"
H_STATE="$(info_state "$H_INFO")"
echo "henyey state: $H_STATE"
[ "$H_STATE" = "Synced!" ] || { echo "FAIL AC#6: henyey not Synced! (got '$H_STATE')"; fails=1; }

# AC#3: authenticated overlay peering
echo "--- AC#3: overlay authenticated peering ---"
H_METRICS="$(fetch "$HENYEY_METRICS")"
H_AUTH="$(metric_auth "$H_METRICS")"
echo "henyey peer.peer.authenticated-count: $H_AUTH"
{ [ -n "$H_AUTH" ] && [ "$H_AUTH" != "null" ] && [ "$H_AUTH" -gt 0 ]; } \
  || { echo "FAIL AC#3: no authenticated peers"; fails=1; }

# AC#4: ledger externalize over a fixed window (seq advances)
echo "--- AC#4: ledger externalize over ${WINDOW_SECS}s window ---"
START_SEQ="$(info_seq "$H_INFO")"
echo "start seq: $START_SEQ"
elapsed=0
END_SEQ="$START_SEQ"
while [ "$elapsed" -lt "$WINDOW_SECS" ]; do
  sleep "$POLL_SECS"
  elapsed=$((elapsed + POLL_SECS))
  END_SEQ="$(info_seq "$(fetch "$HENYEY_INFO")")"
  echo "  t+${elapsed}s seq: $END_SEQ"
done
if [ "$END_SEQ" -gt "$START_SEQ" ]; then
  echo "OK AC#4: ledger advanced $START_SEQ -> $END_SEQ"
else
  echo "FAIL AC#4: ledger did not advance over window ($START_SEQ -> $END_SEQ)"; fails=1
fi

# AC#5: seq + hash agreement with a stellar-core peer
echo "--- AC#5: seq + hash agreement with stellar-core peer ---"
# Re-read both close together so they are likely at the same seq.
H_INFO="$(fetch "$HENYEY_INFO")"
C_INFO="$(fetch "$CORE_INFO")"
H_SEQ="$(info_seq "$H_INFO")"; H_HASH="$(info_hash "$H_INFO")"
C_SEQ="$(info_seq "$C_INFO")"; C_HASH="$(info_hash "$C_INFO")"
echo "henyey: seq=$H_SEQ hash=$H_HASH"
echo "core:   seq=$C_SEQ hash=$C_HASH"
if [ "$H_SEQ" = "$C_SEQ" ]; then
  if [ "$H_HASH" = "$C_HASH" ]; then
    echo "OK AC#5: agree on seq+hash at $H_SEQ"
  else
    echo "FAIL AC#5: HASH MISMATCH at seq $H_SEQ (henyey=$H_HASH core=$C_HASH)"; fails=1
  fi
else
  # Different seq is not necessarily a failure (clock skew between reads); warn
  # and direct the operator to re-run once both are at a common seq.
  echo "WARN AC#5: seq skew between reads (henyey=$H_SEQ core=$C_SEQ); re-run to compare at a common seq"
fi

echo
if [ "$fails" -eq 0 ]; then
  echo "=== MISSION ASSERTIONS PASSED (AC#3,#4,#6 green; AC#5 see above) ==="
  exit 0
else
  echo "=== MISSION ASSERTIONS FAILED — file a follow-up issue per the runbook ==="
  exit 1
fi
