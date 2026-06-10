#!/usr/bin/env bash
# monitor.sh -- monitor_fleet <run_id> [<run_dir>]
#
# Polls GCS for _DONE sentinels every 60s. When a sentinel appears for a
# machine, issues `gcloud compute instances delete` from the orchestrator
# (we run as bench-sweep, which has compute.instanceAdmin.v1).
#
# Exits when:
#  - every machine in <run_dir> is in state {complete, provision-failed}, OR
#  - the overall 8h fleet timeout fires.
#
# On exit, force-deletes any VMs still labeled with this run-id (belt-and-
# suspenders cleanup).
#
# Source-only.

_MON_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
# shellcheck source=lib/common.sh
# shellcheck disable=SC1091 # path is resolved at runtime from $BASH_SOURCE
. "${_MON_DIR}/common.sh"

# Defaults (overridable by env).
: "${FLEET_POLL_INTERVAL:=60}"          # seconds between polls
: "${FLEET_STATUS_INTERVAL:=300}"       # seconds between full status prints
: "${FLEET_OVERALL_TIMEOUT:=28800}"     # 8h overall cap

# Read current state for one machine: prints "<state>\t<zone>\t<instance>".
_read_state() {
  local f="$1"
  if [[ -r "$f" ]]; then
    head -n1 "$f"
  else
    printf 'unknown\tNONE\tNONE\n'
  fi
}

_write_state() {
  local f="$1" state="$2" zone="$3" inst="$4"
  printf '%s\t%s\t%s\n' "$state" "$zone" "$inst" > "$f"
}

# delete_vm <zone> <instance>
_delete_vm() {
  local zone="$1" inst="$2"
  if [[ "$zone" == "NONE" || "$inst" == "NONE" ]]; then
    return 0
  fi
  log_info "deleting VM ${inst} in ${zone}"
  gcloud_imp compute instances delete "${inst}" \
    --zone="${zone}" --quiet 2>&1 | tail -10 || {
      log_warn "delete failed for ${inst} (already gone?)"
      return 0
    }
}

# Force-cleanup pass over any VMs still labeled with this run.
_cleanup_stragglers() {
  local run_id="$1"
  log_info "cleanup pass: looking for any VMs labeled run-id=${run_id}"
  local list
  list="$(gcloud_imp compute instances list \
            --filter="labels.run-id=${run_id} AND labels.purpose=bench-fleet" \
            --format='value(name,zone.basename())' 2>/dev/null)" || return 0
  if [[ -z "$list" ]]; then
    log_ok "no straggler VMs"
    return 0
  fi
  while read -r name zone; do
    [[ -z "$name" ]] && continue
    log_warn "straggler: ${name} in ${zone} -- force-deleting"
    gcloud_imp compute instances delete "${name}" --zone="${zone}" --quiet \
      2>&1 | tail -5 || true
  done <<< "$list"
}

_print_status_table() {
  local run_dir="$1" t_start="$2"
  local now_t; now_t=$(date +%s)
  local elapsed=$(( now_t - t_start ))
  local elapsed_fmt; elapsed_fmt="$(fmt_duration "$elapsed")"
  echo "" >&2
  printf '=== Fleet status @ T+%s (run_dir=%s) ===\n' "$elapsed_fmt" "$run_dir" >&2
  printf '%-22s %-18s %-10s %-9s\n' "machine_type" "state" "sentinel" "instance" >&2
  printf '%-22s %-18s %-10s %-9s\n' "----------------------" "------------------" "----------" "---------" >&2
  local mt state zone inst sent
  for f in "${run_dir}"/*.state; do
    [[ -r "$f" ]] || continue
    mt="$(basename "$f" .state)"
    IFS=$'\t' read -r state zone inst < <(_read_state "$f")
    if [[ -f "${run_dir}/${mt}.sentinel" ]]; then
      sent="yes"
    else
      sent="no"
    fi
    printf '%-22s %-18s %-10s %s\n' "$mt" "$state" "$sent" "$inst" >&2
  done
  echo "" >&2
}

# monitor_fleet <run_id> [<run_dir>]
monitor_fleet() {
  local run_id="$1"
  local run_dir="${2:-/tmp/bench-fleet-runs/${run_id}}"
  [[ -d "$run_dir" ]] || die "run_dir does not exist: ${run_dir}"

  local t_start; t_start=$(date +%s)
  local t_last_status=$t_start

  log_info "monitor_fleet: run_id=${run_id} run_dir=${run_dir}"
  log_info "poll=${FLEET_POLL_INTERVAL}s status=${FLEET_STATUS_INTERVAL}s timeout=${FLEET_OVERALL_TIMEOUT}s"

  while true; do
    local now; now=$(date +%s)
    local elapsed=$(( now - t_start ))

    # Overall timeout?
    if (( elapsed > FLEET_OVERALL_TIMEOUT )); then
      log_warn "overall fleet timeout (${FLEET_OVERALL_TIMEOUT}s) reached"
      break
    fi

    # Status pass: iterate machines, check sentinels.
    local all_terminal=1
    for f in "${run_dir}"/*.state; do
      [[ -r "$f" ]] || continue
      local mt; mt="$(basename "$f" .state)"
      local state zone inst
      IFS=$'\t' read -r state zone inst < <(_read_state "$f")

      case "$state" in
        complete|provision-failed)
          continue
          ;;
        running)
          # Look for sentinel
          local sent_uri; sent_uri="$(sentinel_uri "$run_id" "$mt")"
          if sentinel_exists "$sent_uri"; then
            log_ok "[${mt}] sentinel seen at ${sent_uri}"
            touch "${run_dir}/${mt}.sentinel"
            _delete_vm "$zone" "$inst"
            _write_state "$f" "complete" "$zone" "$inst"
          else
            all_terminal=0
          fi
          ;;
        *)
          all_terminal=0
          ;;
      esac
    done

    if (( all_terminal == 1 )); then
      log_ok "all machines reached terminal state"
      break
    fi

    # Periodic status print
    if (( now - t_last_status >= FLEET_STATUS_INTERVAL )); then
      _print_status_table "$run_dir" "$t_start"
      t_last_status=$now
    fi

    sleep "$FLEET_POLL_INTERVAL"
  done

  _print_status_table "$run_dir" "$t_start"
  _cleanup_stragglers "$run_id"
}
