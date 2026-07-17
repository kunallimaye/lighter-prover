#!/usr/bin/env python3
# Copyright (c) Elliot Technologies, Inc.
# SPDX-License-Identifier: BUSL-1.1

"""Unit tests for the #347 events-GCS input mode of extract_gke_telemetry.py.

These tests feed the extractor SYNTHETIC ProverEvent JSON dicts (the exact shape
a prover pod writes to `<run-prefix>/events/<key>.json`) and assert that:

  * a ProverEvent JSON maps into the same event-dict shape the coordinator-log
    parser produces (so the SAME derivation math runs),
  * dedup by logical key collapses concurrent-pod / redrive duplicates to ONE
    logical task while COUNTING the extra attempts,
  * build_metrics_from_events computes the SAME bench_summary fields
    (core_sec_per_block, fleet_sizing_projection, leaf/fold split, peak RSS)
    the coordinator-log path computes.

The fixtures are invented for deterministic MATH tests — NOT measured benchmark
data, and never written under reports/. This validates the CODE, not perf. No
GCS/network is touched: only the pure mapping + math functions are exercised.

Runs two ways:
  * pytest infra-as-code/scripts/tests/test_extract_events_gcs.py
  * python3 infra-as-code/scripts/tests/test_extract_events_gcs.py  (self-test)
"""

import importlib.util
import os

_HERE = os.path.dirname(os.path.abspath(__file__))
_SCRIPT = os.path.join(_HERE, "..", "extract_gke_telemetry.py")

_spec = importlib.util.spec_from_file_location("extract_gke_telemetry", _SCRIPT)
ext = importlib.util.module_from_spec(_spec)
_spec.loader.exec_module(ext)


def _prover_event(role, *, level=0, chunk_idx=0, node_idx=0, lo=0, hi=0,
                  prove_ms=1000, peak_rss=0, fold_kind="n/a",
                  is_first=False, status="success", tx_per_proof=4,
                  leaf_count=4, merge_span=0, block_ns=None):
  """Build ONE ProverEvent JSON dict exactly as a prover pod serializes it.

  `block_ns` (#355/#357) is the per-replay block namespace stamped on the
  descriptor (`block_N`). When None it is OMITTED from the descriptor entirely,
  reproducing a pre-#355 / single-block event that carries no namespace.
  """
  descriptor = {
      "role": role,
      "radix": 2,
      "leaf_count": leaf_count,
      "tx_per_proof": tx_per_proof,
      "chunk_idx": chunk_idx,
      "level": level,
      "node_idx": node_idx,
      "lo": lo,
      "hi": hi,
      "fold_strategy": "reduction" if role == "reduction-fold" else "hex",
      "redriven": False,
      "dispatch_ts_ms": 0,
  }
  if block_ns is not None:
    descriptor["block_ns"] = block_ns
  return {
      "descriptor": descriptor,
      "status": status,
      "prove_time_ms": prove_ms,
      "gcs_time_ms": 50,
      "total_time_ms": prove_ms + 50,
      "peak_rss_bytes": peak_rss,
      "prestate_source": "corpus" if role == "leaf" else "n/a",
      "pull_ms": 0,
      "pre_exec_ms": 0,
      "prove_ms": prove_ms,
      "gcs_write_ms": 50,
      "queue_wait_ms": 0,
      "is_first_task_on_pod": is_first,
      "chunk_size": tx_per_proof,
      "leaf_count": leaf_count,
      "fold_kind": fold_kind,
      "merge_interval_span": merge_span,
      "redriven_after_lease_expiry": False,
      "pull_ts_ms": 0,
      "scheduling_class": "sequential",
  }


# ---------------------------------------------------------------------------
# Mapping: ProverEvent JSON -> extractor event-dict.
# ---------------------------------------------------------------------------
def test_map_leaf_event():
  ev = ext.prover_event_json_to_event(
      _prover_event("leaf", chunk_idx=3, prove_ms=990, peak_rss=4_200_000_000)
  )
  assert ev is not None
  assert ev["role"] == "leaf"
  assert ev["idx"] == 3          # leaf addressed by chunk_idx
  assert ev["prove_ms"] == 990
  assert ev["peak_rss_bytes"] == 4_200_000_000
  assert ev["status"] == "success"


def test_map_reduction_fold_derives_span():
  # No explicit merge_interval_span -> derived from lo/hi interval [2,3] -> 2.
  ev = ext.prover_event_json_to_event(
      _prover_event("reduction-fold", level=1, lo=2, hi=3, merge_span=0)
  )
  assert ev["role"] == "reduction-fold"
  assert ev["merge_interval_span"] == 2
  assert ev["level"] == 1


def test_map_rejects_garbage():
  assert ext.prover_event_json_to_event({"no": "descriptor"}) is None
  assert ext.prover_event_json_to_event("not a dict") is None


# ---------------------------------------------------------------------------
# Dedup by logical key: concurrent pods + redrives collapse; count preserved.
# ---------------------------------------------------------------------------
def test_dedup_counts_redrives():
  # Same logical leaf proved by TWO pods (redrive/race): distinct GCS objects,
  # same logical key -> collapses to 1, with 1 extra attempt counted.
  raw = [
      ext.prover_event_json_to_event(_prover_event("leaf", chunk_idx=0, prove_ms=100)),
      ext.prover_event_json_to_event(_prover_event("leaf", chunk_idx=0, prove_ms=110)),
      ext.prover_event_json_to_event(_prover_event("leaf", chunk_idx=1, prove_ms=200)),
  ]
  deduped, redrive_extra = ext.dedupe_events_by_logical_key(raw)
  assert len(deduped) == 2          # two DISTINCT logical leaves
  assert redrive_extra == 1         # one extra attempt on leaf 0


def test_dedup_prefers_success():
  raw = [
      ext.prover_event_json_to_event(_prover_event("leaf", chunk_idx=0, status="failed")),
      ext.prover_event_json_to_event(_prover_event("leaf", chunk_idx=0, status="success", prove_ms=123)),
  ]
  deduped, redrive_extra = ext.dedupe_events_by_logical_key(raw)
  assert len(deduped) == 1
  assert deduped[0]["status"] == "success"
  assert deduped[0]["prove_ms"] == 123
  assert redrive_extra == 1


# ---------------------------------------------------------------------------
# Metrics build: same core_sec_per_block + fleet projection as the log path.
# ---------------------------------------------------------------------------
def _sample_run_events():
  # 4 leaves @ prove_ms=1000 each, 3 folds @ prove_ms=500 each (a 4-leaf
  # reduction tree). total_cpu = 4*1.0 + 3*0.5 = 5.5 core-sec.
  leaves = [
      ext.prover_event_json_to_event(_prover_event("leaf", chunk_idx=i, prove_ms=1000,
                                                    peak_rss=4_200_000_000))
      for i in range(4)
  ]
  folds = [
      ext.prover_event_json_to_event(_prover_event("reduction-fold", level=1, lo=0, hi=1,
                                                    prove_ms=500, fold_kind="real")),
      ext.prover_event_json_to_event(_prover_event("reduction-fold", level=1, lo=2, hi=3,
                                                    prove_ms=500, fold_kind="real")),
      ext.prover_event_json_to_event(_prover_event("reduction-fold", level=2, lo=0, hi=3,
                                                    prove_ms=500, fold_kind="real")),
  ]
  return leaves + folds


def test_metrics_core_sec_per_block():
  events = _sample_run_events()
  # vcpu_per_node is DERIVED from the supplied machine type (#352); c3d-highcpu-60
  # => 60, reproducing the /60 math from the REAL machine type (not a hardcoded
  # constant). Single-block sample (no block_ns) => blocks_config=1 => guard OK.
  m = ext.build_metrics_from_events(events, run_config={"blocks": 1},
                                    target_bps=[10, 12],
                                    machine_type="c3d-highcpu-60")
  tp = m["throughput"]
  assert tp["measured"] is True
  assert abs(tp["leaf_cpu_core_sec"] - 4.0) < 1e-9
  assert abs(tp["fold_cpu_core_sec"] - 1.5) < 1e-9
  assert abs(tp["total_cpu_core_sec"] - 5.5) < 1e-9
  # 1 block -> core_sec_per_block == total_cpu_core_sec.
  assert abs(tp["core_sec_per_block"] - 5.5) < 1e-9
  # Fleet projection: cores = core_sec_per_block * bps; nodes = ceil(cores/60).
  proj = {r["target_bps"]: r for r in tp["fleet_sizing_projection"]["by_target_bps"]}
  assert abs(proj[10]["cores_required"] - 55.0) < 1e-9
  assert proj[10]["c3d_nodes_required"] == 1     # ceil(55/60)
  assert abs(proj[12]["cores_required"] - 66.0) < 1e-9
  assert proj[12]["c3d_nodes_required"] == 2     # ceil(66/60)


def test_metrics_two_block_events_not_collapsed():
  # #357 REGRESSION GUARD for the events-JSON collapse bug. TWO distinct replays
  # (block_0, block_1), EACH with the SAME geometry (leaf_0..leaf_3 + 3 folds).
  # Before the fix, every block's leaf_0 (role=leaf,L0,N0,lo0,hi0) shared an
  # identical logical key and dedupe collapsed the 2 blocks into 1 (silently
  # discarding block_1). With block_ns in the key + the observed-block accounting:
  #   * BOTH blocks' tasks are retained (14 events, not 7),
  #   * distinct_blocks_observed == 2, block_ns_field_present is True,
  #   * the guard PASSES (observed 2 == config 2) and core_sec_per_block = total/2.
  def _block(ns):
    leaves = [
        ext.prover_event_json_to_event(
            _prover_event("leaf", chunk_idx=i, prove_ms=1000,
                          peak_rss=4_200_000_000, block_ns=ns))
        for i in range(4)
    ]
    folds = [
        ext.prover_event_json_to_event(
            _prover_event("reduction-fold", level=1, lo=0, hi=1, prove_ms=500,
                          fold_kind="real", block_ns=ns)),
        ext.prover_event_json_to_event(
            _prover_event("reduction-fold", level=1, lo=2, hi=3, prove_ms=500,
                          fold_kind="real", block_ns=ns)),
        ext.prover_event_json_to_event(
            _prover_event("reduction-fold", level=2, lo=0, hi=3, prove_ms=500,
                          fold_kind="real", block_ns=ns)),
    ]
    return leaves + folds

  raw = _block("block_0") + _block("block_1")
  # Each event carries its block_ns; the mapper must extract it.
  assert all(e["block_ns"] in ("block_0", "block_1") for e in raw)
  # Dedup must NOT collapse the two blocks: 14 distinct logical tasks, 0 redrives.
  deduped, redrive_extra = ext.dedupe_events_by_logical_key(raw)
  assert len(deduped) == 14, "both blocks' tasks retained (NOT collapsed to 7)"
  assert redrive_extra == 0, "distinct blocks are NOT redrives of one another"

  m = ext.build_metrics_from_events(deduped, run_config={"blocks": 2},
                                    machine_type="c3d-highcpu-60")
  tp = m["throughput"]
  # total = 2 blocks * (4*1.0 + 3*0.5) = 2 * 5.5 = 11.0 core-sec.
  assert abs(tp["total_cpu_core_sec"] - 11.0) < 1e-9
  assert tp["blocks_config"] == 2
  assert tp["distinct_blocks_observed"] == 2, "must OBSERVE 2 distinct namespaces"
  assert tp["block_ns_field_present"] is True
  assert tp["divisor_guard_note"] is None, "no guard when observed == config"
  # core_sec_per_block = total / 2 = 5.5 (a REAL number, guard NOT fired).
  assert abs(tp["core_sec_per_block"] - 5.5) < 1e-9
  assert abs(tp["core_sec_per_block_all"] - 5.5) < 1e-9


def test_metrics_guard_refuses_divide_when_block_ns_absent():
  # #357 guard null-handling on the events-JSON path: run_config claims blocks=5
  # but the events carry NO block_ns (unobservable). The extractor MUST NOT divide
  # a 1-block cost by 5 phantom blocks and emit a plausible-but-WRONG per-block
  # cost — it must refuse: core_sec_per_block=null + a divisor_guard_note.
  events = _sample_run_events()               # no block_ns on any event
  assert all(e["block_ns"] is None for e in events)
  m = ext.build_metrics_from_events(events, run_config={"blocks": 5},
                                    machine_type="c3d-highcpu-60")
  tp = m["throughput"]
  assert tp["blocks_config"] == 5
  assert tp["distinct_blocks_observed"] is None, "no block_ns => unobservable"
  assert tp["block_ns_field_present"] is False
  assert tp["core_sec_per_block"] is None, "must REFUSE un-corroborated divide"
  assert tp["core_sec_per_block_all"] is None
  assert tp["core_sec_per_block_warm"] is None
  assert tp["divisor_guard_note"] is not None
  note = tp["divisor_guard_note"].lower()
  assert "un-corroborated" in note or "could not be determined" in note
  # Fleet projection must also be UNMEASURED (never sized off a null).
  for row in tp["fleet_sizing_projection"]["by_target_bps"]:
    assert row["cores_required"] is None
    assert row["nodes_required"] is None


def test_metrics_single_block_backcompat_unchanged():
  # #357 back-compat: a BLOCKS=1 events-JSON run with NO block_ns must behave
  # EXACTLY as before — divide-by-1 is a no-op, no spurious guard, output equals
  # the pre-change single-block result (core_sec_per_block == total_cpu_core_sec).
  events = _sample_run_events()               # no block_ns; single-block run
  assert all(e["block_ns"] is None for e in events)
  m = ext.build_metrics_from_events(events, run_config={"blocks": 1},
                                    machine_type="c3d-highcpu-60")
  tp = m["throughput"]
  assert tp["blocks_config"] == 1
  assert tp["block_ns_field_present"] is False
  assert tp["divisor_guard_note"] is None, "no guard for a legitimate 1-block run"
  # Byte-for-byte: 1 block => core_sec_per_block == total == 5.5 (unchanged).
  assert abs(tp["core_sec_per_block"] - 5.5) < 1e-9
  assert abs(tp["total_cpu_core_sec"] - 5.5) < 1e-9


def test_metrics_peak_rss_and_redrive_surfaced():
  events = _sample_run_events()
  m = ext.build_metrics_from_events(events, run_config={"blocks": 1},
                                    redrive_extra=3)
  # peak RSS max over successful leaf events x safety margin (via sizing).
  leaf_rss = m["derived"]["leaf_peak_rss"]
  assert leaf_rss["peak_rss_measured"] is True
  assert leaf_rss["peak_rss_bytes_max"] == 4_200_000_000
  # #347 redrive count from GCS objects is preserved in recovery.
  assert m["derived"]["recovery"]["redrive_extra_attempts_from_gcs_events"] == 3


def test_metrics_full_summary_shape_matches_log_path():
  # The events-GCS metrics must feed build_summary just like the log path.
  events = _sample_run_events()
  m = ext.build_metrics_from_events(events, run_config={"blocks": 1})
  sizing = ext.build_sizing_derivation(m)
  summary = ext.build_summary(m, {"engine": "gke"}, {"source_kind": "events-gcs"}, sizing)
  # Same top-level keys the coordinator-log path produces.
  for key in ("cryptographic_phase_telemetry", "throughput", "derived_sizing_metrics",
              "sizing_derivation", "descriptors", "provenance", "metadata"):
    assert key in summary
  assert summary["throughput"]["core_sec_per_block"] is not None


def test_distinct_same_span_intervals_not_false_duplicates():
  # Two level-1 reduction folds cover DIFFERENT intervals ([0,1] and [2,3]) but
  # the SAME span (2). They are distinct logical tasks and must NOT be flagged as
  # duplicates (the events-GCS output_key keys on the interval endpoints).
  events = _sample_run_events()
  d = ext.compute_derived(events)
  assert d["duplicate_proved"]["duplicate_output_keys"] == 0
  assert d["duplicate_proved"]["wasted_extra_events"] == 0


def _run_self_test():
  tests = [v for k, v in sorted(globals().items())
           if k.startswith("test_") and callable(v)]
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
  import sys
  sys.exit(_run_self_test())
