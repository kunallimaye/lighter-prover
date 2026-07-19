#!/usr/bin/env python3
# Copyright (c) Elliot Technologies, Inc.
# SPDX-License-Identifier: BUSL-1.1

"""Unit tests for the #377 lag-to-tip / fleet-occupancy / per-block-latency
metrics + the observed_peak_concurrency root-cause fix.

These exercise compute_fleet_occupancy / compute_lag_to_tip /
compute_per_block_latency (and the pull_ts_ms + total_time_ms completion-ts
derivation that now wires observed_peak_concurrency) in extract_gke_telemetry.py
using SYNTHETIC, clearly-labeled event dicts (the SAME event shape
parse_event_line and prover_event_json_to_event produce) + synthetic run_config
admissions[]. The fixtures are invented for deterministic math tests -- they are
NOT measured benchmark data and are NOT written under reports/. This validates
the CODE, not performance.

Edge cases covered: known overlapping/disjoint occupancy intervals -> known
per-second busy/idle + leaf:fold tag correctness; lag-to-tip flat vs rising
trend; per-block-latency p50/p95/p99; observed_peak_concurrency derived from
pull_ts_ms + total_time_ms (GCS-events path, ts=None); touching intervals not
double-counted; and every UNMEASURED anti-fabrication branch (missing
admissions[] -> lag UNMEASURED, missing pull_ts_ms -> occupancy UNMEASURED,
missing pod_count -> idle UNMEASURED).

Runs two ways:
  * pytest infra-as-code/scripts/tests/test_lag_to_tip_telemetry.py
  * python3 infra-as-code/scripts/tests/test_lag_to_tip_telemetry.py
"""

import importlib.util
import os
import sys

_HERE = os.path.dirname(os.path.abspath(__file__))
_SCRIPT = os.path.join(_HERE, "..", "extract_gke_telemetry.py")

_spec = importlib.util.spec_from_file_location("extract_gke_telemetry", _SCRIPT)
ext = importlib.util.module_from_spec(_spec)
_spec.loader.exec_module(ext)

# Epoch-ms of 2024-01-01T00:00:00.000Z. pull_ts_ms and the completion timestamp
# are both absolute epoch-ms, so fixtures express both as EPOCH + a small offset.
_EPOCH = ext._completion_ts_ms({"ts": "2024-01-01T00:00:00.000Z"})


# ---------------------------------------------------------------------------
# Synthetic event builders (match the parsed event-dict shape exactly).
# ---------------------------------------------------------------------------
def _base(role, idx, prove_ms=None, prove_time_ms=100, status="success",
          total_time_ms=None, **kw):
  e = {
      "ts": None,
      "role": role,
      "idx": idx,
      "status": status,
      "prove_time_ms": prove_time_ms,
      "gcs_time_ms": 0,
      "total_time_ms": (total_time_ms if total_time_ms is not None
                        else prove_time_ms),
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
# (0) completion-ts derivation + observed_peak_concurrency ROOT-CAUSE FIX (#377).
# The GCS-events path carries ts=None but DOES carry pull_ts_ms + total_time_ms,
# so completion (and hence observed concurrency) must be DERIVABLE from those.
# ---------------------------------------------------------------------------
def test_derive_completion_prefers_ts():
  # ev['ts'] present -> used verbatim; pull+total ignored.
  e = reduction_fold(1, 0, lo=0, hi=0, pull_ts_ms=_EPOCH + 100,
                     total_time_ms=999999, ts="2024-01-01T00:00:00.500Z")
  assert ext._derive_completion_ts_ms(e) == _EPOCH + 500


def test_derive_completion_falls_back_to_pull_plus_total():
  # ts absent (GCS-events shape) -> pull_ts_ms + total_time_ms.
  e = reduction_fold(1, 0, lo=0, hi=0, pull_ts_ms=_EPOCH + 100,
                     total_time_ms=250)
  assert ext._derive_completion_ts_ms(e) == _EPOCH + 350


def test_derive_completion_none_when_no_source():
  e = reduction_fold(1, 0, lo=0, hi=0)  # no ts, no pull_ts_ms
  assert ext._derive_completion_ts_ms(e) is None


def test_observed_peak_concurrency_from_pull_plus_total_gcs_path():
  # ROOT-CAUSE FIX: NO ev['ts'] (GCS payloads), only pull_ts_ms + total_time_ms.
  # [100,300) and [200,400) overlap -> 2; [500,600) disjoint. Previously this was
  # always UNMEASURED because completion was read only from ev['ts'].
  events = [
      reduction_fold(1, 0, lo=0, hi=0, pull_ts_ms=_EPOCH + 100, total_time_ms=200),
      reduction_fold(1, 1, lo=1, hi=1, pull_ts_ms=_EPOCH + 200, total_time_ms=200),
      reduction_fold(1, 2, lo=2, hi=2, pull_ts_ms=_EPOCH + 500, total_time_ms=100),
  ]
  r = ext.compute_fold_parallelism(events)
  assert r["observed_peak_concurrency"] == 2
  assert r["observed_peak_concurrency_provenance"] == "observed-from-timestamps"


def test_observed_peak_concurrency_touching_not_double_counted_gcs_path():
  # [0,100) then [100,200): completion==next pull -> concurrency 1 (half-open).
  events = [
      reduction_fold(1, 0, lo=0, hi=0, pull_ts_ms=_EPOCH + 0, total_time_ms=100),
      reduction_fold(1, 1, lo=1, hi=1, pull_ts_ms=_EPOCH + 100, total_time_ms=100),
  ]
  r = ext.compute_fold_parallelism(events)
  assert r["observed_peak_concurrency"] == 1


def test_observed_peak_concurrency_unmeasured_without_pull_ts():
  # No pull_ts_ms anywhere -> honest UNMEASURED (never faked).
  events = [reduction_fold(1, 0, lo=0, hi=63, prove_ms=1700)]
  r = ext.compute_fold_parallelism(events)
  assert r["observed_peak_concurrency"] is None
  assert r["observed_peak_concurrency_provenance"] == "UNMEASURED"


def test_observed_critical_path_from_pull_plus_total_gcs_path():
  # Observed critical path now works on the GCS path too (ts=None).
  events = [
      reduction_fold(1, 0, lo=0, hi=63, pull_ts_ms=_EPOCH + 1000,
                     total_time_ms=1700),
      reduction_fold(2, 0, lo=0, hi=127, pull_ts_ms=_EPOCH + 2800,
                     total_time_ms=1700),
  ]
  r = ext.compute_fold_critical_path(events)
  blk = r["per_block"][0]
  # observed = max completion (2800+1700=4500) - min pull (1000) = 3500.
  assert blk["observed_critical_path_ms"] == 3500.0
  assert blk["observed_provenance"] == "observed-from-timestamps"


# ---------------------------------------------------------------------------
# (1) fleet_occupancy
# ---------------------------------------------------------------------------
def test_occupancy_known_overlap_busy_counts():
  # leaf [0,2000): buckets 0,1 ; fold [1000,3000): buckets 1,2.
  # -> t0: leaf1 fold0 ; t1: leaf1 fold1 ; t2: leaf0 fold1.
  events = [
      leaf(0, pull_ts_ms=_EPOCH + 0, total_time_ms=2000),
      reduction_fold(1, 0, lo=0, hi=1, pull_ts_ms=_EPOCH + 1000,
                     total_time_ms=2000),
  ]
  r = ext.compute_fleet_occupancy(events)
  assert r["measured"] is True
  ps = {row["t_sec"]: row for row in r["per_second"]}
  assert (ps[0]["busy_leaf"], ps[0]["busy_fold"]) == (1, 0)
  assert (ps[1]["busy_leaf"], ps[1]["busy_fold"]) == (1, 1)
  assert (ps[2]["busy_leaf"], ps[2]["busy_fold"]) == (0, 1)
  assert r["summary"]["peak_busy"] == 2


def test_occupancy_leaf_fold_tag_correctness():
  # Two leaves + one fold overlapping in bucket 0 -> busy_leaf=2, busy_fold=1.
  events = [
      leaf(0, pull_ts_ms=_EPOCH + 0, total_time_ms=800),
      leaf(1, pull_ts_ms=_EPOCH + 100, total_time_ms=800),
      reduction_fold(1, 0, lo=0, hi=1, pull_ts_ms=_EPOCH + 200,
                     total_time_ms=800),
  ]
  r = ext.compute_fleet_occupancy(events)
  ps = {row["t_sec"]: row for row in r["per_second"]}
  assert (ps[0]["busy_leaf"], ps[0]["busy_fold"]) == (2, 1)
  # total task-seconds: 3 tasks each in exactly bucket 0.
  assert r["summary"]["total_leaf_task_seconds"] == 2
  assert r["summary"]["total_fold_task_seconds"] == 1
  assert r["summary"]["observed_busy_leaf_fold_ratio"] == 2.0


def test_occupancy_disjoint_intervals_no_overlap():
  events = [
      leaf(0, pull_ts_ms=_EPOCH + 0, total_time_ms=500),
      leaf(1, pull_ts_ms=_EPOCH + 2000, total_time_ms=500),
  ]
  r = ext.compute_fleet_occupancy(events)
  assert r["summary"]["peak_busy"] == 1


def test_occupancy_idle_with_pod_count_from_run_config():
  # pod_count=4; bucket 0 has 2 busy -> idle 2, busy_pct 50, idle_pct 50.
  events = [
      leaf(0, pull_ts_ms=_EPOCH + 0, total_time_ms=800),
      reduction_fold(1, 0, lo=0, hi=1, pull_ts_ms=_EPOCH + 0,
                     total_time_ms=800),
  ]
  r = ext.compute_fleet_occupancy(events, run_config={"pod_count": 4})
  assert r["pod_count"] == 4
  ps = {row["t_sec"]: row for row in r["per_second"]}
  assert ps[0]["idle"] == 2
  assert ps[0]["busy_pct"] == 50.0
  assert ps[0]["idle_pct"] == 50.0
  assert r["summary"]["mean_idle_pct"] == 50.0
  assert r["summary"]["idle_provenance"] == "measured-derived"


def test_occupancy_idle_unmeasured_without_pod_count():
  # No pod count derivable -> busy still reported; idle honestly UNMEASURED.
  events = [leaf(0, pull_ts_ms=_EPOCH + 0, total_time_ms=800)]
  r = ext.compute_fleet_occupancy(events)  # no run_config
  assert r["measured"] is True
  assert r["pod_count"] is None
  assert r["per_second"][0]["idle"] is None
  assert r["per_second"][0]["idle_provenance"] == "UNMEASURED"
  assert r["summary"]["idle_provenance"] == "UNMEASURED"
  assert "idle_note" in r["summary"]


def test_occupancy_unmeasured_without_pull_ts():
  # No pull_ts_ms anywhere -> occupancy UNMEASURED (cannot place on wall-clock).
  events = [leaf(0), reduction_fold(1, 0, lo=0, hi=1)]
  r = ext.compute_fleet_occupancy(events)
  assert r["measured"] is False
  assert r["provenance"] == "UNMEASURED"
  assert "pull_ts_ms" in r["note"]
  assert r["per_second"] is None


# ---------------------------------------------------------------------------
# (2) lag_to_tip
# ---------------------------------------------------------------------------
def _stream_run_config(admissions):
  return {"admission_mode": "stream", "admissions": admissions}


def test_lag_to_tip_flat_trend_keeping_up():
  # 3 blocks admitted at t=0,1s,2s; each root-verified 500ms after admission ->
  # distance stays bounded-flat (never grows).
  admissions = [
      {"block_ns": "block_0", "admit_ts_ms": _EPOCH + 0},
      {"block_ns": "block_1", "admit_ts_ms": _EPOCH + 1000},
      {"block_ns": "block_2", "admit_ts_ms": _EPOCH + 2000},
  ]
  events = [
      reduction_fold(3, 0, lo=0, hi=1, block_ns="block_0",
                     pull_ts_ms=_EPOCH + 0, total_time_ms=500),
      reduction_fold(3, 0, lo=0, hi=1, block_ns="block_1",
                     pull_ts_ms=_EPOCH + 1000, total_time_ms=500),
      reduction_fold(3, 0, lo=0, hi=1, block_ns="block_2",
                     pull_ts_ms=_EPOCH + 2000, total_time_ms=500),
  ]
  r = ext.compute_lag_to_tip(events, run_config=_stream_run_config(admissions))
  assert r["measured"] is True
  assert r["trend"]["classification"] == "bounded-flat"
  assert r["trend"]["max_distance_behind_tip"] == 1  # at most 1 in flight
  assert r["trend"]["blocks_admitted_total"] == 3
  assert r["trend"]["blocks_root_verified_total"] == 3


def test_lag_to_tip_rising_trend_falling_behind():
  # A TRUNCATED / still-in-flight run: 6 blocks admitted 1s apart (the tip keeps
  # advancing) but only the first two ever reach a root-verified event -- blocks
  # 2..5 are still in flight when telemetry was captured, so they contribute to
  # blocks_admitted (from admissions[]) but NOT to blocks_root_verified. The
  # sample span is the admission window, over which distance climbs monotonically
  # -> "monotonically-rising", the #372 fail signal. Blocks with no fold event
  # are honestly NOT counted as verified (never faked).
  admissions = [
      {"block_ns": f"block_{i}", "admit_ts_ms": _EPOCH + i * 1000}
      for i in range(6)
  ]
  events = [
      reduction_fold(3, 0, lo=0, hi=1, block_ns="block_0",
                     pull_ts_ms=_EPOCH + 400, total_time_ms=100),   # verify ~500
      reduction_fold(3, 0, lo=0, hi=1, block_ns="block_1",
                     pull_ts_ms=_EPOCH + 1400, total_time_ms=100),  # verify ~1500
  ]
  r = ext.compute_lag_to_tip(events, run_config=_stream_run_config(admissions))
  assert r["measured"] is True
  assert r["trend"]["classification"] == "monotonically-rising"
  assert r["trend"]["slope_blocks_per_sec"] > 0
  # Only 2 of the 6 admitted blocks are root-verified (rest still in flight).
  assert r["trend"]["blocks_admitted_total"] == 6
  assert r["trend"]["blocks_root_verified_total"] == 2
  ps = {row["t_sec"]: row for row in r["per_second"]}
  # By the last admission (t=5s) the backlog is 6 admitted - 2 verified = 4.
  assert ps[5]["blocks_admitted"] == 6
  assert ps[5]["blocks_root_verified"] == 2
  assert ps[5]["distance_behind_tip"] == 4


def test_lag_to_tip_unmeasured_without_admissions():
  # Batch run / pre-#378 run_config: admissions[] absent -> UNMEASURED, no tip.
  events = [
      reduction_fold(3, 0, lo=0, hi=1, block_ns="block_0",
                     pull_ts_ms=_EPOCH + 0, total_time_ms=500),
  ]
  r = ext.compute_lag_to_tip(events, run_config={"blocks": 1})
  assert r["measured"] is False
  assert r["provenance"] == "UNMEASURED"
  assert "admissions" in r["note"]
  assert r["per_second"] is None
  # None run_config also UNMEASURED.
  r2 = ext.compute_lag_to_tip(events, run_config=None)
  assert r2["measured"] is False


def test_lag_to_tip_unmeasured_empty_admissions_array():
  # admissions[] present but empty (batch mode writes []) -> UNMEASURED.
  events = [
      reduction_fold(3, 0, lo=0, hi=1, block_ns="block_0",
                     pull_ts_ms=_EPOCH + 0, total_time_ms=500),
  ]
  r = ext.compute_lag_to_tip(events, run_config={"admissions": []})
  assert r["measured"] is False
  assert "admissions" in r["note"]


def test_lag_to_tip_unmeasured_without_root_verified_ts():
  # admissions present but folds carry no derivable completion ts -> UNMEASURED.
  admissions = [{"block_ns": "block_0", "admit_ts_ms": _EPOCH + 0}]
  events = [reduction_fold(3, 0, lo=0, hi=1, block_ns="block_0")]  # no pull_ts
  r = ext.compute_lag_to_tip(events, run_config=_stream_run_config(admissions))
  assert r["measured"] is False
  assert "root-verified" in r["note"]


def test_lag_to_tip_root_is_max_level_fold():
  # A block's root-verified ts must come from the MAX-level (root) fold, not an
  # earlier level. L1 finishes at 500, L2 (root) finishes at 3000.
  admissions = [{"block_ns": "block_0", "admit_ts_ms": _EPOCH + 0}]
  events = [
      reduction_fold(1, 0, lo=0, hi=1, block_ns="block_0",
                     pull_ts_ms=_EPOCH + 0, total_time_ms=500),
      reduction_fold(2, 0, lo=0, hi=3, block_ns="block_0",
                     pull_ts_ms=_EPOCH + 1000, total_time_ms=2000),
  ]
  rv = ext._root_verified_ts_by_block(events)
  assert rv["block_0"] == _EPOCH + 3000  # L2 completion, not L1's 500


# ---------------------------------------------------------------------------
# (3) per_block_latency
# ---------------------------------------------------------------------------
def test_per_block_latency_admit_anchor_distribution():
  # 5 blocks with known latencies 100,200,300,400,500 -> p50=300, p99=500.
  latencies = [100, 200, 300, 400, 500]
  admissions = []
  events = []
  for i, lat in enumerate(latencies):
    admissions.append({"block_ns": f"block_{i}", "admit_ts_ms": _EPOCH + 0})
    events.append(reduction_fold(3, 0, lo=0, hi=1, block_ns=f"block_{i}",
                                 pull_ts_ms=_EPOCH + 0, total_time_ms=lat))
  r = ext.compute_per_block_latency(
      events, run_config=_stream_run_config(admissions))
  assert r["measured"] is True
  assert r["anchor"] == "admit_ts_ms"
  d = r["distribution_ms"]
  assert d["p50"] == 300
  assert d["p95"] == 500
  assert d["p99"] == 500
  assert d["max"] == 500
  assert r["num_blocks"] == 5


def test_per_block_latency_earliest_pull_anchor_when_no_admissions():
  # No admissions[] -> anchor is earliest leaf pull_ts_ms; latency = root - pull.
  events = [
      leaf(0, block_ns="block_0", pull_ts_ms=_EPOCH + 100, total_time_ms=50),
      leaf(1, block_ns="block_0", pull_ts_ms=_EPOCH + 300, total_time_ms=50),
      reduction_fold(3, 0, lo=0, hi=1, block_ns="block_0",
                     pull_ts_ms=_EPOCH + 400, total_time_ms=600),
  ]
  r = ext.compute_per_block_latency(events, run_config={"blocks": 1})
  assert r["measured"] is True
  assert r["anchor"] == "earliest_leaf_pull_ts_ms"
  # root-verified = 400+600 = 1000; earliest leaf pull = 100 -> latency 900.
  assert r["per_block"][0]["latency_ms"] == 900.0
  assert r["distribution_ms"]["p50"] == 900


def test_per_block_latency_unmeasured_without_root_ts():
  # No derivable root-verified ts -> UNMEASURED.
  events = [leaf(0, block_ns="block_0", pull_ts_ms=_EPOCH + 0)]
  r = ext.compute_per_block_latency(events, run_config={"blocks": 1})
  assert r["measured"] is False
  assert "root-verified" in r["note"]


def test_per_block_latency_unmeasured_without_anchor():
  # Root ts derivable but no admit ts and no leaf pull ts -> UNMEASURED.
  events = [
      reduction_fold(3, 0, lo=0, hi=1, block_ns="block_0",
                     pull_ts_ms=_EPOCH + 0, total_time_ms=500),
  ]  # a fold has a pull ts, but there is no LEAF pull and no admissions.
  r = ext.compute_per_block_latency(events, run_config={"blocks": 1})
  assert r["measured"] is False
  assert "anchor" in r["note"]


# ---------------------------------------------------------------------------
# (4) integration: compute_derived wires the three #377 blocks + threads
# run_config's admissions[] end-to-end.
# ---------------------------------------------------------------------------
def test_compute_derived_includes_377_blocks():
  events = [
      leaf(0, block_ns="block_0", pull_ts_ms=_EPOCH + 0, total_time_ms=1000),
      reduction_fold(1, 0, lo=0, hi=1, block_ns="block_0",
                     pull_ts_ms=_EPOCH + 1000, total_time_ms=500),
  ]
  admissions = [{"block_ns": "block_0", "admit_ts_ms": _EPOCH + 0}]
  derived = ext.compute_derived(
      events, run_config=_stream_run_config(admissions))
  assert "fleet_occupancy" in derived
  assert "lag_to_tip" in derived
  assert "per_block_latency" in derived
  # They are the real computed blocks (measured with these fixtures).
  assert derived["fleet_occupancy"]["measured"] is True
  assert derived["lag_to_tip"]["measured"] is True
  assert derived["per_block_latency"]["measured"] is True
  # observed_peak_concurrency is now populated from pull+total (root-cause fix).
  assert derived["fold_parallelism"]["observed_peak_concurrency"] == 1


def test_compute_derived_377_blocks_unmeasured_without_run_config():
  # No run_config at all: occupancy still measured (pull_ts present) but idle +
  # lag are honestly UNMEASURED; nothing fabricated.
  events = [
      leaf(0, block_ns="block_0", pull_ts_ms=_EPOCH + 0, total_time_ms=1000),
      reduction_fold(1, 0, lo=0, hi=1, block_ns="block_0",
                     pull_ts_ms=_EPOCH + 1000, total_time_ms=500),
  ]
  derived = ext.compute_derived(events)  # run_config=None
  assert derived["fleet_occupancy"]["measured"] is True
  assert derived["fleet_occupancy"]["summary"]["idle_provenance"] == "UNMEASURED"
  assert derived["lag_to_tip"]["measured"] is False
  assert derived["lag_to_tip"]["provenance"] == "UNMEASURED"


# ---------------------------------------------------------------------------
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
