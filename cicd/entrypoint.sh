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

# GCS upload knobs. When BENCH_BUCKET is non-empty, the worker role
# tees its stdout to /tmp/bench.log, filters BENCH_EVENT lines into
# /tmp/bench.jsonl, uploads both to gs://$BENCH_BUCKET/$BENCH_PREFIX/,
# and writes a DONE sentinel as the last action so an external poller
# can detect completion. Unset = previous behavior (no upload). See #25.
BENCH_BUCKET="${BENCH_BUCKET:-}"
BENCH_PREFIX="${BENCH_PREFIX:-}"
BENCH_LOG_PATH="/tmp/bench.log"
BENCH_JSONL_PATH="/tmp/bench.jsonl"
BENCH_DONE_PATH="/tmp/DONE"

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

# Runs ./bench REPEAT times. Returns the exit code of the LAST bench
# invocation (0 if all succeeded). Also writes that code to
# /tmp/.bench_rc so the BENCH_BUCKET-enabled caller can recover it
# after this function runs inside a `tee` pipeline subshell — where
# PIPESTATUS would only see the trailing `echo WORKER_DONE` (=0) and
# mask any real failure.
run_worker() {
  emit_header
  local i rc=0
  for ((i=1; i<=REPEAT; i++)); do
    # Don't let `set -e` abort the loop — we want to record the exit
    # code and still emit WORKER_DONE for parseability.
    run_worker_once "${i}" || rc=$?
  done
  echo "### WORKER_DONE iterations=${REPEAT} bench_exit_code=${rc}"
  # Side-channel: when this function runs in a subshell (under `tee`),
  # the BENCH_BUCKET-enabled caller reads /tmp/.bench_rc to recover
  # bench's real exit code (PIPESTATUS would only see `echo`).
  printf '%s' "${rc}" > /tmp/.bench_rc 2>/dev/null || true
  return "$rc"
}

# Upload /tmp/bench.log and the filtered /tmp/bench.jsonl to
# gs://$BENCH_BUCKET/$BENCH_PREFIX/, then write a DONE sentinel last so
# an external poller can detect completion. Best-effort: a failed upload
# of one file does not prevent uploading the others — operators need to
# see whatever artifacts they can. DONE is always written last and
# contains the bench's exit code so consumers can distinguish
# "bench ran cleanly" from "bench panicked but plumbing worked".
upload_results_to_gcs() {
  local exit_code="$1"
  # Trim any trailing slash from prefix so we don't produce // paths.
  local prefix="${BENCH_PREFIX%/}"
  local dest="gs://${BENCH_BUCKET}/${prefix:+${prefix}/}"

  echo "### UPLOAD_START bucket=${BENCH_BUCKET} prefix=${prefix} exit_code=${exit_code} dest=${dest}"

  # Derive BENCH_EVENT-filtered subset from the full stdout. The bench
  # binary emits one BENCH_EVENT JSON object per line prefixed with
  # "BENCH_EVENT " (see #18). Strip the prefix so the .jsonl is
  # directly jq-parseable.
  if [[ -f "${BENCH_LOG_PATH}" ]]; then
    grep '^BENCH_EVENT ' "${BENCH_LOG_PATH}" | sed 's/^BENCH_EVENT //' \
      > "${BENCH_JSONL_PATH}" || true
  else
    : > "${BENCH_JSONL_PATH}"
  fi

  local upload_status=0

  # gcloud storage cp is the modern replacement for gsutil cp and is
  # what google-cloud-cli ships. `|| upload_status=...` lets us record
  # the failure without aborting under `set -e`.
  if [[ -f "${BENCH_LOG_PATH}" ]]; then
    gcloud storage cp "${BENCH_LOG_PATH}" "${dest}bench.log" \
      || { echo "### UPLOAD_WARN bench.log upload failed" >&2; upload_status=1; }
  else
    echo "### UPLOAD_WARN no ${BENCH_LOG_PATH} to upload" >&2
    upload_status=1
  fi

  if [[ -f "${BENCH_JSONL_PATH}" ]]; then
    gcloud storage cp "${BENCH_JSONL_PATH}" "${dest}bench.jsonl" \
      || { echo "### UPLOAD_WARN bench.jsonl upload failed" >&2; upload_status=1; }
  fi

  # DONE is written LAST and ALWAYS, even if previous uploads failed.
  # Its contents record the bench exit code plus an upload-status flag
  # so a poller seeing DONE knows what actually happened.
  {
    printf 'bench_exit_code=%s\n' "${exit_code}"
    printf 'upload_status=%s\n' "${upload_status}"
    printf 'ts=%s\n' "$(date -u +%Y-%m-%dT%H:%M:%SZ)"
  } > "${BENCH_DONE_PATH}"

  gcloud storage cp "${BENCH_DONE_PATH}" "${dest}DONE" \
    || echo "### UPLOAD_WARN DONE upload failed (results may be incomplete)" >&2

  echo "### UPLOAD_DONE upload_status=${upload_status} bench_exit_code=${exit_code}"
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
    #
    # When BENCH_BUCKET is set, we tee the worker's stdout to a local
    # log file and upload it (plus a BENCH_EVENT-filtered subset) to
    # GCS after exit. We must disable `set -e` around the worker
    # invocation so we can capture a non-zero exit code and still run
    # the upload — operators need to see the panic log too. See #25.
    worker_exit_code=0
    if [[ -n "${BENCH_BUCKET}" ]]; then
      echo "### BENCH_UPLOAD_ENABLED bucket=${BENCH_BUCKET} prefix=${BENCH_PREFIX:-}"
      # Disable `set -e` around the worker block: a non-zero bench
      # exit must NOT abort us before we get a chance to upload the
      # panic log. We capture the exit code via /tmp/.bench_rc
      # (written inside run_worker / inline below) rather than
      # PIPESTATUS, because the pipeline's last command is
      # `echo WORKER_DONE` which always exits 0 and would mask a
      # real failure.
      set +e
      if (( $# > 0 )); then
        bench_rc=0
        {
          echo "### WORKER_EXTRA_ARGS $*"
          emit_header
          for ((i=1; i<=REPEAT; i++)); do
            echo "### WORKER_ITERATION i=${i}/${REPEAT}"
            /app/bench "$@" || bench_rc=$?
          done
          echo "### WORKER_DONE iterations=${REPEAT} bench_exit_code=${bench_rc}"
          # Surface bench_rc to the outer shell via a side-channel
          # file, since this entire block runs in a subshell (piped
          # to tee) and would otherwise lose its variable scope.
          printf '%s' "${bench_rc}" > /tmp/.bench_rc
        } 2>&1 | tee "${BENCH_LOG_PATH}"
        worker_exit_code="$(cat /tmp/.bench_rc 2>/dev/null || echo 1)"
      else
        run_worker 2>&1 | tee "${BENCH_LOG_PATH}"
        # run_worker writes BENCH_EXIT_CODE to /tmp/.bench_rc for the
        # same subshell-scope reason as above.
        worker_exit_code="$(cat /tmp/.bench_rc 2>/dev/null || echo 1)"
      fi
      set -e
      upload_results_to_gcs "${worker_exit_code}"
      exit "${worker_exit_code}"
    else
      # Original code path — no upload, no tee, behavior unchanged.
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
