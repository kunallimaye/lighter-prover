#!/usr/bin/env python3
"""Standard telemetry extractor invoked via `make cloud-extract-metrics`.

Pulls authentic measured block finality metrics directly from uploaded GCS
bench_summary.json objects restricting searches to input parent prefix.
"""

import argparse
import concurrent.futures
import csv
import datetime
import json
import math
import subprocess
import sys


def fetch_summary(gcs_uri):
  out = subprocess.run(
      ["gcloud", "storage", "cat", gcs_uri], capture_output=True, text=True
  )
  try:
    d = json.loads(out.stdout)
    cpt = d.get("cryptographic_phase_telemetry", {})
    wall = float(cpt.get("total_pipelined_scope_wall_sec", 0.0))
    if wall == 0.0:
      wall = float(cpt.get("total_stark_prove_sec", 0.0))
    if wall == 0.0:
      tps = float(d.get("system_telemetry", {}).get("effective_tps", 0.0))
      txs = float(d.get("total_transactions", 500))
      if tps > 0:
        wall = txs / tps
    if wall == 0.0:
      raise ValueError("Computed wall time is 0.0")
    conc = 0
    for part in gcs_uri.split("/"):
      if part.startswith("job_") and part[4:].isdigit():
        conc = int(part[4:])
      elif part.startswith("blocks-") and part[7:].isdigit():
        conc = int(part[7:])
    if conc == 0:
      conc = 1
    if conc <= 0 or conc > 10:
      raise ValueError(f"Invalid concurrency count: {conc}")
    return gcs_uri, round(wall, 8), "", conc
  except Exception as e:
    print(f"[ERROR] Corrupted or missing telemetry artifact {gcs_uri}: {e}", file=sys.stderr)
    return gcs_uri, None, None, None


def parse_args():
  p = argparse.ArgumentParser(description="Extract GCS finality telemetry.")
  p.add_argument(
      "--gcs-prefix",
      default="gs://kunal-scratch-tfstate/benchmark-reports/**/bench_summary.json",
      help="GCS parent prefix or wildcard path to scan.",
  )
  p.add_argument(
      "--sheet-id",
      default="1z8bIeeKaEnXP6UZW52pGLll0XrwjoLS0aBJOvs1qqd0",
      help="Target Google Spreadsheet ID for importing.",
  )
  return p.parse_args()


def main():
  args = parse_args()
  query_path = args.gcs_prefix
  if not query_path.endswith(".json"):
    query_path = query_path.rstrip("/") + "/**/bench_summary.json"

  print(f"[Phase 1] Listing summary objects in GCS under {query_path}...")
  res = subprocess.run(
      ["gcloud", "storage", "ls", query_path],
      capture_output=True,
      text=True,
  )
  gcs_paths = sorted([f.strip() for f in res.stdout.splitlines() if f.strip()])
  print(f"Found {len(gcs_paths)} uploaded bench_summary.json files in GCS.")
  if not gcs_paths:
    print("[WARNING] No GCS summary files found matching prefix.")
    return

  print(f"[Phase 2] Fetching {len(gcs_paths)} summary objects...")
  cache = {}
  with concurrent.futures.ThreadPoolExecutor(max_workers=32) as ex:
    for u, wall, code_rel, conc in ex.map(fetch_summary, gcs_paths):
      if wall is not None:
        cache[u] = (wall, code_rel, conc)

  print("[Phase 3] Grouping records by Benchmark ID, Code Release, Machine Type, and Timestamp...")
  import re
  groups = {}
  for p in sorted(gcs_paths):
    if p not in cache:
      continue
    parts = p.split("/")
    if len(parts) >= 7 and parts[3] == "benchmark-reports":
      if not parts[5].startswith("v0.0") and not parts[5].startswith("radix"):
        continue
      bench_id = parts[4]
      code_rel = parts[5]
      mtype = parts[6]
      wall, _, _ = cache[p]
      ts_m = re.findall(r"\d{8}-\d{6}", p)
      ts = ts_m[-1] if ts_m else "default_ts"
      key = (bench_id, code_rel, mtype, ts)
      groups.setdefault(key, []).append(wall)

  extracted_records = []
  for (bench_id, code_rel, mtype, ts), walls in sorted(groups.items()):
    min_w = min(walls)
    max_w = max(walls)
    avg_w = round(sum(walls) / len(walls), 8)
    conc_blocks = len(walls)
    proj_vms = round(10.0 * avg_w / conc_blocks, 2) if conc_blocks > 0 else 0.0
    avg_min = round(avg_w / 60.0, 8)
    extracted_records.append({
        "benchmark_id": bench_id,
        "code_release": code_rel,
        "machine_type": mtype,
        "concurrent_jobs_or_blocks": conc_blocks,
        "min_wall_time_sec": min_w,
        "max_wall_time_sec": max_w,
        "avg_wall_time_sec": avg_w,
        "projected_vm_count_10_bps": proj_vms,
        "avg_wall_time_min": avg_min,
        "timestamp": ts,
    })

  bench_id_suffix = ""
  if extracted_records:
    bench_id_suffix = "_" + str(extracted_records[0]["benchmark_id"])

  json_path = f"reports/capstone_extracted_telemetry{bench_id_suffix}.json"
  with open(json_path, "w", encoding="utf-8") as f:
    json.dump(extracted_records, f, indent=2)

  csv_path = f"reports/capstone_extracted_telemetry{bench_id_suffix}.csv"
  headers = [
      "Benchmark ID",
      "Code Release",
      "Machine Type",
      "Concurrent Jobs or Blocks",
      "Minimum Elapsed Wall Time (sec)",
      "Maximum Elapsed Wall Time (sec)",
      "Average Elapsed Wall Time (sec)",
      "Projected VM Count (10 Blocks/Sec)",
      "Avg Time",
      "Execution Timestamp",
  ]
  with open(csv_path, "w", newline="", encoding="utf-8") as f:
    w = csv.writer(f)
    w.writerow(headers)
    for r in extracted_records:
      w.writerow([
          r["benchmark_id"],
          r["code_release"],
          r["machine_type"],
          r["concurrent_jobs_or_blocks"],
          r["min_wall_time_sec"],
          r["max_wall_time_sec"],
          r["avg_wall_time_sec"],
          r["projected_vm_count_10_bps"],
          r["avg_wall_time_min"],
          r["timestamp"],
      ])
  print(f"[OK] Saved extracted telemetry JSON {json_path} and CSV {csv_path}")

  if args.sheet_id:
    print(f"[Phase 4] Importing CSV into Google Spreadsheet {args.sheet_id}...")
    ts = datetime.datetime.now(datetime.timezone.utc).strftime(
        "%Y-%m-%d_%H-%M-%S_UTC"
    )
    gsheets_cli = "/google/bin/releases/gemini-agents-gsheets/gsheets"
    try:
      subprocess.run(
          [gsheets_cli, "mutate", "add-sheet", args.sheet_id, f"--title={ts}"],
          check=True,
      )
      subprocess.run(
          [
              gsheets_cli,
              "mutate",
              "import-csv",
              args.sheet_id,
              csv_path,
              f"--sheet={ts}",
          ],
          check=True,
      )
      print(f"[SUCCESS] Reconciled telemetry imported to sheet tab: {ts}")
    except Exception as e:
      print(f"[WARNING] Spreadsheet import skipped: {e}")


if __name__ == "__main__":
  main()
