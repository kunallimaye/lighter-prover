#!/usr/bin/env bash
# calibration-check.sh -- calibration-registry staleness guard (issue #102).
#
# Recomputes the deterministic hash over circuit/src/** (working-tree
# contents, via circuit_src_hash in common.sh) and compares it against
# the circuit_hash stamped into every calibration/*.json registry entry.
# Prints OK when everything matches, or a WARNING naming the stale
# shapes when the circuit has changed since they were calibrated.
#
# This guard NEVER fails: it exits 0 in every case (no registry, parse
# errors, stale entries). It is wired into `make local-test` as a
# warning-only line and must not affect that target's pass/fail.
#
# Usage:
#   bash scripts/calibration-check.sh                # check the registry
#   bash scripts/calibration-check.sh --print-hash   # print current hash
#
# Knobs:
#   REGISTRY_DIR   Registry directory (default: <repo>/calibration).
#                  Override is primarily for the test suite
#                  (scripts/tests/test-registry.sh).

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
# Source common.sh with stdout silenced: it logs a config-loading INFO
# line when config.toml exists, which would corrupt --print-hash's bare
# output (and add noise to the warn-only local-test line).
# shellcheck disable=SC1091
source "${SCRIPT_DIR}/common.sh" >/dev/null

# --print-hash: bare output for scripting/tests; skip start_log noise.
if [[ "${1:-}" == "--print-hash" ]]; then
  circuit_src_hash
  exit 0
fi

start_log "calibration-check"

REGISTRY_DIR="${REGISTRY_DIR:-${PROJECT_ROOT}/calibration}"
current="$(circuit_src_hash)"

shopt -s nullglob
entries=("${REGISTRY_DIR}"/*.json)
shopt -u nullglob

if (( ${#entries[@]} == 0 )); then
  log_info "calibration-check: no registry entries under ${REGISTRY_DIR} -- nothing to check (run 'make s-calibrate OUT_REGISTRY=1' to seed one)"
  exit 0
fi

stale=()
for f in "${entries[@]}"; do
  h="$(python3 -c 'import json,sys; print(json.load(open(sys.argv[1])).get("circuit_hash","unknown"))' "$f" 2>/dev/null || echo "parse-error")"
  if [[ "${h}" != "${current}" ]]; then
    stale+=("$(basename "${f}") (calibrated at ${h:0:12}, tree is ${current:0:12})")
  fi
done

if (( ${#stale[@]} > 0 )); then
  log_warn "calibration-check: WARNING -- circuit/src has changed since these calibration entries were measured (results may be stale):"
  for s in "${stale[@]}"; do
    log_warn "  - ${s}"
  done
  log_warn "  refresh with 'make s-calibrate OUT_REGISTRY=1' (or 'make s-calibrate-fleet' for the cloud shapes); see calibration/README.md"
else
  log_ok "calibration-check: OK -- ${#entries[@]} registry entries match circuit hash ${current:0:12}"
fi

# Staleness is advisory by design (issue #102): always exit 0.
exit 0
