# Lighter Prover and Circuits

A fork of [`elliottech/lighter-prover`](https://github.com/elliottech/lighter-prover)
with throughput-benchmarking infrastructure layered on top.

## What this fork adds

- **CLI flags + env overrides for `bench`** (#4) so a single binary
  covers the chunk-size sweep `S ∈ {1, 2, 4, 8, 16, 32}` without
  hard-coded constants.
- **Containerized fan-out throughput benchmark** (#2, Phase 1) — one
  OCI image, two roles (worker / orchestrator), local and GCP runners.
- **Truthful image provenance** — every container build derives its
  `LIGHTER_REF` / `GIT_SHA` env var, OCI `image.revision` label, and
  `:ref-<short>` tag from the actual git SHA of the source tree baked
  in (Cloud Build uses `$COMMIT_SHA`; local podman uses
  `git rev-parse HEAD`). See
  [ADR-0001 §Revision 1](docs/decisions/ADR-0001-container-topology.md#revision-1-2026-06-10-tag-provenance-fix).

Phase 2 (#3) will replace the embarrassingly-parallel fan-out with true
work-sharding across layer-1 chunks. Phase 1 ships fan-out only.

## Quickstart — local (Podman)

Requires: `podman` and `python3` (for the orchestrator).

```bash
# 1. Build the bench image (~10-15 min on first build; cached after)
make container-build

# 2. Smoke test — tx_per_proof=1, tx_limit=4, < 1 min
make container-test

# 3. Full single-worker bench (default tx_per_proof=4, tx_limit=480)
make container-bench

# 4. N-worker fan-out — 4 concurrent worker containers
make container-fanout N=4
```

The fan-out target spawns N podman workers in parallel, captures their
stdout, parses every `TOTAL ... ::prove time` and `AVERAGE ... ::prove
time` line emitted by the bench binary, and prints aggregate stats:

```
========================================================================
FAN-OUT SUMMARY  backend=podman workers=4 completed=4 failed=0
image=localhost/lighter-bench:latest
========================================================================
label                                                     n       mean        p50        p95      stdev
------------------------------------------------------------------------
AVERAGE BlockTxCircuit::prove time                        4    2.040s     2.040s     2.070s     0.014s
TOTAL BlockPreExecutionCircuit::prove time                4   12.300s    12.200s    12.600s     0.200s
TOTAL BlockTxChainCircuit::prove time                     4   88.200s    88.100s    89.000s     0.400s
TOTAL BlockTxCircuit::prove time                          4  245.100s   244.900s   248.300s     1.400s
========================================================================
```

### Knobs

| Variable           | Default                                       | Effect                                                  |
|--------------------|-----------------------------------------------|---------------------------------------------------------|
| `TX_PER_PROOF`     | `4`                                           | Chunk size handed to `bench` (must be ≤ 6, per #4)      |
| `TX_LIMIT`         | `480`                                         | Tx cap; bench aligns down to multiple of `TX_PER_PROOF` |
| `N`                | `1`                                           | Worker count for fan-out targets                        |
| `BENCH_REPEAT`     | `1`                                           | Times each worker repeats the bench pipeline            |
| `TARGET_CPU_NATIVE`| `0`                                           | `1` enables `-C target-cpu=native` (non-portable image; deprecated alias for `TARGET_CPU=native`) |

Since #33 the CI pipeline (`make cloud-bench-build`) builds a full
matrix: a portable multi-arch `:<sha>`/`:latest` manifest plus three
cross-compiled per-microarch variants (`:<sha>-znver5`,
`:<sha>-neoverse-v2`, `:<sha>-neoverse-n1`) consumed by the bench-fleet
(see `scripts/bench-fleet/README.md`). The image's `LIGHTER_REF` /
`GIT_SHA` / OCI `image.revision` label and the `:<sha>` tag are derived
from `git rev-parse HEAD` (local) or `$COMMIT_SHA` (Cloud Build), not
from a user-supplied knob — so the tag always names the source actually
baked in.

Example: 8-worker fan-out at chunk size 2 with native-CPU build:

```bash
make container-build TARGET_CPU_NATIVE=1
make container-fanout N=8 TX_PER_PROOF=2 TX_LIMIT=480
```

## Quickstart — host (no container)

For iterating on the bench code without rebuilding the image. Requires:
a working Rust nightly toolchain (auto-installed from `rust-toolchain`).

```bash
make local-build         # cargo build --release -p bench --bin bench
make local-test          # smoke test
make local-bench         # full single-process bench
make local-fanout N=4    # 4 parallel bench processes; same parser as container fan-out
```

## Quickstart — Google Cloud

Phase 1 ships the image to Artifact Registry; one-shot Cloud Run Jobs
invocation runs the worker in GCP and writes timings to Cloud Logging.

```bash
# One-time bootstrap (Owner-tier; enables APIs, creates AR repo + builder SA)
make admin-cloud-init

# Apply the Phase 1 Terraform module (AR repo + IAM)
make cloud-infra

# Build + push the bench image (Cloud Build → Artifact Registry)
make cloud-bench-build

# Run a worker once in Cloud Run Jobs (manual one-shot for Phase 1)
gcloud run jobs create lighter-bench-worker \
  --image=us-central1-docker.pkg.dev/<project>/lighter-prover/bench:latest \
  --region=us-central1 \
  --set-env-vars=LIGHTER_ROLE=worker,LIGHTER_TX_PER_PROOF=4,LIGHTER_TX_LIMIT=480 \
  --cpu=4 --memory=8Gi --max-retries=0
gcloud run jobs execute lighter-bench-worker --region=us-central1 --wait
```

Cloud Run Jobs streams the worker's stdout (including the bench's
`TOTAL` / `AVERAGE` lines) directly to Cloud Logging:

```bash
gcloud logging read \
  'resource.type=cloud_run_job AND resource.labels.job_name=lighter-bench-worker' \
  --limit=200 --format='value(textPayload)'
```

A Cloud Run Jobs fan-out wrapper is a Phase 2 deliverable; for Phase 1,
N parallel Cloud Run Jobs are spawned by an operator-side loop:

```bash
for i in $(seq 1 4); do
  gcloud run jobs execute lighter-bench-worker --region=us-central1 &
done
wait
```

## Configuration

Cloud topology is configured in `config.toml` (copy from
`config.toml.example`). The three roles (orchestration / build / runtime)
can all collapse to one GCP project for personal use; the split is a
config edit, not a refactor. See
[`docs/decisions/ADR-0001-container-topology.md`](docs/decisions/ADR-0001-container-topology.md)
for the rationale.

## Project layout

```
├── bench/                          # Bench crate (patched per #4)
├── cicd/
│   ├── Containerfile               # Multi-stage Rust build → debian-slim runtime
│   ├── .dockerignore
│   ├── entrypoint.sh               # Two roles: worker | orchestrator
│   ├── orchestrator.py             # Fan-out + timing aggregation (Phase 1)
│   ├── cloudbuild.yaml             # Build + push the bench image
│   ├── cloudbuild-plan.yaml        # TF plan via Cloud Build
│   ├── cloudbuild-apply.yaml       # TF apply via Cloud Build
│   └── terraform/                  # AR repo + IAM (Phase 1 scope)
├── docs/decisions/
│   ├── ADR-0001-container-topology.md
│   └── ADR-template-cloud-topology.md
├── scripts/                        # Modular shell + Python helpers
│   ├── common.sh                   # Shared helpers + Tier-2 detached orchestration
│   ├── local.sh                    # Host cargo flows
│   ├── container.sh                # Podman flows
│   ├── cloud.sh                    # gcloud + Cloud Build flows
│   └── config.py                   # config.toml → shell exports + TF_VAR_*
├── Makefile                        # Operator entry point — see `make help`
├── config.toml.example
└── .env.example
```

## License

BUSL-1.1 — inherited from upstream `elliottech/lighter-prover`. See
[`LICENSE`](LICENSE) and [`THIRD_PARTY_NOTICES`](THIRD_PARTY_NOTICES).
