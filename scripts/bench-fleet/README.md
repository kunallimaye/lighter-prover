# bench-fleet

A one-command toolkit that provisions 10 ephemeral GCE VMs in parallel,
runs the `bench` chunk-size sweep S∈{1,2,4,6} on each, collects results to
GCS, and publishes a single comparison GitHub Discussion.

See [ADR-0001](../../docs/decisions/ADR-0001-gcp-fleet-bench-architecture.md)
for the architectural rationale.

## Prereqs

The toolkit assumes you're running it from a machine that's already set up
as a "fleet orchestrator" for the `kl-ai-workstation` GCP project:

| Requirement | How to verify |
|---|---|
| `gcloud` CLI authenticated | `gcloud auth list` |
| Active SA can impersonate `bench-sweep@kl-ai-workstation.iam.gserviceaccount.com` | `gcloud --impersonate-service-account=bench-sweep@... projects describe kl-ai-workstation` |
| `gh` CLI authenticated to `github.com` | `gh auth status` |
| GCS bucket exists: `gs://kl-ai-workstation-bench-fleet-runs` | `gcloud --impersonate-service-account=bench-sweep@... storage ls gs://kl-ai-workstation-bench-fleet-runs` |
| VPC `ai-workstation-ws-net` + subnet `ai-workstation-ws-subnet` exist in `us-central1` | `gcloud --impersonate-service-account=bench-sweep@... compute networks list` |
| `python3` (Debian default) | `python3 --version` |
| `shellcheck` (optional, for dev/test) | `shellcheck --version` |

`jq` is **not** required. The toolkit uses `python3` for JSON parsing.

## Required IAM (already provisioned)

The `bench-sweep` SA needs:

- `roles/compute.instanceAdmin.v1` (create + delete VMs)
- `roles/storage.admin` on `gs://kl-ai-workstation-bench-fleet-runs`
- `roles/serviceusage.serviceUsageConsumer`
- `roles/iam.serviceAccountUser` on the default Compute SA (so it can
  attach the Compute SA to created instances).

The orchestrator's active SA needs:

- `roles/iam.serviceAccountTokenCreator` on `bench-sweep` (so it can
  impersonate).

If any of the above is missing, every `gcloud ... compute ...` call in
this toolkit will return `PERMISSION_DENIED`. Fix the IAM, don't patch
the scripts.

## One-command quickstart

```sh
# 1. Verify quotas (read-only; safe to re-run).
./scripts/bench-fleet/run-fleet.sh quota-check

# 2. Provision all 10 shapes, monitor, collect.
./scripts/bench-fleet/run-fleet.sh run --yes
# (prints a run_id like 20260610-153045-abc123 when finished)

# 3. Pull logs from GCS and parse into one TSV.
./scripts/bench-fleet/run-fleet.sh collect --run-id <run_id>

# 4. Render markdown, post Discussion, comment on #6.
./scripts/bench-fleet/run-fleet.sh publish --run-id <run_id>
```

The whole pipeline (excluding wall-clock build+sweep time) takes seconds.
A real fleet run takes ~1h wall-clock with all 10 VMs in parallel.

## Subcommand reference

| Subcommand | Purpose | Spends? |
|---|---|---|
| `quota-check` | Verify GCP vCPU quotas suffice | No (read-only API) |
| `run [--machines L] [--ref R] [--yes] [--dry-run]` | Provision + monitor + collect | Yes (full fleet) |
| `status [--run-id ID]` | Show VM state + GCS contents | No (read-only API) |
| `collect --run-id ID` | Download logs, run parser, emit TSV | No (GCS read only) |
| `publish --run-id ID` | Render markdown, create Discussion, comment on #6 | No (no GCP spend; uses GitHub API) |
| `teardown [--run-id ID] [--all]` | Force-delete leftover VMs | No (delete API only) |

### `run` flags

- `--machines c4a-highcpu-32,t2a-standard-48` — comma-separated subset
  (default: all 10 from `machines.tsv`).
- `--ref main` — git ref to build (branch or full SHA). Default `main`.
- `--yes` — skip the interactive cost-estimate confirmation.
- `--dry-run` — print the rendered `gcloud compute instances create`
  commands for every machine without executing them. No spend.

## How a run works, end to end

1. `run` generates a fresh `run_id`, resolves `--ref` to a SHA via
   `git ls-remote`, prints a per-machine cost estimate, and asks for
   confirmation (unless `--yes`).
2. For each machine, `provision_one_vm` renders the startup-script
   template with the SHA + S-values + bucket prefix, then runs
   `gcloud compute instances create` with mandatory project flags.
   Tries each `preferred_zone` in order; on `ZONE_RESOURCE_POOL_EXHAUSTED`
   it advances to the next.
3. All 10 provisions run in parallel (`bash &`/`wait`).
4. `monitor_fleet` polls GCS every 60s for `_DONE` sentinel files.
   When a sentinel appears for a machine, the orchestrator (running as
   `bench-sweep`, which has `compute.instanceAdmin.v1`) issues a
   `gcloud compute instances delete` from outside the VM.
5. On the VM: startup script runs `apt update` → install `rustup` →
   clone `lighter-prover` at the SHA → `cargo build --release -p bench`
   → loop S in `{1,2,4,6}` running `bench --tx-per-proof $S --tx-limit 480`
   → upload `/opt/results/` to GCS → write `_DONE` sentinel.
6. `collect` downloads everything under `gs://.../<run_id>/`, runs
   `parse-bench-log.sh` on every `bench-S*.log`, and writes one TSV row
   per (machine, S).
7. `publish` calls `render-discussion.sh` to assemble the markdown body,
   then uses `gh api graphql` to create a new Discussion in `Show and tell`
   and post a back-link comment on Discussion #6.

## Troubleshooting

### `PERMISSION_DENIED` on every `gcloud` call
The active SA can't impersonate `bench-sweep`. Verify with:
```sh
gcloud --impersonate-service-account=bench-sweep@kl-ai-workstation.iam.gserviceaccount.com \
  projects describe kl-ai-workstation
```
If this errors, the missing IAM is `roles/iam.serviceAccountTokenCreator`
on `bench-sweep` for your active principal.

### `ZONE_RESOURCE_POOL_EXHAUSTED` for a specific shape
The toolkit tries each preferred zone in order. If all preferred zones
for a shape are exhausted, that machine is marked `provision-failed` and
the fleet continues without it. Add more zones to the
`preferred_zones` column in `machines.tsv` for that shape and re-run.

### `quota-check` says `INSUFFICIENT`
Click the URL printed in the error message — it deep-links to the
`Quotas & System Limits` page filtered for that metric. Request a raise.
Typical lead time is minutes to hours for small bumps, days for large
ones.

### VMs aren't being deleted after upload
The orchestrator polls GCS every `FLEET_POLL_INTERVAL=60` seconds (env
override). If your orchestrator process died, run
`./run-fleet.sh teardown --run-id <id>` to force-delete. The
`--max-run-duration=8h --instance-termination-action=DELETE` flag on
each VM serves as a final safety net.

### Discussion body > 65KB GitHub limit
The renderer enforces ≤60KB via `tests/test-render.sh`. If a real run
exceeds, trim the per-machine `machine-info.txt` content embedded in
`<details>` blocks (currently the full `lscpu` + `free -h` + `rustc cfg`
dump — already capped to ~40 lines for the cfg).

### `_DONE` sentinel never appears
Look at `gs://kl-ai-workstation-bench-fleet-runs/<run_id>/<machine>/startup.log`.
The startup script tees its own stdout/stderr to `/var/log/startup.log`
and uploads that even on failure. Check for `FLEET_BUILD_FAILED` or
panics.

## Files

```
scripts/bench-fleet/
├── README.md                    (this file)
├── machines.tsv                 source-of-truth: 10 machine types + zones
├── run-fleet.sh                 top-level CLI with 6 subcommands
├── lib/
│   ├── common.sh                impersonation, logging, run-id helpers
│   ├── provision.sh             provision_one_vm() with zone fallback
│   ├── monitor.sh               monitor_fleet() — sentinel-driven teardown
│   ├── parse-bench-log.sh       text-log → one TSV row
│   └── render-discussion.sh     parsed TSV + machine-info → markdown body
├── templates/
│   ├── vm-startup.sh.tmpl       GCE startup script (placeholders)
│   └── discussion-body.md.tmpl  Discussion body skeleton
└── tests/
    ├── fixtures/
    │   ├── bench-S4-sample.log  sliced from Discussion #6
    │   └── expected-parsed.tsv  expected parser output
    ├── test-parser.sh           parser regression test
    └── test-render.sh           renderer well-formedness test
```

Run `bash tests/test-parser.sh` and `bash tests/test-render.sh` from
`scripts/bench-fleet/` to validate the parser + renderer locally before
shipping changes.

## What this toolkit is NOT

- It is **not** a Terraform module. Every VM is single-use, so the
  ceremony of Terraform state buys nothing. See ADR-0001 §Decision-6.
- It is **not** a containerization layer. The fleet builds Rust on each
  VM directly with `RUSTFLAGS="-C target-cpu=native"`. See ADR-0001
  §Decision-3.
- It does **not** sweep S ∈ {7, 8, 16, 32}. The chain recursion circuit
  at current `main` panics with `Failed to build circuit` for
  `tx_per_proof > 6` (tracked in #8). Add those S values to
  `DEFAULT_SVALUES` in `run-fleet.sh` after #8 lands.
