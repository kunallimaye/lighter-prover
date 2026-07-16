#!/usr/bin/env python3
# Copyright (c) Elliot Technologies, Inc.
# SPDX-License-Identifier: BUSL-1.1

"""Compare THROUGHPUT across a chunk-size (C) sweep.

Consumes the per-C ``bench_summary.json`` files produced by
``extract_gke_telemetry.py`` (issue #321 C-sweep) and prints a comparison table:

    C | leaf_count | core_sec_per_block | nodes@10bps | nodes@12bps | cold_fold_cpu

so the CPU-optimal C (lowest core_sec_per_block => smallest fleet) is readable at
a glance. The production objective is THROUGHPUT (10-12 blocks/sec); the lever is
TOTAL CPU per block (core-sec/block).

ANTI-FABRICATION (reports/PROVENANCE.md): this tool NEVER invents a number. It
only reads the ``throughput`` section that ``extract_gke_telemetry.py`` already
computed from REAL parsed prove_ms. A run with ``measured=false`` (old log, no
prove_ms) is shown as ``UNMEASURED`` — never a fabricated zero. The fleet-node
columns are the PROJECTIONS the extractor derived from the measured
core_sec_per_block; they are measured-derived, not fabricated.

Inputs may be local paths or gs:// URIs (gs:// is fetched via ``gcloud storage
cat`` — no gcloud needed for local paths / the unit tests).

Runs two ways:
  * as a CLI: python3 csweep_report.py A/bench_summary.json B/bench_summary.json
  * imported by the unit tests (build_rows / format_table are pure functions).
"""

import argparse
import json
import subprocess
import sys


def _read_summary(path):
  """Load a bench_summary.json from a local path or a gs:// URI."""
  if path.startswith("gs://"):
    res = subprocess.run(
        ["gcloud", "storage", "cat", path], capture_output=True, text=True
    )
    if res.returncode != 0:
      raise RuntimeError(f"failed to read {path}: {res.stderr.strip()}")
    return json.loads(res.stdout)
  with open(path, "r", encoding="utf-8") as f:
    return json.load(f)


def _nodes_at(tp, bps):
  """nodes_required at target `bps` from the extractor's projection, or None.

  Reads the arch-neutral `nodes_required` key (#352), falling back to the
  DEPRECATED `c3d_nodes_required` alias so summaries produced by an older
  extractor still render during the one-release migration window.
  """
  proj = tp.get("fleet_sizing_projection") or {}
  for row in proj.get("by_target_bps", []):
    if row.get("target_bps") == bps:
      if "nodes_required" in row:
        return row.get("nodes_required")
      return row.get("c3d_nodes_required")  # DEPRECATED alias fallback.
  return None


def build_rows(summaries, sources=None):
  """Turn a list of loaded bench_summary dicts into comparison rows.

  Each row echoes the REAL throughput fields the extractor computed. Never
  fabricates: an unmeasured / throughput-less summary yields measured=False.
  """
  rows = []
  for i, s in enumerate(summaries):
    tp = s.get("throughput")
    src = sources[i] if sources else None
    if not tp or not tp.get("measured"):
      rows.append({
          "source": src,
          "measured": False,
          "chunk_size_C": (tp or {}).get("chunk_size_C"),
          "leaf_count": (tp or {}).get("leaf_count"),
          "core_sec_per_block": None,
          "nodes_10bps": None,
          "nodes_12bps": None,
          "cold_fold_cpu_core_sec": None,
      })
      continue
    rows.append({
        "source": src,
        "measured": True,
        "chunk_size_C": tp.get("chunk_size_C"),
        "leaf_count": tp.get("leaf_count"),
        "core_sec_per_block": tp.get("core_sec_per_block"),
        "nodes_10bps": _nodes_at(tp, 10),
        "nodes_12bps": _nodes_at(tp, 12),
        "cold_fold_cpu_core_sec": tp.get("cold_fold_cpu_core_sec"),
    })
  # Sort by C when known so the sweep reads left-to-right; unknown C last.
  rows.sort(key=lambda r: (r["chunk_size_C"] is None, r["chunk_size_C"] or 0))
  return rows


def optimal_row(rows):
  """The measured row with the LOWEST core_sec_per_block (CPU-optimal C)."""
  measured = [r for r in rows if r["measured"] and r["core_sec_per_block"] is not None]
  if not measured:
    return None
  return min(measured, key=lambda r: r["core_sec_per_block"])


def _fmt(v, spec=""):
  if v is None:
    return "UNMEASURED"
  if spec:
    return format(v, spec)
  return str(v)


def format_table(rows):
  header = (
      f"{'C':>4} | {'leaf_count':>10} | {'core_sec/block':>15} | "
      f"{'nodes@10bps':>11} | {'nodes@12bps':>11} | {'cold_fold_cpu':>13}"
  )
  sep = "-" * len(header)
  lines = [header, sep]
  for r in rows:
    lines.append(
        f"{_fmt(r['chunk_size_C']):>4} | {_fmt(r['leaf_count']):>10} | "
        f"{_fmt(r['core_sec_per_block'], '.3f') if r['core_sec_per_block'] is not None else 'UNMEASURED':>15} | "
        f"{_fmt(r['nodes_10bps']):>11} | {_fmt(r['nodes_12bps']):>11} | "
        f"{_fmt(r['cold_fold_cpu_core_sec'], '.3f') if r['cold_fold_cpu_core_sec'] is not None else 'UNMEASURED':>13}"
    )
  best = optimal_row(rows)
  lines.append(sep)
  if best is not None:
    lines.append(
        f"CPU-optimal C = {best['chunk_size_C']} "
        f"(core_sec_per_block={best['core_sec_per_block']:.3f}, "
        f"nodes@10bps={best['nodes_10bps']}, @12bps={best['nodes_12bps']})"
    )
  else:
    lines.append("CPU-optimal C = UNMEASURED (no measured runs)")
  lines.append(
      "NOTE: node columns are PROJECTIONS derived from the MEASURED "
      "core_sec_per_block (see each bench_summary.json throughput."
      "fleet_sizing_projection.assumptions). Real C-sweep numbers require GCP runs."
  )
  return "\n".join(lines)


def main():
  parser = argparse.ArgumentParser(
      description="Compare THROUGHPUT across a chunk-size (C) sweep (#321)."
  )
  parser.add_argument(
      "summaries", nargs="+",
      help="Per-C bench_summary.json paths (local) or gs:// URIs.",
  )
  args = parser.parse_args()

  loaded = []
  sources = []
  for p in args.summaries:
    try:
      loaded.append(_read_summary(p))
      sources.append(p)
    except Exception as e:  # noqa: BLE001
      print(f"[ERROR] {p}: {e}", file=sys.stderr)
      sys.exit(1)

  rows = build_rows(loaded, sources)
  print(format_table(rows))


if __name__ == "__main__":
  main()
