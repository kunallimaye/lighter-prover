# Distributed proving on Kubernetes — two orchestration paths

This directory holds the Kubernetes manifests for distributed recursive proving.
There are **two valid orchestration paths** with different tradeoffs. Pick one
per deployment; do not run both against the same Pub/Sub subscription + bucket at
once.

Reference: `docs/decisions/ADR-distributed-recursive-proving-architecture.md`
(§4 fungible worker pool, §5 transport, §6 claim guard, §7 autoscaling / graceful
drain).

> **Scope note (issue #302):** everything here is **manifests + config only**.
> No KEDA install, no live Pub/Sub scaling, no `kubectl` against a real cluster,
> no Terraform apply is performed by this slice. Anything that can only be
> confirmed on a running cluster is flagged `TODO(confirm-on-live-run)`.

---

## Path A — Fungible autoscaled pool (issue #302) — `fungible_pool.yaml` + `keda_scaledobject.yaml`

**One pod shape, role-per-message, pull-based, KEDA-autoscaled.**

- A single `Deployment` (`lighter-fungible-prover`) runs
  `prover-node work --transport=pubsub`. Each pod pulls one `WorkDescriptor`
  (leaf prove **or** level-k fold) from a Pub/Sub subscription with flow-control
  = 1, proves it, commits the proof bytes idempotently to GCS via the native
  `ifGenerationMatch=0` CAS, acks, then pulls the next ready task — until the
  dynamic-depth root proof exists.
- **Autoscaling:** a KEDA `ScaledObject` scales the Deployment on the Pub/Sub
  **backlog** (`num_undelivered_messages`). `minReplicaCount` = **baseload**
  (always-on, committed nodes, ~60% of peak parallel width — **never 0**, to
  protect the narrow aggregation tail and root completion). `maxReplicaCount` =
  baseload + **burst** (Spot capacity).
- **Node topology:** baseload pool = dedicated/**committed** (NOT Spot); burst
  pool = **Spot**. Both are Terraform-defined in
  `../terraform/modules/proving_pod_node_pool` (resources
  `fungible_baseload_pool` + `fungible_burst_pool`, gated on
  `orchestration_engine == "gke"` **and** `enable_fungible_pool = true`). Both
  carry `role=fungible-worker` (the Deployment's `nodeSelector`) and the
  `dedicated=zkp-prover:NoSchedule` taint (tolerated by the Deployment).
- **Graceful drain (MANDATORY, ADR §7):** `terminationGracePeriodSeconds: 120`
  (≥ max prove time; the radix-16 fold is ≈30s, the long pole) plus an in-binary
  **SIGTERM handler** (`bench::shutdown`): on scale-down / Spot preemption the
  dispatch loop **stops pulling new work, finishes the in-flight prove, commits +
  acks, then exits** — a pod is never killed mid-prove. The
  `controller.kubernetes.io/pod-deletion-cost` annotation lets scale-down prefer
  **idle** pods over busy ones (the live runner should patch it high while a lease
  is held; `TODO(confirm-on-live-run)`).

**Tradeoffs:** simplest operationally — one image, one pod shape, one Spot pool,
one autoscaling knob, trivial bin-packing, and "dial back in" (a pod that
finishes a leaf immediately pulls the next ready fold). Requires the managed
queue (Pub/Sub) and KEDA. The block volumetric stays a free, forgiving knob.
This is the ADR's recommended target path for scale-out.

### KEDA prerequisite (NOT installed by this slice)

```sh
helm repo add kedacore https://kedacore.github.io/charts
helm repo update
helm install keda kedacore/keda --namespace keda --create-namespace
```

The `gcp-pubsub` scaler authenticates via Workload Identity (bind the KEDA
operator KSA to a GSA with `roles/monitoring.viewer` + `roles/pubsub.viewer`) or
a `TriggerAuthentication`. See <https://keda.sh/docs/latest/scalers/gcp-pub-sub/>.

### Rendering

Static templates live here with clear placeholders (`PROJECT_ID`, `GSA_EMAIL`,
`IMAGE_URI`, `PUBSUB_TOPIC`, `PUBSUB_SUBSCRIPTION`, `GCS_BUCKET`,
`BASELOAD_REPLICAS`, `MAX_REPLICAS`). Alternatively render them filled-in from
`config.toml`:

```sh
python3 infra-as-code/scripts/render_pod_spec.py \
  --config config.toml --image default --emit-fungible \
  --arch c3d --radix 16 --leaf-count 256 \
  --topic prover-folds --subscription prover-work \
  --baseload 6 --burst 80 --ack-deadline 60
# -> *-fungible.rendered.yaml (Deployment) + *-fungible-keda.rendered.yaml (KEDA)
```

The image must be built with the `pubsub` cargo feature
(`cargo build --features pubsub`); the default build is cloud-free and fails fast
on `--transport=pubsub`.

---

## Path B — Phase-locked per-level Jobs (#293/#297) — rendered by `render_pod_spec.py`

**Typed Indexed Jobs per tree level, cross-level gated by Cloud Build.**

- `render_pod_spec.py` (default mode, no `--emit-fungible`) emits an Indexed leaf
  `Job` + one Indexed tree-node `Job` per level (`lighter-tree-aggregator-l{N}`)
  + a root-coordinator `Job`, plus a machine-readable `*-tree.plan.env`.
- **Ordering:** Kubernetes `batch/v1` Jobs have no native inter-Job gating, so
  `infra-as-code/cloudbuild-distributed.yaml` (ENGINE=gke) applies each level one
  at a time, blocking on `kubectl wait --for=condition=complete` between levels
  (#297). The plan file is the single source of truth for depth/node-counts.
- Uses the same `prover-node` image but the explicit `leaf-worker` / `tree-node`
  / `root-coordinator` subcommands (role baked into the command), not the
  fungible `work` dispatch loop.

**Tradeoffs:** no managed queue or KEDA needed; deterministic, phase-locked
execution that is easy to reason about and debug per level. But it is **not**
work-stealing (a pod that finishes early idles until its level's Job completes),
the pod count is tied to per-level geometry, and cross-level gating lives in the
orchestrator rather than in the data. Retained as a valid alternative.

---

## Which to use

| Aspect | Path A — Fungible (KEDA) | Path B — Phase-locked Jobs |
|---|---|---|
| Pod shape | One (`work`) | Typed per role |
| Work assignment | Pull / work-stealing | Indexed Job completions |
| Autoscaling | KEDA on backlog | GKE autoscaler on pending pods |
| Scale-to-zero | No (baseload floor) | Jobs complete + clear |
| Cross-level ordering | Readiness gating in data | Cloud Build `kubectl wait` |
| Queue dependency | Pub/Sub + KEDA | None |
| Idle waste | Minimal (dial back in) | Possible (level barriers) |
| ADR status | Recommended target | Valid alternative |

Both depend on the verified transport primitives (Pub/Sub pull flow-control = 1,
GCS `ifGenerationMatch=0` idempotent commit, ack-after-commit). The live
end-to-end run of either path on a real cluster is `TODO(confirm-on-live-run)`.
