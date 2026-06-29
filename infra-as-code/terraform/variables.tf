# Phase 1 needs three pieces of cloud topology: the build project where
# the AR repo lives, the SA that Cloud Build runs as, and (optionally)
# the runtime SA that pulls the image. Everything else is deferred to
# Phase 2 / a follow-up runtime-service issue.

# ─── Three-role topology (issue #141) ────────────────────────────────
# Roles are kept as separate variables even though Phase 1 only uses
# build_*. Leaving them named here keeps the variable interface stable
# for when Phase 2 adds the runtime + orchestration resources back.

variable "orchestration_project_id" {
  description = "Orchestration project — where the agent SA lives (operator identity)."
  type        = string
  default     = ""
}

variable "orchestration_region" {
  description = "Region for orchestration-project resources."
  type        = string
  default     = "us-central1"
}

variable "build_project_id" {
  description = "Build project — hosts Cloud Build and the Artifact Registry repo."
  type        = string
}

variable "build_region" {
  description = "Region for build-project resources (AR repo)."
  type        = string
  default     = "us-central1"
}

variable "runtime_project_id" {
  description = "Runtime project — where Cloud Run Jobs invocations of the bench image run. Defaults to the build project."
  type        = string
  default     = ""
}

variable "runtime_region" {
  description = "Region for runtime-project resources."
  type        = string
  default     = "us-central1"
}

variable "region" {
  description = "Default GCP deployment region."
  type        = string
  default     = "us-central1"
}

variable "silicon_arch" {
  description = "Target CPU silicon architecture ('c4a', 'c3d', 't2d')"
  type        = string
  default     = "c3d"
}

variable "orchestration_engine" {
  description = "Orchestration execution engine ('gke' or 'mig')"
  type        = string
  default     = "gke"
}

# ─── Artifact Registry ───────────────────────────────────────────────

variable "ar_repo" {
  description = "Artifact Registry repository ID for managed container workloads."
  type        = string
  default     = "lighter-prover-iac"
}

variable "ar_region" {
  description = "Artifact Registry repository location."
  type        = string
  default     = "us"
}

variable "build_machine_type" {
  description = "Cloud Build execution machine type specification."
  type        = string
  default     = "UNSPECIFIED"
}

variable "tf_state_bucket" {
  description = "GCS bucket used as remote backend for Terraform state files."
  type        = string
  default     = ""
}

variable "tf_state_prefix" {
  description = "State file prefix inside the GCS state bucket."
  type        = string
  default     = "lighter-prover-iac"
}

variable "bench_bucket" {
  description = "GCS bucket for benchmark reports."
  type        = string
  default     = ""
}

variable "bench_path_template" {
  description = "GCS path template for benchmark reports."
  type        = string
  default     = "benchmark-reports/{machine_type}/{instance_id}/{timestamp}"
}

# ─── Service account identities ──────────────────────────────────────

variable "builder_sa_email" {
  description = "Service account email used by Cloud Build. Granted artifactregistry.writer on the bench repo."
  type        = string
}

variable "runtime_sa_email" {
  description = "Optional runtime SA email. When set, granted artifactregistry.reader on the bench repo so Cloud Run Jobs in a different project can pull. Leave empty for single-project deployments."
  type        = string
  default     = ""
}

variable "target_sas" {
  description = "Map of target Service Accounts and their expected IAM roles."
  type = map(object({
    email = string
    roles = optional(list(string), [])
  }))
  default = {}
}

# ─── GCE Virtual Machines ────────────────────────────────────────────

variable "vms" {
  description = "Map of VM configurations to provision."
  type = map(object({
    machine_type    = string
    zone            = string
    disk_size_gb    = optional(number, 100)
    disk_type       = optional(string, "pd-ssd")
    image           = optional(string, "debian-cloud/debian-12")
    turbo_mode      = optional(string, "")
    service_account = optional(string, "")
  }))
  default = {}
}

variable "enable_static_vms" {
  description = "Enable provisioning of static GCE VMs defined in var.vms"
  type        = bool
  default     = false
}

variable "enable_hypothesis_fleet" {
  description = "Enable provisioning of the Phase 4 A/B hypothesis fleet (MIGs)"
  type        = bool
  default     = false
}

variable "enable_shared_resources" {
  description = "Enable provisioning of shared resources (Artifact Registry, IAM)"
  type        = bool
  default     = true
}

# ─── Fungible Pool autoscaling topology (issue #302) ───────────────────────
# Drives the baseload(committed)+burst(spot) node pools for the KEDA-autoscaled
# fungible `prover-node work` pool. OFF by default so this slice changes nothing
# until opted in (GKE engine only; the MIG path is unaffected).

variable "enable_fungible_pool" {
  description = "Provision the fungible baseload(committed)+burst(spot) GKE node pools for the KEDA-autoscaled prover-node work pool (#302). Off by default."
  type        = bool
  default     = false
}

variable "fungible_leaf_machine_type" {
  description = "Compute machine shape for the GKE fungible leaf worker pool (e.g. c3d-highcpu-30)"
  type        = string
  default     = "c3d-highcpu-30"
}

variable "fungible_leaf_node_count" {
  description = "Static node count for the GKE fungible leaf worker pool."
  type        = number
  default     = 8
}

variable "fungible_agg_machine_type" {
  description = "Compute machine shape for the GKE fungible aggregator pool (e.g. c3d-highcpu-60)"
  type        = string
  default     = "c3d-highcpu-60"
}

variable "fungible_agg_node_count" {
  description = "Static node count for the GKE fungible aggregator pool."
  type        = number
  default     = 2
}
