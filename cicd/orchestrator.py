#!/usr/bin/env python3
"""Fan-out orchestrator for the Lighter bench container (Phase 1).

Spawns ``LIGHTER_WORKERS`` sibling worker containers, collects their
stdout, parses the bench binary's ``TOTAL`` / ``AVERAGE`` timing lines,
and prints aggregated statistics (count, mean, p50, p95, stdev).

Two backends:

* ``podman`` (default for local fan-out)
* ``noop`` (orchestrator runs each worker in-process via the bench
  binary directly; useful inside one container for smoke testing)

The Cloud Run Jobs target lives outside the container (the orchestrator
runs on the operator's workstation or inside Cloud Build) and is
implemented in ``scripts/cloud.sh``; this script intentionally does NOT
embed a Cloud Run code path because the container shouldn't know how to
spawn its own siblings on GCP — that's the build/orchestration role's
job per the three-role topology.

The bench binary's timing-line format (copied verbatim from
``bench/src/bin/bench.rs``):

    TOTAL <Circuit>::prove time: <seconds>
    AVERAGE <Circuit>::prove time: <seconds>

We extract every ``TOTAL`` / ``AVERAGE`` field, group by exact label,
and report stats. Any non-matching line is ignored — robust against
log_level changes, banner injection, etc.
"""

from __future__ import annotations

import argparse
import json
import os
import re
import shlex
import statistics
import subprocess
import sys
import threading
from concurrent.futures import ThreadPoolExecutor, as_completed
from dataclasses import dataclass, field
from typing import Dict, List, Tuple

# Capture both TOTAL and AVERAGE lines. The bench binary emits e.g.
#   TOTAL BlockPreExecutionCircuit::prove time: 12.345s
#   AVERAGE BlockTxCircuit::prove time: 1.234s
# Time is plonky2-style (Rust's Duration Display: "12.345s", "1.234ms",
# "987ns"). We normalise to seconds.
_TIMING_LINE_RE = re.compile(
    r"^(?P<kind>TOTAL|AVERAGE)\s+"
    r"(?P<label>\S+)::prove\s+time:\s+"
    r"(?P<value>[0-9]+(?:\.[0-9]+)?)(?P<unit>s|ms|us|µs|ns)?\b",
    re.MULTILINE,
)


def _to_seconds(value: str, unit: str | None) -> float:
    """Convert a Rust-Duration-style number+unit pair into seconds.

    The bench binary uses the default ``Duration`` ``Display`` impl,
    which picks the unit that gives a readable value. We invert that.
    """
    v = float(value)
    if unit is None or unit == "s":
        return v
    if unit == "ms":
        return v / 1_000.0
    if unit in ("us", "µs"):
        return v / 1_000_000.0
    if unit == "ns":
        return v / 1_000_000_000.0
    raise ValueError(f"unknown duration unit: {unit!r}")


@dataclass
class WorkerResult:
    """One worker's parsed output."""

    worker_id: int
    exit_code: int
    timings: Dict[str, List[float]] = field(default_factory=dict)
    stdout_lines: int = 0


def _parse_stdout(stdout: str) -> Dict[str, List[float]]:
    """Group ``TOTAL`` / ``AVERAGE`` timings by ``"<kind> <label>"``.

    A single worker may emit a label multiple times if ``LIGHTER_BENCH_REPEAT``
    > 1; we keep all values so the aggregate stats include intra-worker
    variance.
    """
    out: Dict[str, List[float]] = {}
    for m in _TIMING_LINE_RE.finditer(stdout):
        key = f"{m.group('kind')} {m.group('label')}"
        out.setdefault(key, []).append(
            _to_seconds(m.group("value"), m.group("unit"))
        )
    return out


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
    timings = _parse_stdout(proc.stdout)
    # Also dump raw stdout, marked, so the operator can grep by worker.
    for line in proc.stdout.splitlines():
        print(f"### W{worker_id} {line}", flush=True)
    return WorkerResult(
        worker_id=worker_id,
        exit_code=proc.returncode,
        timings=timings,
        stdout_lines=len(proc.stdout.splitlines()),
    )


def _aggregate(results: List[WorkerResult]) -> Dict[str, Dict[str, float]]:
    """Aggregate per-label timings across all workers.

    For each label, computes: ``n``, ``mean``, ``p50``, ``p95``,
    ``stdev`` (population stdev when n < 2, else sample stdev).
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
        agg[label] = {
            "n": n,
            "mean": statistics.mean(values),
            "p50": p50,
            "p95": p95,
            "stdev": statistics.stdev(values) if n >= 2 else 0.0,
            "min": values_sorted[0],
            "max": values_sorted[-1],
        }
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
            "WARNING: no TOTAL/AVERAGE timing lines were parsed. "
            "Either the bench failed to run or the log format changed."
        )
        return
    # Column widths chosen to fit a typical 100-col terminal.
    header = f"{'label':<55} {'n':>3} {'mean':>10} {'p50':>10} {'p95':>10} {'stdev':>10}"
    print(header)
    print("-" * len(header))
    for label in sorted(agg):
        s = agg[label]
        print(
            f"{label:<55} {int(s['n']):>3} "
            f"{s['mean']:>9.3f}s {s['p50']:>9.3f}s "
            f"{s['p95']:>9.3f}s {s['stdev']:>9.3f}s"
        )
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
