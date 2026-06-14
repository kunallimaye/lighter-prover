#!/usr/bin/env bash
# GKE Autopilot smoke deployment automation (issue #151, G4 enabler).
#
# Operator interface is the Makefile (gke-smoke-up / gke-smoke-validate /
# gke-smoke-down). Never invoke this script directly — go through
# `make <target>` so logging + trap handlers engage.
#
# Verbs:
#   up        — submit the GKE smoke stand-up + validate Cloud Build
#               pipeline (cluster + both classes + eviction mitigation +
#               HPA-on-Pub/Sub-backlog). Stand-up and validation are one
#               pipeline so the build log is the single proof artifact.
#   validate  — alias of `up` (the up pipeline includes validation).
#   down      — submit the teardown + verify-nothing-remains pipeline.
#   plan      — terraform plan via Cloud Build (no mutation).
#
# WHY Cloud Build: the production prover platform is GKE Autopilot
# (ADR-0003 amendment 2026-06-13). The Terraform + kubectl validation runs
# AS the build/owner service account (GKE-capable), matching the existing
# Cloud-Build-drives-Terraform idiom (cicd/cloudbuild-apply.yaml). The
# build SA is passed via --service-account so the pipeline can create the
# Autopilot cluster.
#
# Config knobs (env or .env / config.toml):
#   GKE_PROJECT        GCP project (default: BUILD_PROJECT / GCP_PROJECT)
#   GKE_REGION         region (default: us-central1; must support c4a/Axion)
#   GKE_CLUSTER        cluster name (default: lighter-prover-smoke)
#   GKE_TF_BUCKET      GCS bucket for GKE TF state (default:
#                      <project>-lighter-prover-gke-state)
#   GKE_TF_PREFIX      state prefix (default: lighter-prover/gke)
#   GKE_BUILD_SA       service account the build runs as (REQUIRED for the
#                      live run — must have container.admin / owner)
#   GKE_BACKLOG_MSGS   messages to hand-publish for the HPA test (default 30)

set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
# shellcheck disable=SC1091
source "${SCRIPT_DIR}/common.sh"
start_log "gke-smoke-${1:-unknown}" 2>/dev/null || true

PROJECT_ROOT="$(cd "${SCRIPT_DIR}/.." && pwd)"

# ─── Resolve config ──────────────────────────────────────────────────
GKE_PROJECT="${GKE_PROJECT:-${BUILD_PROJECT:-${GCP_PROJECT:-}}}"
GKE_REGION="${GKE_REGION:-${BUILD_REGION:-${GCP_REGION:-us-central1}}}"
GKE_CLUSTER="${GKE_CLUSTER:-lighter-prover-smoke}"
GKE_TF_BUCKET="${GKE_TF_BUCKET:-${GKE_PROJECT}-lighter-prover-gke-state}"
GKE_TF_PREFIX="${GKE_TF_PREFIX:-lighter-prover/gke}"
GKE_BUILD_SA="${GKE_BUILD_SA:-}"
GKE_BACKLOG_MSGS="${GKE_BACKLOG_MSGS:-30}"
GKE_PUBSUB_TOPIC="${GKE_PUBSUB_TOPIC:-lighter-prover-smoke-dispatch}"
# Note: the subscription name the HPA watches lives in smoke.tfvars
# (pubsub_subscription) — it is not passed via the pipeline, so it is
# intentionally not a script knob here.

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
  [[ -n "${extra}" ]] && subs="${subs},${extra}"
  echo "${subs}"
}

cmd_up() {
  _require_project
  log_info "GKE smoke stand-up + validate → project=${GKE_PROJECT} region=${GKE_REGION} cluster=${GKE_CLUSTER}"
  [[ -n "${GKE_BUILD_SA}" ]] || log_warn "GKE_BUILD_SA not set — relying on the default Cloud Build SA having container.admin"
  # shellcheck disable=SC2046
  gcloud builds submit "${PROJECT_ROOT}" \
    --project "${GKE_PROJECT}" \
    --config "${PROJECT_ROOT}/cicd/cloudbuild-gke-smoke.yaml" \
    --substitutions "$(_subs "_PUBSUB_TOPIC=${GKE_PUBSUB_TOPIC},_BACKLOG_MSGS=${GKE_BACKLOG_MSGS}")" \
    $(_sa_flag)
}

cmd_down() {
  _require_project
  log_info "GKE smoke teardown + verify → project=${GKE_PROJECT} cluster=${GKE_CLUSTER}"
  # shellcheck disable=SC2046
  gcloud builds submit "${PROJECT_ROOT}" \
    --project "${GKE_PROJECT}" \
    --config "${PROJECT_ROOT}/cicd/cloudbuild-gke-teardown.yaml" \
    --substitutions "$(_subs)" \
    $(_sa_flag)
}

cmd_plan() {
  # A local plan needs the GCS backend + provider auth, which only the
  # GKE-capable build SA has. The gated mutation is the up pipeline's
  # auto-approve apply; for a no-credentials dry check use fmt/validate.
  log_info "GKE smoke: no local plan (backend + provider auth live in the build SA)."
  log_info "Apply:     make gke-smoke-up"
  log_info "Dry check: (cd cicd/terraform/gke && terraform init -backend=false && terraform validate)"
}

case "${1:-}" in
  up | validate) cmd_up ;;
  down) cmd_down ;;
  plan) cmd_plan ;;
  *) die "usage: gke-smoke.sh {up|validate|down|plan}" ;;
esac
