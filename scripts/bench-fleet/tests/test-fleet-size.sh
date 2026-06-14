#!/usr/bin/env bash
# test-fleet-size.sh -- golden test for the parametric fleet-sizing model
# (issue #95; scripts/fleet-size.py).
#
# The model consumes the MEASURED single-machine constants in
# calibration/*.json and emits machines + topology. These cases assert:
#
#   1. SELF-CONSISTENCY: the c4a-highcpu-64 @ S=9, 9000-tx central path
#      reproduces the committed single_machine_wall_9000 = 8.730 to the
#      millisecond (--self-check; the worked example / ADR-0004 §3.3).
#   2. TWO SEPARATE CLASSES: the JSON output carries worker_cells AND
#      coordinators as distinct counts and NO combined/summed machine
#      total (ADR-0004 §6.2: never sum the two pools).
#   3. SLO slack matches the committed slo_slack_min (+11.270 s) for the
#      central path -- the model agrees with the calibration registry.
#   4. COST IS NON-GATING: enabling --cost-overlay does NOT change any
#      machine count, RAM, or verdict vs the no-cost run (Discussion #77).
#   5. HONESTY: an unmeasured shape and an unmeasured S row are REFUSED
#      (non-zero exit) -- the model never invents a row.
#   6. CITATIONS PRESENT: every consumed constant cites its real artifact.
#
# Math is proven analytically against the committed calibration JSON here;
# the live distributed numbers (contention, p99 tail) are UNMODELED until
# the conductor (#75) runs -- see the model's UNMODELED ledger.

set -euo pipefail

THIS_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
REPO_ROOT="$(cd "${THIS_DIR}/../../.." && pwd)"
MODEL_PY="${REPO_ROOT}/scripts/fleet-size.py"

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

# ── 1. Self-consistency: central path == single_machine_wall_9000 ──────
rc=0
out="$(python3 "${MODEL_PY}" --self-check 2>&1)" || rc=$?
if [[ "${rc}" -eq 0 && "${out}" == *"match (to the ms): PASS"* \
      && "${out}" == *"8.730 s"* ]]; then
  report "self-check reproduces single_machine_wall_9000 = 8.730 (to the ms)" 0
else
  report "self-check reproduces single_machine_wall_9000 = 8.730 (to the ms)" 1 \
    "exit=${rc}: ${out}"
fi

# ── 2. Two SEPARATE classes, never summed ─────────────────────────────
rc=0
detail="$(python3 - "${MODEL_PY}" 2>&1 <<'PY'
import json, subprocess, sys
model = sys.argv[1]
out = subprocess.check_output(
    ["python3", model, "--json",
     "--shape", "c4a-highcpu-64", "--s", "9",
     "--blocks-per-s", "5", "--tx-per-block", "9000"]).decode()
d = json.loads(out)
sbc = d["size_by_class"]
assert "worker_cells" in sbc, "missing worker_cells class"
assert "coordinators" in sbc, "missing coordinators class"
assert sbc["worker_cells"]["count"] > 0, "worker cells must be > 0"
assert sbc["coordinators"]["count"] > 0, "coordinators must be > 0"
# NO combined machine total anywhere (the cardinal ADR-0004 §6.2 rule).
blob = json.dumps(d).lower()
for forbidden in ("total_machines", "machines_total", "combined_count",
                  "sum_machines", "total_count"):
    assert forbidden not in blob, f"found forbidden summed field: {forbidden}"
assert "_never_summed" in sbc, "missing the explicit never-summed marker"
print("two-class ok: cells=%d coords=%d (separate, not summed)"
      % (sbc["worker_cells"]["count"], sbc["coordinators"]["count"]))
PY
)" && rc=0 || rc=$?
report "two SEPARATE machine classes; no summed total (ADR-0004 §6.2)" "${rc}" "${detail}"

# ── 3. SLO slack matches the committed slo_slack_min (+11.270 s) ───────
rc=0
detail="$(python3 - "${MODEL_PY}" 2>&1 <<'PY'
import json, subprocess, sys
model = sys.argv[1]
out = subprocess.check_output(
    ["python3", model, "--json",
     "--shape", "c4a-highcpu-64", "--s", "9",
     "--blocks-per-s", "5", "--tx-per-block", "9000"]).decode()
d = json.loads(out)
slack = d["lag_readout"]["slo_slack_s"]
assert abs(slack - 11.270) <= 0.001, f"slack {slack} != committed 11.270"
assert d["lag_readout"]["verdict"] == "FEASIBLE", d["lag_readout"]["verdict"]
assert d["lag_readout"]["central_path_s"] == 8.730, d["lag_readout"]
print(f"slo slack ok: {slack:+.3f} s, FEASIBLE")
PY
)" && rc=0 || rc=$?
report "SLO slack matches committed slo_slack_min (+11.270 s, FEASIBLE)" "${rc}" "${detail}"

# ── 4. Cost overlay is NON-GATING (does not change any sizing) ─────────
rc=0
detail="$(python3 - "${MODEL_PY}" 2>&1 <<'PY'
import json, subprocess, sys
model = sys.argv[1]
base_args = ["python3", model, "--json", "--shape", "c4a-highcpu-64",
             "--s", "9", "--blocks-per-s", "5", "--tx-per-block", "9000"]
no_cost = json.loads(subprocess.check_output(base_args).decode())
with_cost = json.loads(subprocess.check_output(
    base_args + ["--cost-overlay", "3.14159"]).decode())
# Every sizing field must be byte-identical with vs without cost.
nc, wc = no_cost["size_by_class"], with_cost["size_by_class"]
assert nc["worker_cells"]["count"] == wc["worker_cells"]["count"], "cost changed cells!"
assert nc["coordinators"]["count"] == wc["coordinators"]["count"], "cost changed coords!"
assert (no_cost["lag_readout"]["verdict"]
        == with_cost["lag_readout"]["verdict"]), "cost changed verdict!"
assert (no_cost["lag_readout"]["slo_slack_s"]
        == with_cost["lag_readout"]["slo_slack_s"]), "cost changed slack!"
# The no-cost run must carry NO cost field at all.
assert "cost_overlay_reporting_only_non_gating" not in no_cost, \
    "cost present without --cost-overlay"
# The with-cost run labels it non-gating.
co = with_cost["cost_overlay_reporting_only_non_gating"]
assert "NON-GATING" in co["note"], co["note"]
print("cost non-gating ok: identical sizing with/without --cost-overlay")
PY
)" && rc=0 || rc=$?
report "cost overlay is NON-GATING (no sizing change; Discussion #77)" "${rc}" "${detail}"

# ── 5. Honesty: unmeasured shape + unmeasured S are REFUSED ────────────
rc=0
if python3 "${MODEL_PY}" --shape c4a-highcpu-999 >/dev/null 2>&1; then
  report "refuse unmeasured shape (no invented row)" 1 "accepted a bogus shape"
else
  report "refuse unmeasured shape (no invented row)" 0
fi
if python3 "${MODEL_PY}" --shape c4a-highcpu-64 --s 13 >/dev/null 2>&1; then
  report "refuse unmeasured S row (no extrapolation)" 1 "accepted an unmeasured S"
else
  report "refuse unmeasured S row (no extrapolation)" 0
fi

# ── 6. Citations present for every consumed constant ──────────────────
rc=0
detail="$(python3 - "${MODEL_PY}" 2>&1 <<'PY'
import json, subprocess, sys
model = sys.argv[1]
d = json.loads(subprocess.check_output(
    ["python3", model, "--json", "--shape", "c4a-highcpu-64", "--s", "9"]).decode())
keys = {c["key"] for c in d["citations"]}
need = {"per_s_table[S=9].l1_wall_ms", "constants.merge_s.value",
        "constants.l4_wall_s.value", "per_s_table[S=9].peak_rss_mb"}
missing = need - keys
assert not missing, f"missing citations: {missing}"
for c in d["citations"]:
    assert "c4a-highcpu-64.json" in c["source"], c["source"]
print(f"citations ok: {len(keys)} constants cited to calibration/*.json")
PY
)" && rc=0 || rc=$?
report "every consumed constant cites its real artifact" "${rc}" "${detail}"

echo
echo "Fleet-size tests: ${pass} passed, ${fail} failed."
if [[ ${fail} -gt 0 ]]; then
  exit 1
fi
