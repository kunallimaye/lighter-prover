#!/usr/bin/env bash
# Copyright (c) Elliot Technologies, Inc.
# SPDX-License-Identifier: BUSL-1.1
#
# stream-sweep.sh -- rate-ladder sweep for `bench --stream` (issue #49).
#
# For each rate in the ladder, pipes `feeder.py replay --target-rate R`
# into `bench --stream`, parses the final `stream_summary` BENCH_EVENT,
# and reports the highest rate at which the queue stays bounded
# (stable: zero dropped chunks) versus diverging (drops > 0).
#
# DEPENDENCY NOTE: the feeder (bench/feeder/feeder.py) lands in the
# sibling PR for issue #48 on the same base branch. Until both PRs
# merge, this script cannot run end-to-end -- it fails fast with a
# clear message if the feeder is missing. That is expected and
# documented in bench/README.md.
#
# Usage:
#   scripts/stream-sweep.sh [trace.jsonl]
#
# Environment knobs:
#   TRACE        Trace file to replay (default: $1 or
#                bench/feeder/fixtures/trace_excerpt.jsonl)
#   RATES        Space-separated tx/s ladder (default: "250 500 1000 2000 4438")
#   TX_PER_PROOF Chunk size (default: 4)
#   DURATION     Per-rung run cap passed to bench --duration (default: 5m)
#   MAX_QUEUE    Bounded queue capacity (default: 1024)
#   OUT_DIR      Where per-rung logs land (default: /tmp/stream-sweep.<pid>)

set -euo pipefail

REPO_ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
FEEDER="${REPO_ROOT}/bench/feeder/feeder.py"
BENCH_BIN="${REPO_ROOT}/bench/bench"

TRACE="${TRACE:-${1:-${REPO_ROOT}/bench/feeder/fixtures/trace_excerpt.jsonl}}"
RATES="${RATES:-250 500 1000 2000 4438}"
TX_PER_PROOF="${TX_PER_PROOF:-4}"
DURATION="${DURATION:-5m}"
MAX_QUEUE="${MAX_QUEUE:-1024}"
OUT_DIR="${OUT_DIR:-/tmp/stream-sweep.$$}"

die() { echo "error: $*" >&2; exit 1; }

if [ ! -f "${FEEDER}" ]; then
  die "feeder not found at ${FEEDER}. The feeder is delivered by the
issue #48 sibling PR; this sweep is runnable only after both #48 and
#49 merge. See bench/README.md (Streaming mode) for context."
fi
if [ ! -x "${BENCH_BIN}" ]; then
  die "bench binary not found at ${BENCH_BIN}. Run 'make -C bench build' first."
fi
if [ ! -f "${TRACE}" ]; then
  die "trace file not found: ${TRACE}"
fi
command -v python3 >/dev/null 2>&1 || die "python3 is required"

mkdir -p "${OUT_DIR}"
echo "stream-sweep: trace=${TRACE} tx_per_proof=${TX_PER_PROOF} duration=${DURATION} max_queue=${MAX_QUEUE}"
echo "stream-sweep: logs in ${OUT_DIR}"
echo

# Extract a field from the final stream_summary event of a log.
summary_field() { # $1=log $2=field
  grep '^BENCH_EVENT ' "$1" \
    | sed 's/^BENCH_EVENT //' \
    | python3 -c '
import json, sys
last = None
for line in sys.stdin:
    try:
        ev = json.loads(line)
    except json.JSONDecodeError:
        continue
    if ev.get("event") == "stream_summary" and ev.get("phase") == "final":
        last = ev
print("" if last is None else last.get(sys.argv[1], ""))
' "$2"
}

best_stable=""
first_diverging=""

printf '%-10s %-14s %-12s %-10s %-10s %s\n' "rate" "throughput" "lag_p95_ms" "dropped" "arrivals" "verdict"
for rate in ${RATES}; do
  log="${OUT_DIR}/rate-${rate}.log"
  # The feeder replays the trace renormalized to the target aggregate
  # rate (trace-format.md P1); bench consumes it on stdin. bench's
  # exit code is authoritative; the feeder ending first is normal.
  if ! python3 "${FEEDER}" replay --input "${TRACE}" --target-rate "${rate}" \
      | "${BENCH_BIN}" --stream \
          --tx-per-proof "${TX_PER_PROOF}" \
          --max-queue "${MAX_QUEUE}" \
          --duration "${DURATION}" \
      > "${log}" 2>&1; then
    echo "warning: rung at ${rate} tx/s exited non-zero; see ${log}" >&2
  fi

  dropped="$(summary_field "${log}" dropped_chunks)"
  throughput="$(summary_field "${log}" throughput_tx_s)"
  lag_p95="$(summary_field "${log}" lag_p95_ms)"
  arrivals="$(summary_field "${log}" arrivals)"

  if [ -z "${dropped}" ]; then
    verdict="no-summary"
  elif [ "${dropped}" = "0" ]; then
    verdict="stable"
    best_stable="${rate}"
  else
    verdict="diverging"
    [ -z "${first_diverging}" ] && first_diverging="${rate}"
  fi
  printf '%-10s %-14s %-12s %-10s %-10s %s\n' \
    "${rate}" "${throughput:-?}" "${lag_p95:-?}" "${dropped:-?}" "${arrivals:-?}" "${verdict}"
done

echo
if [ -n "${best_stable}" ]; then
  echo "highest stable rate (queue bounded, zero drops): ${best_stable} tx/s"
else
  echo "no stable rate found in the ladder (${RATES})"
fi
if [ -n "${first_diverging}" ]; then
  echo "first diverging rate (drops > 0): ${first_diverging} tx/s"
fi
