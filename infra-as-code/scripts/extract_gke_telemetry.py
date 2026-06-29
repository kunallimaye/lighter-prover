#!/usr/bin/env python3
# Copyright (c) Elliot Technologies, Inc.
# SPDX-License-Identifier: BUSL-1.1

"""Extracts GKE fungible pool telemetry from coordinator logs and writes bench_summary.json."""

import argparse
import datetime
import json
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


def parse_coordinator_log_v2(log_path, seeder_start_dt=None):
  if not os.path.exists(log_path):
    print(f"[ERROR] Coordinator log {log_path} not found.", file=sys.stderr)
    return None

  leaf_provings = []
  leaf_gcs = []
  leaf_totals = []
  
  node_foldings = []
  node_gcs = []
  node_totals = []

  root_reached_time = None
  first_event_time = None
  
  # Regex to match:
  # 2026-06-29T00:00:00.123456789Z <log_prefix> Received event: role=leaf, idx=0, status=success, prove_time_ms=1000, gcs_time_ms=200, total_time_ms=1200
  event_re = re.compile(
      r"^([^\s]+)\s+.*Received event: role=([\w-]+), idx=(\d+), status=(\w+), prove_time_ms=(\d+), gcs_time_ms=(\d+), total_time_ms=(\d+)"
  )
  # Regex to match:
  # 2026-06-29T00:01:00.123456789Z <log_prefix> ROOT REACHED!
  root_re = re.compile(r"^([^\s]+)\s+.*ROOT REACHED!")

  verification_time_ms = 0.0
  total_tx = 0

  with open(log_path, "r", encoding="utf-8") as f:
    for line in f:
      # Look for the JSON telemetry line (verification time and batch size)
      if '"telemetry_event":"ROOT_PROOF_VERIFIED"' in line:
        try:
          json_start = line.find("{")
          if json_start != -1:
            data = json.loads(line[json_start:])
            verification_time_ms = float(data.get("verification_time_ms", 0.0))
            total_tx = int(data.get("aggregated_batch_size", 0))
        except Exception as e:
          print(f"[WARNING] Failed to parse coordinator telemetry line: {e}", file=sys.stderr)

      # Match event
      m = event_re.match(line)
      if m:
        ts_str, role, _, status, prove_ms, gcs_ms, total_ms = m.groups()
        try:
          ts = parse_k8s_timestamp(ts_str)
          if first_event_time is None:
            first_event_time = ts
          
          prove_ms = float(prove_ms)
          gcs_ms = float(gcs_ms)
          total_ms = float(total_ms)
          
          if status == "success":
            if role == "leaf":
              leaf_provings.append(prove_ms)
              leaf_gcs.append(gcs_ms)
              leaf_totals.append(total_ms)
            elif role == "node" or role == "tree-node":
              node_foldings.append(prove_ms)
              node_gcs.append(gcs_ms)
              node_totals.append(total_ms)
        except Exception as e:
          print(f"[WARNING] Failed to parse event line: {e}", file=sys.stderr)

      # Match root reached
      m_root = root_re.match(line)
      if m_root:
        try:
          root_reached_time = parse_k8s_timestamp(m_root.group(1))
        except Exception as e:
          print(f"[WARNING] Failed to parse root reached timestamp: {e}", file=sys.stderr)

  def stats(lst):
    if not lst:
      return {"min": 0.0, "max": 0.0, "avg": 0.0, "total": 0.0, "count": 0}
    return {
        "min": min(lst),
        "max": max(lst),
        "avg": sum(lst) / len(lst),
        "total": sum(lst),
        "count": len(lst)
    }

  start_dt = seeder_start_dt
  if not start_dt and first_event_time:
    start_dt = first_event_time

  wall_sec = 0.0
  if start_dt and root_reached_time:
    wall_sec = (root_reached_time - start_dt).total_seconds()

  return {
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
  }


def main():
  parser = argparse.ArgumentParser(description="Extract GKE benchmark telemetry")
  parser.add_argument("--arch", required=True, help="Silicon architecture (t2d, c4d, etc.)")
  parser.add_argument("--coordinator-log", default="coordinator.log", help="Path to coordinator log")
  parser.add_argument("--config", default="config.toml", help="Path to config.toml")
  args = parser.parse_args()

  # 1. Read GCS prefix
  if not os.path.exists("gcs_prefix.txt"):
    print("[ERROR] gcs_prefix.txt not found. Run render_pod_spec.py first.", file=sys.stderr)
    sys.exit(1)

  with open("gcs_prefix.txt", "r") as f:
    gcs_uri = f.read().strip()

  print(f"[INFO] Target GCS URI: {gcs_uri}")

  match = re.match(r"gs://([^/]+)/(.+)", gcs_uri)
  if not match:
    print(f"[ERROR] Invalid GCS URI in gcs_prefix.txt: {gcs_uri}", file=sys.stderr)
    sys.exit(1)
  
  gcs_prefix = match.group(2)
  parts = gcs_prefix.split("/")
  if len(parts) < 4 or parts[0] != "benchmark-reports":
    print(f"[ERROR] GCS prefix does not match expected structure: {gcs_prefix}", file=sys.stderr)
    sys.exit(1)
  
  benchmark_id = parts[1]
  code_release = parts[2]
  leaf_machine = parts[3]

  # 2. Query GKE Seeder Job for start time
  print("[INFO] Querying GKE Seeder Job for start time...")
  seeder_start = get_job_start_time("lighter-seeder")
  if seeder_start:
    print(f"[INFO] Seeder Start Time: {seeder_start.isoformat()}")
  else:
    print("[WARNING] Could not retrieve seeder job start time. Will fallback to first coordinator event.")

  # 3. Parse Coordinator Log
  print(f"[INFO] Parsing coordinator log {args.coordinator_log}...")
  metrics = parse_coordinator_log_v2(args.coordinator_log, seeder_start)
  if not metrics:
    print("[ERROR] Failed to parse coordinator log.", file=sys.stderr)
    sys.exit(1)

  print("\n=== BENCHMARK TELEMETRY SUMMARY ===")
  print(f"Total Wall Time: {metrics['wall_sec']:.2f}s")
  print(f"Total Transactions: {metrics['total_tx']}")
  print(f"Effective TPS: {metrics['total_tx'] / metrics['wall_sec'] if metrics['wall_sec'] > 0 else 0.0:.2f}")
  print(f"Verification Time: {metrics['verification_time_ms']:.2f}ms")
  
  print("\n--- Leaf Proving (ms) ---")
  print(f"  Count: {metrics['leaf_proving']['count']}")
  print(f"  Min  : {metrics['leaf_proving']['min']:.2f}")
  print(f"  Max  : {metrics['leaf_proving']['max']:.2f}")
  print(f"  Avg  : {metrics['leaf_proving']['avg']:.2f}")
  print(f"  Total: {metrics['leaf_proving']['total']:.2f}")

  print("\n--- Leaf GCS Commit (ms) ---")
  print(f"  Min  : {metrics['leaf_gcs']['min']:.2f}")
  print(f"  Max  : {metrics['leaf_gcs']['max']:.2f}")
  print(f"  Avg  : {metrics['leaf_gcs']['avg']:.2f}")
  print(f"  Total: {metrics['leaf_gcs']['total']:.2f}")

  print("\n--- Aggregator Folding (ms) ---")
  print(f"  Count: {metrics['node_folding']['count']}")
  print(f"  Min  : {metrics['node_folding']['min']:.2f}")
  print(f"  Max  : {metrics['node_folding']['max']:.2f}")
  print(f"  Avg  : {metrics['node_folding']['avg']:.2f}")
  print(f"  Total: {metrics['node_folding']['total']:.2f}")

  print("\n--- Aggregator GCS Commit (ms) ---")
  print(f"  Min  : {metrics['node_gcs']['min']:.2f}")
  print(f"  Max  : {metrics['node_gcs']['max']:.2f}")
  print(f"  Avg  : {metrics['node_gcs']['avg']:.2f}")
  print(f"  Total: {metrics['node_gcs']['total']:.2f}")
  print("===================================\n")

  # 4. Resolve Aggregator Machine Type from config.toml
  agg_machine = "unknown"
  if os.path.exists(args.config):
    try:
      with open(args.config, "rb") as f:
        cfg_data = tomllib.load(f)
      agg_machine = cfg_data.get("proving_pod", {}).get(args.arch, {}).get("aggregator", {}).get("machine_type", "unknown")
    except Exception as e:
      print(f"[WARNING] Failed to read aggregator machine type from config.toml: {e}", file=sys.stderr)

  # 5. Construct bench_summary.json
  # Maintain backward compatibility for top-level keys, but inject rich stats
  leaf_total_sec = metrics["leaf_proving"]["total"] / 1000.0
  node_total_sec = metrics["node_folding"]["total"] / 1000.0
  
  summary = {
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
      "metadata": {
          "engine": "gke",
          "benchmark_id": benchmark_id,
          "code_release": code_release,
          "leaf_machine_type": leaf_machine,
          "aggregator_machine_type": agg_machine,
          "run_start": metrics["start_time"].isoformat() if metrics["start_time"] else None,
          "run_end": metrics["end_time"].isoformat() if metrics["end_time"] else None,
      },
  }

  summary_path = "bench_summary.json"
  with open(summary_path, "w", encoding="utf-8") as f:
    json.dump(summary, f, indent=2)
  print(f"[INFO] Wrote local {summary_path}")

  # Upload to GCS
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
