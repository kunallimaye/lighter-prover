#!/usr/bin/env python3
# Copyright (c) Elliot Technologies, Inc.
# SPDX-License-Identifier: BUSL-1.1

"""Unit tests for the #365 fold parallelism / critical-path / ingestion metrics.

These exercise compute_fold_critical_path / compute_fold_parallelism /
compute_gating_ingestion_rate in extract_gke_telemetry.py using SYNTHETIC,
clearly-labeled event dicts (the SAME event shape parse_event_line and
prover_event_json_to_event produce). The fixtures are invented for deterministic
math tests -- they are NOT measured benchmark data and are NOT written under
reports/. This validates the CODE, not performance.

Edge cases covered: hex vs reduction topology, single-level tree, missing
timestamps, per-block (block_ns) grouping, the observed-from-timestamps path,
the sweep-line peak-concurrency counter, and every UNMEASURED anti-fabrication
branch.

Runs two ways:
  * pytest infra-as-code/scripts/tests/test_fold_parallelism_telemetry.py
  * python3 infra-as-code/scripts/tests/test_fold_parallelism_telemetry.py
"""

import importlib.util
import os
import sys

_HERE = os.path.dirname(os.path.abspath(__file__))
_SCRIPT = os.path.join(_HERE, "..", "extract_gke_telemetry.py")

_spec = importlib.util.spec_from_file_location("extract_gke_telemetry", _SCRIPT)
ext = importlib.util.module_from_spec(_spec)
_spec.loader.exec_module(ext)

# Epoch-ms of 2024-01-01T00:00:00.000Z. In reality pull_ts_ms and the log
# completion timestamp (ev['ts']) are BOTH absolute epoch-ms, so tests that
# combine them (observed critical path / concurrency) must keep them on the same
# scale. Helpers below express both as EPOCH + a small offset.
_EPOCH = ext._completion_ts_ms({"ts": "2024-01-01T00:00:00.000Z"})


def _ts_at(offset_ms):
  """RFC3339 'Z' timestamp `offset_ms` after the shared epoch base."""
  dt = ext.datetime.datetime.fromtimestamp(
      (_EPOCH + offset_ms) / 1000.0, tz=ext.datetime.timezone.utc)
  return dt.strftime("%Y-%m-%dT%H:%M:%S.") + f"{dt.microsecond // 1000:03d}Z"


# ---------------------------------------------------------------------------
# Synthetic event builders (match the parsed event-dict shape exactly).
# ---------------------------------------------------------------------------
def _base(role, idx, prove_ms=None, prove_time_ms=100, status="success", **kw):
  e = {
      "ts": None,
      "role": role,
      "idx": idx,
      "status": status,
      "prove_time_ms": prove_time_ms,
      "gcs_time_ms": 0,
      "total_time_ms": prove_time_ms,
      "fold_strategy": None,
      "level": None,
      "peak_rss_bytes": None,
      "prestate_source": None,
      "is_first_task_on_pod": None,
      "chunk_size": None,
      "leaf_count": None,
      "pull_ms": None,
      "pre_exec_ms": None,
      "prove_ms": prove_ms,
      "gcs_write_ms": None,
      "queue_wait_ms": None,
      "merge_interval_span": None,
      "redriven_after_lease_expiry": None,
      "pull_ts_ms": None,
      "scheduling_class": None,
      "block_ns": None,
      "lo": None,
      "hi": None,
  }
  e.update(kw)
  return e


def leaf(idx, **kw):
  return _base("leaf", idx, **kw)


def reduction_fold(level, idx, lo=None, hi=None, prove_ms=1700, **kw):
  return _base("reduction-fold", idx, prove_ms=prove_ms, level=level,
               lo=lo, hi=hi, fold_strategy="reduction", **kw)


def hex_node(level, idx, prove_ms=74000, **kw):
  return _base("tree-node", idx, prove_ms=prove_ms, level=level,
               fold_strategy="hex", **kw)


# ---------------------------------------------------------------------------
# (1) fold_critical_path
# ---------------------------------------------------------------------------
def test_critical_path_reduction_multi_level():
  # L1: two folds (max 1800); L2: one fold (1700). CP = 1800 + 1700 = 3500.
  events = [
      reduction_fold(1, 0, lo=0, hi=63, prove_ms=1700),
      reduction_fold(1, 1, lo=64, hi=127, prove_ms=1800),
      reduction_fold(2, 0, lo=0, hi=127, prove_ms=1700),
  ]
  r = ext.compute_fold_critical_path(events)
  assert r["measured"] is True
  assert r["provenance"] == "measured-derived"
  assert r["num_blocks"] == 1
  blk = r["per_block"][0]
  assert blk["num_levels"] == 2
  assert blk["per_level_max_prove_ms"] == {"1": 1800.0, "2": 1700.0}
  assert blk["modeled_critical_path_ms"] == 3500.0
  assert blk["modeled_provenance"] == "modeled-from-prove-times"
  assert r["avg_modeled_critical_path_ms"] == 3500.0


def test_critical_path_hex_two_deep():
  # hex: 2 levels x 74000 = 148000 (vs reduction's ~12k) -- the thesis figure.
  events = [
      hex_node(1, 0, prove_ms=74000),
      hex_node(1, 1, prove_ms=70000),
      hex_node(2, 0, prove_ms=74000),
  ]
  r = ext.compute_fold_critical_path(events)
  blk = r["per_block"][0]
  assert blk["modeled_critical_path_ms"] == 148000.0
  assert blk["per_level_max_prove_ms"] == {"1": 74000.0, "2": 74000.0}


def test_critical_path_single_level_tree():
  events = [reduction_fold(1, 0, lo=0, hi=1, prove_ms=1500)]
  r = ext.compute_fold_critical_path(events)
  blk = r["per_block"][0]
  assert blk["num_levels"] == 1
  assert blk["modeled_critical_path_ms"] == 1500.0


def test_critical_path_prove_time_ms_fallback():
  # OLD logs lack prove_ms -> fall back to prove_time_ms (like _fold_prove).
  events = [reduction_fold(1, 0, lo=0, hi=1, prove_ms=None, prove_time_ms=1234)]
  r = ext.compute_fold_critical_path(events)
  assert r["per_block"][0]["modeled_critical_path_ms"] == 1234.0


def test_critical_path_observed_from_timestamps():
  events = [
      reduction_fold(1, 0, lo=0, hi=63, prove_ms=1700,
                     pull_ts_ms=_EPOCH + 1000, ts=_ts_at(2700)),
      reduction_fold(1, 1, lo=64, hi=127, prove_ms=1800,
                     pull_ts_ms=_EPOCH + 1000, ts=_ts_at(2800)),
      reduction_fold(2, 0, lo=0, hi=127, prove_ms=1700,
                     pull_ts_ms=_EPOCH + 2800, ts=_ts_at(4500)),
  ]
  r = ext.compute_fold_critical_path(events)
  blk = r["per_block"][0]
  # observed = max completion (epoch+4500) - min pull (epoch+1000) = 3500.
  assert blk["observed_critical_path_ms"] == 3500.0
  assert blk["observed_provenance"] == "observed-from-timestamps"
  # modeled still computed alongside observed.
  assert blk["modeled_critical_path_ms"] == 3500.0
  assert r["avg_observed_critical_path_ms"] == 3500.0


def test_critical_path_missing_timestamps_observed_null():
  events = [reduction_fold(1, 0, lo=0, hi=1, prove_ms=1500)]
  r = ext.compute_fold_critical_path(events)
  blk = r["per_block"][0]
  assert blk["observed_critical_path_ms"] is None
  assert blk["observed_provenance"] == "UNMEASURED"
  assert r["avg_observed_critical_path_ms"] is None


def test_critical_path_per_block_grouping_and_average():
  # Two blocks (block_ns) with different critical paths -> averaged.
  events = [
      reduction_fold(1, 0, lo=0, hi=1, prove_ms=1000, block_ns="block_0"),
      reduction_fold(1, 0, lo=0, hi=1, prove_ms=3000, block_ns="block_1"),
  ]
  r = ext.compute_fold_critical_path(events)
  assert r["num_blocks"] == 2
  by_ns = {b["block_ns"]: b["modeled_critical_path_ms"] for b in r["per_block"]}
  assert by_ns == {"block_0": 1000.0, "block_1": 3000.0}
  assert r["avg_modeled_critical_path_ms"] == 2000.0


def test_critical_path_no_folds_unmeasured():
  r = ext.compute_fold_critical_path([leaf(0), leaf(1)])
  assert r["measured"] is False
  assert r["provenance"] == "UNMEASURED"
  assert r["per_block"] == []
  assert "note" in r


def test_critical_path_folds_without_prove_or_ts_unmeasured():
  # A fold with neither prove-time nor timestamps -> UNMEASURED, never faked.
  events = [_base("reduction-fold", 0, prove_ms=None, prove_time_ms=None,
                  level=1)]
  r = ext.compute_fold_critical_path(events)
  assert r["measured"] is False
  assert r["provenance"] == "UNMEASURED"


# ---------------------------------------------------------------------------
# (2) fold_parallelism
# ---------------------------------------------------------------------------
def test_parallelism_reduction_width_64():
  events = [reduction_fold(1, i, lo=i, hi=i, prove_ms=1700) for i in range(64)]
  events += [reduction_fold(2, 0, lo=0, hi=63, prove_ms=1700)]
  r = ext.compute_fold_parallelism(events)
  assert r["measured"] is True
  assert r["per_level_width"]["1"] == 64
  assert r["per_level_width"]["2"] == 1
  assert r["peak_width"] == 64
  assert r["peak_width_provenance"] == "measured-derived"


def test_parallelism_hex_width_capped_at_8():
  events = [hex_node(1, i, prove_ms=74000) for i in range(8)]
  events += [hex_node(2, 0, prove_ms=74000)]
  r = ext.compute_fold_parallelism(events)
  assert r["peak_width"] == 8
  assert r["per_level_width"]["1"] == 8


def test_parallelism_observed_peak_concurrency_sweepline():
  # Three folds; two overlap in time, one disjoint -> observed peak = 2.
  # [1,200] and [50,250] overlap; [300,400] is disjoint.
  events = [
      reduction_fold(1, 0, lo=0, hi=0, pull_ts_ms=_EPOCH + 1,
                     ts=_ts_at(200)),
      reduction_fold(1, 1, lo=1, hi=1, pull_ts_ms=_EPOCH + 50,
                     ts=_ts_at(250)),
      reduction_fold(1, 2, lo=2, hi=2, pull_ts_ms=_EPOCH + 300,
                     ts=_ts_at(400)),
  ]
  r = ext.compute_fold_parallelism(events)
  assert r["observed_peak_concurrency"] == 2
  assert r["observed_peak_concurrency_provenance"] == "observed-from-timestamps"


def test_parallelism_touching_intervals_not_double_counted():
  # One ends exactly when the next starts -> concurrency 1, not 2.
  # [epoch+0, epoch+100] then [epoch+100, epoch+200].
  events = [
      reduction_fold(1, 0, lo=0, hi=0, pull_ts_ms=_EPOCH + 0,
                     ts=_ts_at(100)),
      reduction_fold(1, 1, lo=1, hi=1, pull_ts_ms=_EPOCH + 100,
                     ts=_ts_at(200)),
  ]
  r = ext.compute_fold_parallelism(events)
  assert r["observed_peak_concurrency"] == 1


def test_parallelism_per_block_width_not_summed_across_blocks():
  # Same level in two blocks -> reported width is the per-block MAX, not the sum.
  events = [
      reduction_fold(1, 0, lo=0, hi=1, block_ns="block_0"),
      reduction_fold(1, 1, lo=2, hi=3, block_ns="block_0"),
      reduction_fold(1, 0, lo=0, hi=1, block_ns="block_1"),
  ]
  r = ext.compute_fold_parallelism(events)
  # block_0 has width 2 at L1, block_1 has width 1 -> max is 2 (not 3).
  assert r["per_level_width"]["1"] == 2
  assert r["peak_width"] == 2


def test_parallelism_missing_timestamps_observed_null():
  events = [reduction_fold(1, 0, lo=0, hi=63, prove_ms=1700)]
  r = ext.compute_fold_parallelism(events)
  assert r["peak_width"] == 1
  assert r["observed_peak_concurrency"] is None
  assert r["observed_peak_concurrency_provenance"] == "UNMEASURED"


def test_parallelism_no_folds_unmeasured():
  r = ext.compute_fold_parallelism([leaf(0)])
  assert r["measured"] is False
  assert r["peak_width"] is None
  assert r["provenance"] == "UNMEASURED"


# ---------------------------------------------------------------------------
# (3) gating_ingestion_rate
# ---------------------------------------------------------------------------
def test_ingestion_rate_events_per_sec():
  # 5 completions spanning 4000ms => 5 / 4.0 = 1.25 ev/s.
  events = [
      leaf(i, ts=f"2024-01-01T00:00:0{i}.000Z") for i in range(5)
  ]
  r = ext.compute_gating_ingestion_rate(events)
  assert r["measured"] is True
  assert r["completion_event_count"] == 5
  assert r["span_sec"] == 4.0
  assert r["events_per_sec"] == 1.25
  assert r["events_per_sec_provenance"] == "measured-derived"


def test_ingestion_rate_histogram_buckets():
  events = [
      leaf(0, ts="2024-01-01T00:00:00.000Z"),
      leaf(1, ts="2024-01-01T00:00:00.500Z"),
      leaf(2, ts="2024-01-01T00:00:01.500Z"),
  ]
  r = ext.compute_gating_ingestion_rate(events)
  assert r["ingestion_rate_per_sec_histogram"] == [
      {"t_sec": 0, "events": 2},
      {"t_sec": 1, "events": 1},
  ]


def test_ingestion_reuses_queue_wait_redrive_and_duplicates():
  events = [
      leaf(0, ts="2024-01-01T00:00:00.000Z", queue_wait_ms=100),
      leaf(1, ts="2024-01-01T00:00:01.000Z", queue_wait_ms=500,
           redriven_after_lease_expiry=True),
      # A duplicate completion of leaf_0 (same output key) -> counted once.
      leaf(0, ts="2024-01-01T00:00:01.500Z"),
  ]
  r = ext.compute_gating_ingestion_rate(events)
  c = r["counters"]
  assert c["duplicate_completions"] == 1
  assert c["redriven_after_lease_expiry_count"] == 1
  assert c["queue_wait_ms_max"] == 500
  assert c["queue_wait_provenance"] == "measured"


def test_ingestion_no_timestamps_unmeasured_but_counters_present():
  events = [
      leaf(0, redriven_after_lease_expiry=True),
      leaf(1, redriven_after_lease_expiry=False),
  ]
  r = ext.compute_gating_ingestion_rate(events)
  assert r["measured"] is False
  assert r["provenance"] == "UNMEASURED"
  assert r["events_per_sec"] is None
  # Counters need no timestamp and are still measured.
  assert r["counters"]["redriven_after_lease_expiry_count"] == 1


def test_ingestion_empty_events_unmeasured():
  r = ext.compute_gating_ingestion_rate([])
  assert r["measured"] is False
  assert r["provenance"] == "UNMEASURED"


def test_ingestion_single_event_zero_span_no_fabrication():
  r = ext.compute_gating_ingestion_rate([leaf(0, ts="2024-01-01T00:00:05.000Z")])
  # Span is 0 -> rate must be null, never a fabricated infinity.
  assert r["span_sec"] == 0.0
  assert r["events_per_sec"] is None
  assert r["events_per_sec_provenance"] == "UNMEASURED"


# ---------------------------------------------------------------------------
# Integration: the three blocks appear in compute_derived output.
# ---------------------------------------------------------------------------
def test_compute_derived_includes_three_365_blocks():
  events = [
      leaf(0, prove_ms=1000, ts="2024-01-01T00:00:00.000Z"),
      reduction_fold(1, 0, lo=0, hi=1, prove_ms=1700,
                     ts="2024-01-01T00:00:02.000Z"),
  ]
  derived = ext.compute_derived(events)
  assert "fold_critical_path" in derived
  assert "fold_parallelism" in derived
  assert "gating_ingestion_rate" in derived
  # And they are the real computed blocks, not placeholders.
  assert derived["fold_parallelism"]["peak_width"] == 1
  assert derived["fold_critical_path"]["measured"] is True


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
  sys.exit(_run_self_test())
