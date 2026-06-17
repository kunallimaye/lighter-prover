#!/usr/bin/env bash
set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
source "${SCRIPT_DIR}/common.sh"

# ─── Configuration Resolvers ──────────────────────────────────────────

_generate_tfvars() {
  local config_path="${CONFIG_TOML:-config.toml}"
  local target_vms="infra-as-code/terraform/vms.auto.tfvars.json"
  local target_sa="infra-as-code/terraform/target.auto.tfvars.json"

  if [[ -f "${config_path}" ]]; then
    python3 infra-as-code/scripts/parse_config.py "${config_path}" vms > "${target_vms}"
    python3 infra-as-code/scripts/parse_config.py "${config_path}" target > "${target_sa}"
  else
    _die "Configuration file ${config_path} not found. Create it from config.toml.example first."
  fi
}

_resolve_build_project() {
  _generate_tfvars
  local target_sa="infra-as-code/terraform/target.auto.tfvars.json"
  local project=""
  if [[ -f "${target_sa}" ]]; then
    project="$(python3 -c "import json; print(json.load(open('${target_sa}')).get('build_project_id', ''))" 2>/dev/null || true)"
  fi
  if [[ -z "${project}" ]]; then
    project="$(gcloud config get-value project 2>/dev/null || true)"
  fi
  if [[ -z "${project}" ]]; then
    _die "Unable to detect GCP project ID. Set [gcp.defaults].project in config.toml."
  fi
  echo "${project}"
}

_build_substitutions() {
  local build_project="$1"
  local action="$2"
  local builder_sa="$3"
  local runtime_sa="$4"

  local target_sa="infra-as-code/terraform/target.auto.tfvars.json"
  local cfg_bucket="" cfg_prefix="" cfg_repo="" cfg_region=""
  if [[ -f "${target_sa}" ]]; then
    cfg_bucket="$(python3 -c "import json; print(json.load(open('${target_sa}')).get('tf_state_bucket', ''))" 2>/dev/null || true)"
    cfg_prefix="$(python3 -c "import json; print(json.load(open('${target_sa}')).get('tf_state_prefix', ''))" 2>/dev/null || true)"
    cfg_repo="$(python3 -c "import json; print(json.load(open('${target_sa}')).get('ar_repo', ''))" 2>/dev/null || true)"
    cfg_region="$(python3 -c "import json; print(json.load(open('${target_sa}')).get('region', ''))" 2>/dev/null || true)"
  fi

  local bucket="${TF_STATE_BUCKET:-${cfg_bucket:-${build_project}-tfstate}}"
  local prefix="${TF_STATE_PREFIX:-${cfg_prefix:-lighter-prover-iac}}"
  local region="${GCP_REGION:-${cfg_region:-us-central1}}"
  local ar_repo="${AR_REPO:-${cfg_repo:-lighter-prover-iac}}"
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

# ─── Preflight & IAM Bootstrapping ────────────────────────────────────

_verify_service_accounts_and_auth() {
  local build_project="${1:-$(_resolve_build_project)}"
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

  local builder_sa
  builder_sa="$(python3 -c "import json; print(json.load(open('${target_sa}')).get('builder_sa_email', ''))" 2>/dev/null || true)"
  if [[ -n "${builder_sa}" ]]; then
    _log_info "Testing local caller access (actAs) on Build SA: ${builder_sa}..."
    if ! gcloud auth print-access-token --impersonate-service-account="${builder_sa}" &>/dev/null; then
      local current_caller member_prefix="user"
      current_caller="$(gcloud config get-value account 2>/dev/null || echo '<OPERATOR_EMAIL>')"
      if [[ "${current_caller}" == *".gserviceaccount.com"* ]]; then
        member_prefix="serviceAccount"
      fi

      printf '\n\033[1;31m[ERROR]\033[0m Active local caller identity (%s) lacks permission to act as Build SA %s.\n' "${current_caller}" "${builder_sa}" >&2
      printf 'Ask the cloud administrator to run the following EXACT gcloud commands:\n\n' >&2
      printf '  gcloud iam service-accounts add-iam-policy-binding %s \\\n' "${builder_sa}" >&2
      printf '    --project="%s" \\\n' "${build_project}" >&2
      printf '    --member="%s:%s" \\\n' "${member_prefix}" "${current_caller}" >&2
      printf '    --role="roles/iam.serviceAccountUser"\n\n' >&2
      printf '  gcloud iam service-accounts add-iam-policy-binding %s \\\n' "${builder_sa}" >&2
      printf '    --project="%s" \\\n' "${build_project}" >&2
      printf '    --member="%s:%s" \\\n' "${member_prefix}" "${current_caller}" >&2
      printf '    --role="roles/iam.serviceAccountTokenCreator"\n\n' >&2
      exit 1
    fi
    _log_ok "  Impersonation access confirmed."
  fi
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
    local n=0
    until gcloud projects add-iam-policy-binding "${project}" \
      --member="serviceAccount:${sa_email}" \
      --role="${role}" \
      --condition=None \
      --quiet >/dev/null 2>&1; do
      n=$((n + 1))
      if [[ $n -ge 6 ]]; then
        _die "Failed to bind role ${role} after 6 retries (GCP IAM eventual consistency timeout)."
      fi
      _log_info "    Waiting for GCP IAM replication ($n/6)..."
      sleep 5
    done
  done < <(python3 -c "import json; [print(r) for r in json.load(open('${json_file}')).get('target_sas', {}).get('${sa_key}', {}).get('roles', [])]" 2>/dev/null || true)
}

_teardown_sa_and_roles() {
  local project="$1"
  local sa_email="$2"
  local sa_key="$3"
  local json_file="$4"

  if [[ -z "${sa_email}" ]]; then return 0; fi

  _log_info "Tearing down SA identity (${sa_key}) and role allocations: ${sa_email}..."

  local role
  while IFS= read -r role; do
    if [[ -z "${role}" ]]; then continue; fi
    _log_info "  Revoking IAM role '${role}' from ${sa_email}..."
    gcloud projects remove-iam-policy-binding "${project}" \
      --member="serviceAccount:${sa_email}" \
      --role="${role}" \
      --condition=None \
      --quiet >/dev/null 2>&1 || true
  done < <(python3 -c "import json; [print(r) for r in json.load(open('${json_file}')).get('target_sas', {}).get('${sa_key}', {}).get('roles', [])]" 2>/dev/null || true)

  if gcloud iam service-accounts describe "${sa_email}" --project="${project}" &>/dev/null; then
    _log_info "  Deleting Service Account ${sa_email}..."
    gcloud iam service-accounts delete "${sa_email}" --project="${project}" --quiet
    _log_ok "  Deleted ${sa_email}"
  else
    _log_ok "  SA identity already removed."
  fi
}

# ─── IaC Submission Plane ─────────────────────────────────────────────

_execute_cloudbuild() {
  local action="$1"
  local build_project
  build_project="$(_resolve_build_project)"

  _verify_service_accounts_and_auth "${build_project}"

  local target_sa="infra-as-code/terraform/target.auto.tfvars.json"
  local builder_sa runtime_sa build_machine
  builder_sa="$(python3 -c "import json; print(json.load(open('${target_sa}')).get('builder_sa_email', ''))" 2>/dev/null || true)"
  runtime_sa="$(python3 -c "import json; print(json.load(open('${target_sa}')).get('runtime_sa_email', ''))" 2>/dev/null || true)"
  build_machine="$(python3 -c "import json; print(json.load(open('${target_sa}')).get('build_machine_type', 'UNSPECIFIED'))" 2>/dev/null || true)"

  _log_info "Submitting IaC pipeline to Cloud Build (action: ${action})..."
  _log_info "  Build Project: ${build_project}"
  _log_info "  Builder SA:    ${builder_sa}"
  _log_info "  Runtime SA:    ${runtime_sa}"
  _log_info "  Machine Type:  ${build_machine}"

  local substitutions
  substitutions="$(_build_substitutions "${build_project}" "${action}" "${builder_sa}" "${runtime_sa}")"

  local cb_args=()
  if [[ -n "${builder_sa}" ]]; then
    cb_args+=(--service-account="projects/${build_project}/serviceAccounts/${builder_sa}")
  fi
  if [[ -n "${build_machine}" && "${build_machine}" != "UNSPECIFIED" ]]; then
    cb_args+=(--machine-type="${build_machine}")
  fi

  gcloud builds submit . \
    --project="${build_project}" \
    "${cb_args[@]}" \
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

  local target_json="infra-as-code/terraform/target.auto.tfvars.json"

  local state_bucket region
  state_bucket="$(python3 -c "import json; print(json.load(open('${target_json}')).get('tf_state_bucket', '${build_project}-tfstate'))" 2>/dev/null || true)"
  region="${GCP_REGION:-us-central1}"

  if [[ -n "${state_bucket}" ]]; then
    _log_info "Verifying GCS Terraform state bucket: gs://${state_bucket}..."
    if ! gcloud storage buckets describe "gs://${state_bucket}" &>/dev/null; then
      _log_info "  Bucket gs://${state_bucket} not found, creating in ${region}..."
      gcloud storage buckets create "gs://${state_bucket}" \
        --project="${build_project}" \
        --location="${region}" \
        --uniform-bucket-level-access \
        --quiet
      _log_ok "  Created gs://${state_bucket}"
    else
      _log_ok "  GCS state bucket gs://${state_bucket} already exists."
    fi
  fi

  local sa_key sa_email
  while IFS= read -r sa_key; do
    if [[ -z "${sa_key}" ]]; then continue; fi
    sa_email="$(python3 -c "import json; print(json.load(open('${target_json}')).get('target_sas', {}).get('${sa_key}', {}).get('email', ''))" 2>/dev/null || true)"
    if [[ -z "${sa_email}" ]]; then continue; fi

    _provision_sa_and_roles "${build_project}" "${sa_email}" "${sa_key}" "${target_json}"
  done < <(python3 -c "import json; [print(k) for k in json.load(open('${target_json}')).get('target_sas', {}).keys()]" 2>/dev/null || true)

  local builder_sa
  builder_sa="$(python3 -c "import json; print(json.load(open('${target_json}')).get('builder_sa_email', ''))" 2>/dev/null || true)"

  _log_ok "All target Service Accounts and IAM role allocations successfully provisioned."

  if [[ -n "${builder_sa}" ]]; then
    printf '\n\033[1;33m[OWNER ACTION REQUIRED]\033[0m To allow operators or CI identities to execute cloud deployment targets,\n'
    printf 'grant impersonation permissions on Build SA (%s) by running:\n\n' "${builder_sa}"
    printf '  gcloud iam service-accounts add-iam-policy-binding %s \\\n' "${builder_sa}"
    printf '    --project="%s" \\\n' "${build_project}"
    printf '    --member="user:<OPERATOR_EMAIL>" \\\n'
    printf '    --role="roles/iam.serviceAccountUser"\n\n'
    printf '  gcloud iam service-accounts add-iam-policy-binding %s \\\n' "${builder_sa}"
    printf '    --project="%s" \\\n' "${build_project}"
    printf '    --member="user:<OPERATOR_EMAIL>" \\\n'
    printf '    --role="roles/iam.serviceAccountTokenCreator"\n\n'
  fi
}

cloud_admin_undo() {
  local build_project
  build_project="$(_resolve_build_project)"

  _log_info "Tearing down target GCP Service Accounts & IAM roles (cloud-admin-undo)..."
  _log_info "  Target Project: ${build_project}"

  local target_json="infra-as-code/terraform/target.auto.tfvars.json"

  local sa_key sa_email
  while IFS= read -r sa_key; do
    if [[ -z "${sa_key}" ]]; then continue; fi
    sa_email="$(python3 -c "import json; print(json.load(open('${target_json}')).get('target_sas', {}).get('${sa_key}', {}).get('email', ''))" 2>/dev/null || true)"
    if [[ -z "${sa_email}" ]]; then continue; fi

    _teardown_sa_and_roles "${build_project}" "${sa_email}" "${sa_key}" "${target_json}"
  done < <(python3 -c "import json; [print(k) for k in json.load(open('${target_json}')).get('target_sas', {}).keys()]" 2>/dev/null || true)

  _log_ok "All target Service Accounts and IAM role allocations successfully torn down."
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

cloud_zkp_build() {
  local arch="${1:-arm64}"

  if [[ "${arch}" == "all" ]]; then
    _log_info "Submitting Cloud Build pipelines for both ARM64 and AMD64 images..."
    cloud_zkp_build arm64
    cloud_zkp_build amd64
    return
  fi

  local build_project
  build_project="$(_resolve_build_project)"

  _generate_tfvars
  _verify_service_accounts_and_auth "${build_project}"

  local target_sa="infra-as-code/terraform/target.auto.tfvars.json"
  local builder_sa build_machine cfg_repo="" cfg_region="" cfg_ar_region=""
  builder_sa="$(python3 -c "import json; print(json.load(open('${target_sa}')).get('builder_sa_email', ''))" 2>/dev/null || true)"
  build_machine="$(python3 -c "import json; print(json.load(open('${target_sa}')).get('build_machine_type', 'UNSPECIFIED'))" 2>/dev/null || true)"
  cfg_repo="$(python3 -c "import json; print(json.load(open('${target_sa}', encoding='utf-8')).get('ar_repo', ''))" 2>/dev/null || true)"
  cfg_region="$(python3 -c "import json; print(json.load(open('${target_sa}', encoding='utf-8')).get('region', ''))" 2>/dev/null || true)"
  cfg_ar_region="$(python3 -c "import json; print(json.load(open('${target_sa}', encoding='utf-8')).get('ar_region', ''))" 2>/dev/null || true)"

  local region="${GCP_REGION:-${cfg_region:-us-central1}}"
  local ar_region="${cfg_ar_region:-${region}}"
  local ar_repo="${AR_REPO:-${cfg_repo:-lighter-prover-iac}}"

  local dockerfile="Dockerfile.zkp-arm64"
  local image_tag="arm64"
  if [[ "${arch}" == "amd64" ]]; then
    dockerfile="Dockerfile.zkp"
    image_tag="amd64"
  fi

  local image_uri="${ar_region}-docker.pkg.dev/${build_project}/${ar_repo}/zkp-prover:${image_tag}"

  _log_info "Submitting isolated ZKP container image build (${arch}) to Cloud Build..."
  _log_info "  Build Project: ${build_project}"
  _log_info "  Builder SA:    ${builder_sa}"
  _log_info "  Target Image:  ${image_uri}"
  _log_info "  Dockerfile:    ${dockerfile}"
  _log_info "  Machine Type:  ${build_machine}"

  local cb_args=()
  if [[ -n "${builder_sa}" ]]; then
    cb_args+=(--service-account="projects/${build_project}/serviceAccounts/${builder_sa}")
  fi
  if [[ -n "${build_machine}" && "${build_machine}" != "UNSPECIFIED" ]]; then
    cb_args+=(--machine-type="${build_machine}")
  fi

  gcloud builds submit . \
    --project="${build_project}" \
    "${cb_args[@]}" \
    --config="infra-as-code/cloudbuild-zkp.yaml" \
    --substitutions="_IMAGE_URI=${image_uri},_DOCKERFILE=${dockerfile}" \
    --quiet

  _log_ok "ZKP container image built and pushed successfully to ${image_uri}."
}

cloud_bench_run() {
  local target_vm="${1:-all}"
  local jobs="${2:-1}"
  local build_project
  build_project="$(_resolve_build_project)"

  _generate_tfvars

  local target_vms="infra-as-code/terraform/vms.auto.tfvars.json"
  local target_sa="infra-as-code/terraform/target.auto.tfvars.json"

  if [[ "${target_vm}" == "all" || -z "${target_vm}" ]]; then
    _log_info "Executing benchmark proving container across ALL provisioned instances (jobs=${jobs})..."
    local vm_list=()
    while IFS= read -r v; do
      [[ -n "$v" ]] && vm_list+=("$v")
    done < <(python3 -c "import json; print('\n'.join(json.load(open('${target_vms}')).get('vms', {}).keys()))" 2>/dev/null || true)

    for vm in "${vm_list[@]}"; do
      cloud_bench_run "${vm}" "${jobs}" &
    done
    wait
    return
  fi

  local zone cfg_repo="" cfg_region="" cfg_ar_region="" bench_bucket="" bench_template=""
  zone="$(python3 -c "import json; print(json.load(open('${target_vms}')).get('vms', {}).get('${target_vm}', {}).get('zone', 'us-central1-a'))" 2>/dev/null || true)"
  cfg_repo="$(python3 -c "import json; print(json.load(open('${target_sa}')).get('ar_repo', 'lighter-prover-iac'))" 2>/dev/null || true)"
  cfg_region="$(python3 -c "import json; print(json.load(open('${target_sa}')).get('region', 'us-central1'))" 2>/dev/null || true)"
  cfg_ar_region="$(python3 -c "import json; print(json.load(open('${target_sa}')).get('ar_region', 'us'))" 2>/dev/null || true)"
  bench_bucket="$(python3 -c "import json; print(json.load(open('${target_sa}')).get('bench_bucket', ''))" 2>/dev/null || true)"
  bench_template="$(python3 -c "import json; print(json.load(open('${target_sa}')).get('bench_path_template', 'benchmark-reports/{machine_type}/{instance_id}/{timestamp}'))" 2>/dev/null || true)"

  local ar_region="${cfg_ar_region:-${cfg_region}}"
  local ar_repo="${AR_REPO:-${cfg_repo}}"
  local image_uri="${ar_region}-docker.pkg.dev/${build_project}/${ar_repo}/zkp-prover:arm64"

  _log_info "Executing remote ZKP proving benchmark on instance '${target_vm}' (${zone}, jobs=${jobs})..."
  _log_info "  Target VM:      ${target_vm} (${zone})"
  _log_info "  Container:      ${image_uri}"
  _log_info "  Concurrency:    ${jobs} simultaneous job(s)"
  _log_info "  Prioritization: nice -n -20, --pids-limit=-1"

  gcloud compute ssh "${target_vm}" --zone="${zone}" --project="${build_project}" --command="
    set -euo pipefail
    if ! command -v docker >/dev/null 2>&1; then
      sudo apt-get update && sudo apt-get install -y docker.io
    fi
    sudo gcloud auth configure-docker ${ar_region}-docker.pkg.dev --quiet
    sudo rm -rf /tmp/reports && mkdir -p /tmp/reports

    # Note: CFS vs. hard core pinning via --cpuset-cpus is a critical performance knob/option worth trialing in future benchmarking.
    if [[ ${jobs} -eq 1 ]]; then
      sudo nice -n -20 docker run --rm \
        --pids-limit=-1 \
        --ulimit nofile=1048576:1048576 \
        -v /tmp/reports:/data/reports:rw \
        ${image_uri}
    else
      threads_per_job=\$(( \$(nproc) / ${jobs} ))
      for j in \$(seq 1 ${jobs}); do
        mkdir -p /tmp/reports/job_\${j}
        sudo nice -n -20 docker run --rm \
          --pids-limit=-1 \
          --ulimit nofile=1048576:1048576 \
          -e RAYON_NUM_THREADS=\${threads_per_job} \
          -v /tmp/reports/job_\${j}:/data/reports:rw \
          ${image_uri} &
      done
      wait
    fi

    if [[ -n '${bench_bucket}' ]]; then
      machine_type=\$(curl -s -H 'Metadata-Flavor: Google' http://metadata.google.internal/computeMetadata/v1/instance/machine-type | awk -F/ '{print \$NF}')
      instance_id=\$(curl -s -H 'Metadata-Flavor: Google' http://metadata.google.internal/computeMetadata/v1/instance/id)
      ts=\$(date +%Y%m%d-%H%M%S)
      dest=\$(echo '${bench_template}' | sed -e \"s/{machine_type}/\$machine_type/g\" -e \"s/{instance_id}/\$instance_id/g\" -e \"s/{timestamp}/\$ts/g\" -e \"s/{build_id}/\$ts/g\")
      gsutil cp -r /tmp/reports/* \"gs://${bench_bucket}/\$dest/\"
    fi
  "

  _log_ok "Remote benchmark completed successfully on '${target_vm}'."
}

cloud_vm_start() {
  local target_vm="${1:-all}"
  local build_project
  build_project="$(_resolve_build_project)"

  _generate_tfvars

  local target_vms="infra-as-code/terraform/vms.auto.tfvars.json"

  if [[ "${target_vm}" == "all" || -z "${target_vm}" ]]; then
    _log_info "Starting ALL provisioned VM instances defined in config.toml..."
    local vm_list=()
    while IFS= read -r v; do
      [[ -n "$v" ]] && vm_list+=("$v")
    done < <(python3 -c "import json; print('\n'.join(json.load(open('${target_vms}')).get('vms', {}).keys()))" 2>/dev/null || true)

    for vm in "${vm_list[@]}"; do
      cloud_vm_start "${vm}"
    done
    return
  fi

  local zone
  zone="$(python3 -c "import json; print(json.load(open('${target_vms}')).get('vms', {}).get('${target_vm}', {}).get('zone', 'us-central1-a'))" 2>/dev/null || true)"

  _log_info "Starting GCE VM instance '${target_vm}' (${zone})..."
  gcloud compute instances start "${target_vm}" --zone="${zone}" --project="${build_project}" --quiet || true
  _log_ok "Instance '${target_vm}' start signal issued."
}

cloud_vm_stop() {
  local target_vm="${1:-all}"
  local build_project
  build_project="$(_resolve_build_project)"

  _generate_tfvars

  local target_vms="infra-as-code/terraform/vms.auto.tfvars.json"

  if [[ "${target_vm}" == "all" || -z "${target_vm}" ]]; then
    _log_info "Stopping ALL provisioned VM instances defined in config.toml..."
    local vm_list=()
    while IFS= read -r v; do
      [[ -n "$v" ]] && vm_list+=("$v")
    done < <(python3 -c "import json; print('\n'.join(json.load(open('${target_vms}')).get('vms', {}).keys()))" 2>/dev/null || true)

    for vm in "${vm_list[@]}"; do
      cloud_vm_stop "${vm}"
    done
    return
  fi

  local zone
  zone="$(python3 -c "import json; print(json.load(open('${target_vms}')).get('vms', {}).get('${target_vm}', {}).get('zone', 'us-central1-a'))" 2>/dev/null || true)"

  _log_info "Stopping GCE VM instance '${target_vm}' (${zone})..."
  gcloud compute instances stop "${target_vm}" --zone="${zone}" --project="${build_project}" --quiet || true
  _log_ok "Instance '${target_vm}' stop signal issued."
}

# ─── Main Dispatch ────────────────────────────────────────────────────

case "${1:-}" in
  cloud-admin-init) cloud_admin_init ;;
  cloud-admin-undo) cloud_admin_undo ;;
  cloud-bench-run)  shift; cloud_bench_run "${1:-all}" ;;
  cloud-deploy)     cloud_deploy ;;
  cloud-plan)       cloud_plan ;;
  cloud-destroy)    cloud_destroy ;;
  cloud-vm-start)   shift; cloud_vm_start "${1:-all}" ;;
  cloud-vm-stop)    shift; cloud_vm_stop "${1:-all}" ;;
  cloud-zkp-build)  shift; cloud_zkp_build "${1:-arm64}" ;;
  *) _die "Usage: $0 {cloud-admin-init|cloud-admin-undo|cloud-bench-run [vm|all]|cloud-deploy|cloud-plan|cloud-destroy|cloud-vm-start [vm|all]|cloud-vm-stop [vm|all]|cloud-zkp-build [arm64|amd64|all]}" ;;
esac
