#!/usr/bin/env bash
# Verify that Cargo.toml soroban-env-host rev pins match stellar-core
# submodule gitlinks. Requires the stellar-core submodule to be initialized
# (the nested soroban submodules do NOT need to be checked out).
#
# Usage: bash scripts/check-soroban-pins.sh
set -euo pipefail

if [ ! -d "stellar-core/.git" ] && [ ! -f "stellar-core/.git" ]; then
    echo "ERROR: stellar-core submodule not initialized."
    echo "Run: git submodule update --init stellar-core"
    exit 1
fi

errors=0
for proto in p24 p25 p26 p27; do
    # Extract recorded gitlink SHA from stellar-core's tree
    submodule_sha=$(git -C stellar-core ls-tree HEAD "src/rust/soroban/$proto" | awk '{print $3}')
    if [ -z "$submodule_sha" ]; then
        echo "ERROR: could not read gitlink for $proto from stellar-core"
        errors=$((errors + 1))
        continue
    fi

    # Extract rev from Cargo.toml by matching the soroban-env-host-$proto line
    cargo_rev=$(grep "soroban-env-host-$proto" Cargo.toml \
        | grep -oP 'rev\s*=\s*"\K[^"]+' || true)

    if [ -z "$cargo_rev" ]; then
        echo "ERROR: could not extract rev for soroban-env-host-$proto from Cargo.toml"
        errors=$((errors + 1))
        continue
    fi

    if [ "$submodule_sha" != "$cargo_rev" ]; then
        echo "MISMATCH: $proto"
        echo "  Cargo.toml rev: $cargo_rev"
        echo "  submodule SHA:  $submodule_sha"
        errors=$((errors + 1))
    else
        echo "OK: $proto — $cargo_rev"
    fi
done

if [ $errors -gt 0 ]; then
    echo ""
    echo "FAIL: $errors soroban pin(s) out of sync with stellar-core submodule."
    echo "Update Cargo.toml revs to match:"
    echo "  git -C stellar-core ls-tree HEAD src/rust/soroban/p24 src/rust/soroban/p25 src/rust/soroban/p26 src/rust/soroban/p27"
    exit 1
fi
echo ""
echo "All soroban pins match stellar-core submodule."
