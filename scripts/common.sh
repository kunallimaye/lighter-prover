#!/usr/bin/env bash
# Common functions sourced by all scripts
# Tier-1 hygiene (issue #140): set -euo pipefail, traps, stable log paths,
# exit-code discipline. Tier-2 detached-orchestration helpers are below.
set -euo pipefail

# ─── Logging ──────────────────────────────────────────────────────────

RED='\033[0;31m'
GREEN='\033[0;32m'
YELLOW='\033[0;33m'
BLUE='\033[0;34m'
NC='\033[0m' # No Color

log_info()  { echo -e "${BLUE}[INFO]${NC}  $*"; }
log_ok()    { echo -e "${GREEN}[OK]${NC}    $*"; }
log_warn()  { echo -e "${YELLOW}[WARN]${NC}  $*"; }
log_error() { echo -e "${RED}[ERROR]${NC} $*" >&2; }

die() { log_error "$@"; exit 1; }

# ─── Paths ───────────────────────────────────────────────────────────

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
PROJECT_ROOT="$(cd "${SCRIPT_DIR}/.." && pwd)"

# Load .env if it exists. .env is the override layer for sensitive values
# (project IDs, billing accounts, emails, API keys). Never committed.
if [[ -f "${PROJECT_ROOT}/.env" ]]; then
  set -a
  # shellcheck disable=SC1091
  source "${PROJECT_ROOT}/.env"
  set +a
fi

# ─── Environment Selection ───────────────────────────────────────────
# Priority: CLI env var > .env file > default (staging).
# Environment-axis layering (per #115) is independent of the role axis
# (per #141). A single environment slices through all three roles.
export ENVIRONMENT="${ENVIRONMENT:-staging}"

# ─── Config.toml Parsing (via Python) ────────────────────────────────
# scripts/config.py parses config.toml with role-axis (defaults +
# orchestration + build + runtime) and environment-axis layering, then
# emits shell exports. Same parser is the source of truth for Terraform
# (via Cloud Build TF_VAR_* substitutions).

if [[ -f "${PROJECT_ROOT}/config.toml" ]]; then
  log_info "Loading config from config.toml (environment: ${ENVIRONMENT})"
  eval "$(python3 "${SCRIPT_DIR}/config.py")"
fi

# ─── Defaults (override in .env, config.toml, or environment) ────────

export PROJECT_NAME="${PROJECT_NAME:-$(basename "${PROJECT_ROOT}")}"
export IMAGE_NAME="${IMAGE_NAME:-${PROJECT_NAME}}"
export IMAGE_TAG="${IMAGE_TAG:-latest}"

# Three-role topology resolved values.
# Each role can collapse to the same project (90% case) or split.
# config.py resolves env > role > defaults > error.
export ORCH_PROJECT="${ORCH_PROJECT:-}"
export ORCH_REGION="${ORCH_REGION:-}"
export BUILD_PROJECT="${BUILD_PROJECT:-}"
export BUILD_REGION="${BUILD_REGION:-}"
export RUNTIME_PROJECT="${RUNTIME_PROJECT:-}"
export RUNTIME_REGION="${RUNTIME_REGION:-}"

# Legacy aliases for back-compat with downstream snippets that still use
# the pre-role-topology names. Resolve to the matching role.
export GCP_PROJECT="${GCP_PROJECT:-${RUNTIME_PROJECT}}"
export GCP_REGION="${GCP_REGION:-${RUNTIME_REGION:-us-central1}}"
export CB_PROJECT="${CB_PROJECT:-${BUILD_PROJECT}}"

# Resource defaults
export AR_REPO="${AR_REPO:-${PROJECT_NAME}}"
export TF_STATE_BUCKET="${TF_STATE_BUCKET:-}"
export TF_STATE_PREFIX="${TF_STATE_PREFIX:-${PROJECT_NAME}/${ENVIRONMENT}}"
export DOMAIN="${DOMAIN:-}"
export DNS_PROJECT_ID="${DNS_PROJECT_ID:-}"
export DNS_MANAGED_ZONE="${DNS_MANAGED_ZONE:-}"
export DNS_RECORD_NAME="${DNS_RECORD_NAME:-}"
export MIN_INSTANCES="${MIN_INSTANCES:-0}"
export MAX_INSTANCES="${MAX_INSTANCES:-3}"
export INGRESS="${INGRESS:-all}"

# Service account defaults — agent runs in orchestration project (operator
# CLI identity), builder runs in build project (Cloud Build identity),
# runtime runs in runtime project (Cloud Run app identity).
export AGENT_SA_NAME="${AGENT_SA_NAME:-${PROJECT_NAME}-agent}"
export BUILDER_SA_NAME="${BUILDER_SA_NAME:-${PROJECT_NAME}-builder}"
export RUNTIME_SA_NAME="${RUNTIME_SA_NAME:-${PROJECT_NAME}-runtime}"

# Custom role for the agent SA (curated YAML in cicd/iam/).
# The custom role ID GCP wants is camelCase (no dashes / slashes).
_to_camel() {
  echo "$1" | awk -F'[-_]' '{out=$1; for(i=2;i<=NF;i++) out=out toupper(substr($i,1,1)) substr($i,2); print out}'
}
export DEPLOYER_ROLE_ID="${DEPLOYER_ROLE_ID:-$(_to_camel "${PROJECT_NAME}")Deployer}"
export DEPLOYER_ROLE_YAML="${DEPLOYER_ROLE_YAML:-${PROJECT_ROOT}/cicd/iam/${PROJECT_NAME}-deployer-role.yaml}"

# 30-day expiry for the agent → custom-role binding (#141 lesson 2).
# Operator re-runs admin-cloud-init to refresh.
export AGENT_ROLE_EXPIRY_DAYS="${AGENT_ROLE_EXPIRY_DAYS:-30}"

# Derived SA emails. When all three role projects collapse to one, these
# all live in the same project but have distinct local-parts.
export AGENT_SA_EMAIL="${AGENT_SA_EMAIL:-${AGENT_SA_NAME}@${ORCH_PROJECT:-${BUILD_PROJECT}}.iam.gserviceaccount.com}"
export BUILDER_SA_EMAIL="${BUILDER_SA_EMAIL:-${BUILDER_SA_NAME}@${BUILD_PROJECT}.iam.gserviceaccount.com}"
export RUNTIME_SA_EMAIL="${RUNTIME_SA_EMAIL:-${RUNTIME_SA_NAME}@${RUNTIME_PROJECT}.iam.gserviceaccount.com}"

# Legacy alias: some downstream snippets still reference CB_SERVICE_ACCOUNT.
# Map it to the builder SA (which is what Cloud Build submits as).
export CB_SERVICE_ACCOUNT="${CB_SERVICE_ACCOUNT:-${BUILDER_SA_EMAIL}}"

# ─── Helpers ──────────────────────────────────────────────────────────

require_cmd() {
  command -v "$1" &>/dev/null || die "'$1' is required but not installed."
}

confirm() {
  local prompt="${1:-Are you sure?} [y/N] "
  read -r -p "${prompt}" response
  [[ "${response}" =~ ^[Yy]$ ]]
}

# Print the resolved three-role topology. Used by `make cloud-help` and
# preflight. When all three projects collapse to one, the operator sees
# an explicit "collapsed to one project" note so the choice is obvious.
print_topology() {
  local op="${ORCH_PROJECT:-<unset>}"
  local bp="${BUILD_PROJECT:-<unset>}"
  local rp="${RUNTIME_PROJECT:-<unset>}"
  echo "Cloud topology: orchestration=${op}, build=${bp}, runtime=${rp}"
  if [[ "${op}" == "${bp}" && "${bp}" == "${rp}" && "${op}" != "<unset>" ]]; then
    echo "               (all three collapsed to one project — fine for personal/hobby use)"
  elif [[ "${bp}" == "${rp}" ]]; then
    echo "               (build + runtime collapsed; orchestration split — useful when agent identity lives outside build/runtime)"
  elif [[ "${op}" == "${bp}" ]]; then
    echo "               (orchestration + build collapsed; runtime split — production tenancy pattern)"
  else
    echo "               (fully split — production multi-project tenancy)"
  fi
}

# Deterministic hash over the tracked circuit sources (circuit/src/**),
# computed from WORKING-TREE contents so uncommitted edits are detected.
# Calibration validity is tied to the circuit code it measured (issue
# #102): scripts/s-calibrate.sh stamps this hash into every calibration
# registry entry, and scripts/calibration-check.sh compares it against
# the current tree (warn-only staleness guard). Stable and cheap: a few
# `git hash-object` calls over a small file set.
circuit_src_hash() {
  if ! command -v git >/dev/null 2>&1 \
     || ! git -C "${PROJECT_ROOT}" rev-parse --git-dir >/dev/null 2>&1; then
    echo "unknown"
    return 0
  fi
  (
    cd "${PROJECT_ROOT}" \
      && git ls-files -- circuit/src | LC_ALL=C sort | while IFS= read -r f; do
           printf '%s ' "${f}"
           git hash-object -- "${f}"
         done | git hash-object --stdin
  ) 2>/dev/null || echo "unknown"
}

# Returns 0 (true) when role_a project equals role_b project.
# Usage: same_project ORCH_PROJECT BUILD_PROJECT && echo collapsed
#
# Uses bash indirect expansion (${!var}) instead of `eval` — avoids the
# class of code-injection risk that `eval` on caller-supplied strings
# carries, even though every caller here passes a hardcoded identifier.
same_project() {
  local a_val="${!1:-}" b_val="${!2:-}"
  [[ -n "${a_val}" && "${a_val}" == "${b_val}" ]]
}

# ─── Log Capture ─────────────────────────────────────────────────────

LOG_DIR="${PROJECT_ROOT}/logs"
mkdir -p "${LOG_DIR}"

# Start capturing all stdout/stderr to a per-run log file.
# Stable path convention (#140 Tier-1): logs/<timestamp>-<action>.log.
# Operators and agents always know where to look after the fact.
# Usage: start_log <action-name>
start_log() {
  local action="${1:-unknown}"
  LOG_FILE="${LOG_DIR}/$(date +%Y%m%d-%H%M%S)-${action}.log"
  # Set SCRIPT_ACTION so the EXIT/INT/TERM/HUP trap handler can name the
  # action in its forensic log line — without this, only detached-orchestrated
  # runs (which set SCRIPT_ACTION inside run_detached_*) had meaningful context.
  SCRIPT_ACTION="${action}"
  exec > >(tee -a "${LOG_FILE}") 2>&1
  log_info "Logging to ${LOG_FILE}"
}

# ─── Tier-1 hygiene: trap on EXIT/INT/TERM/HUP ────────────────────────
# The real universal lesson from kunal-labs/dex-arb-agent#136 is that
# any script mutating external state must leave a forensic breadcrumb
# when interrupted. Even a stub handler that logs "interrupted at line N"
# is a huge win when the parent shell disconnects mid-deploy.
#
# Scripts that want richer behavior (recovery file, heartbeat cleanup)
# should override _trap_handler after sourcing common.sh.

_trap_handler() {
  local exit_code="$1"
  local line_no="${2:-?}"
  if (( exit_code != 0 )); then
    log_error "Script interrupted (exit=${exit_code}, line=${line_no})."
    log_error "Action: ${SCRIPT_ACTION:-unknown}"
    log_error "Log file: ${LOG_FILE:-(none — start_log not called)}"
    # If a heartbeat / checkpoint is active, surface it so the operator
    # knows recovery state may exist.
    if [[ -n "${HEARTBEAT_FILE:-}" && -f "${HEARTBEAT_FILE}" ]]; then
      log_error "Heartbeat file: ${HEARTBEAT_FILE} (run 'make cloud-status' for state)"
    fi
    if [[ -n "${RECOVERY_FILE:-}" ]]; then
      echo "interrupted exit=${exit_code} line=${line_no} time=$(date +%s) action=${SCRIPT_ACTION:-unknown}" \
        >> "${RECOVERY_FILE}"
      log_error "Recovery hint written to ${RECOVERY_FILE} (run 'make cloud-recover')"
    fi
  fi
}

trap '_trap_handler $? ${LINENO}' EXIT
trap '_trap_handler 130 ${LINENO}; exit 130' INT
trap '_trap_handler 143 ${LINENO}; exit 143' TERM
trap '_trap_handler 129 ${LINENO}; exit 129' HUP

# ─── Tier-2 detached orchestration helpers (#140 + #141 lesson 3) ─────
#
# Two flavors:
#
#   1. run_detached_cloudbuild  — single atomic remote job.
#      Heartbeat fields: build_id, started_at, last_seen_at, status.
#      tfstate-lock-aware recovery: if the parent dies but the build
#      runs to SUCCESS, the next 'make cloud-status' / 'cloud-recover'
#      can break a stuck tfstate lock and reconcile.
#
#   2. run_detached_stepwise    — N sequential local steps + checkpoint.
#      Checkpoint embeds sha256 of the step list (#141 lesson 3): on
#      resume, mismatch = treat checkpoint as stale and restart from
#      step 1. Step idempotency is a contract; restart-from-1 is safe.
#
# Operator escape hatch: ORCH_FORCE_RESTART=1 invalidates the checkpoint
# unconditionally and starts fresh. Document prominently in cloud.sh
# help text + Makefile.

ORCH_STATE_DIR="${PROJECT_ROOT}/.orchestration"
mkdir -p "${ORCH_STATE_DIR}"

# Heartbeat / checkpoint / recovery file paths are per-action.
_orch_paths() {
  local action="$1"
  HEARTBEAT_FILE="${ORCH_STATE_DIR}/${action}.heartbeat"
  CHECKPOINT_FILE="${ORCH_STATE_DIR}/${action}.checkpoint"
  RECOVERY_FILE="${ORCH_STATE_DIR}/${action}.recovery"
}

# Write a JSON-ish heartbeat line. Append-only; cloud-status reads tail.
_heartbeat_write() {
  local action="$1" phase="$2" extra="${3:-}"
  local now
  now="$(date +%s)"
  printf '{"action":"%s","phase":"%s","ts":%s%s}\n' \
    "${action}" "${phase}" "${now}" "${extra:+,${extra}}" \
    >> "${HEARTBEAT_FILE}"
}

# Compute checkpoint key = sha256 of the step list string. When the step
# list changes between runs, the key changes; a stale checkpoint is
# silently invalidated. Prevents the dex-arb-agent #87 / onchain-markets
# #89 class of bug ("we added two new steps but the checkpoint=6 skipped
# them silently").
_step_list_hash() {
  printf '%s\n' "$@" | sha256sum | awk '{print $1}'
}

_checkpoint_read() {
  [[ -f "${CHECKPOINT_FILE}" ]] || { echo ""; return 0; }
  cat "${CHECKPOINT_FILE}"
}

_checkpoint_write() {
  local hash="$1" step_idx="$2"
  printf 'hash=%s\nstep=%s\nupdated_at=%s\n' "${hash}" "${step_idx}" "$(date +%s)" \
    > "${CHECKPOINT_FILE}"
}

_checkpoint_clear() { rm -f "${CHECKPOINT_FILE}"; }

# Drive a sequence of idempotent step functions through with checkpoint
# resume + step-list-hash invalidation. The caller passes the action
# name as $1 and the step-function names as $2..$N.
#
# Each step function takes no arguments. Each MUST be idempotent (run
# twice is a no-op the second time). Exit nonzero = halt + leave
# checkpoint at the last-completed step so re-run picks up there.
run_detached_stepwise() {
  local action="$1"; shift
  local -a steps=("$@")
  local nsteps="${#steps[@]}"
  (( nsteps > 0 )) || die "run_detached_stepwise: no steps provided"

  _orch_paths "${action}"
  local current_hash
  current_hash="$(_step_list_hash "${steps[@]}")"

  local start_step=1
  local prior
  prior="$(_checkpoint_read)"

  if [[ "${ORCH_FORCE_RESTART:-0}" == "1" ]]; then
    log_warn "ORCH_FORCE_RESTART=1 set — clearing checkpoint and restarting from step 1."
    _checkpoint_clear
  elif [[ -n "${prior}" ]]; then
    local prior_hash prior_step
    prior_hash="$(echo "${prior}" | awk -F'=' '/^hash=/ {print $2}')"
    prior_step="$(echo "${prior}" | awk -F'=' '/^step=/ {print $2}')"
    if [[ "${prior_hash}" == "${current_hash}" ]]; then
      start_step=$((prior_step + 1))
      log_info "Resuming from step ${start_step}/${nsteps} (checkpoint hash matches)."
    else
      log_warn "Checkpoint hash mismatch (step list changed since last run)."
      log_warn "Treating checkpoint as stale — restarting from step 1."
      log_warn "(Override: set ORCH_FORCE_RESTART=1 to silence this. Step idempotency makes restart safe.)"
      _checkpoint_clear
    fi
  fi

  _heartbeat_write "${action}" "start" "\"nsteps\":${nsteps},\"start_step\":${start_step}"
  SCRIPT_ACTION="${action}"

  local i=0
  for step_fn in "${steps[@]}"; do
    i=$((i + 1))
    if (( i < start_step )); then
      log_info "Step ${i}/${nsteps}: ${step_fn} — skipped (checkpoint)."
      continue
    fi
    log_info "Step ${i}/${nsteps}: ${step_fn}"
    _heartbeat_write "${action}" "step" "\"i\":${i},\"fn\":\"${step_fn}\""
    if ! "${step_fn}"; then
      _heartbeat_write "${action}" "failed" "\"i\":${i},\"fn\":\"${step_fn}\""
      die "Step ${i}/${nsteps} (${step_fn}) failed. Re-run to resume; ORCH_FORCE_RESTART=1 to restart from scratch."
    fi
    _checkpoint_write "${current_hash}" "${i}"
  done

  _heartbeat_write "${action}" "complete" "\"nsteps\":${nsteps}"
  _checkpoint_clear
  log_ok "Detached run complete: ${action} (${nsteps} steps)."
}

# Single atomic remote job (Cloud Build). The caller provides the
# gcloud-builds-submit command as a function name. The helper records
# the build_id, heartbeats while it runs, and writes a recovery hint if
# the parent dies. tfstate-lock-aware recovery is out of scope here
# (the runner is expected to use a tfstate backend with the lock TTL
# tuned to the build's max duration).
run_detached_cloudbuild() {
  local action="$1" submit_fn="$2"
  _orch_paths "${action}"
  _heartbeat_write "${action}" "submit"
  SCRIPT_ACTION="${action}"
  if ! "${submit_fn}"; then
    _heartbeat_write "${action}" "failed"
    die "Cloud Build submit failed for action: ${action}"
  fi
  _heartbeat_write "${action}" "complete"
}

# Read the most recent heartbeat phase. Used by cloud-status.
heartbeat_status() {
  local action="$1"
  _orch_paths "${action}"
  if [[ ! -f "${HEARTBEAT_FILE}" ]]; then
    echo "NEVER_STARTED"
    return 0
  fi
  local last phase ts now age
  last="$(tail -1 "${HEARTBEAT_FILE}")"
  phase="$(echo "${last}" | sed -E 's/.*"phase":"([^"]+)".*/\1/')"
  ts="$(echo "${last}" | sed -E 's/.*"ts":([0-9]+).*/\1/')"
  now="$(date +%s)"
  age=$((now - ts))
  case "${phase}" in
    complete) echo "COMPLETE (age=${age}s)" ;;
    failed)   echo "FAILED (age=${age}s, see ${HEARTBEAT_FILE})" ;;
    start|step|submit)
      if (( age > 600 )); then
        echo "STALLED (phase=${phase}, age=${age}s)"
      else
        echo "RUNNING (phase=${phase}, age=${age}s)"
      fi
      ;;
    *) echo "UNKNOWN (phase=${phase})" ;;
  esac
}

# Read the recovery file (written by the EXIT/HUP trap) and emit hints
# to the operator. Used by cloud-recover.
recovery_summary() {
  local action="$1"
  _orch_paths "${action}"
  if [[ ! -f "${RECOVERY_FILE}" ]]; then
    echo "No recovery state for ${action}."
    return 0
  fi
  echo "Recovery state for ${action}:"
  cat "${RECOVERY_FILE}"
}
