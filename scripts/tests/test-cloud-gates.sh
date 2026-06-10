#!/usr/bin/env bash
# test-cloud-gates.sh -- regression tests for issue #36.
#
# Bug: `[[ -z "${VAR}" ]] && die ...` as the FINAL statement of a function
# returns 1 when VAR is set (the AND-list short-circuits false). Under
# `set -euo pipefail` the function-call then fails and errexit kills the
# whole script with no error message -- precisely when the config is VALID.
#
# Case 1 (functional): extract _require_topology from scripts/cloud.sh,
#   give it a full collapsed topology, and assert it returns 0 under
#   `set -euo pipefail`.
# Case 2 (tripwire):   assert the forbidden `]] && die` pattern never
#   reappears in scripts/.

set -euo pipefail

THIS_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
SCRIPTS_DIR="$(cd "${THIS_DIR}/.." && pwd)"
CLOUD_SH="${SCRIPTS_DIR}/cloud.sh"

pass=0
fail=0

report() {
  local name="$1" ok="$2" detail="${3:-}"
  if [[ "${ok}" == "0" ]]; then
    printf 'CASE %s ... PASS\n' "${name}"
    pass=$((pass+1))
  else
    printf 'CASE %s ... FAIL\n' "${name}"
    if [[ -n "${detail}" ]]; then
      while IFS= read -r _line; do printf '    %s\n' "${_line}"; done <<< "${detail}"
    fi
    fail=$((fail+1))
  fi
}

# ── Case 1: _require_topology returns 0 with a valid collapsed topology ──
# Extract just the function body (cloud.sh has a dispatcher at the bottom
# and sources common.sh with traps/log dirs -- too entangled to source
# wholesale). awk pulls the function definition verbatim from the file
# under test, so the test exercises the real shipped code.
extracted="$(awk '/^_require_topology\(\)/,/^}/' "${CLOUD_SH}")"
if [[ -z "${extracted}" ]]; then
  report "extract _require_topology from cloud.sh" 1 "function not found"
else
  rc=0
  out="$(bash -c '
    set -euo pipefail
    die() { echo "die: $*" >&2; exit 1; }
    ORCH_PROJECT=dummy
    BUILD_PROJECT=dummy
    RUNTIME_PROJECT=dummy
    TF_STATE_BUCKET=dummy
    eval "$1"
    _require_topology
    echo "gate-returned-0"
  ' _ "${extracted}" 2>&1)" || rc=$?
  if [[ "${rc}" -eq 0 && "${out}" == *"gate-returned-0"* ]]; then
    report "_require_topology returns 0 when topology IS set (set -euo pipefail)" 0
  else
    report "_require_topology returns 0 when topology IS set (set -euo pipefail)" 1 \
      "exit=${rc} output: ${out}"
  fi

  # Negative control: gate must still die when a project is missing.
  rc=0
  out="$(bash -c '
    set -euo pipefail
    die() { echo "die: $*" >&2; exit 1; }
    ORCH_PROJECT=dummy
    BUILD_PROJECT=dummy
    RUNTIME_PROJECT=""
    eval "$1"
    _require_topology
  ' _ "${extracted}" 2>&1)" || rc=$?
  if [[ "${rc}" -ne 0 && "${out}" == *"RUNTIME_PROJECT not set"* ]]; then
    report "_require_topology still dies when RUNTIME_PROJECT is empty" 0
  else
    report "_require_topology still dies when RUNTIME_PROJECT is empty" 1 \
      "exit=${rc} output: ${out}"
  fi
fi

# ── Case 2: tripwire -- forbidden pattern must not reappear ──────────────
# `[[ ... ]] && die` is always a latent bug under set -e (fatal when
# function-final). The safe forms are `if [[ ... ]]; then die ...; fi`
# or `[[ ... ]] || die ...`. Exclude this test file and comments.
hits="$(grep -rn ']] && die' "${SCRIPTS_DIR}" \
  --include='*.sh' \
  | grep -v "tests/test-cloud-gates.sh" \
  | grep -v '^\s*[^:]*:[0-9]*:\s*#' || true)"
if [[ -z "${hits}" ]]; then
  report "no ']] && die' pattern anywhere in scripts/" 0
else
  report "no ']] && die' pattern anywhere in scripts/" 1 "${hits}"
fi

echo ""
echo "test-cloud-gates: ${pass} passed, ${fail} failed"
[[ "${fail}" -eq 0 ]]
