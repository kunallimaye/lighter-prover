#!/usr/bin/env bash
# test-slo-objective.sh -- golden-fixture test for the s-calibrate v2
# SLO-slack objective (objective 4, issue #102).
#
# Two fixture sets, both built from the Phase A measured numbers
# (issue #102 Phase A comment / pilot cross-check):
#
#   A. EPYC reference (measured constants MERGE_S=0.4764 s,
#      L4_WALL=5.155 s, L1 walls from the #85 reference run):
#        1. recommend[slo_slack] = S=9 with min-over-B slack ~4.63 s
#           (tolerance +-0.3 s)
#        2. S=11 verdict = MARGINAL (slack ~0.21 s at B=9000)
#        3. S in {20, 21, 32, 40} verdict = INFEASIBLE
#        4. data-quality flags: noisy (stdev/mean > 10%) and low_n
#           (< 3 steady samples) surface in the l1_quality column
#   B. c4a-like shape (extrapolated constants, r-scaled by the S=20
#      L1-wall ratio against the EPYC reference):
#        5. recommend[slo_slack] = S=9 with min slack ~12 s (+-0.5 s)
#        6. the report carries the scaled/unscaled L4 interval section
#
# The objective math is proven analytically here; live reproduction on
# cloud shapes is issue #102 Phase C.

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

EPYC_DIR="$(mktemp -d -t slo-test-epyc.XXXXXX)"
C4A_DIR="$(mktemp -d -t slo-test-c4a.XXXXXX)"
trap 'rm -rf "${EPYC_DIR}" "${C4A_DIR}"' EXIT

# ── Fixture writer: per-S jsonl from a "S l1_ms pattern" table ─────────
# pattern: steady  -> 4 chunks, chunk 0 +300 ms warm-up, rest constant
#          noisy   -> 4 chunks with >10% stdev around the mean
#          single  -> 1 chunk only (n=1 -> low_n flag)
make_jsonl() {
  local out_dir="$1" spec="$2"
  python3 - "${out_dir}" "${spec}" <<'PY'
import json, sys
out, spec = sys.argv[1], sys.argv[2]
for entry in spec.strip().splitlines():
    parts = entry.split()
    s, l1, pattern = int(parts[0]), float(parts[1]), parts[2]
    l2 = 550
    if pattern == "steady":
        walls = [l1 + 300, l1, l1, l1]
    elif pattern == "noisy":
        # stdev/mean > 0.10 across the steady samples (chunk 0 excluded)
        walls = [l1, l1 * 0.85, l1 * 1.15, l1 * 1.0]
    elif pattern == "single":
        walls = [l1]
    else:
        raise SystemExit(f"unknown pattern {pattern}")
    lines = []
    for idx, wall in enumerate(walls):
        lines.append({"event": "layer_prove", "layer": 1, "name": "BlockTxCircuit",
                      "chunk_idx": idx, "chunk_total": len(walls), "tx_per_proof": s,
                      "wall_ms": wall, "cpu_ms": wall * 30, "rss_mb_peak": 5000,
                      "rss_mb_after": 5000, "ts": "2026-06-13T00:00:00Z"})
        lines.append({"event": "layer_prove", "layer": 2, "name": "BlockTxChainCircuit",
                      "chunk_idx": idx, "chunk_total": len(walls), "tx_per_proof": s,
                      "wall_ms": l2, "cpu_ms": l2 * 30, "rss_mb_peak": 5000,
                      "rss_mb_after": 5000, "ts": "2026-06-13T00:00:00Z"})
    lines.append({"event": "summary", "tx_per_proof": s, "tx_limit": len(walls) * s,
                  "chunks": len(walls), "total_wall_ms": int(sum(walls)),
                  "total_cpu_ms": None, "peak_rss_mb": 5000,
                  "ts": "2026-06-13T00:00:30Z"})
    with open(f"{out}/cal-S{s}.jsonl", "w") as fh:
        for ev in lines:
            fh.write(json.dumps(ev) + "\n")
PY
}

# ── Fixture A: EPYC reference, Phase A steady L1 walls ─────────────────
# S=32 is written noisy and S=40 single-chunk to exercise the
# data-quality flags without disturbing the S=9 verdict.
make_jsonl "${EPYC_DIR}" "
8 5762 steady
9 5451 steady
10 6228 steady
11 9868 steady
20 12784 steady
21 22817 steady
32 31827 noisy
40 32741 single
"

EPYC_OUT="${EPYC_DIR}/stdout.txt"
rc=0
python3 "${REPORT_PY}" \
  --out-dir "${EPYC_DIR}" \
  --merge-s 0.4764 --l4-wall 5.155 \
  --merge-label measured --l4-label measured \
  --machine-label "epyc-7b13-golden" --git-sha "f1x7ure" \
  > "${EPYC_OUT}" 2>&1 || rc=$?
report "EPYC report script exits 0" "${rc}" "$(cat "${EPYC_OUT}")"

TSV="${EPYC_DIR}/calibration.tsv"

# 1. Recommendation S=9 with min slack 4.63 +- 0.3 s.
slo_line="$(grep 'recommend\[slo_slack\]' "${EPYC_OUT}" || true)"
if [[ "${slo_line}" == *"S=9 "* ]]; then
  report "EPYC slo_slack recommendation = S=9" 0
else
  report "EPYC slo_slack recommendation = S=9" 1 "${slo_line:-no recommendation line}"
fi
slack="$(printf '%s\n' "${slo_line}" | sed -n 's/.*min_slack=\([0-9.-]*\) s.*/\1/p')"
if [[ -n "${slack}" ]] && python3 -c "import sys; sys.exit(0 if abs(float('${slack}') - 4.63) <= 0.3 else 1)"; then
  report "EPYC min slack ~4.63 s (got ${slack})" 0
else
  report "EPYC min slack ~4.63 s" 1 "got: '${slack:-NA}' (line: ${slo_line})"
fi

# 2-3. Per-S verdicts from the TSV (last column = slo_verdict).
check_verdict() {
  local s="$1" want="$2"
  local got
  got="$(awk -F'\t' -v s="$s" '$1==s {print $NF}' "${TSV}")"
  if [[ "${got}" == "${want}" ]]; then
    report "EPYC S=${s} slo_verdict = ${want}" 0
  else
    report "EPYC S=${s} slo_verdict = ${want}" 1 "got: '${got}'"
  fi
}
check_verdict 9  "FEASIBLE"
check_verdict 11 "MARGINAL"
check_verdict 20 "INFEASIBLE"
check_verdict 21 "INFEASIBLE"
check_verdict 32 "INFEASIBLE"
check_verdict 40 "INFEASIBLE"

# 4. Data-quality flags (issue #102 encoding 1).
check_quality() {
  local s="$1" want="$2"
  local got
  got="$(awk -F'\t' -v s="$s" '$1==s {print $13}' "${TSV}")"
  if [[ "${got}" == *"${want}"* ]]; then
    report "EPYC S=${s} l1_quality contains '${want}'" 0
  else
    report "EPYC S=${s} l1_quality contains '${want}'" 1 "got: '${got}'"
  fi
}
check_quality 9  "ok"
check_quality 32 "noisy"
check_quality 40 "low_n"

# Objective-4 section present in report.md.
if grep -q '## Objective 4 -- SLO slack' "${EPYC_DIR}/report.md" \
   && grep -q '### Per-bracket best' "${EPYC_DIR}/report.md"; then
  report "EPYC report.md has objective-4 + per-bracket sections" 0
else
  report "EPYC report.md has objective-4 + per-bracket sections" 1 \
    "$(grep '^#' "${EPYC_DIR}/report.md")"
fi

# Ledger carries the slo_opt headline.
if grep -q 'slo_opt=S9' "${EPYC_DIR}/ledger.md"; then
  report "EPYC ledger.md headline has slo_opt=S9" 0
else
  report "EPYC ledger.md headline has slo_opt=S9" 1 "$(cat "${EPYC_DIR}/ledger.md")"
fi

# ── Fixture B: c4a-like shape, extrapolated r-scaled constants ─────────
make_jsonl "${C4A_DIR}" "
8 2921 steady
9 3008 steady
10 3130 steady
11 5431 steady
20 6466 steady
21 11828 steady
32 12888 steady
40 13848 steady
"

C4A_OUT="${C4A_DIR}/stdout.txt"
rc=0
python3 "${REPORT_PY}" \
  --out-dir "${C4A_DIR}" \
  --machine-label "c4a-golden" --git-sha "f1x7ure" \
  > "${C4A_OUT}" 2>&1 || rc=$?
report "c4a report script exits 0" "${rc}" "$(cat "${C4A_OUT}")"

# 5. Recommendation S=9 with min slack ~12 s.
slo_line="$(grep 'recommend\[slo_slack\]' "${C4A_OUT}" || true)"
if [[ "${slo_line}" == *"S=9 "* ]]; then
  report "c4a slo_slack recommendation = S=9" 0
else
  report "c4a slo_slack recommendation = S=9" 1 "${slo_line:-no recommendation line}"
fi
slack="$(printf '%s\n' "${slo_line}" | sed -n 's/.*min_slack=\([0-9.-]*\) s.*/\1/p')"
if [[ -n "${slack}" ]] && python3 -c "import sys; sys.exit(0 if abs(float('${slack}') - 12.0) <= 0.5 else 1)"; then
  report "c4a min slack ~12 s (got ${slack})" 0
else
  report "c4a min slack ~12 s" 1 "got: '${slack:-NA}' (line: ${slo_line})"
fi

# 6. Scaled/unscaled L4 interval for extrapolated shapes.
if grep -q '### Extrapolation interval' "${C4A_DIR}/report.md" \
   && grep -q 'unscaled (conservative' "${C4A_DIR}/report.md"; then
  report "c4a report.md carries the scaled/unscaled L4 interval" 0
else
  report "c4a report.md carries the scaled/unscaled L4 interval" 1 \
    "$(grep -A3 'Extrapolation' "${C4A_DIR}/report.md" || echo 'section missing')"
fi

echo
echo "SLO-objective tests: ${pass} passed, ${fail} failed."
if [[ ${fail} -gt 0 ]]; then
  exit 1
fi
