#!/usr/bin/env bash
# tx-mix Cloud Run Job entrypoint (issue #128).
#
# Runs the hardened `tx-mix` capture (bench/feeder/feeder.py, merged in
# PR #152) from Tokyo egress and lands its results DURABLY in GCS. A
# Cloud Run Job's filesystem evaporates on exit, so writing to GCS is
# MANDATORY — nothing of value may live only in the ephemeral container.
#
# WHY a JOB (not a Service): the capture runs to completion and exits;
# a rate-limited representative window may take HOURS. Cloud Run Jobs
# support long task timeouts and run-to-completion semantics.
#
# The SAME image runs a tiny SMOKE window AND a big REPRESENTATIVE window
# purely by env config — no redefinition. The operator picks the window
# by setting TXMIX_HEIGHTS or TXMIX_BLOCKS; everything else is identical.
#
# ── Egress honesty (issue #128 / PR #152 / measurement-citation norm) ──
# The zklighter blockTxs API geo-blocks some regions (observed: US) with
# HTTP 403 and is the ONLY HTTP source exposing per-tx tx_type. This job
# is meant to run from asia-northeast1 (Tokyo), which is normally NOT
# geo-blocked. If it 403s anyway, the tool hard-fails (exit 2) with crisp
# Tokyo guidance and this entrypoint records that finding to GCS rather
# than fabricating a mix. A captured mix is NEVER presented without its
# provenance (region / endpoint / window / N / rate).
set -euo pipefail

# ── Required config ────────────────────────────────────────────────────
#   TXMIX_BUCKET    GCS bucket for durable output (no gs:// prefix). REQUIRED.
# ── Window selection (exactly one is used; HEIGHTS wins if both set) ────
#   TXMIX_HEIGHTS   "LO HI" inclusive height range (e.g. "1000 1005").
#   TXMIX_BLOCKS    N most-recent blocks (default 200; ignored if HEIGHTS set).
# ── Rate / region / egress (all forwarded to the tool's real flags) ────
#   TXMIX_MAX_RPM   --max-rpm (default 80, under the 90/min per-IP cap).
#   TXMIX_REGION    --region citation label (default asia-northeast1).
#   TXMIX_BASE_URL  --base-url override (env LIGHTER_TXMIX_BASE_URL also works).
#   TXMIX_PROXY     --proxy override (env LIGHTER_EGRESS_PROXY also works).
#   TXMIX_PAGE_LIMIT --page-limit (default 100).
# ── Output path ────────────────────────────────────────────────────────
#   TXMIX_PREFIX    GCS object prefix (default txmix/<UTC-ts>-<short-id>).
#   TXMIX_LABEL     freeform run label folded into the prefix/summary
#                   (e.g. "smoke" or "peak-2026-06-14"); default "run".

TXMIX_BUCKET="${TXMIX_BUCKET:-}"
TXMIX_HEIGHTS="${TXMIX_HEIGHTS:-}"
TXMIX_BLOCKS="${TXMIX_BLOCKS:-200}"
TXMIX_MAX_RPM="${TXMIX_MAX_RPM:-80}"
TXMIX_REGION="${TXMIX_REGION:-asia-northeast1}"
TXMIX_BASE_URL="${TXMIX_BASE_URL:-}"
TXMIX_PROXY="${TXMIX_PROXY:-}"
TXMIX_PAGE_LIMIT="${TXMIX_PAGE_LIMIT:-100}"
TXMIX_LABEL="${TXMIX_LABEL:-run}"

if [[ -z "${TXMIX_BUCKET}" ]]; then
  echo "FATAL: TXMIX_BUCKET is required (durable GCS output is mandatory for a Job)." >&2
  exit 64
fi

TS="$(date -u +%Y%m%dT%H%M%SZ)"
SHORT_ID="$(tr -dc 'a-z0-9' </dev/urandom 2>/dev/null | head -c6 || echo nohash)"
TXMIX_PREFIX="${TXMIX_PREFIX:-txmix/${TS}-${TXMIX_LABEL}-${SHORT_ID}}"
PREFIX="${TXMIX_PREFIX%/}"
DEST="gs://${TXMIX_BUCKET}/${PREFIX}/"

# Local staging (ephemeral — uploaded before exit).
RAW_PATH="/tmp/tx-mix.txt"          # the rendered mix table (human + cite)
META_PATH="/tmp/tx-mix.meta.json"   # machine-readable provenance + result
DONE_PATH="/tmp/DONE"

# Determine my egress IP (provenance hygiene — proves WHERE we egressed).
EGRESS_IP="$(curl -s --max-time 10 https://ipinfo.io/ip 2>/dev/null || echo unknown)"
EGRESS_GEO="$(curl -s --max-time 10 https://ipinfo.io/json 2>/dev/null \
  | tr -d '\n' | sed -E 's/.*"country"[: ]*"([^"]*)".*/\1/' || echo unknown)"

echo "### TXMIX_RUN_START ts=${TS} region=${TXMIX_REGION} egress_ip=${EGRESS_IP} egress_country=${EGRESS_GEO} dest=${DEST}"

# ── Build the tx-mix argv from config (real, verified flags) ───────────
ARGS=(tx-mix --max-rpm "${TXMIX_MAX_RPM}" --page-limit "${TXMIX_PAGE_LIMIT}" \
      --region "${TXMIX_REGION}")
if [[ -n "${TXMIX_HEIGHTS}" ]]; then
  # shellcheck disable=SC2206  # word-split "LO HI" intentionally
  HL=(${TXMIX_HEIGHTS})
  ARGS+=(--heights "${HL[0]}" "${HL[1]}")
  WINDOW_DESC="heights ${HL[0]}-${HL[1]}"
else
  ARGS+=(--blocks "${TXMIX_BLOCKS}")
  WINDOW_DESC="${TXMIX_BLOCKS} most-recent blocks"
fi
[[ -n "${TXMIX_BASE_URL}" ]] && ARGS+=(--base-url "${TXMIX_BASE_URL}")
[[ -n "${TXMIX_PROXY}" ]] && ARGS+=(--proxy "${TXMIX_PROXY}")

echo "### TXMIX_CMD feeder.py ${ARGS[*]}"

# ── Run the capture. Disable set -e so we can capture the exit code and
#    STILL upload (even a 403 finding must land in GCS, not vanish). ─────
set +e
python3 /app/feeder.py "${ARGS[@]}" >"${RAW_PATH}" 2>/tmp/tx-mix.err
RC=$?
set -e

# Tee stderr into the artifact too (the 403 guidance lives on stderr).
cat /tmp/tx-mix.err >&2 || true

ENDPOINT="${TXMIX_BASE_URL:-https://mainnet.zklighter.elliot.ai/api/v1/blockTxs}"

# Detect a geo-block (tool exit 2 == 403 hard-fail).
OUTCOME="success"
if [[ "${RC}" -eq 2 ]]; then
  OUTCOME="geo_blocked_403"
elif [[ "${RC}" -ne 0 ]]; then
  OUTCOME="error_rc_${RC}"
fi

# ── Machine-readable provenance + result (measurement-citation norm) ───
{
  printf '{\n'
  printf '  "issue": 128,\n'
  printf '  "tool": "bench/feeder/feeder.py tx-mix (PR #152 hardened)",\n'
  printf '  "outcome": "%s",\n' "${OUTCOME}"
  printf '  "tool_exit_code": %s,\n' "${RC}"
  printf '  "egress_region_intended": "%s",\n' "${TXMIX_REGION}"
  printf '  "egress_ip": "%s",\n' "${EGRESS_IP}"
  printf '  "egress_country": "%s",\n' "${EGRESS_GEO}"
  printf '  "endpoint": "%s",\n' "${ENDPOINT}"
  printf '  "window": "%s",\n' "${WINDOW_DESC}"
  printf '  "max_rpm": %s,\n' "${TXMIX_MAX_RPM}"
  printf '  "page_limit": %s,\n' "${TXMIX_PAGE_LIMIT}"
  printf '  "label": "%s",\n' "${TXMIX_LABEL}"
  printf '  "ts_utc": "%s",\n' "${TS}"
  printf '  "gcs_dest": "%s"\n' "${DEST}"
  printf '}\n'
} > "${META_PATH}"

echo "### TXMIX_RESULT outcome=${OUTCOME} rc=${RC} window=\"${WINDOW_DESC}\" endpoint=${ENDPOINT}"

# ── Upload to GCS (mandatory — best-effort per file, DONE written last) ──
upload_status=0
gcloud storage cp "${RAW_PATH}" "${DEST}tx-mix.txt" \
  || { echo "### UPLOAD_WARN tx-mix.txt failed" >&2; upload_status=1; }
gcloud storage cp "${META_PATH}" "${DEST}tx-mix.meta.json" \
  || { echo "### UPLOAD_WARN tx-mix.meta.json failed" >&2; upload_status=1; }
if [[ -s /tmp/tx-mix.err ]]; then
  gcloud storage cp /tmp/tx-mix.err "${DEST}tx-mix.stderr.txt" \
    || echo "### UPLOAD_WARN tx-mix.stderr.txt failed" >&2
fi

{
  printf 'outcome=%s\n' "${OUTCOME}"
  printf 'tool_exit_code=%s\n' "${RC}"
  printf 'upload_status=%s\n' "${upload_status}"
  printf 'egress_ip=%s\n' "${EGRESS_IP}"
  printf 'ts=%s\n' "$(date -u +%Y-%m-%dT%H:%M:%SZ)"
} > "${DONE_PATH}"
gcloud storage cp "${DONE_PATH}" "${DEST}DONE" \
  || echo "### UPLOAD_WARN DONE upload failed" >&2

echo "### TXMIX_UPLOAD_DONE dest=${DEST} upload_status=${upload_status} outcome=${OUTCOME}"

# Exit non-zero on a genuine failure so the Cloud Run Job execution is
# marked failed (the operator sees red), but the artifacts are already in
# GCS for diagnosis. A geo-block is a real finding -> non-zero.
exit "${RC}"
