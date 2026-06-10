#!/usr/bin/env bash
# Cloud runtime operations — three-role topology (orchestration / build / runtime)
#
# Operator interface is the Makefile; this script is the implementation.
# Never invoke this script directly — go through 'make <target>' so logging,
# trap handlers, and the heartbeat/checkpoint machinery engage.
#
# Verbs (role-aware vocabulary, #141 lesson 6):
#
#   help                — print resolved three-role topology
#   admin-cloud-init    — Owner-tier 8-step bootstrap (run as Owner once)
#   admin-cloud-destroy — symmetric teardown (preserves TF state + AR by default)
#   cloud-preflight     — read-only audit (per-role-aware messaging)
#   cloud-infra         — TF apply via Cloud Build (builder SA in build project)
#   cloud-app-deploy    — image build + Cloud Run revision swap
#   cloud-app-promote   — semver tag + deploy to non-staging runtime (VERSION + IMAGE required)
#   cloud-app-undeploy  — revert Cloud Run to placeholder image
#   cloud-clean         — TF destroy (runtime resources only)
#   cloud-status        — read heartbeat: RUNNING | STALLED | COMPLETE | NEVER_STARTED
#   cloud-recover       — read EXIT/HUP recovery file, complete teardown
#
# Operator escape hatch:
#   ORCH_FORCE_RESTART=1 — invalidates stepwise checkpoint, restarts from step 1.
#                          Step idempotency is a contract; restart-from-1 is safe.

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
# shellcheck disable=SC1091
source "${SCRIPT_DIR}/common.sh"
start_log "cloud-${1:-unknown}"

# Build Cloud Build substitutions from config. Carries the three-role
# topology to Terraform via TF_VAR_* env (see cloudbuild-apply.yaml).
#
# Substitution set is intentionally trimmed to match the Phase 1
# Terraform module (cicd/terraform/main.tf): AR repo + IAM. Variables
# the upstream scaffold passed for runtime resources (Cloud Run,
# LB/DNS) were removed when the module was trimmed. Re-add them when
# Phase 2 reintroduces the runtime resources.
_tf_substitutions() {
  local subs="_REGION=${RUNTIME_REGION:-${GCP_REGION:-us-central1}}"
  [[ -n "${TF_STATE_BUCKET}" ]] && subs="${subs},_TF_STATE_BUCKET=${TF_STATE_BUCKET}"
  [[ -n "${TF_STATE_PREFIX}" ]] && subs="${subs},_TF_STATE_PREFIX=${TF_STATE_PREFIX}"
  subs="${subs},_ORCH_PROJECT_ID=${ORCH_PROJECT}"
  subs="${subs},_BUILD_PROJECT_ID=${BUILD_PROJECT}"
  subs="${subs},_RUNTIME_PROJECT_ID=${RUNTIME_PROJECT}"
  subs="${subs},_BUILDER_SA_EMAIL=${BUILDER_SA_EMAIL}"
  subs="${subs},_RUNTIME_SA_EMAIL=${RUNTIME_SA_EMAIL:-}"
  subs="${subs},_AR_REPO=${AR_REPO}"
  echo "${subs}"
}

# Resolve the caller's gcloud quota project once and cache it in
# CALLER_PROJECT. Used by _grant_role to decide whether a grant is
# cross-project (caller's quota project ≠ target).
#
# Why this matters: the --billing-project flag routes the API call's
# quota+billing to the target project, which is required when the
# caller's quota project differs from the target (otherwise an org
# policy can reject with "no billing project"). The previous
# implementation compared against ORCH_PROJECT, which only matched
# the caller's project by coincidence in fully-collapsed topologies.
# In split-orch topologies (or any case where `gcloud config` points
# at a project other than ORCH_PROJECT), the branch misfired —
# missing the flag for genuine cross-project grants, or adding it
# unnecessarily for local grants.
_resolve_caller_project() {
  if [[ -z "${CALLER_PROJECT:-}" ]]; then
    CALLER_PROJECT="$(gcloud config get-value project 2>/dev/null || true)"
    if [[ -z "${CALLER_PROJECT}" ]]; then
      die "Unable to detect caller's gcloud project. Run 'gcloud config set project <id>' first."
    fi
    log_info "  caller project (gcloud quota): ${CALLER_PROJECT}"
  fi
}

# Cross-project IAM grant helper. Each grant in admin-cloud-init may target
# the local project (caller's quota project) or a different one (cross-project).
# The behavior is uniform — branch on caller-vs-target for the operator
# warning message and the --billing-project flag.
_grant_role() {
  local target_project="$1" member="$2" role="$3"
  _resolve_caller_project
  local extra_flag=""
  if [[ "${target_project}" != "${CALLER_PROJECT}" ]]; then
    extra_flag="--billing-project=${target_project}"
    log_info "  cross-project grant: ${role} on ${target_project} for ${member}"
  fi
  # shellcheck disable=SC2086
  gcloud projects add-iam-policy-binding "${target_project}" \
    ${extra_flag} \
    --member="${member}" \
    --role="${role}" \
    --condition=None \
    --quiet
}

_require_topology() {
  [[ -z "${ORCH_PROJECT}" ]]    && die "ORCH_PROJECT not set. Fill [gcp.defaults].project or [gcp.orchestration].project in config.toml."
  [[ -z "${BUILD_PROJECT}" ]]   && die "BUILD_PROJECT not set. Fill [gcp.defaults].project or [gcp.build].project in config.toml."
  [[ -z "${RUNTIME_PROJECT}" ]] && die "RUNTIME_PROJECT not set. Fill [gcp.defaults].project or [gcp.runtime].project in config.toml."
}

# ─── help ─────────────────────────────────────────────────────────────

help_cmd() {
  print_topology
  echo ""
  echo "Operator interface: see 'make help' (the Makefile is the operator surface."
  echo "                    Never invoke scripts/cloud.sh directly.)"
  echo ""
  echo "Escape hatch: ORCH_FORCE_RESTART=1 invalidates the stepwise checkpoint"
  echo "              and restarts the run from step 1. Always safe (step"
  echo "              idempotency is a contract)."
}

# ─── admin-cloud-init ─────────────────────────────────────────────────
# Owner-tier 8-step bootstrap. Runs in the orchestration project as Owner.
# Cross-project-aware: each grant branches on local-vs-cross-project.
# Stepwise checkpointed via run_detached_stepwise — re-run resumes; the
# step-list hash invalidates stale checkpoints automatically (#141 lesson 3).
# All steps are idempotent.

_step_enable_apis() {
  local apis=(
    "serviceusage.googleapis.com"
    "iam.googleapis.com"
    "cloudresourcemanager.googleapis.com"
    "cloudbuild.googleapis.com"
    "artifactregistry.googleapis.com"
    "run.googleapis.com"
    "storage.googleapis.com"
    "logging.googleapis.com"
  )
  # API enable is per-project (each role's project that owns resources).
  # Build, runtime, AND orchestration need APIs — orchestration needs
  # iam.googleapis.com + cloudresourcemanager.googleapis.com so the
  # agent SA management and custom-role creation steps work.
  # We enable the full set on each distinct project for simplicity —
  # idempotent and cheap. Dedup by value across all three roles.
  local projects=("${BUILD_PROJECT}")
  same_project BUILD_PROJECT RUNTIME_PROJECT || projects+=("${RUNTIME_PROJECT}")
  if ! same_project ORCH_PROJECT BUILD_PROJECT \
      && ! same_project ORCH_PROJECT RUNTIME_PROJECT; then
    projects+=("${ORCH_PROJECT}")
  fi
  for p in "${projects[@]}"; do
    log_info "  enabling APIs on ${p}..."
    for api in "${apis[@]}"; do
      gcloud services enable "${api}" --project="${p}" --quiet
    done
  done
}

_step_create_ar_repo() {
  if gcloud artifacts repositories describe "${AR_REPO}" \
      --location="${BUILD_REGION:-${GCP_REGION}}" \
      --project="${BUILD_PROJECT}" &>/dev/null; then
    log_ok "  AR repo already exists: ${AR_REPO} in ${BUILD_PROJECT}"
    return 0
  fi
  gcloud artifacts repositories create "${AR_REPO}" \
    --repository-format=docker \
    --location="${BUILD_REGION:-${GCP_REGION}}" \
    --description="Container images for ${PROJECT_NAME}" \
    --project="${BUILD_PROJECT}" \
    --quiet
}

_step_create_tfstate_bucket() {
  [[ -z "${TF_STATE_BUCKET}" ]] && die "TF_STATE_BUCKET not set."
  if gcloud storage buckets describe "gs://${TF_STATE_BUCKET}" --project="${BUILD_PROJECT}" &>/dev/null; then
    log_ok "  TF state bucket already exists: gs://${TF_STATE_BUCKET}"
    return 0
  fi
  gcloud storage buckets create "gs://${TF_STATE_BUCKET}" \
    --project="${BUILD_PROJECT}" \
    --location="${BUILD_REGION:-${GCP_REGION}}" \
    --uniform-bucket-level-access \
    --quiet
}

_step_create_builder_sa() {
  if gcloud iam service-accounts describe "${BUILDER_SA_EMAIL}" --project="${BUILD_PROJECT}" &>/dev/null; then
    log_ok "  Builder SA already exists: ${BUILDER_SA_EMAIL}"
    return 0
  fi
  gcloud iam service-accounts create "${BUILDER_SA_NAME}" \
    --display-name="${PROJECT_NAME} Builder (Cloud Build)" \
    --project="${BUILD_PROJECT}" \
    --quiet
}

_step_create_custom_role() {
  [[ -f "${DEPLOYER_ROLE_YAML}" ]] || die "Custom role YAML missing: ${DEPLOYER_ROLE_YAML}. Re-run /scaffold to generate it."
  # Custom role lives on the orchestration project (where the agent identity lives).
  if gcloud iam roles describe "${DEPLOYER_ROLE_ID}" --project="${ORCH_PROJECT}" &>/dev/null; then
    log_info "  Custom role exists — updating from YAML..."
    gcloud iam roles update "${DEPLOYER_ROLE_ID}" \
      --project="${ORCH_PROJECT}" \
      --file="${DEPLOYER_ROLE_YAML}" \
      --quiet
  else
    log_info "  Creating custom role from YAML..."
    gcloud iam roles create "${DEPLOYER_ROLE_ID}" \
      --project="${ORCH_PROJECT}" \
      --file="${DEPLOYER_ROLE_YAML}" \
      --quiet
  fi
}

_step_create_agent_sa_and_bind() {
  # Create agent SA (operator identity) in orchestration project.
  if ! gcloud iam service-accounts describe "${AGENT_SA_EMAIL}" --project="${ORCH_PROJECT}" &>/dev/null; then
    gcloud iam service-accounts create "${AGENT_SA_NAME}" \
      --display-name="${PROJECT_NAME} Agent (operator identity)" \
      --project="${ORCH_PROJECT}" \
      --quiet
  else
    log_ok "  Agent SA already exists: ${AGENT_SA_EMAIL}"
  fi

  # Bind agent SA to custom role on orchestration project with 30-day expiry.
  # Expiry condition forces graceful credential rotation (re-run admin-cloud-init).
  #
  # Guard against empty AGENT_ROLE_EXPIRY_DAYS: BSD date (macOS) silently
  # emits 'now' when the relative offset is empty, which would bind the
  # role with an already-expired condition. common.sh sets a default of 30,
  # but be explicit about the failure mode if it ever gets cleared.
  [[ -n "${AGENT_ROLE_EXPIRY_DAYS}" ]] || die "AGENT_ROLE_EXPIRY_DAYS is empty. Set it in config.toml ([gcp.orchestration].agent_role_expiry_days) or .env, or accept the 30-day default in common.sh."
  local expiry_ts
  expiry_ts="$(date -u -d "+${AGENT_ROLE_EXPIRY_DAYS} days" '+%Y-%m-%dT%H:%M:%SZ' 2>/dev/null \
    || date -u -v+${AGENT_ROLE_EXPIRY_DAYS}d '+%Y-%m-%dT%H:%M:%SZ')"
  local condition_title="agent-role-expiry-${AGENT_ROLE_EXPIRY_DAYS}d"
  log_info "  binding agent SA → custom role with expiry ${expiry_ts}"
  gcloud projects add-iam-policy-binding "${ORCH_PROJECT}" \
    --member="serviceAccount:${AGENT_SA_EMAIL}" \
    --role="projects/${ORCH_PROJECT}/roles/${DEPLOYER_ROLE_ID}" \
    --condition="expression=request.time < timestamp(\"${expiry_ts}\"),title=${condition_title},description=Auto-expires; re-run admin-cloud-init to refresh." \
    --quiet
}

_step_grant_agent_actas_builder() {
  # Agent SA needs iam.serviceAccountUser on the builder SA so it can
  # pass --service-account=<builder> to gcloud builds submit.
  gcloud iam service-accounts add-iam-policy-binding "${BUILDER_SA_EMAIL}" \
    --project="${BUILD_PROJECT}" \
    --member="serviceAccount:${AGENT_SA_EMAIL}" \
    --role="roles/iam.serviceAccountUser" \
    --condition=None \
    --quiet
}

_step_grant_builder_roles() {
  # The 6 predefined roles the builder SA holds. Scoped to what TF
  # actually needs to construct resources (NO projectIamAdmin, NO
  # serviceUsageAdmin — those would defeat the agent's least-privilege model).
  #
  # Each grant targets the runtime project (where TF builds things), with
  # one grant on the build project for storage admin (TF state + Cloud
  # Build staging bucket). Cross-project-aware: branches on local-vs-cross.
  local builder_roles_runtime=(
    "roles/run.admin"
    "roles/artifactregistry.admin"
    "roles/iam.serviceAccountUser"
    "roles/iam.serviceAccountAdmin"
    "roles/logging.logWriter"
  )
  local builder_member="serviceAccount:${BUILDER_SA_EMAIL}"

  log_info "  granting builder SA 5 functional roles on runtime project (${RUNTIME_PROJECT})"
  for role in "${builder_roles_runtime[@]}"; do
    _grant_role "${RUNTIME_PROJECT}" "${builder_member}" "${role}"
  done

  # Storage admin lives on the build project (TF state bucket + Cloud
  # Build staging bucket are both there).
  log_info "  granting builder SA storage.admin on build project (${BUILD_PROJECT})"
  _grant_role "${BUILD_PROJECT}" "${builder_member}" "roles/storage.admin"

  # DNS admin if a DNS project is configured. Cross-project-aware.
  if [[ -n "${DNS_PROJECT_ID}" ]]; then
    log_info "  granting builder SA dns.admin on DNS project (${DNS_PROJECT_ID})"
    _grant_role "${DNS_PROJECT_ID}" "${builder_member}" "roles/dns.admin"
  fi
}

# ─── Fleet bootstrap steps (bench-fleet toolkit, #33) ────────────────
# Appended to the admin-cloud-init step list. All idempotent. Note: the
# run_detached_stepwise checkpoint hash invalidates when the step list
# changes — by design; restart-from-1 is safe.
#
# Identities:
#   FLEET_ORCHESTRATOR_SA — the principal that runs the fleet toolkit
#       (gcloud builds submit, VM create/delete, GCS polling). Default
#       is the agent workstation's runtime SA; override via env or .env.
#   default Compute SA — what the fleet VMs run as (pull images, upload
#       results from inside the worker containers).
#
# NOT granted here: roles/cloudbuild.builds.editor — the #33 submit test
# proved the orchestrator SA can already submit builds to kunal-scratch.

: "${FLEET_ORCHESTRATOR_SA:=ai-workstation-runtime@kl-ai-workstation.iam.gserviceaccount.com}"

# Resolve the runtime project's default Compute SA email.
_fleet_compute_sa() {
  local pn
  pn="$(gcloud projects describe "${RUNTIME_PROJECT}" --format='value(projectNumber)')" \
    || die "could not resolve project number for ${RUNTIME_PROJECT}"
  printf '%s-compute@developer.gserviceaccount.com\n' "${pn}"
}

_step_fleet_create_results_bucket() {
  local bucket="${FLEET_RESULTS_BUCKET:-gs://${RUNTIME_PROJECT}-bench-fleet-runs}"
  if gcloud storage buckets describe "${bucket}" --project="${RUNTIME_PROJECT}" &>/dev/null; then
    log_ok "  fleet results bucket already exists: ${bucket}"
    return 0
  fi
  gcloud storage buckets create "${bucket}" \
    --project="${RUNTIME_PROJECT}" \
    --location="${RUNTIME_REGION:-${GCP_REGION}}" \
    --uniform-bucket-level-access \
    --quiet
}

_step_fleet_grant_orchestrator() {
  local bucket="${FLEET_RESULTS_BUCKET:-gs://${RUNTIME_PROJECT}-bench-fleet-runs}"
  local member="serviceAccount:${FLEET_ORCHESTRATOR_SA}"
  # actAs the Compute SA so `gcloud compute instances create
  # --service-account=<compute-sa>` works. Project-level grant for
  # simplicity (single-purpose scratch project).
  log_info "  granting ${FLEET_ORCHESTRATOR_SA} iam.serviceAccountUser on ${RUNTIME_PROJECT}"
  _grant_role "${RUNTIME_PROJECT}" "${member}" "roles/iam.serviceAccountUser"
  # Bucket-scoped storage admin: write probes, artifact collection,
  # sentinel polling, cleanup.
  log_info "  granting ${FLEET_ORCHESTRATOR_SA} storage.admin on ${bucket}"
  gcloud storage buckets add-iam-policy-binding "${bucket}" \
    --project="${RUNTIME_PROJECT}" \
    --member="${member}" \
    --role="roles/storage.admin" \
    --quiet
}

_step_fleet_grant_compute_sa() {
  local bucket="${FLEET_RESULTS_BUCKET:-gs://${RUNTIME_PROJECT}-bench-fleet-runs}"
  local compute_sa
  compute_sa="$(_fleet_compute_sa)" || return 1
  local member="serviceAccount:${compute_sa}"
  # VM-side uploads (worker containers + fleet sentinels). Issue #23:
  # without this every upload 403s and the monitor never sees _DONE.
  log_info "  granting ${compute_sa} storage.objectAdmin on ${bucket}"
  gcloud storage buckets add-iam-policy-binding "${bucket}" \
    --project="${RUNTIME_PROJECT}" \
    --member="${member}" \
    --role="roles/storage.objectAdmin" \
    --quiet
  # Image pulls from Artifact Registry on the COS VMs (#33).
  log_info "  granting ${compute_sa} artifactregistry.reader on ${RUNTIME_PROJECT}"
  _grant_role "${RUNTIME_PROJECT}" "${member}" "roles/artifactregistry.reader"
}

admin_cloud_init() {
  log_info "Owner-tier bootstrap (${ENVIRONMENT})..."
  print_topology
  require_cmd gcloud
  _require_topology
  _resolve_caller_project
  [[ -z "${TF_STATE_BUCKET}" ]] && die "TF_STATE_BUCKET is not set."

  if [[ "${CONFIRM:-}" != "yes" ]]; then
    confirm "Proceed with 11-step bootstrap?" || { log_warn "Aborted."; exit 0; }
  fi

  run_detached_stepwise "admin-cloud-init" \
    _step_enable_apis \
    _step_create_ar_repo \
    _step_create_tfstate_bucket \
    _step_create_builder_sa \
    _step_create_custom_role \
    _step_create_agent_sa_and_bind \
    _step_grant_agent_actas_builder \
    _step_grant_builder_roles \
    _step_fleet_create_results_bucket \
    _step_fleet_grant_orchestrator \
    _step_fleet_grant_compute_sa

  log_ok "admin-cloud-init complete."
  log_info "Next: 'make cloud-preflight' to verify, then 'make cloud-infra' to provision runtime resources."
  log_info "Fleet: 'make fleet-quota-check' validates the bench-fleet bucket + IAM."
}

# ─── admin-cloud-destroy ──────────────────────────────────────────────
# Symmetric to admin-cloud-init. Preserves TF state bucket + AR repo by
# default (those are too destructive to remove without explicit confirm).
# Set DESTROY_STATE_BUCKET=yes / DESTROY_AR_REPO=yes to include them.

_step_destroy_grant_builder_roles() {
  local builder_roles_runtime=(
    "roles/run.admin"
    "roles/artifactregistry.admin"
    "roles/iam.serviceAccountUser"
    "roles/iam.serviceAccountAdmin"
    "roles/logging.logWriter"
  )
  local builder_member="serviceAccount:${BUILDER_SA_EMAIL}"
  for role in "${builder_roles_runtime[@]}"; do
    gcloud projects remove-iam-policy-binding "${RUNTIME_PROJECT}" \
      --member="${builder_member}" \
      --role="${role}" \
      --quiet 2>/dev/null || true
  done
  gcloud projects remove-iam-policy-binding "${BUILD_PROJECT}" \
    --member="${builder_member}" \
    --role="roles/storage.admin" \
    --quiet 2>/dev/null || true
}

_step_destroy_agent_actas_builder() {
  gcloud iam service-accounts remove-iam-policy-binding "${BUILDER_SA_EMAIL}" \
    --project="${BUILD_PROJECT}" \
    --member="serviceAccount:${AGENT_SA_EMAIL}" \
    --role="roles/iam.serviceAccountUser" \
    --quiet 2>/dev/null || true
}

_step_destroy_agent_sa() {
  gcloud iam service-accounts delete "${AGENT_SA_EMAIL}" --project="${ORCH_PROJECT}" --quiet 2>/dev/null || true
}

_step_destroy_custom_role() {
  gcloud iam roles delete "${DEPLOYER_ROLE_ID}" --project="${ORCH_PROJECT}" --quiet 2>/dev/null || true
}

_step_destroy_builder_sa() {
  gcloud iam service-accounts delete "${BUILDER_SA_EMAIL}" --project="${BUILD_PROJECT}" --quiet 2>/dev/null || true
}

_step_destroy_tfstate_bucket() {
  if [[ "${DESTROY_STATE_BUCKET:-no}" == "yes" ]]; then
    log_warn "  DESTROY_STATE_BUCKET=yes — removing gs://${TF_STATE_BUCKET}"
    gcloud storage rm -r "gs://${TF_STATE_BUCKET}" --project="${BUILD_PROJECT}" --quiet 2>/dev/null || true
  else
    log_info "  preserving TF state bucket gs://${TF_STATE_BUCKET} (set DESTROY_STATE_BUCKET=yes to remove)"
  fi
}

_step_destroy_ar_repo() {
  if [[ "${DESTROY_AR_REPO:-no}" == "yes" ]]; then
    log_warn "  DESTROY_AR_REPO=yes — removing AR repo ${AR_REPO}"
    gcloud artifacts repositories delete "${AR_REPO}" \
      --location="${BUILD_REGION:-${GCP_REGION}}" \
      --project="${BUILD_PROJECT}" --quiet 2>/dev/null || true
  else
    log_info "  preserving AR repo ${AR_REPO} (set DESTROY_AR_REPO=yes to remove)"
  fi
}

admin_cloud_destroy() {
  log_warn "Owner-tier teardown of bootstrap (${ENVIRONMENT})..."
  print_topology
  require_cmd gcloud
  _require_topology

  if [[ "${CONFIRM:-}" != "yes" ]]; then
    confirm "Proceed with destructive teardown of agent/builder SAs and IAM bindings?" \
      || { log_warn "Aborted."; exit 0; }
  fi

  run_detached_stepwise "admin-cloud-destroy" \
    _step_destroy_grant_builder_roles \
    _step_destroy_agent_actas_builder \
    _step_destroy_agent_sa \
    _step_destroy_custom_role \
    _step_destroy_builder_sa \
    _step_destroy_tfstate_bucket \
    _step_destroy_ar_repo

  log_ok "admin-cloud-destroy complete."
}

# ─── cloud-preflight ──────────────────────────────────────────────────
# Read-only audit. Verifies bootstrap state across all three roles with
# per-role-aware messaging. Never mutates.

cloud_preflight() {
  log_info "Cloud preflight audit (read-only)..."
  print_topology
  require_cmd gcloud
  _require_topology

  local errors=0
  local checks=0

  # APIs (sample one critical API per project where resources live)
  for p in "${BUILD_PROJECT}" "${RUNTIME_PROJECT}"; do
    checks=$((checks + 1))
    if gcloud services list --enabled --project="${p}" --filter="config.name=run.googleapis.com" --format="value(config.name)" 2>/dev/null | grep -q "run.googleapis.com"; then
      log_ok "  APIs enabled on ${p} (run.googleapis.com sentinel ✓)"
    else
      log_error "  APIs not enabled on ${p} — run 'make admin-cloud-init'"
      errors=$((errors + 1))
    fi
  done

  # AR repo exists in build project
  checks=$((checks + 1))
  if gcloud artifacts repositories describe "${AR_REPO}" \
      --location="${BUILD_REGION:-${GCP_REGION}}" \
      --project="${BUILD_PROJECT}" &>/dev/null; then
    log_ok "  AR repo exists: ${AR_REPO} in ${BUILD_PROJECT}"
  else
    log_error "  AR repo missing: ${AR_REPO} in ${BUILD_PROJECT}"
    errors=$((errors + 1))
  fi

  # TF state bucket exists in build project
  checks=$((checks + 1))
  if [[ -n "${TF_STATE_BUCKET}" ]] && gcloud storage buckets describe "gs://${TF_STATE_BUCKET}" --project="${BUILD_PROJECT}" &>/dev/null; then
    log_ok "  TF state bucket exists: gs://${TF_STATE_BUCKET} in ${BUILD_PROJECT}"
  else
    log_error "  TF state bucket missing: gs://${TF_STATE_BUCKET:-<unset>}"
    errors=$((errors + 1))
  fi

  # Builder SA exists in build project
  checks=$((checks + 1))
  if gcloud iam service-accounts describe "${BUILDER_SA_EMAIL}" --project="${BUILD_PROJECT}" &>/dev/null; then
    log_ok "  Builder SA exists: ${BUILDER_SA_EMAIL}"
  else
    log_error "  Builder SA missing: ${BUILDER_SA_EMAIL}"
    errors=$((errors + 1))
  fi

  # Builder SA has expected 5 roles on runtime project
  checks=$((checks + 1))
  local builder_member="serviceAccount:${BUILDER_SA_EMAIL}"
  local policy
  policy="$(gcloud projects get-iam-policy "${RUNTIME_PROJECT}" --format=json 2>/dev/null || echo '{}')"
  local missing_roles=()
  for role in "roles/run.admin" "roles/artifactregistry.admin" "roles/iam.serviceAccountUser" "roles/iam.serviceAccountAdmin" "roles/logging.logWriter"; do
    # grep for the quoted role string in the JSON IAM policy.
    if ! grep -q "\"${role}\"" <<<"${policy}"; then
      missing_roles+=("${role}")
    fi
  done
  if (( ${#missing_roles[@]} == 0 )); then
    log_ok "  Builder SA has all 5 functional roles on runtime project (${RUNTIME_PROJECT})"
  else
    log_error "  Builder SA missing roles on ${RUNTIME_PROJECT}: ${missing_roles[*]}"
    errors=$((errors + 1))
  fi

  # Agent SA exists in orchestration project
  checks=$((checks + 1))
  if gcloud iam service-accounts describe "${AGENT_SA_EMAIL}" --project="${ORCH_PROJECT}" &>/dev/null; then
    log_ok "  Agent SA exists: ${AGENT_SA_EMAIL}"
  else
    log_warn "  Agent SA missing: ${AGENT_SA_EMAIL} (run 'make admin-cloud-init')"
    errors=$((errors + 1))
  fi

  # Custom role exists on orchestration project
  checks=$((checks + 1))
  if gcloud iam roles describe "${DEPLOYER_ROLE_ID}" --project="${ORCH_PROJECT}" &>/dev/null; then
    log_ok "  Custom role exists: ${DEPLOYER_ROLE_ID} in ${ORCH_PROJECT}"
  else
    log_warn "  Custom role missing: ${DEPLOYER_ROLE_ID} in ${ORCH_PROJECT}"
    errors=$((errors + 1))
  fi

  echo ""
  if (( errors == 0 )); then
    log_ok "Preflight passed: ${checks}/${checks} checks OK."
    return 0
  fi
  log_error "Preflight failed: ${errors}/${checks} checks failed."
  return 1
}

# ─── cloud-infra ──────────────────────────────────────────────────────
# TF apply via Cloud Build. Builder SA runs in the build project and
# provisions runtime-project resources (Cloud Run, runtime SA, LB/DNS).

cloud_infra() {
  log_info "Provisioning runtime infrastructure (${ENVIRONMENT})..."
  require_cmd gcloud
  _require_topology
  [[ -z "${TF_STATE_BUCKET}" ]] && die "TF_STATE_BUCKET not set."

  gcloud builds submit "${PROJECT_ROOT}" \
    --project="${BUILD_PROJECT}" \
    --service-account="projects/${BUILD_PROJECT}/serviceAccounts/${BUILDER_SA_EMAIL}" \
    --config="${PROJECT_ROOT}/cicd/cloudbuild-apply.yaml" \
    --substitutions="_TF_ACTION=apply,$(_tf_substitutions)" \
    --quiet

  log_ok "Infrastructure ready (${ENVIRONMENT})"
}

# ─── cloud-app-deploy / promote / undeploy ────────────────────────────
#
# Phase 1 (#2) intentionally does NOT ship a long-lived Cloud Run
# service. Bench is one-shot — invoked via `gcloud run jobs execute`,
# not via a Cloud Run revision swap. The verbs below are kept as stubs
# pointing at the Phase 1 image-build verb (cloud-bench-build) and the
# Cloud Run Jobs README section, so an operator who reaches for the
# generic scaffold vocabulary gets a clear redirect.
#
# These come back to life in Phase 2 (#3) when work-sharding introduces
# a coordinator service that benefits from a long-lived Cloud Run
# deployment.

cloud_app_deploy() {
  cat >&2 <<'MSG'
[INFO] cloud-app-deploy is a Phase 2 verb. Phase 1 (#2) ships the bench
       container only; there is no long-lived Cloud Run service to swap.

       To build + push the bench image:
         make cloud-bench-build [LIGHTER_REF=<sha>]

       To run the bench as a one-shot in GCP (see README for full flow):
         gcloud run jobs create lighter-bench-worker --image=<image>:<tag> ...
         gcloud run jobs execute lighter-bench-worker --region=<region> --wait
MSG
  die "cloud-app-deploy is deferred to Phase 2."
}

cloud_app_promote() {
  cat >&2 <<'MSG'
[INFO] cloud-app-promote is a Phase 2 verb. Phase 1 (#2) doesn't run a
       long-lived Cloud Run service, so there's no environment promotion
       semantics yet. Tag the bench image directly with gcloud:
         gcloud artifacts docker tags add <src-image>:sha-<sha> <src-image>:vX.Y.Z
MSG
  die "cloud-app-promote is deferred to Phase 2."
}

cloud_app_undeploy() {
  cat >&2 <<'MSG'
[INFO] cloud-app-undeploy is a Phase 2 verb. Phase 1 (#2) doesn't run a
       long-lived Cloud Run service, so there's nothing to revert. To
       remove the AR repo + IAM Phase 1 provisioned:
         make cloud-clean
MSG
  die "cloud-app-undeploy is deferred to Phase 2."
}

# ─── cloud-clean ──────────────────────────────────────────────────────
# TF destroy. Removes runtime resources (Cloud Run, runtime SA, LB/DNS).
# Does NOT remove bootstrap state (use admin-cloud-destroy for that).

cloud_clean() {
  log_warn "Tearing down runtime infrastructure (${ENVIRONMENT})..."
  require_cmd gcloud
  _require_topology

  if [[ "${CONFIRM:-}" != "yes" ]]; then
    confirm "Destroy runtime infrastructure for ${ENVIRONMENT}?" \
      || { log_warn "Aborted."; exit 0; }
  fi

  gcloud builds submit "${PROJECT_ROOT}" \
    --project="${BUILD_PROJECT}" \
    --service-account="projects/${BUILD_PROJECT}/serviceAccounts/${BUILDER_SA_EMAIL}" \
    --config="${PROJECT_ROOT}/cicd/cloudbuild-apply.yaml" \
    --substitutions="_TF_ACTION=destroy,$(_tf_substitutions)" \
    --quiet

  log_ok "Runtime infrastructure destroyed."
}

# ─── cloud-status / cloud-recover ─────────────────────────────────────

cloud_status() {
  # Show heartbeat for the most-likely active actions. cloud-status is
  # diagnostic; it never mutates.
  for action in admin-cloud-init admin-cloud-destroy cloud-infra cloud-clean cloud-bench-build; do
    printf '  %-22s %s\n' "${action}" "$(heartbeat_status "${action}")"
  done
}

cloud_recover() {
  echo "Recovery state for all known actions:"
  for action in admin-cloud-init admin-cloud-destroy cloud-infra cloud-clean cloud-bench-build; do
    echo ""
    echo "[${action}]"
    recovery_summary "${action}"
  done
}

# ─── cloud-bench-build ────────────────────────────────────────────────
# Build the bench container matrix via Cloud Build (#33): one submit
# produces the portable multi-arch manifest (:<sha> + :latest) plus the
# three per-microarch variants (:<sha>-znver5, :<sha>-neoverse-v2,
# :<sha>-neoverse-n1) that the bench-fleet VMs pull. All arm64 binaries
# are CROSS-COMPILED on the x86 worker — see cicd/cloudbuild.yaml.
#
# Note: LIGHTER_REF is derived from $COMMIT_SHA so the value baked into
# the image truthfully matches the source COPY'd into the builder stage.
# The same full SHA is the image tag the fleet resolves against
# (machines.tsv image_tag column appends the microarch suffix).
#
# Required cloud topology (from config.toml/.env):
#   BUILD_PROJECT, BUILD_REGION, AR_REPO.
# Optional: BUILDER_SA_EMAIL — used only when the SA actually exists
# (collapsed scratch topologies submit as the caller's default identity).

cloud_bench_build() {
  log_info "Cloud Build: bench container matrix (#33)..."
  require_cmd gcloud
  _require_topology

  local commit_sha
  commit_sha="$(git -C "${PROJECT_ROOT}" rev-parse HEAD 2>/dev/null || echo manual)"
  local image_name="${BUILD_REGION:-us-central1}-docker.pkg.dev/${BUILD_PROJECT}/${AR_REPO}/bench"

  log_info "  image:        ${image_name}"
  log_info "  tags:         :latest :${commit_sha} :${commit_sha}-{znver5,neoverse-v2,neoverse-n1}"
  log_info "  commit:       ${commit_sha}"

  # Use the dedicated builder SA when it exists; otherwise fall back to
  # the project's default Cloud Build identity (fine for collapsed
  # personal topologies like kunal-scratch).
  local sa_flag=()
  if gcloud iam service-accounts describe "${BUILDER_SA_EMAIL}" \
      --project="${BUILD_PROJECT}" &>/dev/null; then
    sa_flag=(--service-account="projects/${BUILD_PROJECT}/serviceAccounts/${BUILDER_SA_EMAIL}")
    log_info "  builder SA:   ${BUILDER_SA_EMAIL}"
  else
    log_warn "  builder SA ${BUILDER_SA_EMAIL} not found — submitting with default Cloud Build identity"
  fi

  # `gcloud builds submit` from a local source directory does NOT
  # auto-populate $COMMIT_SHA (only git triggers do), so pass it
  # explicitly via substitutions.
  gcloud builds submit "${PROJECT_ROOT}" \
    --project="${BUILD_PROJECT}" \
    "${sa_flag[@]}" \
    --config="${PROJECT_ROOT}/cicd/cloudbuild.yaml" \
    --substitutions="_IMAGE_NAME=${image_name},COMMIT_SHA=${commit_sha}" \
    --quiet

  log_ok "Bench image matrix built and pushed: ${image_name}:${commit_sha}{,-znver5,-neoverse-v2,-neoverse-n1}"
}

# Compatibility: legacy verbs from the pre-#141 scaffold. Stub out with a
# helpful message rather than silently breaking downstream Makefiles that
# someone forgot to update. This is the only back-compat — config and TF
# are clean breaks.
_legacy_stub() {
  local old="$1" new="$2"
  die "Verb '${old}' has been removed. Use 'make ${new}' (three-role topology, #141)."
}

# ─── Dispatch ─────────────────────────────────────────────────────────

case "${1:-}" in
  help|cloud-help)        help_cmd ;;
  admin-cloud-init)       admin_cloud_init ;;
  admin-cloud-destroy)    admin_cloud_destroy ;;
  cloud-preflight)        cloud_preflight ;;
  cloud-infra)            cloud_infra ;;
  cloud-app-deploy)       cloud_app_deploy ;;
  cloud-bench-build)      cloud_bench_build ;;
  cloud-app-promote)      cloud_app_promote ;;
  cloud-app-undeploy)     cloud_app_undeploy ;;
  cloud-clean)            cloud_clean ;;
  cloud-status)           cloud_status ;;
  cloud-recover)          cloud_recover ;;
  # Removed legacy verbs (#141 — breaking change). Stub with a clear redirect.
  init)         _legacy_stub init admin-cloud-init ;;
  init-prod)    _legacy_stub init-prod admin-cloud-init ;;
  infra)        _legacy_stub infra cloud-infra ;;
  app-deploy)   _legacy_stub app-deploy cloud-app-deploy ;;
  app-promote)  _legacy_stub app-promote cloud-app-promote ;;
  app-undeploy) _legacy_stub app-undeploy cloud-app-undeploy ;;
  clean)        _legacy_stub clean cloud-clean ;;
   *) die "Usage: $0 {help|admin-cloud-init|admin-cloud-destroy|cloud-preflight|cloud-infra|cloud-app-deploy|cloud-bench-build|cloud-app-promote|cloud-app-undeploy|cloud-clean|cloud-status|cloud-recover}" ;;
esac
