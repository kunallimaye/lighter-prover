#!/usr/bin/env bash
# test-parser.sh -- exercise parse-bench-log.sh against the canonical S=4
# fixture (sliced from Discussion #6) and diff against expected output.

set -euo pipefail

THIS_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
FLEET_ROOT="$(cd "${THIS_DIR}/.." && pwd)"

PARSER="${FLEET_ROOT}/lib/parse-bench-log.sh"
FIXTURE="${THIS_DIR}/fixtures/bench-S4-sample.log"
EXPECTED="${THIS_DIR}/fixtures/expected-parsed.tsv"

pass=0
fail=0

run_case() {
  local name="$1" fixture="$2" expected="$3"
  printf 'CASE %s ... ' "$name"
  local actual
  actual="$(bash "${PARSER}" "${fixture}")"
  if diff -u <(printf '%s\n' "${actual}") "${expected}" >/tmp/test-parser.diff 2>&1; then
    printf 'PASS\n'
    pass=$((pass+1))
  else
    printf 'FAIL\n'
    fail=$((fail+1))
    sed 's/^/    /' /tmp/test-parser.diff
  fi
  rm -f /tmp/test-parser.diff
}

run_case "S=4 from Discussion #6" "${FIXTURE}" "${EXPECTED}"

# Current-main format (issue #21): BENCH_EVENT JSONL interleaved with the
# legacy INFO lines, captured from a real local run of bench@main with the
# startup-wrapper S4_WALL_SECONDS / S4_EXIT_CODE lines appended. Exercises
# the BENCH_EVENT summary fallback (rss_kb from peak_rss_mb).
run_case "S=4 current-main BENCH_EVENT format" \
  "${THIS_DIR}/fixtures/bench-S4-current-main.log" \
  "${THIS_DIR}/fixtures/expected-parsed-current-main.tsv"

# Synthetic panic case: log with only BENCH_META + panic marker.
PANIC_FIX="$(mktemp)"
trap 'rm -f "${PANIC_FIX}"' EXIT
cat > "${PANIC_FIX}" <<'PANIC_EOF'
[2026-06-10T03:11:42Z INFO  bench] BENCH_META host=vm1 cpu="Ampere Altra" cores=32 ram=131904212 kB git_sha=0ae123b tx_per_proof=8 tx_limit=480
[2026-06-10T03:12:32Z INFO  bench] BlockPreExecutionCircuit defined!

thread 'main' (15402) panicked at /home/user/.cargo/git/checkouts/plonky2-73cf2a4074a1e1be/e1c2d35/plonky2/src/plonk/circuit_builder.rs:1072:13:
Failed to build circuit
note: run with `RUST_BACKTRACE=1` environment variable to display a backtrace
S8_WALL_SECONDS=18
S8_EXIT_CODE=101
PANIC_EOF

EXPECTED_PANIC=$'0ae123b\tvm1\tAmpere Altra\t32\t131904212\t8\tNA\tNA\tNA\tNA\tNA\tNA\t18000\tNA\tNA\tNA\t101\tpanic'
actual_panic="$(bash "${PARSER}" "${PANIC_FIX}")"
printf 'CASE panic detection ... '
if [[ "${actual_panic}" == "${EXPECTED_PANIC}" ]]; then
  printf 'PASS\n'
  pass=$((pass+1))
else
  printf 'FAIL\n'
  printf '    expected: %s\n' "${EXPECTED_PANIC}"
  printf '    actual:   %s\n' "${actual_panic}"
  fail=$((fail+1))
fi

# Synthetic timeout case
TIMEOUT_FIX="$(mktemp)"
trap 'rm -f "${PANIC_FIX}" "${TIMEOUT_FIX}"' EXIT
cat > "${TIMEOUT_FIX}" <<'TIMEOUT_EOF'
[2026-06-10T04:00:00Z INFO  bench] BENCH_META host=vm2 cpu="Axion" cores=32 ram=65536000 kB git_sha=0ae123b tx_per_proof=6 tx_limit=480
[2026-06-10T04:00:00Z INFO  bench] there will be 80 iterations of proving.
S6_WALL_SECONDS=14400
S6_EXIT_CODE=124
TIMEOUT_EOF

# ms_per_tx/tx_per_sec are computed even for timeout rows (chunks=80 × S=6 =
# 480 tx over the capped wall) — status communicates the cap, data stays data.
EXPECTED_TIMEOUT=$'0ae123b\tvm2\tAxion\t32\t65536000\t6\t80\tNA\tNA\tNA\tNA\tNA\t14400000\t30000.000\t0.033\tNA\t124\ttimeout'
actual_timeout="$(bash "${PARSER}" "${TIMEOUT_FIX}")"
printf 'CASE timeout detection ... '
if [[ "${actual_timeout}" == "${EXPECTED_TIMEOUT}" ]]; then
  printf 'PASS\n'
  pass=$((pass+1))
else
  printf 'FAIL\n'
  printf '    expected: %s\n' "${EXPECTED_TIMEOUT}"
  printf '    actual:   %s\n' "${actual_timeout}"
  fail=$((fail+1))
fi

echo
echo "Parser tests: ${pass} passed, ${fail} failed."
if [[ ${fail} -gt 0 ]]; then
  exit 1
fi
