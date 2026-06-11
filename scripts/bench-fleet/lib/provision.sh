#!/usr/bin/env bash
# provision.sh -- provision_one_vm <machine_type> <run_id> <sha> <svalues> [dry_run=0]
#
# Renders the startup-script template, then runs `gcloud compute instances
# create` with the toolkit's mandatory flag set. Tries each preferred zone
# in order until one succeeds. On ZONE_RESOURCE_POOL_EXHAUSTED, advances to
# the next zone.
#
# On success: prints "<instance_name> <zone>" to stdout, writes a status
# record under <RUN_DIR>/<machine_type>.state.
# On failure: writes "provision-failed" status, returns non-zero.
#
# Source-only.

# Don't `set -e` here -- caller decides per-machine policy.

_PROV_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
# shellcheck source=lib/common.sh
# shellcheck disable=SC1091 # path is resolved at runtime from $BASH_SOURCE
. "${_PROV_DIR}/common.sh"

# render_startup <image> <svalues> <host_label> <run_id> [<cal_mode>] -> path
# <image> is the full AR URI including the per-microarch tag (#33).
# <cal_mode> (issue #85): "1" switches the per-S worker container to
# tx_limit=4*S; default "0" keeps the historical fixed __TX_LIMIT__.
render_startup() {
  local image="$1" svalues="$2" host_label="$3" run_id="$4"
  local cal_mode="${5:-0}"
  local tmpl="${FLEET_TEMPLATES}/vm-startup.sh.tmpl"
  local out
  out="$(mktemp -t bench-fleet-startup.XXXXXX)"
  # Use awk for the substitution -- sed's delimiter escaping is brittle when
  # the bucket / image URIs contain "/".
  awk \
    -v image="${image}" \
    -v bucket="${GCS_BUCKET}" \
    -v run_prefix="${run_id}/${host_label}/" \
    -v svalues="${svalues}" \
    -v host_label="${host_label}" \
    -v tx_limit="${TX_LIMIT}" \
    -v cal_mode="${cal_mode}" \
    '{ gsub(/__IMAGE__/, image);
       gsub(/__BUCKET__/, bucket);
       gsub(/__RUN_PREFIX__/, run_prefix);
       gsub(/__SVALUES__/, svalues);
       gsub(/__HOST_LABEL__/, host_label);
       gsub(/__TX_LIMIT__/, tx_limit);
       gsub(/__CAL_MODE__/, cal_mode);
       print }' "${tmpl}" > "${out}"
  printf '%s\n' "${out}"
}

# fleet_image_for <machine_type> <sha> -> full AR image URI with the
# per-microarch tag from machines.tsv (image_tag column). #33: prebuilt
# images replace on-VM builds; "native honesty" is preserved by mapping
# every shape to an explicit -C target-cpu variant.
fleet_image_for() {
  local mt="$1" sha="$2"
  local tag
  tag="$(machine_field "$mt" "image_tag")" || return 1
  if [[ -z "${tag}" ]]; then
    log_err "[${mt}] image_tag missing in machines.tsv"
    return 1
  fi
  printf '%s:%s-%s\n' "${AR_IMAGE_BASE}" "${sha}" "${tag}"
}

# provision_one_vm <machine_type> <run_id> <sha> <svalues> [<dry_run>] [<run_dir>]
# dry_run: when "1", prints the gcloud command WITHOUT executing it.
# run_dir: per-run state directory (defaults to /tmp/bench-fleet-runs/<run-id>).
provision_one_vm() {
  local mt="$1" run_id="$2" sha="$3" svalues="$4"
  local dry_run="${5:-0}"
  local run_dir="${6:-/tmp/bench-fleet-runs/${run_id}}"
  mkdir -p "${run_dir}"

  local image_family image_project preferred_zones disk_type
  image_family="$(machine_field "$mt" "image_family")"   || return 1
  image_project="$(machine_field "$mt" "image_project")" || return 1
  preferred_zones="$(machine_field "$mt" "preferred_zones")" || return 1
  disk_type="$(machine_field "$mt" "disk_type")" || return 1
  if [[ -z "${disk_type}" ]]; then
    log_err "[${mt}] disk_type missing in machines.tsv"
    return 1
  fi

  local inst_name
  inst_name="$(instance_name "${run_id}" "${mt}")"
  if (( ${#inst_name} > 63 )); then
    log_err "instance name >63 chars: ${inst_name}"
    return 1
  fi

  local compute_sa
  if [[ "${dry_run}" == "1" ]]; then
    compute_sa="<PROJECT_NUMBER>-compute@developer.gserviceaccount.com"
  else
    compute_sa="$(get_compute_sa)" || return 1
  fi

  # Resolve the prebuilt per-microarch image for this shape (#33).
  local image
  image="$(fleet_image_for "${mt}" "${sha}")" || return 1

  local startup_path
  startup_path="$(render_startup "${image}" "${svalues}" "${mt}" "${run_id}" "${CAL_MODE:-0}")"
  # Stash the rendered script next to the run state for debugging.
  cp "${startup_path}" "${run_dir}/${mt}.startup.sh"

  local labels="purpose=bench-fleet,owner=lighter,run-id=${run_id},machine=${mt}"

  # Try each preferred zone.
  local ZONES IFS=,
  read -r -a ZONES <<< "${preferred_zones}"
  unset IFS

  local zone
  for zone in "${ZONES[@]}"; do
    log_info "[${mt}] trying zone=${zone} instance=${inst_name}"

    # Impersonation flag only when BENCH_SWEEP_SA is set (#33: default
    # empty — the active account is the orchestrator identity).
    local create_cmd=(gcloud)
    if [[ -n "${BENCH_SWEEP_SA}" ]]; then
      create_cmd+=(--impersonate-service-account="${BENCH_SWEEP_SA}")
    fi
    create_cmd+=(
             --project="${PROJECT}"
        compute instances create "${inst_name}"
        --zone="${zone}"
        --machine-type="${mt}"
        --image-family="${image_family}"
        --image-project="${image_project}"
        --boot-disk-size=100GB
        --boot-disk-type="${disk_type}"
        --service-account="${compute_sa}"
        --scopes=cloud-platform
        --max-run-duration=10h
        --instance-termination-action=DELETE
        --network="${NETWORK}"
        --subnet="${SUBNET}"
        --metadata-from-file=startup-script="${startup_path}"
        --labels="${labels}"
    )

    if [[ "${dry_run}" == "1" ]]; then
      printf '# %s in %s\n' "${mt}" "${zone}"
      # Print quoted command (one arg per line for readability).
      printf '%q ' "${create_cmd[@]}"
      printf '\n\n'
      # Stash record so dry-run callers see the same state-file layout.
      printf 'dry-run\t%s\t%s\n' "${zone}" "${inst_name}" > "${run_dir}/${mt}.state"
      return 0
    fi

    local create_log; create_log="$(mktemp -t bench-fleet-create.XXXXXX)"
    if "${create_cmd[@]}" > "${create_log}" 2>&1; then
      log_ok "[${mt}] created instance=${inst_name} zone=${zone}"
      printf 'running\t%s\t%s\n' "${zone}" "${inst_name}" > "${run_dir}/${mt}.state"
      printf '%s\t%s\n' "${inst_name}" "${zone}"
      rm -f "${create_log}"
      return 0
    fi

    if grep -qE 'ZONE_RESOURCE_POOL_EXHAUSTED|does not have enough resources available' "${create_log}"; then
      log_warn "[${mt}] zone ${zone} exhausted (stockout) -- trying next"
      cat "${create_log}" >&2
      rm -f "${create_log}"
      continue
    fi

    log_err "[${mt}] create failed in ${zone} for non-stockout reason:"
    cat "${create_log}" >&2
    rm -f "${create_log}"
    printf 'provision-failed\t%s\t%s\n' "${zone}" "${inst_name}" > "${run_dir}/${mt}.state"
    return 1
  done

  log_err "[${mt}] all zones exhausted: ${preferred_zones}"
  printf 'provision-failed\tNONE\t%s\n' "${inst_name}" > "${run_dir}/${mt}.state"
  return 1
}
