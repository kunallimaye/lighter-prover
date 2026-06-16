#!/usr/bin/env bash
set -euo pipefail

# ─── Logging Helpers ──────────────────────────────────────────────────

_log_info()  { printf '\033[1;34m[INFO]\033[0m %s\n' "$*"; }
_log_ok()    { printf '\033[1;32m[OK]\033[0m %s\n' "$*"; }
_log_error() { printf '\033[1;31m[ERROR]\033[0m %s\n' "$*" >&2; }
_die()       { _log_error "$@"; exit 1; }

# ─── Configuration Resolvers ──────────────────────────────────────────

_resolve_build_project() {
  local project
  project="$(gcloud config get-value project 2>/dev/null || true)"
  if [[ -z "${project}" ]]; then
    _die "Unable to detect gcloud project. Run 'gcloud config set project <id>' first."
  fi
  echo "${project}"
}

_build_substitutions() {
  local build_project="$1"
  local action="$2"

  local bucket="${TF_STATE_BUCKET:-${build_project}-tfstate}"
  local prefix="${TF_STATE_PREFIX:-lighter-prover}"
  local region="${GCP_REGION:-us-central1}"
  local ar_repo="${AR_REPO:-lighter-prover}"
  local builder_sa="${BUILDER_SA_EMAIL:-lighter-builder@${build_project}.iam.gserviceaccount.com}"
  local orch_project="${ORCH_PROJECT:-${build_project}}"
  local runtime_project="${RUNTIME_PROJECT:-${build_project}}"

  local subs="_TF_ACTION=${action}"
  subs="${subs},_REGION=${region}"
  subs="${subs},_TF_STATE_BUCKET=${bucket}"
  subs="${subs},_TF_STATE_PREFIX=${prefix}"
  subs="${subs},_BUILD_PROJECT_ID=${build_project}"
  subs="${subs},_ORCH_PROJECT_ID=${orch_project}"
  subs="${subs},_RUNTIME_PROJECT_ID=${runtime_project}"
  subs="${subs},_AR_REPO=${ar_repo}"
  subs="${subs},_BUILDER_SA_EMAIL=${builder_sa}"
  subs="${subs},_RUNTIME_SA_EMAIL=${RUNTIME_SA_EMAIL:-}"

  echo "${subs}"
}

# ─── Core IaC Execution ───────────────────────────────────────────────

_execute_cloudbuild() {
  local action="$1"
  local build_project
  build_project="$(_resolve_build_project)"

  _log_info "Submitting IaC pipeline to Cloud Build (action: ${action})..."
  _log_info "  Build Project: ${build_project}"

  local substitutions
  substitutions="$(_build_substitutions "${build_project}" "${action}")"

  gcloud builds submit . \
    --project="${build_project}" \
    --config="infra-as-code/cloudbuild.yaml" \
    --substitutions="${substitutions}" \
    --quiet

  _log_ok "IaC pipeline completed successfully."
}

# ─── Operator Verbs ───────────────────────────────────────────────────

cloud_deploy() {
  _execute_cloudbuild "apply"
}

cloud_plan() {
  _execute_cloudbuild "plan"
}

cloud_destroy() {
  _execute_cloudbuild "destroy"
}

# ─── Main Dispatch ────────────────────────────────────────────────────

case "${1:-}" in
  cloud-deploy)  cloud_deploy ;;
  cloud-plan)    cloud_plan ;;
  cloud-destroy) cloud_destroy ;;
  *) _die "Usage: $0 {cloud-deploy|cloud-plan|cloud-destroy}" ;;
esac
