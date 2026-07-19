#!/usr/bin/env python3
# Copyright (c) Elliot Technologies, Inc.
# SPDX-License-Identifier: BUSL-1.1

"""Extracts fungible-pool coordinator telemetry and writes bench_summary.json.

Two input paths are supported:

  * GKE (default): pull the coordinator pod's logs via ``kubectl`` and query the
    seeder job for a start time. This is the original behaviour.
  * Local file (``--log-file PATH``, issue #321 Phase 7 / #328): parse a
    coordinator log captured from a local run with NO cluster. In this mode we
    never touch ``kubectl``/``gcloud`` and never upload to GCS.

The coordinator (``bench/src/bin/coordinator.rs``) logs ONE line per completion
event. Since #328/#321-Phase-5 that line carries the full per-task sizing
telemetry:

    Received event: role=leaf, idx=0, fold_strategy=Hex, level=0, status=success,
    prove_time_ms=1000, gcs_time_ms=200, total_time_ms=1200, peak_rss_bytes=0,
    prestate_source=corpus, is_first_task_on_pod=true, chunk_size=4, leaf_count=4,
    pull_ms=7, pre_exec_ms=0, prove_ms=990, gcs_write_ms=56, queue_wait_ms=0,
    fold_kind=n/a, merge_interval_span=0, redriven_after_lease_expiry=false

The extractor is TOLERANT of OLD (pre-#328) log lines that only carry the
original five fields (role, idx, status, prove_time_ms, gcs_time_ms,
total_time_ms): those still parse, and every #328 field simply reads as
"not present" (None) so nothing crashes and no metric is fabricated.

ANTI-FABRICATION (reports/PROVENANCE.md, enforced by ``make lint-reports``):
this script NEVER invents a number. Missing telemetry is reported as ``null`` /
``0`` / ``"UNMEASURED"`` with an explicit provenance note. Every derived number
is traceable through the ``provenance`` block to the log line(s) it came from.
"""

import argparse
import datetime
import json
import math
import os
import re
import subprocess
import sys

try:
  import tomllib  # Python 3.11+
except ImportError:
  try:
    import tomli as tomllib
  except ImportError:
    import toml as tomllib


# --- Safety margin applied to a MEASURED peak RSS to recommend memory_requests.
# Stated here (not hidden) so the report is self-describing. Not a benchmark
# number: it is a sizing policy constant, echoed into the provenance block.
MEMORY_SAFETY_MARGIN = 1.3


def parse_k8s_timestamp(ts_str):
  # Strip leading bracket if present (common in Rust env_logger output)
  ts_str = ts_str.lstrip("[")
  # Replace 'Z' with '+00:00' for compatibility with Python < 3.11
  normalized = ts_str.replace("Z", "+00:00")
  # Truncate nanoseconds if present (Python fromisoformat only supports up to 6 digits of microsec)
  # e.g., 2026-06-29T00:00:00.123456789Z -> 2026-06-29T00:00:00.123456+00:00
  match = re.match(r"^([^.]+)\.(\d+)(.*)$", normalized)
  if match:
    base, subsec, tz = match.groups()
    normalized = f"{base}.{subsec[:6]}{tz}"
  return datetime.datetime.fromisoformat(normalized)


def get_git_commit():
  """Best-effort git commit for provenance. Never fatal."""
  try:
    res = subprocess.run(
        ["git", "rev-parse", "HEAD"], capture_output=True, text=True
    )
    if res.returncode == 0:
      return res.stdout.strip()
  except Exception:
    pass
  return "unknown"


def get_job_start_time(job_name):
  cmd = ["kubectl", "get", f"job/{job_name}", "-o", "json"]
  res = subprocess.run(cmd, capture_output=True, text=True)
  if res.returncode != 0:
    print(f"[WARNING] Failed to get job {job_name}: {res.stderr}", file=sys.stderr)
    return None

  try:
    data = json.loads(res.stdout)
    status = data.get("status", {})
    start_time_str = status.get("startTime")
    if start_time_str:
      return parse_k8s_timestamp(start_time_str)
  except Exception as e:
    print(f"[WARNING] Failed to parse job {job_name} JSON: {e}", file=sys.stderr)
  return None


# ---------------------------------------------------------------------------
# Coordinator "Received event:" line parsing (back-compatible / two-stage).
# ---------------------------------------------------------------------------
#
# Stage 1 matches the ORIGINAL five fields plus the leading timestamp. This is
# the only mandatory shape, so pre-#328 logs still parse.
#
#   <ts> ... Received event: role=..., idx=..., status=..., prove_time_ms=...,
#            gcs_time_ms=..., total_time_ms=...
#
# `fold_strategy=<Debug>` and `level=<n>` sit BETWEEN idx and status in the new
# format, so the anchor tolerates their optional presence via `.*?`.
_EVENT_PREFIX_RE = re.compile(
    r"^(?P<ts>[^\s]+)\s+.*Received event: "
    r"role=(?P<role>[\w-]+), idx=(?P<idx>\d+),.*?"
    r"status=(?P<status>\w+), "
    r"prove_time_ms=(?P<prove_time_ms>\d+), "
    r"gcs_time_ms=(?P<gcs_time_ms>\d+), "
    r"total_time_ms=(?P<total_time_ms>\d+)"
)

# Stage 2: individual optional #328 fields, each scraped independently so any
# subset can be present/absent without breaking the others. `key=value` scanning
# keeps this forward-compatible if the coordinator adds still-more fields later.
_FOLD_STRATEGY_RE = re.compile(r"fold_strategy=(?P<v>\w+)")
_LEVEL_RE = re.compile(r"[,\s]level=(?P<v>\d+)")
_PEAK_RSS_RE = re.compile(r"peak_rss_bytes=(?P<v>\d+)")
_PRESTATE_RE = re.compile(r"prestate_source=(?P<v>[\w/-]+)")
_FIRST_TASK_RE = re.compile(r"is_first_task_on_pod=(?P<v>true|false)")
_CHUNK_SIZE_RE = re.compile(r"chunk_size=(?P<v>\d+)")
_LEAF_COUNT_RE = re.compile(r"leaf_count=(?P<v>\d+)")
_PULL_MS_RE = re.compile(r"pull_ms=(?P<v>\d+)")
_PRE_EXEC_MS_RE = re.compile(r"pre_exec_ms=(?P<v>\d+)")
_PROVE_MS_RE = re.compile(r"[,\s]prove_ms=(?P<v>\d+)")
_GCS_WRITE_MS_RE = re.compile(r"gcs_write_ms=(?P<v>\d+)")
_QUEUE_WAIT_MS_RE = re.compile(r"queue_wait_ms=(?P<v>\d+)")
_FOLD_KIND_RE = re.compile(r"fold_kind=(?P<v>[\w/-]+)")
_MERGE_SPAN_RE = re.compile(r"merge_interval_span=(?P<v>\d+)")
_REDRIVEN_RE = re.compile(r"redriven_after_lease_expiry=(?P<v>true|false)")
# #349: absolute epoch-ms pull TIMESTAMP (not the pull DURATION `pull_ms`), plus
# the seed-order class. Both are already on the wire and, since #349, emitted on
# the coordinator line. Absent in pre-#349 logs -> None (honest "not present").
_PULL_TS_MS_RE = re.compile(r"pull_ts_ms=(?P<v>\d+)")
_SCHEDULING_CLASS_RE = re.compile(r"scheduling_class=(?P<v>[\w-]+)")
# #355: the per-replay BLOCK NAMESPACE this event's descriptor belongs to
# (`block_3`, or the literal `<base>` for a single-block / un-namespaced run).
# Emitted on the coordinator line since #355. Absent in pre-#355 logs -> None
# (honest "not present"). Used to COUNT distinct blocks actually observed and
# cross-check that against the run_config `blocks` divisor (anti-fabrication) and
# to attribute cold replay-0 vs warm later replays. The value is `block_<n>` or
# `<base>`; the angle brackets are matched explicitly so a `<base>` sentinel is
# captured verbatim and distinguished from a real `block_N` namespace.
_BLOCK_NS_RE = re.compile(r"block_ns=(?P<v>(?:block_\d+|<base>|[\w-]+))")

# Ancillary markers.
_ROOT_HEX_RE = re.compile(r"^([^\s]+)\s+.*ROOT REACHED!")
_ROOT_REDUCTION_RE = re.compile(r"REDUCTION ROOT REACHED \[0, (?P<padded_last>\d+)\]")
_STALE_REDRIVE_RE = re.compile(r"stale_lease_redrive_count=(?P<v>\d+)")


def _opt_int(regex, line):
  m = regex.search(line)
  return int(m.group("v")) if m else None


def _opt_bool(regex, line):
  m = regex.search(line)
  if not m:
    return None
  return m.group("v") == "true"


def _opt_str(regex, line):
  m = regex.search(line)
  return m.group("v") if m else None


def parse_event_line(line):
  """Return a dict of the parsed event, or None if the line is not an event.

  Mandatory fields (original five + timestamp) come from `_EVENT_PREFIX_RE`.
  Every #328 field is OPTIONAL: absent -> None (honest "not present"), never a
  fabricated default.
  """
  m = _EVENT_PREFIX_RE.match(line)
  if not m:
    return None

  # `fold_strategy` is Rust Debug (`Hex`/`Reduction`); normalize to lower.
  fold_strategy = _opt_str(_FOLD_STRATEGY_RE, line)
  if fold_strategy is not None:
    fold_strategy = fold_strategy.lower()

  return {
      "ts": m.group("ts"),
      "role": m.group("role"),
      "idx": int(m.group("idx")),
      "status": m.group("status"),
      "prove_time_ms": int(m.group("prove_time_ms")),
      "gcs_time_ms": int(m.group("gcs_time_ms")),
      "total_time_ms": int(m.group("total_time_ms")),
      # ---- #328 optional per-task sizing fields (None if absent) ----
      "fold_strategy": fold_strategy,
      "level": _opt_int(_LEVEL_RE, line),
      "peak_rss_bytes": _opt_int(_PEAK_RSS_RE, line),
      "prestate_source": _opt_str(_PRESTATE_RE, line),
      "is_first_task_on_pod": _opt_bool(_FIRST_TASK_RE, line),
      "chunk_size": _opt_int(_CHUNK_SIZE_RE, line),
      "leaf_count": _opt_int(_LEAF_COUNT_RE, line),
      "pull_ms": _opt_int(_PULL_MS_RE, line),
      "pre_exec_ms": _opt_int(_PRE_EXEC_MS_RE, line),
      "prove_ms": _opt_int(_PROVE_MS_RE, line),
      "gcs_write_ms": _opt_int(_GCS_WRITE_MS_RE, line),
      "queue_wait_ms": _opt_int(_QUEUE_WAIT_MS_RE, line),
      "fold_kind": _opt_str(_FOLD_KIND_RE, line),
      "merge_interval_span": _opt_int(_MERGE_SPAN_RE, line),
      "redriven_after_lease_expiry": _opt_bool(_REDRIVEN_RE, line),
      # ---- #349 leaf/fold overlap fields (None if absent in pre-#349 logs) ----
      "pull_ts_ms": _opt_int(_PULL_TS_MS_RE, line),
      "scheduling_class": _opt_str(_SCHEDULING_CLASS_RE, line),
      # ---- #355 per-replay block namespace (None if absent in pre-#355 logs) ----
      "block_ns": _opt_str(_BLOCK_NS_RE, line),
  }


# ---------------------------------------------------------------------------
# Statistics helpers (pure math over REAL measured lists; empty -> honest zero).
# ---------------------------------------------------------------------------
def stats(lst):
  if not lst:
    return {"min": 0.0, "max": 0.0, "avg": 0.0, "total": 0.0, "count": 0}
  return {
      "min": min(lst),
      "max": max(lst),
      "avg": sum(lst) / len(lst),
      "total": sum(lst),
      "count": len(lst),
  }


def _percentile(sorted_vals, pct):
  """Nearest-rank percentile over an ALREADY-SORTED, non-empty list."""
  if not sorted_vals:
    return None
  if len(sorted_vals) == 1:
    return sorted_vals[0]
  rank = math.ceil(pct / 100.0 * len(sorted_vals))
  rank = max(1, min(rank, len(sorted_vals)))
  return sorted_vals[rank - 1]


def distribution(vals):
  """P50/P95/P99/max/mean/CV over a list of REAL measurements.

  Returns a dict with `measured: False` when the list is empty so a report can
  honestly say "not measured" rather than print a fabricated zero-distribution.
  """
  if not vals:
    return {
        "measured": False,
        "count": 0,
        "p50": None, "p95": None, "p99": None,
        "max": None, "mean": None, "cv": None,
    }
  s = sorted(vals)
  mean = sum(s) / len(s)
  if len(s) > 1 and mean > 0:
    var = sum((x - mean) ** 2 for x in s) / len(s)
    cv = math.sqrt(var) / mean
  else:
    cv = 0.0
  return {
      "measured": True,
      "count": len(s),
      "p50": _percentile(s, 50),
      "p95": _percentile(s, 95),
      "p99": _percentile(s, 99),
      "max": max(s),
      "mean": mean,
      "cv": cv,
  }


def _mean_max(vals):
  if not vals:
    return {"count": 0, "mean": None, "max": None}
  return {"count": len(vals), "mean": sum(vals) / len(vals), "max": max(vals)}


# ---------------------------------------------------------------------------
# Fold PARALLELISM telemetry (#365). The existing derived block above captures
# aggregate CPU cost (core-sec/block, prove-time distributions); it does NOT
# capture the WALL-CLOCK / CONCURRENCY advantage of the radix-2 `reduction` fold
# strategy over radix-16 `hex`. These three blocks close that gap so the
# reduction thesis (umbrella #366) can be proven on both axes:
#
#   (1) fold_critical_path   -- sum of per-level MAX fold prove-time = the true
#                               serialized wall-clock through the fold tree
#                               (hex ~2 deep x ~74s vs reduction ~7 deep x ~1.7s).
#   (2) fold_parallelism     -- per-level fold width + peak concurrent width
#                               (hex caps ~8, reduction reaches ~64).
#   (3) gating_ingestion_rate-- completion events ingested/sec + reset/redrive/
#                               duplicate counters (coordinator-scaling signal).
#
# ANTI-FABRICATION: every number traces to a REAL measured field on a parsed
# event (see parse_event_line / prover_event_json_to_event). The fold role set,
# the prove_ms->prove_time_ms fallback, the pull_ts_ms 0/None sentinel, the
# completion-timestamp derivation (parse_k8s_timestamp(ev['ts'])), and the
# per-block `block_ns` grouping all MATCH compute_derived exactly. When a source
# field is absent the block is still emitted but with null values + a
# machine-readable "UNMEASURED" note (mirroring the all-zero-RSS / partial-
# pull_ts guards above) -- never a fabricated or back-filled value.
# ---------------------------------------------------------------------------

# Fold roles, shared verbatim with compute_derived / build_metrics_from_events.
_FOLD_ROLES = ("node", "tree-node", "reduction", "reduction-fold")


def _fold_events(events):
  """Successful fold (aggregation) events, using the SAME role set + status
  filter as compute_derived so the two blocks describe the same population."""
  return [
      e for e in events
      if e["role"] in _FOLD_ROLES and e["status"] == "success"
  ]


def _fold_prove_ms(e):
  """Fold prove-time in ms: prove_ms, falling back to prove_time_ms on OLD logs
  that predate the split prove_ms field (identical to compute_derived's
  _fold_prove). Returns None only if BOTH are absent."""
  v = e.get("prove_ms")
  if v is not None:
    return float(v)
  v = e.get("prove_time_ms")
  return float(v) if v is not None else None


def _block_ns_key(e):
  """Per-block grouping key. Since #355 the coordinator stamps `block_ns`
  (e.g. `block_0`). A None/empty block_ns (single-block runs / pre-#355 logs)
  groups under one synthetic namespace -- byte-for-byte the same grouping the
  duplicate detector uses, so single-block behaviour is unchanged."""
  ns = e.get("block_ns")
  return ns if ns else "__single_block__"


def _completion_ts_ms(e):
  """Completion timestamp (epoch-ms) from the coordinator log line ev['ts'], or
  None when unparseable/absent (events-GCS payloads carry ts=None)."""
  ts = e.get("ts")
  if ts is None:
    return None
  try:
    dt = parse_k8s_timestamp(ts)
  except Exception:  # noqa: BLE001 -- honest None on any parse failure
    return None
  return int(dt.timestamp() * 1000)


def _pull_ts_ms(e):
  """Absolute pull timestamp in epoch-ms, or None when unstamped (0/None is the
  coordinator's honest 'dispatch time not recorded' sentinel)."""
  v = e.get("pull_ts_ms")
  return v if (v is not None and v > 0) else None


def _sweepline_peak_concurrency(intervals):
  """Max number of simultaneously-open [start, end) intervals (a sweep-line).
  End is processed BEFORE a start at the same timestamp, so a task that finishes
  exactly when another begins is NOT counted as concurrent."""
  points = []
  for start, end in intervals:
    points.append((start, 1))
    points.append((end, -1))
  # -1 (ends) sort before +1 (starts) at an equal timestamp.
  points.sort(key=lambda p: (p[0], p[1]))
  cur = peak = 0
  for _, delta in points:
    cur += delta
    if cur > peak:
      peak = cur
  return peak


def compute_fold_critical_path(events):
  """(#365) Per-block fold critical-path wall-clock = sum over tree levels of the
  MAX fold prove-time at that level.

  A tree level cannot finish until its SLOWEST fold finishes, and the next level
  cannot start until the level below it is done, so the true serialized critical
  path through the fold tree is ``sum_levels(max_folds(prove_time))``. This is
  the metric that exposes hex (~2 levels x ~74s = ~148s) vs reduction (~7 levels
  x ~1.7s = ~12s) -- an advantage the aggregate core-sec/block metric hides.

  When wall-clock timestamps exist (pull_ts_ms + a parseable completion ts) an
  OBSERVED critical path (max completion - min pull over the block's folds) is
  ALSO reported, clearly distinguished from the prove-time-modeled figure.
  Absent fields -> the corresponding value is null + "UNMEASURED", never faked.
  """
  folds = _fold_events(events)
  if not folds:
    return {
        "measured": False,
        "provenance": "UNMEASURED",
        "note": "no fold events (role in node/tree-node/reduction/reduction-fold)",
        "per_block": [],
        "avg_modeled_critical_path_ms": None,
        "avg_observed_critical_path_ms": None,
    }

  # block_ns -> level -> [prove_ms]; and block_ns -> ([pull_ts], [completion_ts]).
  by_block_level = {}
  ts_by_block = {}
  saw_prove = False
  saw_ts = False

  for e in folds:
    bk = _block_ns_key(e)
    lvl = e.get("level")
    pm = _fold_prove_ms(e)
    if lvl is not None and pm is not None:
      saw_prove = True
      by_block_level.setdefault(bk, {}).setdefault(int(lvl), []).append(pm)
    pull = _pull_ts_ms(e)
    comp = _completion_ts_ms(e)
    if pull is not None and comp is not None:
      saw_ts = True
      pulls, comps = ts_by_block.setdefault(bk, ([], []))
      pulls.append(pull)
      comps.append(comp)

  if not saw_prove and not saw_ts:
    return {
        "measured": False,
        "provenance": "UNMEASURED",
        "note": (
            "fold events lack a level+prove_time AND lack (pull_ts_ms, "
            "parseable completion ts); critical path not derivable"
        ),
        "per_block": [],
        "avg_modeled_critical_path_ms": None,
        "avg_observed_critical_path_ms": None,
    }

  per_block = []
  modeled_totals = []
  observed_totals = []
  all_blocks = sorted(set(by_block_level) | set(ts_by_block), key=str)

  for bk in all_blocks:
    entry = {"block_ns": None if bk == "__single_block__" else bk}

    levels = by_block_level.get(bk)
    if levels:
      per_level_max = {lvl: max(v) for lvl, v in levels.items()}
      modeled = float(sum(per_level_max.values()))
      entry["num_levels"] = len(per_level_max)
      entry["per_level_max_prove_ms"] = {
          str(k): per_level_max[k] for k in sorted(per_level_max)
      }
      entry["modeled_critical_path_ms"] = modeled
      entry["modeled_provenance"] = "modeled-from-prove-times"
      modeled_totals.append(modeled)
    else:
      entry["num_levels"] = None
      entry["per_level_max_prove_ms"] = None
      entry["modeled_critical_path_ms"] = None
      entry["modeled_provenance"] = "UNMEASURED"

    tspair = ts_by_block.get(bk)
    if tspair and tspair[0] and tspair[1]:
      pulls, comps = tspair
      observed = float(max(comps) - min(pulls))
      entry["observed_critical_path_ms"] = observed
      entry["observed_provenance"] = "observed-from-timestamps"
      observed_totals.append(observed)
    else:
      entry["observed_critical_path_ms"] = None
      entry["observed_provenance"] = "UNMEASURED"

    per_block.append(entry)

  return {
      "measured": True,
      "provenance": "measured-derived",
      "definition": (
          "critical_path = sum over tree levels of the MAX fold prove_time at "
          "that level (a level cannot complete until its slowest fold does)"
      ),
      "num_blocks": len(per_block),
      "per_block": per_block,
      "avg_modeled_critical_path_ms": (
          sum(modeled_totals) / len(modeled_totals) if modeled_totals else None
      ),
      "avg_observed_critical_path_ms": (
          sum(observed_totals) / len(observed_totals) if observed_totals else None
      ),
  }


def compute_fold_parallelism(events):
  """(#365) Per-level fold width + peak concurrent fold width.

  ``per_level_width`` counts folds at each tree level; ``peak_width`` is the
  widest level = the maximum AVAILABLE fold parallelism (hex caps ~8 at the
  fan-in bound; reduction reaches ~64 at the base level). When fold events carry
  BOTH pull_ts_ms and a parseable completion ts, ``observed_peak_concurrency``
  reports the TRUE simultaneously-in-flight maximum via a timestamp sweep-line
  (which can be lower than peak_width if the pool is under-provisioned).

  Widths are grouped by block_ns and the peak is taken over the whole run so a
  multi-block run does not inflate a single level's width. Absent fields -> null
  + "UNMEASURED"; nothing is fabricated.
  """
  folds = _fold_events(events)
  if not folds:
    return {
        "measured": False,
        "provenance": "UNMEASURED",
        "note": "no fold events (role in node/tree-node/reduction/reduction-fold)",
        "per_level_width": None,
        "peak_width": None,
        "observed_peak_concurrency": None,
        "observed_peak_concurrency_provenance": "UNMEASURED",
    }

  # Per (block_ns, level) width so cross-block same-level folds are not summed
  # into a false wider level; the reported per_level_width is the MAX width seen
  # for that level across blocks (the available parallelism per block).
  width_by_block_level = {}
  intervals = []
  saw_level = False
  saw_ts = False

  for e in folds:
    lvl = e.get("level")
    if lvl is not None:
      saw_level = True
      key = (_block_ns_key(e), int(lvl))
      width_by_block_level[key] = width_by_block_level.get(key, 0) + 1
    pull = _pull_ts_ms(e)
    comp = _completion_ts_ms(e)
    if pull is not None and comp is not None:
      saw_ts = True
      intervals.append((pull, comp))

  if not saw_level and not saw_ts:
    return {
        "measured": False,
        "provenance": "UNMEASURED",
        "note": (
            "fold events lack a level field AND lack (pull_ts_ms, parseable "
            "completion ts); fold width/concurrency not derivable"
        ),
        "per_level_width": None,
        "peak_width": None,
        "observed_peak_concurrency": None,
        "observed_peak_concurrency_provenance": "UNMEASURED",
    }

  if saw_level:
    max_width_per_level = {}
    for (_bk, lvl), w in width_by_block_level.items():
      if w > max_width_per_level.get(lvl, 0):
        max_width_per_level[lvl] = w
    per_level_width = {
        str(lvl): max_width_per_level[lvl] for lvl in sorted(max_width_per_level)
    }
    peak_width = max(max_width_per_level.values())
  else:
    per_level_width = None
    peak_width = None

  if saw_ts and intervals:
    observed_peak = _sweepline_peak_concurrency(intervals)
    observed_prov = "observed-from-timestamps"
  else:
    observed_peak = None
    observed_prov = "UNMEASURED"

  return {
      "measured": True,
      "provenance": "measured-derived",
      "definition": (
          "per_level_width = folds at each tree level (max across blocks); "
          "peak_width = widest level (available parallelism); "
          "observed_peak_concurrency = max simultaneously in-flight folds via a "
          "pull_ts_ms->completion-ts sweep-line"
      ),
      "per_level_width": per_level_width,
      "peak_width": peak_width,
      "peak_width_provenance": (
          "measured-derived" if peak_width is not None else "UNMEASURED"
      ),
      "observed_peak_concurrency": observed_peak,
      "observed_peak_concurrency_provenance": observed_prov,
  }


def compute_gating_ingestion_rate(events):
  """(#365) Coordinator completion-event ingestion rate + resilience counters.

  ingestion_rate = completion-event count / event-time span (seconds), derived
  from the coordinator log line timestamps (ev['ts']). This is the coordinator-
  scaling signal: how fast the single coordinator drains the completion stream.
  A per-second histogram is reported when timestamps exist so backlog build-up
  (a falling rate over time) is visible.

  Resilience counters REUSE the fields the extractor already surfaces rather
  than duplicating them: queue_wait_ms (dispatch backlog), redriven_after_lease
  _expiry (redeliveries), and the duplicate-output-key count from the same
  output_key() identity compute_derived uses. Absent timestamps -> rate is null
  + "UNMEASURED" but the counters (which need no timestamp) are still reported.
  """
  # Reuse queue_wait exactly as compute_derived measures it (positive-only; 0 is
  # the coordinator's 'dispatch timestamp not stamped' sentinel).
  qw_positive = [
      e["queue_wait_ms"] for e in events
      if e.get("queue_wait_ms") is not None and e["queue_wait_ms"] > 0
  ]
  qw_field_present = any(e.get("queue_wait_ms") is not None for e in events)
  redriven = sum(
      1 for e in events if e.get("redriven_after_lease_expiry") is True
  )
  redrive_field_present = any(
      e.get("redriven_after_lease_expiry") is not None for e in events
  )

  # Duplicate completions = same logical output proved by >1 event. Reuse the
  # SAME identity compute_derived's duplicate_proved uses (block_ns-prefixed).
  def _output_key(e):
    ns = e.get("block_ns") or ""
    role = e["role"]
    if role == "leaf":
      return f"{ns}|leaf_{e['idx']}"
    if role in ("node", "tree-node"):
      return f"{ns}|tree_L{e.get('level')}_N{e['idx']}"
    if role in ("reduction", "reduction-fold"):
      lo, hi = e.get("lo"), e.get("hi")
      if lo is not None and hi is not None:
        return f"{ns}|reduction_L{e.get('level')}_lo{lo}_hi{hi}"
      return f"{ns}|reduction_{e['idx']}_span{e.get('merge_interval_span')}"
    return f"{ns}|{role}_{e['idx']}"

  key_counts = {}
  for e in events:
    if e["status"] == "success":
      k = _output_key(e)
      key_counts[k] = key_counts.get(k, 0) + 1
  duplicate_completions = sum(c - 1 for c in key_counts.values() if c > 1)

  counters = {
      "duplicate_completions": duplicate_completions,
      "redriven_after_lease_expiry_count": redriven,
      "redriven_field_present": redrive_field_present,
      "queue_wait_field_present": qw_field_present,
      "queue_wait_ms_mean": (
          sum(qw_positive) / len(qw_positive) if qw_positive else None
      ),
      "queue_wait_ms_max": max(qw_positive) if qw_positive else None,
      "queue_wait_provenance": "measured" if qw_positive else "UNMEASURED",
  }

  completion_ts = [
      t for t in (_completion_ts_ms(e) for e in events
                  if e["status"] == "success") if t is not None
  ]

  if not completion_ts:
    return {
        "measured": False,
        "provenance": "UNMEASURED",
        "note": (
            "no parseable completion timestamps (ev['ts']); ingestion rate not "
            "derivable (events-GCS payloads carry ts=None). Counters below need "
            "no timestamp and are still measured."
        ),
        "completion_event_count": len(completion_ts),
        "span_sec": None,
        "events_per_sec": None,
        "events_per_sec_provenance": "UNMEASURED",
        "ingestion_rate_per_sec_histogram": None,
        "counters": counters,
    }

  span_ms = max(completion_ts) - min(completion_ts)
  span_sec = span_ms / 1000.0
  count = len(completion_ts)
  # A single instantaneous completion has no measurable span: rate is null, not
  # a fabricated infinity (mirrors the wall_sec>0 guards elsewhere).
  events_per_sec = (count / span_sec) if span_sec > 0 else None

  histogram = None
  if span_sec > 0:
    origin = min(completion_ts)
    buckets = {}
    for t in completion_ts:
      b = int((t - origin) // 1000)
      buckets[b] = buckets.get(b, 0) + 1
    histogram = [{"t_sec": b, "events": buckets[b]} for b in sorted(buckets)]

  return {
      "measured": True,
      "provenance": "measured-derived",
      "definition": (
          "events_per_sec = successful completion-event count / completion-"
          "timestamp span (sec); histogram buckets completions per 1s window "
          "from the first completion to reveal backlog build-up"
      ),
      "completion_event_count": count,
      "span_sec": span_sec,
      "events_per_sec": events_per_sec,
      "events_per_sec_provenance": (
          "measured-derived" if events_per_sec is not None else "UNMEASURED"
      ),
      "ingestion_rate_per_sec_histogram": histogram,
      "counters": counters,
  }


# ---------------------------------------------------------------------------
# Derived sizing metrics (#328 §C). Everything here is computed from the parsed
# REAL events; no constant is invented.
# ---------------------------------------------------------------------------
def compute_derived(events):
  leaves = [e for e in events if e["role"] == "leaf" and e["status"] == "success"]
  folds = [
      e for e in events
      if e["role"] in ("node", "tree-node", "reduction", "reduction-fold")
      and e["status"] == "success"
  ]

  def _present_positive(evts, key):
    """Values of `key` that are present (not None) and > 0."""
    return [e[key] for e in evts if e.get(key) is not None and e[key] > 0]

  def _present(evts, key):
    return [e[key] for e in evts if e.get(key) is not None]

  # ---- Per-role peak RSS MAX (memory_requests figure). Skip 0s (0 = not
  # measured; do NOT let an honest 0 mask a real max, and do NOT report a fake
  # max when nothing was measured). ----
  def peak_rss(evts):
    positive = _present_positive(evts, "peak_rss_bytes")
    any_field = [e for e in evts if e.get("peak_rss_bytes") is not None]
    measured = len(positive) > 0
    return {
        "peak_rss_bytes_max": max(positive) if positive else 0,
        "peak_rss_measured": measured,
        "samples_with_field": len(any_field),
        "samples_nonzero": len(positive),
    }

  leaf_rss = peak_rss(leaves)
  fold_rss = peak_rss(folds)

  # ---- Leaf prove-time distribution (prove_ms; fall back to prove_time_ms for
  # OLD logs that lack the split prove_ms field). ----
  leaf_prove_ms = _present(leaves, "prove_ms")
  leaf_prove_source = "prove_ms"
  if not leaf_prove_ms:
    leaf_prove_ms = [e["prove_time_ms"] for e in leaves]
    leaf_prove_source = "prove_time_ms (fallback: prove_ms absent in log)"
  leaf_prove_dist = distribution(leaf_prove_ms)
  leaf_prove_dist["source_field"] = leaf_prove_source

  # ---- Fold-time split: real vs padding-noop (by fold_kind); cold vs cached
  # (by is_first_task_on_pod). Uses prove_ms with prove_time_ms fallback. ----
  def _fold_prove(e):
    return e["prove_ms"] if e.get("prove_ms") is not None else e["prove_time_ms"]

  real_fold = [_fold_prove(e) for e in folds if e.get("fold_kind") == "real"]
  noop_fold = [_fold_prove(e) for e in folds if e.get("fold_kind") == "padding-noop"]
  fold_kind_known = any(e.get("fold_kind") is not None for e in folds)

  cold_fold = [_fold_prove(e) for e in folds if e.get("is_first_task_on_pod") is True]
  cached_fold = [_fold_prove(e) for e in folds if e.get("is_first_task_on_pod") is False]
  first_task_known = any(e.get("is_first_task_on_pod") is not None for e in folds)

  fold_split = {
      "fold_kind_field_present": fold_kind_known,
      "real": _mean_max(real_fold),
      "padding_noop": _mean_max(noop_fold),
      "is_first_task_field_present": first_task_known,
      "cold_first_task_on_pod": _mean_max(cold_fold),
      "cached_warm_pod": _mean_max(cached_fold),
  }

  # ---- Prestate hit rate (leaf prestate_source corpus vs replay-fallback). A
  # single replay-fallback is a regression flag. ----
  prestate_vals = _present(leaves, "prestate_source")
  corpus = sum(1 for v in prestate_vals if v == "corpus")
  replay = sum(1 for v in prestate_vals if v == "replay-fallback")
  other = len(prestate_vals) - corpus - replay
  prestate = {
      "prestate_field_present": len(prestate_vals) > 0,
      "corpus_count": corpus,
      "replay_fallback_count": replay,
      "other_count": other,
      "corpus_hit_rate": (corpus / len(prestate_vals)) if prestate_vals else None,
      "REGRESSION_replay_fallback_present": replay > 0,
  }

  # ---- Wave width + leaf/fold overlap (#349). Since #349 the coordinator line
  # carries pull_ts_ms (absolute epoch-ms pull TIMESTAMP), so we can now measure
  # how spread-out the leaf pulls were and whether fold workers engaged while
  # leaves were still being produced. Every number here is null (never 0, never
  # fabricated) when the required timestamp is absent, with an explicit note. ----

  def _pull_ts(e):
    """Absolute pull timestamp in epoch-ms, or None if unstamped (0/None)."""
    v = e.get("pull_ts_ms")
    return v if (v is not None and v > 0) else None

  def _event_ts_ms(e):
    """Coordinator log/completion timestamp (ev['ts']) in epoch-ms, or None."""
    try:
      dt = parse_k8s_timestamp(e["ts"])
    except Exception:
      return None
    return int(dt.timestamp() * 1000)

  leaf_pull_ts = [_pull_ts(e) for e in leaves]
  leaf_pull_ts_present = [t for t in leaf_pull_ts if t is not None]
  all_leaves_stamped = bool(leaves) and all(t is not None for t in leaf_pull_ts)

  if all_leaves_stamped:
    wave_min, wave_max = min(leaf_pull_ts_present), max(leaf_pull_ts_present)
    wave_width = {
        "wave_width_ms": wave_max - wave_min,
        "leaf_pull_span": {
            "min_pull_ts_ms": wave_min,
            "max_pull_ts_ms": wave_max,
            "count": len(leaf_pull_ts_present),
        },
        "note": None,
    }
  else:
    # Partial or fully-absent pull_ts_ms -> do NOT fabricate from partial data.
    wave_width = {
        "wave_width_ms": None,
        "leaf_pull_span": {
            "min_pull_ts_ms": min(leaf_pull_ts_present) if leaf_pull_ts_present else None,
            "max_pull_ts_ms": max(leaf_pull_ts_present) if leaf_pull_ts_present else None,
            "count": len(leaf_pull_ts_present),
        },
        "note": (
            "not derivable: one or more leaf events lack pull_ts_ms (absolute "
            "pull timestamp; 0/None honest sentinel). Wave width needs every "
            "leaf's pull_ts_ms; refusing to fabricate from partial data."
        ),
    }

  # ---- fold_overlap: does the fold phase begin before the last leaf completes?
  # last_leaf_completed_ts = max leaf event (completion/log) TIMESTAMP.
  # first_fold_pulled_ts   = min pull_ts_ms over FOLD events (role != leaf AND
  #                          role != root-coordinator) — robust to hex tree-node
  #                          and reduction fold role strings alike.
  leaf_completed_ts = [t for t in (_event_ts_ms(e) for e in leaves) if t is not None]
  fold_events = [
      e for e in events
      if e["status"] == "success"
      and e["role"] != "leaf"
      and e["role"] != "root-coordinator"
  ]
  fold_pull_ts = [t for t in (_pull_ts(e) for e in fold_events) if t is not None]

  last_leaf_completed_ts = max(leaf_completed_ts) if leaf_completed_ts else None
  first_fold_pulled_ts = min(fold_pull_ts) if fold_pull_ts else None

  if last_leaf_completed_ts is not None and first_fold_pulled_ts is not None:
    fold_overlap = {
        "last_leaf_completed_ts": last_leaf_completed_ts,
        "first_fold_pulled_ts": first_fold_pulled_ts,
        "fold_started_before_last_leaf": first_fold_pulled_ts < last_leaf_completed_ts,
        "overlap_ms": max(0, last_leaf_completed_ts - first_fold_pulled_ts),
        "note": None,
    }
  else:
    missing = []
    if last_leaf_completed_ts is None:
      missing.append("no parseable leaf completion timestamp (ev['ts'])")
    if first_fold_pulled_ts is None:
      missing.append("no fold event carried pull_ts_ms")
    fold_overlap = {
        "last_leaf_completed_ts": last_leaf_completed_ts,
        "first_fold_pulled_ts": first_fold_pulled_ts,
        "fold_started_before_last_leaf": None,
        "overlap_ms": None,
        "note": "not derivable: " + "; ".join(missing) + ".",
    }
  wave_width["fold_overlap"] = fold_overlap

  # ---- queue_wait: mean/max over queue_wait_ms > 0 (else not measured). The
  # honest sentinel from the coordinator is 0 => dispatch time not stamped. ----
  qw_positive = _present_positive(events, "queue_wait_ms")
  qw_field_present = any(e.get("queue_wait_ms") is not None for e in events)
  queue_wait = {
      "queue_wait_field_present": qw_field_present,
      "measured": len(qw_positive) > 0,
      "mean_ms": (sum(qw_positive) / len(qw_positive)) if qw_positive else None,
      "max_ms": max(qw_positive) if qw_positive else None,
      "note": None if qw_positive else (
          "not measured: all queue_wait_ms == 0 (dispatch timestamp not stamped)"
      ),
  }

  # ---- Duplicate-proved count: group successful events by output-key-equivalent
  # and count keys proved by >1 event (effective vs wasted compute). ----
  def output_key(e):
    # #360 (follow-up to #357): the per-replay block namespace is PREPENDED to the
    # output identity so cross-block same-geometry tasks are NOT counted as false
    # duplicates. In a genuine N-block run every block proves the SAME geometry
    # (block_0's leaf_0 and block_1's leaf_0 are DISTINCT tasks, not duplicates);
    # without block_ns here `output_key()` collapses them and reports N-1 phantom
    # "wasted_extra_events" per geometry. A None/empty block_ns (single-block runs)
    # yields a leading `|`, which is byte-for-byte the SAME grouping as before —
    # single-block duplicate detection is unchanged.
    ns = e.get("block_ns") or ""
    role = e["role"]
    if role == "leaf":
      return f"{ns}|leaf_{e['idx']}"
    if role in ("node", "tree-node"):
      lvl = e.get("level")
      return f"{ns}|tree_L{lvl}_N{e['idx']}"
    if role in ("reduction", "reduction-fold"):
      # Interval identity. When the exact endpoints are known (events-GCS source,
      # #347) key on (level, lo, hi) — the TRUE logical identity of a reduction
      # fold — so two distinct same-span intervals at one level (e.g. [0,1] and
      # [2,3], both span 2) are NOT collapsed into a false duplicate. When only
      # the span is known (coordinator-log source, which logs idx + span but not
      # the endpoints) keep the existing (idx, span) key — unchanged behavior.
      lo = e.get("lo")
      hi = e.get("hi")
      if lo is not None and hi is not None:
        lvl = e.get("level")
        return f"{ns}|reduction_L{lvl}_lo{lo}_hi{hi}"
      span = e.get("merge_interval_span")
      return f"{ns}|reduction_{e['idx']}_span{span}"
    return f"{ns}|{role}_{e['idx']}"

  key_counts = {}
  for e in [x for x in events if x["status"] == "success"]:
    k = output_key(e)
    key_counts[k] = key_counts.get(k, 0) + 1
  duplicate_keys = {k: c for k, c in key_counts.items() if c > 1}
  duplicates = {
      "duplicate_output_keys": len(duplicate_keys),
      "wasted_extra_events": sum(c - 1 for c in duplicate_keys.values()),
      "detail": duplicate_keys,
  }

  # ---- Recovery: redriven count + max stale_lease_redrive_count. ----
  redriven = sum(1 for e in events if e.get("redriven_after_lease_expiry") is True)

  return {
      "leaf_peak_rss": leaf_rss,
      "fold_peak_rss": fold_rss,
      "leaf_prove_time_distribution_ms": leaf_prove_dist,
      "fold_time_split_ms": fold_split,
      "prestate": prestate,
      "wave_width": wave_width,
      "queue_wait": queue_wait,
      "duplicate_proved": duplicates,
      "recovery": {
          "redriven_after_lease_expiry_count": redriven,
          # max_stale_lease_redrive_count filled in by the caller (it comes from
          # a separate log marker, not the event line).
      },
      # ---- #365 fold parallelism / wall-clock / ingestion telemetry. These
      # quantify the reduction strategy's CONCURRENCY + WALL-CLOCK advantage,
      # which the aggregate core-sec/block metrics above cannot show. Each block
      # is self-describing (provenance + measured flag) and emits null +
      # "UNMEASURED" when its source fields are absent -- never a fabricated
      # value. See compute_fold_critical_path / _parallelism / gating_ingestion.
      "fold_critical_path": compute_fold_critical_path(events),
      "fold_parallelism": compute_fold_parallelism(events),
      "gating_ingestion_rate": compute_gating_ingestion_rate(events),
  }


# ---------------------------------------------------------------------------
# THROUGHPUT metric (#321 C-sweep). The production objective is THROUGHPUT
# (target 10-12 blocks/sec). The controlling lever is TOTAL CPU per block
# (core-seconds/block): fewer core-sec/block => a smaller fleet at a given bps.
#
# Everything in this block is computed from the REAL parsed prove_ms values (or
# prove_time_ms fallback on old logs). The fleet-sizing figures are PROJECTIONS
# derived from the measured core_sec_per_block via a steady-state / utilization
# (Little's-Law-style) argument: cores = core_sec_per_block * arrival_rate. They
# are measured-DERIVED, not fabricated, and are explicitly labelled + carry their
# assumptions (perfect packing, steady state, no scheduling overhead).
# ---------------------------------------------------------------------------

# DOCUMENTATION-ONLY reference (NOT an authoritative divisor). The measurement
# layer must NEVER divide by a hardcoded machine-specific constant: vcpu_per_node
# is DERIVED from the REAL resolved machine type (run_config -> config -> null)
# via `vcpu_per_node_from_machine_type`. This value exists only to document that
# c3d-highcpu-60 happens to have 60 vCPU; it is deliberately unused as a fallback.
# See issue #352 (de-hardcode the measurement layer).
_DOC_C3D_HIGHCPU_60_VCPU = 60

# Default target block arrival rates (blocks/sec) for the fleet projection. The
# production window is 10-12 bps; override via --target-bps.
DEFAULT_TARGET_BPS = [10, 12]


def vcpu_per_node_from_machine_type(mtype):
  """Derive vCPU-per-node from a GCP machine-type string, or None if underivable.

  GCP machine types encode the vCPU count as the trailing integer, e.g.
  ``c4d-highcpu-64`` -> 64, ``c3d-highcpu-60`` -> 60, ``c3d-highcpu-30`` -> 30,
  ``t2d-standard-60`` -> 60.

  ANTI-FABRICATION (issue #352): returns ``None`` when the string is missing,
  ``"unknown"``, or unparseable. It NEVER defaults to 60 (or any other guess) —
  an unknown machine type must surface as null + a note, never a wrong constant.
  """
  if not mtype or not isinstance(mtype, str):
    return None
  m = mtype.strip().lower()
  if not m or m == "unknown":
    return None
  tail = m.rsplit("-", 1)[-1]
  if not tail.isdigit():
    return None
  vcpu = int(tail)
  return vcpu if vcpu > 0 else None


def compute_throughput(events, run_config=None, target_bps=None,
                       machine_type=None):
  """Compute the core-sec/block throughput metric + fleet-sizing PROJECTIONS.

  Args:
    events: parsed coordinator events (from parse_event_line).
    run_config: optional dict from run_config.json (blocks, txs_per_chunk=C,
      leaf_count_per_block, machine_type, ...). Authoritative for `blocks`, C/N,
      and the machine_type when present.
    target_bps: list of target block arrival rates for the fleet projection.
    machine_type: the REAL resolved machine type (run_config -> config-resolved
      aggregator -> None). vcpu_per_node is DERIVED from it; NEVER hardcoded. If
      run_config carries a non-null machine_type it takes precedence over the
      caller-supplied value (self-describing telemetry wins).

  Returns a dict suitable for embedding into bench_summary.json. Missing prove_ms
  (old logs with no split field) falls back to prove_time_ms; if NEITHER exists
  the run is marked measured=False / UNMEASURED — never fabricated. An unknown /
  underivable machine type surfaces as vcpu_per_node=null and nodes_required=null
  with an explanatory note — never a guessed divisor (anti-fabrication, #352).
  """
  if target_bps is None:
    target_bps = list(DEFAULT_TARGET_BPS)

  leaves = [e for e in events if e["role"] == "leaf" and e["status"] == "success"]
  folds = [
      e for e in events
      if e["role"] in ("node", "tree-node", "reduction", "reduction-fold")
      and e["status"] == "success"
  ]

  # prove_ms is the pure prove cost; fall back to prove_time_ms for OLD logs
  # (honest, provenance-noted) and never invent a value.
  def _prove(e):
    return e["prove_ms"] if e.get("prove_ms") is not None else e["prove_time_ms"]

  prove_ms_field_present = any(
      e.get("prove_ms") is not None for e in (leaves + folds)
  )
  prove_source = "prove_ms" if prove_ms_field_present else (
      "prove_time_ms (fallback: split prove_ms absent in log)"
  )

  leaf_prove_ms_sum = sum(_prove(e) for e in leaves)
  fold_prove_ms_sum = sum(_prove(e) for e in folds)

  leaf_cpu_core_sec = leaf_prove_ms_sum / 1000.0
  fold_cpu_core_sec = fold_prove_ms_sum / 1000.0
  total_cpu_core_sec = leaf_cpu_core_sec + fold_cpu_core_sec

  measured = (len(leaves) + len(folds)) > 0

  # ---- blocks CONFIG divisor: run_config is authoritative; else infer from
  # distinct block namespaces if the coordinator stamped them; else default 1. --
  blocks_config = None
  blocks_source = None
  if run_config and run_config.get("blocks"):
    blocks_config = int(run_config["blocks"])
    blocks_source = "run_config.json"
  if blocks_config is None:
    # Try to infer from distinct legacy block_number on events, if ever stamped.
    distinct_legacy = {
        e["block_number"] for e in events if e.get("block_number") is not None
    }
    if distinct_legacy:
      blocks_config = len(distinct_legacy)
      blocks_source = "inferred from distinct block_number on events"
  # #371 step-2b: run_config.json absent (or carried no `blocks`) AND no legacy
  # block_number stamped, but the coordinator DID stamp per-replay `block_ns`
  # (`block_N`) since #355. Events carry `block_ns`, NEVER `block_number`, so the
  # legacy inference above never fires for real multi-block runs — the old code
  # then defaulted to blocks=1, dividing an N-tree cost by 1 and inflating the
  # projected fleet ~N×. Infer the divisor from the DISTINCT REAL block
  # namespaces on events. "Real" EXCLUDES the `<base>` single-block sentinel and
  # any None/empty: a genuine single-block run (only `<base>`, or no block_ns at
  # all) has zero real namespaces and correctly falls through to the default 1
  # below — preserving byte-for-byte back-compat. Note: the coordinator-log
  # parser captures `<base>` verbatim (it is NOT normalized to None like the
  # events-GCS path), so we must filter it explicitly here.
  if blocks_config is None:
    distinct_real_block_ns = {
        e["block_ns"]
        for e in events
        if e.get("block_ns") not in (None, "", "<base>")
    }
    if distinct_real_block_ns:
      blocks_config = len(distinct_real_block_ns)
      blocks_source = (
          "inferred from distinct block_ns on events (no run_config.json)"
      )
  if blocks_config is None:
    blocks_config = 1
    blocks_source = "default 1 (no run_config.json, no block_number in log)"

  # ---- #355 divisor-vs-observed GUARD (anti-fabrication, same principle as #354).
  # Count the DISTINCT per-replay block namespaces ACTUALLY observed on events
  # (`block_ns=block_N` stamped by the coordinator since #355). If the run genuinely
  # proved N distinct blocks, we see N distinct `block_N/` namespaces; if the
  # multi-block replay silently COLLAPSED (the A2/A3 collision bug this fix closes:
  # identical keys dedup under the GCS CAS → one tree), we see FEWER. We must NEVER
  # divide a 1-tree cost by a phantom `blocks_config` and report a bogus
  # per-block cost + undersized fleet — so when the observed distinct blocks are
  # FEWER than the config divisor, we REFUSE to divide and emit null + a note.
  #
  # `<base>` is the un-namespaced single-block sentinel; a run that emitted only
  # `<base>` (or nothing) is a legitimate 1-block run and observes 1 block.
  observed_block_namespaces = {
      e["block_ns"] for e in events if e.get("block_ns") is not None
  }
  block_ns_field_present = len(observed_block_namespaces) > 0
  # #371: the count of DISTINCT REAL block namespaces (excludes the `<base>`
  # single-block sentinel and any None/empty). This is the same set the step-2b
  # inference above uses; recomputed here so the defensive provenance guard below
  # can assert the divisor never silently collapses a multi-block run to 1.
  distinct_real_block_ns_observed = {
      e["block_ns"] for e in events if e.get("block_ns") not in (None, "", "<base>")
  }
  if block_ns_field_present:
    distinct_blocks_observed = len(observed_block_namespaces)
  else:
    # Pre-#355 logs carry no block_ns field. We cannot OBSERVE the block count,
    # so we fall back to the config divisor WITHOUT a false disagreement (honest:
    # "not observable", not "collapsed"). This keeps old logs dividing exactly as
    # before while new logs get the real guard.
    distinct_blocks_observed = None

  # #357 anti-fabrication invariant: we divide the total cost by the config block
  # count ONLY when that count is CORROBORATED by the blocks actually OBSERVED on
  # events. Two failure modes both fabricate a plausible-but-wrong per-block cost:
  #   (A) COLLAPSE   — observed < config: the multi-block replay collapsed (the
  #                    #355/#357 dedup bug); dividing an M-tree cost by config N.
  #   (B) UNKNOWABLE — observed is None (no block_ns on events) while config > 1:
  #                    we CANNOT confirm N blocks were proved, so dividing by N is
  #                    an un-corroborated guess (this was the silent JSON-path
  #                    fabrication: None < N never fired the guard).
  # In BOTH cases we REFUSE to divide → core_sec_per_block=null + a note. We divide
  # only when observed >= config, OR when config == 1 (a single-block run's divide-
  # by-1 is a no-op that changes nothing and preserves byte-for-byte back-compat).
  divisor_guard_note = None
  if distinct_blocks_observed is not None and distinct_blocks_observed < blocks_config:
    # (A) COLLAPSE guard fires: refuse to divide by phantom blocks.
    core_sec_per_block = None
    blocks = distinct_blocks_observed
    divisor_guard_note = (
        f"REFUSING to divide by phantom blocks: observed {distinct_blocks_observed} "
        f"distinct block namespace(s) on events but run_config claimed "
        f"{blocks_config} — the multi-block replay likely COLLAPSED under the GCS "
        f"CAS (the #355/#357 collision this build fixes). core_sec_per_block is null "
        f"until the observed distinct blocks match the configured divisor."
    )
  elif distinct_blocks_observed is None and blocks_config > 1:
    # (B) UNKNOWABLE guard fires: cannot verify the block count, so refuse to
    # divide by an un-corroborated config divisor (the #357 anti-fabrication fix).
    core_sec_per_block = None
    blocks = blocks_config
    divisor_guard_note = (
        f"REFUSING to divide by an un-corroborated block count: "
        f"distinct_blocks_observed could not be determined from events "
        f"(no block_ns field present) but run_config claimed {blocks_config} "
        f"blocks. Refusing to divide by config blocks to avoid fabricating a "
        f"per-block cost; core_sec_per_block is null until the block count can be "
        f"observed from events."
    )
  elif blocks_config == 1 and len(distinct_real_block_ns_observed) > 1:
    # (C) #371 DEFENSIVE provenance invariant. After the step-2b inference above,
    # this state should be UNREACHABLE: a run with multiple distinct real
    # `block_ns` always infers blocks_config = that count, never the default 1.
    # This belt-and-suspenders branch guards against a FUTURE regression that
    # re-introduces the "default 1" path for a genuine multi-block run. Rather
    # than silently divide an N-tree cost by 1 (the exact fleet-inflating bug
    # #371 fixes), we REFUSE to divide → null + a loud note (anti-fabrication,
    # same principle as the (A)/(B) guards above).
    core_sec_per_block = None
    blocks = len(distinct_real_block_ns_observed)
    divisor_guard_note = (
        f"REFUSING to divide by a collapsed block count: blocks_config fell "
        f"through to the default 1 but {len(distinct_real_block_ns_observed)} "
        f"distinct real block_ns namespace(s) were observed on events "
        f"(#371 regression tripwire — inference should have set blocks_config to "
        f"{len(distinct_real_block_ns_observed)}). core_sec_per_block is null "
        f"until the divisor reflects the real block count."
    )
  else:
    # Trust the config divisor: observed corroborates it, or it is a single-block
    # (blocks_config == 1) run whose divide-by-1 is a behavior-preserving no-op.
    blocks = blocks_config
    core_sec_per_block = (
        (total_cpu_core_sec / blocks) if blocks > 0 else None
    )

  # The number of blocks actually used as the divisor for the "_all"/"_warm"
  # metrics. When the guard refused (note set), these per-block metrics are forced
  # null downstream regardless, so this only matters on the trusted path: use the
  # observed count when known, else the (corroborated or single-block) config.
  blocks_divisor_used = distinct_blocks_observed if (
      distinct_blocks_observed is not None
  ) else blocks_config

  # ---- Self-describing knobs. run_config authoritative, else echo from events. -
  def _first_present(key):
    for e in events:
      if e.get(key) is not None:
        return e[key]
    return None

  chunk_size = None
  leaf_count = None
  if run_config:
    chunk_size = run_config.get("txs_per_chunk")
    leaf_count = run_config.get("leaf_count_per_block")
  if chunk_size is None:
    chunk_size = _first_present("chunk_size")
  if leaf_count is None:
    leaf_count = _first_present("leaf_count")

  # ---- Cold vs warm FOLD CPU split (by is_first_task_on_pod). The cold portion
  # is the recoverable CPU that node-baking (#338: cold builds eliminated)
  # reclaims — measuring it across runs shows the baking win. ----
  first_task_known = any(e.get("is_first_task_on_pod") is not None for e in folds)
  cold_fold_ms = sum(
      _prove(e) for e in folds if e.get("is_first_task_on_pod") is True
  )
  warm_fold_ms = sum(
      _prove(e) for e in folds if e.get("is_first_task_on_pod") is False
  )
  cold_fold_cpu_core_sec = cold_fold_ms / 1000.0
  warm_fold_cpu_core_sec = warm_fold_ms / 1000.0

  # ---- #355 COLD-START vs WARM STEADY-STATE per-block split (linearity fix). ----
  # In a replay run, replay 0 is disproportionately COLD: its tasks are the
  # FIRST-TASK-ON-POD ones that paid the circuit build (~8s leaves) while later
  # replays run WARM (~4.3s leaves). Blindly averaging blends a one-time startup
  # transient with steady state, making per-block cost artificially FALL as BLOCKS
  # rises — it LOOKS like amortization but is not. So we compute BOTH:
  #   * core_sec_per_block_all  = total (incl. cold) / observed blocks
  #   * core_sec_per_block_warm = warm-only (excl. first-task-on-pod) / observed
  # and surface the cold overhead separately as cold_start_core_sec. WARM is the
  # steady-state number to size fleets from; ALL is the full-including-cold number.
  #
  # The cold/warm partition uses the SAME is_first_task_on_pod flag for BOTH leaves
  # and folds (a pod's first task, whatever its role, is the one that built the
  # circuit). Absent flag -> we cannot split -> warm metrics are null + a note
  # (anti-fabrication: never guess which tasks were cold).
  first_task_known_any = any(
      e.get("is_first_task_on_pod") is not None for e in (leaves + folds)
  )
  cold_leaf_ms = sum(
      _prove(e) for e in leaves if e.get("is_first_task_on_pod") is True
  )
  warm_leaf_ms = sum(
      _prove(e) for e in leaves if e.get("is_first_task_on_pod") is False
  )
  # Total cold-start overhead (leaf + fold first-task-on-pod prove cost). This is
  # the one-time transient a warm fleet does not repeatedly pay.
  cold_start_core_sec = (cold_leaf_ms + cold_fold_ms) / 1000.0
  # Warm steady-state CPU (everything NOT first-task-on-pod).
  warm_total_core_sec = (warm_leaf_ms + warm_fold_ms) / 1000.0

  # Per-block ALL (incl. cold) and WARM (excl. cold), divided by OBSERVED blocks
  # so the guard above governs (never divide by a phantom config count). Both are
  # null when unmeasured / unsplittable — never fabricated.
  _div = blocks_divisor_used if (blocks_divisor_used and blocks_divisor_used > 0) else None
  if divisor_guard_note is not None or _div is None:
    core_sec_per_block_all = None
  else:
    core_sec_per_block_all = total_cpu_core_sec / _div
  if not first_task_known_any or divisor_guard_note is not None or _div is None:
    core_sec_per_block_warm = None
    warm_note = (
        "warm per-block is null: is_first_task_on_pod flag absent (cannot isolate "
        "the cold transient) — anti-fabrication, never guessed"
        if not first_task_known_any
        else "warm per-block is null: divisor guard active (see divisor_guard_note)"
    )
  else:
    core_sec_per_block_warm = warm_total_core_sec / _div
    warm_note = (
        "WARM = steady-state per-block cost (excludes replay-0 first-task-on-pod "
        "cold build); size fleets from THIS. ALL includes the one-time cold start. "
        "NOTE: multi-block REPLAY proves the SAME block N times, so prestate is "
        "100% warm across replays BY DESIGN — this is a warm best-case FLOOR; "
        "production with DISTINCT blocks carries extra cold-prestate variance, so "
        "size with the #346 1.2-1.4x headroom."
    )

  # ---- Resolve the REAL machine type + DERIVE vcpu_per_node from it. ----
  # Precedence: run_config's self-describing machine_type (survives config drift)
  # -> caller-supplied (config-resolved aggregator) machine_type -> None. NEVER a
  # hardcoded machine-specific constant (anti-fabrication, #352).
  resolved_machine_type = None
  if run_config and run_config.get("machine_type"):
    resolved_machine_type = run_config.get("machine_type")
  elif machine_type:
    resolved_machine_type = machine_type
  # run_config may also carry a pre-derived vcpu_per_node (self-describing);
  # prefer it when present, else derive from the machine-type string.
  vcpu_per_node = None
  if run_config and run_config.get("vcpu_per_node"):
    vcpu_per_node = int(run_config["vcpu_per_node"])
  if vcpu_per_node is None:
    vcpu_per_node = vcpu_per_node_from_machine_type(resolved_machine_type)

  # ---- Fleet-sizing PROJECTIONS (measured-derived, clearly labelled). ----
  fleet_projection = []
  for bps in target_bps:
    if core_sec_per_block is None or not measured:
      fleet_projection.append({
          "target_bps": bps,
          "cores_required": None,
          "nodes_required": None,
          # DEPRECATED alias (drop after one release); kept so existing consumers
          # (csweep_report.py + external) don't read a missing key mid-migration.
          "c3d_nodes_required": None,
          "note": "UNMEASURED — no prove_ms/prove_time_ms in log",
      })
      continue
    cores_required = core_sec_per_block * bps
    if vcpu_per_node is None:
      # Machine type unknown/underivable: we can compute cores_required (real,
      # measured-derived) but CANNOT size the fleet without a guess. Emit null +
      # an honest note rather than divide by a fabricated constant (#352).
      fleet_projection.append({
          "target_bps": bps,
          "cores_required": cores_required,
          "nodes_required": None,
          "c3d_nodes_required": None,  # DEPRECATED alias.
          "note": (
              "vcpu_per_node underivable — machine_type "
              f"{resolved_machine_type!r}; cannot size fleet without guessing"
          ),
      })
      continue
    nodes_required = math.ceil(cores_required / vcpu_per_node)
    fleet_projection.append({
        "target_bps": bps,
        "cores_required": cores_required,
        "nodes_required": nodes_required,
        # DEPRECATED alias (drop after one release). Arch-neutral name is
        # `nodes_required`; this mirror keeps old consumers working one release.
        "c3d_nodes_required": nodes_required,
    })

  # Node-math + node-type strings reflect the REAL resolved machine type (or an
  # honest null + note when underivable) — never the literal "c3d-highcpu-60".
  if vcpu_per_node is not None:
    node_math = f"nodes_required = ceil(cores_required / {vcpu_per_node})"
  else:
    node_math = (
        "nodes_required = null (vcpu_per_node underivable; machine_type "
        f"{resolved_machine_type!r})"
    )

  return {
      "measured": measured,
      "prove_source_field": prove_source,
      "chunk_size_C": chunk_size,
      "leaf_count": leaf_count,
      "blocks": blocks,
      "blocks_source": blocks_source,
      # #355 provenance: config divisor vs distinct blocks ACTUALLY observed.
      "blocks_config": blocks_config,
      "distinct_blocks_observed": distinct_blocks_observed,
      "block_ns_field_present": block_ns_field_present,
      "divisor_guard_note": divisor_guard_note,
      "leaf_cpu_core_sec": leaf_cpu_core_sec,
      "fold_cpu_core_sec": fold_cpu_core_sec,
      "total_cpu_core_sec": total_cpu_core_sec,
      "core_sec_per_block": core_sec_per_block,
      # #355 cold-start vs warm steady-state per-block split (linearity fix).
      "core_sec_per_block_all": core_sec_per_block_all,
      "core_sec_per_block_warm": core_sec_per_block_warm,
      "cold_start_core_sec": cold_start_core_sec,
      "warm_total_core_sec": warm_total_core_sec,
      "warm_note": warm_note,
      "cold_fold_cpu_core_sec": cold_fold_cpu_core_sec,
      "warm_fold_cpu_core_sec": warm_fold_cpu_core_sec,
      "is_first_task_field_present": first_task_known,
      "fleet_sizing_projection": {
          "kind": "PROJECTION (measured-derived, not fabricated)",
          "basis": "cores_required = core_sec_per_block * target_bps",
          "node_math": node_math,
          "vcpu_per_node": vcpu_per_node,
          "node_type": resolved_machine_type,
          "vcpu_per_node_source": (
              "derived from machine_type (run_config -> config -> null); "
              "NOT a hardcoded constant (#352)"
          ),
          "assumptions": [
              "steady state (arrival rate == service rate)",
              "perfect bin-packing of prove work onto vCPUs",
              "no scheduler/queueing/GCS overhead in the core-sec accounting",
              "core_sec_per_block itself is REAL (summed measured prove_ms)",
              # #371: core_sec_per_block divides total by the REAL block count —
              # from run_config.json when present, else inferred from the distinct
              # `block_ns` namespaces on events (never a phantom default 1 that
              # would inflate the projected fleet ~N× for an N-block run).
              # #355: this projection uses core_sec_per_block (all, incl. cold). For
              # a STEADY-STATE fleet the warm number (core_sec_per_block_warm) is the
              # right basis — it excludes the one-time replay-0 cold-build transient.
              # Size steady-state fleets from core_sec_per_block_warm and add the
              # #346 1.2-1.4x headroom for distinct-block cold-prestate variance.
          ],
          "by_target_bps": fleet_projection,
      },
  }


def print_throughput(tp):
  print("\n================= THROUGHPUT (#321 C-sweep) ==================")
  if not tp["measured"]:
    print("  UNMEASURED — no leaf/fold prove_ms (or prove_time_ms) in log.")
    print("==============================================================\n")
    return
  print(
      f"C(chunk_size)={tp['chunk_size_C']} leaf_count={tp['leaf_count']} "
      f"blocks={tp['blocks']} ({tp['blocks_source']})"
  )
  # #355 divisor provenance: config vs observed distinct blocks.
  print(
      f"  blocks_config={tp['blocks_config']} "
      f"distinct_blocks_observed={tp['distinct_blocks_observed']} "
      f"(block_ns field present={tp['block_ns_field_present']})"
  )
  if tp.get("divisor_guard_note"):
    print(f"  [GUARD] {tp['divisor_guard_note']}")
  print(f"prove source: {tp['prove_source_field']}")
  print(
      f"  leaf_cpu={tp['leaf_cpu_core_sec']:.3f} core-sec  "
      f"fold_cpu={tp['fold_cpu_core_sec']:.3f} core-sec  "
      f"total_cpu={tp['total_cpu_core_sec']:.3f} core-sec"
  )
  # #355 the divisor guard may set core_sec_per_block to null (refuse to divide
  # by phantom blocks). Print honestly rather than crash on a None format.
  if tp["core_sec_per_block"] is None:
    print("  >>> core_sec_per_block = null (divisor guard active — see [GUARD]) <<<")
  else:
    print(f"  >>> core_sec_per_block = {tp['core_sec_per_block']:.3f} <<<")
  # #355 cold-start vs warm steady-state per-block split.
  _all = tp.get("core_sec_per_block_all")
  _warm = tp.get("core_sec_per_block_warm")
  print(
      "  core_sec_per_block_all="
      + ("null" if _all is None else f"{_all:.3f}")
      + "  core_sec_per_block_warm="
      + ("null" if _warm is None else f"{_warm:.3f}")
      + f"  cold_start_core_sec={tp['cold_start_core_sec']:.3f}"
  )
  print(f"  [warm/all] {tp['warm_note']}")
  print(
      f"  fold CPU cold(first-task-on-pod)={tp['cold_fold_cpu_core_sec']:.3f} "
      f"warm(cached)={tp['warm_fold_cpu_core_sec']:.3f} core-sec "
      f"(field_present={tp['is_first_task_field_present']})"
  )
  proj = tp["fleet_sizing_projection"]
  node_type = proj.get("node_type") or "unknown-machine-type"
  vcpu = proj.get("vcpu_per_node")
  print("\n  -- fleet-sizing PROJECTION (measured-derived; see assumptions) --")
  print(f"  node_type={node_type} vcpu_per_node={vcpu} ({proj.get('node_math')})")
  for row in proj["by_target_bps"]:
    if row.get("cores_required") is None:
      print(f"    @{row['target_bps']}bps: {row.get('note')}")
    elif row.get("nodes_required") is None:
      # cores are real/measured-derived but the fleet is unsizable (unknown mtype).
      print(
          f"    @{row['target_bps']}bps: cores={row['cores_required']:.1f} "
          f"=> nodes=null ({row.get('note')})"
      )
    else:
      print(
          f"    @{row['target_bps']}bps: cores={row['cores_required']:.1f} "
          f"=> {node_type} nodes={row['nodes_required']}"
      )
  print("==============================================================\n")


def load_run_config(path):
  """Read a run_config.json (blocks, txs_per_chunk=C, ...). None if absent."""
  if not path or not os.path.exists(path):
    return None
  try:
    with open(path, "r", encoding="utf-8") as f:
      return json.load(f)
  except Exception as e:  # noqa: BLE001
    print(f"[WARNING] Failed to read run_config {path}: {e}", file=sys.stderr)
    return None


# ---------------------------------------------------------------------------
# EVENTS-GCS mode (#347): durable per-pod telemetry read directly from the GCS
# `events/` prefix — decoupled from coordinator stdout, later pipeline steps, and
# cluster lifetime. Each prover pod writes its ProverEvent JSON to
# `<run-prefix>/events/<unique-key>.json` at completion (a sibling of the
# stark_proofs/ dir), so telemetry survives ANY downstream failure. This is the
# ADDITIONAL, PREFERRED source; the coordinator-log/local-file modes stay intact.
# ---------------------------------------------------------------------------

# Map the Rust `Role` kebab-case serde tag -> the extractor's canonical role
# string used by compute_derived/compute_throughput. (The extractor already
# treats "tree-node" and "reduction-fold" as folds, so these pass straight
# through; the map is explicit so an unexpected tag is visible rather than silent.)
_GCS_ROLE_MAP = {
    "leaf": "leaf",
    "tree-node": "tree-node",
    "reduction-fold": "reduction-fold",
    "root-coordinator": "root-coordinator",
}


def prover_event_json_to_event(obj):
  """Map ONE deserialized ProverEvent JSON dict (as written to GCS by a prover
  pod) into the SAME event-dict shape `parse_event_line` produces, so the exact
  same compute_derived / compute_throughput / stats path builds bench_summary.

  The ProverEvent nests the geometry under `descriptor` (role, level, chunk_idx,
  node_idx, lo, hi, tx_per_proof) and carries the phase timers + sizing fields at
  the top level. Returns None if the object is not a well-formed event.
  """
  if not isinstance(obj, dict):
    return None
  desc = obj.get("descriptor")
  if not isinstance(desc, dict):
    return None

  raw_role = desc.get("role")
  role = _GCS_ROLE_MAP.get(raw_role, raw_role)
  if role is None:
    return None

  # Role-appropriate index: leaves -> chunk_idx; folds/nodes -> node_idx. This
  # mirrors the coordinator log's `idx=` field so the extractor's output_key()
  # dedup groups the same logical keys identically across both sources.
  if role == "leaf":
    idx = desc.get("chunk_idx", 0)
  else:
    idx = desc.get("node_idx", 0)

  # Reduction folds are interval-addressed; the extractor's output_key() dedup
  # for reductions keys on (idx, merge_interval_span). The pod emits lo/hi on the
  # descriptor and merge_interval_span at the top level (0 for non-reductions);
  # prefer the explicit field, else derive the span from the interval so
  # concurrent redrives of the same interval dedupe to ONE logical key.
  merge_span = obj.get("merge_interval_span")
  if (not merge_span) and role in ("reduction", "reduction-fold"):
    lo = desc.get("lo", 0)
    hi = desc.get("hi", 0)
    merge_span = (hi - lo + 1) if hi >= lo else 0

  fold_strategy = desc.get("fold_strategy")
  if isinstance(fold_strategy, str):
    fold_strategy = fold_strategy.lower()

  # #355/#357: the per-replay block namespace (`block_N`) the coordinator stamps
  # on the descriptor. This is the field that makes N distinct replays DISTINCT;
  # without it every replay's leaf_0 (role=leaf,L0,N0,lo0,hi0) shares an identical
  # logical key and dedupe_events_by_logical_key() collapses all N blocks into one
  # (silently discarding N-1 blocks) AND the divisor guard cannot observe the real
  # block count. Normalize empty/`<base>` to None so single-block runs (no real
  # namespace) key + behave EXACTLY as before (`_opt_str` None convention, matching
  # the coordinator-log parser). The raw JSON always carries it under `descriptor`.
  block_ns = desc.get("block_ns")
  if isinstance(block_ns, str):
    block_ns = block_ns.strip()
    if not block_ns or block_ns == "<base>":
      block_ns = None
  elif block_ns is not None:
    block_ns = None

  return {
      # No wall-clock timestamp travels in the ProverEvent payload (the pull_ts_ms
      # field is a dispatch timestamp, not an emit timestamp); None is honest.
      "ts": None,
      "role": role,
      "idx": int(idx),
      "status": obj.get("status", "success"),
      "prove_time_ms": int(obj.get("prove_time_ms", 0)),
      "gcs_time_ms": int(obj.get("gcs_time_ms", 0)),
      "total_time_ms": int(obj.get("total_time_ms", 0)),
      # ---- per-task sizing fields (present in every #347 event; None-tolerant) --
      "fold_strategy": fold_strategy,
      "level": desc.get("level"),
      "peak_rss_bytes": obj.get("peak_rss_bytes"),
      "prestate_source": obj.get("prestate_source"),
      "is_first_task_on_pod": obj.get("is_first_task_on_pod"),
      "chunk_size": obj.get("chunk_size"),
      "leaf_count": obj.get("leaf_count"),
      "pull_ms": obj.get("pull_ms"),
      "pre_exec_ms": obj.get("pre_exec_ms"),
      "prove_ms": obj.get("prove_ms"),
      "gcs_write_ms": obj.get("gcs_write_ms"),
      "queue_wait_ms": obj.get("queue_wait_ms"),
      "fold_kind": obj.get("fold_kind"),
      "merge_interval_span": merge_span,
      "redriven_after_lease_expiry": obj.get("redriven_after_lease_expiry"),
      # #347: the reduction interval endpoints. Present ONLY on events-GCS
      # events (the ProverEvent descriptor carries lo/hi); coordinator-log
      # events lack them. output_key() uses them, WHEN present, to disambiguate
      # distinct same-span intervals at the same level (two folds covering
      # [0,1] and [2,3] both have span 2 but are DIFFERENT logical tasks).
      "lo": desc.get("lo"),
      "hi": desc.get("hi"),
      # #355/#357: per-replay block namespace, mirroring the coordinator-log
      # parser's `block_ns` field (None when absent/`<base>`). Feeds the divisor
      # guard's distinct-blocks-observed accounting AND the logical key below.
      "block_ns": block_ns,
      # #347 dedup helper: the LOGICAL key (role+level+idx+interval) shared by all
      # attempts (redrives) of the same task. Not emitted into bench_summary; used
      # only to dedupe + count redrives below.
      #
      # #357: block_ns is PREPENDED so identical-geometry tasks in DIFFERENT
      # replays (each replay's leaf_0 is role=leaf,L0,N0,lo0,hi0) are NOT collapsed
      # into one by dedupe_events_by_logical_key(). A None/empty block_ns (a genuine
      # single-block run) yields the leading `|` prefix, which is byte-for-byte the
      # SAME grouping as before for single-block runs (all events share the empty
      # namespace, so their relative keys are unchanged).
      "_logical_key": (
          f"{block_ns or ''}|{role}|L{desc.get('level', 0)}|N{idx}"
          f"|lo{desc.get('lo', 0)}|hi{desc.get('hi', 0)}"
      ),
  }


def _list_gcs_events(gcs_prefix):
  """List every `events/*.json` object under `gcs_prefix` (a gs:// URI) via
  `gcloud storage ls`. Returns a list of full gs:// object URIs (JSON only)."""
  prefix = gcs_prefix.rstrip("/") + "/"
  cmd = ["gcloud", "storage", "ls", f"{prefix}**"]
  res = subprocess.run(cmd, capture_output=True, text=True)
  if res.returncode != 0:
    # Fall back to a non-recursive listing (older gsutil-style globbing).
    cmd = ["gcloud", "storage", "ls", prefix]
    res = subprocess.run(cmd, capture_output=True, text=True)
    if res.returncode != 0:
      print(f"[WARNING] Failed to list {prefix}: {res.stderr}", file=sys.stderr)
      return []
  uris = []
  for line in res.stdout.splitlines():
    line = line.strip()
    if line.endswith(".json") and line.startswith("gs://"):
      uris.append(line)
  return uris


def _download_gcs_object(uri):
  """Download ONE gs:// object's bytes via `gcloud storage cat`. None on error."""
  cmd = ["gcloud", "storage", "cat", uri]
  res = subprocess.run(cmd, capture_output=True, text=True)
  if res.returncode != 0:
    print(f"[WARNING] Failed to read {uri}: {res.stderr}", file=sys.stderr)
    return None
  return res.stdout


def dedupe_events_by_logical_key(events):
  """Dedupe events by their logical key (role+level+idx+interval), keeping the
  LAST successful attempt (or the last attempt if none succeeded) while COUNTING
  redrives — the extra attempts beyond the first for any key.

  Returns `(deduped_events, redrive_extra_count)`. Every event carries its own
  object (uuid-suffixed) so concurrent pods + redrives never clobbered each other
  in GCS; here we collapse them to ONE per logical task for the sizing math, and
  surface how many EXTRA attempts existed (the redrive/dupe count) so that signal
  is preserved rather than silently discarded.
  """
  by_key = {}
  attempts = {}
  for e in events:
    k = e.get("_logical_key", f"{e['role']}_{e['idx']}")
    attempts[k] = attempts.get(k, 0) + 1
    prev = by_key.get(k)
    # Keep a success over a non-success; otherwise keep the latest seen.
    if prev is None:
      by_key[k] = e
    elif prev.get("status") != "success" and e.get("status") == "success":
      by_key[k] = e
    elif e.get("status") == "success":
      by_key[k] = e  # latest successful attempt
  redrive_extra = sum(c - 1 for c in attempts.values() if c > 1)
  return list(by_key.values()), redrive_extra


def build_metrics_from_events(events, run_config=None, target_bps=None,
                              redrive_extra=0, machine_type=None):
  """Build the SAME metrics dict `parse_coordinator_log_v2` returns, but from a
  pre-parsed `events` list (the #347 GCS-events source) rather than a log file.

  Reuses compute_derived / compute_throughput / stats verbatim so the produced
  bench_summary.json (core_sec_per_block, leaf/fold split, fleet_sizing_projection,
  peak RSS, distributions, ...) is identical in shape + math to the log path.

  `machine_type` (the REAL resolved machine type) is threaded into
  compute_throughput so vcpu_per_node is DERIVED, not hardcoded (#352).
  """
  leaf_provings, leaf_gcs, leaf_totals = [], [], []
  node_foldings, node_gcs, node_totals = [], [], []
  saw_new_fields = False

  for ev in events:
    if ev.get("fold_strategy") is not None or ev.get("prove_ms") is not None:
      saw_new_fields = True
    if ev["status"] == "success":
      role = ev["role"]
      if role == "leaf":
        leaf_provings.append(float(ev["prove_time_ms"]))
        leaf_gcs.append(float(ev["gcs_time_ms"]))
        leaf_totals.append(float(ev["total_time_ms"]))
      elif role in ("node", "tree-node", "reduction", "reduction-fold"):
        node_foldings.append(float(ev["prove_time_ms"]))
        node_gcs.append(float(ev["gcs_time_ms"]))
        node_totals.append(float(ev["total_time_ms"]))

  derived = compute_derived(events)
  # The events payload does not carry the coordinator's stale-lease-redrive
  # marker, but the #347 dedup DID observe the redrive/dupe extras directly from
  # the number of GCS objects per logical key — a stronger, source-of-truth count.
  derived["recovery"]["max_stale_lease_redrive_count"] = 0
  derived["recovery"]["redrive_extra_attempts_from_gcs_events"] = redrive_extra

  throughput = compute_throughput(events, run_config=run_config,
                                  target_bps=target_bps,
                                  machine_type=machine_type)

  def _first_present(key):
    for e in events:
      if e.get(key) is not None:
        return e[key]
    return None

  descriptors = {
      "fold_strategy": _first_present("fold_strategy"),
      "chunk_size_C": _first_present("chunk_size"),
      "leaf_count_N": _first_present("leaf_count"),
      # No explicit "root reached" marker travels in the per-pod events; a
      # reduction run is inferred from the presence of reduction-fold events.
      "reduction_root_reached": any(
          e["role"] in ("reduction", "reduction-fold") for e in events
      ),
      "hex_root_reached": any(e["role"] in ("node", "tree-node") for e in events),
      "scheduling_class": None,
  }

  return {
      "events_parsed": len(events),
      "back_compat_old_log": not saw_new_fields and len(events) > 0,
      "leaf_proving": stats(leaf_provings),
      "leaf_gcs": stats(leaf_gcs),
      "leaf_total": stats(leaf_totals),
      "node_folding": stats(node_foldings),
      "node_gcs": stats(node_gcs),
      "node_total": stats(node_totals),
      # Verification time + total_tx live in the coordinator's ROOT_PROOF_VERIFIED
      # line, not the per-pod events; honest 0 here (the events source is about
      # per-task SIZING, which is what feeds core_sec/block + fleet sizing).
      "verification_time_ms": 0.0,
      "total_tx": 0,
      # Wall time needs the seeder-start + root-reached timestamps (coordinator
      # markers); the per-pod events don't carry emit timestamps, so 0 (honest).
      "wall_sec": 0.0,
      "start_time": None,
      "end_time": None,
      "derived": derived,
      "throughput": throughput,
      "descriptors": descriptors,
  }


def parse_events_gcs(gcs_prefix, run_config=None, target_bps=None,
                     machine_type=None):
  """Read every ProverEvent JSON under the GCS `events/` prefix, dedupe by logical
  key (counting redrives), and build the metrics dict. None if nothing readable."""
  print(f"[INFO] EVENTS-GCS mode: listing {gcs_prefix} ...")
  uris = _list_gcs_events(gcs_prefix)
  if not uris:
    print(f"[WARNING] No events/*.json objects found under {gcs_prefix}",
          file=sys.stderr)
    return None
  print(f"[INFO] Found {len(uris)} event object(s); downloading + parsing ...")

  raw_events = []
  for uri in uris:
    body = _download_gcs_object(uri)
    if body is None:
      continue
    try:
      obj = json.loads(body)
    except Exception as e:  # noqa: BLE001
      print(f"[WARNING] {uri}: not valid JSON ({e}); skipping", file=sys.stderr)
      continue
    ev = prover_event_json_to_event(obj)
    if ev is None:
      print(f"[WARNING] {uri}: not a well-formed ProverEvent; skipping",
            file=sys.stderr)
      continue
    raw_events.append(ev)

  if not raw_events:
    print("[ERROR] No parseable ProverEvent objects in GCS events prefix.",
          file=sys.stderr)
    return None

  events, redrive_extra = dedupe_events_by_logical_key(raw_events)
  print(
      f"[INFO] Parsed {len(raw_events)} raw event(s) -> {len(events)} unique "
      f"logical task(s); {redrive_extra} extra redrive/dupe attempt(s)."
  )
  return build_metrics_from_events(
      events, run_config=run_config, target_bps=target_bps,
      redrive_extra=redrive_extra, machine_type=machine_type,
  )


def parse_coordinator_log_v2(log_path, seeder_start_dt=None, run_config=None,
                             target_bps=None, machine_type=None):
  if not os.path.exists(log_path):
    print(f"[ERROR] Coordinator log {log_path} not found.", file=sys.stderr)
    return None

  events = []
  leaf_provings = []
  leaf_gcs = []
  leaf_totals = []
  node_foldings = []
  node_gcs = []
  node_totals = []

  root_reached_time = None
  first_event_time = None
  max_stale_redrive = 0
  reduction_root_reached = False
  hex_root_reached = False

  verification_time_ms = 0.0
  total_tx = 0

  # For back-compat detection: were ANY #328 fields ever seen?
  saw_new_fields = False

  with open(log_path, "r", encoding="utf-8") as f:
    for line in f:
      # ROOT_PROOF_VERIFIED JSON telemetry line (verification time + batch size).
      if '"telemetry_event":"ROOT_PROOF_VERIFIED"' in line:
        try:
          json_start = line.find("{")
          if json_start != -1:
            data = json.loads(line[json_start:])
            verification_time_ms = float(data.get("verification_time_ms", 0.0))
            total_tx = int(data.get("aggregated_batch_size", 0))
        except Exception as e:
          print(f"[WARNING] Failed to parse coordinator telemetry line: {e}", file=sys.stderr)

      # stale_lease_redrive_count marker (re-driven tasks).
      m_stale = _STALE_REDRIVE_RE.search(line)
      if m_stale:
        max_stale_redrive = max(max_stale_redrive, int(m_stale.group("v")))

      # Completion markers.
      if _ROOT_REDUCTION_RE.search(line):
        reduction_root_reached = True
      m_hex = _ROOT_HEX_RE.match(line)
      if m_hex:
        hex_root_reached = True
        try:
          root_reached_time = parse_k8s_timestamp(m_hex.group(1))
        except Exception as e:
          print(f"[WARNING] Failed to parse root reached timestamp: {e}", file=sys.stderr)

      # Per-task event line.
      ev = parse_event_line(line)
      if ev is None:
        continue

      if ev.get("fold_strategy") is not None or ev.get("prove_ms") is not None:
        saw_new_fields = True

      try:
        ts = parse_k8s_timestamp(ev["ts"])
        if first_event_time is None:
          first_event_time = ts
        # If reduction root was reached (no explicit hex ROOT REACHED! ts), track
        # the last successful event time as an end-of-run proxy.
        if reduction_root_reached and root_reached_time is None:
          root_reached_time = ts
      except Exception as e:
        print(f"[WARNING] Failed to parse event timestamp: {e}", file=sys.stderr)

      events.append(ev)

      if ev["status"] == "success":
        role = ev["role"]
        if role == "leaf":
          leaf_provings.append(float(ev["prove_time_ms"]))
          leaf_gcs.append(float(ev["gcs_time_ms"]))
          leaf_totals.append(float(ev["total_time_ms"]))
        elif role in ("node", "tree-node", "reduction", "reduction-fold"):
          node_foldings.append(float(ev["prove_time_ms"]))
          node_gcs.append(float(ev["gcs_time_ms"]))
          node_totals.append(float(ev["total_time_ms"]))

  start_dt = seeder_start_dt
  if not start_dt and first_event_time:
    start_dt = first_event_time

  wall_sec = 0.0
  if start_dt and root_reached_time:
    wall_sec = (root_reached_time - start_dt).total_seconds()

  # Derived sizing metrics (#328 §C).
  derived = compute_derived(events)
  derived["recovery"]["max_stale_lease_redrive_count"] = max_stale_redrive

  # THROUGHPUT metric (#321 C-sweep). Additive; consumes run_config if given.
  # machine_type is threaded so vcpu_per_node is DERIVED, not hardcoded (#352).
  throughput = compute_throughput(events, run_config=run_config,
                                  target_bps=target_bps,
                                  machine_type=machine_type)

  # Self-describing run descriptors (echoed from the events; None if not logged).
  def _first_present(key):
    for e in events:
      if e.get(key) is not None:
        return e[key]
    return None

  descriptors = {
      "fold_strategy": _first_present("fold_strategy"),
      "chunk_size_C": _first_present("chunk_size"),
      "leaf_count_N": _first_present("leaf_count"),
      "reduction_root_reached": reduction_root_reached,
      "hex_root_reached": hex_root_reached,
      # #349: scheduling_class is now emitted on the coordinator line; echo the
      # first present value so the run self-describes its seed order. Stays None
      # only when genuinely absent (pre-#349 logs) — never fabricated.
      "scheduling_class": _first_present("scheduling_class"),
  }

  return {
      "events_parsed": len(events),
      "back_compat_old_log": not saw_new_fields and len(events) > 0,
      "leaf_proving": stats(leaf_provings),
      "leaf_gcs": stats(leaf_gcs),
      "leaf_total": stats(leaf_totals),
      "node_folding": stats(node_foldings),
      "node_gcs": stats(node_gcs),
      "node_total": stats(node_totals),
      "verification_time_ms": verification_time_ms,
      "total_tx": total_tx,
      "wall_sec": wall_sec,
      "start_time": start_dt,
      "end_time": root_reached_time,
      "derived": derived,
      "throughput": throughput,
      "descriptors": descriptors,
  }


# ---------------------------------------------------------------------------
# SIZING DERIVATION block (#328 §C). Human-readable + embedded in JSON.
# ---------------------------------------------------------------------------
def build_sizing_derivation(metrics):
  d = metrics["derived"]
  desc = metrics["descriptors"]

  def _mem_rec(rss_block):
    if rss_block["peak_rss_measured"]:
      rec = int(rss_block["peak_rss_bytes_max"] * MEMORY_SAFETY_MARGIN)
      return {
          "peak_rss_bytes_max": rss_block["peak_rss_bytes_max"],
          "safety_margin": MEMORY_SAFETY_MARGIN,
          "recommended_memory_requests_bytes": rec,
          "measured": True,
      }
    return {
        "peak_rss_bytes_max": 0,
        "safety_margin": MEMORY_SAFETY_MARGIN,
        "recommended_memory_requests_bytes": "UNMEASURED — run with cgroup/RSS access",
        "measured": False,
    }

  sizing = {
      "run": {
          "fold_strategy": desc["fold_strategy"],
          "chunk_size_C": desc["chunk_size_C"],
          "leaf_count_N": desc["leaf_count_N"],
          "scheduling_class": desc["scheduling_class"],
      },
      "memory_requests": {
          "leaf": _mem_rec(d["leaf_peak_rss"]),
          "fold": _mem_rec(d["fold_peak_rss"]),
      },
      "cpu_and_pods_per_node": (
          "requires node metrics (#328 §B, GCP) — Prometheus/node-exporter not "
          "available here; NOT fabricated."
      ),
      "leaf_prove_time_distribution_ms": d["leaf_prove_time_distribution_ms"],
      "prestate_hit_rate": d["prestate"],
      "queue_wait": d["queue_wait"],
      "wave_width": d["wave_width"],
      "duplicate_proved": d["duplicate_proved"],
      "recovery": d["recovery"],
  }
  return sizing


def print_sizing_derivation(sizing):
  print("\n================= SIZING DERIVATION (#328 §C) =================")
  run = sizing["run"]
  print(
      f"Run: fold_strategy={run['fold_strategy']} "
      f"C(chunk_size)={run['chunk_size_C']} N(leaf_count)={run['leaf_count_N']} "
      f"scheduling_class={run['scheduling_class']}"
  )

  print("\n-- memory_requests (peak_rss_bytes_max x safety margin) --")
  for role in ("leaf", "fold"):
    mr = sizing["memory_requests"][role]
    if mr["measured"]:
      print(
          f"  {role:5s}: peak_rss_max={mr['peak_rss_bytes_max']} bytes"
          f" x {mr['safety_margin']} => {mr['recommended_memory_requests_bytes']} bytes"
      )
    else:
      print(f"  {role:5s}: {mr['recommended_memory_requests_bytes']}")

  print(f"\n-- cpu / pods-per-node --\n  {sizing['cpu_and_pods_per_node']}")

  dist = sizing["leaf_prove_time_distribution_ms"]
  print("\n-- leaf prove-time distribution (ms) --")
  if dist["measured"]:
    print(
        f"  n={dist['count']} p50={dist['p50']} p95={dist['p95']} "
        f"p99={dist['p99']} max={dist['max']} mean={dist['mean']:.2f} "
        f"CV={dist['cv']:.4f}"
    )
  else:
    print("  not measured (no leaf events)")

  ps = sizing["prestate_hit_rate"]
  print("\n-- prestate hit rate --")
  if ps["prestate_field_present"]:
    print(
        f"  corpus={ps['corpus_count']} replay-fallback={ps['replay_fallback_count']} "
        f"other={ps['other_count']} hit_rate={ps['corpus_hit_rate']}"
    )
    if ps["REGRESSION_replay_fallback_present"]:
      print("  *** REGRESSION FLAG: replay-fallback occurred (corpus miss) ***")
  else:
    print("  prestate_source not present in log (older coordinator)")

  qw = sizing["queue_wait"]
  print("\n-- queue_wait (ms) --")
  if qw["measured"]:
    print(f"  mean={qw['mean_ms']:.2f} max={qw['max_ms']}")
  else:
    print(f"  {qw['note']}")

  print(f"\n-- wave width --\n  {sizing['wave_width']['note']}")

  dup = sizing["duplicate_proved"]
  print("\n-- duplicate-proved (wasted compute) --")
  print(
      f"  duplicate_output_keys={dup['duplicate_output_keys']} "
      f"wasted_extra_events={dup['wasted_extra_events']}"
  )

  rec = sizing["recovery"]
  print("\n-- recovery --")
  print(
      f"  redriven_after_lease_expiry_count="
      f"{rec['redriven_after_lease_expiry_count']} "
      f"max_stale_lease_redrive_count={rec['max_stale_lease_redrive_count']}"
  )
  print("==============================================================\n")


def build_summary(metrics, metadata, provenance, sizing):
  leaf_total_sec = metrics["leaf_proving"]["total"] / 1000.0
  node_total_sec = metrics["node_folding"]["total"] / 1000.0

  return {
      "cryptographic_phase_telemetry": {
          "total_stark_prove_sec": leaf_total_sec + node_total_sec,
          "leaf_prove_sec": leaf_total_sec,
          "tree_aggregate_sec": node_total_sec,
          "total_pipelined_scope_wall_sec": metrics["wall_sec"],
          "leaf_proving_stats_ms": metrics["leaf_proving"],
          "leaf_gcs_stats_ms": metrics["leaf_gcs"],
          "leaf_total_stats_ms": metrics["leaf_total"],
          "node_folding_stats_ms": metrics["node_folding"],
          "node_gcs_stats_ms": metrics["node_gcs"],
          "node_total_stats_ms": metrics["node_total"],
      },
      "coordinator_telemetry": {
          "verification_time_ms": metrics["verification_time_ms"],
          "total_coordinator_sec": metrics["wall_sec"],
      },
      "system_telemetry": {
          "effective_tps": metrics["total_tx"] / metrics["wall_sec"] if metrics["wall_sec"] > 0 else 0.0,
      },
      "total_transactions": metrics["total_tx"],
      "descriptors": metrics["descriptors"],
      "derived_sizing_metrics": metrics["derived"],
      "throughput": metrics["throughput"],
      "sizing_derivation": sizing,
      "provenance": provenance,
      "metadata": metadata,
  }


def main():
  parser = argparse.ArgumentParser(description="Extract fungible-pool coordinator telemetry")
  parser.add_argument("--arch", default="local", help="Silicon architecture (t2d, c4d, etc.)")
  parser.add_argument("--coordinator-log", default="coordinator.log", help="Path to coordinator log (GKE path)")
  parser.add_argument("--log-file", default=None, help="Local coordinator log file (no cluster; skips kubectl/GCS)")
  parser.add_argument("--config", default="config.toml", help="Path to config.toml")
  parser.add_argument("--benchmark-id", default="local", help="Benchmark ID")
  parser.add_argument("--image", default="local", help="Image tag (code release)")
  parser.add_argument("--out", default=None, help="Output path for bench_summary.json (default: CWD/bench_summary.json in local mode)")
  parser.add_argument("--no-upload", action="store_true", help="Never upload to GCS (implied by --log-file)")
  parser.add_argument("--run-config", default=None, help="Path to run_config.json (blocks, txs_per_chunk=C, leaf_count_per_block) for the throughput metric")
  parser.add_argument("--target-bps", default=None, help="Comma-separated target blocks/sec for the fleet-sizing projection (default: 10,12)")
  parser.add_argument(
      "--events-gcs-prefix", default=None,
      help=(
          "(#347) DURABLE, PREFERRED source: gs://<bucket>/benchmark-reports/"
          "<id>/<image>/<arch>/events/ prefix of per-pod ProverEvent JSON. When "
          "given (or auto-derived in GKE mode from --benchmark-id/--image/--arch "
          "+ config bucket), telemetry is read directly from GCS — decoupled from "
          "coordinator stdout, later pipeline steps, and cluster lifetime. Falls "
          "back to --coordinator-log when the events prefix is empty/unreadable."
      ),
  )
  parser.add_argument(
      "--no-events-gcs", action="store_true",
      help="Disable the events-GCS source even in GKE mode (use coordinator-log only).",
  )
  args = parser.parse_args()

  target_bps = None
  if args.target_bps:
    target_bps = [int(x) for x in args.target_bps.split(",") if x.strip()]
  run_config = load_run_config(args.run_config)

  local_mode = args.log_file is not None
  log_path = args.log_file if local_mode else args.coordinator_log
  git_commit = get_git_commit()
  gen_ts = datetime.datetime.now(datetime.timezone.utc).isoformat()

  seeder_start = None
  gcs_uri = None
  agg_machine = "unknown"
  leaf_machine = None  # REAL leaf machine type resolved from config (#352).
  # (#347) The events-GCS prefix + which source ultimately produced the metrics.
  events_gcs_prefix = args.events_gcs_prefix
  source_kind = "local-log-file" if local_mode else "kubectl-gke"
  source_ref = os.path.abspath(log_path)

  if local_mode:
    print(f"[INFO] LOCAL mode: parsing {log_path} (no kubectl/GCS).")
  else:
    # GKE path: resolve GCS bucket + aggregator machine type from config.toml.
    gcs_bucket = "kunal-scratch-tfstate"
    if os.path.exists(args.config):
      try:
        with open(args.config, "rb") as f:
          cfg_data = tomllib.load(f)
        gcs_bucket = cfg_data.get("gcp", {}).get("bench", {}).get("bucket", "kunal-scratch-tfstate")
        _pod = cfg_data.get("proving_pod", {}).get(args.arch, {})
        agg_machine = _pod.get("aggregator", {}).get("machine_type", "unknown")
        # Resolve the REAL leaf machine type too (mirrors agg_machine). None when
        # unresolvable — the metadata block then honestly records the arch + a note
        # rather than mislabeling the arch as a machine type (#352).
        leaf_machine = _pod.get("leaf_worker", {}).get("machine_type")
      except Exception as e:
        print(f"[WARNING] Failed to parse config.toml: {e}", file=sys.stderr)
    gcs_prefix = f"benchmark-reports/{args.benchmark_id}/{args.image}/{args.arch}"
    gcs_uri = f"gs://{gcs_bucket}/{gcs_prefix}"
    print(f"[INFO] Target GCS URI: {gcs_uri}")
    # (#347) Auto-derive the events prefix from the same run coordinates when the
    # caller did not pass --events-gcs-prefix. events/ is a SIBLING of stark_proofs/.
    if events_gcs_prefix is None and not args.no_events_gcs:
      events_gcs_prefix = f"{gcs_uri}/events/"
      print(f"[INFO] Auto-derived events-GCS prefix: {events_gcs_prefix}")

  # ---- Resolve the REAL machine type to thread into the fleet-sizing math so
  #      vcpu_per_node is DERIVED, never hardcoded (#352). Precedence:
  #      run_config.machine_type (self-describing telemetry, survives config
  #      drift) -> config-resolved aggregator machine_type -> None. compute_
  #      throughput itself re-applies the run_config precedence, so passing the
  #      config-resolved agg_machine here is the correct fallback. ------------
  effective_machine_type = None
  if run_config and run_config.get("machine_type"):
    effective_machine_type = run_config.get("machine_type")
  elif agg_machine and agg_machine != "unknown":
    effective_machine_type = agg_machine
  if effective_machine_type:
    print(f"[INFO] Fleet-sizing machine_type: {effective_machine_type} "
          f"(vcpu_per_node derived, not hardcoded)")
  else:
    print("[INFO] Fleet-sizing machine_type: UNKNOWN — nodes_required will be "
          "null + note (no guessed divisor).")

  # ---- (#347) Prefer the DURABLE events-GCS source when available. It survives
  #      coordinator/pipeline/cluster failure, so it is the source of truth; the
  #      coordinator log is the fallback. --------------------------------------
  metrics = None
  if events_gcs_prefix and not args.no_events_gcs:
    print(f"[INFO] Preferring durable events-GCS source: {events_gcs_prefix}")
    metrics = parse_events_gcs(
        events_gcs_prefix, run_config=run_config, target_bps=target_bps,
        machine_type=effective_machine_type,
    )
    if metrics:
      source_kind = "events-gcs (#347 durable per-pod)"
      source_ref = events_gcs_prefix
      print(f"[INFO] Built metrics from {metrics['events_parsed']} durable GCS event(s).")
    else:
      print("[WARNING] events-GCS source empty/unreadable; falling back to coordinator log.",
            file=sys.stderr)

  if metrics is None:
    # Seeder start time is only needed for the coordinator-log wall-time; skip
    # the kubectl call entirely when the events-GCS source already succeeded.
    if not local_mode:
      print("[INFO] Querying GKE Seeder Job for start time...")
      seeder_start = get_job_start_time("lighter-seeder")
      if seeder_start:
        print(f"[INFO] Seeder Start Time: {seeder_start.isoformat()}")
      else:
        print("[WARNING] Could not retrieve seeder job start time. Will fallback to first coordinator event.")

    print(f"[INFO] Parsing coordinator log {log_path}...")
    metrics = parse_coordinator_log_v2(
        log_path, seeder_start, run_config=run_config, target_bps=target_bps,
        machine_type=effective_machine_type,
    )
    if not metrics:
      print("[ERROR] Failed to parse coordinator log (and events-GCS source unavailable).",
            file=sys.stderr)
      sys.exit(1)

  # ---- Legacy human-readable summary (kept for continuity). ----
  print("\n=== BENCHMARK TELEMETRY SUMMARY ===")
  print(f"Events parsed: {metrics['events_parsed']} (old-format log: {metrics['back_compat_old_log']})")
  print(f"Total Wall Time: {metrics['wall_sec']:.2f}s")
  print(f"Total Transactions: {metrics['total_tx']}")
  print(f"Effective TPS: {metrics['total_tx'] / metrics['wall_sec'] if metrics['wall_sec'] > 0 else 0.0:.2f}")
  print(f"Verification Time: {metrics['verification_time_ms']:.2f}ms")
  print("\n--- Leaf Proving (ms) ---")
  print(f"  Count: {metrics['leaf_proving']['count']}  Min: {metrics['leaf_proving']['min']:.2f}"
        f"  Max: {metrics['leaf_proving']['max']:.2f}  Avg: {metrics['leaf_proving']['avg']:.2f}")
  print("\n--- Aggregator Folding (ms) ---")
  print(f"  Count: {metrics['node_folding']['count']}  Min: {metrics['node_folding']['min']:.2f}"
        f"  Max: {metrics['node_folding']['max']:.2f}  Avg: {metrics['node_folding']['avg']:.2f}")
  print("===================================")

  # ---- SIZING DERIVATION (#328 §C). ----
  sizing = build_sizing_derivation(metrics)
  print_sizing_derivation(sizing)

  # ---- THROUGHPUT (#321 C-sweep). ----
  print_throughput(metrics["throughput"])

  # ---- Provenance: every number traceable. ----
  provenance = {
      "source_kind": source_kind,
      "source": source_ref,
      "generated_at_utc": gen_ts,
      "git_commit": git_commit,
      "extractor": "infra-as-code/scripts/extract_gke_telemetry.py",
      "memory_safety_margin": MEMORY_SAFETY_MARGIN,
      "events_parsed": metrics["events_parsed"],
      "back_compat_old_log": metrics["back_compat_old_log"],
      "field_provenance": {
          "leaf_prove_time_distribution_ms": "derived from leaf events' prove_ms (prove_time_ms fallback for old logs)",
          "memory_requests": "peak_rss_bytes_max over successful events per role, x safety margin; UNMEASURED when all peak_rss_bytes==0",
          "prestate_hit_rate": "count of leaf prestate_source in {corpus, replay-fallback}",
          "queue_wait": "queue_wait_ms>0 over all events (0 == honest sentinel, dispatch not stamped)",
          "wave_width": "NULL — needs pull_ts_ms (absolute), log carries pull_ms (duration)",
          "duplicate_proved": "successful events grouped by output-key-equivalent; count>1 == duplicate",
          "recovery": (
              "redriven_after_lease_expiry=true count + max stale_lease_redrive_count "
              "marker (coordinator-log source); in events-GCS mode (#347) the "
              "redrive/dupe count is the number of EXTRA GCS event objects per "
              "logical key (redrive_extra_attempts_from_gcs_events) — a direct "
              "source-of-truth count from the durable per-pod objects."
          ),
          "cpu_and_pods_per_node": "NOT derived — requires node metrics (#328 §B, GCP)",
          "throughput": (
              "leaf/fold/total_cpu_core_sec = sum(prove_ms)/1000 over successful "
              "leaf / fold events (prove_time_ms fallback); core_sec_per_block = "
              "total / blocks (blocks from run_config.json or default 1). "
              "fleet_sizing_projection is a PROJECTION derived from the measured "
              "core_sec_per_block (cores = core_sec_per_block * target_bps; "
              "nodes = ceil(cores / vcpu_per_node), where vcpu_per_node is DERIVED "
              "from the REAL machine_type (run_config -> config -> null) — NEVER a "
              "hardcoded machine-specific constant (#352). Unknown machine_type -> "
              "nodes_required null + note, never a guessed divisor) — "
              "measured-derived, not fabricated; assumptions stated inline."
          ),
      },
      "notes": [
          "No benchmark number is fabricated. Missing telemetry -> null/0/UNMEASURED.",
      ],
  }

  # Leaf machine type: prefer the REAL config-resolved value; if unresolvable,
  # record the arch honestly labelled as an arch (NOT a machine type) rather than
  # mislabeling the arch under a machine-type key (#352). Never fabricate.
  if leaf_machine:
    leaf_machine_type = leaf_machine
    leaf_machine_type_note = "resolved from config.toml proving_pod.<arch>.leaf_worker.machine_type"
  else:
    leaf_machine_type = None
    leaf_machine_type_note = (
        f"UNRESOLVED — config did not yield a leaf_worker machine_type for "
        f"arch={args.arch!r}; this is the ARCH, not a machine type (#352)"
    )

  metadata = {
      "engine": "local" if local_mode else "gke",
      "benchmark_id": args.benchmark_id,
      "code_release": args.image,
      "arch": args.arch,
      "leaf_machine_type": leaf_machine_type,
      "leaf_machine_type_note": leaf_machine_type_note,
      "aggregator_machine_type": agg_machine,
      "run_start": metrics["start_time"].isoformat() if metrics["start_time"] else None,
      "run_end": metrics["end_time"].isoformat() if metrics["end_time"] else None,
  }

  summary = build_summary(metrics, metadata, provenance, sizing)

  # ---- Default output: CWD (or CWD/out), NEVER reports/ by default. ----
  if args.out:
    summary_path = args.out
  else:
    summary_path = "bench_summary.json"
  out_dir = os.path.dirname(summary_path)
  if out_dir:
    os.makedirs(out_dir, exist_ok=True)
  with open(summary_path, "w", encoding="utf-8") as f:
    json.dump(summary, f, indent=2)
  print(f"[INFO] Wrote {summary_path}")

  # ---- Upload only on the GKE path (never in local mode). ----
  if local_mode or args.no_upload or gcs_uri is None:
    print("[INFO] Local/no-upload mode: skipping GCS upload.")
    print("[SUCCESS] Telemetry extraction complete.")
    return

  gcs_dest = f"{gcs_uri}/bench_summary.json"
  print(f"[INFO] Uploading to GCS: {gcs_dest}...")
  cmd = ["gcloud", "storage", "cp", summary_path, gcs_dest]
  res = subprocess.run(cmd, capture_output=True, text=True)
  if res.returncode != 0:
    print(f"[ERROR] Failed to upload to GCS: {res.stderr}", file=sys.stderr)
    sys.exit(1)
  print("[SUCCESS] GKE telemetry extraction and upload complete.")


if __name__ == "__main__":
  main()
