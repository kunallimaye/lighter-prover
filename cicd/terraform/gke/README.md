# GKE Autopilot deployment automation (issue #151, G4 enabler)

Parametrised, config-driven Terraform that stands up the distributed
prover's **deployment topology** on **GKE Autopilot**, per:

- **ADR-0006** — the two-machine-class topology (chunk-prover **cells** +
  **coordinators**).
- **ADR-0003 amendment (platform decision — GKE Autopilot, 2026-06-13)** —
  the platform decision and its **HARD DAY-1 eviction mitigation**.

This module is a **platform backend on the ADR seam** (ADR-0003 amendment
§5 / ADR-0005 §5 / ADR-0006 §5): a new Terraform root, not a redesign. A
future GKE-Standard or MIG backend is a sibling root selected by the
`platform` field — this one is the `gke-autopilot` backend.

## What it provisions

| Resource | Purpose |
|---|---|
| `google_container_cluster` (Autopilot) | Managed cluster; Google bin-packs nodes. |
| `google_pubsub_topic` + `subscription` | The block-dispatch backlog signal (ADR-0006 §1.1). |
| `kubernetes_deployment.cells` | **Machine CLASS 1** — chunk-prover cells (CPU-saturating; c4a/Axion in prod). |
| `kubernetes_deployment.coordinator` | **Machine CLASS 2** — coordinators (fold L2 + prove L4; distinct class). |
| `kubernetes_pod_disruption_budget_v1.coordinator` | **HARD DAY-1** — PDB for the coordinator pool. |
| coordinator pod annotation `safe-to-evict=false` | **HARD DAY-1** — hardwired in `locals`, cannot be turned off by tfvars. |
| `kubernetes_horizontal_pod_autoscaler_v2.backlog` | HPA on `num_undelivered_messages` external metric. |
| metrics adapter (applied by the deploy pipeline) | custom-metrics-stackdriver-adapter — the external-metrics path. |

## The two machine classes are sized SEPARATELY (never summed)

Each class has its own machine-shape, resource-request, and replica-count
variables. The cell tier is the large, CPU-saturating fleet; the
coordinator tier is ~1% of it but a **distinct** compute class.

## Which image tag wires to each class

| Class | Smoke (validation) | Production |
|---|---|---|
| **cells** | `registry.k8s.io/pause:3.10` (trivial no-op; we validate the automation, not the arm64 build) | `…/bench:<sha>-neoverse-v2` — the **arm64/Axion** image emitted by `cicd/cloudbuild.yaml` (arm64 cross-compile landmine already solved there). |
| **coordinators** | `busybox:1.36` sleeping stub (a real process to drain against) | `…/bench:<sha>-neoverse-v2` in coordinator mode. |

The arm64/Axion build is **reused, not rebuilt** — `cicd/cloudbuild.yaml`
already cross-compiles aarch64 (neoverse-v2) natively on x86 Cloud Build
workers and emits the `:<sha>-neoverse-v2` per-microarch tag.

## Smoke vs production — same variable surface, no structural change

- **`smoke.tfvars`** — tiny counts, trivial workloads. What CI actually
  applies for the smoke validation.
- **`production.tfvars.example`** — same variables, **placeholder** sizes
  marked `# filled from the sizing model (#95) — placeholder sizes, do
  not apply`. Proves the module accepts production sizes without a
  redesign. The parameterised RUN against real data is gated on **G2** +
  sizing **#95** and is OUT OF SCOPE here.

## Operating it

```sh
# Stand up + live-validate (cluster, both classes, eviction mitigation, HPA):
make gke-smoke-up   GKE_PROJECT=<proj> GKE_BUILD_SA=<gke-capable-sa>

# Tear down + verify nothing remains:
make gke-smoke-down GKE_PROJECT=<proj> GKE_BUILD_SA=<gke-capable-sa>
```

Both run via **Cloud Build** as a GKE-capable service account (the
existing Cloud-Build-drives-Terraform idiom). `terraform fmt` and
`terraform validate` are clean; see `cicd/cloudbuild-gke-smoke.yaml` and
`cicd/cloudbuild-gke-teardown.yaml`.

## Tier-2 scale run — set region + project ONCE (issue #216)

The `scale-0p2pct.tfvars` / `scale-0p3pct.tfvars` / `scale-0p5pct.tfvars`
configs deploy a **real proving** run. They carry **no** literal
`PROJECT`/`SHA` tokens to hand-edit:

- **Image SHA is pinned.** `cell_image`/`coordinator_image` point at a REAL
  arm64 (`-neoverse-v2`/Axion) `bench` image that already EXISTS in Artifact
  Registry — **no build needed**. Currently pinned:
  `us-central1-docker.pkg.dev/kunal-scratch/lighter-prover/bench:1d5036b5369fcf6a966738e6de8265b6e5a6e800-neoverse-v2`.
  Re-pin to a newer `cicd/cloudbuild.yaml` output as the bench binary advances
  (`gcloud artifacts docker tags list .../lighter-prover/bench --filter="tag~neoverse-v2"`).
- **Project + proof bucket are auto-wired.** `--project` and `--proof-bucket`
  are **not** in the command args; Terraform injects `LIGHTER_PROJECT`
  (= `var.project_id`) and `LIGHTER_PROOF_BUCKET` (= the resolved proof-store
  bucket name) as container env vars the `bench` binary reads
  (`main.tf` `local.prover_wiring_env`).
- **The proof bucket is co-regional.** `proof_store_location` defaults to
  empty and **follows `var.region`** — a single `region` override keeps the
  bucket co-located with the coordinator that folds the proofs (a cross-region
  bucket taxes the serial L4 stage). Override `proof_store_location` only to
  deliberately place the bucket elsewhere.

So a us-east4 Tier-2 apply needs only the region + project set **once**:

```sh
terraform apply \
  -var-file=scale-0p2pct.tfvars \
  -var="project_id=<proj>" \
  -var="region=us-east4"
# bucket co-regional in us-east4; pods get the real project + bucket via env;
# image SHA already pinned -> pods PROVE on first deploy.
```

## Scope guard

Does **NOT** run real proving load (gated on G2) and does **NOT**
provision production sizes (gated on sizing #95). This validates the
**automation**, not a production deployment.
