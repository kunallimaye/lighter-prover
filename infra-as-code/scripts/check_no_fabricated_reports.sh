#!/usr/bin/env bash
#
# check_no_fabricated_reports.sh  --  anti-fabrication CI guard (issue #282)
#
# Purpose
# -------
# The benchmark suite once fabricated "empirical" metrics: shell functions
# slept for a fixed duration and then wrote hardcoded heredoc JSON/Markdown
# ledgers (e.g. empirical GKE wall times, fixed annual-savings figures, a
# hardcoded capstone proving-time matrix, and an impossible 500-tx wall time of
# a few hundred microseconds). Issue #282 removed those functions and the
# artifacts they produced.
#
# This guard makes the fabrication non-recurring. It fails (exit 1) if EITHER:
#
#   (1) STRUCTURAL: any infra-as-code/scripts/*.sh contains a heredoc that
#       writes a file under reports/ (i.e. `cat <<...EOF > .../reports/...`).
#       Report files must be produced by the prover/bench binaries from parsed
#       measurements, never hand-written by shell heredocs.
#
#   (2) FINGERPRINT: any *newly introduced* tracked file contains one of the
#       known fabricated literal constants. Files already documented as
#       known-suspect in reports/PROVENANCE.md (awaiting human verification)
#       are allow-listed so the guard passes on the cleaned tree but still
#       catches NEW reintroductions.
#
# The guard intentionally ignores:
#   - reports/PROVENANCE.md            (the audit trail documents the literals)
#   - this script itself               (the blocklist necessarily names them)
#   - files allow-listed by PROVENANCE.md's "FLAG-SUSPECT" section
#
# Performance: one `git grep` invocation per fingerprint across all scanned
# paths (not per-file), then results are filtered against the allow-list.
#
# Usage:   bash infra-as-code/scripts/check_no_fabricated_reports.sh
# Exit:    0 = clean, 1 = fabrication detected
#
set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
REPO_ROOT="$(cd "${SCRIPT_DIR}/../.." && pwd)"
cd "${REPO_ROOT}"

SELF_REL="infra-as-code/scripts/check_no_fabricated_reports.sh"
PROVENANCE="reports/PROVENANCE.md"

fail=0
note() { printf '\033[1;31m[FABRICATION]\033[0m %s\n' "$*" >&2; }
ok()   { printf '\033[1;32m[OK]\033[0m %s\n' "$*"; }
info() { printf '\033[1;34m[INFO]\033[0m %s\n' "$*"; }

# --- Known fabricated literal fingerprints (extended regex, digit-bounded) ---
# Bounded with (^|[^0-9.]) ... ([^0-9]|$) so legitimate per-chunk timings such
# as "441.658993ms" or "9.231450516s" do NOT false-positive on 41.65 / 231450.
FINGERPRINTS=(
  '(^|[^0-9.])12152([^0-9]|$)'
  '(^|[^0-9])12\.152([^0-9]|$)'
  '(^|[^0-9])1384431([^0-9]|$)'
  '1,384,431'
  '(^|[^0-9])0\.000291749([^0-9]|$)'
  '(^|[^0-9.])231450([^0-9]|$)'
  'empirical_gke_wall_time_ms'
  'annual_fleet_savings_usd'
  'measured_block_proving_time_s"?[[:space:]]*:?[[:space:]]*(224\.60|206\.20|22\.50|1254\.50|19\.50|26\.41)'
)

# Paths scanned for fingerprints.
SCAN_PATHS=(reports/ infra-as-code/scripts/)

# --- Build the allow-list of known-suspect files from PROVENANCE.md ----------
# Any reports/... path mentioned in PROVENANCE.md is treated as documented and
# excluded from the fingerprint check (the audit file is the human record).
ALLOW_PATTERN=""
if [[ -f "${PROVENANCE}" ]]; then
  ALLOW_PATTERN="$(grep -oE 'reports/[^ `|]+' "${PROVENANCE}" | sort -u || true)"
fi

is_allowlisted() {
  local f="$1"
  [[ "${f}" == "${PROVENANCE}" ]] && return 0
  [[ "${f}" == "${SELF_REL}" ]] && return 0
  if [[ -n "${ALLOW_PATTERN}" ]] && grep -qxF "${f}" <<< "${ALLOW_PATTERN}"; then
    return 0
  fi
  return 1
}

# ---------------------------------------------------------------------------
# (1) STRUCTURAL CHECK: no heredoc in scripts/*.sh writing into reports/
# ---------------------------------------------------------------------------
info "Structural check: scripts must not heredoc-write report files..."
struct_out="$(git grep -nE "cat[[:space:]]+<<[-]?[\"']?[A-Za-z_]+[\"']?[[:space:]]*>>?[[:space:]]*[^|]*reports/" -- 'infra-as-code/scripts/*.sh' || true)"
# Drop matches from this guard script itself.
struct_out="$(grep -v "^${SELF_REL}:" <<< "${struct_out}" || true)"
if [[ -n "${struct_out}" ]]; then
  note "Heredoc(s) writing into reports/ detected:"
  sed 's/^/    /' <<< "${struct_out}" >&2
  fail=1
else
  ok "No report-writing heredocs in shell scripts."
fi

# ---------------------------------------------------------------------------
# (2) FINGERPRINT CHECK: no fabricated literals in non-allow-listed files
# ---------------------------------------------------------------------------
info "Fingerprint check: scanning tracked files for fabricated constants..."
fp_reported=0
for fp in "${FINGERPRINTS[@]}"; do
  # One grep per fingerprint across all scan paths; -I skips binary files.
  hits="$(git grep -I -nE "${fp}" -- "${SCAN_PATHS[@]}" || true)"
  [[ -z "${hits}" ]] && continue
  while IFS= read -r line; do
    [[ -z "${line}" ]] && continue
    f="${line%%:*}"
    is_allowlisted "${f}" && continue
    note "Fabricated fingerprint (${fp}) in: ${line}"
    fp_reported=$((fp_reported + 1))
    fail=1
  done <<< "${hits}"
done
[[ ${fp_reported} -eq 0 ]] && ok "No fabricated fingerprints outside the documented suspect allow-list."

# ---------------------------------------------------------------------------
if [[ ${fail} -ne 0 ]]; then
  note "Fabricated benchmark metrics detected. See ${PROVENANCE} for the policy."
  note "Report files must be emitted by the prover/bench binaries from parsed"
  note "measurements - never hand-written heredocs. If a flagged file is a"
  note "genuine measured artifact, document it in ${PROVENANCE}."
  exit 1
fi
ok "Anti-fabrication guard passed: no fabricated metrics in tracked files."
exit 0
