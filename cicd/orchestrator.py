#!/usr/bin/env python3
"""Fan-out orchestrator for the Lighter bench container (Phase 1).

Spawns ``LIGHTER_WORKERS`` sibling worker containers, collects their
stdout, parses the bench binary's structured ``BENCH_EVENT`` JSONL
output, and prints aggregated statistics (count, mean, p50, p95, stdev,
peak RSS, CPU efficiency).

This script runs from the **host** (or the build/orchestration role),
never inside the runtime container — fan-out is an orchestration concern,
not a runtime concern, per the three-role topology (see
``docs/decisions/ADR-0001-container-topology.md``). The Cloud Run Jobs
target is implemented in ``scripts/cloud.sh``; this script intentionally
does NOT embed a Cloud Run code path because the container shouldn't know
how to spawn its own siblings on GCP.

Spawn backend: ``podman`` (the only supported backend; Cloud Run Jobs is
invoked from ``scripts/cloud.sh``).

The bench binary emits one structured event per line, prefixed with
``BENCH_EVENT `` (trailing space), as JSON Lines (see
``bench/src/events.rs`` for the authoritative schema). The three event
types this orchestrator consumes:

* ``layer_prove`` — per-chunk (L1/L2) or one-shot (L3) prove timing.
  Fields: ``layer``, ``name``, ``chunk_idx`` (null for one-shot),
  ``chunk_total``, ``tx_per_proof``, ``wall_ms``, ``cpu_ms`` (nullable),
  ``rss_mb_peak`` (nullable), ``rss_mb_after`` (nullable), ``ts``.
* ``circuit_define`` — define + build time per circuit. Fields:
  ``layer``, ``name``, ``wall_ms``, ``rss_mb_after``, ``ts``.
* ``summary`` — end-of-run aggregate. Fields: ``tx_per_proof``,
  ``tx_limit``, ``chunks``, ``total_wall_ms``, ``total_cpu_ms``
  (nullable), ``peak_rss_mb`` (nullable), ``ts``.

Only lines starting with the ``BENCH_EVENT `` prefix are parsed; the
worker's interleaved ``info!()`` lines, banners, and any other stdout
are ignored — robust against log-level changes and banner injection.
This is a hard cut: there is no fallback to the legacy
``TOTAL``/``AVERAGE`` regex parser (removed in #21). Pre-#9 images that
only emit the legacy lines can still be run directly via ``podman run``;
they just won't be aggregable through this orchestrator.
"""

from __future__ import annotations

import argparse
import json
import os
import shlex
import statistics
import subprocess
import sys
from concurrent.futures import ThreadPoolExecutor, as_completed
from dataclasses import dataclass, field
from typing import Any, Dict, List, Optional

# The bench binary emits one structured event per line, prefixed with
# this exact string (note the trailing space). Everything after the
# prefix is a single-line JSON object. See bench/src/events.rs.
_BENCH_EVENT_PREFIX = "BENCH_EVENT "


@dataclass
class WorkerResult:
    """One worker's parsed output.

    ``timings`` keeps the legacy ``Dict[label, List[seconds]]`` shape so
    ``_aggregate`` and host consumers (``scripts/local.sh``) work
    unchanged. It is populated from ``layer_prove`` events grouped by
    ``"L<layer> <name>"``. The JSONL parser additionally captures the
    richer per-worker data below.
    """

    worker_id: int
    exit_code: int
    timings: Dict[str, List[float]] = field(default_factory=dict)
    stdout_lines: int = 0
    # Richer BENCH_EVENT-derived fields (populated by _parse_events).
    events: List[Dict[str, Any]] = field(default_factory=list)
    events_parsed: int = 0
    # Per-label peak RSS (MiB) across this worker's layer_prove events,
    # plus the run-level summary peak. Keyed by the same label as timings.
    peak_rss_mb_by_label: Dict[str, int] = field(default_factory=dict)
    peak_rss_mb: Optional[int] = None
    cpu_ms_total: Optional[int] = None
    # Per-label (wall_ms_sum, cpu_ms_sum) so _aggregate can derive
    # CPU efficiency = cpu / wall. None for a label means CPU data was
    # absent (non-Linux worker) for at least one of its events.
    cpu_ms_by_label: Dict[str, Optional[int]] = field(default_factory=dict)
    wall_ms_by_label: Dict[str, int] = field(default_factory=dict)


def _layer_label(event: Dict[str, Any]) -> str:
    """Stable label for a layer_prove event: ``"L<layer> <name>"``."""
    return f"L{event.get('layer')} {event.get('name')}"


def _parse_events(stdout: str) -> List[Dict[str, Any]]:
    """Extract and decode every ``BENCH_EVENT `` JSONL line from stdout.

    Only lines starting with the ``BENCH_EVENT `` prefix are considered;
    interleaved ``info!()`` output, banners, and ``### W…`` markers are
    ignored. Malformed JSON on an otherwise-prefixed line is skipped
    (defensive — a truncated line shouldn't abort the whole aggregate).
    """
    events: List[Dict[str, Any]] = []
    for line in stdout.splitlines():
        if not line.startswith(_BENCH_EVENT_PREFIX):
            continue
        payload = line[len(_BENCH_EVENT_PREFIX):]
        try:
            obj = json.loads(payload)
        except (json.JSONDecodeError, ValueError):
            continue
        if isinstance(obj, dict):
            events.append(obj)
    return events


def _parse_stdout(stdout: str) -> Dict[str, List[float]]:
    """Group ``layer_prove`` wall times (seconds) by ``"L<layer> <name>"``.

    Back-compat shim: returns the same ``Dict[label, List[seconds]]``
    shape the legacy regex parser produced, so ``_aggregate`` and host
    consumers keep working. ``layer_prove`` is the structured replacement
    for the old per-``prove`` ``TOTAL``/``AVERAGE`` lines; a single worker
    may emit a label many times (per chunk, and per repeat), so all values
    are retained for honest intra-worker variance.
    """
    out: Dict[str, List[float]] = {}
    for event in _parse_events(stdout):
        if event.get("event") != "layer_prove":
            continue
        wall_ms = event.get("wall_ms")
        if wall_ms is None:
            continue
        out.setdefault(_layer_label(event), []).append(float(wall_ms) / 1000.0)
    return out


def _parse_worker(stdout: str) -> Dict[str, Any]:
    """Parse a worker's full stdout into the richer WorkerResult fields.

    Returns a dict (not a WorkerResult) so callers in both the podman
    spawn path and the host import path can splat it. Keys mirror the
    WorkerResult fields populated from BENCH_EVENT data.
    """
    events = _parse_events(stdout)
    timings: Dict[str, List[float]] = {}
    peak_rss_by_label: Dict[str, int] = {}
    wall_ms_by_label: Dict[str, int] = {}
    cpu_ms_by_label: Dict[str, Optional[int]] = {}
    run_peak_rss: Optional[int] = None
    run_cpu_total: Optional[int] = None

    for event in events:
        etype = event.get("event")
        if etype == "layer_prove":
            wall_ms = event.get("wall_ms")
            if wall_ms is None:
                continue
            label = _layer_label(event)
            timings.setdefault(label, []).append(float(wall_ms) / 1000.0)
            wall_ms_by_label[label] = wall_ms_by_label.get(label, 0) + int(wall_ms)
            # CPU: keep a running sum, but only if every event for this
            # label reported CPU. A single None poisons the label's eff.
            cpu_ms = event.get("cpu_ms")
            if label not in cpu_ms_by_label:
                cpu_ms_by_label[label] = 0
            if cpu_ms is None:
                cpu_ms_by_label[label] = None
            elif cpu_ms_by_label[label] is not None:
                cpu_ms_by_label[label] += int(cpu_ms)
            # Per-label peak RSS = max of layer_prove rss_mb_peak values.
            rss_peak = event.get("rss_mb_peak")
            if rss_peak is not None:
                prev = peak_rss_by_label.get(label)
                peak_rss_by_label[label] = (
                    int(rss_peak) if prev is None else max(prev, int(rss_peak))
                )
        elif etype == "summary":
            srss = event.get("peak_rss_mb")
            if srss is not None:
                run_peak_rss = int(srss) if run_peak_rss is None else max(
                    run_peak_rss, int(srss)
                )
            scpu = event.get("total_cpu_ms")
            if scpu is not None:
                run_cpu_total = (
                    int(scpu) if run_cpu_total is None else run_cpu_total + int(scpu)
                )
        # circuit_define events are captured in the raw event list for
        # downstream consumers but not folded into the layer timings.

    # If no summary peak was present, fall back to the max per-label peak.
    if run_peak_rss is None and peak_rss_by_label:
        run_peak_rss = max(peak_rss_by_label.values())

    return {
        "timings": timings,
        "events": events,
        "events_parsed": len(events),
        "peak_rss_mb_by_label": peak_rss_by_label,
        "peak_rss_mb": run_peak_rss,
        "cpu_ms_total": run_cpu_total,
        "cpu_ms_by_label": cpu_ms_by_label,
        "wall_ms_by_label": wall_ms_by_label,
    }


def _spawn_podman_worker(
    worker_id: int,
    image: str,
    env: Dict[str, str],
    extra_args: List[str],
) -> WorkerResult:
    """Run one podman worker, capture stdout, return parsed timings.

    Each worker gets a unique container name so concurrent runs don't
    collide. ``--rm`` cleans up on exit so a stuck worker doesn't pollute
    the next fan-out.
    """
    name = f"lighter-bench-w{worker_id}-{os.getpid()}"
    cmd = ["podman", "run", "--rm", "--name", name]
    for k, v in env.items():
        cmd += ["-e", f"{k}={v}"]
    cmd += [image]
    cmd += extra_args
    print(
        f"### ORCH spawn worker={worker_id} cmd={' '.join(shlex.quote(c) for c in cmd)}",
        flush=True,
    )
    proc = subprocess.run(
        cmd,
        capture_output=True,
        text=True,
        check=False,
    )
    # Surface the worker's stderr early — debugging blind workers is
    # painful. Stdout is parsed below; stderr is just dumped through.
    if proc.stderr:
        for line in proc.stderr.splitlines():
            print(f"### W{worker_id} STDERR {line}", flush=True)
    parsed = _parse_worker(proc.stdout)
    # Also dump raw stdout, marked, so the operator can grep by worker.
    for line in proc.stdout.splitlines():
        print(f"### W{worker_id} {line}", flush=True)
    return WorkerResult(
        worker_id=worker_id,
        exit_code=proc.returncode,
        stdout_lines=len(proc.stdout.splitlines()),
        **parsed,
    )


def _aggregate(results: List[WorkerResult]) -> Dict[str, Dict[str, float]]:
    """Aggregate per-label timings across all workers.

    For each label, computes: ``n``, ``mean``, ``p50``, ``p95``,
    ``stdev`` (population stdev when n < 2, else sample stdev), ``min``,
    ``max`` — all in seconds. When BENCH_EVENT data carries it, also:

    * ``peak_rss_mb`` — max-of-maxes across workers for this label.
    * ``cpu_eff_pct`` — ``cpu_ms / wall_ms * 100`` summed across workers
      for this label (omitted when any contributing event lacked CPU
      data, e.g. a non-Linux worker). This is *multicore* utilization:
      ``cpu_ms`` is total CPU time across all cores, so a value of e.g.
      1600% means ~16 cores were busy for this layer — it is expected to
      exceed 100% on parallel proving, not a bug.
    """
    all_labels = sorted({k for r in results for k in r.timings})
    agg: Dict[str, Dict[str, float]] = {}
    for label in all_labels:
        values = [v for r in results for v in r.timings.get(label, [])]
        if not values:
            continue
        values_sorted = sorted(values)
        n = len(values)
        p50 = statistics.median(values_sorted)
        # P95: standard linear-interp would be overkill for typical n=4-32
        # fan-outs. nearest-rank is honest and matches what most ops dashboards
        # use.
        p95_idx = max(0, min(n - 1, int(round(0.95 * n)) - 1))
        p95 = values_sorted[p95_idx]
        entry: Dict[str, float] = {
            "n": n,
            "mean": statistics.mean(values),
            "p50": p50,
            "p95": p95,
            "stdev": statistics.stdev(values) if n >= 2 else 0.0,
            "min": values_sorted[0],
            "max": values_sorted[-1],
        }

        # Peak RSS for this label: max across all workers' per-label peaks.
        rss_peaks = [
            r.peak_rss_mb_by_label[label]
            for r in results
            if label in r.peak_rss_mb_by_label
        ]
        if rss_peaks:
            entry["peak_rss_mb"] = max(rss_peaks)

        # CPU efficiency: sum cpu and wall across workers for this label.
        # Omit if any worker's label CPU sum is None (incomplete data).
        wall_ms_sum = 0
        cpu_ms_sum = 0
        cpu_complete = True
        for r in results:
            if label not in r.wall_ms_by_label:
                continue
            wall_ms_sum += r.wall_ms_by_label[label]
            label_cpu = r.cpu_ms_by_label.get(label)
            if label_cpu is None:
                cpu_complete = False
            else:
                cpu_ms_sum += label_cpu
        if cpu_complete and wall_ms_sum > 0 and cpu_ms_sum > 0:
            entry["cpu_eff_pct"] = cpu_ms_sum / wall_ms_sum * 100.0

        agg[label] = entry
    return agg


def _print_summary(
    results: List[WorkerResult],
    agg: Dict[str, Dict[str, float]],
    workers: int,
    backend: str,
    image: str | None,
) -> None:
    nonzero = [r for r in results if r.exit_code != 0]
    print()
    print("=" * 72)
    print(
        f"FAN-OUT SUMMARY  backend={backend} workers={workers} "
        f"completed={len(results)} failed={len(nonzero)}"
    )
    if image:
        print(f"image={image}")
    print("=" * 72)
    if nonzero:
        for r in nonzero:
            print(f"  worker {r.worker_id}: exit={r.exit_code}")
    if not agg:
        print(
            "WARNING: no BENCH_EVENT layer_prove events were parsed. "
            "Either the bench failed to run, the image predates the "
            "BENCH_EVENT instrumentation (#9), or the log format changed."
        )
        events_seen = sum(r.events_parsed for r in results)
        print(f"  (total BENCH_EVENT lines parsed across workers: {events_seen})")
        return
    # Only show the RSS / CPU-efficiency columns when at least one label
    # carries that data — keeps the table narrow on non-Linux / pre-#9
    # output while surfacing the richer metrics whenever they exist.
    show_rss = any("peak_rss_mb" in s for s in agg.values())
    show_cpu = any("cpu_eff_pct" in s for s in agg.values())
    # Column widths chosen to fit a typical 100-col terminal.
    header = f"{'label':<48} {'n':>3} {'mean':>10} {'p50':>10} {'p95':>10} {'stdev':>10}"
    if show_rss:
        header += f" {'peak_rss':>10}"
    if show_cpu:
        header += f" {'cpu_eff':>8}"
    print(header)
    print("-" * len(header))
    for label in sorted(agg):
        s = agg[label]
        row = (
            f"{label:<48} {int(s['n']):>3} "
            f"{s['mean']:>9.3f}s {s['p50']:>9.3f}s "
            f"{s['p95']:>9.3f}s {s['stdev']:>9.3f}s"
        )
        if show_rss:
            rss = s.get("peak_rss_mb")
            row += f" {(str(int(rss)) + 'MB') if rss is not None else 'NA':>10}"
        if show_cpu:
            eff = s.get("cpu_eff_pct")
            row += f" {(f'{eff:.0f}%') if eff is not None else 'NA':>8}"
        print(row)
    print("=" * 72)


def _emit_json(
    results: List[WorkerResult],
    agg: Dict[str, Dict[str, float]],
    workers: int,
    backend: str,
    image: str | None,
    out_path: str,
) -> None:
    payload = {
        "backend": backend,
        "workers": workers,
        "image": image,
        "completed": len(results),
        "failed": sum(1 for r in results if r.exit_code != 0),
        "per_worker": [
            {
                "worker_id": r.worker_id,
                "exit_code": r.exit_code,
                "timings": r.timings,
                "peak_rss_mb": r.peak_rss_mb,
                "cpu_ms_total": r.cpu_ms_total,
                "events_parsed": r.events_parsed,
                # Raw BENCH_EVENT objects, verbatim, so downstream
                # consumers (DuckDB / Pandas, per #9) get the full
                # per-chunk × per-layer measurement surface.
                "events": r.events,
            }
            for r in results
        ],
        "aggregate": agg,
    }
    with open(out_path, "w", encoding="utf-8") as fh:
        json.dump(payload, fh, indent=2, sort_keys=True)
    print(f"### ORCH wrote JSON summary to {out_path}", flush=True)


def main(argv: List[str] | None = None) -> int:
    parser = argparse.ArgumentParser(
        prog="orchestrator",
        description="Fan-out orchestrator for Lighter bench workers.",
    )
    parser.add_argument(
        "--workers",
        type=int,
        default=int(os.environ.get("LIGHTER_WORKERS", "1")),
        help="Number of worker containers to spawn (default: $LIGHTER_WORKERS or 1).",
    )
    parser.add_argument(
        "--image",
        default=os.environ.get(
            "LIGHTER_WORKER_IMAGE", "localhost/lighter-bench:latest"
        ),
        help="Container image for workers (default: $LIGHTER_WORKER_IMAGE or localhost/lighter-bench:latest).",
    )
    parser.add_argument(
        "--backend",
        choices=["podman"],
        default="podman",
        help="Worker spawn backend (only podman is supported today; Cloud Run Jobs is invoked from scripts/cloud.sh).",
    )
    parser.add_argument(
        "--tx-per-proof",
        type=int,
        default=int(os.environ.get("LIGHTER_TX_PER_PROOF", "4")),
        help="Forwarded to bench as --tx-per-proof.",
    )
    parser.add_argument(
        "--tx-limit",
        type=int,
        default=int(os.environ.get("LIGHTER_TX_LIMIT", "480")),
        help="Forwarded to bench as --tx-limit.",
    )
    parser.add_argument(
        "--bench-repeat",
        type=int,
        default=int(os.environ.get("LIGHTER_BENCH_REPEAT", "1")),
        help="Times each worker repeats the bench pipeline.",
    )
    parser.add_argument(
        "--json-out",
        default=os.environ.get("LIGHTER_ORCH_JSON_OUT", ""),
        help="If set, write a JSON summary to this path (in addition to stdout).",
    )
    args = parser.parse_args(argv)

    if args.workers < 1:
        print(f"ERROR: --workers must be >= 1, got {args.workers}", file=sys.stderr)
        return 2

    worker_env = {
        "LIGHTER_ROLE": "worker",
        "LIGHTER_TX_PER_PROOF": str(args.tx_per_proof),
        "LIGHTER_TX_LIMIT": str(args.tx_limit),
        "LIGHTER_BENCH_REPEAT": str(args.bench_repeat),
        # Propagate logging knob so all workers emit at the same level.
        "RUST_LOG": os.environ.get("RUST_LOG", "info"),
    }

    print(
        f"### ORCH start workers={args.workers} image={args.image} "
        f"tx_per_proof={args.tx_per_proof} tx_limit={args.tx_limit} "
        f"repeat={args.bench_repeat}",
        flush=True,
    )

    results: List[WorkerResult] = []
    # Concurrent spawn. ThreadPoolExecutor is fine here — workers are
    # subprocess.run calls that release the GIL while waiting.
    with ThreadPoolExecutor(max_workers=args.workers) as pool:
        futures = [
            pool.submit(
                _spawn_podman_worker,
                worker_id=i + 1,
                image=args.image,
                env=worker_env,
                extra_args=[],
            )
            for i in range(args.workers)
        ]
        for fut in as_completed(futures):
            results.append(fut.result())

    results.sort(key=lambda r: r.worker_id)
    agg = _aggregate(results)
    _print_summary(results, agg, args.workers, args.backend, args.image)

    if args.json_out:
        _emit_json(results, agg, args.workers, args.backend, args.image, args.json_out)

    # Non-zero exit if any worker failed. This is the contract the
    # Makefile and CI rely on for pass/fail.
    return 0 if all(r.exit_code == 0 for r in results) else 1


if __name__ == "__main__":
    sys.exit(main())
