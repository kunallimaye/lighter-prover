#!/usr/bin/env bash
# test-startup-render.sh -- tripwire for vm-startup.sh.tmpl <-> provision.sh
# placeholder coupling (issue #102 Phase C).
#
# Bug class this guards against: adding a __TOKEN__ to the template
# without a matching gsub() in render_startup (or vice versa) ships a
# startup script with a literal "__TOKEN__" string to the VM -- which
# fails at runtime, after spend. Cases:
#
#   1. Every __TOKEN__ used in vm-startup.sh.tmpl has a gsub(/__TOKEN__/...)
#      in lib/provision.sh.
#   2. Every gsub(/__TOKEN__/...) in lib/provision.sh has at least one
#      consumer line in the template.
#   3. Simulated render (the same awk substitution provision.sh runs)
#      leaves no __[A-Z0-9_]__ placeholder behind and bash-parses clean.

set -euo pipefail

THIS_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
FLEET_ROOT="$(cd "${THIS_DIR}/.." && pwd)"
TMPL="${FLEET_ROOT}/templates/vm-startup.sh.tmpl"
PROV="${FLEET_ROOT}/lib/provision.sh"

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

# Comment lines may mention placeholder tokens descriptively (e.g. the
# header's "__PLACEHOLDER__ tokens" prose) -- only code lines count.
tmpl_tokens="$(grep -v '^[[:space:]]*#' "${TMPL}" | grep -o '__[A-Z0-9_]*__' | sort -u)"
prov_tokens="$(grep -o 'gsub(/__[A-Z0-9_]*__/' "${PROV}" \
                | sed 's|gsub(/||; s|/$||' | sort -u)"

# Case 1: template tokens all rendered.
missing=""
for t in ${tmpl_tokens}; do
  if ! grep -q "gsub(/${t}/" "${PROV}"; then
    missing="${missing} ${t}"
  fi
done
if [[ -z "${missing}" ]]; then
  report "every template __TOKEN__ has a gsub in provision.sh" 0
else
  report "every template __TOKEN__ has a gsub in provision.sh" 1 "unrendered:${missing}"
fi

# Case 2: no orphan gsubs.
orphans=""
for t in ${prov_tokens}; do
  if ! grep -q "${t}" "${TMPL}"; then
    orphans="${orphans} ${t}"
  fi
done
if [[ -z "${orphans}" ]]; then
  report "every provision.sh gsub token appears in the template" 0
else
  report "every provision.sh gsub token appears in the template" 1 "orphans:${orphans}"
fi

# Case 3: simulated render leaves no placeholders + parses as bash.
WORK="$(mktemp -d)"
trap 'rm -rf "${WORK}"' EXIT
awk \
  -v image="us-central1-docker.pkg.dev/p/r/bench:deadbee-neoverse-v2" \
  -v bucket="gs://test-bucket" \
  -v run_prefix="testrun/c4a-highcpu-4/" \
  -v svalues="8 9 10 11 20 21 32 40" \
  -v host_label="c4a-highcpu-4" \
  -v tx_limit="480" \
  -v cal_mode="1" \
  -v cal_l4="1" \
  '{ gsub(/__IMAGE__/, image);
     gsub(/__BUCKET__/, bucket);
     gsub(/__RUN_PREFIX__/, run_prefix);
     gsub(/__SVALUES__/, svalues);
     gsub(/__HOST_LABEL__/, host_label);
     gsub(/__TX_LIMIT__/, tx_limit);
     gsub(/__CAL_MODE__/, cal_mode);
     gsub(/__CAL_L4__/, cal_l4);
     print }' "${TMPL}" > "${WORK}/rendered.sh"

leftover="$(grep -v '^[[:space:]]*#' "${WORK}/rendered.sh" | grep -n '__[A-Z0-9_]*__' || true)"
if [[ -z "${leftover}" ]]; then
  report "simulated render leaves no __TOKEN__ behind" 0
else
  report "simulated render leaves no __TOKEN__ behind" 1 "${leftover}"
fi

rc=0
out="$(bash -n "${WORK}/rendered.sh" 2>&1)" || rc=$?
report "rendered startup script parses as bash (bash -n)" "${rc}" "${out}"

echo ""
echo "test-startup-render: ${pass} passed, ${fail} failed"
[[ "${fail}" -eq 0 ]]
