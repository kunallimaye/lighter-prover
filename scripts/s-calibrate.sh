#!/usr/bin/env bash
# s-calibrate.sh -- per-machine chunk-size calibration suite
# (issue #85; SLO objective + calibration registry: issue #102).
#
# Probes only the degree-bracket TOPS (the candidate set from issue #60's
# step-function finding: per-tx cost is minimized at the top of each
# power-of-two degree bracket), RAM-gates infeasible brackets, runs the
# 4-chunk methodology per surviving S (--tx-limit 4*S), parses the
# BENCH_EVENT JSONL the bench emits, and computes the optimal S per
# objective (serial fold / tree fold / s-per-tx / SLO slack) via
# scripts/s-calibrate-report.py.
#
# Outputs (under $OUT_DIR):
#   machine-info.txt   lscpu / free -h / uname -a / host label / loadavg
#   cal-S<N>.log       full bench stdout+stderr per candidate S
#   cal-S<N>.jsonl     extracted BENCH_EVENT lines per candidate S
#   cal-l4check.*      opt-in CAL_L4=1 merge/L4 measurement probe
#   skipped.tsv        RAM-gated candidates (S <TAB> reason)
#   calibration.tsv    per-S metrics + objectives (machine-parseable)
#   report.md          human-readable summary, per-objective recommendations
#   ledger.md          BENCH-LEDGER entry (Discussion #77 template)
# Plus, when OUT_REGISTRY=1 (issue #102):
#   calibration/<shape>.json + README.md in the repo working tree --
#   the committed calibration registry (recalibration = a PR that diffs it).
#
# Knobs (override at make-invoke time, e.g. `make s-calibrate CAL_SVALUES="20 21"`):
#   CAL_SVALUES   Candidate S list (default: auto -- bracket tops + edge
#                 probes "8 9 10 11 20 21 32", plus 40 when RAM clears the
#                 2^20 gate)
#   BLOCK_TX      Block size for the objective-1..3 math (default: 500)
#   MERGE_S       Tree-merge step constant in seconds (default: 0.4764,
#                 Phase A measured, S-independent -- issue #102; used when
#                 a run has no measured merge events)
#   L4_WALL       L4 prove+verify wall in seconds (default: 5.155, Phase A
#                 measured on the EPYC reference; the one-time L4 build is
#                 resident and NOT part of the per-block wall)
#   LAG_P50       Proof-lag SLO p50 budget in s (default: 20, Discussion #77)
#   LAG_P99       Proof-lag SLO p99 budget in s (default: 40, context only)
#   BLOCK_SIZES   Block sizes B for objective 4 (default: "500 4000 9000")
#   CAL_L4        Set 1 to run the opt-in per-machine MERGE_S/L4 measurement
#                 (one --l2-fold tree --l4-check probe at S=4/tx-limit 32);
#                 constants then carry label=measured (default: 0)
#   OUT_REGISTRY  Set 1 to emit calibration/<shape>.json + README.md into
#                 the repo working tree (default: 0)
#   SHAPE_LABEL   Registry shape name (default: HOST_LABEL)
#   CHUNKS        Chunks per probe run (default: 4 -- the #60 methodology)
#   OUT_DIR       Output directory (default: /tmp/s-calibrate.<timestamp>)
#   HEADROOM      RAM headroom multiplier for the gate (default: 1.5)
#
# ADR-0003 §D4 note: this suite is fully independent of the historical
# comparison fleet (S in {1,2,4,6}); it never touches machines.tsv or the
# fleet defaults.

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
# shellcheck disable=SC1091
source "${SCRIPT_DIR}/common.sh"
start_log "s-calibrate"

BLOCK_TX="${BLOCK_TX:-500}"
MERGE_S="${MERGE_S:-0.4764}"
L4_WALL="${L4_WALL:-5.155}"
LAG_P50="${LAG_P50:-20}"
LAG_P99="${LAG_P99:-40}"
BLOCK_SIZES="${BLOCK_SIZES:-500 4000 9000}"
CAL_L4="${CAL_L4:-0}"
OUT_REGISTRY="${OUT_REGISTRY:-0}"
CHUNKS="${CHUNKS:-4}"
HEADROOM="${HEADROOM:-1.5}"
OUT_DIR="${OUT_DIR:-/tmp/s-calibrate.$(date +%Y%m%d-%H%M%S)}"
TARGET_CPU_NATIVE="${TARGET_CPU_NATIVE:-0}"

require_cmd python3

# ─── Helpers ──────────────────────────────────────────────────────────

# Resolve the bench binary, building it if missing or not runnable here.
# Mirrors scripts/stream.sh ensure_bench_bin(): existence alone is not
# enough -- a foreign-architecture bench/bench artifact passes [[ -x ]]
# yet dies with "Exec format error" the moment it is exec'd (#56).
ensure_bench_bin() {
  if [[ -x "${PROJECT_ROOT}/bench/bench" ]] \
     && "${PROJECT_ROOT}/bench/bench" --help >/dev/null 2>&1; then
    BENCH_BIN="${PROJECT_ROOT}/bench/bench"
    return 0
  fi
  if [[ -e "${PROJECT_ROOT}/bench/bench" ]]; then
    log_warn "bench binary missing or wrong architecture -- rebuilding (release)..."
  else
    log_info "bench binary not found at bench/bench; building (release)..."
  fi
  require_cmd cargo
  local rustflags=""
  [[ "${TARGET_CPU_NATIVE}" == "1" ]] && rustflags="-C target-cpu=native"
  (cd "${PROJECT_ROOT}" && RUSTFLAGS="${rustflags}" cargo build --release -p bench --bin bench)
  BENCH_BIN="${PROJECT_ROOT}/target/release/bench"
}

# Projected peak RSS in GB for a candidate S, from issue #60's bracket
# table. Edge-probe values (9..11, 21) are gated against the HIGHER of
# the two brackets they might land in -- the probe's whole purpose is to
# find the edge, so the gate must assume the expensive outcome.
#   2^17 (S<=8):    ~5 GB        2^18 (S 12..20): ~10 GB
#   2^19 (S 22..32): ~17 GB      2^20 (S>32):     ~32 GB (projected)
projected_rss_gb() {
  local s="$1"
  if   (( s <= 8 ));  then echo 5
  elif (( s <= 20 )); then echo 10   # 9..11 edge probes: assume 2^18
  elif (( s <= 32 )); then echo 17   # 21 edge probe: assume 2^19
  else                     echo 32   # 2^20 bracket, unmeasured projection
  fi
}

mem_total_gb() {
  awk '/^MemTotal:/ {printf "%d\n", $2 / 1048576}' /proc/meminfo
}

# ─── 1. Machine info capture ─────────────────────────────────────────
# Mirrors the fleet's machine-info.txt collection (vm-startup.sh.tmpl §3)
# so cross-machine reports have identical provenance sections.

mkdir -p "${OUT_DIR}"
HOST_LABEL="${HOST_LABEL:-$(hostname -s 2>/dev/null || echo unknown)}"

# Load-quality flag (issue #102 encoding 5): capture the 1-min loadavg +
# core count AT RUN START. Walls measured on an already-loaded machine
# are inflated (~10-20% observed in Phase A), so near-zero-slack SLO
# verdicts from a "loaded" run are flagged as unreliable downstream.
LOAD1="$(cut -d' ' -f1 /proc/loadavg 2>/dev/null || echo 0)"
CORES="$(nproc 2>/dev/null || echo 1)"
LOAD_QUALITY="$(python3 -c "print('loaded' if ${LOAD1} / max(${CORES}, 1) > 0.2 else 'clean')" 2>/dev/null || echo unknown)"

{
  echo "=== host_label ==="
  echo "${HOST_LABEL}"
  echo
  echo "=== lscpu ==="
  lscpu
  echo
  echo "=== free -h ==="
  free -h
  echo
  echo "=== uname -a ==="
  uname -a
  echo
  echo "=== loadavg ==="
  echo "loadavg_1min: ${LOAD1}"
  echo "cores: ${CORES}"
  echo "load_quality: ${LOAD_QUALITY}"
} > "${OUT_DIR}/machine-info.txt"
log_ok "machine info captured -> ${OUT_DIR}/machine-info.txt (load_quality=${LOAD_QUALITY})"

MEM_GB="$(mem_total_gb)"
log_info "MemTotal: ${MEM_GB} GB (headroom x${HEADROOM})"

# ─── 2. Candidate set + RAM gate ─────────────────────────────────────

if [[ -z "${CAL_SVALUES:-}" ]]; then
  CAL_SVALUES="8 9 10 11 20 21 32"
  # Probe the 2^20 bracket only when RAM clears the projected 32 GB
  # with headroom -- first machines able to do this are c4a-highmem-*.
  if python3 -c "import sys; sys.exit(0 if ${MEM_GB} >= 32 * ${HEADROOM} else 1)"; then
    CAL_SVALUES="${CAL_SVALUES} 40"
    log_info "RAM clears the 2^20 gate -- adding S=40 to the candidate set"
  fi
fi

for s in ${CAL_SVALUES}; do
  [[ "${s}" =~ ^[0-9]+$ && "${s}" != "0" ]] \
    || die "CAL_SVALUES must be space-separated positive integers (got: '${s}')"
done

: > "${OUT_DIR}/skipped.tsv"
SURVIVORS=""
for s in ${CAL_SVALUES}; do
  proj="$(projected_rss_gb "${s}")"
  if python3 -c "import sys; sys.exit(0 if ${MEM_GB} >= ${proj} * ${HEADROOM} else 1)"; then
    SURVIVORS="${SURVIVORS} ${s}"
  else
    log_warn "skipping S=${s}: projected ~${proj} GB peak RSS x${HEADROOM} headroom exceeds ${MEM_GB} GB MemTotal"
    printf '%s\tprojected ~%s GB RSS x%s headroom exceeds %s GB MemTotal\n' \
      "${s}" "${proj}" "${HEADROOM}" "${MEM_GB}" >> "${OUT_DIR}/skipped.tsv"
  fi
done
SURVIVORS="${SURVIVORS# }"
[[ -n "${SURVIVORS}" ]] || die "every candidate S was RAM-gated out (MemTotal=${MEM_GB} GB); nothing to probe"
log_info "candidate S after RAM gate: ${SURVIVORS}"

# ─── 3. Probe runs ───────────────────────────────────────────────────
# Per candidate S: CHUNKS chunks at --tx-limit CHUNKS*S (the issue #60
# methodology; minutes each, circuit build dominates). The bench loads
# ./bench_test.json relative to CWD, so run from bench/.

ensure_bench_bin
FAILED=0
for s in ${SURVIVORS}; do
  tx_limit=$((CHUNKS * s))
  log_info "probe S=${s}: ${CHUNKS} chunks at --tx-limit ${tx_limit}"
  rc=0
  (
    cd "${PROJECT_ROOT}/bench" && \
      LIGHTER_TX_PER_PROOF="${s}" \
      LIGHTER_TX_LIMIT="${tx_limit}" \
      RUST_LOG=info \
      "${BENCH_BIN}"
  ) > "${OUT_DIR}/cal-S${s}.log" 2>&1 || rc=$?
  grep '^BENCH_EVENT ' "${OUT_DIR}/cal-S${s}.log" \
    | sed 's/^BENCH_EVENT //' > "${OUT_DIR}/cal-S${s}.jsonl" || true
  if (( rc != 0 )); then
    log_warn "probe S=${s} exited rc=${rc} (see ${OUT_DIR}/cal-S${s}.log); partial events kept"
    FAILED=$((FAILED + 1))
  else
    log_ok "probe S=${s} done ($(wc -l < "${OUT_DIR}/cal-S${s}.jsonl") events)"
  fi
done

# ─── 3b. Opt-in per-machine MERGE_S / L4 measurement (issue #102) ────
# CAL_L4=1 runs ONE --l2-fold tree --l4-check probe (S=4, tx-limit 32:
# 8 leaves -> 7 pairwise merges + a full L4 prove+verify) so this
# machine's objective-4 constants are MEASURED instead of extrapolated
# from the Phase A reference. The report script auto-detects
# cal-l4check.jsonl and labels the constants accordingly.

if [[ "${CAL_L4}" == "1" ]]; then
  CAL_L4_S="${CAL_L4_S:-4}"
  cal_l4_limit=$((CAL_L4_S * 8))
  log_info "CAL_L4: measuring MERGE_S/L4_WALL (--l2-fold tree --l4-check, S=${CAL_L4_S}, tx-limit ${cal_l4_limit})"
  rc=0
  (
    cd "${PROJECT_ROOT}/bench" && \
      RUST_LOG=info \
      "${BENCH_BIN}" \
        --tx-per-proof "${CAL_L4_S}" \
        --tx-limit "${cal_l4_limit}" \
        --l2-fold tree \
        --l4-check
  ) > "${OUT_DIR}/cal-l4check.log" 2>&1 || rc=$?
  grep '^BENCH_EVENT ' "${OUT_DIR}/cal-l4check.log" \
    | sed 's/^BENCH_EVENT //' > "${OUT_DIR}/cal-l4check.jsonl" || true
  if (( rc != 0 )); then
    log_warn "CAL_L4 probe exited rc=${rc} (see ${OUT_DIR}/cal-l4check.log) -- falling back to extrapolated constants"
    rm -f "${OUT_DIR}/cal-l4check.jsonl"
  else
    log_ok "CAL_L4 probe done ($(wc -l < "${OUT_DIR}/cal-l4check.jsonl") events) -- constants will carry label=measured"
  fi
fi

# ─── 4. Objectives + report ──────────────────────────────────────────

CIRCUIT_HASH="$(circuit_src_hash)"
SHAPE_LABEL="${SHAPE_LABEL:-${HOST_LABEL}}"
registry_args=()
if [[ "${OUT_REGISTRY}" == "1" ]]; then
  registry_args+=(--out-registry "${PROJECT_ROOT}/calibration" --shape-label "${SHAPE_LABEL}")
  log_info "registry emission enabled -> calibration/${SHAPE_LABEL}.json"
fi

log_info "computing objectives (BLOCK_TX=${BLOCK_TX}, MERGE_S=${MERGE_S}, L4_WALL=${L4_WALL}, LAG_P50=${LAG_P50}, B={${BLOCK_SIZES}})"
python3 "${SCRIPT_DIR}/s-calibrate-report.py" \
  --out-dir "${OUT_DIR}" \
  --block-tx "${BLOCK_TX}" \
  --merge-s "${MERGE_S}" \
  --l4-wall "${L4_WALL}" \
  --lag-p50 "${LAG_P50}" \
  --lag-p99 "${LAG_P99}" \
  --block-sizes "${BLOCK_SIZES}" \
  --load-quality "${LOAD_QUALITY}" \
  --circuit-hash "${CIRCUIT_HASH}" \
  --machine-label "${HOST_LABEL}" \
  "${registry_args[@]}" \
  || die "report generation failed"

log_ok "calibration complete -> ${OUT_DIR}/calibration.tsv, report.md, ledger.md"
if (( FAILED > 0 )); then
  log_warn "${FAILED} probe run(s) exited non-zero -- their rows carry label=failed in the TSV"
  exit 1
fi
