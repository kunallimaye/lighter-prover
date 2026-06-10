#!/usr/bin/env bash
# run-fleet.sh -- top-level CLI for the bench-fleet toolkit.
#
# Subcommands:
#   quota-check                                Verify GCP quotas (read-only)
#   run [--machines L] [--ref R] [--yes] [--dry-run]
#                                              Provision + monitor + collect
#   status [--run-id ID]                       Print fleet state from GCS
#   collect --run-id ID                        Pull logs from GCS, run parser
#   publish --run-id ID                        Create Discussion, comment on #6
#   teardown [--run-id ID] [--all]             Force-delete any leftover VMs
#
# Hard rule: every gcloud/gcloud-storage call goes through gcloud_imp /
# gstorage_imp wrappers in common.sh (impersonation enforced).

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

DEFAULT_SVALUES="1 2 4 6"

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
    --yes            Skip cost-estimate confirmation prompt
    --dry-run        Print create commands; do not provision

  status [--run-id ID]               Show current state from GCS
  collect --run-id ID                Pull logs from GCS, run parser
  publish --run-id ID                Create Discussion + comment on #6
  teardown [--run-id ID] [--all]     Force-delete leftover VMs

Env overrides (rarely needed):
  PROJECT, REGION, BENCH_SWEEP_SA, NETWORK, SUBNET, GCS_BUCKET
EOF
}

# ---------------------------------------------------------------------------
# quota-check
# ---------------------------------------------------------------------------
cmd_quota_check() {
  log_info "querying ${REGION} quotas (impersonating ${BENCH_SWEEP_SA})"

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
  log_ok "quota-check PASSED"
}

# ---------------------------------------------------------------------------
# run
# ---------------------------------------------------------------------------
cmd_run() {
  local machines="" ref="main" auto_yes=0 dry_run=0
  while [[ $# -gt 0 ]]; do
    case "$1" in
      --machines) machines="$2"; shift 2 ;;
      --ref)      ref="$2"; shift 2 ;;
      --yes)      auto_yes=1; shift ;;
      --dry-run)  dry_run=1; shift ;;
      *) log_err "unknown run flag: $1"; usage; exit 2 ;;
    esac
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

  # Cost estimate
  local cost
  cost="$(estimate_cost 1.0 "${machine_list[@]}")"
  echo "" >&2
  echo "Fleet plan:" >&2
  echo "  ref:       ${ref} (${sha})" >&2
  echo "  S sweep:   ${DEFAULT_SVALUES}" >&2
  echo "  machines:  ${#machine_list[@]}" >&2
  for mt in "${machine_list[@]}"; do
    echo "             - ${mt}" >&2
  done
  echo "  est. cost: ${cost} (assumes 1h wall per VM; build+sweep typically <1h)" >&2
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
  printf '%s\n' "${machine_list[@]}" > "${run_dir}/machines.txt"

  log_info "run_id=${run_id} (state in ${run_dir})"

  # Provision in parallel
  local pids=()
  for mt in "${machine_list[@]}"; do
    (
      provision_one_vm "$mt" "$run_id" "$sha" "$DEFAULT_SVALUES" "$dry_run" "$run_dir" \
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
  printf 'machine_type\tgit_sha\thost\tcpu\tcores\tram_kb\tS\tchunks\tpre_exec_ms\ttotal_tx_ms\tavg_tx_ms\ttotal_chain_ms\tavg_chain_ms\twall_ms\trss_kb\texit_code\tstatus\n' > "${out_tsv}"

  local count=0 fail=0
  for mt_dir in "${local_dir}"/*/; do
    [[ -d "$mt_dir" ]] || continue
    local mt; mt="$(basename "$mt_dir")"
    # If "results" subdir got included (from the VM's /opt/results upload),
    # tunnel into it.
    local search_dir="${mt_dir}"
    [[ -d "${mt_dir}/results" ]] && search_dir="${mt_dir}/results"

    for log_file in "${search_dir}"/bench-S*.log; do
      [[ -r "$log_file" ]] || continue
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
