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

  # ---- Wave width: the coordinator line carries pull_ms (a DURATION), not
  # pull_ts_ms (a timestamp), so wave width across leaf pull TIMESTAMPS is NOT
  # derivable from this log. Report null with an explicit note rather than
  # misusing pull_ms as if it were a timestamp. ----
  wave_width = {
      "wave_width_ms": None,
      "note": (
          "not derivable: coordinator log carries pull_ms (per-task pull "
          "DURATION), not pull_ts_ms (absolute pull timestamp). Wave width "
          "needs the absolute pull timestamps; emit pull_ts_ms to derive it."
      ),
  }

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
    role = e["role"]
    if role == "leaf":
      return f"leaf_{e['idx']}"
    if role in ("node", "tree-node"):
      lvl = e.get("level")
      return f"tree_L{lvl}_N{e['idx']}"
    if role in ("reduction", "reduction-fold"):
      # Interval identity: idx + span uniquely name the merged interval when
      # present; fall back to (idx, level) if span is absent.
      span = e.get("merge_interval_span")
      return f"reduction_{e['idx']}_span{span}"
    return f"{role}_{e['idx']}"

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
  }


def parse_coordinator_log_v2(log_path, seeder_start_dt=None):
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
      # scheduling_class is not in the current coordinator line; report unknown
      # rather than fabricate it.
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
      "verification_time_ms": verification_time_ms,
      "total_tx": total_tx,
      "wall_sec": wall_sec,
      "start_time": start_dt,
      "end_time": root_reached_time,
      "derived": derived,
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
  args = parser.parse_args()

  local_mode = args.log_file is not None
  log_path = args.log_file if local_mode else args.coordinator_log
  git_commit = get_git_commit()
  gen_ts = datetime.datetime.now(datetime.timezone.utc).isoformat()

  seeder_start = None
  gcs_uri = None
  agg_machine = "unknown"

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
        agg_machine = cfg_data.get("proving_pod", {}).get(args.arch, {}).get("aggregator", {}).get("machine_type", "unknown")
      except Exception as e:
        print(f"[WARNING] Failed to parse config.toml: {e}", file=sys.stderr)
    gcs_prefix = f"benchmark-reports/{args.benchmark_id}/{args.image}/{args.arch}"
    gcs_uri = f"gs://{gcs_bucket}/{gcs_prefix}"
    print(f"[INFO] Target GCS URI: {gcs_uri}")
    print("[INFO] Querying GKE Seeder Job for start time...")
    seeder_start = get_job_start_time("lighter-seeder")
    if seeder_start:
      print(f"[INFO] Seeder Start Time: {seeder_start.isoformat()}")
    else:
      print("[WARNING] Could not retrieve seeder job start time. Will fallback to first coordinator event.")

  print(f"[INFO] Parsing coordinator log {log_path}...")
  metrics = parse_coordinator_log_v2(log_path, seeder_start)
  if not metrics:
    print("[ERROR] Failed to parse coordinator log.", file=sys.stderr)
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

  # ---- Provenance: every number traceable. ----
  provenance = {
      "source_kind": "local-log-file" if local_mode else "kubectl-gke",
      "source": os.path.abspath(log_path),
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
          "recovery": "redriven_after_lease_expiry=true count + max stale_lease_redrive_count marker",
          "cpu_and_pods_per_node": "NOT derived — requires node metrics (#328 §B, GCP)",
      },
      "notes": [
          "No benchmark number is fabricated. Missing telemetry -> null/0/UNMEASURED.",
      ],
  }

  metadata = {
      "engine": "local" if local_mode else "gke",
      "benchmark_id": args.benchmark_id,
      "code_release": args.image,
      "leaf_machine_type": args.arch,
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
