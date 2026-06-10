#!/usr/bin/env bash
# parse-bench-log.sh -- extract one TSV row from a single bench-S<N>.log file.
#
# Output columns (tab-separated, NO header — header is added by collect step):
#   git_sha host cpu cores ram_kb S chunks pre_exec_ms total_tx_ms avg_tx_ms \
#   total_chain_ms avg_chain_ms wall_ms rss_kb exit_code status
#
# Missing fields are emitted as "NA". Status semantics:
#   ok      = all metrics present + exit_code=0
#   panic   = log contains "panicked at"
#   timeout = exit_code=124
#   error   = anything else
#
# Usage:
#   parse-bench-log.sh <bench-S4.log>
#   parse-bench-log.sh --help

set -euo pipefail

if [[ "${1:-}" == "--help" || "${1:-}" == "-h" || $# -eq 0 ]]; then
  cat <<'EOF'
Usage: parse-bench-log.sh <bench-log-file>

Emits a single tab-separated row describing one bench run.
EOF
  exit 0
fi

LOG="$1"
[[ -r "$LOG" ]] || { echo "cannot read: $LOG" >&2; exit 2; }

# Use python3 for the whole parse. Bash regex + float math gets cumbersome
# and python3 is on the Debian base image we target.
python3 - "$LOG" <<'PYEOF'
import json
import re
import sys
from pathlib import Path

log_path = Path(sys.argv[1])
text = log_path.read_text(errors="replace")

# Default field values.
fields = {
    "git_sha": "NA",
    "host": "NA",
    "cpu": "NA",
    "cores": "NA",
    "ram_kb": "NA",
    "S": "NA",
    "chunks": "NA",
    "pre_exec_ms": "NA",
    "total_tx_ms": "NA",
    "avg_tx_ms": "NA",
    "total_chain_ms": "NA",
    "avg_chain_ms": "NA",
    "wall_ms": "NA",
    "rss_kb": "NA",
    "exit_code": "NA",
    "status": "error",
}

# ---------- BENCH_META ----------
# BENCH_META host=phali cpu="AMD EPYC 7B13" cores=32 ram=131904212 kB git_sha=unknown tx_per_proof=4 tx_limit=480
meta_re = re.compile(
    r'BENCH_META\s+'
    r'host=(?P<host>\S+)\s+'
    r'cpu="(?P<cpu>[^"]*)"\s+'
    r'cores=(?P<cores>\d+)\s+'
    r'ram=(?P<ram>\d+)\s*kB\s+'
    r'git_sha=(?P<sha>\S+)\s+'
    r'tx_per_proof=(?P<S>\d+)\s+'
    r'tx_limit=(?P<lim>\d+)'
)
m = meta_re.search(text)
if m:
    fields["host"]    = m.group("host")
    fields["cpu"]     = m.group("cpu")
    fields["cores"]   = m.group("cores")
    fields["ram_kb"]  = m.group("ram")
    fields["git_sha"] = m.group("sha")
    fields["S"]       = m.group("S")

# ---------- chunks ----------
# "...so there will be 120 iterations of proving."
chunks_re = re.compile(r'there will be\s+(\d+)\s+iterations of proving')
m = chunks_re.search(text)
if m:
    fields["chunks"] = m.group(1)

# ---------- duration parser ----------
# bench prints durations as Rust's Duration Debug format:
#   "277.131220603s", "577.356709ms", "1.176925208s", "823.4µs", "12.5us"
DUR_RE = re.compile(r'^(?P<num>\d+(?:\.\d+)?)(?P<unit>ms|µs|us|ns|s)$')

def to_ms(token):
    """Convert one duration token (e.g. '577.356709ms') to milliseconds (float)."""
    mm = DUR_RE.match(token.strip())
    if not mm:
        return None
    n = float(mm.group("num"))
    unit = mm.group("unit")
    if unit == "s":   return n * 1000.0
    if unit == "ms":  return n
    if unit == "µs" or unit == "us": return n / 1000.0
    if unit == "ns":  return n / 1_000_000.0
    return None

def find_metric(label):
    """Find the duration token after `label`. Returns float ms or None."""
    # The colon may have multiple spaces after it ("time:   277.131220603s")
    pat = re.compile(re.escape(label) + r'\s*([0-9][0-9.]*(?:ms|µs|us|ns|s))')
    mm = pat.search(text)
    if not mm:
        return None
    return to_ms(mm.group(1))

# Map metric label -> output field
metric_map = [
    ("TOTAL BlockPreExecutionCircuit::prove time:",  "pre_exec_ms"),
    ("TOTAL BlockTxCircuit::prove time:",            "total_tx_ms"),
    ("AVERAGE BlockTxCircuit::prove time:",          "avg_tx_ms"),
    ("TOTAL BlockTxChainCircuit::prove time:",       "total_chain_ms"),
    ("AVERAGE BlockTxChainCircuit::prove time:",     "avg_chain_ms"),
]
for label, fld in metric_map:
    v = find_metric(label)
    if v is not None:
        # Round to 3 decimal places (microsecond precision) and strip trailing zeros.
        fields[fld] = f"{v:.3f}"

# ---------- wall + exit code (set by startup-script wrapper) ----------
# S4_WALL_SECONDS=345
wall_re = re.compile(r'^S\d+_WALL_SECONDS=(\d+)\s*$', re.MULTILINE)
m = wall_re.search(text)
if m:
    fields["wall_ms"] = str(int(m.group(1)) * 1000)

exit_re = re.compile(r'^S\d+_EXIT_CODE=(\d+)\s*$', re.MULTILINE)
m = exit_re.search(text)
if m:
    fields["exit_code"] = m.group(1)

# ---------- BENCH_EVENT JSONL (issue #21) ----------
# Current main (PR #18) emits structured `BENCH_EVENT {json}` lines in
# addition to the legacy INFO text. The legacy regexes above still match
# (validated against a real current-main run), so BENCH_EVENT is used to
# fill fields the text scrape cannot provide (rss_kb from the summary
# event's peak_rss_mb) and as a fallback for S / chunks.
summary = None
for line in text.splitlines():
    if line.startswith("BENCH_EVENT "):
        try:
            ev = json.loads(line[len("BENCH_EVENT "):])
        except json.JSONDecodeError:
            continue
        if ev.get("event") == "summary":
            summary = ev  # last summary wins (there should be exactly one)
if summary is not None:
    if fields["rss_kb"] == "NA" and summary.get("peak_rss_mb") is not None:
        fields["rss_kb"] = str(int(summary["peak_rss_mb"]) * 1024)
    if fields["S"] == "NA" and summary.get("tx_per_proof") is not None:
        fields["S"] = str(summary["tx_per_proof"])
    if fields["chunks"] == "NA" and summary.get("chunks") is not None:
        fields["chunks"] = str(summary["chunks"])

# ---------- status ----------
has_panic = "panicked at" in text
all_metrics_present = all(
    fields[k] != "NA"
    for k in ("pre_exec_ms", "total_tx_ms", "avg_tx_ms", "total_chain_ms", "avg_chain_ms")
)
exit_code_int = None
if fields["exit_code"] != "NA":
    try:
        exit_code_int = int(fields["exit_code"])
    except ValueError:
        pass

if has_panic:
    fields["status"] = "panic"
elif exit_code_int == 124:
    fields["status"] = "timeout"
elif exit_code_int == 0 and all_metrics_present:
    fields["status"] = "ok"
elif exit_code_int is None and all_metrics_present:
    # No wrapper SX_EXIT_CODE line but full metrics present (e.g. fixture
    # files harvested from a bare run). Treat as ok.
    fields["status"] = "ok"
else:
    fields["status"] = "error"

# ---------- emit ----------
order = [
    "git_sha", "host", "cpu", "cores", "ram_kb",
    "S", "chunks",
    "pre_exec_ms", "total_tx_ms", "avg_tx_ms",
    "total_chain_ms", "avg_chain_ms",
    "wall_ms", "rss_kb", "exit_code", "status",
]
print("\t".join(fields[k] for k in order))
PYEOF
