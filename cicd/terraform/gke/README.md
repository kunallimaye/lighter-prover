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
| `kubernetes_deployment.cells` | **Machine CLASS 1** — chunk-prover cells (CPU-saturating; c4a/Axion in prod). Mount the proof store (issue #235) like the coordinator/fold-worker — see below. |
| `kubernetes_deployment.coordinator` | **Machine CLASS 2** — coordinators (fold L2 + prove L4; distinct class). |
| `kubernetes_deployment.fold_worker` | **Machine CLASS 3** — fold workers (issue #232) that competing-pull the MERGE-TASK subscription for the cross-machine distributed fold (issue #198). Gated by `enable_fold_workers`. |
| `kubernetes_pod_disruption_budget_v1.coordinator` | **HARD DAY-1** — PDB for the coordinator pool. |
| coordinator pod annotation `safe-to-evict=false` | **HARD DAY-1** — hardwired in `locals`, cannot be turned off by tfvars. |
| `kubernetes_horizontal_pod_autoscaler_v2.backlog` | HPA on `num_undelivered_messages` external metric. |
| metrics adapter (applied by the deploy pipeline) | custom-metrics-stackdriver-adapter — the external-metrics path. |

## Workload Identity for the prover pods (issue #231)

The cell/coordinator/fold-worker pods authenticate to **Pub/Sub** and **GCS**
via Workload Identity. Terraform creates a dedicated KSA, annotates it to the
pod GSA, binds `roles/iam.workloadIdentityUser`, and sets
`service_account_name` on all three deployments. Without this, pods run as the
`default` KSA and cannot auth — the whole pipeline fails.

All four knobs are **defaulted** so the existing `smoke.tfvars` /
`scale-*.tfvars` deploy WI with **no tfvars edit**:

| Variable | Default | Purpose |
|---|---|---|
| `enable_pod_workload_identity` | `true` | Create the prover KSA + `workloadIdentityUser` binding and set `service_account_name` on the cell/coordinator/fold-worker pods. WI is harmless when planes are off (smoke), required whenever pods must auth. |
| `pod_ksa_name` | `prover` | Name of the KSA (in the `default` namespace) the pods run as, annotated to the pod GSA. |
| `proof_store_pod_gsa_email` | `null` → derives `lighter-prover-pods@${project_id}.iam.gserviceaccount.com` | The pod GSA the KSA impersonates. `null` makes the email **follow `project_id`**; set a value to override. The GSA itself is **not** created here (exists out-of-band). |
| `enable_pubsub_iam` | `false` | When `false`, the pod GSA's `pubsub.publisher`/`pubsub.subscriber` are relied on **out-of-band** (VERIFY) — Terraform does not touch them. Set `true` to bring those grants under Terraform management (GRANT). Satisfies "grant (or verify)". |

The WI member pattern follows the proven metrics-adapter binding:
`serviceAccount:${project_id}.svc.id.goog[default/${pod_ksa_name}]`.

> **Operator step:** a live `terraform apply` + verifying real pod auth to
> Pub/Sub + GCS (no auth errors) is the operator's separate verification of
> the #231 acceptance criteria — out of scope for the Terraform change itself.

## The machine classes are sized SEPARATELY (never summed)

Each class has its own machine-shape, resource-request, and replica-count
variables. The cell tier is the large, CPU-saturating fleet; the
coordinator tier is ~1% of it but a **distinct** compute class.

A third class, the **fold workers** (issue #232), is the consumer side of the
cross-machine distributed fold (issue #198). When a coordinator runs
`--fold-distributed`, the leader publishes one merge task per merge pair to the
MERGE-TASK Pub/Sub plane; the fold-worker pods competing-pull them, prove ONE
merge each on their **full core budget**, transit the output through the
gcsfuse-mounted proof store, and report on the MERGE-RESULT plane. **Without a
fold-worker pool, those merge tasks are pulled by nobody → the per-level
barrier times out → the run fails.** The #198 governing principle is *one merge
per worker, scale by worker count* — so the lever is `fold_worker_replicas`,
not a bigger box. Fold workers deliberately carry **no** `safe-to-evict=false`
and **no** PDB: they are stateless competing-pull consumers, so an evicted
mid-merge task is simply redelivered to another worker (the #198 at-least-once
contract). The pool is gated behind `enable_fold_workers` (off at smoke scale;
the scale tfvars turn it on alongside `enable_merge_plane` +
`enable_proof_store` + `enable_proof_mount`).

The leader and the fold workers must agree on the merge-plane names, and those
names must match what `enable_merge_plane` provisions. The scale tfvars
**tier-prefix** the merge-task/merge-result topic+subscription names
(`lighter-prover-scale-<tier>-merge-*`, exactly like the chunk/results planes)
via `merge_*` variable overrides — set on both the `coordinator_command` /
`fold_worker_command` flags and the provisioned Pub/Sub names — so two scale
tiers running concurrently get tier-isolated merge planes and never collide
(issue #233). The generic `enable_merge_plane` defaults in `variables.tf` are
unchanged; the per-tier names are tfvars overrides.

The fold worker reaches Pub/Sub (pull the merge-task subscription, publish
merge results) and GCS (transit intermediate proofs), so it runs under
**Workload Identity** via the SAME knobs as the cell/coordinator pods — it sets
`service_account_name` to the prover KSA when `enable_pod_workload_identity` is
true (the default; see the Workload Identity section above), and falls back to
the `default` KSA when WI is off. No fold-worker-specific WI variable is needed.

## Cells mount the proof store (issue #235)

All three machine classes now mount the gcsfuse-backed proof store identically.
The cell command passes `--proof-mount-path /mnt/proof-store`, so the bench
binary (`bench/src/conductor/storage.rs`) selects **file-I/O MOUNT mode** over
the `gcloud storage cp` fallback to ship its L2 leaf proof. Before #235 the
cells pod had **no** gcsfuse volume, so those uploads wrote to a non-existent
path while the flag claimed otherwise. The fix mirrors the proven
coordinator/fold-worker pattern, all gated by `local.proof_mount_on`
(= `enable_proof_mount && enable_proof_store`):

- the `gke-gcsfuse/volumes=true` pod annotation (`local.cell_annotations`) that
  opts the pod into the gcsfuse CSI driver,
- the `LIGHTER_PROOF_MOUNT` env var (= `var.proof_mount_path`),
- the `proof-store` `volume_mount` at that path, and
- the gcsfuse CSI inline `volume` backed by the resolved proof-store bucket.

No tfvars change is needed — the scale tfvars already pass the flag and set
`enable_proof_mount = true`. Like the fold workers (and unlike the coordinator),
cells carry **no** `safe-to-evict=false`: they are stateless competing-pull
consumers.

## Zone topology-spread tolerates a single-zone c4a stockout (issue #235)

The cluster is **regional** Autopilot (`location = var.region`), so Google *can*
place pods across zones — but nothing forced it to. The
`enable_zone_spread` knob adds a `topologySpreadConstraint` across
`topology.kubernetes.io/zone` to **all three** deployments so a single-zone
**c4a (Axion) stockout** doesn't strand the whole pool. This directly addresses
`docs/live-benchmark-results.md` **FINDING C**: c4a stocked out across *all*
us-central1 zones during the multi-node benchmark.

| Variable | Default | Purpose |
|---|---|---|
| `enable_zone_spread` | `false` (smoke); `true` in the scale tfvars | Add the per-zone `topologySpreadConstraint` to cells/coordinator/fold-worker. |
| `zone_spread_max_skew` | `1` | maxSkew — spread as evenly as possible across zones. |

`when_unsatisfiable = ScheduleAnyway` (NOT `DoNotSchedule`) is deliberate: a
real N-1-zone stockout must **not** block scheduling entirely. Spread is
*preferred*; concentration into a single available zone is *tolerated* over a
stranded run.

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

## Automated scale apply / teardown (issue #235)

The raw `terraform apply` above is wrapped by a **parameterized, automated**
Cloud Build path — the SCALE sibling of the `gke-smoke-*` targets — so a scale
tier can be applied and torn down via the Makefile operator interface:

```sh
# Apply + confirm a scale tier (region defaults to us-east4; see below):
make gke-scale-up   GKE_PROJECT=<proj> GKE_BUILD_SA=<gke-capable-sa> GKE_TFVARS=scale-0p2pct.tfvars

# Tear down + verify nothing remains (match GKE_TFVARS to the applied tier):
make gke-scale-down GKE_PROJECT=<proj> GKE_BUILD_SA=<gke-capable-sa> GKE_TFVARS=scale-0p2pct.tfvars
```

| Piece | What it does |
|---|---|
| `make gke-scale-up` / `gke-scale-validate` | `scripts/gke-scale.sh up` → submits `cicd/cloudbuild-gke-scale.yaml`: apply `${GKE_TFVARS}` + confirm all enabled machine classes roll out. |
| `make gke-scale-down` | `scripts/gke-scale.sh down` → submits `cicd/cloudbuild-gke-scale-teardown.yaml`: destroy + verify nothing remains. |
| `GKE_TFVARS=` | Which scale tier to apply/destroy (default `scale-0p2pct.tfvars`). Threaded into the pipeline as the `-var-file`. |
| `GKE_REGION=` | Region override (default **us-east4** — see below). |

**Default region is us-east4** (the scale automation + the GKE module's `region`
default), unlike the smoke path which stays on us-central1. Rationale:
`docs/live-benchmark-results.md` **FINDING C** — c4a (Axion) stocked out across
*all* us-central1 zones during the multi-node benchmark, while us-east4 confirmed
real Axion capacity. The scale automation also uses a **separate TF state
prefix** (`lighter-prover/gke-scale`) and **cluster name**
(`lighter-prover-scale`) so a scale run never clobbers the smoke state.

Unlike the smoke pipeline, the scale validate step **omits** the synthetic
Pub/Sub-backlog HPA publish loop and the forced-drain eviction probe: those are
smoke-acceptance theater proven once + hardwired in `main.tf`. The scale
pipeline confirms the tier *came up* (all enabled classes scheduled) — the HPA
reacts to REAL load on a real run.

## Scope guard

Does **NOT** run real proving load (gated on G2) and does **NOT**
provision production sizes (gated on sizing #95). This validates the
**automation**, not a production deployment.
