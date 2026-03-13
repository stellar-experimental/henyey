#!/usr/bin/env python3
"""
Analyze classic TX dependency structure from CDP ledger data to evaluate
cost/benefit of optimistic concurrent classic execution.

Uses union-find (O(n*k)) instead of O(n^2) pairwise comparison.

Usage:
    python3 scripts/analyze_classic_parallelism.py [cache_dir]
    Default cache_dir: ~/data/9283cc8b/cache/cdp/mainnet
"""

import sys
import struct
import collections
import io
import os
from dataclasses import dataclass, field

from stellar_sdk.xdr import LedgerCloseMeta, EnvelopeType, OperationType, AssetType

# ---- Union-Find ----

class UF:
    def __init__(self, n):
        self.p = list(range(n))
        self.rank = [0] * n
    def find(self, x):
        while self.p[x] != x:
            self.p[x] = self.p[self.p[x]]
            x = self.p[x]
        return x
    def union(self, a, b):
        a, b = self.find(a), self.find(b)
        if a == b: return
        if self.rank[a] < self.rank[b]: a, b = b, a
        self.p[b] = a
        if self.rank[a] == self.rank[b]: self.rank[a] += 1
    def clusters(self, n):
        return len(set(self.find(i) for i in range(n)))

# ---- Key extraction ----

SOROBAN_OP_TYPES = {
    OperationType.INVOKE_HOST_FUNCTION,
    OperationType.EXTEND_FOOTPRINT_TTL,
    OperationType.RESTORE_FOOTPRINT,
}

def is_soroban_op(op_type):
    return op_type in SOROBAN_OP_TYPES

def acct_key(ed25519_bytes):
    return ("A", bytes(ed25519_bytes))

def muxed_key(muxed):
    try:
        t = muxed.type
        tv = t.value if hasattr(t, 'value') else int(t)
        if tv == 256:  # KEY_TYPE_MUXED_ED25519
            return acct_key(muxed.med25519.ed25519.uint256)
        else:
            return acct_key(muxed.ed25519.uint256)
    except Exception:
        return ("A", b"\x00" * 32)

def asset_id(asset):
    try:
        at = asset.type
        atv = at.value if hasattr(at, 'value') else int(at)
        if atv == 0:  # NATIVE
            return b"XLM"
        elif atv == 1:  # ALPHANUM4
            return b"4:" + bytes(asset.alpha_num4.issuer.account_id.ed25519.uint256) + asset.alpha_num4.asset_code.asset_code4
        else:  # ALPHANUM12
            return b"12:" + bytes(asset.alpha_num12.issuer.account_id.ed25519.uint256) + asset.alpha_num12.asset_code.asset_code12
    except Exception:
        return b"?"

def tl_key(acct_ed25519, asset):
    return ("T", bytes(acct_ed25519), asset_id(asset))

def dex_key(sell_asset, buy_asset):
    # Both directions of a pair map to the same key (canonical order)
    a, b = asset_id(sell_asset), asset_id(buy_asset)
    return ("D", min(a, b), max(a, b))

def write_keys_for_tx(env):
    """
    Returns (write_keys: set, is_soroban: bool, op_types: list).
    write_keys is the minimal set of ledger keys this TX writes to.
    Any two TXs sharing a write key are considered dependent.
    """
    try:
        et = env.type
        etv = et.value if hasattr(et, 'value') else int(et)

        if etv == 5:  # FEE_BUMP
            fee_src_key = muxed_key(env.fee_bump.tx.fee_source)
            inner = env.fee_bump.tx.inner_tx.v1.tx
            source = inner.source_account
        elif etv == 2:  # TX v1 (ENVELOPE_TYPE_TX = 2)
            inner = env.v1.tx
            fee_src_key = muxed_key(inner.source_account)
            source = inner.source_account
        else:  # v0
            inner = env.v0.tx
            src_bytes = inner.source_account.account_id.ed25519.uint256
            fee_src_key = acct_key(src_bytes)
            source = inner.source_account

        ops = inner.operations
        op_names = [op.body.type.name for op in ops]
        is_sor = any(is_soroban_op(op.body.type) for op in ops)

        if is_sor:
            return {fee_src_key}, True, op_names

        wkeys = {fee_src_key}

        for op in ops:
            ot = op.body.type
            op_src = op.source_account if op.source_account else source

            # Get op source as ed25519 bytes
            try:
                op_src_t = op_src.type
                op_src_tv = op_src_t.value if hasattr(op_src_t, 'value') else int(op_src_t)
                if op_src_tv == 256:
                    op_ed = op_src.med25519.ed25519.uint256
                else:
                    op_ed = op_src.ed25519.uint256
            except Exception:
                op_ed = None

            if op_ed:
                wkeys.add(acct_key(op_ed))

            try:
                if ot == OperationType.PAYMENT:
                    b = op.body.payment_op
                    wkeys.add(muxed_key(b.destination))
                    if b.asset.type != AssetType.ASSET_TYPE_NATIVE and op_ed:
                        wkeys.add(tl_key(op_ed, b.asset))

                elif ot in (OperationType.PATH_PAYMENT_STRICT_RECEIVE,
                            OperationType.PATH_PAYMENT_STRICT_SEND):
                    if ot == OperationType.PATH_PAYMENT_STRICT_RECEIVE:
                        b = op.body.path_payment_strict_receive_op
                    else:
                        b = op.body.path_payment_strict_send_op
                    wkeys.add(muxed_key(b.destination))
                    if op_ed:
                        if b.send_asset.type != AssetType.ASSET_TYPE_NATIVE:
                            wkeys.add(tl_key(op_ed, b.send_asset))
                    # DEX pair dependency — any two TXs on same pair conflict
                    wkeys.add(dex_key(b.send_asset, b.dest_asset))

                elif ot in (OperationType.MANAGE_SELL_OFFER,
                            OperationType.MANAGE_BUY_OFFER,
                            OperationType.CREATE_PASSIVE_SELL_OFFER):
                    if ot == OperationType.MANAGE_SELL_OFFER:
                        b = op.body.manage_sell_offer_op
                    elif ot == OperationType.MANAGE_BUY_OFFER:
                        b = op.body.manage_buy_offer_op
                    else:
                        b = op.body.create_passive_sell_offer_op
                    wkeys.add(dex_key(b.selling, b.buying))
                    if op_ed:
                        if b.selling.type != AssetType.ASSET_TYPE_NATIVE:
                            wkeys.add(tl_key(op_ed, b.selling))
                        if b.buying.type != AssetType.ASSET_TYPE_NATIVE:
                            wkeys.add(tl_key(op_ed, b.buying))

                elif ot == OperationType.CREATE_ACCOUNT:
                    wkeys.add(acct_key(op.body.create_account_op.destination.account_id.ed25519.uint256))

                elif ot == OperationType.ACCOUNT_MERGE:
                    wkeys.add(muxed_key(op.body.account_merge_op.destination))

                elif ot == OperationType.CHANGE_TRUST:
                    line = op.body.change_trust_op.line
                    asset = line.asset if hasattr(line, 'asset') else line
                    if op_ed:
                        wkeys.add(tl_key(op_ed, asset))

                elif ot == OperationType.ALLOW_TRUST:
                    wkeys.add(acct_key(op.body.allow_trust_op.trustor.account_id.ed25519.uint256))

                elif ot == OperationType.CLAIM_CLAIMABLE_BALANCE:
                    bid = op.body.claim_claimable_balance_op.balance_id.v0.uint256
                    wkeys.add(("CB", bytes(bid)))

                elif ot == OperationType.CLAWBACK_CLAIMABLE_BALANCE:
                    bid = op.body.clawback_claimable_balance_op.balance_id.v0.uint256
                    wkeys.add(("CB", bytes(bid)))

                elif ot == OperationType.LIQUIDITY_POOL_DEPOSIT:
                    pid = op.body.liquidity_pool_deposit_op.liquidity_pool_id.pool_id.uint256
                    wkeys.add(("LP", bytes(pid)))

                elif ot == OperationType.LIQUIDITY_POOL_WITHDRAW:
                    pid = op.body.liquidity_pool_withdraw_op.liquidity_pool_id.pool_id.uint256
                    wkeys.add(("LP", bytes(pid)))

            except Exception:
                pass  # op extraction failed; source account key already added

        return wkeys, False, op_names

    except Exception:
        return set(), False, []

# ---- Ledger analysis ----

def analyze_ledger(meta_bytes):
    try:
        meta = LedgerCloseMeta.from_xdr_bytes(meta_bytes)
    except Exception:
        return None

    try:
        v = meta.v
        if v == 2:
            seq = meta.v2.ledger_header.header.ledger_seq.uint32
            tx_set_obj = meta.v2.tx_set
        elif v == 1:
            seq = meta.v1.ledger_header.header.ledger_seq.uint32
            tx_set_obj = meta.v1.tx_set
        else:
            return None
    except Exception:
        return None

    # Decode phases: phase 0 = classic, phase 1 = Soroban
    classic_envs = []
    soroban_count = 0
    try:
        phases = tx_set_obj.v1_tx_set.phases
        for phase in phases:
            if phase.v0_components:
                for comp in phase.v0_components:
                    classic_envs.extend(comp.txs_maybe_discounted_fee.txs)
            elif phase.parallel_txs_component:
                ptc = phase.parallel_txs_component
                if hasattr(ptc, 'execution_stages') and ptc.execution_stages:
                    for stage in ptc.execution_stages:
                        for cluster in stage.parallel_tx_execution_stage:
                            soroban_count += len(cluster.dependent_tx_cluster)
    except Exception:
        return None

    # Extract write key sets for each classic TX
    op_type_counts = collections.Counter()
    tx_wkeys = []
    dex_tx_count = 0

    for env in classic_envs:
        wkeys, is_sor, op_names = write_keys_for_tx(env)
        for n in op_names:
            op_type_counts[n] += 1
        if is_sor:
            soroban_count += 1
            continue
        has_dex = any(k[0] == "D" for k in wkeys)
        if has_dex:
            dex_tx_count += 1
        tx_wkeys.append(wkeys)

    n = len(tx_wkeys)
    if n == 0:
        return None

    # Union-Find: for each write key, union all TXs that share it
    uf = UF(n)
    key_to_first_tx = {}
    for i, wkeys in enumerate(tx_wkeys):
        for k in wkeys:
            if k in key_to_first_tx:
                uf.union(key_to_first_tx[k], i)
            else:
                key_to_first_tx[k] = i

    num_clusters = uf.clusters(n)

    # Find largest cluster size
    root_counts = collections.Counter(uf.find(i) for i in range(n))
    max_cluster = max(root_counts.values())

    return {
        "seq": seq,
        "classic": n,
        "soroban": soroban_count,
        "clusters": num_clusters,
        "max_cluster": max_cluster,
        "dex_txs": dex_tx_count,
        "ops": dict(op_type_counts),
    }

# ---- CDP file reader ----

def read_zst_xdr_files(directory):
    import zstandard as zstd
    dctx = zstd.ZstdDecompressor()
    files = sorted(
        [f for f in os.listdir(directory) if f.endswith(".xdr.zst")],
        key=lambda f: int(f.split(".")[0])
    )
    for fname in files:
        path = os.path.join(directory, fname)
        with open(path, "rb") as fh:
            compressed = fh.read()
        try:
            raw = dctx.stream_reader(io.BytesIO(compressed)).read()
        except Exception:
            continue
        # CDP cache format: 12-byte prefix + LedgerCloseMeta XDR
        if len(raw) > 12:
            yield raw[12:]

# ---- Report ----

def print_report(results):
    results = [r for r in results if r and r["classic"] > 0]
    if not results:
        print("No classic ledgers found.")
        return

    n = len(results)
    total_classic = sum(r["classic"] for r in results)
    total_soroban = sum(r["soroban"] for r in results)

    avg_classic = total_classic / n
    avg_soroban = total_soroban / n
    classic_frac = total_classic / (total_classic + total_soroban) * 100

    avg_clusters = sum(r["clusters"] for r in results) / n
    avg_pf = sum(r["classic"] / r["clusters"] for r in results) / n
    avg_lc_frac = sum(r["max_cluster"] / r["classic"] for r in results) / n
    avg_dex_frac = sum(r["dex_txs"] / r["classic"] for r in results) / n

    serial_fallback = sum(1 for r in results if r["max_cluster"] / r["classic"] > 0.80)

    # Parallelism factor distribution
    pf_hist = collections.Counter()
    for r in results:
        pf = r["classic"] / r["clusters"]
        bucket = int(pf * 2) / 2  # round to 0.5
        pf_hist[bucket] += 1

    lc_hist = collections.Counter()
    for r in results:
        frac = r["max_cluster"] / r["classic"]
        bucket = int(frac * 10) / 10
        lc_hist[bucket] += 1

    # Op type totals
    all_ops = collections.Counter()
    for r in results:
        for k, v in r["ops"].items():
            all_ops[k] += v
    total_ops = sum(all_ops.values())

    print(f"{'='*62}")
    print(f"Classic Parallelism Analysis — {n} ledgers (mainnet p25)")
    print(f"{'='*62}")

    print(f"\n--- TX Composition ---")
    print(f"  Avg classic TXs/ledger:   {avg_classic:.1f}")
    print(f"  Avg Soroban TXs/ledger:   {avg_soroban:.1f}")
    print(f"  Classic share:            {classic_frac:.1f}%")

    print(f"\n--- Classic Dependency Structure ---")
    print(f"  Avg independent clusters: {avg_clusters:.1f}")
    print(f"  Avg parallelism factor:   {avg_pf:.2f}x  (TXs / clusters)")
    print(f"  Avg largest cluster:      {avg_lc_frac*100:.1f}%  of classic TXs")
    print(f"  DEX-touching TXs:         {avg_dex_frac*100:.1f}%  of classic TXs")
    print(f"  Serial fallback (>80% in one cluster): {serial_fallback}/{n} ({serial_fallback/n*100:.1f}%)")

    print(f"\n--- Parallelism Factor Distribution ---")
    for pf in sorted(pf_hist):
        bar = "#" * max(1, pf_hist[pf] * 50 // n)
        print(f"  {pf:4.1f}x: {pf_hist[pf]:4d}  {bar}")

    print(f"\n--- Largest Cluster Fraction Distribution ---")
    for frac in sorted(lc_hist):
        bar = "#" * max(1, lc_hist[frac] * 50 // n)
        print(f"  {frac*100:3.0f}%+: {lc_hist[frac]:4d}  {bar}")

    print(f"\n--- Classic Operation Types ---")
    for op, count in sorted(all_ops.items(), key=lambda x: -x[1])[:15]:
        pct = count / total_ops * 100
        print(f"  {op:<42} {count:8d}  ({pct:.1f}%)")

    print(f"\n--- Cost/Benefit Assessment ---")
    classic_ms = 95.8
    for cores in [2, 4]:
        speedup = min(avg_pf, cores)
        saving = classic_ms * (1.0 - 1.0 / speedup) if speedup > 1 else 0.0
        print(f"  {cores} cores: speedup={speedup:.2f}x → saves ~{saving:.0f}ms of {classic_ms:.0f}ms classic phase")

    overhead_est = 3.0  # cluster setup, merge, conflict detection
    net_2c = min(avg_pf, 2)
    net_saving = classic_ms * (1 - 1/net_2c) - overhead_est if net_2c > 1 else 0
    print(f"  Est. net saving (2 cores, minus ~{overhead_est:.0f}ms overhead): ~{max(0,net_saving):.0f}ms")
    print(f"  Serial fallback rate: {serial_fallback/n*100:.1f}% of ledgers would not benefit")

    print()

# ---- Main ----

if __name__ == "__main__":
    cache_dir = sys.argv[1] if len(sys.argv) > 1 else os.path.expanduser("~/data/9283cc8b/cache/cdp/mainnet")

    results = []
    count = 0
    for frame in read_zst_xdr_files(cache_dir):
        r = analyze_ledger(frame)
        if r:
            results.append(r)
            count += 1
            if count % 100 == 0:
                print(f"  ... {count} ledgers", file=sys.stderr)

    print_report(results)
