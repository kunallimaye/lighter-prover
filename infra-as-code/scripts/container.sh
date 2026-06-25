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
  local arch="${1:-arm64}"
  cd "${ROOT_DIR}"

  if [[ "${arch}" == "all" ]]; then
    _log_info "Building both ARM64 and AMD64 container images..."
    container_build arm64
    container_build amd64
    return
  fi

  local dockerfile="Dockerfile.zkp-arm64"
  local tag="lighter-zkp-prover:arm64"
  if [[ "${arch}" == "amd64" ]]; then
    dockerfile="Dockerfile.zkp"
    tag="lighter-zkp-prover:amd64"
  fi

  _require_file "${dockerfile}"
  _log_info "Building local ZKP proving container (${arch}) using ${ENGINE}..."
  _log_info "  Dockerfile: ${dockerfile}"
  _log_info "  Image Tag:  ${tag}"

  "${ENGINE}" build -f "${dockerfile}" -t "${tag}" -t "lighter-zkp-prover:latest" .
  _log_ok "Successfully compiled container image '${tag}'."
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

test_distributed_fast() {
  cd "${ROOT_DIR}"
  _log_info "Compiling prover-node distributed daemon..."
  cargo build --release --bin prover-node

  # The prover-node uses a filesystem proof transport (reports/stark_proofs/),
  # NOT Pub/Sub. Leaves write proofs, the tree node reads + folds them, and the
  # root coordinator harvests + verifies the parent. Every stage verifies its own
  # proof; this integration run fails (non-zero exit) if any verification fails.
  rm -rf reports/stark_proofs
  _log_info "Running leaf -> tree -> root pipeline over the filesystem transport..."
  target/release/prover-node leaf-worker --chunk-idx 0 --tx-per-proof 1
  target/release/prover-node leaf-worker --chunk-idx 1 --tx-per-proof 1
  target/release/prover-node tree-node --level 1 --node-idx 0 --radix 2 --tx-per-proof 1
  target/release/prover-node root-coordinator --block-number 1042 --radix 2 --node-idx 0 --tx-per-proof 1

  # Assert the real aggregated parent proof was produced.
  [ -f reports/stark_proofs/tree_L1_N0.proof ] \
    || _die "Distributed pipeline did not produce an aggregated parent proof"

  _log_ok "Distributed leaf->tree->root pipeline produced and verified a real aggregated proof!"
}

verify_enhanced_proof_validity() {
  cd "${ROOT_DIR}"
  # On-chain (EVM) verification of the SNARK-wrapped validium proof is NOT
  # implemented. It requires: (1) the gnark wrapper producing real Groth16/Plonk
  # calldata from a harvested root proof, and (2) the deployed verifier contract
  # plus a Foundry test that calls verifyProof() against that real calldata and
  # measures actual gas. None of that is wired here.
  #
  # The previous implementation fabricated contracts/test_calldata.json and
  # printed a "verified on EVM in <= 235,000 gas" claim without running any
  # verification. That fake-success path has been removed; we fail loudly instead
  # of fabricating a result.
  _die "verify_enhanced_proof_validity: on-chain EVM proof verification is not implemented (no real calldata harvest / verifier-contract gas measurement is wired). Refusing to fabricate a gas figure. See issue #283."
}

# ─── Main Dispatch ────────────────────────────────────────────────────

case "${1:-}" in
  container-build)       shift; container_build "${1:-arm64}" ;;
  container-run)         shift; container_run "$@" ;;
  test-distributed-fast) shift; test_distributed_fast ;;
  verify-enhanced-proof-validity) shift; verify_enhanced_proof_validity ;;
  *) _die "Usage: $0 {container-build [arm64|amd64|all]|container-run [block.json]|test-distributed-fast|verify-enhanced-proof-validity}" ;;
esac
