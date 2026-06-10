#!/usr/bin/env bash
# common.sh -- shared helpers for bench-fleet scripts.
# Source-only. Do not execute directly.
#
# Conventions:
#  - All gcloud / gcloud storage calls go through gcloud_imp / gstorage_imp,
#    which add --project and (only when BENCH_SWEEP_SA is non-empty)
#    --impersonate-service-account. Since #33 the default is NO
#    impersonation: the active gcloud account is the orchestrator
#    identity (the old bench-sweep SA was deleted — see #32).
#  - All stdout from helper functions goes to stderr unless explicitly noted;
#    primary tool output (e.g. emitted TSV) stays on stdout.

set -euo pipefail

# ---------------------------------------------------------------------------
# Globals
# ---------------------------------------------------------------------------

# Toolkit root paths (resolve based on this file's location).
# These are deliberately exported so child processes (e.g. lib/render-discussion.sh
# invoked from run-fleet.sh) see them too.
_COMMON_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
FLEET_ROOT="$(cd "${_COMMON_DIR}/.." && pwd)"
export FLEET_ROOT
export FLEET_LIB="${FLEET_ROOT}/lib"
export FLEET_TEMPLATES="${FLEET_ROOT}/templates"
export FLEET_TESTS="${FLEET_ROOT}/tests"
_FLEET_REPO_ROOT="$(cd "${FLEET_ROOT}/../.." && pwd)"

# ── config.toml resolution (#33) ──
# Single source of truth is the repo-root config.toml, parsed by
# scripts/config.py (same resolver the scaffold's cloud.sh uses). We eval
# its exports here rather than sourcing scripts/common.sh, because that
# file installs traps + log redirection that would fight this toolkit's
# own logging. Env vars override everything below (`: "${VAR:=...}"`
# only assigns when unset/empty).
if [[ -f "${_FLEET_REPO_ROOT}/config.toml" ]]; then
  eval "$(python3 "${_FLEET_REPO_ROOT}/scripts/config.py")"
fi

# Project invariants. Resolution: env var > config.toml > hardcoded default.
# GCP_PROJECT / GCP_REGION / FLEET_* come from the config.py eval above.
: "${PROJECT:=${GCP_PROJECT:-kunal-scratch}}"
: "${REGION:=${GCP_REGION:-us-central1}}"
# bench-sweep impersonation is GONE (#32: the SA was deleted). Empty =
# use the active gcloud account directly. Set BENCH_SWEEP_SA only if you
# fork this toolkit into an environment that still needs impersonation.
: "${BENCH_SWEEP_SA:=}"
# kunal-scratch uses the AUTO-mode `default` VPC.
: "${NETWORK:=default}"
: "${SUBNET:=default}"
: "${GCS_BUCKET:=${FLEET_RESULTS_BUCKET:-gs://${PROJECT}-bench-fleet-runs}}"
# Artifact Registry image base for the prebuilt bench containers (#33).
# Per-machine tags resolve via machines.tsv's image_tag column:
#   <sha>-znver5 | <sha>-neoverse-v2 | <sha>-neoverse-n1
: "${AR_IMAGE_BASE:=${REGION}-docker.pkg.dev/${PROJECT}/lighter-prover/bench}"
# --tx-limit handed to every bench container (LIGHTER_TX_LIMIT env).
: "${TX_LIMIT:=${FLEET_TX_LIMIT:-480}}"

export PROJECT REGION BENCH_SWEEP_SA NETWORK SUBNET GCS_BUCKET AR_IMAGE_BASE TX_LIMIT

# Machine matrix: env > config.toml ([fleet].machines_tsv, repo-root
# relative) > toolkit default.
if [[ -z "${FLEET_MACHINES_TSV:-}" ]]; then
  if [[ -n "${FLEET_MACHINES_TSV_CFG:-}" ]]; then
    FLEET_MACHINES_TSV="${_FLEET_REPO_ROOT}/${FLEET_MACHINES_TSV_CFG}"
  else
    FLEET_MACHINES_TSV="${FLEET_ROOT}/machines.tsv"
  fi
fi
export FLEET_MACHINES_TSV

# ---------------------------------------------------------------------------
# Color logging
# ---------------------------------------------------------------------------

if [[ -t 2 ]]; then
  _C_RESET=$'\033[0m'
  _C_RED=$'\033[31m'
  _C_GREEN=$'\033[32m'
  _C_YELLOW=$'\033[33m'
  _C_BLUE=$'\033[34m'
  _C_GREY=$'\033[90m'
else
  _C_RESET=""; _C_RED=""; _C_GREEN=""; _C_YELLOW=""; _C_BLUE=""; _C_GREY=""
fi

_ts() { date -u +'%Y-%m-%dT%H:%M:%SZ'; }

log_info()  { printf '%s[INFO]%s  %s %s\n' "${_C_BLUE}"   "${_C_RESET}" "$(_ts)" "$*" >&2; }
log_ok()    { printf '%s[OK]%s    %s %s\n' "${_C_GREEN}"  "${_C_RESET}" "$(_ts)" "$*" >&2; }
log_warn()  { printf '%s[WARN]%s  %s %s\n' "${_C_YELLOW}" "${_C_RESET}" "$(_ts)" "$*" >&2; }
log_err()   { printf '%s[ERR]%s   %s %s\n' "${_C_RED}"    "${_C_RESET}" "$(_ts)" "$*" >&2; }
log_debug() {
  [[ "${FLEET_DEBUG:-0}" == "1" ]] || return 0
  printf '%s[DBG]%s   %s %s\n' "${_C_GREY}" "${_C_RESET}" "$(_ts)" "$*" >&2
}

die() { log_err "$*"; exit 1; }

# ---------------------------------------------------------------------------
# gcloud wrappers (impersonation only when BENCH_SWEEP_SA is set)
# ---------------------------------------------------------------------------

# Wraps `gcloud` with --project, adding --impersonate-service-account
# ONLY when BENCH_SWEEP_SA is non-empty. Default since #33: empty (the
# bench-sweep SA was deleted, #32) — calls run as the active account.
gcloud_imp() {
  if [[ -n "${BENCH_SWEEP_SA}" ]]; then
    gcloud --impersonate-service-account="${BENCH_SWEEP_SA}" \
           --project="${PROJECT}" \
           "$@"
  else
    gcloud --project="${PROJECT}" "$@"
  fi
}

# `gcloud storage` variant (rare separate code path in case we ever want to
# tweak storage-specific flags).
gstorage_imp() {
  gcloud_imp storage "$@"
}

# Human-readable description of the identity gcloud_imp runs as. Used in
# log lines so operators see which principal is acting.
fleet_identity() {
  if [[ -n "${BENCH_SWEEP_SA}" ]]; then
    printf 'impersonating %s\n' "${BENCH_SWEEP_SA}"
  else
    printf 'active account %s\n' "$(gcloud config get-value account 2>/dev/null || echo '<unknown>')"
  fi
}

# ---------------------------------------------------------------------------
# machines.tsv loader
# ---------------------------------------------------------------------------

# Print the data rows of machines.tsv (header stripped) to stdout.
# One row = one machine type.
machines_all_rows() {
  tail -n +2 "${FLEET_MACHINES_TSV}"
}

# Print just the machine_type column.
machines_all_types() {
  machines_all_rows | awk -F'\t' '{print $1}'
}

# Look up one row by machine_type. Prints the row to stdout, exit 1 if not found.
machines_lookup() {
  local mt="$1"
  local row
  row="$(machines_all_rows | awk -F'\t' -v m="$mt" '$1==m {print; exit}')"
  if [[ -z "$row" ]]; then
    log_err "machine_type not found in machines.tsv: $mt"
    return 1
  fi
  printf '%s\n' "$row"
}

# Field extractors. Usage: machine_field <machine_type> <field_name>
# Valid fields: machine_type vcpus arch image_family image_project
#               quota_family preferred_zones disk_type image_tag
machine_field() {
  local mt="$1" field="$2"
  local row
  row="$(machines_lookup "$mt")" || return 1
  # Header field index map
  local hdr; hdr="$(head -n1 "${FLEET_MACHINES_TSV}")"
  local idx
  idx="$(printf '%s\n' "$hdr" | awk -F'\t' -v f="$field" '{for(i=1;i<=NF;i++) if($i==f){print i; exit}}')"
  if [[ -z "$idx" ]]; then
    log_err "unknown field: $field"
    return 1
  fi
  printf '%s\n' "$row" | awk -F'\t' -v i="$idx" '{print $i}'
}

# ---------------------------------------------------------------------------
# Cost estimate
# ---------------------------------------------------------------------------
#
# Approximate on-demand hourly prices in USD for us-central1 (sourced from
# https://cloud.google.com/compute/all-pricing on 2026-06-10; verify before
# relying on for budgeting).
#
# Update this table when prices change OR when adding new machine types.

declare -A _PRICE_PER_HR=(
  [c4a-highcpu-32]=1.40
  [c4a-highcpu-64]=2.80
  [n4a-highcpu-32]=1.20
  [n4a-highcpu-64]=2.40
  [n4d-highcpu-32]=1.10
  [n4d-highcpu-64]=2.20
  [t2a-standard-32]=1.30
  [t2a-standard-48]=1.95
  [c4d-highcpu-32]=1.40
  [c4d-highcpu-64]=2.80
)

# Per-shape realistic full-sweep wall-time estimates (hours), calibrated
# against the v2 run findings tracked in issue #19. The previous estimator
# assumed 1h per VM, which was 3-6× too low.
#
#  - T2A (Neoverse-N1, weakest):       6h
#  - C4A / N4A (Axion, mid):           4h
#  - C4D / N4D (AMD Turin, strongest): 3h
declare -A _HOURS_PER_SHAPE=(
  [c4a-highcpu-32]=4
  [c4a-highcpu-64]=4
  [n4a-highcpu-32]=4
  [n4a-highcpu-64]=4
  [n4d-highcpu-32]=3
  [n4d-highcpu-64]=3
  [t2a-standard-32]=6
  [t2a-standard-48]=6
  [c4d-highcpu-32]=3
  [c4d-highcpu-64]=3
)

# estimate_cost <hours> <machine_type...> -> prints "$X.XX"
#
# If <hours> is "auto", uses _HOURS_PER_SHAPE per machine_type.
# Otherwise, uses the given fixed hours for every machine.
estimate_cost() {
  local hours="$1"; shift
  local total=0
  local mt price h
  for mt in "$@"; do
    price="${_PRICE_PER_HR[$mt]:-}"
    if [[ -z "$price" ]]; then
      log_warn "no price entry for $mt — estimating \$1.50/h"
      price=1.50
    fi
    if [[ "$hours" == "auto" ]]; then
      h="${_HOURS_PER_SHAPE[$mt]:-4}"
    else
      h="$hours"
    fi
    total="$(python3 -c "print(f'{$total + $price * $h:.4f}')")"
  done
  python3 -c "print(f'\${$total:.2f}')"
}

# estimate_cost_breakdown <machine_type...> -> prints per-shape lines to stderr
# and a final total to stdout (format "$X.XX"). Uses _HOURS_PER_SHAPE.
estimate_cost_breakdown() {
  local total=0
  local mt price h subtotal
  for mt in "$@"; do
    price="${_PRICE_PER_HR[$mt]:-1.50}"
    h="${_HOURS_PER_SHAPE[$mt]:-4}"
    subtotal="$(python3 -c "print(f'{$price * $h:.2f}')")"
    total="$(python3 -c "print(f'{$total + $price * $h:.4f}')")"
    printf '    %-20s  %2sh × $%-5s/h = $%s\n' "$mt" "$h" "$price" "$subtotal" >&2
  done
  python3 -c "print(f'\${$total:.2f}')"
}

# ---------------------------------------------------------------------------
# Run-id helpers
# ---------------------------------------------------------------------------

# Generate a fresh run-id: <UTC date>-<UTC time>-<6 char random>
new_run_id() {
  local stamp; stamp="$(date -u +'%Y%m%d-%H%M%S')"
  # /dev/urandom path is portable to Debian/macOS/Linux.
  local rand; rand="$(LC_ALL=C tr -dc 'a-z0-9' < /dev/urandom | head -c 6)"
  printf '%s-%s\n' "$stamp" "$rand"
}

# Short form for use in VM instance names (which max out at 63 chars).
short_run_id() {
  local rid="$1"
  # Take the last 8 chars of the timestamp + the 6-char random suffix.
  # e.g. 20260610-153045-abc123 -> 153045-abc123
  printf '%s\n' "$rid" | awk -F'-' '{printf "%s-%s\n", $2, $3}'
}

# Instance name: bf-<short-run-id>-<machine-shortname>
# GCE constraint: lowercase, hyphens only, <=63 chars, must start with letter.
instance_name() {
  local rid="$1" mt="$2"
  local short; short="$(short_run_id "$rid")"
  # Shorten machine type: drop the family prefix, keep size suffix.
  # c4a-highcpu-32 -> hc32, c4a-highcpu-64 -> hc64, n4a-highcpu-32 -> hc32,
  # t2a-standard-32 -> std32, t2a-standard-48 -> std48.
  # We must also keep the family bits to disambiguate, so use a stable mapping:
  # c4a-highcpu-32 -> c4ah32, n4a-highcpu-64 -> n4ah64, t2a-standard-48 -> t2as48.
  local short_mt
  case "$mt" in
    c4a-highcpu-*) short_mt="c4ah${mt##*-}" ;;
    c4d-highcpu-*) short_mt="c4dh${mt##*-}" ;;
    n4a-highcpu-*) short_mt="n4ah${mt##*-}" ;;
    n4d-highcpu-*) short_mt="n4dh${mt##*-}" ;;
    t2a-standard-*) short_mt="t2as${mt##*-}" ;;
    *) short_mt="$(printf '%s' "$mt" | tr -cd 'a-z0-9-' | cut -c1-15)" ;;
  esac
  printf 'bf-%s-%s\n' "$short" "$short_mt"
}

# GCS prefix for a given run + machine: gs://<bucket>/<run-id>/<machine>/
gcs_prefix() {
  local rid="$1" mt="$2"
  printf '%s/%s/%s/\n' "${GCS_BUCKET}" "$rid" "$mt"
}

# ---------------------------------------------------------------------------
# GCP runtime helpers
# ---------------------------------------------------------------------------

# Resolve the project's default Compute Engine SA (what the VMs run as).
# Cached for the life of the process.
_CACHED_COMPUTE_SA=""
get_compute_sa() {
  if [[ -n "$_CACHED_COMPUTE_SA" ]]; then
    printf '%s\n' "$_CACHED_COMPUTE_SA"
    return
  fi
  local pn
  pn="$(gcloud_imp projects describe "${PROJECT}" --format='value(projectNumber)')" \
    || die "could not resolve project number for ${PROJECT}"
  _CACHED_COMPUTE_SA="${pn}-compute@developer.gserviceaccount.com"
  printf '%s\n' "$_CACHED_COMPUTE_SA"
}

# Sentinel object name in GCS.
sentinel_uri() {
  local rid="$1" mt="$2"
  printf '%s_DONE\n' "$(gcs_prefix "$rid" "$mt")"
}

# Check whether a sentinel exists. Returns 0 if found, 1 otherwise.
# Suppresses normal output; errors go to debug log.
sentinel_exists() {
  local uri="$1"
  if gstorage_imp ls "$uri" >/dev/null 2>&1; then
    return 0
  fi
  return 1
}

# Pretty-print a duration in seconds as "Xm Ys".
fmt_duration() {
  local s="$1"
  python3 -c "
s = int(${s})
m, s = divmod(s, 60)
h, m = divmod(m, 60)
if h:
  print(f'{h}h {m}m {s}s')
elif m:
  print(f'{m}m {s}s')
else:
  print(f'{s}s')
"
}
