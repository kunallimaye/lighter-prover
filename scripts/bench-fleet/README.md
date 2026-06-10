# bench-fleet

A one-command toolkit that provisions 10 ephemeral GCE VMs in parallel,
runs the `bench` chunk-size sweep S∈{1,2,4,6} on each via **prebuilt
per-microarch containers**, collects results to GCS, and publishes a
single comparison GitHub Discussion.

Since #33 the fleet is **container-based**: VMs run Container-Optimized
OS and pull cross-compiled images from Artifact Registry — nothing is
built on the VMs. All infrastructure lives in the `kunal-scratch` GCP
project, configured through the repo-root `config.toml`.

See [ADR-0001](../../docs/decisions/ADR-0001-gcp-fleet-bench-architecture.md)
(including the #33 container-pivot addendum) for the architectural
rationale.

## Configuration

The repo-root `config.toml` (gitignored; copy from `config.toml.example`)
is the single source of truth:

```toml
[project]
name = "lighter-prover"

[gcp.defaults]
project = "kunal-scratch"
region  = "us-central1"

[fleet]
results_bucket = "gs://kunal-scratch-bench-fleet-runs"
svalues        = "1 2 4 6"
tx_limit       = 480
```

Derived defaults (env vars override everything):

| Variable | Default | Meaning |
|---|---|---|
| `PROJECT` | `[gcp.defaults].project` | GCP project for VMs, AR, GCS |
| `REGION` | `[gcp.defaults].region` | Region for quotas + bucket |
| `GCS_BUCKET` | `[fleet].results_bucket` → `gs://<project>-bench-fleet-runs` | Results + sentinels |
| `NETWORK` / `SUBNET` | `default` / `default` | kunal-scratch AUTO-mode VPC |
| `AR_IMAGE_BASE` | `<region>-docker.pkg.dev/<project>/lighter-prover/bench` | Prebuilt image base |
| `TX_LIMIT` | `[fleet].tx_limit` → `480` | `LIGHTER_TX_LIMIT` for every run |
| `BENCH_SWEEP_SA` | *(empty)* | Legacy impersonation — off by default (#32: the old `bench-sweep` SA is gone) |

## Prereqs

1. **gcloud authenticated** as the orchestrator identity
   (`gcloud auth list`), with `gcloud config set project kunal-scratch`.
2. **One-time Owner-tier bootstrap** — run once with Owner credentials:

   ```sh
   CONFIRM=yes make admin-cloud-init
   ```

   The fleet steps of that bootstrap (idempotent, resumable):
   - create `gs://kunal-scratch-bench-fleet-runs` (us-central1, uniform
     bucket-level access);
   - grant the orchestrator SA `roles/iam.serviceAccountUser` on the
     project (attach the Compute SA to VMs) and `roles/storage.admin`
     on the bucket;
   - grant the default Compute SA (`<projectNumber>-compute@developer.gserviceaccount.com`)
     `roles/storage.objectAdmin` on the bucket (issue #23: without it
     every VM upload 403s) and `roles/artifactregistry.reader` on the
     project (image pulls).

   The orchestrator SA already holds `compute.instanceAdmin.v1`,
   `artifactregistry.admin`, and Cloud Build submit rights on
   kunal-scratch (verified #33) — those are not re-granted.
3. **Prebuilt image matrix** for the SHA you want to benchmark:

   ```sh
   make cloud-bench-build
   ```

   pushes `:<sha>` (+`:latest`, portable multi-arch) and the three
   microarch variants `:<sha>-znver5`, `:<sha>-neoverse-v2`,
   `:<sha>-neoverse-n1` to Artifact Registry. `run-fleet.sh run`
   verifies the tags exist before any spend.
4. `gh` CLI authenticated to `github.com` (publish step only).
5. `python3` (Debian default). `shellcheck` optional for dev. `jq` is
   **not** required.

If IAM is missing, every `gcloud` call returns `PERMISSION_DENIED`.
Fix the IAM (`make admin-cloud-init`), don't patch the scripts.

## Pre-run checklist

Before typing `make fleet-run`, confirm all three:

1. **`make fleet-quota-check` passes.** It verifies vCPU quotas, bucket
   existence, an orchestrator-side write probe, AND the VM-side Compute
   SA bucket grant (issues #19/#23 — both burned real money).
2. **Terminal survives the wall.** A full sweep takes hours. Run
   `make fleet-run` under `tmux`, `screen`, or `nohup … &` so a dropped
   SSH session does not orphan the orchestrator (and the VMs it should
   delete).
3. **You understand the spend commitment.** Realistic full-sweep cost is
   $80-150 (the `fleet-run` cost estimate prints a per-shape breakdown
   before any spend). `--max-run-duration=10h` per VM is the final
   safety net. Note: no more on-VM builds means each VM saves ~15-20 min
   of toolchain+compile wall time vs the pre-#33 flow.

## Quickstart (via Makefile)

```sh
# 0. One-time (Owner creds): bucket + IAM
CONFIRM=yes make admin-cloud-init

# 1. Build the per-microarch image matrix for HEAD
make cloud-bench-build

# 2. Verify quotas + bucket + IAM (no spend)
make fleet-quota-check

# 3. Dry-run to inspect the 10 gcloud commands (no spend)
make fleet-run-dry

# 4. Provision + monitor + collect (~$80-150, runs S in {1,2,4,6} on all 10 shapes)
make fleet-run
# Note the RUN_ID printed in the output — needed for the next two steps.

# 5. Parse logs to TSV
make fleet-collect RUN_ID=<id-from-step-4>

# 6. Publish Discussion + comment on Discussion #6
make fleet-publish RUN_ID=<id-from-step-4>

# Emergency cleanup if anything goes wrong:
make fleet-teardown RUN_ID=<id>          # specific run only
make fleet-teardown                       # all leftover fleet VMs
```

### Calling the script directly

```sh
# Only two machines, alternate ref, interactive confirmation:
./scripts/bench-fleet/run-fleet.sh run \
    --machines c4a-highcpu-32,t2a-standard-32 \
    --ref feature/some-branch

# Smoke test a single shape with one S value:
./scripts/bench-fleet/run-fleet.sh run --machines c4d-highcpu-64 --svalues "4" --yes
```

## Subcommand reference

| Subcommand | Purpose | Spends? |
|---|---|---|
| `quota-check` | Verify quotas, bucket, write-probe, VM-side IAM | No (read-only API) |
| `run [--machines L] [--ref R] [--svalues "L"] [--yes] [--dry-run]` | Provision + monitor + collect | Yes (full fleet) |
| `status [--run-id ID]` | Show VM state + GCS contents | No |
| `collect --run-id ID` | Download logs, run parser, emit TSV | No |
| `publish --run-id ID` | Render markdown, create Discussion, comment on #6 | No GCP spend |
| `teardown [--run-id ID] [--all]` | Force-delete leftover VMs | No (delete API only) |

## How a run works, end to end (#33 container flow)

1. `run` generates a `run_id`, resolves `--ref` to a SHA, **verifies the
   per-microarch image tags exist in Artifact Registry**, prints a cost
   estimate, and asks for confirmation (unless `--yes`).
2. For each machine, `provision_one_vm` resolves the image
   (`machines.tsv` `image_tag` column: `znver5` for c4d/n4d, `neoverse-v2`
   for c4a/n4a, `neoverse-n1` for t2a), renders the COS startup template,
   and runs `gcloud compute instances create` (COS image family per
   arch, zone fallback on stockout).
3. On the VM (Container-Optimized OS — docker preinstalled, no gcloud):
   `docker-credential-gcr` auths Artifact Registry via the VM's metadata
   identity → `docker pull` (3 retries) → machine-info captured (image
   URI + digest provenance; nothing is compiled) → one worker container
   per S value with `LIGHTER_TX_PER_PROOF=$S`, `LIGHTER_TX_LIMIT`,
   `BENCH_BUCKET`, `BENCH_PREFIX=<run-id>/<machine>/S<S>`.
4. **Each container uploads its own results** (`bench.log`,
   `bench.jsonl`, per-S `DONE`) via the entrypoint's #25 contract. After
   the loop the VM uploads `machine-info.txt`, `svalues-summary.txt`,
   `startup.log`, and the fleet-level `_DONE` sentinel (gcloud runs
   inside a container — COS has none on the host; falls back to
   `google/cloud-sdk:slim` if the bench image pull failed so
   diagnostics still land).
5. `monitor_fleet` polls `gs://…/<run-id>/<machine>/_DONE` every 60s and
   deletes each VM when its sentinel appears.
6. `collect` downloads everything under `gs://…/<run_id>/`, runs
   `parse-bench-log.sh` on every `S*/bench.log`, and writes one TSV row
   per (machine, S).
7. `publish` renders the Discussion body and posts it.

## GCS layout per run

```
gs://kunal-scratch-bench-fleet-runs/<run-id>/<machine>/
├── S1/bench.log bench.jsonl DONE     (per-S, uploaded by the container)
├── S2/… S4/… S6/…
├── machine-info.txt                  (fleet-level, uploaded by the VM)
├── svalues-summary.txt               (rc + wall seconds per S)
├── startup.log
├── status.txt
└── _DONE                             (fleet sentinel — monitor polls this)
```

## Troubleshooting

### `PERMISSION_DENIED` on every `gcloud` call
The active account lacks the kunal-scratch grants. Run
`CONFIRM=yes make admin-cloud-init` with Owner credentials, then
`make fleet-quota-check`.

### `image not found` during run preflight
The matrix wasn't built for that SHA. `make cloud-bench-build` (builds
from HEAD; pass `--ref <sha>` to the fleet matching the built SHA).

### `ZONE_RESOURCE_POOL_EXHAUSTED` for a specific shape
The toolkit tries each preferred zone in order. If all are exhausted,
that machine is marked `provision-failed` and the fleet continues. Add
zones to `preferred_zones` in `machines.tsv` and re-run.

### `quota-check` says `INSUFFICIENT` / `NOT-LISTED`
`INSUFFICIENT`: click the printed deep-link and request a raise.
`NOT-LISTED`: the quota family isn't enumerated in the region describe
(common for newer families like C4A/N4A/N4D/C4D) — creation may still
succeed; the warning just means we can't pre-verify.

### VMs aren't being deleted after upload
The orchestrator polls GCS every `FLEET_POLL_INTERVAL=60` seconds. If
your orchestrator died, `./run-fleet.sh teardown --run-id <id>`.
`--max-run-duration=10h --instance-termination-action=DELETE` is the
final safety net.

### `_DONE` sentinel never appears
Look at `gs://kunal-scratch-bench-fleet-runs/<run_id>/<machine>/startup.log`
(uploaded even on failure) and the per-S `DONE` files (they contain
`bench_exit_code=` + `upload_status=`). Statuses are honest by design
(#23): `pull-failed`, `bench-failed-<n>`, `…+upload-failed-rc<n>`.

### Discussion body > 65KB GitHub limit
The renderer enforces ≤60KB via `tests/test-render.sh`. Trim the
per-machine `machine-info.txt` content if a real run exceeds it.

## Files

```
scripts/bench-fleet/
├── README.md                    (this file)
├── machines.tsv                 source-of-truth: 10 machine types, zones, image tags
├── run-fleet.sh                 top-level CLI with 6 subcommands
├── lib/
│   ├── common.sh                config.toml resolution, logging, run-id helpers
│   ├── provision.sh             provision_one_vm() with zone fallback + image tag resolution
│   ├── monitor.sh               monitor_fleet() — sentinel-driven teardown
│   ├── parse-bench-log.sh       bench log → one TSV row (container + legacy formats)
│   └── render-discussion.sh     parsed TSV + machine-info → markdown body
├── templates/
│   ├── vm-startup.sh.tmpl       COS startup script (docker pull + per-S containers)
│   └── discussion-body.md.tmpl  Discussion body skeleton
└── tests/
    ├── fixtures/
    ├── test-parser.sh           parser regression test
    └── test-render.sh           renderer well-formedness test
```

Run `bash tests/test-parser.sh` and `bash tests/test-render.sh` from
`scripts/bench-fleet/` to validate the parser + renderer locally before
shipping changes.

## What this toolkit is NOT

- It is **not** a Terraform module. Every VM is single-use, so the
  ceremony of Terraform state buys nothing. See ADR-0001 §Decision-6.
- It does **not** build anything on VMs anymore (#33 reversed ADR-0001
  §Decision-3). Native-codegen honesty is preserved through explicit
  per-microarch image tags (`znver5`, `neoverse-v2`, `neoverse-n1`)
  cross-compiled on x86 Cloud Build workers.
- It does **not** sweep S ∈ {7, 8, 16, 32}. The chain recursion circuit
  panics for `tx_per_proof > 6` (tracked in #8). Add those S values to
  `[fleet].svalues` in config.toml after #8 lands.
