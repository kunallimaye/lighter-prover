#!/usr/bin/env bash
# GKE Autopilot SCALE deployment automation (issue #235; #229 Track 1).
#
# The SCALE sibling of scripts/gke-smoke.sh. Where the smoke path validates the
# AUTOMATION with a trivial workload, this path applies + tears down a real,
# PARAMETERIZED scale tier (GKE_TFVARS, e.g. scale-0p2pct.tfvars) and confirms
# the tier came up.
#
# Operator interface is the Makefile (gke-scale-up / gke-scale-validate /
# gke-scale-down). Never invoke this script directly — go through
# `make <target>` so logging + trap handlers engage.
#
# Verbs:
#   up        — submit the GKE scale stand-up + readiness Cloud Build pipeline
#               (cluster + all enabled machine classes scheduled). Stand-up and
#               readiness are one pipeline so the build log is the proof artifact.
#   validate  — alias of `up` (the up pipeline includes the readiness check).
#   down      — submit the teardown + verify-nothing-remains pipeline.
#   plan      — terraform plan via Cloud Build (no mutation).
#
# WHY us-east4 default: c4a (Axion) STOCKED OUT across all us-central1 zones
# during the multi-node benchmark, while us-east4 confirmed real Axion capacity
# (docs/live-benchmark-results.md FINDING C). The literal region fallback here is
# us-east4 (NOT us-central1 like the smoke path), so an unconfigured scale run
# targets the region with proven Axion capacity.
#
# WHY Cloud Build: the production prover platform is GKE Autopilot. The Terraform
# + kubectl readiness check runs AS the build/owner service account (GKE-capable),
# matching the existing Cloud-Build-drives-Terraform idiom. The build SA is
# passed via --service-account so the pipeline can create the Autopilot cluster.
#
# Config knobs (env or .env / config.toml):
#   GKE_PROJECT     GCP project (default: BUILD_PROJECT / GCP_PROJECT)
#   GKE_REGION      region (default: us-east4 — c4a/Axion; see FINDING C)
#   GKE_CLUSTER     cluster name (default: lighter-prover-scale)
#   GKE_TFVARS      tfvars file applied (default: scale-0p2pct.tfvars)
#   GKE_TF_BUCKET   GCS bucket for GKE TF state (default:
#                   <project>-lighter-prover-gke-state)
#   GKE_TF_PREFIX   state prefix (default: lighter-prover/gke-scale — kept
#                   SEPARATE from the smoke state so they never clobber)
#   GKE_BUILD_SA    service account the build runs as (REQUIRED for the live
#                   run — must have container.admin / owner)

set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
# shellcheck disable=SC1091
source "${SCRIPT_DIR}/common.sh"
start_log "gke-scale-${1:-unknown}" 2>/dev/null || true

PROJECT_ROOT="$(cd "${SCRIPT_DIR}/.." && pwd)"

# ─── Resolve config ──────────────────────────────────────────────────
GKE_PROJECT="${GKE_PROJECT:-${BUILD_PROJECT:-${GCP_PROJECT:-}}}"
# us-east4 literal fallback (NOT us-central1): FINDING C — c4a stocked out in
# us-central1; us-east4 confirmed Axion capacity. config.toml us-east4 flows
# through BUILD_REGION/GCP_REGION; the literal fallback keeps an unconfigured
# scale run on the region with proven capacity.
GKE_REGION="${GKE_REGION:-${BUILD_REGION:-${GCP_REGION:-us-east4}}}"
GKE_CLUSTER="${GKE_CLUSTER:-lighter-prover-scale}"
GKE_TFVARS="${GKE_TFVARS:-scale-0p2pct.tfvars}"
GKE_TF_BUCKET="${GKE_TF_BUCKET:-${GKE_PROJECT}-lighter-prover-gke-state}"
GKE_TF_PREFIX="${GKE_TF_PREFIX:-lighter-prover/gke-scale}"
GKE_BUILD_SA="${GKE_BUILD_SA:-}"

_require_project() {
  [[ -n "${GKE_PROJECT}" ]] || die "GKE_PROJECT (or BUILD_PROJECT/GCP_PROJECT) must be set"
}

_sa_flag() {
  # When GKE_BUILD_SA is set, run the build as that SA (needed for GKE
  # creation since the default Cloud Build SA may lack container.admin).
  if [[ -n "${GKE_BUILD_SA}" ]]; then
    echo "--service-account=projects/${GKE_PROJECT}/serviceAccounts/${GKE_BUILD_SA}"
  fi
}

_subs() {
  local extra="${1:-}"
  local subs="_PROJECT_ID=${GKE_PROJECT}"
  subs="${subs},_REGION=${GKE_REGION}"
  subs="${subs},_TF_STATE_BUCKET=${GKE_TF_BUCKET}"
  subs="${subs},_TF_STATE_PREFIX=${GKE_TF_PREFIX}"
  subs="${subs},_CLUSTER_NAME=${GKE_CLUSTER}"
  subs="${subs},_TFVARS=${GKE_TFVARS}"
  [[ -n "${extra}" ]] && subs="${subs},${extra}"
  echo "${subs}"
}

cmd_up() {
  _require_project
  log_info "GKE scale stand-up + validate → project=${GKE_PROJECT} region=${GKE_REGION} cluster=${GKE_CLUSTER} tfvars=${GKE_TFVARS}"
  [[ -n "${GKE_BUILD_SA}" ]] || log_warn "GKE_BUILD_SA not set — relying on the default Cloud Build SA having container.admin"
  # shellcheck disable=SC2046
  gcloud builds submit "${PROJECT_ROOT}" \
    --project "${GKE_PROJECT}" \
    --config "${PROJECT_ROOT}/cicd/cloudbuild-gke-scale.yaml" \
    --substitutions "$(_subs)" \
    $(_sa_flag)
}

cmd_down() {
  _require_project
  log_info "GKE scale teardown + verify → project=${GKE_PROJECT} region=${GKE_REGION} cluster=${GKE_CLUSTER} tfvars=${GKE_TFVARS}"
  # shellcheck disable=SC2046
  gcloud builds submit "${PROJECT_ROOT}" \
    --project "${GKE_PROJECT}" \
    --config "${PROJECT_ROOT}/cicd/cloudbuild-gke-scale-teardown.yaml" \
    --substitutions "$(_subs)" \
    $(_sa_flag)
}

cmd_plan() {
  # A local plan needs the GCS backend + provider auth, which only the
  # GKE-capable build SA has. The gated mutation is the up pipeline's
  # auto-approve apply; for a no-credentials dry check use fmt/validate.
  log_info "GKE scale: no local plan (backend + provider auth live in the build SA)."
  log_info "Apply:     make gke-scale-up GKE_TFVARS=${GKE_TFVARS}"
  log_info "Dry check: (cd cicd/terraform/gke && terraform init -backend=false && terraform validate)"
}

case "${1:-}" in
  up | validate) cmd_up ;;
  down) cmd_down ;;
  plan) cmd_plan ;;
  *) die "usage: gke-scale.sh {up|validate|down|plan}" ;;
esac
