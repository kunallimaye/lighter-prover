#!/usr/bin/env bash
# run-fleet.sh -- top-level CLI for the bench-fleet toolkit.
#
# Subcommands:
#   quota-check                                Verify GCP quotas (read-only)
#   run [--machines L] [--ref R] [--svalues "L"] [--yes] [--dry-run]
#                                              Provision + monitor + collect
#   status [--run-id ID]                       Print fleet state from GCS
#   collect --run-id ID                        Pull logs from GCS, run parser
#   publish --run-id ID                        Create Discussion, comment on #6
#   teardown [--run-id ID] [--all]             Force-delete any leftover VMs
#
# Hard rule: every gcloud/gcloud-storage call goes through gcloud_imp /
# gstorage_imp wrappers in common.sh (project pinned; impersonation only
# when BENCH_SWEEP_SA is set — default empty since #33).

set -euo pipefail

_SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
# shellcheck source=lib/common.sh
# shellcheck disable=SC1091 # path is resolved at runtime from $BASH_SOURCE
. "${_SCRIPT_DIR}/lib/common.sh"
# shellcheck source=lib/provision.sh
# shellcheck disable=SC1091
. "${_SCRIPT_DIR}/lib/provision.sh"
# shellcheck source=lib/monitor.sh
# shellcheck disable=SC1091
. "${_SCRIPT_DIR}/lib/monitor.sh"

# Default sweep values: config.toml [fleet].svalues > built-in.
DEFAULT_SVALUES="${FLEET_SVALUES:-1 2 4 6}"

# ---------------------------------------------------------------------------
# Usage
# ---------------------------------------------------------------------------
usage() {
  cat <<'EOF'
Usage: run-fleet.sh <subcommand> [options]

Subcommands:
  quota-check                        Verify GCP quotas (read-only)
  run [opts]                         Provision + monitor + collect (no publish)
    --machines L     Comma-separated machine_types (default: all 10)
    --ref R          Git ref (branch or SHA) to build (default: main)
    --svalues "L"    Space-separated S sweep values (default: "1 2 4 6")
    --yes            Skip cost-estimate confirmation prompt
    --dry-run        Print create commands; do not provision

  status [--run-id ID]               Show current state from GCS
  collect --run-id ID                Pull logs from GCS, run parser
  publish --run-id ID                Create Discussion + comment on #6
  teardown [--run-id ID] [--all]     Force-delete leftover VMs

Configuration: repo-root config.toml ([gcp.defaults] + [fleet]) is the
source of truth. Env overrides (rarely needed):
  PROJECT, REGION, NETWORK, SUBNET, GCS_BUCKET, AR_IMAGE_BASE, TX_LIMIT,
  BENCH_SWEEP_SA (legacy impersonation -- empty/off by default since #33)
EOF
}

# ---------------------------------------------------------------------------
# quota-check
# ---------------------------------------------------------------------------
cmd_quota_check() {
  log_info "querying ${REGION} quotas in ${PROJECT} (as $(fleet_identity))"

  # Pull regional quotas as JSON, then summarize per family. Falls back to
  # ALL_QUOTAS when a specific family doesn't exist (e.g. older API).
  local quotas_json
  quotas_json="$(gcloud_imp compute regions describe "${REGION}" --format='json(quotas)')" || {
    log_err "could not describe region ${REGION}"
    return 1
  }

  # Aggregate requested vCPU per quota family from machines.tsv.
  declare -A NEEDED
  # shellcheck disable=SC2034 # _arch _ifam _iproj _zones are columns we read past to reach qfam
  while IFS=$'\t' read -r _mt vcpus _arch _ifam _iproj qfam _zones; do
    NEEDED[$qfam]=$(( ${NEEDED[$qfam]:-0} + vcpus ))
  done < <(machines_all_rows)

  local fail=0
  local fam
  for fam in "${!NEEDED[@]}"; do
    local need="${NEEDED[$fam]}"
    # Use python to parse out the named quota's limit + usage.
    local quota_line
    quota_line="$(python3 - "${fam}" <<PYEOF
import json, sys
fam = sys.argv[1]
data = json.loads('''${quotas_json//\'/\\\'}''')
for q in data.get("quotas", []):
    if q.get("metric") == fam:
        limit = q.get("limit", 0)
        usage = q.get("usage", 0)
        print(f"{limit}\t{usage}")
        sys.exit(0)
print("MISSING\tMISSING")
PYEOF
    )" || quota_line="MISSING\tMISSING"

    local limit usage
    IFS=$'\t' read -r limit usage <<< "${quota_line}"

    if [[ "$limit" == "MISSING" ]]; then
      log_warn "quota family ${fam} not found in ${REGION} (may be named differently or auto-managed)"
      printf '  %-22s need=%-4s   status=NOT-LISTED (request raise if create fails)\n' "${fam}" "${need}" >&2
      continue
    fi

    # Compute available (limit - usage). Use python for float-safe arithmetic.
    local available
    available="$(python3 -c "print(int(float(${limit}) - float(${usage})))")"

    if (( need > available )); then
      log_err "[INSUFFICIENT] ${fam}: need ${need}, available ${available} (limit ${limit}, usage ${usage})"
      printf '              request raise at:\n' >&2
      printf '              https://console.cloud.google.com/iam-admin/quotas?project=%s&service=compute.googleapis.com&metric=%s\n' \
        "${PROJECT}" "${fam}" >&2
      fail=1
    else
      log_ok  "[OK]           ${fam}: need ${need}, available ${available} (limit ${limit}, usage ${usage})"
    fi
  done

  if (( fail )); then
    log_err "quota-check FAILED"
    return 1
  fi

  # Bucket existence assertion (added per issue #19). Previous v2 run burned
  # ~$30 because VMs provisioned cleanly but had no bucket to upload to,
  # so the monitor never saw _DONE sentinels and timed out.
  log_info "verifying GCS bucket ${GCS_BUCKET} exists"
  if ! gcloud_imp storage buckets describe "${GCS_BUCKET}" --format='value(name)' >/dev/null 2>&1; then
    log_err "bucket ${GCS_BUCKET} does not exist."
    printf '  The bucket (plus its IAM grants) is created by the Owner-tier bootstrap:\n' >&2
    printf '    make admin-cloud-init\n' >&2
    printf '  (or manually: gcloud storage buckets create %s --location=%s \\\n' "${GCS_BUCKET}" "${REGION}" >&2
    printf '     --uniform-bucket-level-access --project=%s)\n' "${PROJECT}" >&2
    printf '  Then re-run '\''make fleet-quota-check'\''.\n' >&2
    return 1
  fi
  log_ok "bucket ${GCS_BUCKET} exists"

  # Write-probe (added per issue #23). v3 burned a full fleet because VMs
  # could provision and run but every upload got HTTP 403 — and the startup
  # script's `|| true` swallowed it. Two distinct checks are needed:
  #
  #  1. Orchestrator-side: can the orchestrator identity write an object?
  #  2. VM-side: does the bucket IAM policy grant the default Compute SA
  #     (what the VMs actually run as) roles/storage.objectAdmin? An
  #     orchestrator-side write does NOT prove VM-side metadata-server
  #     access, so we inspect the policy directly.
  log_info "write-probe: uploading test object to ${GCS_BUCKET}"
  local probe_obj
  probe_obj="${GCS_BUCKET}/_quota_check_$(date -u +%s)_$$"
  if ! printf 'bench-fleet quota-check write probe\n' \
        | gstorage_imp cp - "${probe_obj}" >/dev/null 2>&1; then
    log_err "write-probe FAILED: orchestrator ($(fleet_identity)) cannot write to ${GCS_BUCKET}"
    printf '  The orchestrator identity needs object write on the bucket.\n' >&2
    printf '  This grant is part of the Owner-tier bootstrap: make admin-cloud-init\n' >&2
    return 1
  fi
  gstorage_imp rm "${probe_obj}" >/dev/null 2>&1 \
    || log_warn "could not delete probe object ${probe_obj} (harmless; clean up manually)"
  log_ok "write-probe passed (orchestrator-side, $(fleet_identity))"

  # VM-side IAM assertion: the bucket policy must include the Compute SA
  # with objectAdmin, or every VM upload will 403 exactly like v3 (#23).
  local compute_sa
  compute_sa="$(get_compute_sa)" || return 1
  log_info "verifying bucket IAM grants ${compute_sa} roles/storage.objectAdmin"
  local policy
  if ! policy="$(gcloud_imp storage buckets get-iam-policy "${GCS_BUCKET}" --format=json 2>/dev/null)"; then
    log_err "could not read IAM policy on ${GCS_BUCKET} (need storage.buckets.getIamPolicy)"
    return 1
  fi
  if ! printf '%s' "${policy}" | python3 -c '
import json, sys
sa = sys.argv[1]
member = f"serviceAccount:{sa}"
pol = json.load(sys.stdin)
ok = any(
    b.get("role") == "roles/storage.objectAdmin" and member in b.get("members", [])
    for b in pol.get("bindings", [])
)
sys.exit(0 if ok else 1)
' "${compute_sa}"; then
    log_err "Compute SA ${compute_sa} lacks roles/storage.objectAdmin on ${GCS_BUCKET}"
    log_err "VM uploads WILL fail with HTTP 403 (v3 root cause — issue #23)."
    printf '  This grant is part of the Owner-tier bootstrap: make admin-cloud-init\n' >&2
    printf '  (or manually: gcloud storage buckets add-iam-policy-binding %s \\\n' "${GCS_BUCKET}" >&2
    printf '     --member=serviceAccount:%s \\\n' "${compute_sa}" >&2
    printf '     --role=roles/storage.objectAdmin --project=%s)\n' "${PROJECT}" >&2
    printf '  Then re-run quota-check.\n' >&2
    return 1
  fi
  log_ok "bucket IAM grants ${compute_sa} objectAdmin (VM-side uploads authorized)"

  log_ok "quota-check PASSED"
}

# ---------------------------------------------------------------------------
# run
# ---------------------------------------------------------------------------
cmd_run() {
  local machines="" ref="main" auto_yes=0 dry_run=0 svalues="${DEFAULT_SVALUES}"
  while [[ $# -gt 0 ]]; do
    case "$1" in
      --machines) machines="$2"; shift 2 ;;
      --ref)      ref="$2"; shift 2 ;;
      --svalues)  svalues="$2"; shift 2 ;;
      --yes)      auto_yes=1; shift ;;
      --dry-run)  dry_run=1; shift ;;
      *) log_err "unknown run flag: $1"; usage; exit 2 ;;
    esac
  done

  # Validate svalues: non-empty, space-separated positive integers.
  [[ -n "${svalues}" ]] || die "--svalues must not be empty"
  local sv
  for sv in ${svalues}; do
    [[ "${sv}" =~ ^[0-9]+$ && "${sv}" != "0" ]] \
      || die "--svalues must be space-separated positive integers (got: '${sv}')"
  done

  # Default machine list = all 10
  local -a machine_list
  if [[ -n "$machines" ]]; then
    IFS=',' read -r -a machine_list <<< "$machines"
  else
    mapfile -t machine_list < <(machines_all_types)
  fi

  # Validate
  for mt in "${machine_list[@]}"; do
    machines_lookup "$mt" >/dev/null || die "unknown machine_type: $mt"
  done

  # Resolve --ref to a SHA. For dry-run, use ref as-is if it looks SHA-ish.
  local sha
  if [[ "$dry_run" == "1" && "$ref" =~ ^[0-9a-f]{7,40}$ ]]; then
    sha="$ref"
  else
    sha="$(git ls-remote https://github.com/kunallimaye/lighter-prover.git "$ref" \
            | awk 'NR==1 {print $1}')"
    if [[ -z "$sha" ]]; then
      # Treat as already-a-SHA if ls-remote returned nothing.
      if [[ "$ref" =~ ^[0-9a-f]{7,40}$ ]]; then
        sha="$ref"
      else
        die "could not resolve ref to SHA: $ref"
      fi
    fi
  fi
  log_info "ref=${ref} -> sha=${sha}"

  # Image preflight (#33): the fleet pulls prebuilt per-microarch images —
  # verify every needed tag exists in Artifact Registry BEFORE any spend.
  # Build them with `make cloud-bench-build` (cicd/cloudbuild.yaml pushes
  # :<sha>, :<sha>-znver5, :<sha>-neoverse-v2, :<sha>-neoverse-n1).
  if [[ "${dry_run}" != "1" ]]; then
    local -A _seen_tags=()
    local img_tag image
    for mt in "${machine_list[@]}"; do
      img_tag="$(machine_field "$mt" "image_tag")" || die "no image_tag for ${mt}"
      [[ -n "${_seen_tags[$img_tag]:-}" ]] && continue
      _seen_tags[$img_tag]=1
      image="${AR_IMAGE_BASE}:${sha}-${img_tag}"
      log_info "verifying image exists: ${image}"
      # Use `tags list` instead of `images describe`: describe requires
      # containeranalysis.occurrences.list, which the orchestrator SA may
      # not hold. Listing tags only needs artifactregistry.reader.
      if [[ -z "$(gcloud_imp artifacts docker tags list "${AR_IMAGE_BASE}" \
            --filter="tag:${sha}-${img_tag}" --format='value(tag)' 2>/dev/null)" ]]; then
        log_err "image not found: ${image}"
        printf '  Build the matrix for this sha first:\n' >&2
        printf '    make cloud-bench-build    # (submits cicd/cloudbuild.yaml)\n' >&2
        return 1
      fi
    done
    log_ok "all required per-microarch images exist for sha=${sha}"
  fi

  # Cost estimate (per-shape, calibrated against v2 findings — issue #19).
  # Previous estimator assumed 1h per VM, which was 3-6× too low: realistic
  # full-sweep wall is 6h on T2A, 4h on Axion, 3h on Turin. The breakdown
  # below lets the operator see where the spend goes.
  echo "" >&2
  echo "Fleet plan:" >&2
  echo "  ref:       ${ref} (${sha})" >&2
  echo "  S sweep:   ${svalues}" >&2
  echo "  machines:  ${#machine_list[@]}" >&2
  for mt in "${machine_list[@]}"; do
    echo "             - ${mt}" >&2
  done
  echo "" >&2
  echo "  Per-shape cost estimate (price × realistic full-sweep wall):" >&2
  local cost
  cost="$(estimate_cost_breakdown "${machine_list[@]}")"
  echo "" >&2
  echo "  est. cost: ${cost}  (T2A:6h, C4A/N4A:4h, C4D/N4D:3h — calibrated against v2)" >&2
  echo "  wall:      ~6h (limited by slowest shape; 10h max-run-duration kill)" >&2
  echo "" >&2

  if (( ! auto_yes )); then
    read -r -p "Proceed? [y/N] " ans
    case "$ans" in
      [yY]|[yY][eE][sS]) ;;
      *) log_warn "aborted"; return 1 ;;
    esac
  fi

  local run_id; run_id="$(new_run_id)"
  local run_dir="/tmp/bench-fleet-runs/${run_id}"
  mkdir -p "${run_dir}"
  echo "${sha}"  > "${run_dir}/sha.txt"
  echo "${ref}"  > "${run_dir}/ref.txt"
  echo "${svalues}" > "${run_dir}/svalues.txt"
  printf '%s\n' "${machine_list[@]}" > "${run_dir}/machines.txt"

  log_info "run_id=${run_id} (state in ${run_dir})"

  # Provision in parallel
  local pids=()
  for mt in "${machine_list[@]}"; do
    (
      provision_one_vm "$mt" "$run_id" "$sha" "$svalues" "$dry_run" "$run_dir" \
        || log_warn "[${mt}] provision_one_vm returned non-zero"
    ) &
    pids+=($!)
  done
  for pid in "${pids[@]}"; do
    wait "$pid" || true
  done

  if [[ "$dry_run" == "1" ]]; then
    log_ok "dry-run complete; no VMs provisioned"
    return 0
  fi

  log_info "all provision attempts done -- starting monitor"
  monitor_fleet "$run_id" "$run_dir"

  log_ok "fleet ${run_id} complete; next: ./run-fleet.sh collect --run-id ${run_id}"
  echo "${run_id}"
}

# ---------------------------------------------------------------------------
# status
# ---------------------------------------------------------------------------
cmd_status() {
  local run_id=""
  while [[ $# -gt 0 ]]; do
    case "$1" in
      --run-id) run_id="$2"; shift 2 ;;
      *) die "unknown status flag: $1" ;;
    esac
  done
  [[ -n "$run_id" ]] || die "status: --run-id required"

  log_info "VMs labeled with run-id=${run_id}:"
  gcloud_imp compute instances list \
    --filter="labels.run-id=${run_id}" \
    --format='table(name,zone.basename(),status,labels.machine)' || true

  log_info "GCS contents under ${GCS_BUCKET}/${run_id}/:"
  gstorage_imp ls -r "${GCS_BUCKET}/${run_id}/**" 2>&1 | head -100 || true
}

# ---------------------------------------------------------------------------
# collect
# ---------------------------------------------------------------------------
cmd_collect() {
  local run_id=""
  while [[ $# -gt 0 ]]; do
    case "$1" in
      --run-id) run_id="$2"; shift 2 ;;
      *) die "unknown collect flag: $1" ;;
    esac
  done
  [[ -n "$run_id" ]] || die "collect: --run-id required"

  local local_dir="/tmp/bench-fleet-runs/${run_id}/collected"
  mkdir -p "${local_dir}"

  log_info "downloading GCS artifacts to ${local_dir}"
  gstorage_imp cp -r "${GCS_BUCKET}/${run_id}/*" "${local_dir}/" 2>&1 | tail -20 || true

  # Build parsed-results.tsv
  local out_tsv="${local_dir}/parsed-results.tsv"
  printf 'machine_type\tgit_sha\thost\tcpu\tcores\tram_kb\tS\tchunks\tpre_exec_ms\ttotal_tx_ms\tavg_tx_ms\ttotal_chain_ms\tavg_chain_ms\twall_ms\tms_per_tx\ttx_per_sec\trss_kb\texit_code\tstatus\n' > "${out_tsv}"

  # Layout since #33 (container fleet): each S value ran in its own
  # worker container, whose entrypoint uploaded
  #   <run-id>/<machine>/S<N>/{bench.log,bench.jsonl,DONE}
  # Fleet-level files (machine-info.txt, svalues-summary.txt, startup.log,
  # status.txt, _DONE) live at <run-id>/<machine>/.
  local count=0 fail=0
  for mt_dir in "${local_dir}"/*/; do
    [[ -d "$mt_dir" ]] || continue
    local mt; mt="$(basename "$mt_dir")"

    local s_dir log_file
    for s_dir in "${mt_dir}"S*/; do
      [[ -d "$s_dir" ]] || continue
      log_file="${s_dir}bench.log"
      if [[ ! -r "$log_file" ]]; then
        log_warn "no bench.log under ${s_dir}"
        fail=$((fail+1))
        continue
      fi
      local row
      if row="$(bash "${FLEET_LIB}/parse-bench-log.sh" "$log_file")"; then
        printf '%s\t%s\n' "$mt" "$row" >> "${out_tsv}"
        count=$((count+1))
      else
        log_warn "parse failed for ${log_file}"
        fail=$((fail+1))
      fi
    done
  done

  log_ok "parsed ${count} log files (${fail} failures) -> ${out_tsv}"
  printf '%s\n' "${out_tsv}"
}

# ---------------------------------------------------------------------------
# publish
# ---------------------------------------------------------------------------
cmd_publish() {
  local run_id=""
  while [[ $# -gt 0 ]]; do
    case "$1" in
      --run-id) run_id="$2"; shift 2 ;;
      *) die "unknown publish flag: $1" ;;
    esac
  done
  [[ -n "$run_id" ]] || die "publish: --run-id required"

  command -v gh >/dev/null || die "gh CLI not installed"

  local local_dir="/tmp/bench-fleet-runs/${run_id}/collected"
  local out_tsv="${local_dir}/parsed-results.tsv"
  [[ -r "$out_tsv" ]] || die "no parsed-results.tsv at ${out_tsv} -- run collect first"

  # Build an info dir suitable for the renderer.
  local info_dir="${local_dir}/info"
  mkdir -p "${info_dir}"
  for mt_dir in "${local_dir}"/*/; do
    [[ -d "$mt_dir" ]] || continue
    local mt; mt="$(basename "$mt_dir")"
    [[ "$mt" == "info" ]] && continue
    mkdir -p "${info_dir}/${mt}"
    local search="${mt_dir}"
    [[ -d "${mt_dir}/results" ]] && search="${mt_dir}/results"
    if [[ -r "${search}/machine-info.txt" ]]; then
      cp "${search}/machine-info.txt" "${info_dir}/${mt}/machine-info.txt"
    fi
    # GCS URI list (one per file)
    gstorage_imp ls -r "${GCS_BUCKET}/${run_id}/${mt}/**" 2>/dev/null \
      | grep -v '/$' \
      > "${info_dir}/${mt}/_GCS_PREFIX" || true
  done

  local sha
  sha="$(cat "/tmp/bench-fleet-runs/${run_id}/sha.txt" 2>/dev/null || echo "unknown")"
  local title="GCP fleet S∈{1,2,4,6} baseline: bench@${sha:0:7} across 10 shapes (3 architectures)"

  local body_file="${local_dir}/discussion.md"
  bash "${FLEET_LIB}/render-discussion.sh" "${out_tsv}" "${info_dir}" "${title}" > "${body_file}"
  log_ok "rendered discussion body: ${body_file} ($(wc -c < "${body_file}") bytes)"

  # Repo + category IDs from pilot.
  local REPO_ID="R_kgDOS15Okw"
  local SHOW_AND_TELL_ID="DIC_kwDOS15Ok84C-3Hg"

  log_info "creating Discussion in Show and tell"
  local resp
  # NB: query is single-quoted intentionally -- the `$repositoryId` etc. are
  # GraphQL variable references, NOT bash variables. gh substitutes them from
  # the -F flags above. shellcheck SC2016 doesn't apply here.
  # shellcheck disable=SC2016
  resp="$(gh api graphql \
    -F body=@"${body_file}" \
    -F title="${title}" \
    -F repositoryId="${REPO_ID}" \
    -F categoryId="${SHOW_AND_TELL_ID}" \
    -f query='mutation($repositoryId:ID!,$categoryId:ID!,$title:String!,$body:String!){createDiscussion(input:{repositoryId:$repositoryId,categoryId:$categoryId,title:$title,body:$body}){discussion{number url}}}')" \
    || die "createDiscussion failed"
  local disc_url
  disc_url="$(printf '%s' "${resp}" | python3 -c 'import json,sys; d=json.load(sys.stdin); print(d["data"]["createDiscussion"]["discussion"]["url"])')"
  log_ok "Discussion created: ${disc_url}"

  # Comment on #6
  log_info "posting cross-link comment on Discussion #6"
  local disc6_id
  disc6_id="$(gh api graphql -f query='query{repository(owner:"kunallimaye",name:"lighter-prover"){discussion(number:6){id}}}' \
    | python3 -c 'import json,sys; print(json.load(sys.stdin)["data"]["repository"]["discussion"]["id"])')"
  # shellcheck disable=SC2016 # GraphQL variable references, not bash
  gh api graphql \
    -F discussionId="${disc6_id}" \
    -F body="GCP fleet results for S∈{1,2,4,6} across 10 shapes published at ${disc_url}" \
    -f query='mutation($discussionId:ID!,$body:String!){addDiscussionComment(input:{discussionId:$discussionId,body:$body}){comment{url}}}' \
    > /dev/null || log_warn "comment on #6 failed (Discussion may still be the canonical home)"
  log_ok "back-link comment posted on Discussion #6"
  printf '%s\n' "${disc_url}"
}

# ---------------------------------------------------------------------------
# teardown
# ---------------------------------------------------------------------------
cmd_teardown() {
  local run_id="" all=0
  while [[ $# -gt 0 ]]; do
    case "$1" in
      --run-id) run_id="$2"; shift 2 ;;
      --all)    all=1; shift ;;
      *) die "unknown teardown flag: $1" ;;
    esac
  done

  local filter="labels.purpose=bench-fleet"
  if (( all == 0 )); then
    [[ -n "$run_id" ]] || die "teardown: provide --run-id ID or --all"
    filter="${filter} AND labels.run-id=${run_id}"
  fi

  log_info "looking up VMs matching: ${filter}"
  local list
  list="$(gcloud_imp compute instances list \
            --filter="${filter}" \
            --format='value(name,zone.basename())')"
  if [[ -z "$list" ]]; then
    log_ok "no matching VMs"
    return 0
  fi
  echo "${list}" >&2
  while read -r name zone; do
    [[ -z "$name" ]] && continue
    log_info "deleting ${name} in ${zone}"
    gcloud_imp compute instances delete "${name}" --zone="${zone}" --quiet \
      2>&1 | tail -5 || true
  done <<< "$list"
  log_ok "teardown complete"
}

# ---------------------------------------------------------------------------
# Dispatch
# ---------------------------------------------------------------------------
main() {
  if [[ $# -eq 0 ]]; then
    usage
    exit 2
  fi
  local sub="$1"; shift
  case "$sub" in
    quota-check) cmd_quota_check "$@" ;;
    run)         cmd_run "$@" ;;
    status)      cmd_status "$@" ;;
    collect)     cmd_collect "$@" ;;
    publish)     cmd_publish "$@" ;;
    teardown)    cmd_teardown "$@" ;;
    -h|--help|help) usage ;;
    *) log_err "unknown subcommand: $sub"; usage; exit 2 ;;
  esac
}

main "$@"
