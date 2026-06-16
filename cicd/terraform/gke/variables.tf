# ─────────────────────────────────────────────────────────────────────
# Parametrisation contract (issue #151).
#
# RULE: everything that varies by SIZING is a variable here, never a
# literal in main.tf. The same variable surface accepts a "smoke" config
# (tiny — smoke.tfvars, what we actually apply for validation) and a
# "production" config (real sizes — production.tfvars.example, filled
# from the sizing model #95) WITHOUT any structural change to the module.
#
# The two ADR-0006 machine CLASSES (cells + coordinators) are sized
# SEPARATELY and NEVER summed: each class gets its own machine-shape,
# resource-request, and replica-count variables.
# ─────────────────────────────────────────────────────────────────────

# ─── Project / location ──────────────────────────────────────────────

variable "project_id" {
  description = "GCP project that hosts the GKE Autopilot cluster, Pub/Sub, and metrics."
  type        = string
}

variable "region" {
  description = "Region for the Autopilot cluster and Pub/Sub. Must support Autopilot + the c4a (Axion) shape (e.g. us-central1)."
  type        = string
  default     = "us-central1"
}

# ─── Cluster ─────────────────────────────────────────────────────────

variable "cluster_name" {
  description = "Name of the GKE Autopilot cluster."
  type        = string
  default     = "lighter-prover-smoke"
}

variable "release_channel" {
  description = "GKE release channel (RAPID | REGULAR | STABLE). Autopilot requires a channel."
  type        = string
  default     = "REGULAR"
}

variable "deletion_protection" {
  description = "Cluster deletion-protection flag. MUST be false for the smoke cluster so teardown can destroy it."
  type        = bool
  default     = false
}

variable "resource_labels" {
  description = "Labels applied to the cluster and Pub/Sub resources. Used by the teardown verifier to find leftovers."
  type        = map(string)
  default = {
    managed = "terraform"
    purpose = "gke-smoke-validation"
    issue   = "151"
  }
}

# ─── Machine CLASS 1: chunk-prover CELLS (ADR-0006 §1.2) ──────────────
# CPU-saturating whole-machine pods. Production shape is c4a-highcpu-64
# (Axion / neoverse-v2 arm64). At SMOKE scale we deploy a tiny no-op
# workload; the machine-class identity is still encoded via the compute
# class / nodeSelector and the (smoke-tiny) resource requests.

variable "cell_replicas" {
  description = "Number of chunk-prover cell pods. SMOKE: tiny (1-2). PRODUCTION: large (sized from #95). The two classes are never summed."
  type        = number
  default     = 1
}

variable "cell_compute_class" {
  description = "Autopilot compute class for cells. 'Scale-Out' selects Axion (arm64) nodes on Autopilot. Empty string = default Autopilot class (x86, used at smoke scale when an arm64 image is not built)."
  type        = string
  default     = ""
}

variable "cell_arch" {
  description = "kubernetes.io/arch nodeSelector for cells (arm64 for c4a/Axion, amd64 otherwise). Drives WHICH machine class the cell pods land on."
  type        = string
  default     = "amd64"
}

variable "cell_machine_family" {
  description = "Optional Autopilot machine-family nodeSelector for cells (cloud.google.com/machine-family). Set to 'c4a' to PIN Axion/neoverse-v2 (Gen-1) nodes. IMPORTANT: the 'Scale-Out' compute class provisions t2a (Ampere/neoverse-n1) nodes on Autopilot — a neoverse-v2 binary SIGILLs (exit 132) there. Use compute_class='Performance' + machine_family='c4a' for real Axion. Empty = no family pin (Autopilot chooses)."
  type        = string
  default     = ""
}

variable "cell_cpu_request" {
  description = "CPU request per cell pod. PRODUCTION whole-machine pods request the full c4a-highcpu-64 core count; SMOKE requests a fraction of a vCPU so a trivial workload schedules cheaply."
  type        = string
  default     = "250m"
}

variable "cell_memory_request" {
  description = "Memory request per cell pod. SMOKE: tiny. PRODUCTION: sized to the cell RSS (~9.4 GB L1/L2 + L4/L5 keys, ADR-0003)."
  type        = string
  default     = "512Mi"
}

variable "cell_image" {
  description = "Container image for cell pods. SMOKE: a trivial no-op/health image. PRODUCTION: the arm64/Axion prover image tag (<sha>-neoverse-v2 from cicd/cloudbuild.yaml)."
  type        = string
  default     = "registry.k8s.io/pause:3.10"
}

variable "cell_command" {
  description = "Container command for cell pods. SMOKE keeps them as a trivial long-sleep no-op; PRODUCTION runs the prover in cell mode (set via tfvars)."
  type        = list(string)
  default     = []
}

# ─── Machine CLASS 2: COORDINATORS (ADR-0006 §1.1, §2) ────────────────
# A DISTINCT compute class (fold L2 merge tree + prove L4), ~1% of the
# cell fleet. Hold resident proving keys and are frequently mid-fold —
# hence the HARD DAY-1 eviction mitigation below.

variable "coordinator_replicas" {
  description = "Number of coordinator pods. ADR-0006 §2: ~1% of the cell fleet — a small, SEPARATE pool. SMOKE: tiny (1)."
  type        = number
  default     = 1
}

variable "coordinator_compute_class" {
  description = "Autopilot compute class for coordinators. A DISTINCT class from cells. Empty = default Autopilot class (smoke)."
  type        = string
  default     = ""
}

variable "coordinator_arch" {
  description = "kubernetes.io/arch nodeSelector for coordinators."
  type        = string
  default     = "amd64"
}

variable "coordinator_machine_family" {
  description = "Optional Autopilot machine-family nodeSelector for coordinators (cloud.google.com/machine-family). Set to 'c4a' to PIN Axion/neoverse-v2 nodes (see cell_machine_family for the SIGILL rationale). Empty = no family pin."
  type        = string
  default     = ""
}

variable "coordinator_cpu_request" {
  description = "CPU request per coordinator pod. SMOKE: tiny. PRODUCTION: sized to the coordinator compute profile (#113 — undetermined pending a real utilization measurement)."
  type        = string
  default     = "250m"
}

variable "coordinator_memory_request" {
  description = "Memory request per coordinator pod. PRODUCTION holds resident L4/L5 proving keys."
  type        = string
  default     = "512Mi"
}

variable "coordinator_image" {
  description = "Container image for coordinator pods. SMOKE: trivial no-op; PRODUCTION: the prover image in coordinator mode."
  type        = string
  default     = "registry.k8s.io/pause:3.10"
}

variable "coordinator_command" {
  description = "Container command for coordinator pods."
  type        = list(string)
  default     = []
}

# ─── HARD DAY-1 eviction mitigation (ADR-0003 amendment §3) ──────────
# NON-NEGOTIABLE: every coordinator pod carries
# cluster-autoscaler.kubernetes.io/safe-to-evict=false AND the
# coordinator pool has a PodDisruptionBudget. These are NOT optional and
# NOT footnotes. The variables only parametrise the PDB threshold; the
# annotation + PDB existence are hardwired in main.tf so they cannot be
# turned off by a bad config.

variable "coordinator_pdb_min_available" {
  description = "PodDisruptionBudget minAvailable for the coordinator pool. Defaults to all coordinator replicas so an eviction/bin-pack of an in-flight coordinator is blocked."
  type        = number
  default     = 1
}

# ─── Autoscaling on Pub/Sub backlog (ADR-0006 §1.1, §5; ADR-0003 §D7) ─

variable "pubsub_topic" {
  description = "Pub/Sub topic name for the block-dispatch backlog signal the HPA watches."
  type        = string
  default     = "lighter-prover-smoke-dispatch"
}

variable "pubsub_subscription" {
  description = "Pub/Sub subscription name. The HPA scales on this subscription's num_undelivered_messages external metric."
  type        = string
  default     = "lighter-prover-smoke-dispatch-sub"
}

# ── Inner chunk-dispatch + results planes (issue #172) ──
# The genuine distributed coordinator/cell coordination needs two MORE
# topic/subscription pairs beyond the outer block-dispatch + backlog signal:
# the chunk plane (coordinator -> cells) and the results plane (cells ->
# coordinator). Gated behind enable_chunk_plane so the smoke automation is
# unchanged by default; the scale tfvars turn it on.

variable "enable_chunk_plane" {
  description = "Create the inner chunk-dispatch + results Pub/Sub topic/subscription pairs (issue #172). false at smoke scale (only the outer backlog signal is needed); true for the real distributed coordinator/cell run."
  type        = bool
  default     = false
}

variable "chunk_topic" {
  description = "Pub/Sub topic the coordinator publishes chunk REFERENCES to (coordinator -> cells; ADR-0006 §1.2). Maps to the cell pods' LIGHTER_CHUNK_TOPIC / the coordinator's --chunk-topic."
  type        = string
  default     = "lighter-prover-chunk"
}

variable "chunk_subscription" {
  description = "Pub/Sub subscription the cell pods competing-pull chunk references from. Maps to the cells' LIGHTER_CHUNK_SUBSCRIPTION."
  type        = string
  default     = "lighter-prover-chunk-sub"
}

variable "chunk_ack_deadline_seconds" {
  description = "Ack deadline for the chunk subscription. A chunk's L1+L2 prove is multi-second; give cells ample headroom so an in-flight chunk is not redelivered mid-prove."
  type        = number
  default     = 600
}

variable "results_topic" {
  description = "Pub/Sub topic the cell pods publish chunk RESULTS to (cells -> coordinator). Maps to the cells' LIGHTER_RESULTS_TOPIC."
  type        = string
  default     = "lighter-prover-results"
}

variable "results_subscription" {
  description = "Pub/Sub subscription the coordinator pulls chunk results from. Maps to the coordinator's LIGHTER_RESULTS_SUBSCRIPTION."
  type        = string
  default     = "lighter-prover-results-sub"
}

# ── Cross-machine fold fan-out: the MERGE-TASK plane (issue #198) ──
# To shard ONE block's merge tree across separate coordinator machines, the
# leader emits merge tasks here and independent fold-worker pods competing-pull
# them, transiting intermediate proofs through the proof store. Gated behind
# enable_merge_plane so smoke automation is unchanged by default; the scale
# tfvars turn it on alongside enable_chunk_plane + enable_proof_store.

variable "enable_merge_plane" {
  description = "Create the MERGE-TASK + MERGE-RESULT Pub/Sub topic/subscription pairs (issue #198 — cross-machine fold fan-out). false by default; true to run the distributed fold across separate coordinator/fold-worker machines."
  type        = bool
  default     = false
}

variable "merge_task_topic" {
  description = "Pub/Sub topic the leader publishes MERGE TASKS to (leader -> fold workers; issue #198). Maps to the leader's --merge-task-topic / LIGHTER_MERGE_TASK_TOPIC."
  type        = string
  default     = "lighter-prover-merge-task"
}

variable "merge_task_subscription" {
  description = "Pub/Sub subscription the fold-worker pods competing-pull merge tasks from. Maps to the workers' LIGHTER_MERGE_TASK_SUBSCRIPTION."
  type        = string
  default     = "lighter-prover-merge-task-sub"
}

variable "merge_ack_deadline_seconds" {
  description = "Ack deadline for the merge-task subscription. A single merge prove is ~1.6 s on a c4a-standard-4 (issue #198 pilot fact); give workers ample headroom so an in-flight merge is not redelivered mid-prove."
  type        = number
  default     = 600
}

variable "merge_result_topic" {
  description = "Pub/Sub topic the fold workers publish MERGE RESULTS to (fold workers -> leader; issue #198). Maps to the workers' --merge-result-topic / LIGHTER_MERGE_RESULT_TOPIC."
  type        = string
  default     = "lighter-prover-merge-result"
}

variable "merge_result_subscription" {
  description = "Pub/Sub subscription the leader pulls merge results from (the per-level barrier; issue #198). Maps to the leader's LIGHTER_MERGE_RESULT_SUBSCRIPTION."
  type        = string
  default     = "lighter-prover-merge-result-sub"
}

# ── Shared proof store (issue #179, slice 1 / WS1) ──
# The fan-IN half of the distributed prover needs a SHARED proof store so
# cells can ship their L2 leaf proof BYTES back to the coordinator (Pub/Sub
# message-size limits make inline proof bytes impractical — ADR-0008 §1.2 /
# issue #179 scope item 1). A GCS bucket is the natural fit: the cell writes
# its L2 leaf proof keyed by {height, witness_index}, and ChunkResultMessage
# carries a reference (the object key) to it. Gated behind enable_proof_store
# so the smoke automation (topology + backlog HPA only) is unchanged by
# default; the scale tfvars turn it on alongside enable_chunk_plane.
#
# NOTE: cell upload + coordinator fetch/merge are LATER slices of #179. This
# slice only provisions the bucket + the pod-SA permission + the output ref.

variable "enable_proof_store" {
  description = "Create the shared proof-store GCS bucket + grant the pod GSA objectAdmin on it (issue #179). false at smoke scale; true for the real distributed coordinator/cell run that ships L2 leaf proofs."
  type        = bool
  default     = false
}

variable "proof_store_bucket" {
  description = "Name of the shared proof-store GCS bucket cells write L2 leaf proofs to and the coordinator reads them from (issue #179). Empty = derive a deterministic name from project_id (\"<project_id>-lighter-prover-proofs\"). Bucket names are globally unique, so the project-derived default is collision-resistant."
  type        = string
  default     = ""
}

variable "proof_store_pod_gsa_email" {
  description = "Email of the EXISTING pod Google Service Account that cells/coordinators run as (Workload Identity). It already holds the pubsub roles; this module additionally grants it roles/storage.objectAdmin on the proof-store bucket ONLY (bucket-scoped, not project-wide) and binds it roles/iam.workloadIdentityUser for the prover KSA. The SA is NOT created here. If null (the default), it DERIVES from project_id as \"lighter-prover-pods@$${project_id}.iam.gserviceaccount.com\" so the email follows the project; set a non-null value to override (issue #231)."
  type        = string
  default     = null
}

# ─── Workload Identity for the prover pods (issue #231) ──────────────
# Without WI the cell/coordinator pods run as the `default` KSA and cannot
# authenticate to Pub/Sub or GCS. These variables create a dedicated KSA,
# annotate it to the pod GSA, bind workloadIdentityUser, and set
# service_account_name on both deployments. All defaulted so the existing
# smoke + scale tfvars deploy WI WITHOUT any tfvars edit.

variable "enable_pod_workload_identity" {
  description = "Create the prover KSA + the roles/iam.workloadIdentityUser binding to the pod GSA, and set service_account_name on the cell/coordinator pods so they run as the pod GSA via Workload Identity (issue #231). Defaults true: WI is harmless when planes are off (smoke), and required whenever pods must auth to Pub/Sub + GCS."
  type        = bool
  default     = true
}

variable "pod_ksa_name" {
  description = "Name of the Kubernetes ServiceAccount (in the `default` namespace) the prover cell/coordinator pods run as, annotated to the pod GSA for Workload Identity (issue #231)."
  type        = string
  default     = "prover"
}

variable "enable_pubsub_iam" {
  description = "Also grant the pod GSA roles/pubsub.publisher + roles/pubsub.subscriber via Terraform (issue #231). Default false: the pod GSA already holds these roles out-of-band, so the default-off path VERIFIES the working grants without disturbing them. Set true to bring the pubsub grants under Terraform management (GRANT) — satisfies the issue's \"grant (or verify)\"."
  type        = bool
  default     = false
}

variable "proof_store_location" {
  description = "Location for the proof-store bucket. Empty (the default) tracks var.region so the bucket is co-regional with the cluster — a SINGLE region override suffices and proofs stay co-located with the coordinator that folds them (minimizes fetch latency in the serial L4 stage). Set a non-empty value only to deliberately place the bucket in a DIFFERENT location than the cluster (a cross-region tax on the serial L4 stage)."
  type        = string
  default     = ""
}

variable "proof_store_force_destroy" {
  description = "Allow `terraform destroy` to delete the proof-store bucket even if it still contains objects. true for ephemeral smoke/scale validation runs so teardown is clean; set false for any bucket holding proofs you must retain."
  type        = bool
  default     = true
}

# Issue #206: mount the proof-store bucket into the coordinator/fold-worker pod
# as a gcsfuse VOLUME so the bench binary's proof-store upload/download become
# plain file write/read against the mount, instead of shelling out to
# `gcloud storage cp` once per copy (the per-subprocess overhead that became the
# dominant per-level fold barrier after #203). When enabled, the gcsfuse CSI
# ephemeral inline volume is attached to the coordinator container at
# var.proof_mount_path and LIGHTER_PROOF_MOUNT is set to that path, so
# bench/src/conductor/storage.rs selects mount-mode file I/O. Reuses the
# EXISTING pod-GSA objectAdmin binding from #179/#182 — NO new IAM. The CLI
# transport remains the fallback when this is false (additive/non-regressing).

variable "enable_proof_mount" {
  description = "Mount the proof-store bucket into the coordinator pod via the gcsfuse CSI driver and point LIGHTER_PROOF_MOUNT at it (issue #206). Requires enable_proof_store (the bucket + pod-GSA permission). false = the bench binary keeps using the `gcloud storage cp` CLI transport (unchanged)."
  type        = bool
  default     = false
}

variable "proof_mount_path" {
  description = "In-pod path the proof-store bucket is gcsfuse-mounted at when enable_proof_mount is true (issue #206). Passed to the bench binary as LIGHTER_PROOF_MOUNT so storage.rs maps {height}/{witness_index} and {height}/m/{level}/{index} keys to files under this root."
  type        = string
  default     = "/mnt/proof-store"
}

variable "hpa_target_class" {
  description = "Which workload the backlog HPA scales (cells | coordinator). Cells consume the block backlog at smoke scale."
  type        = string
  default     = "cells"
}

variable "hpa_min_replicas" {
  description = "HPA minimum replicas."
  type        = number
  default     = 1
}

variable "hpa_max_replicas" {
  description = "HPA maximum replicas. SMOKE: small cap so a synthetic backlog can demonstrably move desiredReplicas without a real fleet."
  type        = number
  default     = 5
}

variable "hpa_backlog_target" {
  description = "Target num_undelivered_messages PER replica (averageValue). SMOKE: small so a hand-published backlog crosses it and the HPA scales up. PRODUCTION: sized from throughput model."
  type        = number
  default     = 10
}

# ─── Workload toggle ─────────────────────────────────────────────────

variable "enable_workloads" {
  description = "When true, deploy the two machine-class workloads + PDB + HPA. When false, provision only the cluster + Pub/Sub (used for a faster cluster-only smoke or a phased apply)."
  type        = bool
  default     = true
}

variable "metrics_adapter_enabled" {
  description = "Install the custom-metrics-stackdriver-adapter (the external-metrics path the Pub/Sub-backlog HPA needs). Toggleable so the cluster can come up before the adapter on a phased apply."
  type        = bool
  default     = true
}

variable "metrics_adapter_manifest_url" {
  description = "URL of the custom-metrics-stackdriver-adapter manifest applied to the cluster (new-resource-model variant for Workload Identity)."
  type        = string
  default     = "https://raw.githubusercontent.com/GoogleCloudPlatform/k8s-stackdriver/master/custom-metrics-stackdriver-adapter/deploy/production/adapter_new_resource_model.yaml"
}
