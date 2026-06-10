#!/usr/bin/env bash
# Local Rust+bench operations.
# Usage: bash scripts/local.sh {init|clean|build|test|bench|fanout|lint}
#
# These targets all operate on the host's cargo toolchain (no container).
# For the container path, see scripts/container.sh.

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
# shellcheck disable=SC1091
source "${SCRIPT_DIR}/common.sh"
start_log "local-${1:-unknown}"

# Configurable knobs (override at make-invoke time, e.g. `make local-bench TX_PER_PROOF=2`).
TX_PER_PROOF="${TX_PER_PROOF:-4}"
TX_LIMIT="${TX_LIMIT:-480}"
N="${N:-1}"
TARGET_CPU_NATIVE="${TARGET_CPU_NATIVE:-0}"

init() {
  log_info "Checking Rust toolchain..."
  require_cmd cargo
  require_cmd rustup
  # Trigger rustup to install the pinned nightly from rust-toolchain.
  (cd "${PROJECT_ROOT}" && cargo --version >/dev/null)
  log_ok "Rust toolchain ready"
}

clean() {
  log_info "Cleaning Rust target dir..."
  (cd "${PROJECT_ROOT}" && cargo clean -p bench || true)
  log_ok "Cleaned bench target"
}

build() {
  log_info "Building bench (release)..."
  local rustflags=""
  [[ "${TARGET_CPU_NATIVE}" == "1" ]] && rustflags="-C target-cpu=native"
  RUSTFLAGS="${rustflags}" cargo build --release -p bench --bin bench
  log_ok "Bench binary at target/release/bench"
}

test() {
  # Host smoke test: cargo build + bench --help only.
  #
  # We deliberately do NOT run the bench end-to-end on the host. The
  # bundled bench_test.json is schema-compatible with the source tree
  # vendored in this fork; running it against an arbitrary upstream
  # toolchain combo panics in circuit deserializers. The runtime smoke
  # test (which actually exercises plonky2 end-to-end) lives at
  # `make container-test` and runs against the in-repo source.
  #
  # The host smoke test therefore verifies:
  #   1. cargo build succeeds at HEAD (catches Rust syntax/build regressions).
  #   2. The bench binary's CLI surface is intact (--help works).
  log_info "Running host smoke test (build + --help)..."
  build
  if ! "${PROJECT_ROOT}/target/release/bench" --help >/dev/null 2>&1; then
    die "bench --help failed; CLI surface is broken."
  fi
  log_ok "Host smoke test passed (bench builds + CLI works)"
  log_info "Note: for an end-to-end runtime test, run 'make container-test'."
}

bench() {
  log_info "Running full bench (tx_per_proof=${TX_PER_PROOF} tx_limit=${TX_LIMIT})..."
  build
  cd "${PROJECT_ROOT}/bench" && \
    LIGHTER_TX_PER_PROOF="${TX_PER_PROOF}" \
    LIGHTER_TX_LIMIT="${TX_LIMIT}" \
    RUST_LOG=info \
    "${PROJECT_ROOT}/target/release/bench"
}

# Local fan-out without containers. Spawns N concurrent bench processes
# and pipes their stdout through the orchestrator script for aggregation.
# Useful for sanity-checking the orchestrator parser before involving
# podman.
fanout() {
  log_info "Local fan-out N=${N} tx_per_proof=${TX_PER_PROOF} tx_limit=${TX_LIMIT}..."
  build
  require_cmd python3

  local outdir
  outdir="$(mktemp -d)"
  log_info "Per-worker stdout: ${outdir}"

  local pids=()
  local i
  for ((i=1; i<=N; i++)); do
    (
      cd "${PROJECT_ROOT}/bench" && \
      LIGHTER_TX_PER_PROOF="${TX_PER_PROOF}" \
        LIGHTER_TX_LIMIT="${TX_LIMIT}" \
        RUST_LOG=info \
        "${PROJECT_ROOT}/target/release/bench" \
        > "${outdir}/w${i}.out" 2>&1
    ) &
    pids+=("$!")
  done

  local exit_total=0
  for pid in "${pids[@]}"; do
    if ! wait "${pid}"; then
      exit_total=1
    fi
  done

  log_info "Aggregating with orchestrator parser..."
  # Reuse the orchestrator's parsing + aggregation as the single source
  # of truth. We import orchestrator.py as a module and call its
  # internal helpers directly, so future changes to label-extraction or
  # percentile calculation propagate to both container fan-out and host
  # fan-out without drift.
  PROJECT_ROOT="${PROJECT_ROOT}" python3 - "${outdir}" "${N}" <<'PY'
import sys, os, importlib.util
outdir, n = sys.argv[1], int(sys.argv[2])
project_root = os.environ["PROJECT_ROOT"]
spec = importlib.util.spec_from_file_location(
    "orchestrator", os.path.join(project_root, "cicd", "orchestrator.py")
)
mod = importlib.util.module_from_spec(spec)
# Register in sys.modules BEFORE exec_module — Python 3.12+ dataclass
# resolution under `from __future__ import annotations` looks up the
# defining module by name and crashes otherwise.
sys.modules["orchestrator"] = mod
spec.loader.exec_module(mod)
results = []
for i in range(1, n + 1):
    p = os.path.join(outdir, f"w{i}.out")
    with open(p) as fh:
        stdout = fh.read()
    timings = mod._parse_stdout(stdout)
    results.append(mod.WorkerResult(worker_id=i, exit_code=0, timings=timings))
agg = mod._aggregate(results)
mod._print_summary(results, agg, n, "local", None)
PY

  return ${exit_total}
}

lint() {
  log_info "Running cargo clippy + fmt check..."
  (cd "${PROJECT_ROOT}" && cargo fmt --check -p bench)
  (cd "${PROJECT_ROOT}" && cargo clippy -p bench --release -- -D warnings) || \
    log_warn "clippy reported issues (non-fatal in Phase 1)"
}

# ─── Dispatch ─────────────────────────────────────────────────────────

case "${1:-}" in
  init)    init     ;;
  clean)   clean    ;;
  build)   build    ;;
  test)    test     ;;
  bench)   bench    ;;
  fanout)  fanout   ;;
  lint)    lint     ;;
  *)       die "Usage: $0 {init|clean|build|test|bench|fanout|lint}" ;;
esac
