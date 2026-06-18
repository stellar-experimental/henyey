#!/usr/bin/env bash
#
# launch-henyey-mixed-mission.sh — stand up the nsc-side launch surface for
# the Henyey mixed-image Supercluster (SSC) mission.
#
# This is a THIN WRAPPER over the verified `nsc` commands documented in
# docs/supercluster-nsc-workflow.md: auth smoke check -> build+push
# the henyey image -> capture the image digest -> `nsc create` an ephemeral
# k8s instance -> write the run-dir layout -> print the exact SSC dotnet
# invocation to hand off.
#
# IMPORTANT — this script does NOT run the mission. The actual mission RUN is
# performed by the external `stellar/supercluster` dotnet harness (NOT vendored
# here) against the published image + the instance kubeconfig. This script gets you up
# to the hand-off point and prints the exact SSC invocation.
#
# Modes:
#   --dry-run   Assemble and print every command + the run-dir layout WITHOUT
#               invoking `nsc` or touching live infra. CI / the harness runs
#               this to exercise the command-assembly logic with no auth.
#   (default)   Execute the nsc-side steps for real (requires `nsc login`).
#
# Usage:
#   scripts/ssc/launch-henyey-mixed-mission.sh [--dry-run] [--registry REG]
#                                      [--image-tag TAG] [--duration DUR]
#                                      [--runs-dir DIR]
#
set -euo pipefail

# --- Defaults (verified values from docs/supercluster-nsc-workflow.md) -------
REGISTRY="nscr.io/k4jkul01t5rr0"      # SDF workspace registry (substitute your tenant)
IMAGE_NAME="henyey"
IMAGE_TAG="ssc"
DURATION="2h"                          # ephemeral instance lifetime
K8S_VERSION="1.33"
RUNS_DIR=""                            # default derived below from date
DRY_RUN=0

usage() {
  sed -n '2,40p' "$0" | sed 's/^# \{0,1\}//'
  exit "${1:-0}"
}

while [ $# -gt 0 ]; do
  case "$1" in
    --dry-run) DRY_RUN=1; shift ;;
    --registry) REGISTRY="$2"; shift 2 ;;
    --image-tag) IMAGE_TAG="$2"; shift 2 ;;
    --duration) DURATION="$2"; shift 2 ;;
    --runs-dir) RUNS_DIR="$2"; shift 2 ;;
    -h|--help) usage 0 ;;
    *) echo "Unknown argument: $1" >&2; usage 1 ;;
  esac
done

IMAGE_REF="${REGISTRY}/${IMAGE_NAME}:${IMAGE_TAG}"
DATE_TAG="$(date +%Y%m%d)"
[ -n "$RUNS_DIR" ] || RUNS_DIR="runs/${DATE_TAG}-henyey-mixed-mission"

# `run` echoes the command; in --dry-run it does NOT execute it.
run() {
  echo "+ $*"
  if [ "$DRY_RUN" -eq 0 ]; then
    "$@"
  fi
}

echo "=== Henyey mixed-image SSC launch (nsc side) ==="
echo "registry:   $REGISTRY"
echo "image ref:  $IMAGE_REF"
echo "duration:   $DURATION"
echo "k8s:        $K8S_VERSION"
echo "run dir:    $RUNS_DIR"
if [ "$DRY_RUN" -eq 1 ]; then
  echo "MODE:       DRY RUN (no nsc calls, no live infra)"
else
  echo "MODE:       LIVE (requires prior 'nsc login')"
fi
echo

# --- Run-dir layout (docs/supercluster-nsc-workflow.md §4) -------------------
# In dry-run we still create the layout so CI can assert it; the directory is
# harmless and lives wherever the operator/CI invokes the script.
echo "--- 0. Run directory layout ---"
run mkdir -p "$RUNS_DIR/logs" "$RUNS_DIR/ssc"
echo

# --- 1. Auth smoke check (#3304 §1) ------------------------------------------
echo "--- 1. Auth smoke check ---"
run nsc auth check-login
run nsc workspace describe
echo

# --- 2. Build + publish + capture digest (#3304 §2) --------------------------
echo "--- 2. Build & publish the henyey image ---"
run nsc build -f Dockerfile --platform linux/amd64 --push -n "$IMAGE_REF" .
echo "--- 2b. Capture immutable image digest ---"
DIGEST_FILE="$RUNS_DIR/image-digest.txt"
if [ "$DRY_RUN" -eq 1 ]; then
  echo "+ nsc registry describe $IMAGE_REF -o json > $DIGEST_FILE"
  IMAGE_DIGEST="sha256:<captured-at-runtime>"
else
  nsc registry describe "$IMAGE_REF" -o json > "$DIGEST_FILE"
  IMAGE_DIGEST="$(grep -o '"digest":[^,]*' "$DIGEST_FILE" | head -1 | sed 's/.*"\(sha256:[a-f0-9]*\)".*/\1/')"
fi
IMAGE_PINNED="${REGISTRY}/${IMAGE_NAME}@${IMAGE_DIGEST}"
echo "image (pinned by digest): $IMAGE_PINNED"
echo

# --- 3. Provision the ephemeral k8s instance (#3304 §3a) ---------------------
echo "--- 3. Provision ephemeral k8s instance (nsc side) ---"
INSTANCE_JSON="$RUNS_DIR/instance.json"
run nsc create --ephemeral \
  --enable="kubernetes:${K8S_VERSION}" \
  --duration="$DURATION" \
  --output_json_to="$INSTANCE_JSON" \
  --purpose="SSC Henyey mixed-image mission"
echo

# --- 4. Hand off to the SSC dotnet harness (NOT run here) ---------------
echo "--- 4. Hand off to SSC dotnet harness (operator-executed, NOT run here) ---"
cat <<HANDOFF
The mission RUN is performed by the external stellar/supercluster dotnet
harness (not vendored in this repo). Point it at the published image (pin by
DIGEST for reproducibility) and the instance kubeconfig:

  nsc kubeconfig write <instance-id>   # write a kubeconfig for the instance
  # then, from the stellar/supercluster checkout with PR #400 or equivalent:
  dotnet run --project src/App/App.fsproj --configuration Release -- mission \\
      MixedImageLoadGenerationWithOldImageMajority \\
      --kubeconfig <path-from-nsc-kubeconfig-write> \\
      --namespace default \\
      --destination ${RUNS_DIR}/ssc \\
      --keep-data \\
      --core-http-via-pod-exec \\
      --image=${IMAGE_PINNED} \\
      --old-image=stellar/stellar-core:latest \\
      --probe-timeout 240 \\
      --tx-rate 5 \\
      --num-txs 100 \\
      --num-accounts 100 \\
      --genesis-test-account-count 100

In this mission, --image is Henyey and --old-image is stellar-core.
The mission creates a 2-node stellar-core old-image majority and a 1-node
Henyey new-image minority.

Once the network is up, validate it with:
  scripts/ssc/assert-henyey-mixed-mission.sh \\
      --henyey-info http://<henyey-pod>:11626/info \\
      --core-info   http://<core-pod>:11626/info \\
      --henyey-metrics http://<henyey-pod>:11626/metrics

Then capture artifacts into ${RUNS_DIR}/ (see the runbook §"Artifacts").
HANDOFF
echo

echo "=== nsc-side launch surface ready (mission RUN handed to operator/SSC) ==="
