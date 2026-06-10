#!/usr/bin/env bash
# Lighter bench container entrypoint.
#
# Roles:
#   worker        Run ./bench once (or LIGHTER_BENCH_REPEAT times) against
#                 LIGHTER_BENCH_INPUT, streaming all timing output to
#                 stdout. The bench binary's own log lines
#                 ("TOTAL ... time: ...", "AVERAGE ... time: ...") are the
#                 parseable contract.
#   orchestrator  Spawn LIGHTER_WORKERS sibling worker containers, collect
#                 their stdout, compute mean / p50 / p95 / stdev across
#                 the timing fields. Implemented in /app/orchestrator.py.
#
# Phase 1 ships embarrassingly-parallel fan-out: every worker runs the
# full ./bench pipeline against the same fixture. True work-sharding
# (layer-1 partitioning across workers) is deferred to Phase 2 and
# requires Rust changes.
set -euo pipefail

ROLE="${LIGHTER_ROLE:-worker}"
INPUT="${LIGHTER_BENCH_INPUT:-/app/bench_test.json}"
REPEAT="${LIGHTER_BENCH_REPEAT:-1}"

emit_header() {
  # Single banner line consumed by the orchestrator to bind a run's
  # stdout to its container identity. Cheap, parseable, idempotent.
  printf '### LIGHTER_BENCH_RUN role=%s input=%s repeat=%s tx_per_proof=%s tx_limit=%s ref=%s native=%s host=%s ts=%s\n' \
    "${ROLE}" \
    "${INPUT}" \
    "${REPEAT}" \
    "${LIGHTER_TX_PER_PROOF:-4}" \
    "${LIGHTER_TX_LIMIT:-480}" \
    "${LIGHTER_REF:-unknown}" \
    "${LIGHTER_TARGET_CPU_NATIVE:-0}" \
    "$(hostname)" \
    "$(date -u +%Y-%m-%dT%H:%M:%SZ)"
}

run_worker_once() {
  local i="$1"
  echo "### WORKER_ITERATION i=${i}/${REPEAT}"
  # Bench reads CLI flags and env vars (see issue #4). We pass via env
  # so callers can override either way without rebuilding the image.
  /app/bench
}

run_worker() {
  emit_header
  local i
  for ((i=1; i<=REPEAT; i++)); do
    run_worker_once "${i}"
  done
  echo "### WORKER_DONE iterations=${REPEAT}"
}

run_orchestrator() {
  emit_header
  exec python3 /app/orchestrator.py "$@"
}

case "${ROLE}" in
  worker)
    # Forward any extra args after `--` to the bench binary. Today the
    # bench binary already reads its config from env (LIGHTER_TX_PER_PROOF,
    # LIGHTER_TX_LIMIT), so the extra-args path is unused — kept here so
    # callers can pass `--tx-per-proof 8` without rebuilding if they want.
    if (( $# > 0 )); then
      echo "### WORKER_EXTRA_ARGS $*"
      emit_header
      for ((i=1; i<=REPEAT; i++)); do
        echo "### WORKER_ITERATION i=${i}/${REPEAT}"
        /app/bench "$@"
      done
      echo "### WORKER_DONE iterations=${REPEAT}"
    else
      run_worker
    fi
    ;;
  orchestrator)
    run_orchestrator "$@"
    ;;
  *)
    echo "ERROR: unknown LIGHTER_ROLE='${ROLE}' (expected: worker | orchestrator)" >&2
    exit 2
    ;;
esac
