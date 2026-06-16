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

# ─── Artifact Registry ───────────────────────────────────────────────

variable "ar_repo" {
  description = "Artifact Registry repository ID for managed container workloads."
  type        = string
  default     = "lighter-prover-iac"
}

variable "build_machine_type" {
  description = "Cloud Build execution machine type specification."
  type        = string
  default     = "UNSPECIFIED"
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
    service_account = optional(string, "")
  }))
  default = {}
}
