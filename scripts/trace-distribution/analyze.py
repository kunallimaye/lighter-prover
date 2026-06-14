#!/usr/bin/env python3
"""analyze.py — extract the mainnet load distribution from banked traces (#128).

Pure analysis, no proving and no cloud spend. Computes the three load
characteristics that the JSONL trace format CAN answer:

  1. block-SIZE distribution  (count, non-null mean/median, p50/p90/p95/p99,
     min/max, null fraction)
  2. OUTLIER frequency        (fraction at the 500-tx chain cap, >cap anomalies)
  3. ARRIVAL-RATE / bursts    (inter-block gap median/p95/max, height-jump count
     + delta histogram, aggregate tx/s under the P1 mean-fill policy)

DATA-MODEL CAVEAT (read this): the trace format (bench/trace-format.md §2)
carries ONLY three fields per block event — {ts_ms, height, tx_count}. It has
NO tx-TYPE field. The tx-type MIX needed to scope #121 Phase 3 (#125, the
matching engine) is therefore NOT extractable from any trace. This script does
not and cannot produce a tx-type distribution; see the README and issue #128.

Determinism / dependencies: stdlib only, like feeder's offline subcommands, so
it runs in `make local-test` environments. The pure helpers are imported from
bench/feeder/feeder.py (the trace contract owner) rather than reimplemented, so
this analysis stays bit-for-bit consistent with how the feeder replays traces.

Usage:
    python3 analyze.py [TRACE_PATH]      # default: committed 201-line fixture
    python3 analyze.py --self-check      # assert fixture ground truth (§8.2)
    python3 analyze.py /path/to/trace_15m.jsonl   # e.g. a GCS-fetched trace
"""

import argparse
import json
import os
import statistics
import sys

# ──────────────────────────────────────────────────────────────────────
# Locate the repo and import feeder.py's pure helpers (no reimplementation).
# feeder.py's module-level imports are all stdlib; its network deps
# (websockets/requests) are imported lazily inside the network subcommands,
# so importing the module here is safe and dependency-free.
# ──────────────────────────────────────────────────────────────────────

_THIS_DIR = os.path.dirname(os.path.abspath(__file__))
_REPO_ROOT = os.path.abspath(os.path.join(_THIS_DIR, "..", ".."))
_FEEDER_DIR = os.path.join(_REPO_ROOT, "bench", "feeder")
if _FEEDER_DIR not in sys.path:
    sys.path.insert(0, _FEEDER_DIR)

import feeder  # noqa: E402  (path set up above)

# Re-exported for clarity; these are the contract owner's definitions.
BLOCK_TX_CAP = feeder.BLOCK_TX_CAP
DEFAULT_FIXTURE = os.path.join(
    _REPO_ROOT, "bench", "feeder", "fixtures", "trace_excerpt.jsonl")


# ──────────────────────────────────────────────────────────────────────
# Percentile helper (stdlib-only, deterministic).
# Uses the "nearest-rank" method on the sorted sample: p in [0,100].
# nearest-rank is stable, integer-indexed, and matches how operators
# usually read "the p95 block" (an actual observed value, not interpolated).
# ──────────────────────────────────────────────────────────────────────

def percentile(sorted_values, p):
    """Nearest-rank percentile of an already-sorted, non-empty list."""
    if not sorted_values:
        raise ValueError("percentile of empty list")
    if p <= 0:
        return sorted_values[0]
    if p >= 100:
        return sorted_values[-1]
    # nearest-rank: ceil(p/100 * N), 1-indexed
    rank = -(-p * len(sorted_values) // 100)  # ceil division
    idx = int(rank) - 1
    idx = max(0, min(idx, len(sorted_values) - 1))
    return sorted_values[idx]


# ──────────────────────────────────────────────────────────────────────
# Core analysis. Returns a plain dict so it is easy to assert in tests and
# to render as text. All numbers are computed from the trace events; nothing
# is invented.
# ──────────────────────────────────────────────────────────────────────

def analyze_trace(path):
    """Load a trace file and compute the load distribution.

    Returns a dict with three top-level sections: block_size, outliers,
    arrival_rate — plus meta (path, line/event counts, span).
    """
    with open(path) as f:
        header, events, gap_count, no_expand = feeder.load_trace(f)
    if not events:
        raise ValueError(f"trace {path!r} has no block events")
    feeder.validate_events(events)

    # ── block-SIZE distribution (observed, pre-expansion, non-null only) ──
    non_null = [e["tx_count"] for e in events if e["tx_count"] is not None]
    null_count = len(events) - len(non_null)
    non_null_sorted = sorted(non_null)

    block_size = {
        "blocks": len(events),
        "non_null_blocks": len(non_null),
        "null_blocks": null_count,
        "null_fraction": null_count / len(events),
        "mean_non_null": statistics.mean(non_null) if non_null else None,
        "median_non_null": statistics.median(non_null) if non_null else None,
        "min": non_null_sorted[0] if non_null else None,
        "max": non_null_sorted[-1] if non_null else None,
        "p50": percentile(non_null_sorted, 50) if non_null else None,
        "p90": percentile(non_null_sorted, 90) if non_null else None,
        "p95": percentile(non_null_sorted, 95) if non_null else None,
        "p99": percentile(non_null_sorted, 99) if non_null else None,
    }

    # ── OUTLIER frequency: blocks at / over the chain cap ──
    at_cap = sum(1 for v in non_null if v == BLOCK_TX_CAP)
    over_cap = sum(1 for v in non_null if v > BLOCK_TX_CAP)
    outliers = {
        "cap": BLOCK_TX_CAP,
        "at_cap_blocks": at_cap,
        "at_cap_fraction_of_non_null": (at_cap / len(non_null)
                                        if non_null else None),
        "at_cap_fraction_of_all": at_cap / len(events),
        "over_cap_blocks": over_cap,  # spec violation if > 0
    }

    # ── height jumps (bursts where Δ > 1) + delta histogram ──
    deltas = []
    jump_deltas = []
    prev_h = None
    for e in events:
        if prev_h is not None:
            d = e["height"] - prev_h
            deltas.append(d)
            if d > 1:
                jump_deltas.append(d)
        prev_h = e["height"]
    delta_histogram = {}
    for d in jump_deltas:
        delta_histogram[d] = delta_histogram.get(d, 0) + 1
    skipped_heights = sum(d - 1 for d in jump_deltas)

    # ── arrival rate: inter-block gaps + P1 aggregate tx/s ──
    # Gaps are computed on the OBSERVED events (real cadence). The aggregate
    # tx/s uses the feeder's full P2-expand -> P1-fill -> P4-round pipeline so
    # it matches the replayed throughput the fleet (#75) is sized against.
    raw_gaps = sorted(b["ts_ms"] - a["ts_ms"]
                      for a, b in zip(events, events[1:]))
    expanded = feeder.expand_and_fill(events, no_expand)
    agg_rate = feeder.aggregate_rate(expanded)
    span_s = (events[-1]["ts_ms"] - events[0]["ts_ms"]) / 1000.0

    arrival_rate = {
        "gap_median_ms": statistics.median(raw_gaps) if raw_gaps else 0.0,
        "gap_p95_ms": percentile(raw_gaps, 95) if raw_gaps else 0.0,
        "gap_max_ms": raw_gaps[-1] if raw_gaps else 0.0,
        "gap_min_ms": raw_gaps[0] if raw_gaps else 0.0,
        "jumps": len(jump_deltas),
        "jump_deltas": sorted(jump_deltas, reverse=True),
        "delta_histogram": dict(sorted(delta_histogram.items())),
        "max_jump_delta": max(jump_deltas) if jump_deltas else 0,
        "skipped_heights": skipped_heights,
        "skipped_fraction": (skipped_heights /
                             (events[-1]["height"] - events[0]["height"] + 1)),
        "aggregate_tx_per_s_p1": agg_rate,
        "expanded_blocks": len(expanded),
    }

    return {
        "meta": {
            "path": path,
            "has_header": header is not None,
            "gap_markers": gap_count,
            "height_first": events[0]["height"],
            "height_last": events[-1]["height"],
            "height_span": events[-1]["height"] - events[0]["height"] + 1,
            "ts_first_ms": events[0]["ts_ms"],
            "ts_last_ms": events[-1]["ts_ms"],
            "span_s": span_s,
        },
        "block_size": block_size,
        "outliers": outliers,
        "arrival_rate": arrival_rate,
    }


# ──────────────────────────────────────────────────────────────────────
# Rendering
# ──────────────────────────────────────────────────────────────────────

def _fmt(x, nd=2):
    if x is None:
        return "n/a"
    if isinstance(x, float):
        return f"{x:,.{nd}f}"
    return f"{x:,}"


def render(report):
    m, bs, ol, ar = (report["meta"], report["block_size"],
                     report["outliers"], report["arrival_rate"])
    lines = []
    lines.append("=" * 68)
    lines.append("TRACE LOAD DISTRIBUTION  (issue #128 — analysis only)")
    lines.append("=" * 68)
    lines.append(f"source            : {m['path']}")
    lines.append(f"provenance header : {'yes' if m['has_header'] else 'no (pre-spec/verbatim)'}")
    lines.append(f"blocks (events)   : {_fmt(bs['blocks'])}")
    lines.append(f"height range      : {_fmt(m['height_first'])} - {_fmt(m['height_last'])}"
                 f"  ({_fmt(m['height_span'])} heights spanned)")
    lines.append(f"time span         : {_fmt(m['span_s'])} s")
    lines.append(f"gap markers       : {_fmt(m['gap_markers'])}")
    lines.append("")
    lines.append("-- BLOCK-SIZE DISTRIBUTION (observed tx_count, non-null) --")
    lines.append(f"  non-null blocks : {_fmt(bs['non_null_blocks'])}  "
                 f"(null {_fmt(bs['null_blocks'])} = "
                 f"{_fmt(bs['null_fraction'] * 100, 2)}%)")
    lines.append(f"  mean (non-null) : {_fmt(bs['mean_non_null'])}")
    lines.append(f"  median          : {_fmt(bs['median_non_null'])}")
    lines.append(f"  min / max       : {_fmt(bs['min'])} / {_fmt(bs['max'])}")
    lines.append(f"  p50/p90/p95/p99 : {_fmt(bs['p50'])} / {_fmt(bs['p90'])}"
                 f" / {_fmt(bs['p95'])} / {_fmt(bs['p99'])}")
    lines.append("")
    lines.append("-- OUTLIER (large-block) FREQUENCY --")
    lines.append(f"  chain tx cap    : {_fmt(ol['cap'])}")
    lines.append(f"  blocks at cap   : {_fmt(ol['at_cap_blocks'])}  "
                 f"({_fmt(ol['at_cap_fraction_of_non_null'] * 100, 2)}% of "
                 f"non-null, {_fmt(ol['at_cap_fraction_of_all'] * 100, 2)}% of all)")
    lines.append(f"  blocks > cap    : {_fmt(ol['over_cap_blocks'])}"
                 f"{'  <-- SPEC VIOLATION' if ol['over_cap_blocks'] else ''}")
    lines.append("")
    lines.append("-- ARRIVAL-RATE / BURST DISTRIBUTION --")
    lines.append(f"  inter-block gap : median {_fmt(ar['gap_median_ms'])} ms / "
                 f"p95 {_fmt(ar['gap_p95_ms'])} ms / max {_fmt(ar['gap_max_ms'])} ms "
                 f"(min {_fmt(ar['gap_min_ms'])} ms)")
    lines.append(f"  height jumps    : {_fmt(ar['jumps'])}  "
                 f"(max Δ={_fmt(ar['max_jump_delta'])}, "
                 f"{_fmt(ar['skipped_heights'])} heights skipped = "
                 f"{_fmt(ar['skipped_fraction'] * 100, 2)}%)")
    lines.append(f"  jump Δ histogram: {ar['delta_histogram']}")
    lines.append(f"  aggregate rate  : {_fmt(ar['aggregate_tx_per_s_p1'])} tx/s "
                 f"(P1 mean-fill, post-expand; {_fmt(ar['expanded_blocks'])} blocks)")
    lines.append("")
    lines.append("-- TX-TYPE MIX --")
    lines.append("  NOT AVAILABLE: the trace format (§2) has no tx-type field.")
    lines.append("  The tx-MIX needed to scope #121 Phase 3 (#125 matching")
    lines.append("  engine) is not extractable from any trace. See README.")
    lines.append("=" * 68)
    return "\n".join(lines)


# ──────────────────────────────────────────────────────────────────────
# Self-check: assert the committed fixture matches trace-format.md §8.2.
# This both exercises analyze.py and pins it to documented ground truth.
# ──────────────────────────────────────────────────────────────────────

def self_check():
    report = analyze_trace(DEFAULT_FIXTURE)
    bs, ar, ol, m = (report["block_size"], report["arrival_rate"],
                     report["outliers"], report["meta"])
    failures = []

    def check(name, got, want):
        ok = got == want
        if not ok:
            failures.append(f"  FAIL {name}: got {got!r}, want {want!r}")
        else:
            print(f"  ok   {name}: {got}")

    def check_close(name, got, want, tol):
        ok = abs(got - want) <= tol
        if not ok:
            failures.append(
                f"  FAIL {name}: got {got!r}, want {want!r} (+/-{tol})")
        else:
            print(f"  ok   {name}: {got} (~= {want})")

    print("self-check against bench/trace-format.md §8.2 (committed fixture):")
    # §8.2 documented properties.
    check("lines/blocks", bs["blocks"], 201)
    check("height_first", m["height_first"], 260138266)
    check("height_last", m["height_last"], 260138493)
    check("height_span", m["height_span"], 228)
    check("null_blocks", bs["null_blocks"], 40)
    check_close("null_fraction_pct", bs["null_fraction"] * 100, 19.9, 0.1)
    check("min_non_null", bs["min"], 1)
    check("max_non_null", bs["max"], 500)
    check_close("mean_non_null", bs["mean_non_null"], 367.55, 0.01)
    check("jumps", ar["jumps"], 9)
    check("jump_deltas_desc", ar["jump_deltas"], [9, 4, 4, 4, 4, 4, 3, 2, 2])
    check("max_jump_delta", ar["max_jump_delta"], 9)
    check("skipped_heights", ar["skipped_heights"], 27)
    check_close("span_s", m["span_s"], 19.64, 0.01)
    check("over_cap_blocks", ol["over_cap_blocks"], 0)
    check("ts_first_ms", m["ts_first_ms"], 1781143874390)
    check("ts_last_ms", m["ts_last_ms"], 1781143894030)

    if failures:
        print("\nSELF-CHECK FAILED:")
        print("\n".join(failures))
        return 1
    print("\nSELF-CHECK PASSED: fixture reproduces trace-format.md §8.2.")
    return 0


# ──────────────────────────────────────────────────────────────────────
# CLI
# ──────────────────────────────────────────────────────────────────────

def main(argv=None):
    ap = argparse.ArgumentParser(
        description="Extract mainnet load distribution from a banked trace "
                    "(issue #128, analysis only).")
    ap.add_argument("trace", nargs="?", default=DEFAULT_FIXTURE,
                    help="path to a JSONL trace "
                         "(default: committed 201-line fixture)")
    ap.add_argument("--self-check", action="store_true",
                    help="assert the committed fixture matches §8.2 and exit")
    ap.add_argument("--json", action="store_true",
                    help="emit the raw report dict as JSON instead of text")
    args = ap.parse_args(argv)

    if args.self_check:
        return self_check()

    report = analyze_trace(args.trace)
    if args.json:
        print(json.dumps(report, indent=2, sort_keys=True))
    else:
        print(render(report))
    return 0


if __name__ == "__main__":
    sys.exit(main())
