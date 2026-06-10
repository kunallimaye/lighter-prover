#!/usr/bin/env bash
# test-render.sh -- feed render-discussion.sh a synthetic fixture covering all
# 10 machine types × 4 S values + 1 missing-machine row and assert the output
# is well-formed and reasonably sized.

set -euo pipefail

THIS_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
FLEET_ROOT="$(cd "${THIS_DIR}/.." && pwd)"
RENDER="${FLEET_ROOT}/lib/render-discussion.sh"
MACHINES_TSV="${FLEET_ROOT}/machines.tsv"

WORK="$(mktemp -d)"
trap 'rm -rf "$WORK"' EXIT

TSV="${WORK}/parsed.tsv"
INFO="${WORK}/info"
mkdir -p "${INFO}"

# Header (must match the schema the renderer expects).
printf 'machine_type\tgit_sha\thost\tcpu\tcores\tram_kb\tS\tchunks\tpre_exec_ms\ttotal_tx_ms\tavg_tx_ms\ttotal_chain_ms\tavg_chain_ms\twall_ms\trss_kb\texit_code\tstatus\n' > "${TSV}"

# Generate synthetic rows for every machine_type × S∈{1,2,4,6}.
# Use slightly different fake numbers per shape/S to keep them distinguishable.
i=0
while IFS=$'\t' read -r mt vcpus arch _img _imgp _quota _zones; do
  i=$((i+1))
  for S in 1 2 4 6; do
    # Make wall vary with shape & S to give the speedup column something to do.
    wall_ms=$((100000 + i*1000 + S*10000))
    total_tx=$((250000 + S*5000))
    avg_tx=$(python3 -c "print(f'{$S*577.0:.3f}')")
    total_chain=$((250000 / S))
    avg_chain=520
    pre_exec=$((500 + i*5))
    chunks=$((480 / S))
    printf '%s\t0ae123b\tvm-%s\tFake CPU %s\t%s\t131904212\t%s\t%s\t%s\t%s\t%s\t%s\t%s\t%s\tNA\t0\tok\n' \
      "$mt" "$mt" "$mt" "$vcpus" "$S" "$chunks" \
      "$pre_exec" "$total_tx" "$avg_tx" "$total_chain" "$avg_chain" "$wall_ms" \
      >> "${TSV}"
  done

  # machine-info.txt per shape
  mkdir -p "${INFO}/${mt}"
  cat > "${INFO}/${mt}/machine-info.txt" <<EOF
=== host_label ===
${mt}

=== lscpu ===
Architecture: ${arch}
CPU(s):       ${vcpus}
Model name:   Fake CPU ${mt}

=== rustc ===
rustc 1.94.0-nightly
EOF

  # _GCS_PREFIX with two URIs to exercise the raw-logs section.
  cat > "${INFO}/${mt}/_GCS_PREFIX" <<EOF
gs://kl-ai-workstation-bench-fleet-runs/test/${mt}/bench-S4.log
gs://kl-ai-workstation-bench-fleet-runs/test/${mt}/machine-info.txt
EOF
done < <(tail -n +2 "${MACHINES_TSV}")

# Optional 11th row: a "missing machine" entry — the spec says synthetic fixture
# is "11 rows" so include a phantom shape we don't know about. The renderer
# orders by machines.tsv, so unknown shapes drop off the comparison/arch tables
# but still appear in the full sweep (sorted to the end).
for S in 1 2 4 6; do
  printf 'unknown-experimental-shape\t0ae123b\tvm-x\tExperimental\t128\t262144000\t%s\t1\t100\t100\t100\t100\t100\t%s\tNA\t0\tok\n' \
    "$S" "$((50000 + S*1000))" >> "${TSV}"
done

# -------- render --------
OUT="${WORK}/discussion.md"
bash "${RENDER}" "${TSV}" "${INFO}" "fleet test title" > "${OUT}"

bytes="$(wc -c < "${OUT}")"

pass=0
fail=0
assert() {
  local name="$1"; shift
  printf 'ASSERT %s ... ' "${name}"
  if "$@"; then
    printf 'PASS\n'
    pass=$((pass+1))
  else
    printf 'FAIL\n'
    fail=$((fail+1))
  fi
}

# Size assertions
assert "output > 5KB"  test "${bytes}" -gt 5120
assert "output < 60KB" test "${bytes}" -lt 61440

# Required headings
assert "has TL;DR heading"               grep -q '^## TL;DR'                                "${OUT}"
assert "has Methodology heading"         grep -q '^## Methodology'                          "${OUT}"
assert "has Architectures heading"       grep -q '^## Architectures swept'                  "${OUT}"
assert "has comparison heading"          grep -q '^## Cross-shape comparison at S=4'        "${OUT}"
assert "has full sweep heading"          grep -q '^## Full sweep results'                   "${OUT}"
assert "has per-machine heading"         grep -q '^## Per-machine details'                  "${OUT}"
assert "has raw logs heading"            grep -q '^## Raw logs'                             "${OUT}"
assert "has reproduction heading"        grep -q '^## Reproduction'                         "${OUT}"
assert "has caveats heading"             grep -q '^## Caveats'                              "${OUT}"

# Every machine type appears in the comparison table
while IFS=$'\t' read -r mt _rest; do
  assert "row present for ${mt}" grep -qF "| \`${mt}\` |" "${OUT}"
done < <(tail -n +2 "${MACHINES_TSV}")

# Balanced fences (must be even count)
fence_count=$(grep -c '^```' "${OUT}" || true)
assert "balanced \`\`\` fence pairs (count=${fence_count})" test $((fence_count % 2)) -eq 0

# Comparison table has 9 columns (header had 9 pipes)
header_pipes=$(grep '^| Machine type | wall_s |' "${OUT}" | head -n1 | awk -F'|' '{print NF}')
sep_pipes=$(grep -A1 '^| Machine type | wall_s |' "${OUT}" | tail -n1 | awk -F'|' '{print NF}')
assert "comparison header/separator column counts match" test "${header_pipes}" -eq "${sep_pipes}"

# Speedup column populated (× character present)
assert "speedup column has values" grep -q '×' "${OUT}"

# Caveat about S>6 present
assert "mentions issue #8 caveat" grep -q 'issue #8' "${OUT}"

echo
echo "Render output: ${bytes} bytes"
echo "Render tests: ${pass} passed, ${fail} failed."
if [[ ${fail} -gt 0 ]]; then
  echo "(rendered body saved at ${OUT})"
  # Don't delete on failure
  trap - EXIT
  exit 1
fi
