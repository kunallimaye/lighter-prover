# ADR: Distributed proving — GKE topology disabled until provisioned

- Status: Accepted
- Date: 2026-06-25
- Related: issue #283 (distributed prover-node honesty), issue #281 (reduction-tree circuit fix)

## Context

The distributed proving stack defines two orchestration engines:

- `mig` — a bare GCE Managed Instance Group fleet (`infra-as-code/terraform/mig_fleet.tf`
  + `modules/proving_pod_node_pool`).
- `gke` — a Google Kubernetes Engine fleet driven by the manifests in
  `infra-as-code/kubernetes/` and the node-pool module's GKE branch.

Audit for issue #283 found the GKE path to be **non-functional but presented as
working**:

- The proving-pod node pools are gated on
  `var.orchestration_engine == "gke" && var.cluster_id != ""`, but
  `mig_fleet.tf` hardcodes `cluster_id = ""`, so the pools are always
  `count = 0`.
- **No `google_container_cluster` resource exists** anywhere in the Terraform
  configuration, so `cluster_id` can never be bound to a real cluster.
- `infra-as-code/cloudbuild-distributed.yaml` **skipped `terraform apply` when
  `ENGINE=gke`**, never ran `kubectl apply`, and then ran the prover-node
  coordinator on the Cloud Build VM as if that were the distributed deploy —
  producing fabricated success without provisioning anything.

This let `ENGINE=gke` masquerade as a working deployment path while doing
nothing real.

## Decision

**Disable the GKE engine honestly rather than fake it.**

Full GKE provisioning (a real `google_container_cluster`, networking, Workload
Identity, autoscaling node pools, and a validated `kubectl apply` deploy) is a
large effort that is out of scope for the #283 fix. Until that work lands:

- `infra-as-code/cloudbuild-distributed.yaml` **fails fast** on `ENGINE=gke`
  with a clear error instead of silently skipping apply. The default engine is
  now `mig`.
- `mig_fleet.tf` documents that `cluster_id` is empty because GKE is not
  provisioned, and the GKE node pools remain gated off.
- The distributed pipeline's proving step is an honest leaf -> tree -> root
  smoke run over the filesystem proof transport; the root coordinator fails
  loudly if no real proof was produced upstream, so it cannot report a
  fabricated success.

The bare GCE MIG engine remains the working, honest path.

## Consequences

- `ENGINE=gke` returns a non-zero exit with an actionable message; it can no
  longer pretend to deploy.
- The `prover-node` binary is now built and shipped in `Dockerfile.zkp` and
  `Dockerfile.zkp-arm64`, so the K8s manifests that invoke it
  (`infra-as-code/kubernetes/prover_pod_unit.yaml`) will resolve once a real
  cluster exists — i.e. the image is ready for GKE, only the cluster is missing.
- A follow-up is required to implement real GKE provisioning (cluster resource,
  `cluster_id` binding, `terraform apply` + `kubectl apply` for `ENGINE=gke`).

## Follow-up

- File/track: "Provision a real GKE cluster for the distributed proving fleet"
  — add `google_container_cluster`, bind `cluster_id` in `mig_fleet.tf`, and
  make `cloudbuild-distributed.yaml` apply + `kubectl apply` for `ENGINE=gke`.
