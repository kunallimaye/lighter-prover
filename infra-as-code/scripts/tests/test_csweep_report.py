#!/usr/bin/env python3
# Copyright (c) Elliot Technologies, Inc.
# SPDX-License-Identifier: BUSL-1.1

"""Unit tests for csweep_report.py (#321 C-sweep comparison table).

Feeds the report SYNTHETIC, clearly-labeled bench_summary.json fixtures at
different C (tests/fixtures/bench_summary_c{1,2,4}.json) and asserts:
  * each row echoes the REAL throughput fields (C, leaf_count,
    core_sec_per_block, projected nodes @10/@12 bps, cold_fold_cpu),
  * rows are ordered by C,
  * the CPU-optimal C (lowest core_sec_per_block) is picked correctly,
  * an unmeasured summary is surfaced as UNMEASURED, never fabricated.

Fixtures are invented for deterministic tests — NOT measured, NOT under reports/.

Runs two ways:
  * pytest infra-as-code/scripts/tests/test_csweep_report.py
  * python3 infra-as-code/scripts/tests/test_csweep_report.py  (self-test)
"""

import importlib.util
import json
import os
import sys

_HERE = os.path.dirname(os.path.abspath(__file__))
_SCRIPT = os.path.join(_HERE, "..", "csweep_report.py")
_FIX = os.path.join(_HERE, "fixtures")

_spec = importlib.util.spec_from_file_location("csweep_report", _SCRIPT)
csr = importlib.util.module_from_spec(_spec)
_spec.loader.exec_module(csr)


def _load(name):
  return csr._read_summary(os.path.join(_FIX, name))


_C1 = _load("bench_summary_c1.json")
_C2 = _load("bench_summary_c2.json")
_C4 = _load("bench_summary_c4.json")


# ---------------------------------------------------------------------------
# Rows echo the REAL per-C throughput fields and are ordered by C.
# ---------------------------------------------------------------------------
def test_build_rows_orders_by_c_and_echoes_fields():
  # Deliberately pass out of order; build_rows must sort by C.
  rows = csr.build_rows([_C4, _C1, _C2])
  assert [r["chunk_size_C"] for r in rows] == [1, 2, 4]
  by_c = {r["chunk_size_C"]: r for r in rows}
  # C=1 fixture: core_sec_per_block=12.0, leaf_count=2, cold_fold=2.0,
  # nodes @10bps=2, @12bps=3.
  assert abs(by_c[1]["core_sec_per_block"] - 12.0) < 1e-9
  assert by_c[1]["leaf_count"] == 2
  assert abs(by_c[1]["cold_fold_cpu_core_sec"] - 2.0) < 1e-9
  assert by_c[1]["c3d_nodes_10"] == 2
  assert by_c[1]["c3d_nodes_12"] == 3
  # C=2 fixture: core_sec_per_block=20.0, nodes @10=4, @12=4.
  assert abs(by_c[2]["core_sec_per_block"] - 20.0) < 1e-9
  assert by_c[2]["c3d_nodes_10"] == 4
  # C=4 fixture: core_sec_per_block=4.0, nodes @10=1, cold_fold=0.0.
  assert abs(by_c[4]["core_sec_per_block"] - 4.0) < 1e-9
  assert by_c[4]["c3d_nodes_10"] == 1
  assert abs(by_c[4]["cold_fold_cpu_core_sec"] - 0.0) < 1e-9


def test_optimal_row_picks_lowest_core_sec_per_block():
  rows = csr.build_rows([_C1, _C2, _C4])
  best = csr.optimal_row(rows)
  # C=4 has the lowest core_sec_per_block (4.0) => CPU-optimal.
  assert best["chunk_size_C"] == 4
  assert abs(best["core_sec_per_block"] - 4.0) < 1e-9


def test_optimal_row_flips_when_c1_is_cheapest():
  # Sanity: the picker is data-driven, not hardcoded to C=4. Make a synthetic
  # in-memory summary where C=1 is cheapest and confirm it wins.
  cheap_c1 = {
      "throughput": {
          "measured": True, "chunk_size_C": 1, "leaf_count": 2,
          "core_sec_per_block": 1.0, "cold_fold_cpu_core_sec": 0.0,
          "fleet_sizing_projection": {"vcpu_per_node": 60, "by_target_bps": [
              {"target_bps": 10, "c3d_nodes_required": 1},
              {"target_bps": 12, "c3d_nodes_required": 1}]},
      }
  }
  rows = csr.build_rows([cheap_c1, _C4])
  best = csr.optimal_row(rows)
  assert best["chunk_size_C"] == 1


def test_unmeasured_summary_is_surfaced_not_fabricated():
  unmeasured = {"throughput": {
      "measured": False, "chunk_size_C": 10, "leaf_count": 50,
  }}
  rows = csr.build_rows([unmeasured, _C4])
  by_c = {r["chunk_size_C"]: r for r in rows}
  assert by_c[10]["measured"] is False
  assert by_c[10]["core_sec_per_block"] is None
  assert by_c[10]["c3d_nodes_10"] is None
  # optimal ignores the unmeasured row.
  assert csr.optimal_row(rows)["chunk_size_C"] == 4
  # The table renders it as UNMEASURED (not a fabricated 0).
  table = csr.format_table(rows)
  assert "UNMEASURED" in table


def test_format_table_reports_optimal_and_projection_note():
  rows = csr.build_rows([_C1, _C2, _C4])
  table = csr.format_table(rows)
  assert "CPU-optimal C = 4" in table
  assert "PROJECTION" in table  # node columns clearly labelled as projections.


def test_fixtures_are_labeled_synthetic():
  # Guard: the bench_summary fixtures must carry the SYNTHETIC label so they can
  # never be mistaken for measured data (anti-fabrication hygiene).
  for name in ("bench_summary_c1.json", "bench_summary_c2.json", "bench_summary_c4.json"):
    with open(os.path.join(_FIX, name), "r", encoding="utf-8") as f:
      data = json.load(f)
    assert "_SYNTHETIC" in data
    assert "NOT a real benchmark" in data["_SYNTHETIC"]


def _run_self_test():
  tests = [v for k, v in sorted(globals().items()) if k.startswith("test_") and callable(v)]
  failures = 0
  for t in tests:
    try:
      t()
      print(f"[PASS] {t.__name__}")
    except AssertionError as e:
      failures += 1
      print(f"[FAIL] {t.__name__}: {e}")
    except Exception as e:  # noqa: BLE001
      failures += 1
      print(f"[ERROR] {t.__name__}: {e!r}")
  print(f"\n{len(tests) - failures}/{len(tests)} passed")
  return 1 if failures else 0


if __name__ == "__main__":
  sys.exit(_run_self_test())
