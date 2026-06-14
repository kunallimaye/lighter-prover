#!/usr/bin/env python3
# Copyright (c) Elliot Technologies, Inc.
# SPDX-License-Identifier: BUSL-1.1
"""
Analyse a `BENCH_EVENT` JSONL stream produced by `bench --attribute-tx-type`
(or `--group-by-tx-type`) and report per-tx-type cost statistics for L1
(`BlockTxCircuit`) and L2 (`BlockTxChainCircuit`).

Filters `layer_prove` events that carry `chunk_tx_type_homogeneous` set
(i.e. every tx in the chunk shares the same `tx_type`). Boundary mixed
chunks are dropped from the per-type stats but counted separately.

Usage:
    python3 scripts/spike-tx-type-cost-analyze.py /tmp/spike-tx-type-cost.jsonl
"""

import json
import sys
import statistics
from collections import defaultdict

TX_TYPE_NAMES = {
    14: "TX_TYPE_L2_CREATE_ORDER",
    15: "TX_TYPE_L2_CANCEL_ORDER",
    17: "TX_TYPE_L2_MODIFY_ORDER",
    21: "TX_TYPE_INTERNAL_CLAIM_ORDER",
}


def per_type_stats(events, layer):
    """Group homogeneous layer_prove events by `chunk_tx_type_homogeneous`."""
    by_type = defaultdict(list)
    mixed = 0
    no_attribution = 0
    for e in events:
        if e.get("event") != "layer_prove" or e.get("layer") != layer:
            continue
        if "chunk_tx_type_homogeneous" not in e and "tx_types" not in e:
            no_attribution += 1
            continue
        if e.get("chunk_tx_type_homogeneous") is None:
            mixed += 1
            continue
        t = e["chunk_tx_type_homogeneous"]
        by_type[t].append(
            {
                "wall_ms": e["wall_ms"],
                "cpu_ms": e.get("cpu_ms"),
                "tx_per_proof": e["tx_per_proof"],
            }
        )
    return by_type, mixed, no_attribution


def summarize_layer(events, layer, layer_name):
    by_type, mixed, no_attr = per_type_stats(events, layer)
    if not by_type:
        print(f"\n=== Layer {layer} ({layer_name}): no attributed chunks ===")
        return None
    print(f"\n=== Layer {layer} ({layer_name}) ===")
    print(
        f"Mixed chunks (boundary, dropped): {mixed}; "
        f"chunks without attribution: {no_attr}"
    )
    print(
        f"{'type':>4}  {'name':<32}  {'n':>4}  {'wall_mean':>10}  "
        f"{'wall_min':>9}  {'wall_max':>9}  {'wall_std':>9}  "
        f"{'cpu_mean':>10}  {'per_tx_wall':>12}"
    )
    rows = []
    for t in sorted(by_type):
        s = by_type[t]
        wall = [r["wall_ms"] for r in s]
        cpu = [r["cpu_ms"] for r in s if r["cpu_ms"] is not None]
        tpp = s[0]["tx_per_proof"]
        wall_mean = statistics.mean(wall)
        wall_min = min(wall)
        wall_max = max(wall)
        wall_std = statistics.pstdev(wall) if len(wall) > 1 else 0.0
        cpu_mean = statistics.mean(cpu) if cpu else float("nan")
        per_tx_wall = wall_mean / tpp
        rows.append(
            {
                "type": t,
                "name": TX_TYPE_NAMES.get(t, f"unknown_{t}"),
                "n": len(wall),
                "wall_mean_ms": wall_mean,
                "wall_min_ms": wall_min,
                "wall_max_ms": wall_max,
                "wall_std_ms": wall_std,
                "cpu_mean_ms": cpu_mean,
                "tx_per_proof": tpp,
                "per_tx_wall_ms": per_tx_wall,
            }
        )
        print(
            f"{t:>4}  {TX_TYPE_NAMES.get(t, 'unknown'):<32}  {len(wall):>4}  "
            f"{wall_mean:>10.1f}  {wall_min:>9}  {wall_max:>9}  "
            f"{wall_std:>9.1f}  {cpu_mean:>10.1f}  {per_tx_wall:>12.1f}"
        )
    # Verdict math: max/min per-tx-wall ratio across types
    per_tx_values = [r["per_tx_wall_ms"] for r in rows]
    ratio = max(per_tx_values) / min(per_tx_values)
    spread_pct = (max(per_tx_values) - min(per_tx_values)) / min(per_tx_values) * 100.0
    print(
        f"\n  >>> Layer {layer} max/min per-tx wall ratio = {ratio:.3f} "
        f"(spread = {spread_pct:.1f}%)"
    )
    # Per-type coefficient of variation for honesty
    print("  Per-type CV (std/mean) for noise context:")
    for r in rows:
        cv = (r["wall_std_ms"] / r["wall_mean_ms"]) * 100.0
        print(
            f"    type {r['type']:>2}: CV = {cv:5.1f}%  "
            f"(n={r['n']}, mean={r['wall_mean_ms']:.1f}ms, std={r['wall_std_ms']:.1f}ms)"
        )
    return rows, ratio, spread_pct


def main():
    if len(sys.argv) != 2:
        print(__doc__, file=sys.stderr)
        sys.exit(2)
    events = [json.loads(line) for line in open(sys.argv[1])]
    print(f"Loaded {len(events)} events from {sys.argv[1]}")
    summarize_layer(events, layer=1, layer_name="BlockTxCircuit (per-chunk tx prove)")
    summarize_layer(
        events,
        layer=2,
        layer_name="BlockTxChainCircuit (per-chunk chain recursion)",
    )


if __name__ == "__main__":
    main()
