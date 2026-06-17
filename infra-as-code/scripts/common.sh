#!/usr/bin/env bash
set -euo pipefail

# ─── Common Logging & Color Utilities ─────────────────────────────────

_log_info()  { printf '\033[1;34m[INFO]\033[0m %s\n' "$*"; }
_log_ok()    { printf '\033[1;32m[OK]\033[0m %s\n' "$*"; }
_log_error() { printf '\033[1;31m[ERROR]\033[0m %s\n' "$*" >&2; }
_die()       { _log_error "$@"; exit 1; }

# ─── Shared Project Paths & Constants ─────────────────────────────────

ROOT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
CONFIG_PATH="${ROOT_DIR}/${CONFIG_TOML:-config.toml}"
LOCAL_ZKP_IMAGE="${LOCAL_ZKP_IMAGE:-lighter-zkp-prover:latest}"
ZKP_DOCKERFILE="${ZKP_DOCKERFILE:-Dockerfile.zkp}"

_require_file() {
  if [[ ! -f "$1" ]]; then
    _die "Required file not found: $1"
  fi
}
