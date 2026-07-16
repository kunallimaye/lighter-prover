#!/usr/bin/env bash
#
# csweep_validate.sh  --  validate chunk-size (C) sweep values (issue #321)
#
# The C-sweep make target (`cloud-gke-bench-csweep`) is a THIN foreach loop over
# `cloud-gke-bench`, one submit per C. This helper is the up-front guard it runs
# BEFORE any (costly) submit: every C must be a positive integer that EVENLY
# DIVIDES txs_per_block (=500 for the real production block, #337). Failing early
# here avoids launching a run that the prover-node would reject anyway.
#
# Valid divisors of 500: 1 2 4 5 10 20 25 50 100 125 250 500.
#
# Usage:   bash csweep_validate.sh <C> [<C> ...]
#          TXS_PER_BLOCK=500 bash csweep_validate.sh 1 2 4 5
# Exit:    0 = all C valid, 1 = an invalid C was found
#
set -euo pipefail

# Kept in sync with bench/bench_test.json (500) and the prover-node divisor guard.
TXS_PER_BLOCK="${TXS_PER_BLOCK:-500}"

die() { printf '\033[1;31m[csweep][ERROR]\033[0m %s\n' "$*" >&2; exit 1; }
ok()  { printf '\033[1;32m[csweep]\033[0m %s\n' "$*"; }

_divisors() {
  local n="$1" d out=""
  for (( d = 1; d <= n; d++ )); do
    (( n % d == 0 )) && out+="${d} "
  done
  echo "${out}"
}

[[ $# -ge 1 ]] || die "no C values given (usage: csweep_validate.sh <C> [<C> ...])"

for c in "$@"; do
  [[ "${c}" =~ ^[0-9]+$ ]] || die "C='${c}' is not a positive integer"
  (( c > 0 )) || die "C='${c}' must be > 0"
  if (( TXS_PER_BLOCK % c != 0 )); then
    die "C=${c} does not evenly divide txs_per_block=${TXS_PER_BLOCK} (#337). \
Valid divisors of ${TXS_PER_BLOCK}: $(_divisors "${TXS_PER_BLOCK}")"
  fi
done

ok "All C values valid divisors of txs_per_block=${TXS_PER_BLOCK}: $*"
