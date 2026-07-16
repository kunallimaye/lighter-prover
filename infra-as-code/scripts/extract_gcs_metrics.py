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

# Documented default target block-arrival rate (blocks/sec) for the VM
# projection. Overridable via --target-bps. The production window is 10-12 bps.
DEFAULT_TARGET_BPS = 10.0


def vcpu_per_node_from_machine_type(mtype):
  """Derive vCPU-per-node from a GCP machine-type string, or None if underivable.

  Trailing-int parse (``c4d-highcpu-64`` -> 64). ANTI-FABRICATION (#352):
  missing/``"unknown"``/unparseable -> None; NEVER a guessed default.
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
      # ANTI-FABRICATION (#352): do NOT assume 500 txs when absent. Without a
      # real total_transactions we cannot derive wall from TPS; leave wall as-is
      # (it will fail the 0.0 guard below and be reported as UNMEASURED) rather
      # than fabricate a tx count.
      txs = d.get("total_transactions")
      if tps > 0 and txs is not None:
        wall = float(txs) / tps
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
      # NOTE (#352, low priority): this Sheet-ID default is a hardcoded org
      # artifact. It is overridable here; consider sourcing it from config in a
      # follow-up so the extractor carries no environment-specific default.
      "--sheet-id",
      default="1z8bIeeKaEnXP6UZW52pGLll0XrwjoLS0aBJOvs1qqd0",
      help="Target Google Spreadsheet ID for importing (overridable; see #352).",
  )
  p.add_argument(
      "--target-bps",
      type=float,
      default=DEFAULT_TARGET_BPS,
      help=(
          "Target block-arrival rate (blocks/sec) for the VM/node projection. "
          f"Default {DEFAULT_TARGET_BPS} (production window is 10-12 bps). "
          "Previously hardcoded to 10.0 (#352)."
      ),
  )
  return p.parse_args()


def main():
  args = parse_args()
  target_bps = args.target_bps
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
    # VM projection at the configurable target bps (previously hardcoded 10.0).
    proj_vms = (
        round(target_bps * avg_w / conc_blocks, 2) if conc_blocks > 0 else 0.0
    )
    # (#352) The machine type is parsed from the GCS path (`mtype`). When it is
    # derivable, ALSO normalize to a per-vCPU node count so the projection does
    # not implicitly assume a fixed node shape; when it is not, emit null + a
    # note rather than fabricate a node count.
    vcpu = vcpu_per_node_from_machine_type(mtype)
    if vcpu:
      proj_nodes = math.ceil(proj_vms / vcpu) if proj_vms > 0 else 0
      nodes_note = None
    else:
      proj_nodes = None
      nodes_note = (
          f"vcpu_per_node underivable — machine_type {mtype!r}; cannot size "
          "nodes (no guessed divisor)"
      )
    avg_min = round(avg_w / 60.0, 8)
    extracted_records.append({
        "benchmark_id": bench_id,
        "code_release": code_rel,
        "machine_type": mtype,
        "concurrent_jobs_or_blocks": conc_blocks,
        "min_wall_time_sec": min_w,
        "max_wall_time_sec": max_w,
        "avg_wall_time_sec": avg_w,
        "target_bps": target_bps,
        # Arch-neutral, bps-parametric key (was projected_vm_count_10_bps).
        "projected_vm_count_at_target_bps": proj_vms,
        "vcpu_per_node": vcpu,
        "projected_node_count_at_target_bps": proj_nodes,
        "projected_node_count_note": nodes_note,
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
      "Target Blocks/Sec",
      "Projected VM Count (at Target Blocks/Sec)",
      "vCPU per Node",
      "Projected Node Count (at Target Blocks/Sec)",
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
          r["target_bps"],
          r["projected_vm_count_at_target_bps"],
          r["vcpu_per_node"] if r["vcpu_per_node"] is not None else "",
          r["projected_node_count_at_target_bps"]
          if r["projected_node_count_at_target_bps"] is not None else "",
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
