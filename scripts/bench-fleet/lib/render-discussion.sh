#!/usr/bin/env bash
# render-discussion.sh -- assemble a single GitHub Discussion markdown body
# from a parsed-results.tsv + a directory of per-machine machine-info.txt files.
#
# Usage:
#   render-discussion.sh <parsed-results.tsv> <machine-info-dir> [<title>]
#
# Writes the rendered markdown to stdout.
#
# parsed-results.tsv format (header line REQUIRED, tab-separated):
#   machine_type  git_sha  host  cpu  cores  ram_kb  S  chunks
#   pre_exec_ms  total_tx_ms  avg_tx_ms  total_chain_ms  avg_chain_ms
#   wall_ms  rss_kb  exit_code  status
#
# machine-info-dir layout:
#   <machine_info_dir>/<machine_type>/machine-info.txt
#   <machine_info_dir>/<machine_type>/_GCS_PREFIX   (optional: one URI per line for raw-log section)

set -euo pipefail

if [[ $# -lt 2 ]]; then
  echo "Usage: $0 <parsed-results.tsv> <machine-info-dir> [<title>]" >&2
  exit 2
fi

PARSED="$1"
INFO_DIR="$2"
TITLE="${3:-GCP fleet S∈{1,2,4,6} bench results}"

[[ -r "$PARSED" ]] || { echo "cannot read: $PARSED" >&2; exit 2; }
[[ -d "$INFO_DIR" ]] || { echo "not a directory: $INFO_DIR" >&2; exit 2; }

THIS_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
FLEET_ROOT="$(cd "${THIS_DIR}/.." && pwd)"
TEMPLATE="${FLEET_ROOT}/templates/discussion-body.md.tmpl"
MACHINES_TSV="${FLEET_ROOT}/machines.tsv"

[[ -r "$TEMPLATE" ]] || { echo "missing template: $TEMPLATE" >&2; exit 2; }

# All the actual rendering is delegated to python3 — bash is hostile to
# table assembly and HTML-detail nesting.
python3 - "$PARSED" "$INFO_DIR" "$TEMPLATE" "$MACHINES_TSV" "$TITLE" <<'PYEOF'
import os
import sys
from pathlib import Path

parsed_path, info_dir, tmpl_path, machines_tsv, title = sys.argv[1:6]
info_dir = Path(info_dir)

# -------- load parsed results --------
rows = []
with open(parsed_path) as f:
    hdr = f.readline().rstrip("\n").split("\t")
    for line in f:
        line = line.rstrip("\n")
        if not line:
            continue
        cells = line.split("\t")
        if len(cells) != len(hdr):
            # Skip malformed
            continue
        rows.append(dict(zip(hdr, cells)))

# -------- load machines.tsv (for ordering, arch labels) --------
machines = []
with open(machines_tsv) as f:
    mhdr = f.readline().rstrip("\n").split("\t")
    for line in f:
        line = line.rstrip("\n")
        if not line:
            continue
        cells = line.split("\t")
        machines.append(dict(zip(mhdr, cells)))

machine_order = [m["machine_type"] for m in machines]
arch_lookup = {m["machine_type"]: m["arch"] for m in machines}
vcpu_lookup = {m["machine_type"]: m["vcpus"] for m in machines}

# -------- helpers --------
def ms_to_s(v):
    if v == "NA" or v == "":
        return "—"
    try:
        return f"{float(v) / 1000.0:.2f}"
    except ValueError:
        return "—"

def ms_to_ms(v):
    if v == "NA" or v == "":
        return "—"
    try:
        return f"{float(v):.2f}"
    except ValueError:
        return "—"

def gb(v):
    if v == "NA" or v == "":
        return "—"
    try:
        return f"{float(v) / 1024.0 / 1024.0:.2f}"
    except ValueError:
        return "—"

def s4_speedup(wall_ms):
    if wall_ms == "NA" or wall_ms == "":
        return "—"
    try:
        ws = float(wall_ms) / 1000.0
        if ws <= 0:
            return "—"
        return f"{345.0 / ws:.2f}×"
    except ValueError:
        return "—"

# -------- TL;DR --------
sha = next((r["git_sha"] for r in rows if r["git_sha"] != "NA"), "unknown")
s_set = sorted({r["S"] for r in rows if r["S"] != "NA"}, key=lambda x: int(x))
n_machines = len({r["machine_type"] for r in rows})

tldr = (
    "## TL;DR\n\n"
    f"Ran the `bench` chunk-size sweep **S ∈ {{{','.join(s_set)}}}** on **{n_machines} GCP machine "
    f"shapes** across 3 CPU architectures (Google Axion, Ampere Altra, AMD Turin) at "
    f"commit `{sha}`. All VMs provisioned in parallel as ephemeral GCE instances and "
    f"auto-deleted on completion.\n\n"
    "See [Discussion #6](https://github.com/kunallimaye/lighter-prover/discussions/6) for "
    "the local AMD EPYC 7B13 baseline (S=4 = 345 s) used as the reference for "
    "`speedup_vs_epyc` in the comparison table below.\n"
)

# -------- methodology --------
methodology = (
    "Each machine ran an identical pipeline:\n\n"
    "1. Cold `git clone` of `lighter-prover` at the pinned SHA.\n"
    "2. Cold `cargo build --release -p bench --bin bench` with "
    "`RUSTFLAGS=\"-C target-cpu=native\"`.\n"
    "3. Sequential sweep `for S in 1 2 4 6: bench --tx-per-proof $S --tx-limit 480` "
    "(per-S 4h timeout safety cap, per-shape 8h max-run-duration).\n"
    "4. All bench logs + `machine-info.txt` uploaded to GCS, sentinel file "
    "(`_DONE`) signals orchestrator to delete the VM.\n\n"
    "Cross-machine: all 10 shapes provisioned in parallel via `gcloud compute "
    "instances create`. Per-machine: each S value runs sequentially on its VM.\n\n"
    "Provisioning template (sanitized):\n\n"
    "```\n"
    "gcloud compute instances create <name>\n"
    "  --zone=<zone> --machine-type=<shape>\n"
    "  --image-family=<debian-12[-arm64]> --image-project=debian-cloud\n"
    "  --boot-disk-size=100GB --boot-disk-type=<per-shape>  # hyperdisk-balanced for C4/N4 families, pd-balanced for T2A\n"
    "  --service-account=<compute-sa> --scopes=cloud-platform\n"
    "  --max-run-duration=8h --instance-termination-action=DELETE\n"
    "  --network=ai-workstation-ws-net --subnet=ai-workstation-ws-subnet\n"
    "  --metadata-from-file=startup-script=<rendered-template>\n"
    "  --labels=purpose=bench-fleet,owner=lighter,run-id=<run-id>,machine=<shape>\n"
    "```\n"
)

# -------- arch table --------
arch_lines = ["| Machine type | vCPUs | Architecture |", "|---|---|---|"]
for m in machines:
    arch_lines.append(f"| `{m['machine_type']}` | {m['vcpus']} | {m['arch']} |")
arch_table = "\n".join(arch_lines)

# -------- comparison table at S=4 --------
s4_rows = {r["machine_type"]: r for r in rows if r["S"] == "4"}
comp_lines = [
    "| Machine type | wall_s | total_tx_s | avg_tx_ms | total_chain_s | avg_chain_ms | peak_rss_gb | speedup_vs_epyc |",
    "|---|---:|---:|---:|---:|---:|---:|---:|",
]
for mt in machine_order:
    r = s4_rows.get(mt)
    if not r:
        comp_lines.append(f"| `{mt}` | — | — | — | — | — | — | — |")
        continue
    comp_lines.append(
        f"| `{mt}` "
        f"| {ms_to_s(r['wall_ms'])} "
        f"| {ms_to_s(r['total_tx_ms'])} "
        f"| {ms_to_ms(r['avg_tx_ms'])} "
        f"| {ms_to_s(r['total_chain_ms'])} "
        f"| {ms_to_ms(r['avg_chain_ms'])} "
        f"| {gb(r['rss_kb'])} "
        f"| {s4_speedup(r['wall_ms'])} |"
    )
comparison_table = "\n".join(comp_lines)

# -------- full sweep table --------
full_lines = [
    "| Machine type | S | chunks | pre_exec_ms | total_tx_s | avg_tx_ms | total_chain_s | avg_chain_ms | wall_s | status |",
    "|---|---:|---:|---:|---:|---:|---:|---:|---:|---|",
]
# Sort rows by (machine_order index, int(S))
def sort_key(r):
    try:
        idx = machine_order.index(r["machine_type"])
    except ValueError:
        idx = 9999
    try:
        s = int(r["S"]) if r["S"] != "NA" else 99
    except ValueError:
        s = 99
    return (idx, s)

for r in sorted(rows, key=sort_key):
    full_lines.append(
        f"| `{r['machine_type']}` "
        f"| {r['S']} "
        f"| {r['chunks']} "
        f"| {ms_to_ms(r['pre_exec_ms'])} "
        f"| {ms_to_s(r['total_tx_ms'])} "
        f"| {ms_to_ms(r['avg_tx_ms'])} "
        f"| {ms_to_s(r['total_chain_ms'])} "
        f"| {ms_to_ms(r['avg_chain_ms'])} "
        f"| {ms_to_s(r['wall_ms'])} "
        f"| {r['status']} |"
    )
full_table = "\n".join(full_lines)

# -------- per-machine details --------
per_machine_blocks = []
for mt in machine_order:
    info_path = info_dir / mt / "machine-info.txt"
    if info_path.is_file():
        info_content = info_path.read_text(errors="replace").strip()
    else:
        info_content = "(machine-info.txt not collected for this shape)"
    per_machine_blocks.append(
        f"<details><summary><code>{mt}</code> — machine info</summary>\n\n"
        f"```\n{info_content}\n```\n\n"
        f"</details>"
    )
per_machine = "\n\n".join(per_machine_blocks)

# -------- raw logs --------
raw_log_lines = []
for mt in machine_order:
    prefix_file = info_dir / mt / "_GCS_PREFIX"
    if prefix_file.is_file():
        for line in prefix_file.read_text().splitlines():
            line = line.strip()
            if line:
                raw_log_lines.append(f"- `{mt}`: `{line}`")
    else:
        raw_log_lines.append(f"- `{mt}`: (not uploaded)")
raw_logs = (
    "Downloads require `gcloud storage cp` with bench-sweep impersonation, e.g.:\n\n"
    "```\ngcloud --impersonate-service-account=bench-sweep@kl-ai-workstation.iam.gserviceaccount.com "
    "storage cp -r <uri> .\n```\n\n"
    + "\n".join(raw_log_lines)
)

# -------- reproduction --------
repro = (
    "```sh\n"
    "git clone https://github.com/kunallimaye/lighter-prover\n"
    "cd lighter-prover\n"
    f"git checkout {sha}\n"
    "./scripts/bench-fleet/run-fleet.sh quota-check\n"
    "./scripts/bench-fleet/run-fleet.sh run --yes\n"
    "./scripts/bench-fleet/run-fleet.sh publish --run-id <id>\n"
    "```\n\n"
    "See `scripts/bench-fleet/README.md` for prereqs (gcloud auth, GCS bucket, "
    "bench-sweep SA + impersonation) and full subcommand reference.\n"
)

# -------- assemble --------
tmpl = Path(tmpl_path).read_text()
out = (
    tmpl.replace("__TL_DR__", tldr)
        .replace("__METHODOLOGY__", methodology)
        .replace("__ARCH_TABLE__", arch_table)
        .replace("__COMPARISON_TABLE__", comparison_table)
        .replace("__FULL_TABLE__", full_table)
        .replace("__PER_MACHINE_DETAILS__", per_machine)
        .replace("__RAW_LOG_LINKS__", raw_logs)
        .replace("__REPRO__", repro)
)

sys.stdout.write(out)
PYEOF
