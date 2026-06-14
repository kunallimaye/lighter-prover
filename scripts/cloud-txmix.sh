#!/usr/bin/env bash
# cloud-txmix.sh — deploy + run the Tokyo tx-mix capture Cloud Run JOB (#128).
#
# This is the OPERATOR-FACING automation for the #128 tx-type-mix capture.
# It wraps the hardened `tx-mix` tool (bench/feeder/feeder.py, PR #152) as a
# parametrised Cloud Run JOB in asia-northeast1 (Tokyo) whose results land
# DURABLY in GCS. The SAME job runs a tiny SMOKE window and a big
# REPRESENTATIVE window purely by config — no redefinition.
#
# Subcommands:
#   build      Build + push the tx-mix image via Cloud Build.
#   deploy     Create/update the Cloud Run Job (parametrised by env).
#   smoke      Execute the job with a TINY window (validation; small N).
#   capture    Execute the job with the operator's REPRESENTATIVE window.
#   run        Execute the already-deployed job, --wait, then tail results.
#   results    Print the latest GCS artifact for a prefix (or newest).
#   post       Post a cited summary (from a GCS artifact) to issue #128.
#   all-smoke  build -> deploy -> smoke -> results  (end-to-end validation).
#
# WHY a JOB not a Service: the capture runs to completion and exits; a
# rate-limited representative window may take HOURS. Jobs support long task
# timeouts (TXMIX_TASK_TIMEOUT, default 24h) and run-to-completion.
#
# Egress: PUBLIC default egress (GCP-assigned Tokyo IP). NO Cloud NAT /
# static egress is configured — the maintainer's call is that public Cloud
# Run egress in asia-northeast1 presents a Tokyo IP that is normally NOT
# geo-blocked. If the smoke run 403s, that is a real egress FINDING; the
# tool hard-fails honestly and the fallback (Cloud NAT static IP, or the
# PR #152 VM recipe) is documented in bench/README.md.
#
# Honesty: a smoke window is a small-N VALIDATION sample, NOT the
# representative mainnet mix. The representative capture is THIS operator's
# `capture` run with a peak/off-peak window large enough to be
# representative (a human judgment).
set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
PROJECT_ROOT="$(cd "${SCRIPT_DIR}/.." && pwd)"

# Source common.sh for config.toml/.env resolution if present (sets
# PROJECT_NAME, BUILD_PROJECT, etc.). Best-effort: this script defaults
# everything so it also works on a bare checkout with no config.toml.
# shellcheck disable=SC1091
source "${SCRIPT_DIR}/common.sh" 2>/dev/null || true

# ── Resolve project / region / image / bucket (env > config > default) ──
PROJECT="${TXMIX_PROJECT:-${BUILD_PROJECT:-${GCP_PROJECT:-kunal-scratch}}}"
# Job region STAYS in Tokyo (asia-northeast1): the egress geo-block
# workaround depends on a Tokyo-presented IP. Do NOT change this.
REGION="${TXMIX_JOB_REGION:-asia-northeast1}"                 # Tokyo
AR_LOCATION="${TXMIX_AR_LOCATION:-asia-northeast1}"
AR_REPO_TXMIX="${TXMIX_AR_REPO:-lighter-prover-txmix}"
IMAGE="${TXMIX_IMAGE:-${AR_LOCATION}-docker.pkg.dev/${PROJECT}/${AR_REPO_TXMIX}/txmix}"
IMAGE_TAG="${TXMIX_IMAGE_TAG:-latest}"
JOB_NAME="${TXMIX_JOB_NAME:-lighter-txmix}"
# Durable output bucket. We REUSE the existing shared fleet results
# bucket (gs://<project>-bench-fleet-runs, us-central1, created by
# admin-cloud-init) rather than provisioning a new txmix-only bucket —
# the Tokyo job writes a tiny payload under a txmix/ prefix; a
# cross-region write to a us-central1 bucket is fine. The value flows
# from config.toml ([txmix].bucket) via config.py as a BARE name; env
# TXMIX_BUCKET overrides. Default-when-unset mirrors the fleet default.
BUCKET="${TXMIX_BUCKET:-${PROJECT}-bench-fleet-runs}"
# Belt-and-suspenders: strip a stray gs:// in case TXMIX_BUCKET was set
# by hand to a full URI (config.py already emits a bare name).
BUCKET="${BUCKET#gs://}"; BUCKET="${BUCKET%/}"
TASK_TIMEOUT="${TXMIX_TASK_TIMEOUT:-86400s}"                  # 24h — big windows
MAX_RETRIES_JOB="${TXMIX_JOB_MAX_RETRIES:-0}"                 # don't auto-rerun a capture
MEMORY="${TXMIX_MEMORY:-512Mi}"
CPU="${TXMIX_CPU:-1}"

# ── Identity model (single SA for control-plane + job runtime) ─────────
# The Cloud Run control-plane calls (create/update/execute/describe) run
# AS the project agent SA via IMPERSONATION; the active runtime identity
# holds roles/iam.serviceAccountTokenCreator + serviceAccountUser on it.
# The JOB itself ALSO runs as that agent SA (--service-account), whose
# project-level storage.objects.* lets the running job write to the
# fleet bucket. Both are configurable; default to the agent SA. An empty
# value disables impersonation (use the active gcloud account directly).
RUN_AS_SA="${TXMIX_RUN_AS_SA:-lighter-prover-agent@${PROJECT}.iam.gserviceaccount.com}"
IMPERSONATE_SA="${TXMIX_IMPERSONATE_SA:-${RUN_AS_SA}}"

# Capture knobs forwarded to the container (all overridable per execution).
MAX_RPM="${TXMIX_MAX_RPM:-80}"
PAGE_LIMIT="${TXMIX_PAGE_LIMIT:-100}"
CAP_REGION_LABEL="${TXMIX_REGION:-asia-northeast1}"

usage() {
  # Print the leading comment block (lines starting with '#'), stripping
  # the leading '# '. Stops at the first non-comment, non-blank line.
  awk 'NR>1 { if ($0 ~ /^#/) { sub(/^# ?/,""); print } else if ($0 != "") exit }' \
    "${BASH_SOURCE[0]}"
  cat <<EOF

Resolved configuration:
  project        ${PROJECT}
  job region     ${REGION}   (Tokyo — fixed; egress geo-block workaround)
  image          ${IMAGE}:${IMAGE_TAG}
  job name       ${JOB_NAME}
  output bucket  gs://${BUCKET}   (shared fleet bucket, txmix/ prefix)
  task timeout   ${TASK_TIMEOUT}
  run-as SA      ${RUN_AS_SA}
  impersonate    ${IMPERSONATE_SA:-<none — active gcloud account>}

Usage: $(basename "$0") <build|deploy|smoke|capture|run|results|post|all-smoke> [args]

Operator REPRESENTATIVE capture (the one command that closes the loop):
  TXMIX_HEIGHTS="<LO> <HI>" TXMIX_LABEL=peak $(basename "$0") capture
  # or by recent-block count:
  TXMIX_BLOCKS=5000 TXMIX_LABEL=offpeak $(basename "$0") capture
EOF
}

log() { echo -e "\033[0;34m[txmix]\033[0m $*"; }
die() { echo -e "\033[0;31m[txmix:ERROR]\033[0m $*" >&2; exit 1; }

# ── gcloud wrapper: impersonate the agent SA for control-plane calls ───
# Adds --impersonate-service-account ONLY when IMPERSONATE_SA is set
# (default: the agent SA). The active runtime identity has been granted
# serviceAccountTokenCreator + serviceAccountUser on it. Set
# TXMIX_IMPERSONATE_SA="" to run as the active gcloud account instead.
gcloud_imp() {
  if [[ -n "${IMPERSONATE_SA}" ]]; then
    gcloud --impersonate-service-account="${IMPERSONATE_SA}" "$@"
  else
    gcloud "$@"
  fi
}

# ── Verify the durable output bucket EXISTS (read-only; never creates) ──
# We REUSE the shared fleet results bucket; it is provisioned once by
# `make admin-cloud-init` (Owner-tier). This script must NOT attempt to
# create a bucket (no storage.buckets.create is required of the agent SA).
# A read-only describe gives a crisp error if the configured bucket is
# missing, instead of a cryptic write failure inside the Job hours later.
verify_bucket() {
  if gcloud_imp storage buckets describe "gs://${BUCKET}" --project "${PROJECT}" \
       >/dev/null 2>&1; then
    log "output bucket gs://${BUCKET} exists (reusing shared fleet bucket)"
    return 0
  fi
  die "output bucket gs://${BUCKET} not found.
  This script REUSES an existing bucket (the shared fleet results bucket);
  it does NOT create one. Provision it once (Owner-tier), e.g.:
    make admin-cloud-init        # creates gs://${PROJECT}-bench-fleet-runs
  Or set TXMIX_BUCKET / config.toml [txmix].bucket to an EXISTING bucket
  the run-as SA (${RUN_AS_SA}) can write objects to."
}

# ── Ensure the asia-northeast1 AR repo exists for the txmix image ──────
ensure_ar_repo() {
  if gcloud_imp artifacts repositories describe "${AR_REPO_TXMIX}" \
       --project "${PROJECT}" --location "${AR_LOCATION}" >/dev/null 2>&1; then
    log "AR repo ${AR_REPO_TXMIX} (${AR_LOCATION}) exists"
  else
    log "creating AR repo ${AR_REPO_TXMIX} in ${AR_LOCATION}"
    gcloud_imp artifacts repositories create "${AR_REPO_TXMIX}" \
      --project "${PROJECT}" --location "${AR_LOCATION}" \
      --repository-format=docker \
      --description="tx-mix capture image (issue #128)"
  fi
}

cmd_build() {
  ensure_ar_repo
  local sha
  sha="$(git -C "${PROJECT_ROOT}" rev-parse HEAD 2>/dev/null || echo manual)"
  log "building tx-mix image via Cloud Build (sha=${sha}) -> ${IMAGE}"
  gcloud_imp builds submit "${PROJECT_ROOT}" \
    --project "${PROJECT}" \
    --config "${PROJECT_ROOT}/cicd/cloudbuild-txmix.yaml" \
    --substitutions "COMMIT_SHA=${sha},_IMAGE_NAME=${IMAGE}"
  log "image pushed: ${IMAGE}:${sha} and :latest"
}

# Deploy (create or update) the parametrised Cloud Run JOB. Per-execution
# env overrides (window/label) are applied at `run`/`smoke`/`capture` time
# via --update-env-vars so the SAME job definition serves every window.
cmd_deploy() {
  verify_bucket
  local action=create
  if gcloud_imp run jobs describe "${JOB_NAME}" --project "${PROJECT}" \
       --region "${REGION}" >/dev/null 2>&1; then
    action=update
  fi
  log "${action} Cloud Run JOB ${JOB_NAME} in ${REGION} (Tokyo, public egress)"
  log "  run-as SA: ${RUN_AS_SA}  |  output: gs://${BUCKET}/txmix/"
  gcloud_imp run jobs "${action}" "${JOB_NAME}" \
    --project "${PROJECT}" \
    --region "${REGION}" \
    --image "${IMAGE}:${IMAGE_TAG}" \
    --service-account "${RUN_AS_SA}" \
    --task-timeout "${TASK_TIMEOUT}" \
    --max-retries "${MAX_RETRIES_JOB}" \
    --memory "${MEMORY}" \
    --cpu "${CPU}" \
    --set-env-vars "TXMIX_BUCKET=${BUCKET},TXMIX_MAX_RPM=${MAX_RPM},TXMIX_PAGE_LIMIT=${PAGE_LIMIT},TXMIX_REGION=${CAP_REGION_LABEL},TXMIX_BLOCKS=${TXMIX_BLOCKS:-200}"
  log "deployed. NO Cloud NAT / static egress (public Tokyo egress by design)."
}

# Execute the job for one window. Usage: _execute <label> [extra env k=v ...]
# Window comes from TXMIX_HEIGHTS ("LO HI") or TXMIX_BLOCKS in the caller's env.
_execute() {
  local label="$1"; shift || true
  local -a envs=("TXMIX_LABEL=${label}" "TXMIX_BUCKET=${BUCKET}" \
                 "TXMIX_MAX_RPM=${MAX_RPM}" "TXMIX_REGION=${CAP_REGION_LABEL}")
  if [[ -n "${TXMIX_HEIGHTS:-}" ]]; then
    envs+=("TXMIX_HEIGHTS=${TXMIX_HEIGHTS}")
    log "execute ${JOB_NAME}: window=heights[${TXMIX_HEIGHTS}] label=${label}"
  else
    envs+=("TXMIX_BLOCKS=${TXMIX_BLOCKS:-200}")
    log "execute ${JOB_NAME}: window=${TXMIX_BLOCKS:-200} recent blocks label=${label}"
  fi
  local joined
  joined="$(IFS=,; echo "${envs[*]}")"
  gcloud_imp run jobs execute "${JOB_NAME}" \
    --project "${PROJECT}" --region "${REGION}" \
    --update-env-vars "${joined}" \
    --wait
}

cmd_smoke() {
  # Tiny window — a VALIDATION sample, NOT the representative mix.
  : "${TXMIX_HEIGHTS:=}"
  : "${TXMIX_BLOCKS:=3}"
  log "SMOKE run: tiny window (small N) — validates machinery, NOT the answer to G1."
  _execute "smoke"
}

cmd_capture() {
  # Operator's representative window (peak/off-peak — a human judgment).
  if [[ -z "${TXMIX_HEIGHTS:-}" && -z "${TXMIX_BLOCKS_EXPLICIT:-}" && "${TXMIX_BLOCKS:-200}" == "200" ]]; then
    log "NOTE: using default 200-block window. For a REPRESENTATIVE capture set"
    log "      TXMIX_HEIGHTS=\"<LO> <HI>\" (a chosen peak/off-peak window) or TXMIX_BLOCKS=<N>."
  fi
  _execute "${TXMIX_LABEL:-capture}"
}

cmd_run() { _execute "${TXMIX_LABEL:-run}"; }

# Print the newest artifact set under gs://BUCKET/txmix/ (or a given prefix).
cmd_results() {
  local prefix="${1:-}"
  local base="gs://${BUCKET}/txmix/"
  if [[ -z "${prefix}" ]]; then
    prefix="$(gcloud_imp storage ls "${base}" --project "${PROJECT}" 2>/dev/null \
              | sort | tail -1)"
    [[ -z "${prefix}" ]] && die "no artifacts under ${base}"
  fi
  log "latest artifact: ${prefix}"
  echo "================= tx-mix.meta.json ================="
  gcloud_imp storage cat "${prefix%/}/tx-mix.meta.json" --project "${PROJECT}" 2>/dev/null || echo "(none)"
  echo "================= tx-mix.txt ======================="
  gcloud_imp storage cat "${prefix%/}/tx-mix.txt" --project "${PROJECT}" 2>/dev/null || echo "(none)"
  echo "================= DONE =============================="
  gcloud_imp storage cat "${prefix%/}/DONE" --project "${PROJECT}" 2>/dev/null || echo "(none)"
}

# Post a cited summary built from a GCS artifact to issue #128 (+ optionally
# Discussion #77). This is the thin GitHub follow-up step (in-Job GitHub
# auth is awkward; the durable artifact already lives in GCS). Requires gh.
cmd_post() {
  local prefix="${1:-}"
  local issue="${TXMIX_ISSUE:-128}"
  local base="gs://${BUCKET}/txmix/"
  if [[ -z "${prefix}" ]]; then
    prefix="$(gcloud_imp storage ls "${base}" --project "${PROJECT}" 2>/dev/null | sort | tail -1)"
    [[ -z "${prefix}" ]] && die "no artifacts under ${base}"
  fi
  command -v gh >/dev/null || die "gh CLI not found — needed to post the summary"
  local meta mix
  meta="$(gcloud_imp storage cat "${prefix%/}/tx-mix.meta.json" --project "${PROJECT}" 2>/dev/null || echo '{}')"
  mix="$(gcloud_imp storage cat "${prefix%/}/tx-mix.txt" --project "${PROJECT}" 2>/dev/null || echo '(no mix table)')"
  local body
  body="$(cat <<EOF
## tx-mix capture result (Cloud Run Job, asia-northeast1 / Tokyo)

**Provenance (measurement-citation norm):**
\`\`\`json
${meta}
\`\`\`

**Mix table:**
\`\`\`
${mix}
\`\`\`

Artifact: \`${prefix%/}/\` (tx-mix.txt, tx-mix.meta.json, DONE).

> A small-N window is a VALIDATION sample, not the representative mainnet mix.
> The representative mix requires an operator-chosen peak/off-peak window.

Refs #128
EOF
)"
  log "posting cited summary to issue #${issue}"
  gh issue comment "${issue}" --repo "${TXMIX_REPO:-kunallimaye/lighter-prover}" --body "${body}"
}

cmd_all_smoke() {
  cmd_build
  cmd_deploy
  cmd_smoke
  cmd_results
}

case "${1:-}" in
  build)     cmd_build ;;
  deploy)    cmd_deploy ;;
  smoke)     cmd_smoke ;;
  capture)   cmd_capture ;;
  run)       cmd_run ;;
  results)   shift; cmd_results "${1:-}" ;;
  post)      shift; cmd_post "${1:-}" ;;
  all-smoke) cmd_all_smoke ;;
  ""|-h|--help|help) usage ;;
  *) die "unknown subcommand '$1' (see --help)" ;;
esac
