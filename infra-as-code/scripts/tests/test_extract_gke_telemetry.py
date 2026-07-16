#!/usr/bin/env python3
# Copyright (c) Elliot Technologies, Inc.
# SPDX-License-Identifier: BUSL-1.1

"""Unit tests for extract_gke_telemetry.py parser + derivation MATH.

These tests feed the parser SYNTHETIC, clearly-labeled fixture log lines (see
tests/fixtures/*.log) and assert the derived metrics are computed correctly.
The fixtures are invented for deterministic math tests — they are NOT measured
benchmark data and are NOT written under reports/. This validates the CODE, not
performance.

Runs two ways:
  * pytest infra-as-code/scripts/tests/test_extract_gke_telemetry.py
  * python3 infra-as-code/scripts/tests/test_extract_gke_telemetry.py  (self-test)
"""

import importlib.util
import os
import sys

_HERE = os.path.dirname(os.path.abspath(__file__))
_SCRIPT = os.path.join(_HERE, "..", "extract_gke_telemetry.py")
_FIX = os.path.join(_HERE, "fixtures")

# Load the extractor module by path (it is a script, not a package).
_spec = importlib.util.spec_from_file_location("extract_gke_telemetry", _SCRIPT)
ext = importlib.util.module_from_spec(_spec)
_spec.loader.exec_module(ext)


def _parse(fixture_name):
  return ext.parse_coordinator_log_v2(os.path.join(_FIX, fixture_name))


# ---------------------------------------------------------------------------
# Back-compat: OLD (pre-#328) log parses without crashing; #328 fields -> None.
# ---------------------------------------------------------------------------
def test_old_format_back_compat():
  m = _parse("coordinator_old_format.log")
  assert m is not None
  assert m["events_parsed"] == 3
  assert m["back_compat_old_log"] is True
  assert m["leaf_proving"]["count"] == 2
  assert m["node_folding"]["count"] == 1
  # Old logs lack peak_rss -> UNMEASURED, never fabricated.
  assert m["derived"]["leaf_peak_rss"]["peak_rss_measured"] is False
  assert m["derived"]["fold_peak_rss"]["peak_rss_measured"] is False
  # prove_ms absent -> distribution falls back to prove_time_ms.
  dist = m["derived"]["leaf_prove_time_distribution_ms"]
  assert dist["measured"] is True
  assert "fallback" in dist["source_field"]
  assert m["descriptors"]["hex_root_reached"] is True


# ---------------------------------------------------------------------------
# New-format: full #328 fields parse and every derived metric is correct.
# ---------------------------------------------------------------------------
def test_new_format_peak_rss():
  m = _parse("coordinator_new_format.log")
  leaf = m["derived"]["leaf_peak_rss"]
  fold = m["derived"]["fold_peak_rss"]
  # Leaves: [4.0e9, 4.2e9, 4.1e9, 0] -> max over positives = 4.2e9, measured.
  assert leaf["peak_rss_measured"] is True
  assert leaf["peak_rss_bytes_max"] == 4_200_000_000
  assert leaf["samples_nonzero"] == 3
  # Folds (reduction): [6.0e9, 6.1e9, 1.0e9, 6.2e9] -> max = 6.2e9.
  assert fold["peak_rss_bytes_max"] == 6_200_000_000


def test_new_format_leaf_distribution():
  m = _parse("coordinator_new_format.log")
  dist = m["derived"]["leaf_prove_time_distribution_ms"]
  # prove_ms leaves sorted: [1000, 1100, 1200, 2000]
  assert dist["measured"] is True
  assert dist["count"] == 4
  assert dist["p50"] == 1100   # nearest-rank ceil(.5*4)=2 -> idx 1
  assert dist["p95"] == 2000   # ceil(.95*4)=4 -> idx 3
  assert dist["p99"] == 2000
  assert dist["max"] == 2000
  assert abs(dist["mean"] - 1325.0) < 1e-9
  assert dist["cv"] > 0.0


def test_new_format_fold_split():
  m = _parse("coordinator_new_format.log")
  split = m["derived"]["fold_time_split_ms"]
  # real fold prove_ms: [800, 850, 900]; padding-noop: [5]
  assert split["fold_kind_field_present"] is True
  assert split["real"]["count"] == 3
  assert abs(split["real"]["mean"] - 850.0) < 1e-9
  assert split["real"]["max"] == 900
  assert split["padding_noop"]["count"] == 1
  assert split["padding_noop"]["max"] == 5
  # cold (first task on pod): reduction idx0/level1 -> [800]; cached: rest.
  assert split["is_first_task_field_present"] is True
  assert split["cold_first_task_on_pod"]["count"] == 1
  assert split["cached_warm_pod"]["count"] == 3


def test_new_format_prestate_hit_rate():
  m = _parse("coordinator_new_format.log")
  ps = m["derived"]["prestate"]
  # 3 corpus, 1 replay-fallback over 4 leaves.
  assert ps["corpus_count"] == 3
  assert ps["replay_fallback_count"] == 1
  assert abs(ps["corpus_hit_rate"] - 0.75) < 1e-9
  assert ps["REGRESSION_replay_fallback_present"] is True


def test_new_format_queue_wait():
  m = _parse("coordinator_new_format.log")
  qw = m["derived"]["queue_wait"]
  # queue_wait_ms > 0: [12, 20] -> mean 16, max 20.
  assert qw["measured"] is True
  assert qw["max_ms"] == 20
  assert abs(qw["mean_ms"] - 16.0) < 1e-9


def test_new_format_wave_width_is_null():
  # BACKWARD COMPAT: this pre-#349 fixture carries pull_ms (a DURATION) but NOT
  # pull_ts_ms (absolute pull timestamp), so wave width is NOT derivable. The
  # parser must yield an honest null + a note mentioning pull_ts_ms rather than
  # misusing pull_ms as if it were a timestamp.
  m = _parse("coordinator_new_format.log")
  ww = m["derived"]["wave_width"]
  assert ww["wave_width_ms"] is None
  assert "pull_ts_ms" in ww["note"]
  # scheduling_class absent in this fixture -> honest None, never fabricated.
  assert m["descriptors"]["scheduling_class"] is None


# ---------------------------------------------------------------------------
# #349 leaf/fold OVERLAP: the post-#349 fixture carries pull_ts_ms +
# scheduling_class, so wave width + overlap are now MEASURED (not null). This
# exercises the "aggregators engage while leaves are still being produced"
# happy path (a fold pulled before the last leaf's completion timestamp).
# ---------------------------------------------------------------------------
def test_new_format_wave_width_measured():
  m = _parse("coordinator_wave_width.log")
  ww = m["derived"]["wave_width"]
  # Leaf pull_ts_ms span: [1784073600000 .. 1784073601500] -> width 1500.
  assert isinstance(ww["wave_width_ms"], int)
  assert ww["wave_width_ms"] == 1500
  assert ww["wave_width_ms"] > 0
  assert ww["note"] is None
  span = ww["leaf_pull_span"]
  assert span["min_pull_ts_ms"] == 1784073600000
  assert span["max_pull_ts_ms"] == 1784073601500
  assert span["count"] == 4


def test_new_format_fold_overlap_started_before_last_leaf():
  m = _parse("coordinator_wave_width.log")
  ov = m["derived"]["wave_width"]["fold_overlap"]
  # Last leaf completes (log ts) at 00:00:03.100Z = 1784073603100 ms.
  # First fold pulls at pull_ts_ms=1784073602000 < 1784073603100 -> overlap.
  assert ov["last_leaf_completed_ts"] == 1784073603100
  assert ov["first_fold_pulled_ts"] == 1784073602000
  assert ov["fold_started_before_last_leaf"] is True
  assert ov["overlap_ms"] == 1100
  assert ov["overlap_ms"] > 0
  assert ov["note"] is None


def test_new_format_scheduling_class_parsed():
  m = _parse("coordinator_wave_width.log")
  # scheduling_class is now emitted on the line and echoed into descriptors.
  assert m["descriptors"]["scheduling_class"] == "critical-path-first"


def test_new_format_recovery():
  m = _parse("coordinator_new_format.log")
  rec = m["derived"]["recovery"]
  assert rec["redriven_after_lease_expiry_count"] == 1
  assert rec["max_stale_lease_redrive_count"] == 1
  assert m["descriptors"]["reduction_root_reached"] is True


def test_new_format_descriptors_self_describe():
  m = _parse("coordinator_new_format.log")
  d = m["descriptors"]
  assert d["fold_strategy"] == "reduction"
  assert d["chunk_size_C"] == 4
  assert d["leaf_count_N"] == 4


def test_new_format_no_false_duplicates():
  # reduction idx=0 at level=1(span2) and level=2(span4) are DISTINCT keys.
  m = _parse("coordinator_new_format.log")
  dup = m["derived"]["duplicate_proved"]
  assert dup["duplicate_output_keys"] == 0
  assert dup["wasted_extra_events"] == 0


# ---------------------------------------------------------------------------
# Duplicate detection: same leaf output key proved twice -> 1 wasted event.
# ---------------------------------------------------------------------------
def test_duplicate_detection():
  m = _parse("coordinator_duplicate.log")
  dup = m["derived"]["duplicate_proved"]
  assert dup["duplicate_output_keys"] == 1
  assert dup["wasted_extra_events"] == 1
  assert "leaf_0" in dup["detail"]
  assert dup["detail"]["leaf_0"] == 2


# ---------------------------------------------------------------------------
# Sizing derivation: memory_requests = peak_rss_max x margin when measured.
# ---------------------------------------------------------------------------
def test_sizing_derivation_memory_requests():
  m = _parse("coordinator_new_format.log")
  sizing = ext.build_sizing_derivation(m)
  leaf_mr = sizing["memory_requests"]["leaf"]
  assert leaf_mr["measured"] is True
  expected = int(4_200_000_000 * ext.MEMORY_SAFETY_MARGIN)
  assert leaf_mr["recommended_memory_requests_bytes"] == expected
  # CPU/pods-per-node is NOT fabricated.
  assert "node metrics" in sizing["cpu_and_pods_per_node"]


def test_sizing_derivation_unmeasured_when_no_rss():
  m = _parse("coordinator_old_format.log")
  sizing = ext.build_sizing_derivation(m)
  leaf_mr = sizing["memory_requests"]["leaf"]
  assert leaf_mr["measured"] is False
  assert leaf_mr["recommended_memory_requests_bytes"] == "UNMEASURED — run with cgroup/RSS access"


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
