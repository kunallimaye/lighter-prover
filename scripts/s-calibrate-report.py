#!/usr/bin/env python3
"""s-calibrate-report.py -- objective computation for the per-machine
chunk-size calibration suite (issue #85; SLO objective + registry: #102).

Reads BENCH_EVENT JSONL probe outputs (cal-S<N>.jsonl) from --out-dir,
computes per-S metrics and the four objectives, and writes:

  calibration.tsv   columns: S bracket l1_wall_ms l2_wall_ms peak_rss_mb
                    s_per_tx serial_block_s tree_block_s feasible label
                    l1_n l1_stdev_ms l1_quality full_split_wall_<B>...
                    slo_slack_min slo_verdict
                    (v2 columns are ADDITIVE -- the first ten never move)
  report.md         human-readable summary + per-objective recommendations
  ledger.md         BENCH-LEDGER entry (Discussion #77 comment template)
  <shape>.json      (only with --out-registry DIR) machine-readable
                    calibration registry entry + re-rendered README.md

Objectives (issue #60 / PR #69 methodology, BLOCK_TX-tx block):
  serial_block_s = max(L1_chunk_wall(S), (BLOCK_TX/S) * L2_step(S))
  tree_block_s   = L1_chunk_wall(S) + ceil(log2(BLOCK_TX/S)) * merge_s
                   (merge_s = measured mean when the probe emitted
                   BlockTxChainMergeCircuit events, else the --merge-s
                   constant -> objective labeled "extrapolated")
  s_per_tx       = L1_chunk_wall(S) / S

Objective 4 (issue #102; proof-lag SLO from Discussion #77, always-split
policy, scoped to block-proof-ready L1->L4):
  full_split_wall(S, B) = L1_chunk_wall(S) + merge_depth(B, S) * MERGE_S
                          + L4_WALL
  merge_depth(B, S)     = ceil(log2(ceil(B / S)))     (see merge_depth())
  slack(S, B)           = LAG_P50 - full_split_wall(S, B)
  verdict: FEASIBLE / MARGINAL (slack < 2 s) / INFEASIBLE (slack < 0)
  recommended S = argmax over S of min-over-B slack(S, B)

Constants provenance (issue #102 encoding 3): MERGE_S / L4_WALL are
"measured" when this machine ran the opt-in CAL_L4=1 step (or probes
emitted merge / l4_check events); otherwise the Phase A reference
constants are used, scaled by r = L1(S=20, here) / L1(S=20, reference)
and labeled "extrapolated", with BOTH the scaled and unscaled L4
variants reported as an interval (the winner is insensitive to the
choice; headroom is not).

This script is intentionally the single code path for local runs
(scripts/s-calibrate.sh), fleet collection (run-fleet.sh collect on a
calibration run), the golden-fixture tests
(scripts/bench-fleet/tests/test-calibrate.sh, test-slo-objective.sh),
and registry seeding.
"""

import argparse
import glob
import json
import math
import os
import re
import statistics
import subprocess
import sys
from datetime import date

# The first ten columns are the frozen v1 schema (issue #85) -- never
# reorder or rename them. v2 columns (issue #102) are appended after.
TSV_COLUMNS_V1 = [
    "S", "bracket", "l1_wall_ms", "l2_wall_ms", "peak_rss_mb",
    "s_per_tx", "serial_block_s", "tree_block_s", "feasible", "label",
]
TSV_COLUMNS_V2 = ["l1_n", "l1_stdev_ms", "l1_quality"]
# full_split_wall_<B> (one per --block-sizes entry), slo_slack_min, and
# slo_verdict are appended dynamically in main().

# Verdict thresholds (issue #102): slack < 0 s -> INFEASIBLE,
# slack < MARGINAL_SLACK_S -> MARGINAL, else FEASIBLE.
MARGINAL_SLACK_S = 2.0

# Purpose preamble (issue #102 scope amendment) -- embedded VERBATIM in
# both report.md and calibration/README.md.
PURPOSE_PREAMBLE = """\
The five questions this suite answers: (1) optimal S on an unmeasured
shape; (2) did a circuit change move the optimum; (3) can we trust this
row (n/stdev/load-quality); (4) what S should this worker run
(machine-consumable artifact, future boot-time self-config); (5) is S=9
still the winner with measured per-shape merge/L4 (Phase C, via this
suite)."""

# Bracket bands from issue #60. Edge values (9..11, 21) are unsettled --
# inferred from measured neighbours when possible (see infer_bracket).
# Phase A of issue #102 measured the 2^19 band extending through S=40
# (L1(S=40) ~ L1(S=32) on every measured shape), so HIGH_HI is 40 -- the
# "40-as-measured-cap" bracket top; 2^20 starts above it.
LOWER_TOP = 8       # validated top of 2^17
MID_LO, MID_HI = 12, 20   # validated 2^18 band
HIGH_LO, HIGH_HI = 22, 40  # measured 2^19 band (top measured at S=40)


def projected_bracket(s: int) -> str:
    if s <= LOWER_TOP:
        return "2^17"
    if s <= 11:
        return "2^17|2^18?"
    if s <= MID_HI:
        return "2^18"
    if s == 21:
        return "2^18|2^19?"
    if s <= HIGH_HI:
        return "2^19"
    return "2^20"


def merge_depth(block_tx: int, s: int) -> int:
    """Number of pairwise merge levels needed to fold ceil(B/S) leaf
    chain proofs into one root proof.

    POLICY-DEPENDENT -- this is the SINGLE site encoding the always-split
    policy's merge arity (pairwise / 2-ary, today's
    BlockTxChainMergeCircuit). If merges ever go >2-ary, change the log
    base here and nowhere else.
    """
    chunks = math.ceil(block_tx / s)
    return math.ceil(math.log2(chunks)) if chunks > 1 else 0


def slo_verdict_of(slack: float) -> str:
    if slack < 0:
        return "INFEASIBLE"
    if slack < MARGINAL_SLACK_S:
        return "MARGINAL"
    return "FEASIBLE"


def l1_quality_of(n: int, stdev_ms: float, mean_ms: float) -> str:
    """Data-quality flag (issue #102 encoding 1): low_n when fewer than
    3 steady samples back the mean; noisy when stdev/mean > 10%."""
    flags = []
    if n < 3:
        flags.append("low_n")
    if mean_ms > 0 and (stdev_ms / mean_ms) > 0.10:
        flags.append("noisy")
    return "+".join(flags) if flags else "ok"


def parse_probe(path: str):
    """Parse one cal-S<N>.jsonl. Returns dict with l1_ms, l1_n,
    l1_stdev_ms, l2_ms, merge_ms (or None), l4_split_s (or None),
    peak_rss_mb (or None), chunks -- or None when the probe produced no
    usable L1 events (failed run). bench.jsonl (BENCH_EVENT stream) is
    the single source of truth -- no calibration.tsv is ever assumed."""
    l1, l2, merges, summary_rss, event_rss = {}, {}, [], None, []
    l4_split_s = None
    with open(path) as fh:
        for line in fh:
            line = line.strip()
            if not line:
                continue
            try:
                ev = json.loads(line)
            except json.JSONDecodeError:
                continue
            kind = ev.get("event")
            if kind == "layer_prove":
                if ev.get("rss_mb_peak") is not None:
                    event_rss.append(ev["rss_mb_peak"])
                name = ev.get("name")
                idx = ev.get("chunk_idx")
                if name == "BlockTxCircuit" and idx is not None:
                    l1[idx] = ev["wall_ms"]
                elif name == "BlockTxChainCircuit" and idx is not None:
                    l2[idx] = ev["wall_ms"]
                elif name == "BlockTxChainMergeCircuit":
                    merges.append(ev["wall_ms"])
            elif kind == "l4_check":
                # Issue #102: per-block L4 wall = prove + verify (the
                # one-time build cost is deliberately excluded).
                try:
                    l4_split_s = (ev["l4_prove_ms"] + ev["l4_verify_ms"]) / 1000.0
                except (KeyError, TypeError):
                    pass
            elif kind == "summary":
                summary_rss = ev.get("peak_rss_mb")
    if not l1:
        return None

    def steady_vals(by_idx):
        # Chunk 0 is excluded as warm-up when more than one chunk was
        # measured: circuit build time is already separated into
        # circuit_define events, but the first prove still pays cold
        # caches / first-touch page faults.
        return [v for k, v in by_idx.items() if k != 0] or list(by_idx.values())

    def steady_mean(by_idx):
        vals = steady_vals(by_idx)
        return sum(vals) / len(vals)

    l1_steady = steady_vals(l1)
    return {
        "l1_ms": steady_mean(l1),
        "l1_n": len(l1_steady),
        "l1_stdev_ms": statistics.stdev(l1_steady) if len(l1_steady) > 1 else 0.0,
        "l2_ms": steady_mean(l2) if l2 else None,
        "merge_ms": (sum(merges) / len(merges)) if merges else None,
        "l4_split_s": l4_split_s,
        "peak_rss_mb": summary_rss if summary_rss is not None
                       else (max(event_rss) if event_rss else None),
        "chunks": len(l1),
    }


def parse_cal_l4(out_dir: str):
    """Parse the opt-in CAL_L4=1 probe output (cal-l4check.jsonl) when
    present: returns (merge_s, l4_wall_s) in seconds, either may be None.
    This is the per-machine measured-constants path (issue #102
    encoding 3)."""
    path = os.path.join(out_dir, "cal-l4check.jsonl")
    if not os.path.exists(path):
        return None, None
    merges, l4 = [], None
    with open(path) as fh:
        for line in fh:
            line = line.strip()
            if not line:
                continue
            try:
                ev = json.loads(line)
            except json.JSONDecodeError:
                continue
            kind = ev.get("event")
            if kind == "layer_prove" and ev.get("name") == "BlockTxChainMergeCircuit":
                merges.append(ev["wall_ms"])
            elif kind == "l4_check":
                try:
                    l4 = (ev["l4_prove_ms"] + ev["l4_verify_ms"]) / 1000.0
                except (KeyError, TypeError):
                    pass
    merge_s = (sum(merges) / len(merges)) / 1000.0 if merges else None
    return merge_s, l4


def infer_bracket(s: int, rows: dict) -> str:
    """Settle edge probes (S in 9..11, S=21) by comparing the measured
    L1 wall against the nearest measured anchor in each adjacent
    validated band. The bracket step is ~2x, so 'closer in log space'
    is a robust discriminator."""
    proj = projected_bracket(s)
    if "?" not in proj or s not in rows or rows[s] is None:
        return proj
    if s <= 11:
        lo_band = [v for v in range(1, LOWER_TOP + 1)]
        hi_band = list(range(MID_LO, MID_HI + 1))
        lo_name, hi_name = "2^17", "2^18"
    else:  # s == 21
        lo_band = list(range(MID_LO, MID_HI + 1))
        hi_band = list(range(HIGH_LO, HIGH_HI + 1))
        lo_name, hi_name = "2^18", "2^19"
    lo_anchor = [rows[a]["l1_ms"] for a in lo_band if rows.get(a)]
    hi_anchor = [rows[a]["l1_ms"] for a in hi_band if rows.get(a)]
    if not lo_anchor or not hi_anchor:
        return proj
    l1 = rows[s]["l1_ms"]
    d_lo = abs(math.log(l1 / max(lo_anchor)))
    d_hi = abs(math.log(l1 / min(hi_anchor)))
    return lo_name if d_lo < d_hi else hi_name


def confidence(best, runner_up):
    """Margin-based confidence label for a recommendation."""
    if runner_up is None or best == 0:
        return "high (no contender)"
    margin = (runner_up - best) / best
    if margin > 0.10:
        return f"high (next best +{margin:.0%})"
    if margin > 0.03:
        return f"medium (next best +{margin:.0%})"
    return f"low (next best +{margin:.1%} -- within noise)"


def slack_confidence(best_slack, runner_slack):
    """Margin-based confidence for the SLO-slack recommendation (a
    maximization in seconds, unlike the minimizations above)."""
    if runner_slack is None:
        return "high (no contender)"
    margin = best_slack - runner_slack
    if margin > 0.5:
        return f"high (next best -{margin:.2f} s)"
    if margin > 0.1:
        return f"medium (next best -{margin:.2f} s)"
    return f"low (next best -{margin:.2f} s -- within noise)"


def parse_machine_info(out_dir: str):
    cores, ram = None, None
    path = os.path.join(out_dir, "machine-info.txt")
    if os.path.exists(path):
        with open(path) as fh:
            for line in fh:
                m = re.match(r"^CPU\(s\):\s+(\d+)", line)
                if m and cores is None:
                    cores = m.group(1)
                m = re.match(r"^Mem:\s+(\S+)", line)
                if m and ram is None:
                    ram = m.group(1)
    return cores or "NA", ram or "NA"


def parse_load_quality(out_dir: str):
    """Read the load-quality verdict recorded by s-calibrate.sh into
    machine-info.txt ('=== loadavg ===' section). Returns
    clean|loaded|unknown."""
    path = os.path.join(out_dir, "machine-info.txt")
    if os.path.exists(path):
        with open(path) as fh:
            content = fh.read()
        m = re.search(r"load_quality:\s*(clean|loaded)", content)
        if m:
            return m.group(1)
    return "unknown"


def git_sha(explicit):
    if explicit:
        return explicit
    try:
        return subprocess.run(
            ["git", "rev-parse", "--short", "HEAD"],
            capture_output=True, text=True, check=True,
        ).stdout.strip()
    except Exception:
        return "unknown"


def fmt(v, nd=3):
    return "NA" if v is None else (f"{v:.{nd}f}" if isinstance(v, float) else str(v))


def render_registry_readme(registry_dir: str):
    """Re-render calibration/README.md from every registry JSON present.
    Called after each --out-registry emission so the human table always
    matches the machine-readable entries."""
    entries = []
    for path in sorted(glob.glob(os.path.join(registry_dir, "*.json"))):
        try:
            with open(path) as fh:
                entries.append(json.load(fh))
        except (OSError, json.JSONDecodeError):
            continue

    lines = [
        "# Calibration registry",
        "",
        "Machine-readable chunk-size calibration results, one JSON per",
        "shape, emitted by the s-calibrate suite (issues #85 / #102).",
        "Calibration validity is tied to the circuit code it measured, so",
        "results live in-repo, versioned with that code: **recalibration =",
        "a PR that diffs these files**, making \"did this circuit change",
        "move the optimum?\" a code-review diff.",
        "",
        "## Purpose",
        "",
        PURPOSE_PREAMBLE,
        "",
        "## Current recommendations",
        "",
        "| shape | date | sha | circuit hash | load | MERGE_S (s) | L4_WALL (s) "
        "| S* (SLO slack) | min slack (s) | S* serial | S* tree | S* s/tx |",
        "|---|---|---|---|---|---|---|---|---|---|---|---|",
    ]
    for e in entries:
        obj = e.get("objectives", {})

        def objs(name):
            o = obj.get(name)
            return f"S={o['s']}" if o and o.get("s") is not None else "NA"

        slo = obj.get("slo_slack") or {}
        cons = e.get("constants", {})
        merge = cons.get("merge_s", {})
        l4 = cons.get("l4_wall_s", {})
        lines.append(
            "| {shape} | {date} | {sha} | `{ch}` | {load} "
            "| {mv} ({ml}) | {lv} ({ll}) | {slo_s} | {slack} "
            "| {ser} | {tree} | {spt} |".format(
                shape=e.get("shape", "?"),
                date=e.get("date", "?"),
                sha=e.get("measured_at_sha", "?"),
                ch=str(e.get("circuit_hash", "?"))[:12],
                load=e.get("load_quality", "?"),
                mv=fmt(merge.get("value")),
                ml=merge.get("label", "?"),
                lv=fmt(l4.get("value")),
                ll=l4.get("label", "?"),
                slo_s=f"S={slo['s']}" if slo.get("s") is not None else "NA",
                slack=fmt(slo.get("min_slack")),
                ser=objs("serial"),
                tree=objs("tree"),
                spt=objs("s_per_tx"),
            )
        )
    lines += [
        "",
        "Constants labeled `extrapolated` come from the Phase A reference",
        "machine scaled by the shape's S=20 L1-wall ratio; `measured` means",
        "this shape ran the opt-in `CAL_L4=1` merge/L4 measurement (or the",
        "Phase A reference measurement itself). Rows from `loaded` runs",
        "carry ~10-20% inflated walls -- treat near-zero-slack verdicts as",
        "unreliable there.",
        "",
        "## Regenerating",
        "",
        "```",
        "make s-calibrate OUT_REGISTRY=1                  # this machine",
        "make s-calibrate OUT_REGISTRY=1 CAL_L4=1         # + measured MERGE_S/L4_WALL",
        "make s-calibrate-fleet                           # collect the c4a cloud probes",
        "make calibration-check                           # staleness guard (warn-only)",
        "```",
        "",
        "Fleet runs collect probes + reports per shape; emit their registry",
        "entries afterwards by re-running scripts/s-calibrate-report.py on",
        "each collected directory with `--out-registry calibration",
        "--shape-label <shape>`.",
        "",
        "Commit the resulting `calibration/*.json` + this README in a PR --",
        "the diff IS the recalibration review.",
        "",
        "## Ledger link policy",
        "",
        "Discussion #77's BENCH-LEDGER remains the append-only history;",
        "every new ledger entry should link the commit that updated this",
        "registry. The registry holds only the CURRENT recommendation per",
        "shape; history lives in git + the ledger.",
        "",
    ]
    with open(os.path.join(registry_dir, "README.md"), "w") as fh:
        fh.write("\n".join(lines))


def main():
    ap = argparse.ArgumentParser(description=__doc__)
    ap.add_argument("--out-dir", required=True)
    ap.add_argument("--block-tx", type=int, default=500)
    ap.add_argument("--merge-s", type=float, default=0.4764,
                    help="tree-merge step constant in s (Phase A measured, "
                         "S-independent; issue #102)")
    ap.add_argument("--chunks", type=int, default=4)
    ap.add_argument("--machine-label", default="unknown")
    ap.add_argument("--git-sha", default=None)
    # ---- v2 knobs (issue #102, objective 4) ----
    ap.add_argument("--lag-p50", type=float, default=20.0,
                    help="proof-lag SLO p50 budget in s (Discussion #77); "
                         "the binding constraint for objective 4")
    ap.add_argument("--lag-p99", type=float, default=40.0,
                    help="proof-lag SLO p99 budget in s -- reported as "
                         "context only, never evaluated")
    ap.add_argument("--block-sizes", default="500 4000 9000",
                    help="space-separated block sizes B for objective 4")
    ap.add_argument("--l4-wall", type=float, default=5.155,
                    help="L4 prove+verify wall in s (Phase A reference; "
                         "build cost is one-time/resident and excluded)")
    ap.add_argument("--merge-label", choices=["measured", "extrapolated"],
                    default="extrapolated",
                    help="provenance of --merge-s (CAL_L4/seeding override)")
    ap.add_argument("--l4-label", choices=["measured", "extrapolated"],
                    default="extrapolated",
                    help="provenance of --l4-wall (CAL_L4/seeding override)")
    ap.add_argument("--ref-l1-s20-ms", type=float, default=12784.0,
                    help="Phase A reference machine's L1(S=20) wall in ms; "
                         "anchors the r-scaling of extrapolated constants")
    ap.add_argument("--load-quality", choices=["clean", "loaded", "unknown"],
                    default=None,
                    help="override the load-quality flag (default: parse "
                         "machine-info.txt, else unknown)")
    ap.add_argument("--circuit-hash", default="unknown",
                    help="deterministic hash over circuit/src/** at "
                         "measurement time (staleness guard, issue #102)")
    ap.add_argument("--date", default=None,
                    help="override the report/registry date (YYYY-MM-DD)")
    ap.add_argument("--out-registry", default=None, metavar="DIR",
                    help="emit a calibration registry JSON for this shape "
                         "into DIR and re-render DIR/README.md")
    ap.add_argument("--shape-label", default=None,
                    help="registry shape name (default: --machine-label)")
    args = ap.parse_args()

    block_sizes = [int(b) for b in args.block_sizes.split()]
    tsv_columns = (TSV_COLUMNS_V1 + TSV_COLUMNS_V2
                   + [f"full_split_wall_{b}" for b in block_sizes]
                   + ["slo_slack_min", "slo_verdict"])

    out = args.out_dir
    probes = {}
    for path in sorted(glob.glob(os.path.join(out, "cal-S*.jsonl"))):
        m = re.search(r"cal-S(\d+)\.jsonl$", path)
        if not m:
            continue
        probes[int(m.group(1))] = parse_probe(path)
    if not probes:
        print(f"error: no cal-S*.jsonl probe files under {out}", file=sys.stderr)
        return 1

    skipped = {}
    skip_path = os.path.join(out, "skipped.tsv")
    if os.path.exists(skip_path):
        with open(skip_path) as fh:
            for line in fh:
                parts = line.rstrip("\n").split("\t", 1)
                if len(parts) == 2 and parts[0].isdigit():
                    skipped[int(parts[0])] = parts[1]

    # ---- objective-4 constants (issue #102 encoding 3) ----
    # Priority: explicit measured labels (seeding) > CAL_L4 probe output
    # > merge events in the regular probes > r-scaled reference constants.
    cal_merge_s, cal_l4_s = parse_cal_l4(out)
    probe_l4 = next((p["l4_split_s"] for p in probes.values()
                     if p and p.get("l4_split_s")), None)
    probe_merges = [p["merge_ms"] for p in probes.values()
                    if p and p.get("merge_ms")]

    r_scale = 1.0
    if args.ref_l1_s20_ms > 0 and probes.get(20):
        r_scale = probes[20]["l1_ms"] / args.ref_l1_s20_ms

    merge_label, l4_label = args.merge_label, args.l4_label
    if merge_label == "measured":
        merge_eff = args.merge_s
    elif cal_merge_s is not None:
        merge_eff, merge_label = cal_merge_s, "measured"
    elif probe_merges:
        merge_eff = (sum(probe_merges) / len(probe_merges)) / 1000.0
        merge_label = "measured"
    else:
        merge_eff = args.merge_s * r_scale

    l4_alt = None  # unscaled interval bound for extrapolated shapes
    if l4_label == "measured":
        l4_eff = args.l4_wall
    elif cal_l4_s is not None:
        l4_eff, l4_label = cal_l4_s, "measured"
    elif probe_l4 is not None:
        l4_eff, l4_label = probe_l4, "measured"
    else:
        l4_eff = args.l4_wall * r_scale
        if abs(r_scale - 1.0) > 1e-9:
            l4_alt = args.l4_wall

    load_quality = args.load_quality or parse_load_quality(out)

    # ---- per-S rows + objectives ----
    rows = []           # TSV rows (dicts keyed by column)
    measured = {}       # S -> objective values for recommendations
    for s in sorted(set(probes) | set(skipped)):
        row = dict.fromkeys(tsv_columns, "NA")
        row["S"] = s
        row["bracket"] = infer_bracket(s, probes)
        if s in skipped:
            row["feasible"], row["label"] = "no", "skipped"
            rows.append(row)
            continue
        p = probes[s]
        row["feasible"] = "yes"
        if p is None:
            row["label"] = "failed"
            rows.append(row)
            continue
        l1_s = p["l1_ms"] / 1000.0
        l2_s = p["l2_ms"] / 1000.0 if p["l2_ms"] is not None else None
        merge_meas = p["merge_ms"] / 1000.0 if p["merge_ms"] is not None else None
        merge_s = merge_meas if merge_meas is not None else args.merge_s
        depth = math.ceil(math.log2(max(args.block_tx / s, 1)))

        s_per_tx = l1_s / s
        serial = max(l1_s, (args.block_tx / s) * l2_s) if l2_s is not None else None
        tree = l1_s + depth * merge_s

        # Objective 4 (issue #102): always-split wall + SLO slack per B.
        slo_walls = {b: l1_s + merge_depth(b, s) * merge_eff + l4_eff
                     for b in block_sizes}
        slo_slacks = {b: args.lag_p50 - w for b, w in slo_walls.items()}
        slack_min = min(slo_slacks.values())
        verdict = slo_verdict_of(slack_min)
        quality = l1_quality_of(p["l1_n"], p["l1_stdev_ms"], p["l1_ms"])

        row["l1_wall_ms"] = round(p["l1_ms"])
        row["l2_wall_ms"] = round(p["l2_ms"]) if p["l2_ms"] is not None else "NA"
        row["peak_rss_mb"] = p["peak_rss_mb"] if p["peak_rss_mb"] is not None else "NA"
        row["s_per_tx"] = fmt(s_per_tx)
        row["serial_block_s"] = fmt(serial)
        row["tree_block_s"] = fmt(tree)
        row["label"] = "measured"
        row["l1_n"] = p["l1_n"]
        row["l1_stdev_ms"] = fmt(p["l1_stdev_ms"], 1)
        row["l1_quality"] = quality
        for b in block_sizes:
            row[f"full_split_wall_{b}"] = fmt(slo_walls[b])
        row["slo_slack_min"] = fmt(slack_min)
        row["slo_verdict"] = verdict
        rows.append(row)
        measured[s] = {
            "s_per_tx": s_per_tx, "serial": serial, "tree": tree,
            "tree_merge_measured": merge_meas is not None,
            "slo_walls": slo_walls, "slo_slacks": slo_slacks,
            "slack_min": slack_min, "slo_verdict": verdict,
            "l1_s": l1_s, "quality": quality,
            "bracket": row["bracket"],
        }

    # ---- recommendations per objective ----
    def recommend(key, labeler):
        cands = sorted(
            ((v[key], s) for s, v in measured.items() if v[key] is not None)
        )
        if not cands:
            return None
        best_val, best_s = cands[0]
        runner = cands[1][0] if len(cands) > 1 else None
        return {
            "S": best_s, "value": best_val,
            "confidence": confidence(best_val, runner),
            "basis": labeler(measured[best_s]),
        }

    rec = {
        "serial": recommend("serial", lambda v: "measured (L1+L2 walls measured; scaled to BLOCK_TX)"),
        "tree": recommend("tree", lambda v: "measured merge" if v["tree_merge_measured"]
                          else f"extrapolated (merge = {args.merge_s} s constant from PR #69)"),
        "s_per_tx": recommend("s_per_tx", lambda v: "measured"),
    }

    # Objective 4 recommendation: argmax of min-over-B slack.
    rec_slo = None
    slo_cands = sorted(((v["slack_min"], s) for s, v in measured.items()),
                       reverse=True)
    if slo_cands:
        best_slack, best_s = slo_cands[0]
        runner_slack = slo_cands[1][0] if len(slo_cands) > 1 else None
        rec_slo = {
            "S": best_s, "min_slack": best_slack,
            "verdict": measured[best_s]["slo_verdict"],
            "confidence": slack_confidence(best_slack, runner_slack),
        }

    # Per-bracket best (issue #102 encoding 4): slack is near-flat WITHIN
    # a degree bracket and cliffs BETWEEN brackets, so the per-bracket
    # winners (bracket tops first-class) are the real candidate set.
    bracket_best = {}
    for s, v in measured.items():
        b = v["bracket"]
        if b not in bracket_best or v["slack_min"] > measured[bracket_best[b]]["slack_min"]:
            bracket_best[b] = s

    # Unscaled-L4 interval variant (extrapolated shapes only).
    alt_slo = None
    if l4_alt is not None and measured:
        alt_min = {
            s: min(args.lag_p50 - (v["l1_s"] + merge_depth(b, s) * merge_eff + l4_alt)
                   for b in block_sizes)
            for s, v in measured.items()
        }
        alt_best = max(alt_min, key=lambda s: alt_min[s])
        alt_slo = {"S": alt_best, "min_slack": alt_min[alt_best],
                   "verdict": slo_verdict_of(alt_min[alt_best])}

    # ---- calibration.tsv ----
    tsv_path = os.path.join(out, "calibration.tsv")
    with open(tsv_path, "w") as fh:
        fh.write("\t".join(tsv_columns) + "\n")
        for row in rows:
            fh.write("\t".join(str(row[c]) for c in tsv_columns) + "\n")

    # ---- report.md ----
    cores, ram = parse_machine_info(out)
    sha = git_sha(args.git_sha)
    today = args.date or date.today().isoformat()

    def rec_line(name, r, unit):
        if r is None:
            return f"- **{name}**: no measurable candidate"
        return (f"- **{name}**: **S={r['S']}** ({r['value']:.3f} {unit}; "
                f"{r['basis']}; confidence: {r['confidence']})")

    lines = [
        f"# s-calibrate report -- {args.machine_label}",
        "",
        PURPOSE_PREAMBLE,
        "",
        f"- date / commit: {today} / {sha}",
        f"- machine: {args.machine_label}, {cores} cores, {ram} RAM"
        f" (load quality: {load_quality})",
        f"- circuit hash: {args.circuit_hash}",
        f"- methodology: {args.chunks} chunks at tx_limit={args.chunks}*S per probe "
        f"(issue #60); objectives for a {args.block_tx}-tx block; "
        f"tree merge constant {args.merge_s} s (PR #69) unless measured",
        "",
        "## Per-S measurements",
        "",
        "| " + " | ".join(tsv_columns) + " |",
        "|" + "---|" * len(tsv_columns),
    ]
    for row in rows:
        lines.append("| " + " | ".join(str(row[c]) for c in tsv_columns) + " |")
    lines += [
        "",
        "## Recommended S per objective",
        "",
        rec_line("serial fold block wall", rec["serial"], "s/block"),
        rec_line("tree fold block wall", rec["tree"], "s/block"),
        rec_line("L1 s/tx", rec["s_per_tx"], "s/tx"),
    ]
    if rec_slo:
        lines.append(
            f"- **SLO slack (objective 4)**: **S={rec_slo['S']}** "
            f"(min-over-B slack {rec_slo['min_slack']:.3f} s, "
            f"{rec_slo['verdict']}; confidence: {rec_slo['confidence']})")
    lines.append("")

    # ---- objective-4 section ----
    bsl = ", ".join(str(b) for b in block_sizes)
    lines += [
        f"## Objective 4 -- SLO slack (proof-lag p50 <= {args.lag_p50:g} s)",
        "",
        "- policy: always-split (Discussion #77); scoped to "
        "block-proof-ready L1->L4 -- the one-time resident L4 build cost "
        "is excluded from the per-block wall",
        "- `full_split_wall(S, B) = L1_chunk_wall(S) + merge_depth(B, S) "
        "* MERGE_S + L4_WALL`; `merge_depth(B, S) = ceil(log2(ceil(B/S)))` "
        "(pairwise merges -- policy-dependent, single site in this script)",
        f"- constants: MERGE_S = {merge_eff:.4f} s ({merge_label}), "
        f"L4_WALL = {l4_eff:.3f} s ({l4_label}), scale r = {r_scale:.3f} "
        f"(L1(S=20) here / reference {args.ref_l1_s20_ms:g} ms)",
        f"- lag budgets: p50 <= {args.lag_p50:g} s (binding), "
        f"p99 <= {args.lag_p99:g} s (context only -- not evaluated here)",
        f"- block sizes B: {{{bsl}}}; verdicts: FEASIBLE / MARGINAL "
        f"(slack < {MARGINAL_SLACK_S:g} s) / INFEASIBLE (slack < 0)",
        "",
    ]
    if measured:
        hdr = (["S"] + [f"wall@{b}" for b in block_sizes]
               + [f"slack@{b}" for b in block_sizes] + ["min slack", "verdict"])
        lines.append("| " + " | ".join(hdr) + " |")
        lines.append("|" + "---|" * len(hdr))
        for s in sorted(measured):
            v = measured[s]
            cells = ([str(s)]
                     + [f"{v['slo_walls'][b]:.3f}" for b in block_sizes]
                     + [f"{v['slo_slacks'][b]:.3f}" for b in block_sizes]
                     + [f"{v['slack_min']:.3f}", v["slo_verdict"]])
            lines.append("| " + " | ".join(cells) + " |")
        lines.append("")
    if bracket_best:
        lines += ["### Per-bracket best (objective 4)", ""]
        for b in sorted(bracket_best):
            s = bracket_best[b]
            lines.append(f"- bracket {b}: best S={s} "
                         f"(min slack {measured[s]['slack_min']:.3f} s, "
                         f"{measured[s]['slo_verdict']})")
        lines += [
            "",
            "Slack is near-flat WITHIN a bracket (L1 wall barely moves) and "
            "cliffs BETWEEN brackets (the ~2x L1 step) -- compare bracket "
            "tops, not neighbours.",
            "",
        ]
    if alt_slo is not None and rec_slo:
        lines += [
            "### Extrapolation interval (L4 scaled vs unscaled)",
            "",
            f"- scaled (primary, L4 = {l4_eff:.3f} s): S={rec_slo['S']}, "
            f"min slack {rec_slo['min_slack']:.3f} s ({rec_slo['verdict']})",
            f"- unscaled (conservative, L4 = {l4_alt:.3f} s): "
            f"S={alt_slo['S']}, min slack {alt_slo['min_slack']:.3f} s "
            f"({alt_slo['verdict']})",
            "",
            "Phase A showed the WINNER is insensitive to this choice but "
            "the HEADROOM is not -- treat the slack as an interval until "
            "this shape measures its own L4 (CAL_L4=1 / Phase C).",
            "",
        ]
    quality_flags = [(s, v["quality"]) for s, v in sorted(measured.items())
                     if v["quality"] != "ok"]
    if quality_flags or load_quality == "loaded":
        lines += ["### Data-quality warnings", ""]
        for s, q in quality_flags:
            lines.append(f"- S={s}: l1_quality={q} "
                         "(n < 3 steady samples and/or stdev/mean > 10%)")
        if load_quality == "loaded":
            lines.append(
                "- run started on a LOADED machine (1-min loadavg/cores > "
                "0.2): L1 walls are inflated; near-zero-slack verdicts "
                "(MARGINAL boundaries) are unreliable -- re-run clean "
                "before acting on them.")
        lines.append("")

    edge_rows = [r for r in rows if r["label"] == "measured"
                 and r["S"] in (9, 10, 11, 21)]
    if edge_rows:
        lines += ["## Bracket-edge verdicts", ""]
        for r in edge_rows:
            settled = "?" not in r["bracket"]
            lines.append(
                f"- S={r['S']}: bracket {r['bracket']}"
                + (" (settled by L1-wall comparison against neighbouring "
                   "bracket anchors)" if settled else
                   " (UNSETTLED -- missing a measured anchor in an adjacent bracket)"))
        lines.append("")
    if skipped:
        lines += ["## RAM-gated candidates", ""]
        for s in sorted(skipped):
            lines.append(f"- S={s}: {skipped[s]}")
        lines.append("")
    with open(os.path.join(out, "report.md"), "w") as fh:
        fh.write("\n".join(lines))

    # ---- ledger.md (Discussion #77 BENCH-LEDGER template) ----
    def short(name, r, unit):
        if r is None:
            return f"{name}=NA"
        basis = "measured" if "measured" in r["basis"] and "extrapolated" not in r["basis"] \
                else "extrapolated"
        return f"{name}=S{r['S']} ({r['value']:.3f} {unit}, {basis})"

    probed = " ".join(str(s) for s in sorted(measured))
    gated = (" RAM-gated: " + ",".join(f"S{s}" for s in sorted(skipped))) if skipped else ""
    slo_head = ""
    if rec_slo:
        slo_head = (f"; slo_opt=S{rec_slo['S']} "
                    f"({rec_slo['min_slack']:.3f} s min-slack @ "
                    f"p50<={args.lag_p50:g}s, {merge_label}/{l4_label})")
    ledger = [
        "> **BENCH-LEDGER**",
        f"> date / commit: {today} / {sha}",
        f"> machine: {args.machine_label}, {cores} cores, {ram} RAM",
        f"> config: calibration probe S in {{{probed}}} CHUNKS={args.chunks} "
        f"(tx_limit={args.chunks}*S) fold=serial+tree(objective) workers=1 mode=batch",
        f"> headline: {short('serial_opt', rec['serial'], 's/block')}; "
        f"{short('tree_opt', rec['tree'], 's/block')}; "
        f"{short('s_per_tx_opt', rec['s_per_tx'], 's/tx')}{slo_head}",
        f"> evidence: issue #85 (s-calibrate suite); raw artifacts: {out}",
        f"> notes: brackets per issue #60 step-function; objectives for a "
        f"{args.block_tx}-tx block; objective 4 per issue #102 "
        f"(B in {{{bsl}}}, MERGE_S={merge_eff:.4f}s/{merge_label}, "
        f"L4_WALL={l4_eff:.3f}s/{l4_label}, load={load_quality}).{gated}",
    ]
    with open(os.path.join(out, "ledger.md"), "w") as fh:
        fh.write("\n".join(ledger) + "\n")

    # ---- registry emission (issue #102 scope amendment) ----
    if args.out_registry:
        shape = args.shape_label or args.machine_label
        safe = re.sub(r"[^A-Za-z0-9._-]+", "-", shape)
        os.makedirs(args.out_registry, exist_ok=True)

        brackets_tbl = {}
        for s, v in sorted(measured.items()):
            brackets_tbl.setdefault(v["bracket"], {"s_measured": []})
            brackets_tbl[v["bracket"]]["s_measured"].append(s)
        for b, s in bracket_best.items():
            brackets_tbl[b]["best_s_by_slo"] = s
            brackets_tbl[b]["min_slack"] = round(measured[s]["slack_min"], 3)

        def obj_entry(r, value_key="value"):
            if r is None:
                return None
            return {"s": r["S"], "value": round(r[value_key], 3)}

        entry = {
            "schema": 1,
            "shape": shape,
            "date": today,
            "measured_at_sha": sha,
            "circuit_hash": args.circuit_hash,
            "load_quality": load_quality,
            "lag_p50_s": args.lag_p50,
            "lag_p99_s": args.lag_p99,
            "block_sizes": block_sizes,
            "scale_r": round(r_scale, 4),
            "constants": {
                "merge_s": {"value": round(merge_eff, 4), "label": merge_label},
                "l4_wall_s": {"value": round(l4_eff, 3), "label": l4_label,
                              **({"unscaled_value": round(l4_alt, 3)}
                                 if l4_alt is not None else {})},
            },
            "brackets": brackets_tbl,
            "objectives": {
                "serial": obj_entry(rec["serial"]),
                "tree": obj_entry(rec["tree"]),
                "s_per_tx": obj_entry(rec["s_per_tx"]),
                "slo_slack": ({"s": rec_slo["S"],
                               "min_slack": round(rec_slo["min_slack"], 3),
                               "verdict": rec_slo["verdict"],
                               "lag_p50": args.lag_p50}
                              if rec_slo else None),
            },
            "per_s_table": [
                {c: row[c] for c in tsv_columns} for row in rows
            ],
        }
        reg_path = os.path.join(args.out_registry, f"{safe}.json")
        with open(reg_path, "w") as fh:
            json.dump(entry, fh, indent=2, sort_keys=False)
            fh.write("\n")
        render_registry_readme(args.out_registry)
        print(f"registry: {reg_path} (+ README.md re-rendered)")

    # ---- stdout summary ----
    print(f"calibration.tsv: {tsv_path}")
    for name, unit in (("serial", "s/block"), ("tree", "s/block"), ("s_per_tx", "s/tx")):
        r = rec[name]
        if r:
            print(f"recommend[{name}]: S={r['S']} ({r['value']:.3f} {unit}; {r['confidence']})")
        else:
            print(f"recommend[{name}]: NA")
    if rec_slo:
        print(f"recommend[slo_slack]: S={rec_slo['S']} "
              f"(min_slack={rec_slo['min_slack']:.3f} s over B={{{bsl}}}; "
              f"verdict={rec_slo['verdict']}; {rec_slo['confidence']})")
    else:
        print("recommend[slo_slack]: NA")
    return 0


if __name__ == "__main__":
    sys.exit(main())
