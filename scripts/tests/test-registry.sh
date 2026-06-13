#!/usr/bin/env bash
# test-registry.sh -- calibration registry round-trip + staleness-guard
# test (issue #102).
#
# Round-trip: emit a registry entry from fixture probe data
# (s-calibrate-report.py --out-registry), assert the JSON schema +
# rendered README, then run scripts/calibration-check.sh against it:
#
#   1. --print-hash yields a stable, non-empty circuit hash
#   2. registry JSON carries the required fields (shape, circuit_hash,
#      constants with labels, objectives incl. slo_slack, per_s_table)
#   3. README.md is rendered with the purpose preamble + the shape row
#   4. calibration-check reports OK when circuit_hash matches the tree
#   5. after the circuit hash diverges (the committed-entry-predates-
#      circuit-change scenario, simulated by rewriting the stored hash),
#      calibration-check WARNS, names the stale shape, and still exits 0
#   6. an empty registry dir is a no-op (exit 0, no warning)

set -euo pipefail

THIS_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
REPO_ROOT="$(cd "${THIS_DIR}/../.." && pwd)"
REPORT_PY="${REPO_ROOT}/scripts/s-calibrate-report.py"
CHECK_SH="${REPO_ROOT}/scripts/calibration-check.sh"

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

WORK="$(mktemp -d -t registry-test.XXXXXX)"
trap 'rm -rf "${WORK}"' EXIT
PROBE_DIR="${WORK}/probes"
REG_DIR="${WORK}/registry"
mkdir -p "${PROBE_DIR}" "${REG_DIR}"

# ── 1. Current circuit hash ────────────────────────────────────────────
hash1="$(bash "${CHECK_SH}" --print-hash)"
hash2="$(bash "${CHECK_SH}" --print-hash)"
if [[ -n "${hash1}" && "${hash1}" != "unknown" && "${hash1}" == "${hash2}" ]]; then
  report "--print-hash is non-empty and stable" 0
else
  report "--print-hash is non-empty and stable" 1 "got: '${hash1}' / '${hash2}'"
fi

# ── Fixture probes (EPYC Phase A walls, steady) ────────────────────────
python3 - "${PROBE_DIR}" <<'PY'
import json, sys
out = sys.argv[1]
FIX = {8: 5762, 9: 5451, 10: 6228, 11: 9868, 20: 12784}
for s, l1 in FIX.items():
    lines = []
    for idx in range(4):
        wall = l1 + (300 if idx == 0 else 0)
        lines.append({"event": "layer_prove", "layer": 1, "name": "BlockTxCircuit",
                      "chunk_idx": idx, "chunk_total": 4, "tx_per_proof": s,
                      "wall_ms": wall, "cpu_ms": wall * 30, "rss_mb_peak": 5000,
                      "rss_mb_after": 5000, "ts": "2026-06-13T00:00:00Z"})
    lines.append({"event": "summary", "tx_per_proof": s, "tx_limit": 4 * s,
                  "chunks": 4, "total_wall_ms": 4 * l1, "total_cpu_ms": None,
                  "peak_rss_mb": 5000, "ts": "2026-06-13T00:00:30Z"})
    with open(f"{out}/cal-S{s}.jsonl", "w") as fh:
        for ev in lines:
            fh.write(json.dumps(ev) + "\n")
PY

# ── 2-3. Emit registry entry + README ──────────────────────────────────
rc=0
out="$(python3 "${REPORT_PY}" \
  --out-dir "${PROBE_DIR}" \
  --merge-s 0.4764 --l4-wall 5.155 \
  --merge-label measured --l4-label measured \
  --machine-label "registry-golden" --shape-label "test-shape" \
  --git-sha "f1x7ure" --circuit-hash "${hash1}" \
  --load-quality clean --date 2026-06-13 \
  --out-registry "${REG_DIR}" 2>&1)" || rc=$?
report "report --out-registry exits 0" "${rc}" "${out}"

JSON="${REG_DIR}/test-shape.json"
rc=0
detail="$(python3 - "${JSON}" "${hash1}" 2>&1 <<'PY'
import json, sys
e = json.load(open(sys.argv[1]))
assert e["shape"] == "test-shape", e["shape"]
assert e["circuit_hash"] == sys.argv[2], e["circuit_hash"]
assert e["measured_at_sha"] == "f1x7ure"
assert e["date"] == "2026-06-13"
assert e["load_quality"] == "clean"
assert e["constants"]["merge_s"]["label"] == "measured"
assert e["constants"]["merge_s"]["value"] == 0.4764
assert e["constants"]["l4_wall_s"]["label"] == "measured"
assert e["objectives"]["slo_slack"]["s"] == 9, e["objectives"]["slo_slack"]
assert abs(e["objectives"]["slo_slack"]["min_slack"] - 4.63) <= 0.3
assert e["objectives"]["slo_slack"]["lag_p50"] == 20.0
assert e["objectives"]["s_per_tx"]["s"] == 9
assert len(e["per_s_table"]) == 5
assert e["brackets"], "brackets table empty"
print("schema ok")
PY
)" || rc=$?
report "registry JSON schema + S=9 verdict" "${rc}" "${detail}"

README="${REG_DIR}/README.md"
if grep -q '# Calibration registry' "${README}" \
   && grep -q 'The five questions this suite answers' "${README}" \
   && grep -q 'test-shape' "${README}"; then
  report "README.md rendered (preamble + shape row)" 0
else
  report "README.md rendered (preamble + shape row)" 1 "$(head -30 "${README}" 2>&1)"
fi

# ── 4. calibration-check OK when hashes match ──────────────────────────
rc=0
out="$(REGISTRY_DIR="${REG_DIR}" bash "${CHECK_SH}" 2>&1)" || rc=$?
if [[ "${rc}" -eq 0 && "${out}" == *"calibration-check: OK"* ]]; then
  report "calibration-check OK on fresh registry (exit 0)" 0
else
  report "calibration-check OK on fresh registry (exit 0)" 1 "exit=${rc}: ${out}"
fi

# ── 5. Staleness: stored hash no longer matches the tree ──────────────
# Equivalent to a circuit/src edit after calibration: the entry's
# circuit_hash diverges from the recomputed working-tree hash.
python3 - "${JSON}" <<'PY'
import json, sys
p = sys.argv[1]
e = json.load(open(p))
e["circuit_hash"] = "deadbeef-stale-circuit-hash"
json.dump(e, open(p, "w"), indent=2)
PY
rc=0
out="$(REGISTRY_DIR="${REG_DIR}" bash "${CHECK_SH}" 2>&1)" || rc=$?
if [[ "${rc}" -eq 0 && "${out}" == *"WARNING"* && "${out}" == *"test-shape.json"* ]]; then
  report "calibration-check WARNS on stale entry, still exits 0" 0
else
  report "calibration-check WARNS on stale entry, still exits 0" 1 "exit=${rc}: ${out}"
fi

# ── 6. Empty registry dir is a clean no-op ─────────────────────────────
EMPTY="${WORK}/empty"
mkdir -p "${EMPTY}"
rc=0
out="$(REGISTRY_DIR="${EMPTY}" bash "${CHECK_SH}" 2>&1)" || rc=$?
if [[ "${rc}" -eq 0 && "${out}" == *"nothing to check"* ]]; then
  report "empty registry dir: no-op, exit 0" 0
else
  report "empty registry dir: no-op, exit 0" 1 "exit=${rc}: ${out}"
fi

echo
echo "Registry tests: ${pass} passed, ${fail} failed."
if [[ ${fail} -gt 0 ]]; then
  exit 1
fi
