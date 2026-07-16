#!/usr/bin/env python3
# Copyright (c) Elliot Technologies, Inc.
# SPDX-License-Identifier: BUSL-1.1

"""Unit tests for the THROUGHPUT metric in extract_gke_telemetry.py (#321).

Feeds the parser a SYNTHETIC, clearly-labeled fixture (tests/fixtures/
coordinator_throughput.log) with ROUND prove_ms values and asserts the derived
throughput math is correct:
  * leaf_cpu / fold_cpu / total_cpu core-seconds (= sum(prove_ms)/1000),
  * core_sec_per_block (= total / blocks),
  * the fleet-sizing PROJECTION (cores = core_sec_per_block * bps; nodes =
    ceil(cores / 60)),
  * the cold-vs-warm FOLD CPU split (by is_first_task_on_pod),
  * blocks from run_config.json when provided (else default 1),
  * UNMEASURED fallback when prove_ms/prove_time_ms are absent.

The fixtures are invented for deterministic math tests — NOT measured benchmark
data, NOT under reports/. This validates the CODE, not performance.

Runs two ways:
  * pytest infra-as-code/scripts/tests/test_extract_throughput.py
  * python3 infra-as-code/scripts/tests/test_extract_throughput.py  (self-test)
"""

import importlib.util
import math
import os
import sys

_HERE = os.path.dirname(os.path.abspath(__file__))
_SCRIPT = os.path.join(_HERE, "..", "extract_gke_telemetry.py")
_FIX = os.path.join(_HERE, "fixtures")

_spec = importlib.util.spec_from_file_location("extract_gke_telemetry", _SCRIPT)
ext = importlib.util.module_from_spec(_spec)
_spec.loader.exec_module(ext)


def _parse(fixture_name, run_config=None, target_bps=None):
  return ext.parse_coordinator_log_v2(
      os.path.join(_FIX, fixture_name), run_config=run_config, target_bps=target_bps
  )


# ---------------------------------------------------------------------------
# Core CPU accounting: leaf / fold / total core-seconds from summed prove_ms.
# Fixture: leaves 1000+2000+3000+6000=12000ms=12.0cs; folds 3000+4000+1000=
# 8000ms=8.0cs; total=20.0cs; blocks default 1 => core_sec_per_block=20.0.
# ---------------------------------------------------------------------------
def test_throughput_cpu_core_sec():
  m = _parse("coordinator_throughput.log")
  tp = m["throughput"]
  assert tp["measured"] is True
  assert abs(tp["leaf_cpu_core_sec"] - 12.0) < 1e-9
  assert abs(tp["fold_cpu_core_sec"] - 8.0) < 1e-9
  assert abs(tp["total_cpu_core_sec"] - 20.0) < 1e-9
  assert tp["prove_source_field"] == "prove_ms"


def test_throughput_core_sec_per_block_default_blocks():
  m = _parse("coordinator_throughput.log")
  tp = m["throughput"]
  # No run_config, no block_number in log => blocks default 1.
  assert tp["blocks"] == 1
  assert "default 1" in tp["blocks_source"]
  assert abs(tp["core_sec_per_block"] - 20.0) < 1e-9


def test_throughput_blocks_from_run_config():
  # run_config authoritative for blocks: total 20.0 cs / 4 blocks = 5.0 cs/block.
  rc = {"blocks": 4, "txs_per_chunk": 4, "leaf_count_per_block": 4}
  m = _parse("coordinator_throughput.log", run_config=rc)
  tp = m["throughput"]
  assert tp["blocks"] == 4
  assert tp["blocks_source"] == "run_config.json"
  assert abs(tp["core_sec_per_block"] - 5.0) < 1e-9
  # C / leaf_count echoed from run_config (self-describing row).
  assert tp["chunk_size_C"] == 4
  assert tp["leaf_count"] == 4


def test_throughput_fleet_sizing_projection():
  m = _parse("coordinator_throughput.log")
  tp = m["throughput"]
  proj = tp["fleet_sizing_projection"]
  assert proj["vcpu_per_node"] == 60
  by = {r["target_bps"]: r for r in proj["by_target_bps"]}
  # core_sec_per_block=20.0 => @10bps cores=200 => ceil(200/60)=4 nodes.
  assert abs(by[10]["cores_required"] - 200.0) < 1e-9
  assert by[10]["c3d_nodes_required"] == 4
  assert by[10]["c3d_nodes_required"] == math.ceil(20.0 * 10 / 60)
  # @12bps cores=240 => ceil(240/60)=4 nodes.
  assert abs(by[12]["cores_required"] - 240.0) < 1e-9
  assert by[12]["c3d_nodes_required"] == 4
  # Explicitly labelled a PROJECTION with assumptions (not fabricated).
  assert "PROJECTION" in proj["kind"]
  assert any("steady state" in a for a in proj["assumptions"])


def test_throughput_custom_target_bps():
  # core_sec_per_block=20.0 @1bps => cores=20 => ceil(20/60)=1 node.
  m = _parse("coordinator_throughput.log", target_bps=[1])
  by = {r["target_bps"]: r for r in m["throughput"]["fleet_sizing_projection"]["by_target_bps"]}
  assert list(by.keys()) == [1]
  assert by[1]["c3d_nodes_required"] == 1


def test_throughput_cold_vs_warm_fold_split():
  m = _parse("coordinator_throughput.log")
  tp = m["throughput"]
  # cold fold (is_first_task_on_pod=true): 3000ms => 3.0 core-sec.
  # warm fold (false): 4000+1000=5000ms => 5.0 core-sec.
  assert tp["is_first_task_field_present"] is True
  assert abs(tp["cold_fold_cpu_core_sec"] - 3.0) < 1e-9
  assert abs(tp["warm_fold_cpu_core_sec"] - 5.0) < 1e-9
  # cold+warm == total fold CPU (accounting closes).
  assert abs(
      tp["cold_fold_cpu_core_sec"] + tp["warm_fold_cpu_core_sec"]
      - tp["fold_cpu_core_sec"]
  ) < 1e-9


def test_throughput_old_log_prove_time_ms_fallback():
  # Old format has prove_time_ms but not split prove_ms => fallback, still
  # measured. Fixture leaves: 1000+1100=2100ms=2.1cs; node fold: 800ms=0.8cs.
  m = _parse("coordinator_old_format.log")
  tp = m["throughput"]
  assert tp["measured"] is True
  assert "fallback" in tp["prove_source_field"]
  assert abs(tp["leaf_cpu_core_sec"] - 2.1) < 1e-9
  assert abs(tp["fold_cpu_core_sec"] - 0.8) < 1e-9
  assert abs(tp["total_cpu_core_sec"] - 2.9) < 1e-9


def test_throughput_additive_does_not_break_existing():
  # The throughput section is ADDITIVE: the pre-existing derived block and
  # descriptors must still be present and unchanged in shape.
  m = _parse("coordinator_new_format.log")
  assert "throughput" in m
  assert "derived" in m
  assert m["derived"]["leaf_peak_rss"]["peak_rss_bytes_max"] == 4_200_000_000
  # And the throughput section rides alongside it.
  assert m["throughput"]["measured"] is True


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
