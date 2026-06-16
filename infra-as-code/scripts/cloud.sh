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

  local sa_key sa_email
  while IFS= read -r sa_key; do
    if [[ -z "${sa_key}" ]]; then continue; fi
    sa_email="$(python3 -c "import json; print(json.load(open('${target_sa}')).get('target_sas', {}).get('${sa_key}', {}).get('email', ''))" 2>/dev/null || true)"
    if [[ -z "${sa_email}" ]]; then continue; fi

    _log_info "Preflight verification for SA (${sa_key}): ${sa_email}..."
    if ! gcloud iam service-accounts describe "${sa_email}" &>/dev/null; then
      _die "Service Account ${sa_email} (${sa_key}) does not exist or is not accessible. Contract violation: job failed."
    fi
    _log_ok "  ${sa_email} verified."
  done < <(python3 -c "import json; [print(k) for k in json.load(open('${target_sa}')).get('target_sas', {}).keys()]" 2>/dev/null || true)
}

_provision_sa_and_roles() {
  local project="$1"
  local sa_email="$2"
  local sa_key="$3"
  local json_file="$4"

  if [[ -z "${sa_email}" ]]; then return 0; fi

  local sa_name="${sa_email%%@*}"

  _log_info "Checking SA identity (${sa_key}): ${sa_email}..."
  if ! gcloud iam service-accounts describe "${sa_email}" --project="${project}" &>/dev/null; then
    _log_info "  SA not found, creating '${sa_name}'..."
    gcloud iam service-accounts create "${sa_name}" \
      --project="${project}" \
      --display-name="Managed Lighter SA (${sa_name})" \
      --quiet
    _log_ok "  Created ${sa_email}"
  else
    _log_ok "  SA identity already exists."
  fi

  local role
  while IFS= read -r role; do
    if [[ -z "${role}" ]]; then continue; fi
    _log_info "  Allocating IAM role '${role}' to ${sa_email}..."
    gcloud projects add-iam-policy-binding "${project}" \
      --member="serviceAccount:${sa_email}" \
      --role="${role}" \
      --condition=None \
      --quiet >/dev/null
  done < <(python3 -c "import json; [print(r) for r in json.load(open('${json_file}')).get('target_sas', {}).get('${sa_key}', {}).get('roles', [])]" 2>/dev/null || true)
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

cloud_admin_init() {
  local build_project
  build_project="$(_resolve_build_project)"

  _log_info "Bootstrapping arbitrary GCP target Service Accounts & IAM roles (cloud-admin-init)..."
  _log_info "  Target Project: ${build_project}"

  local config_path="${CONFIG_TOML:-config.toml}"
  if [[ ! -f "${config_path}" ]]; then
    _die "Configuration file ${config_path} missing. Create it from config.toml.example first."
  fi

  _generate_tfvars
  local target_json="infra-as-code/terraform/target.auto.tfvars.json"

  local sa_key sa_email
  while IFS= read -r sa_key; do
    if [[ -z "${sa_key}" ]]; then continue; fi
    sa_email="$(python3 -c "import json; print(json.load(open('${target_json}')).get('target_sas', {}).get('${sa_key}', {}).get('email', ''))" 2>/dev/null || true)"
    if [[ -z "${sa_email}" ]]; then continue; fi

    _provision_sa_and_roles "${build_project}" "${sa_email}" "${sa_key}" "${target_json}"
  done < <(python3 -c "import json; [print(k) for k in json.load(open('${target_json}')).get('target_sas', {}).keys()]" 2>/dev/null || true)

  _log_ok "All target Service Accounts and IAM role allocations successfully provisioned."
}

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
  cloud-admin-init) cloud_admin_init ;;
  cloud-deploy)     cloud_deploy ;;
  cloud-plan)       cloud_plan ;;
  cloud-destroy)    cloud_destroy ;;
  *) _die "Usage: $0 {cloud-admin-init|cloud-deploy|cloud-plan|cloud-destroy}" ;;
esac
