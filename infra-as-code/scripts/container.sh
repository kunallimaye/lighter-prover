#!/usr/bin/env bash
set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
source "${SCRIPT_DIR}/common.sh"

ENGINE="${CONTAINER_ENGINE:-podman}"
if ! command -v "${ENGINE}" &>/dev/null; then
  if command -v docker &>/dev/null; then
    _log_info "${ENGINE} not found, falling back to docker..."
    ENGINE="docker"
  else
    _die "Container engine '${ENGINE}' not found on PATH."
  fi
fi

container_build() {
  cd "${ROOT_DIR}"
  _require_file "${ZKP_DOCKERFILE}"
  _log_info "Building local ZKP proving container image using ${ENGINE}..."
  _log_info "  Dockerfile: ${ZKP_DOCKERFILE}"
  _log_info "  Image Tag:  ${LOCAL_ZKP_IMAGE}"

  "${ENGINE}" build -f "${ZKP_DOCKERFILE}" -t "${LOCAL_ZKP_IMAGE}" .
  _log_ok "Successfully compiled container image '${LOCAL_ZKP_IMAGE}'."
}

container_run() {
  cd "${ROOT_DIR}"
  local block_path="${1:-${BLOCK_JSON_PATH:-bench/bench_test.json}}"

  if [[ ! -f "${block_path}" ]]; then
    _die "Input block JSON '${block_path}' does not exist. Pass valid path as argument 1."
  fi

  local abs_block
  abs_block="$(cd "$(dirname "${block_path}")" && pwd)/$(basename "${block_path}")"

  _log_info "Executing local ZKP performance benchmark container using ${ENGINE}..."
  _log_info "  Container Image: ${LOCAL_ZKP_IMAGE}"
  _log_info "  Input Fixture:   ${abs_block} -> /data/bench_test.json"

  mkdir -p "${ROOT_DIR}/reports"
  _log_info "  Reports Mount:   ${ROOT_DIR}/reports -> /data/reports"

  "${ENGINE}" run --rm \
    -v "${abs_block}:/data/bench_test.json:ro" \
    -v "${ROOT_DIR}/reports:/data/reports:rw" \
    "${LOCAL_ZKP_IMAGE}"

  _log_ok "Performance benchmark testing completed successfully!"
}

# ─── Main Dispatch ────────────────────────────────────────────────────

case "${1:-}" in
  container-build) container_build ;;
  container-run)   shift; container_run "$@" ;;
  *) _die "Usage: $0 {container-build|container-run [block.json] [proof.json]}" ;;
esac
