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
  if [[ ! -f "infra-as-code/terraform/vms.auto.tfvars.json" || ! -f "infra-as-code/terraform/target.auto.tfvars.json" ]]; then
    _generate_tfvars
  fi
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
  _generate_tfvars
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
    _log_info "Submitting concurrent Cloud Build pipelines for ARM64 and AMD64 images in parallel..."
    cloud_zkp_build arm64 &
    local p1=$!
    cloud_zkp_build amd64 &
    local p2=$!
    wait "$p1" "$p2"
    _log_ok "Both ARM64 and AMD64 ZKP container images built and pushed successfully in parallel."
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
  local platform="linux/arm64"
  if [[ "${arch}" == "amd64" ]]; then
    dockerfile="Dockerfile.zkp"
    image_tag="amd64"
    platform="linux/amd64"
  fi

  local git_tag="${TAG:-$(git describe --tags --exact-match 2>/dev/null || git describe --tags --always 2>/dev/null || echo "latest")}"
  local image_uri="${ar_region}-docker.pkg.dev/${build_project}/${ar_repo}/zkp-prover:${image_tag}"
  local extra_tag_uri="${ar_region}-docker.pkg.dev/${build_project}/${ar_repo}/zkp-prover:${git_tag}"

  _log_info "Submitting isolated ZKP container image build (${arch}) to Cloud Build..."
  _log_info "  Build Project: ${build_project}"
  _log_info "  Builder SA:    ${builder_sa}"
  _log_info "  Target Image:  ${image_uri} (and ${extra_tag_uri})"
  _log_info "  Dockerfile:    ${dockerfile}"
  _log_info "  Platform:      ${platform}"
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
    --substitutions="_IMAGE_URI=${image_uri},_EXTRA_TAG_URI=${extra_tag_uri},_DOCKERFILE=${dockerfile},_PLATFORM=${platform}" \
    --quiet

  _log_ok "ZKP container image built and pushed successfully to ${image_uri}."
}

cloud_bench_run() {
  local target_vm="${1:-all}"
  local jobs="${2:-1}"
  local tx_per_proof="${3:-4}"
  local image_arg="${4:-default}"
  local benchmark_id="${5:-}"
  local build_project
  build_project="$(_resolve_build_project)"

  if [[ ! -f "infra-as-code/terraform/vms.auto.tfvars.json" || ! -f "infra-as-code/terraform/target.auto.tfvars.json" ]]; then
    _generate_tfvars
  fi

  local target_vms="infra-as-code/terraform/vms.auto.tfvars.json"
  local target_sa="infra-as-code/terraform/target.auto.tfvars.json"

  if [[ "${target_vm}" == "all" || -z "${target_vm}" || "${target_vm}" == *" "* ]]; then
    local vm_list=()
    if [[ "${target_vm}" == "all" || -z "${target_vm}" ]]; then
      _log_info "Executing benchmark proving container across ALL provisioned instances (jobs=${jobs}, tx_per_proof=${tx_per_proof})..."
      while IFS= read -r v; do
        [[ -n "$v" ]] && vm_list+=("$v")
      done < <(python3 -c "import json; print('\n'.join(json.load(open('${target_vms}')).get('vms', {}).keys()))" 2>/dev/null || true)
    else
      _log_info "Executing benchmark proving container across specified instances (${target_vm}, jobs=${jobs}, tx_per_proof=${tx_per_proof})..."
      for v in ${target_vm}; do
        vm_list+=("$v")
      done
    fi

    for vm in "${vm_list[@]}"; do
      cloud_bench_run "${vm}" "${jobs}" "${tx_per_proof}" "${image_arg}" "${benchmark_id}" &
    done
    wait
    return
  fi

  local zone cfg_repo="" cfg_region="" cfg_ar_region="" bench_bucket="" bench_template=""
  zone="$(gcloud compute instances list --filter="name=${target_vm}" --format="value(zone)" 2>/dev/null | head -n 1)"
  if [[ -z "${zone}" ]]; then
    zone="$(python3 -c "import json; print(json.load(open('${target_vms}')).get('vms', {}).get('${target_vm}', {}).get('zone', 'us-central1-c'))" 2>/dev/null || true)"
  fi
  cfg_repo="$(python3 -c "import json; print(json.load(open('${target_sa}')).get('ar_repo', 'lighter-prover-iac'))" 2>/dev/null || true)"
  cfg_region="$(python3 -c "import json; print(json.load(open('${target_sa}')).get('region', 'us-central1'))" 2>/dev/null || true)"
  cfg_ar_region="$(python3 -c "import json; print(json.load(open('${target_sa}')).get('ar_region', 'us'))" 2>/dev/null || true)"
  bench_bucket="$(python3 -c "import json; print(json.load(open('${target_sa}')).get('bench_bucket', ''))" 2>/dev/null || true)"
  bench_template="$(python3 -c "import json; print(json.load(open('${target_sa}')).get('bench_path_template', 'benchmark-reports/{machine_type}/{instance_id}/{timestamp}'))" 2>/dev/null || true)"

  local ar_region="${cfg_ar_region:-${cfg_region}}"
  local ar_repo="${AR_REPO:-${cfg_repo}}"

  local vm_image vm_mtype image_tag="amd64"
  vm_image="$(python3 -c "import json; print(json.load(open('${target_vms}')).get('vms', {}).get('${target_vm}', {}).get('image', ''))" 2>/dev/null || true)"
  vm_mtype="$(python3 -c "import json; print(json.load(open('${target_vms}')).get('vms', {}).get('${target_vm}', {}).get('machine_type', ''))" 2>/dev/null || true)"
  if [[ "${vm_image}" == *"arm64"* || "${vm_mtype}" == *"a-"* || "${vm_mtype}" == *"t2a"* ]]; then
    image_tag="arm64"
  fi
  local image_uri="${ar_region}-docker.pkg.dev/${build_project}/${ar_repo}/zkp-prover:${image_tag}"
  if [[ "${image_arg}" != "default" && -n "${image_arg}" ]]; then
    if [[ "${image_arg}" != *"/"* ]]; then
      local tag_suffix="${image_arg#zkp-prover:}"
      image_uri="${ar_region}-docker.pkg.dev/${build_project}/${ar_repo}/zkp-prover:${tag_suffix}-${image_tag}"
    else
      image_uri="${image_arg}"
    fi
  fi

  _log_info "Executing remote ZKP proving benchmark on instance '${target_vm}' (${zone}, jobs=${jobs}, tx_per_proof=${tx_per_proof})..."
  _log_info "  Target VM:      ${target_vm} (${zone})"
  _log_info "  Container:      ${image_uri}"
  _log_info "  Concurrency:    ${jobs} simultaneous job(s)"
  _log_info "  Batch Size:     ${tx_per_proof} txs / proof"
  _log_info "  Prioritization: nice -n -20, --pids-limit=-1"

  if ! gcloud compute instances describe "${target_vm}" --zone="${zone}" --project="${build_project}" &>/dev/null; then
    _log_info "  [WARNING] Instance '${target_vm}' (${zone}) not found in project '${build_project}'. Skipping benchmark..."
    return 0
  fi

  _log_info "Ensuring VM instance '${target_vm}' (${zone}) is started before SSH connection..."
  gcloud compute instances start "${target_vm}" --zone="${zone}" --project="${build_project}" --quiet || true

  for _ in {1..15}; do
    if gcloud compute ssh "${target_vm}" --zone="${zone}" --project="${build_project}" --command="echo ready" --quiet 2>/dev/null; then
      break
    fi
    sleep 3
  done

  gcloud compute ssh "${target_vm}" --zone="${zone}" --project="${build_project}" --command="
    nohup bash -c '
      set -euo pipefail
      if ! command -v docker >/dev/null 2>&1; then
        sudo apt-get update && sudo apt-get install -y docker.io
      fi
      sudo gcloud auth configure-docker ${ar_region}-docker.pkg.dev --quiet
      sudo rm -rf /tmp/reports /tmp/bench.done && mkdir -p /tmp/reports

      if [[ ${jobs} -eq 1 ]]; then
        sudo nice -n -20 docker run --rm --pull=always \
          --pids-limit=-1 \
          --ulimit nofile=1048576:1048576 \
          -v /tmp/reports:/data/reports:rw \
          ${image_uri} --tx-per-proof ${tx_per_proof}
      else
        threads_per_job=\$(( \$(nproc) / ${jobs} ))
        pids=()
        for j in \$(seq 1 ${jobs}); do
          mkdir -p /tmp/reports/job_\${j}
          sudo nice -n -20 docker run --rm --pull=always \
            --pids-limit=-1 \
            --ulimit nofile=1048576:1048576 \
            -e RAYON_NUM_THREADS=\${threads_per_job} \
            -v /tmp/reports/job_\${j}:/data/reports:rw \
            ${image_uri} &
          pids+=(\$!)
        done
        for pid in \"\${pids[@]}\"; do
          wait \"\$pid\" || true
        done
      fi

      if [[ -n \"${bench_bucket}\" ]]; then
        machine_type=\$(curl -s -H \"Metadata-Flavor: Google\" http://metadata.google.internal/computeMetadata/v1/instance/machine-type | awk -F/ \"{print \\\$NF}\")
        instance_id=\$(curl -s -H \"Metadata-Flavor: Google\" http://metadata.google.internal/computeMetadata/v1/instance/id)
        ts=\$(date +%Y%m%d-%H%M%S)
        dest=\$(echo \"${bench_template}\" | sed -e \"s/{machine_type}/\$machine_type/g\" -e \"s/{instance_id}/\$instance_id/g\" -e \"s/{timestamp}/\$ts/g\" -e \"s/{build_id}/\$ts/g\")
        if [[ -n \"${benchmark_id}\" ]]; then
          dest=\"benchmark-reports/${benchmark_id}/\$machine_type/\$instance_id/\$ts\"
        fi
        gsutil cp -r /tmp/reports/* \"gs://${bench_bucket}/\$dest/\"
      fi
      touch /tmp/bench.done
    ' > /tmp/bench.log 2>&1 &
  " --quiet

  while ! gcloud compute ssh "${target_vm}" --zone="${zone}" --project="${build_project}" --command="[[ -f /tmp/bench.done ]]" --quiet 2>/dev/null; do
    sleep 10
  done

  _log_ok "Remote benchmark completed successfully on '${target_vm}'."
}

cloud_vm_start() {
  local target_vm="${1:-all}"
  local build_project
  build_project="$(_resolve_build_project)"

  _generate_tfvars

  local target_vms="infra-as-code/terraform/vms.auto.tfvars.json"

  if [[ "${target_vm}" == *" "* ]]; then
    local -a vm_list=(${target_vm})
    _log_info "Starting specified VM instances (${target_vm})..."
    for vm in "${vm_list[@]}"; do
      cloud_vm_start "${vm}"
    done
    return
  fi

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

  if [[ "${target_vm}" == *" "* ]]; then
    local -a vm_list=(${target_vm})
    _log_info "Stopping specified VM instances (${target_vm})..."
    for vm in "${vm_list[@]}"; do
      cloud_vm_stop "${vm}"
    done
    return
  fi

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

cloud_run_distributed_cluster() {
  local engine="${ENGINE:-gke}"
  local arch="${ARCH:-c3d}"
  local blocks="${BLOCKS:-2}"
  local chunk="${CHUNK:-1}"
  local image="${IMAGE:-default}"
  local benchmark_id="${BENCHMARK_ID:-}"
  local radix="${RADIX:-2}"
  for arg in "$@"; do
    case "$arg" in
      --engine=*) engine="${arg#*=}" ;;
      --arch=*)   arch="${arg#*=}" ;;
      --blocks=*) blocks="${arg#*=}" ;;
      --chunk=*)  chunk="${arg#*=}" ;;
      --image=*)  image="${arg#*=}" ;;
      --radix=*)  radix="${arg#*=}" ;;
      --benchmark-id=*) benchmark_id="${arg#*=}" ;;
      [0-9]*)     blocks="$arg" ;;
    esac
  done

  local build_project="$(_resolve_build_project)"
  if [[ ! -f "infra-as-code/terraform/vms.auto.tfvars.json" || ! -f "infra-as-code/terraform/target.auto.tfvars.json" ]]; then
    _generate_tfvars
  fi

  local target_sa="infra-as-code/terraform/target.auto.tfvars.json"
  local builder_sa="" runtime_sa="" build_machine=""
  if [[ -f "${target_sa}" ]]; then
    builder_sa="$(python3 -c "import json; print(json.load(open('${target_sa}')).get('builder_sa_email', ''))" 2>/dev/null || true)"
    runtime_sa="$(python3 -c "import json; print(json.load(open('${target_sa}')).get('runtime_sa_email', ''))" 2>/dev/null || true)"
    build_machine="$(python3 -c "import json; print(json.load(open('${target_sa}')).get('build_machine_type', 'E2_HIGHCPU_32'))" 2>/dev/null || true)"
  fi

  _log_info "Submitting unmocked distributed proving cycle to Cloud Build (engine=${engine}, arch=${arch}, blocks=${blocks}, chunk=${chunk}, radix=${radix})..."
  local substitutions
  substitutions="$(_build_substitutions "${build_project}" "apply" "${builder_sa}" "${runtime_sa}")"
  substitutions="${substitutions},_ENGINE=${engine},_ARCH=${arch},_BLOCK_CONCURRENCY=${blocks},_CHUNK_SIZE=${chunk},_IMAGE=${image},_BENCHMARK_ID=${benchmark_id},_RADIX=${radix}"

  local cb_args=()
  if [[ -n "${builder_sa}" ]]; then
    cb_args+=(--service-account="projects/${build_project}/serviceAccounts/${builder_sa}")
  fi
  if [[ -n "${build_machine}" && "${build_machine}" != "UNSPECIFIED" ]]; then
    cb_args+=(--machine-type="${build_machine}")
  fi

  gcloud builds submit "${ROOT_DIR}" \
    --project="${build_project}" \
    "${cb_args[@]}" \
    --config="infra-as-code/cloudbuild-distributed.yaml" \
    --substitutions="${substitutions}" \
    --quiet

  _log_ok "GCP Cloud Build declarative distributed proving cycle completed successfully!"
}

# ─── Honest benchmark stubs (issue #282) ─────────────────────────────────
#
# The functions below previously fabricated "empirical" metrics: they slept
# for fixed durations and then wrote hardcoded heredoc JSON/Markdown ledgers
# (fixed GKE wall times, fixed annual-savings figures, and a hardcoded capstone
# proving-time matrix). None of those numbers were measured. They have been
# replaced with fail-loud stubs. See reports/PROVENANCE.md for the full list of
# fabricated literals and the artifacts that were purged.
#
# Real numbers require a genuine distributed proving run on live GCP compute
# (#281 reduction-tree circuit correctness + #283 honest distributed
# prover-node, both now merged). Regenerating real, provenance-stamped reports
# from a live cloud run is a deliberate follow-up, NOT part of #282.
#
# To run a real distributed benchmark instead, use the honest verbs directly:
#   make cloud-run-distributed-cluster ENGINE=gke ARCH=c3d BLOCKS=2 CHUNK=1
#   make cloud-bench-run VM=<id> JOBS=<n> CHUNK=<n>
# These submit a real Cloud Build cycle and emit only measured telemetry.

_die_requires_live_run() {
  local name="$1"
  _die "${name}: not implemented. This benchmark previously fabricated hardcoded
       'empirical' metrics and has been removed (issue #282). Generating real
       numbers requires a live distributed proving run on GCP compute (#281 +
       #283, now merged). Run a genuine benchmark with the honest verbs instead:
         make cloud-run-distributed-cluster ENGINE=gke ARCH=c3d BLOCKS=2 CHUNK=1
         make cloud-bench-run VM=<id> JOBS=<n> CHUNK=<n>
       Regenerating provenance-stamped reports from a real run is a follow-up."
}

cloud_test_t2d_hypothesis() {
  _die_requires_live_run "cloud-test-t2d-hypothesis (Phase 4 t2d vs c4a AB race)"
}

cloud_test_gke_performance_tax() {
  _die_requires_live_run "cloud-test-gke-performance-tax (Phase 5 GKE overlay tax)"
}

cloud_test_capstone_matrix() {
  _die_requires_live_run "cloud-test-capstone-matrix (Phase 6 six-release capstone matrix)"
}

cloud_test_omni_silicon_parallel() {
  _die_requires_live_run "cloud-test-omni-silicon-parallel (4-block quad-silicon suite)"
}

# ─── Main Dispatch ────────────────────────────────────────────────────

case "${1:-}" in
  cloud-admin-init)              cloud_admin_init ;;
  cloud-admin-undo)              cloud_admin_undo ;;
  cloud-bench-run)               shift; cloud_bench_run "${1:-all}" "${2:-1}" "${3:-4}" "${4:-default}" "${5:-}" ;;
  cloud-run-distributed-cluster) cloud_run_distributed_cluster ;;
  cloud-test-t2d-hypothesis)     cloud_test_t2d_hypothesis ;;
  cloud-test-gke-performance-tax) cloud_test_gke_performance_tax ;;
  cloud-test-capstone-matrix)    cloud_test_capstone_matrix ;;
  cloud-test-omni-silicon-parallel) cloud_test_omni_silicon_parallel ;;
  cloud-deploy)                  cloud_deploy ;;
  cloud-plan)                    cloud_plan ;;
  cloud-destroy)                 cloud_destroy ;;
  cloud-vm-start)                shift; cloud_vm_start "${1:-all}" ;;
  cloud-vm-stop)                 shift; cloud_vm_stop "${1:-all}" ;;
  cloud-zkp-build)               shift; cloud_zkp_build "${1:-arm64}" ;;
  *) _die "Usage: $0 {cloud-admin-init|cloud-admin-undo|cloud-bench-run|cloud-run-distributed-cluster|cloud-test-t2d-hypothesis|cloud-test-gke-performance-tax|cloud-test-capstone-matrix|cloud-test-omni-silicon-parallel|cloud-deploy|cloud-plan|cloud-destroy|cloud-vm-start|cloud-vm-stop|cloud-zkp-build}" ;;
esac
