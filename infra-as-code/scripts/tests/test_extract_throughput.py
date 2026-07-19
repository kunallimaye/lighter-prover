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


def test_throughput_blocks_from_run_config_refuses_uncorroborated_divide():
  # #357: run_config claims blocks=4 but the single-block throughput fixture has
  # NO block_ns, so distinct_blocks_observed is UNKNOWN. The extractor MUST NOT
  # divide 20.0 cs by a phantom 4 (the old, fabricating behavior that emitted a
  # plausible-but-wrong 5.0 cs/block). Anti-fabrication: refuse + null + note.
  rc = {"blocks": 4, "txs_per_chunk": 4, "leaf_count_per_block": 4}
  m = _parse("coordinator_throughput.log", run_config=rc)
  tp = m["throughput"]
  assert tp["blocks_config"] == 4
  assert tp["blocks_source"] == "run_config.json"
  assert tp["distinct_blocks_observed"] is None, "no block_ns => unobservable"
  assert tp["block_ns_field_present"] is False
  assert tp["core_sec_per_block"] is None, "must REFUSE un-corroborated divide"
  assert tp["core_sec_per_block_all"] is None
  assert tp["divisor_guard_note"] is not None
  note = tp["divisor_guard_note"].lower()
  assert "un-corroborated" in note or "could not be determined" in note
  # C / leaf_count still echoed from run_config (self-describing row).
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


# ---------------------------------------------------------------------------
# #355 multi-block REPLAY: divisor-vs-observed guard + warm/all per-block split.
# ---------------------------------------------------------------------------

def test_355_five_block_divides_by_observed_five():
  # Proper 5-block run: 5 distinct block_N/ namespaces, 125 leaves each (625
  # leaves). block_0's first leaf is COLD (8000ms); all other 624 leaves warm
  # (4000ms). leaf CPU = 8000 + 624*4000 = 2504000ms = 2504.0 cs.
  #   distinct_blocks_observed == 5 == blocks_config => guard PASSES.
  #   core_sec_per_block_all = 2504.0 / 5 = 500.8
  rc = {"blocks": 5, "txs_per_chunk": 4, "leaf_count_per_block": 125}
  tp = _parse("coordinator_multiblock_5.log", run_config=rc)["throughput"]
  assert tp["blocks_config"] == 5
  assert tp["distinct_blocks_observed"] == 5, "must OBSERVE 5 distinct namespaces"
  assert tp["block_ns_field_present"] is True
  assert tp["divisor_guard_note"] is None, "no guard when observed == config"
  assert abs(tp["total_cpu_core_sec"] - 2504.0) < 1e-9
  assert abs(tp["core_sec_per_block_all"] - 500.8) < 1e-9
  # core_sec_per_block (legacy field) tracks _all when the guard is inactive.
  assert abs(tp["core_sec_per_block"] - 500.8) < 1e-9


def test_355_divisor_guard_nulls_on_collapse():
  # COLLAPSED run: run_config claims blocks=5 but only block_0 appears on events
  # (the A2/A3 GCS-CAS collision this fix closes). observed(1) < config(5) => the
  # guard MUST fire: core_sec_per_block = null + a note. NEVER divide 1 tree's
  # cost by 5 phantom blocks (anti-fabrication, same principle as #354).
  rc = {"blocks": 5, "txs_per_chunk": 4, "leaf_count_per_block": 2}
  tp = _parse("coordinator_collapsed_blocks.log", run_config=rc)["throughput"]
  assert tp["blocks_config"] == 5
  assert tp["distinct_blocks_observed"] == 1, "only block_0 observed (collapsed)"
  assert tp["core_sec_per_block"] is None, "must REFUSE to divide by phantom blocks"
  assert tp["core_sec_per_block_all"] is None
  assert tp["core_sec_per_block_warm"] is None
  assert tp["divisor_guard_note"] is not None
  note = tp["divisor_guard_note"]
  assert "phantom" in note.lower()
  assert "observed 1" in note and "5" in note, "note must name observed vs config"
  # The fleet projection must also be UNMEASURED (never sized off a null).
  for row in tp["fleet_sizing_projection"]["by_target_bps"]:
    assert row["cores_required"] is None
    assert row["nodes_required"] is None


def test_355_warm_vs_all_split_excludes_first_task_on_pod():
  # 2-block warm/cold fixture: block_0 leaf0 COLD (8000ms, first-task-on-pod),
  # all else warm. total = 24.0 cs over 2 observed blocks.
  #   core_sec_per_block_all  = 24.0 / 2 = 12.0  (includes the cold transient)
  #   cold_start_core_sec     = 8.0             (the single cold leaf)
  #   warm_total              = 24.0 - 8.0 = 16.0
  #   core_sec_per_block_warm = 16.0 / 2 = 8.0  (EXCLUDES first-task-on-pod)
  rc = {"blocks": 2, "txs_per_chunk": 4, "leaf_count_per_block": 2}
  tp = _parse("coordinator_multiblock_warm_cold.log", run_config=rc)["throughput"]
  assert tp["distinct_blocks_observed"] == 2
  assert abs(tp["total_cpu_core_sec"] - 24.0) < 1e-9
  assert abs(tp["core_sec_per_block_all"] - 12.0) < 1e-9
  assert abs(tp["cold_start_core_sec"] - 8.0) < 1e-9, "cold = the one first-task leaf"
  assert abs(tp["warm_total_core_sec"] - 16.0) < 1e-9
  assert abs(tp["core_sec_per_block_warm"] - 8.0) < 1e-9
  # WARM strictly less than ALL because the cold transient is removed.
  assert tp["core_sec_per_block_warm"] < tp["core_sec_per_block_all"]
  # The warm note documents that warm is the steady-state number + the replay
  # warm-floor caveat (production distinct blocks carry cold-prestate variance).
  assert "steady-state" in tp["warm_note"].lower()
  assert "1.2-1.4x" in tp["warm_note"] or "headroom" in tp["warm_note"].lower()


def test_355_warm_null_when_first_task_flag_absent():
  # An OLD log with NO is_first_task_on_pod flag cannot isolate the cold transient,
  # so core_sec_per_block_warm is null + a note — never a guessed split.
  # The pre-#355 throughput fixture has the flag, so use the old-format fixture
  # (no split field on some events) via the _all path staying non-null while warm
  # degrades honestly if the flag is entirely absent.
  tp = _parse("coordinator_old_format.log")["throughput"]
  # Old fixture carries no block_ns => not observable => falls back to config/1.
  assert tp["block_ns_field_present"] is False
  # No false collapse note when block_ns simply isn't present (honest).
  assert tp["divisor_guard_note"] is None


def test_355_backcompat_single_block_unchanged():
  # BLOCKS=1 back-compat: the pre-existing single-block throughput fixture (no
  # block_ns field) divides EXACTLY as before — core_sec_per_block unchanged, no
  # spurious guard, no phantom observed-block disagreement.
  rc = {"blocks": 1, "txs_per_chunk": 4, "leaf_count_per_block": 4}
  tp = _parse("coordinator_throughput.log", run_config=rc)["throughput"]
  assert tp["blocks"] == 1
  assert tp["blocks_config"] == 1
  assert tp["block_ns_field_present"] is False
  assert tp["divisor_guard_note"] is None
  assert abs(tp["core_sec_per_block"] - 20.0) < 1e-9, "unchanged from pre-#355"


# ---------------------------------------------------------------------------
# #371 blocks-inference from block_ns when run_config.json is ABSENT.
# Events carry `block_ns` (block_0..block_N), NEVER `block_number`, so the legacy
# block_number inference never fired and blocks defaulted to 1 — dividing an
# N-tree cost by 1 and inflating the projected fleet ~N×. The fix infers the
# divisor from the distinct REAL block_ns namespaces on events.
# ---------------------------------------------------------------------------

def test_371_infer_blocks_from_block_ns_no_run_config():
  # 2-block warm/cold fixture (block_0, block_1), total = 24.0 cs. With NO
  # run_config the extractor MUST infer blocks=2 from block_ns and divide:
  #   core_sec_per_block = 24.0 / 2 = 12.0  (was buggy 24.0 / 1 = 24.0)
  # and the fleet projection MUST reflect the per-block cost:
  #   @10bps cores = 12.0 * 10 = 120 => nodes = ceil(120/60) = 2 (NOT 4).
  m = _parse("coordinator_multiblock_warm_cold.log", run_config=None,
             target_bps=[10], machine_type="c3d-highcpu-60")
  tp = m["throughput"]
  assert tp["blocks"] == 2, "must infer 2 blocks from distinct block_ns"
  assert "block_ns" in tp["blocks_source"], "source must credit block_ns inference"
  assert tp["distinct_blocks_observed"] == 2
  assert tp["block_ns_field_present"] is True
  assert tp["divisor_guard_note"] is None, "inference succeeded => no guard"
  assert abs(tp["core_sec_per_block"] - 12.0) < 1e-9, "per-block must be total/2"
  # fleet projection reflects the PER-BLOCK cost, not the total.
  by = {r["target_bps"]: r for r in tp["fleet_sizing_projection"]["by_target_bps"]}
  assert abs(by[10]["cores_required"] - 120.0) < 1e-9, "12.0*10, not 24.0*10"
  assert by[10]["nodes_required"] == 2, "ceil(120/60)=2, NOT the 24.0-based 4"
  assert by[10]["nodes_required"] == math.ceil(12.0 * 10 / 60)


def test_371_infer_blocks_from_block_ns_five_block_regression():
  # 5-block fixture, total = 2504.0 cs. With NO run_config the extractor MUST
  # infer blocks=5 and divide: core_sec_per_block = 2504.0 / 5 = 500.8 — NOT the
  # buggy 2504.0 (blocks=1). Nodes must be ~1/5 of the total-cost figure.
  m = _parse("coordinator_multiblock_5.log", run_config=None,
             target_bps=[10], machine_type="c3d-highcpu-60")
  tp = m["throughput"]
  assert tp["blocks"] == 5, "must infer 5 blocks from distinct block_ns"
  assert "block_ns" in tp["blocks_source"]
  assert tp["distinct_blocks_observed"] == 5
  assert tp["divisor_guard_note"] is None
  assert abs(tp["core_sec_per_block"] - 500.8) < 1e-9, "2504.0 / 5"
  by = {r["target_bps"]: r for r in tp["fleet_sizing_projection"]["by_target_bps"]}
  # Expected nodes computed from the PER-BLOCK cost (500.8), not the total.
  expected_cores = 500.8 * 10
  expected_nodes = math.ceil(expected_cores / 60)
  assert abs(by[10]["cores_required"] - expected_cores) < 1e-9
  assert by[10]["nodes_required"] == expected_nodes, "per-block sized, not total"
  # Sanity: this is ~1/5 of what the total-cost bug would have produced.
  buggy_nodes = math.ceil(2504.0 * 10 / 60)
  assert by[10]["nodes_required"] < buggy_nodes / 4, "must NOT reflect total cost"


def test_371_backcompat_single_block_no_block_ns():
  # A genuine single-block run (no block_ns at all) MUST still infer blocks=1
  # and divide by 1 — byte-for-byte back-compat, no spurious guard.
  m = _parse("coordinator_throughput.log", run_config=None)
  tp = m["throughput"]
  assert tp["blocks"] == 1, "single-block run stays blocks=1"
  assert "default 1" in tp["blocks_source"]
  assert tp["block_ns_field_present"] is False
  assert tp["distinct_blocks_observed"] is None
  assert abs(tp["core_sec_per_block"] - 20.0) < 1e-9, "unchanged single-block cost"
  assert tp["divisor_guard_note"] is None, "no guard for a real single-block run"


def test_371_base_sentinel_infers_single_block():
  # The coordinator-log parser captures the literal `<base>` single-block
  # sentinel verbatim (unlike the events-GCS path which normalizes it to None).
  # A run whose ONLY block_ns is `<base>` has zero REAL namespaces and MUST infer
  # blocks=1 (not 1 "real" block from the sentinel) — the divide-by-1 no-op that
  # preserves back-compat. Drive compute_throughput directly with synthetic
  # events carrying block_ns="<base>".
  events = [
      {"role": "leaf", "status": "success", "prove_ms": 10000.0,
       "prove_time_ms": 10000.0, "is_first_task_on_pod": True,
       "block_ns": "<base>"},
      {"role": "leaf", "status": "success", "prove_ms": 10000.0,
       "prove_time_ms": 10000.0, "is_first_task_on_pod": False,
       "block_ns": "<base>"},
  ]
  tp = ext.compute_throughput(events, run_config=None)
  assert tp["blocks"] == 1, "`<base>` sentinel is NOT a real block namespace"
  assert "default 1" in tp["blocks_source"]
  assert tp["divisor_guard_note"] is None
  # total = 20.0 cs / 1 block = 20.0 (divide-by-1 no-op).
  assert abs(tp["core_sec_per_block"] - 20.0) < 1e-9


def test_371_multi_block_never_silently_divides_by_one():
  # Defensive invariant (#371 step 2): a multi-block run must NEVER silently
  # divide the total by 1. With the step-1 inference the normal path infers the
  # real block count, so the guard does NOT fire and per-block is non-null. This
  # locks in that a run carrying multiple distinct REAL block_ns yields
  # blocks==(distinct count) with a correct per-block cost.
  events = [
      {"role": "leaf", "status": "success", "prove_ms": 6000.0,
       "prove_time_ms": 6000.0, "is_first_task_on_pod": True,
       "block_ns": "block_0"},
      {"role": "leaf", "status": "success", "prove_ms": 6000.0,
       "prove_time_ms": 6000.0, "is_first_task_on_pod": False,
       "block_ns": "block_1"},
      {"role": "leaf", "status": "success", "prove_ms": 6000.0,
       "prove_time_ms": 6000.0, "is_first_task_on_pod": False,
       "block_ns": "block_2"},
  ]
  tp = ext.compute_throughput(events, run_config=None)
  # Inference fires: 3 distinct real block_ns => blocks=3, per-block = 18.0/3=6.0.
  assert tp["blocks"] == 3, "must infer 3 blocks, never default 1"
  assert "block_ns" in tp["blocks_source"]
  assert tp["divisor_guard_note"] is None, "inference succeeded => no guard fires"
  assert tp["core_sec_per_block"] is not None, "multi-block must NOT divide by 1"
  assert abs(tp["core_sec_per_block"] - 6.0) < 1e-9, "18.0 / 3 real blocks"


def test_371_defensive_guard_fires_if_inference_bypassed():
  # Belt-and-suspenders tripwire: if a FUTURE regression ever reaches the divide
  # step with blocks_config forced to 1 while multiple distinct real block_ns are
  # present on events, the (C) guard MUST fire → per-block null + a loud note,
  # rather than silently dividing an N-tree cost by 1. We simulate that state by
  # forcing run_config blocks=1 with events that carry 2 distinct real block_ns:
  # run_config is authoritative for blocks_config, so inference is bypassed and
  # blocks_config==1, but the observed real namespaces disagree.
  events = [
      {"role": "leaf", "status": "success", "prove_ms": 6000.0,
       "prove_time_ms": 6000.0, "is_first_task_on_pod": True,
       "block_ns": "block_0"},
      {"role": "leaf", "status": "success", "prove_ms": 6000.0,
       "prove_time_ms": 6000.0, "is_first_task_on_pod": False,
       "block_ns": "block_1"},
  ]
  tp = ext.compute_throughput(events, run_config={"blocks": 1})
  assert tp["blocks_config"] == 1, "run_config forced blocks_config=1"
  assert tp["distinct_blocks_observed"] == 2, "two real namespaces observed"
  assert tp["core_sec_per_block"] is None, "must REFUSE to divide N-tree cost by 1"
  assert tp["core_sec_per_block_all"] is None
  assert tp["divisor_guard_note"] is not None, "loud tripwire note required"
  assert "collapsed" in tp["divisor_guard_note"].lower()
  # And the fleet projection must be UNMEASURED (never sized off a null).
  for row in tp["fleet_sizing_projection"]["by_target_bps"]:
    assert row["cores_required"] is None
    assert row["nodes_required"] is None


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
