#!/usr/bin/env bash
# test-calibrate.sh -- golden-fixture test for the s-calibrate objective
# computation (issue #85).
#
# Feeds synthetic BENCH_EVENT JSONL built from issue #60's EPYC numbers
# (L1 walls ~4.7 s @ 2^17 / ~10.8 s @ 2^18 / ~21 s @ 2^19, L2 ~0.5 s --
# see the ADR-0003 context table) into scripts/s-calibrate-report.py and
# asserts:
#
#   1. calibration.tsv column order is exactly the documented schema
#   2. serial-fold recommendation reproduces #60's S=20 verdict
#   3. s/tx winner sits at the bracket top (S=20 with these fixtures)
#   4. tree-fold recommendation prefers the small bracket (S=8: shallow
#      L1 + log-depth merges beats deep L1)
#   5. bracket-edge inference settles S=9 -> 2^18 and S=21 -> 2^19 from
#      the synthetic walls
#   6. RAM-gated candidates surface as feasible=no / label=skipped rows
#   7. ledger.md follows the Discussion #77 BENCH-LEDGER template
#
# This proves the objective math analytically; the live reference-machine
# reproduction is issue #85 Phase 2.

set -euo pipefail

THIS_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
REPO_ROOT="$(cd "${THIS_DIR}/../../.." && pwd)"
REPORT_PY="${REPO_ROOT}/scripts/s-calibrate-report.py"

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

OUT_DIR="$(mktemp -d -t s-calibrate-test.XXXXXX)"
trap 'rm -rf "${OUT_DIR}"' EXIT

# ── Fixtures: synthetic probes from #60's EPYC bracket table ────────────
# S=8 (2^17 top), S=9 (edge probe; walls place it in 2^18), S=20 (2^18
# top), S=21 (edge probe; walls place it in 2^19), S=32 (2^19 top).
# Chunk 0 carries +300 ms warm-up to exercise the steady-mean exclusion.
python3 - "${OUT_DIR}" <<'PY'
import json, sys
out = sys.argv[1]

# S -> (steady L1 wall ms, L2 step ms, peak RSS MB)
FIX = {
    8:  (4700, 500, 4900),
    9:  (10500, 500, 9300),
    20: (10800, 500, 9400),
    21: (20500, 500, 16900),
    32: (21000, 500, 16800),
}
for s, (l1, l2, rss) in FIX.items():
    lines = []
    for idx in range(4):
        wall = l1 + (300 if idx == 0 else 0)
        lines.append({"event": "layer_prove", "layer": 1, "name": "BlockTxCircuit",
                      "chunk_idx": idx, "chunk_total": 4, "tx_per_proof": s,
                      "wall_ms": wall, "cpu_ms": wall * 30, "rss_mb_peak": rss,
                      "rss_mb_after": rss, "ts": "2026-06-11T00:00:00Z"})
        lines.append({"event": "layer_prove", "layer": 2, "name": "BlockTxChainCircuit",
                      "chunk_idx": idx, "chunk_total": 4, "tx_per_proof": s,
                      "wall_ms": l2, "cpu_ms": l2 * 30, "rss_mb_peak": rss,
                      "rss_mb_after": rss, "ts": "2026-06-11T00:00:00Z"})
    lines.append({"event": "summary", "tx_per_proof": s, "tx_limit": 4 * s,
                  "chunks": 4, "total_wall_ms": 4 * (l1 + l2),
                  "total_cpu_ms": None, "peak_rss_mb": rss,
                  "ts": "2026-06-11T00:00:30Z"})
    with open(f"{out}/cal-S{s}.jsonl", "w") as fh:
        for ev in lines:
            fh.write(json.dumps(ev) + "\n")
PY

# A RAM-gated candidate, as scripts/s-calibrate.sh would record it.
printf '40\tprojected ~32 GB RSS x1.5 headroom exceeds 31 GB MemTotal\n' \
  > "${OUT_DIR}/skipped.tsv"

# ── Run the report ──────────────────────────────────────────────────────
STDOUT_FILE="${OUT_DIR}/report-stdout.txt"
rc=0
python3 "${REPORT_PY}" \
  --out-dir "${OUT_DIR}" \
  --block-tx 500 \
  --merge-s 0.47 \
  --machine-label "epyc-7b13-golden" \
  --git-sha "f1x7ure" \
  > "${STDOUT_FILE}" 2>&1 || rc=$?
report "report script exits 0" "${rc}" "$(cat "${STDOUT_FILE}")"

TSV="${OUT_DIR}/calibration.tsv"

# 1. Exact column order (golden schema -- consumed by Phase 2 tooling).
# Issue #102 appended the v2 columns (l1_n.. onward) AFTER the frozen v1
# ten -- additive only; the first ten never move.
EXPECTED_HEADER=$'S\tbracket\tl1_wall_ms\tl2_wall_ms\tpeak_rss_mb\ts_per_tx\tserial_block_s\ttree_block_s\tfeasible\tlabel\tl1_n\tl1_stdev_ms\tl1_quality\tfull_split_wall_500\tfull_split_wall_4000\tfull_split_wall_9000\tslo_slack_min\tslo_verdict'
actual_header="$(head -n1 "${TSV}")"
if [[ "${actual_header}" == "${EXPECTED_HEADER}" ]]; then
  report "calibration.tsv column order" 0
else
  report "calibration.tsv column order" 1 \
    "expected: ${EXPECTED_HEADER}"$'\n'"actual:   ${actual_header}"
fi

# 2-4. Per-objective recommendations (#60 reproduction).
check_rec() {
  local name="$1" want="$2"
  if grep -q "recommend\[${name}\]: S=${want} " "${STDOUT_FILE}"; then
    report "recommendation ${name} = S=${want}" 0
  else
    report "recommendation ${name} = S=${want}" 1 \
      "$(grep "recommend\[${name}\]" "${STDOUT_FILE}" || echo 'no recommendation line')"
  fi
}
check_rec "serial"   20   # #60 verdict: serial fold optimum S=20
check_rec "s_per_tx" 20   # min s/tx at the 2^18 bracket top
check_rec "tree"     8    # L1-bound: shallow bracket + log-depth merges wins

# 5. Bracket-edge inference from measured anchors.
check_bracket() {
  local s="$1" want="$2"
  local got
  got="$(awk -F'\t' -v s="$s" '$1==s {print $2}' "${TSV}")"
  if [[ "${got}" == "${want}" ]]; then
    report "bracket edge S=${s} -> ${want}" 0
  else
    report "bracket edge S=${s} -> ${want}" 1 "got: '${got}'"
  fi
}
check_bracket 9  "2^18"
check_bracket 21 "2^19"

# 6. RAM-gated row.
skip_row="$(awk -F'\t' '$1==40 {printf "%s/%s", $9, $10}' "${TSV}")"
if [[ "${skip_row}" == "no/skipped" ]]; then
  report "RAM-gated S=40 row (feasible=no, label=skipped)" 0
else
  report "RAM-gated S=40 row (feasible=no, label=skipped)" 1 "got: '${skip_row}'"
fi

# 7. Ledger follows the Discussion #77 template.
LEDGER="${OUT_DIR}/ledger.md"
ledger_ok=0
for needle in \
  '> \*\*BENCH-LEDGER\*\*' \
  '> date / commit: .* / f1x7ure' \
  '> machine: epyc-7b13-golden' \
  '> config: calibration probe' \
  'serial_opt=S20' \
  '> evidence: issue #85' \
  '> notes: '; do
  if ! grep -qE "${needle}" "${LEDGER}"; then
    ledger_ok=1
    report "ledger.md template field: ${needle}" 1 "$(cat "${LEDGER}")"
  fi
done
if [[ "${ledger_ok}" == "0" ]]; then
  report "ledger.md matches Discussion #77 BENCH-LEDGER template" 0
fi

echo
echo "Calibrate tests: ${pass} passed, ${fail} failed."
if [[ ${fail} -gt 0 ]]; then
  exit 1
fi
