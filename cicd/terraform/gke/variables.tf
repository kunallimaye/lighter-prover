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
