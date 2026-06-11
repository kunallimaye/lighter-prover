#!/usr/bin/env bash
# Streaming bench operations (issues #47-#49; root wiring #54).
# Usage: bash scripts/stream.sh {fetch-trace|record|replay|bench|test|smoke|sweep}
#
# Thin operator surface over the streaming producer/consumer pair:
#   producer  bench/feeder/feeder.py   (issue #48; contract bench/trace-format.md, #47)
#   consumer  bench --stream           (issue #49)
# The headline E2E is `bench`:
#   feeder replay --in <trace> --target-rate R | bench --stream --tx-per-proof S
#
# For the container/cloud paths see scripts/container.sh / scripts/cloud.sh.

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
# shellcheck disable=SC1091
source "${SCRIPT_DIR}/common.sh"

# `replay` emits the JSONL trace on stdout for piping — start_log would
# tee log chatter into the data plane, so it is skipped for that
# subcommand only (diagnostics go to stderr instead).
case "${1:-}" in
  replay) : ;;
  *)      start_log "stream-${1:-unknown}" ;;
esac

# common.sh already applies `set -euo pipefail`; restated here because
# the E2E pipe in bench() depends on pipefail for correct propagation.
set -o pipefail

# Configurable knobs (override at make-invoke time, e.g.
# `make stream-bench TRACE=traces/trace_15m.jsonl RATE=2213`).
TX_PER_PROOF="${TX_PER_PROOF:-4}"
MAX_QUEUE="${MAX_QUEUE:-1024}"
TARGET_CPU_NATIVE="${TARGET_CPU_NATIVE:-0}"
# TRACE / RATE / SPEED / SYNTH_RATE / DURATION / LOOP / OUT are read
# per-subcommand below; see usage messages.

# Canonical banked trace (15-min mainnet capture; too big to commit).
TRACE_GCS_URI="${TRACE_GCS_URI:-gs://kunal-scratch-bench-fleet-runs/traces/2026-06-11T0204Z-15m-offpeak/trace_15m.jsonl}"
TRACES_DIR="${PROJECT_ROOT}/traces"
FEEDER="${PROJECT_ROOT}/bench/feeder/feeder.py"

# ─── Helpers ──────────────────────────────────────────────────────────

# Resolve the bench binary, building it if missing. Mirrors
# scripts/local.sh build (cargo release build at the workspace root);
# prefers bench/bench (the `make -C bench build` artifact) when present.
ensure_bench_bin() {
  if [[ -x "${PROJECT_ROOT}/bench/bench" ]]; then
    BENCH_BIN="${PROJECT_ROOT}/bench/bench"
    return 0
  fi
  log_info "bench binary not found at bench/bench; building (release)..." >&2
  require_cmd cargo
  local rustflags=""
  [[ "${TARGET_CPU_NATIVE}" == "1" ]] && rustflags="-C target-cpu=native"
  (cd "${PROJECT_ROOT}" && RUSTFLAGS="${rustflags}" cargo build --release -p bench --bin bench)
  BENCH_BIN="${PROJECT_ROOT}/target/release/bench"
}

# Validate the replay knobs (TRACE + exactly one of RATE/SPEED) and fill
# REPLAY_ARGS with the feeder argument vector. Shared by `replay` and
# the trace path of `bench`.
build_replay_args() {
  local usage="usage: make stream-replay TRACE=<trace.jsonl> RATE=<tx/s>|SPEED=<mult> [DURATION=15m] [LOOP=1]
  TRACE  input trace (JSONL, contract: bench/trace-format.md)  — required
  RATE   retime so aggregate rate hits this tx/s (--target-rate); mutually exclusive with SPEED
  SPEED  speed multiplier, e.g. 2 = twice as fast (--speed);     mutually exclusive with RATE
  DURATION  stop after this much output time (e.g. 15m, 900s)   — optional
  LOOP=1    repeat the trace (seam per policy P3)               — optional"

  [[ -n "${TRACE:-}" ]] || die "TRACE=<trace.jsonl> is required.
${usage}"
  [[ -f "${TRACE}" ]] || die "trace file not found: ${TRACE}"
  # Canonicalize to an absolute path: bench() cds into bench/ before
  # the feeder opens the trace, so a root-relative TRACE (e.g.
  # traces/trace_15m.jsonl) would otherwise fail mid-pipeline.
  TRACE="$(cd "$(dirname "${TRACE}")" && pwd)/$(basename "${TRACE}")"
  if [[ -n "${RATE:-}" && -n "${SPEED:-}" ]]; then
    die "RATE and SPEED are mutually exclusive — give exactly one.
${usage}"
  fi
  if [[ -z "${RATE:-}" && -z "${SPEED:-}" ]]; then
    die "exactly one of RATE=<tx/s> or SPEED=<multiplier> is required.
${usage}"
  fi

  REPLAY_ARGS=(replay --in "${TRACE}")
  [[ -n "${RATE:-}" ]]     && REPLAY_ARGS+=(--target-rate "${RATE}")
  [[ -n "${SPEED:-}" ]]    && REPLAY_ARGS+=(--speed "${SPEED}")
  [[ -n "${DURATION:-}" ]] && REPLAY_ARGS+=(--duration "${DURATION}")
  [[ "${LOOP:-0}" == "1" ]] && REPLAY_ARGS+=(--loop)
  # Explicit return: under common.sh's `set -e`, a falsy final && guard
  # above would otherwise propagate as a non-zero function status.
  return 0
}

# ─── Subcommands ──────────────────────────────────────────────────────

fetch_trace() {
  local dest="${TRACES_DIR}/trace_15m.jsonl"
  if [[ -f "${dest}" ]]; then
    log_info "Trace already present at ${dest} — skipping download."
    return 0
  fi
  require_cmd gcloud
  log_info "Downloading banked trace ${TRACE_GCS_URI} ..."
  mkdir -p "${TRACES_DIR}"
  gcloud storage cp "${TRACE_GCS_URI}" "${dest}"
  log_ok "Trace at ${dest}"
}

record() {
  require_cmd python3
  local out="${OUT:-${TRACES_DIR}/recorded-$(date -u +%Y%m%dT%H%M%SZ).jsonl}"
  local duration="${DURATION:-900}"
  mkdir -p "$(dirname "${out}")"
  log_info "Recording live trace -> ${out} (duration=${duration}; needs network + deps from bench/feeder/requirements.txt)"
  python3 "${FEEDER}" record --out "${out}" --duration "${duration}"
  log_ok "Trace recorded to ${out}"
}

replay() {
  require_cmd python3
  build_replay_args
  # stdout is the data plane (JSONL trace) — keep it clean for piping.
  exec python3 "${FEEDER}" "${REPLAY_ARGS[@]}"
}

bench() {
  require_cmd python3
  ensure_bench_bin

  # Producer: either trace replay (TRACE + RATE|SPEED) or synthetic
  # peak (SYNTH_RATE, which requires DURATION — synth traces have no
  # natural end otherwise).
  local producer=()
  if [[ -n "${SYNTH_RATE:-}" ]]; then
    if [[ -n "${TRACE:-}" || -n "${RATE:-}" || -n "${SPEED:-}" ]]; then
      die "SYNTH_RATE is mutually exclusive with TRACE/RATE/SPEED."
    fi
    [[ -n "${DURATION:-}" ]] || \
      die "SYNTH_RATE requires DURATION (e.g. DURATION=15m) — synth-peak traces need an explicit length."
    producer=(python3 "${FEEDER}" synth-peak --rate "${SYNTH_RATE}" --duration "${DURATION}")
  else
    build_replay_args
    producer=(python3 "${FEEDER}" "${REPLAY_ARGS[@]}")
  fi

  # Consumer: bench --stream. Runs from bench/ because the binary loads
  # ./bench_test.json relative to CWD.
  local consumer=("${BENCH_BIN}" --stream
                  --tx-per-proof "${TX_PER_PROOF}"
                  --max-queue "${MAX_QUEUE}")
  [[ -n "${DURATION:-}" ]] && consumer+=(--duration "${DURATION}")

  log_info "E2E: ${producer[*]} | bench --stream --tx-per-proof ${TX_PER_PROOF} --max-queue ${MAX_QUEUE}${DURATION:+ --duration ${DURATION}}"
  log_warn "Real proving ahead — circuit define alone takes minutes."
  cd "${PROJECT_ROOT}/bench"
  # `|| rc=$?` keeps set -e from exiting before we can report; pipefail
  # makes rc reflect whichever stage of the pipe failed.
  local rc=0
  RUST_LOG="${RUST_LOG:-info}" "${producer[@]}" | "${consumer[@]}" || rc=$?
  if (( rc == 0 )); then
    log_ok "Streaming bench finished cleanly."
  else
    die "Streaming pipeline failed (exit=${rc})."
  fi
}

# Named run_tests (not `test`) to avoid shadowing the bash builtin,
# which common.sh helpers or future edits could depend on.
run_tests() {
  # Offline suites only: feeder unit tests + bench crate tests (stub
  # prover, zero plonky2 calls). <1 min total, no network, no proving.
  require_cmd python3
  require_cmd cargo
  log_info "Running offline feeder suite (bench/feeder/tests)..."
  (cd "${PROJECT_ROOT}/bench" && python3 -m unittest discover -s feeder/tests -v)
  log_info "Running consumer suite (cargo test -p bench)..."
  (cd "${PROJECT_ROOT}" && cargo test -p bench)
  log_ok "Streaming offline suites passed."
}

smoke() {
  # Passthrough to the bench-level manual smoke (real proving, ~minutes).
  log_info "Delegating to bench/Makefile stream-smoke (manual real-proving smoke)..."
  make -C "${PROJECT_ROOT}/bench" stream-smoke
}

sweep() {
  # Passthrough to the rate-ladder sweep (long-running, real proving).
  # Knobs (TRACE, RATES, TX_PER_PROOF, DURATION, MAX_QUEUE, OUT_DIR)
  # are read from the environment by the script itself.
  ensure_bench_bin
  if [[ "${BENCH_BIN}" != "${PROJECT_ROOT}/bench/bench" ]]; then
    # stream-sweep.sh requires the bench/bench artifact specifically.
    cp "${BENCH_BIN}" "${PROJECT_ROOT}/bench/bench"
    log_info "Copied freshly built binary to bench/bench for stream-sweep.sh"
  fi
  bash "${PROJECT_ROOT}/scripts/stream-sweep.sh"
}

# ─── Dispatch ─────────────────────────────────────────────────────────

case "${1:-}" in
  fetch-trace) fetch_trace ;;
  record)      record      ;;
  replay)      replay      ;;
  bench)       bench       ;;
  test)        run_tests   ;;
  smoke)       smoke       ;;
  sweep)       sweep       ;;
  *)           die "Usage: $0 {fetch-trace|record|replay|bench|test|smoke|sweep}" ;;
esac
