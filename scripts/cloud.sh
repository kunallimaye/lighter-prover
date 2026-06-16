#!/usr/bin/env bash
set -euo pipefail

# Cloud deployment helper script for the Makefile cloud-deploy target.

cloud_deploy() {
  echo "Deploying infrastructure via Cloud Build (infra-as-code/cloudbuild.yaml)..."

  local build_project
  build_project="$(gcloud config get-value project 2>/dev/null || true)"
  if [[ -z "${build_project}" ]]; then
    echo "ERROR: Unable to detect gcloud project. Run 'gcloud config set project <id>' first." >&2
    exit 1
  fi

  local bucket="${TF_STATE_BUCKET:-${build_project}-tfstate}"
  local prefix="${TF_STATE_PREFIX:-lighter-prover}"
  local region="${GCP_REGION:-us-central1}"
  local ar_repo="${AR_REPO:-lighter-prover}"
  local builder_sa_email="${BUILDER_SA_EMAIL:-lighter-builder@${build_project}.iam.gserviceaccount.com}"

  local subs="_REGION=${region},_TF_STATE_BUCKET=${bucket},_TF_STATE_PREFIX=${prefix}"
  subs="${subs},_BUILD_PROJECT_ID=${build_project}"
  subs="${subs},_ORCH_PROJECT_ID=${ORCH_PROJECT:-${build_project}}"
  subs="${subs},_RUNTIME_PROJECT_ID=${RUNTIME_PROJECT:-${build_project}}"
  subs="${subs},_AR_REPO=${ar_repo}"
  subs="${subs},_BUILDER_SA_EMAIL=${builder_sa_email}"
  subs="${subs},_RUNTIME_SA_EMAIL=${RUNTIME_SA_EMAIL:-}"

  gcloud builds submit . \
    --project="${build_project}" \
    --config="infra-as-code/cloudbuild.yaml" \
    --substitutions="_TF_ACTION=apply,${subs}" \
    --quiet

  echo "Infrastructure successfully deployed."
}

case "${1:-}" in
  cloud-deploy) cloud_deploy ;;
  *) echo "Usage: $0 cloud-deploy" >&2; exit 1 ;;
esac
