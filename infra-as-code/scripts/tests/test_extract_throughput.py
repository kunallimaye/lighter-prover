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
    ceil(cores / vcpu_per_node), where vcpu_per_node is DERIVED from the REAL
    machine type — never a hardcoded constant; #352),
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


def _parse(fixture_name, run_config=None, target_bps=None, machine_type=None):
  return ext.parse_coordinator_log_v2(
      os.path.join(_FIX, fixture_name), run_config=run_config,
      target_bps=target_bps, machine_type=machine_type,
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
  # vcpu_per_node is DERIVED from the supplied machine type (#352); here
  # c3d-highcpu-60 => 60, reproducing the original /60 math but from the REAL
  # machine type, not a hardcoded constant.
  m = _parse("coordinator_throughput.log", machine_type="c3d-highcpu-60")
  tp = m["throughput"]
  proj = tp["fleet_sizing_projection"]
  assert proj["vcpu_per_node"] == 60
  assert proj["node_type"] == "c3d-highcpu-60"
  by = {r["target_bps"]: r for r in proj["by_target_bps"]}
  # core_sec_per_block=20.0 => @10bps cores=200 => ceil(200/60)=4 nodes.
  assert abs(by[10]["cores_required"] - 200.0) < 1e-9
  assert by[10]["nodes_required"] == 4
  assert by[10]["nodes_required"] == math.ceil(20.0 * 10 / 60)
  # DEPRECATED alias mirrors the arch-neutral value for one release.
  assert by[10]["c3d_nodes_required"] == 4
  # @12bps cores=240 => ceil(240/60)=4 nodes.
  assert abs(by[12]["cores_required"] - 240.0) < 1e-9
  assert by[12]["nodes_required"] == 4
  # Explicitly labelled a PROJECTION with assumptions (not fabricated).
  assert "PROJECTION" in proj["kind"]
  assert any("steady state" in a for a in proj["assumptions"])


def test_throughput_custom_target_bps():
  # core_sec_per_block=20.0 @1bps => cores=20 => ceil(20/60)=1 node.
  m = _parse("coordinator_throughput.log", target_bps=[1],
             machine_type="c3d-highcpu-60")
  by = {r["target_bps"]: r for r in m["throughput"]["fleet_sizing_projection"]["by_target_bps"]}
  assert list(by.keys()) == [1]
  assert by[1]["nodes_required"] == 1


def test_throughput_vcpu_per_node_derived_from_machine_type():
  # (#352) SAME core-sec input, DIFFERENT machine type => DIFFERENT divisor.
  # core_sec_per_block=20.0 @10bps => cores=200.
  c4d = _parse("coordinator_throughput.log", machine_type="c4d-highcpu-64")
  c4d_proj = c4d["throughput"]["fleet_sizing_projection"]
  assert c4d_proj["vcpu_per_node"] == 64
  assert c4d_proj["node_type"] == "c4d-highcpu-64"
  by64 = {r["target_bps"]: r for r in c4d_proj["by_target_bps"]}
  # ceil(200/64) = 4.
  assert by64[10]["nodes_required"] == math.ceil(200.0 / 64)

  c3d = _parse("coordinator_throughput.log", machine_type="c3d-highcpu-60")
  by60 = {r["target_bps"]: r
          for r in c3d["throughput"]["fleet_sizing_projection"]["by_target_bps"]}
  # ceil(200/60) = 4 (same here, but derived from 60 not a constant).
  assert by60[10]["nodes_required"] == math.ceil(200.0 / 60)
  assert c3d["throughput"]["fleet_sizing_projection"]["vcpu_per_node"] == 60


def test_throughput_unknown_machine_type_emits_null_plus_note():
  # (#352) ANTI-FABRICATION: unknown/None machine type => nodes_required is null
  # WITH an explanatory note, and cores_required (real, measured) is still shown.
  # NEVER a guessed number.
  for mt in (None, "unknown", "not-a-machine"):
    m = _parse("coordinator_throughput.log", machine_type=mt)
    proj = m["throughput"]["fleet_sizing_projection"]
    assert proj["vcpu_per_node"] is None
    assert proj["node_type"] == (mt if mt not in (None,) else None)
    by = {r["target_bps"]: r for r in proj["by_target_bps"]}
    # cores are real/measured; nodes null; note explains why.
    assert abs(by[10]["cores_required"] - 200.0) < 1e-9
    assert by[10]["nodes_required"] is None
    assert by[10]["c3d_nodes_required"] is None  # deprecated alias also null.
    assert "underivable" in by[10]["note"]


def test_throughput_run_config_machine_type_takes_precedence():
  # (#352) run_config's self-describing machine_type wins over the caller's
  # config-resolved fallback (telemetry that describes the ACTUAL run wins).
  rc = {"blocks": 1, "machine_type": "c4d-highcpu-64"}
  m = _parse("coordinator_throughput.log", run_config=rc,
             machine_type="c3d-highcpu-60")  # fallback deliberately different.
  proj = m["throughput"]["fleet_sizing_projection"]
  assert proj["vcpu_per_node"] == 64
  assert proj["node_type"] == "c4d-highcpu-64"


def test_throughput_a1_reprojection_c4d_highcpu_64():
  # (#352 acceptance) The attempt-58-A1-b1 reprojection: a run whose REAL
  # core_sec_per_block ≈ 1700.23 on c4d-highcpu-64 must reproject to
  #   @10bps: ceil(17002.3/64) = 266 nodes
  #   @12bps: ceil(20402.76/64) = 319 nodes
  # We drive compute_throughput directly with a synthetic single leaf whose
  # prove_ms == 1700230 (=> 1700.23 core-sec, blocks=1) to isolate the math.
  events = [{
      "role": "leaf", "status": "success",
      "prove_ms": 1700230.0, "prove_time_ms": 1700230.0,
      "is_first_task_on_pod": True,
  }]
  tp = ext.compute_throughput(events, machine_type="c4d-highcpu-64")
  assert abs(tp["core_sec_per_block"] - 1700.23) < 1e-6
  proj = tp["fleet_sizing_projection"]
  assert proj["vcpu_per_node"] == 64
  by = {r["target_bps"]: r for r in proj["by_target_bps"]}
  assert by[10]["nodes_required"] == 266
  assert by[10]["nodes_required"] == math.ceil(1700.23 * 10 / 64)
  assert by[12]["nodes_required"] == 319
  assert by[12]["nodes_required"] == math.ceil(1700.23 * 12 / 64)


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
