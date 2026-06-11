#!/usr/bin/env python3
"""s-calibrate-report.py -- objective computation for the per-machine
chunk-size calibration suite (issue #85).

Reads BENCH_EVENT JSONL probe outputs (cal-S<N>.jsonl) from --out-dir,
computes per-S metrics and the three objectives, and writes:

  calibration.tsv   columns: S bracket l1_wall_ms l2_wall_ms peak_rss_mb
                    s_per_tx serial_block_s tree_block_s feasible label
  report.md         human-readable summary + per-objective recommendations
  ledger.md         BENCH-LEDGER entry (Discussion #77 comment template)

Objectives (issue #60 / PR #69 methodology, BLOCK_TX-tx block):
  serial_block_s = max(L1_chunk_wall(S), (BLOCK_TX/S) * L2_step(S))
  tree_block_s   = L1_chunk_wall(S) + ceil(log2(BLOCK_TX/S)) * merge_s
                   (merge_s = measured mean when the probe emitted
                   BlockTxChainMergeCircuit events, else the --merge-s
                   constant -> objective labeled "extrapolated")
  s_per_tx       = L1_chunk_wall(S) / S

This script is intentionally the single code path for local runs
(scripts/s-calibrate.sh), fleet collection (run-fleet.sh collect on a
calibration run), and the golden-fixture test
(scripts/bench-fleet/tests/test-calibrate.sh).
"""

import argparse
import glob
import json
import math
import os
import re
import subprocess
import sys
from datetime import date

TSV_COLUMNS = [
    "S", "bracket", "l1_wall_ms", "l2_wall_ms", "peak_rss_mb",
    "s_per_tx", "serial_block_s", "tree_block_s", "feasible", "label",
]

# Bracket bands from issue #60. Edge values (9..11, 21) are unsettled --
# inferred from measured neighbours when possible (see infer_bracket).
LOWER_TOP = 8       # validated top of 2^17
MID_LO, MID_HI = 12, 20   # validated 2^18 band
HIGH_LO, HIGH_HI = 22, 32  # validated 2^19 band


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


def parse_probe(path: str):
    """Parse one cal-S<N>.jsonl. Returns dict with l1_ms, l2_ms,
    merge_ms (or None), peak_rss_mb (or None), chunks -- or None when the
    probe produced no usable L1 events (failed run)."""
    l1, l2, merges, summary_rss, event_rss = {}, {}, [], None, []
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
            elif kind == "summary":
                summary_rss = ev.get("peak_rss_mb")
    if not l1:
        return None

    def steady_mean(by_idx):
        # Chunk 0 is excluded as warm-up when more than one chunk was
        # measured: circuit build time is already separated into
        # circuit_define events, but the first prove still pays cold
        # caches / first-touch page faults.
        vals = [v for k, v in by_idx.items() if k != 0] or list(by_idx.values())
        return sum(vals) / len(vals)

    return {
        "l1_ms": steady_mean(l1),
        "l2_ms": steady_mean(l2) if l2 else None,
        "merge_ms": (sum(merges) / len(merges)) if merges else None,
        "peak_rss_mb": summary_rss if summary_rss is not None
                       else (max(event_rss) if event_rss else None),
        "chunks": len(l1),
    }


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


def main():
    ap = argparse.ArgumentParser(description=__doc__)
    ap.add_argument("--out-dir", required=True)
    ap.add_argument("--block-tx", type=int, default=500)
    ap.add_argument("--merge-s", type=float, default=0.47)
    ap.add_argument("--chunks", type=int, default=4)
    ap.add_argument("--machine-label", default="unknown")
    ap.add_argument("--git-sha", default=None)
    args = ap.parse_args()

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

    # ---- per-S rows + objectives ----
    rows = []           # TSV rows (dicts keyed by column)
    measured = {}       # S -> objective values for recommendations
    for s in sorted(set(probes) | set(skipped)):
        row = dict.fromkeys(TSV_COLUMNS, "NA")
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

        row["l1_wall_ms"] = round(p["l1_ms"])
        row["l2_wall_ms"] = round(p["l2_ms"]) if p["l2_ms"] is not None else "NA"
        row["peak_rss_mb"] = p["peak_rss_mb"] if p["peak_rss_mb"] is not None else "NA"
        row["s_per_tx"] = fmt(s_per_tx)
        row["serial_block_s"] = fmt(serial)
        row["tree_block_s"] = fmt(tree)
        row["label"] = "measured"
        rows.append(row)
        measured[s] = {
            "s_per_tx": s_per_tx, "serial": serial, "tree": tree,
            "tree_merge_measured": merge_meas is not None,
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

    # ---- calibration.tsv ----
    tsv_path = os.path.join(out, "calibration.tsv")
    with open(tsv_path, "w") as fh:
        fh.write("\t".join(TSV_COLUMNS) + "\n")
        for row in rows:
            fh.write("\t".join(str(row[c]) for c in TSV_COLUMNS) + "\n")

    # ---- report.md ----
    cores, ram = parse_machine_info(out)
    sha = git_sha(args.git_sha)
    today = date.today().isoformat()

    def rec_line(name, r, unit):
        if r is None:
            return f"- **{name}**: no measurable candidate"
        return (f"- **{name}**: **S={r['S']}** ({r['value']:.3f} {unit}; "
                f"{r['basis']}; confidence: {r['confidence']})")

    lines = [
        f"# s-calibrate report -- {args.machine_label}",
        "",
        f"- date / commit: {today} / {sha}",
        f"- machine: {args.machine_label}, {cores} cores, {ram} RAM",
        f"- methodology: {args.chunks} chunks at tx_limit={args.chunks}*S per probe "
        f"(issue #60); objectives for a {args.block_tx}-tx block; "
        f"tree merge constant {args.merge_s} s (PR #69) unless measured",
        "",
        "## Per-S measurements",
        "",
        "| " + " | ".join(TSV_COLUMNS) + " |",
        "|" + "---|" * len(TSV_COLUMNS),
    ]
    for row in rows:
        lines.append("| " + " | ".join(str(row[c]) for c in TSV_COLUMNS) + " |")
    lines += [
        "",
        "## Recommended S per objective",
        "",
        rec_line("serial fold block wall", rec["serial"], "s/block"),
        rec_line("tree fold block wall", rec["tree"], "s/block"),
        rec_line("L1 s/tx", rec["s_per_tx"], "s/tx"),
        "",
    ]
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
    ledger = [
        "> **BENCH-LEDGER**",
        f"> date / commit: {today} / {sha}",
        f"> machine: {args.machine_label}, {cores} cores, {ram} RAM",
        f"> config: calibration probe S in {{{probed}}} CHUNKS={args.chunks} "
        f"(tx_limit={args.chunks}*S) fold=serial+tree(objective) workers=1 mode=batch",
        f"> headline: {short('serial_opt', rec['serial'], 's/block')}; "
        f"{short('tree_opt', rec['tree'], 's/block')}; "
        f"{short('s_per_tx_opt', rec['s_per_tx'], 's/tx')}",
        f"> evidence: issue #85 (s-calibrate suite); raw artifacts: {out}",
        f"> notes: brackets per issue #60 step-function; objectives for a "
        f"{args.block_tx}-tx block.{gated}",
    ]
    with open(os.path.join(out, "ledger.md"), "w") as fh:
        fh.write("\n".join(ledger) + "\n")

    # ---- stdout summary ----
    print(f"calibration.tsv: {tsv_path}")
    for name, unit in (("serial", "s/block"), ("tree", "s/block"), ("s_per_tx", "s/tx")):
        r = rec[name]
        if r:
            print(f"recommend[{name}]: S={r['S']} ({r['value']:.3f} {unit}; {r['confidence']})")
        else:
            print(f"recommend[{name}]: NA")
    return 0


if __name__ == "__main__":
    sys.exit(main())
