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

bench_reduction_local() {
  # (#321 Phase 7 / #328) Run the LOCAL reduction pipeline, capture its logs to a
  # file, then run the telemetry extractor's --log-file path to produce a
  # self-describing, provenance-stamped bench_summary.json + SIZING DERIVATION.
  #
  # HONESTY / SCOPE (do NOT fabricate):
  #   * A full coordinator run emitting the "Received event: ..." per-task
  #     telemetry lines requires the `pubsub` feature AND a real Pub/Sub cluster
  #     (bench/src/bin/coordinator.rs, required-features=["pubsub"]). That is a
  #     GKE/GCP concern and is OUT OF SCOPE for a local dev box.
  #   * The available local pipeline is the filesystem-transport `prover-node`
  #     leaf->tree->root run (same machinery as `test-distributed-fast`). It
  #     produces REAL proofs and REAL per-stage timing logs, but does NOT emit
  #     the coordinator "Received event:" line, so the #328 per-task sizing
  #     fields (peak_rss_bytes, prestate_source, fold_kind, ...) are NOT present
  #     in the local log. The extractor therefore reports those as
  #     UNMEASURED/null with provenance notes — it never invents them.
  #
  # The deliverable is the extractor + harness + make target that WOULD consume a
  # real coordinator run; here we validate the harness on the real local pipeline
  # log and honestly surface what that log does and does not contain.
  cd "${ROOT_DIR}"

  local out_dir="${BENCH_OUT_DIR:-${ROOT_DIR}/out/bench-reduction-local}"
  mkdir -p "${out_dir}"
  local log_file="${out_dir}/coordinator.log"
  local summary="${out_dir}/bench_summary.json"

  _log_info "Compiling prover-node distributed daemon (release)..."
  cargo build --release --bin prover-node
  # Honor CARGO_TARGET_DIR (e.g. /tmp/lighter-shared-target) if set, else ./target.
  local target_dir="${CARGO_TARGET_DIR:-${ROOT_DIR}/target}"
  local pn="${target_dir}/release/prover-node"
  [ -x "${pn}" ] || _die "prover-node binary not found at ${pn}"

  # #321 Phase 7: request the reduction fold strategy on the tree-node stage if
  # the local prover-node wires a --fold-strategy flag (it does on `tree-node`;
  # leaf-worker/root-coordinator do NOT take it). Small consistent geometry:
  # N=2 leaves, radix=2 => depth ceil(log2(2))=1, so one L1 fold then root.
  local fold_flag=""
  if "${pn}" tree-node --help 2>&1 | grep -q -- "--fold-strategy"; then
    fold_flag="--fold-strategy reduction"
    _log_info "tree-node supports --fold-strategy; requesting reduction on the fold."
  else
    _log_info "NOTE: local prover-node does not wire --fold-strategy on tree-node;"
    _log_info "      running the default fold. (Full reduction vs hex dispatch is"
    _log_info "      exercised by the coordinator path, which needs pubsub + a cluster.)"
  fi

  rm -rf reports/stark_proofs
  _log_info "Running leaf -> tree -> root pipeline (N=2, radix=2); logs -> ${log_file}"
  # RUST_LOG=info so timing/prestate info! lines are emitted. Tee to console+file.
  {
    RUST_LOG="${RUST_LOG:-info}" "${pn}" leaf-worker --chunk-idx 0 --tx-per-proof 1
    RUST_LOG="${RUST_LOG:-info}" "${pn}" leaf-worker --chunk-idx 1 --tx-per-proof 1
    RUST_LOG="${RUST_LOG:-info}" "${pn}" tree-node --level 1 --node-idx 0 --radix 2 --leaf-count 2 --tx-per-proof 1 ${fold_flag}
    RUST_LOG="${RUST_LOG:-info}" "${pn}" root-coordinator --block-number 1042 --radix 2 --leaf-count 2 --node-idx 0 --tx-per-proof 1
  } 2>&1 | tee "${log_file}"

  [ -f reports/stark_proofs/tree_L1_N0.proof ] \
    || _die "Local reduction pipeline did not produce an aggregated parent proof"
  _log_ok "Local pipeline produced a real aggregated proof. Log at ${log_file}"

  _log_info "Extracting telemetry (extract_gke_telemetry.py --log-file)..."
  # #321 C-sweep: pass reports/run_config.json (blocks, txs_per_chunk=C, ...) if
  # the pipeline wrote one, so the THROUGHPUT metric echoes C/N and computes
  # core_sec_per_block against the REAL block count. Absent => extractor defaults
  # blocks=1 and echoes C/N from the events (never fabricated).
  local run_config_arg=()
  if [[ -f "${ROOT_DIR}/reports/run_config.json" ]]; then
    run_config_arg=(--run-config "${ROOT_DIR}/reports/run_config.json")
    _log_info "Using reports/run_config.json for throughput blocks/C/N."
  fi
  python3 "${SCRIPT_DIR}/extract_gke_telemetry.py" \
    --log-file "${log_file}" \
    --arch local \
    --benchmark-id "bench-reduction-local-$(date -u +%Y%m%dT%H%M%SZ)" \
    --image "local-$(git rev-parse --short HEAD 2>/dev/null || echo unknown)" \
    "${run_config_arg[@]}" \
    --out "${summary}"

  _log_ok "Wrote ${summary}"
  _log_info "If the local log lacks coordinator 'Received event:' lines, the"
  _log_info "#328 sizing fields are reported UNMEASURED/null — that is HONEST, not"
  _log_info "a failure. A real coordinator run (GKE, #328 §B) fills them in."
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
  bench-reduction-local) shift; bench_reduction_local ;;
  verify-enhanced-proof-validity) shift; verify_enhanced_proof_validity ;;
  *) _die "Usage: $0 {container-build [arm64|amd64|all]|container-run [block.json]|test-distributed-fast|bench-reduction-local|verify-enhanced-proof-validity}" ;;
esac
