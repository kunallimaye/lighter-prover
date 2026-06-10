#!/usr/bin/env bash
# Container (Podman) operations for the Lighter bench image.
# Usage: bash scripts/container.sh {init|clean|build|run|test|bench|fanout}

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
# shellcheck disable=SC1091
source "${SCRIPT_DIR}/common.sh"
start_log "container-${1:-unknown}"

# Knobs (override at make-invoke time).
LIGHTER_REF="${LIGHTER_REF:-5bbb307dfb26276c48054f2c3ea9dcfe80d3678a}"
TARGET_CPU_NATIVE="${TARGET_CPU_NATIVE:-0}"
TX_PER_PROOF="${TX_PER_PROOF:-4}"
TX_LIMIT="${TX_LIMIT:-480}"
N="${N:-1}"
BENCH_REPEAT="${BENCH_REPEAT:-1}"

# Override the image-tag defaults from common.sh — we use a more
# specific local name for the bench image.
LOCAL_IMAGE="${LOCAL_IMAGE:-localhost/lighter-bench:latest}"

init() {
  log_info "Initializing container environment..."
  require_cmd podman
  log_ok "Container environment ready"
}

clean() {
  log_info "Cleaning bench containers and image..."
  # Wildcard rm any lingering bench workers from a previous fan-out.
  podman ps --filter "name=lighter-bench-" -aq | xargs -r podman rm -f >/dev/null 2>&1 || true
  podman rmi "${LOCAL_IMAGE}" >/dev/null 2>&1 || true
  log_ok "Cleaned bench containers and image"
}

build() {
  log_info "Building bench container (LIGHTER_REF=${LIGHTER_REF}, native=${TARGET_CPU_NATIVE})..."
  require_cmd podman
  podman build \
    --build-arg "LIGHTER_REF=${LIGHTER_REF}" \
    --build-arg "TARGET_CPU_NATIVE=${TARGET_CPU_NATIVE}" \
    -f "${PROJECT_ROOT}/cicd/Containerfile" \
    -t "${LOCAL_IMAGE}" \
    "${PROJECT_ROOT}"
  # Acceptance: image < 1 GB. Surface size so the operator sees it.
  local sz
  sz="$(podman image inspect "${LOCAL_IMAGE}" --format '{{.Size}}' 2>/dev/null || echo 0)"
  log_ok "Image built: ${LOCAL_IMAGE} ($(numfmt --to=iec "${sz}" 2>/dev/null || echo "${sz} bytes"))"
}

run() {
  log_info "Running one bench worker (defaults)..."
  require_cmd podman
  podman run --rm \
    --name "lighter-bench-run" \
    -e LIGHTER_ROLE=worker \
    -e "LIGHTER_TX_PER_PROOF=${TX_PER_PROOF}" \
    -e "LIGHTER_TX_LIMIT=${TX_LIMIT}" \
    -e "RUST_LOG=${RUST_LOG:-info}" \
    "${LOCAL_IMAGE}"
}

# Smoke test: tiny config, asserts a TOTAL timing line appears.
# Intentionally lightweight so it can run in CI without burning a full
# 5-minute prove cycle. Heavy run is `make local-bench`.
test() {
  log_info "Running container smoke test (tx_per_proof=1 tx_limit=4)..."
  require_cmd podman
  local out
  out="$(podman run --rm \
    --name "lighter-bench-smoke" \
    -e LIGHTER_ROLE=worker \
    -e LIGHTER_TX_PER_PROOF=1 \
    -e LIGHTER_TX_LIMIT=4 \
    -e RUST_LOG=info \
    "${LOCAL_IMAGE}" 2>&1)"
  echo "${out}" | tail -50
  if ! echo "${out}" | grep -qE '^TOTAL .*::prove time'; then
    die "Container smoke test failed: no 'TOTAL ... ::prove time' line in output."
  fi
  log_ok "Container smoke test passed"
}

# Full prove pipeline, single container, default config.
# tx_per_proof=4 tx_limit=480 (per #4 baseline) is the throughput-optimal
# single-worker config under upstream 5bbb307.
bench() {
  log_info "Running full bench (tx_per_proof=${TX_PER_PROOF} tx_limit=${TX_LIMIT})..."
  require_cmd podman
  podman run --rm \
    --name "lighter-bench-full" \
    -e LIGHTER_ROLE=worker \
    -e "LIGHTER_TX_PER_PROOF=${TX_PER_PROOF}" \
    -e "LIGHTER_TX_LIMIT=${TX_LIMIT}" \
    -e "LIGHTER_BENCH_REPEAT=${BENCH_REPEAT}" \
    -e "RUST_LOG=${RUST_LOG:-info}" \
    "${LOCAL_IMAGE}"
}

# N-worker fan-out. Drives the orchestrator on the host (not inside a
# container) so the host's podman socket spawns the worker containers
# directly. Phase-1 fan-out: every worker runs the same fixture; no
# work-sharding.
fanout() {
  log_info "Fan-out N=${N} tx_per_proof=${TX_PER_PROOF} tx_limit=${TX_LIMIT}..."
  require_cmd podman
  require_cmd python3
  LIGHTER_WORKER_IMAGE="${LOCAL_IMAGE}" \
    python3 "${PROJECT_ROOT}/cicd/orchestrator.py" \
      --workers "${N}" \
      --image "${LOCAL_IMAGE}" \
      --tx-per-proof "${TX_PER_PROOF}" \
      --tx-limit "${TX_LIMIT}" \
      --bench-repeat "${BENCH_REPEAT}"
}

# ─── Dispatch ─────────────────────────────────────────────────────────

case "${1:-}" in
  init)    init    ;;
  clean)   clean   ;;
  build)   build   ;;
  run)     run     ;;
  test)    test    ;;
  bench)   bench   ;;
  fanout)  fanout  ;;
  *)       die "Usage: $0 {init|clean|build|run|test|bench|fanout}" ;;
esac
