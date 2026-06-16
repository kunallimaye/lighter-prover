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
  local builder_sa="$3"
  local runtime_sa="$4"

  local bucket="${TF_STATE_BUCKET:-${build_project}-tfstate}"
  local prefix="${TF_STATE_PREFIX:-lighter-prover}"
  local region="${GCP_REGION:-us-central1}"
  local ar_repo="${AR_REPO:-lighter-prover}"
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
  subs="${subs},_RUNTIME_SA_EMAIL=${runtime_sa}"

  echo "${subs}"
}

# ─── Core IaC Execution & Preflight ───────────────────────────────────

_generate_tfvars() {
  local config_path="${CONFIG_TOML:-config.toml}"
  local target_vms="infra-as-code/terraform/vms.auto.tfvars.json"
  local target_sa="infra-as-code/terraform/target.auto.tfvars.json"

  _log_info "Parsing configurations from ${config_path}..."
  if [[ -f "${config_path}" ]]; then
    python3 infra-as-code/scripts/parse_config.py "${config_path}" vms > "${target_vms}"
    python3 infra-as-code/scripts/parse_config.py "${config_path}" target > "${target_sa}"
    _log_info "  Generated ${target_vms}"
    _log_info "  Generated ${target_sa}"
  else
    _die "Configuration file ${config_path} not found. Cannot resolve [gcp.target] service accounts."
  fi
}

_verify_service_accounts() {
  local target_sa="infra-as-code/terraform/target.auto.tfvars.json"
  if [[ ! -f "${target_sa}" ]]; then
    _die "Service account configuration file missing."
  fi

  local build_sa
  build_sa="$(python3 -c "import json; print(json.load(open('${target_sa}')).get('builder_sa_email', ''))" 2>/dev/null || true)"
  local runtime_sa
  runtime_sa="$(python3 -c "import json; print(json.load(open('${target_sa}')).get('runtime_sa_email', ''))" 2>/dev/null || true)"

  if [[ -z "${build_sa}" ]]; then
    _die "Build SA (build_sa) not specified in ${CONFIG_TOML:-config.toml} [gcp.target]."
  fi
  if [[ -z "${runtime_sa}" ]]; then
    _die "Runtime SA (runtime_sa) not specified in ${CONFIG_TOML:-config.toml} [gcp.target]."
  fi

  _log_info "Verifying Build SA existence: ${build_sa}..."
  if ! gcloud iam service-accounts describe "${build_sa}" &>/dev/null; then
    _die "Build SA ${build_sa} does not exist or is not accessible. Contract violation: job failed."
  fi
  _log_ok "  Build SA verified."

  _log_info "Verifying Runtime SA existence: ${runtime_sa}..."
  if ! gcloud iam service-accounts describe "${runtime_sa}" &>/dev/null; then
    _die "Runtime SA ${runtime_sa} does not exist or is not accessible. Contract violation: job failed."
  fi
  _log_ok "  Runtime SA verified."
}

_execute_cloudbuild() {
  local action="$1"
  local build_project
  build_project="$(_resolve_build_project)"

  _generate_tfvars
  _verify_service_accounts

  local target_sa="infra-as-code/terraform/target.auto.tfvars.json"
  local builder_sa runtime_sa
  builder_sa="$(python3 -c "import json; print(json.load(open('${target_sa}')).get('builder_sa_email', ''))" 2>/dev/null || true)"
  runtime_sa="$(python3 -c "import json; print(json.load(open('${target_sa}')).get('runtime_sa_email', ''))" 2>/dev/null || true)"

  _log_info "Submitting IaC pipeline to Cloud Build (action: ${action})..."
  _log_info "  Build Project: ${build_project}"
  _log_info "  Builder SA:    ${builder_sa}"
  _log_info "  Runtime SA:    ${runtime_sa}"

  local substitutions
  substitutions="$(_build_substitutions "${build_project}" "${action}" "${builder_sa}" "${runtime_sa}")"

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
