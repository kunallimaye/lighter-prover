# Copyright (c) Elliot Technologies, Inc.
# SPDX-License-Identifier: BUSL-1.1
#
# Reusable Terraform Module: Proving Pod Collaborative Unit Provisioner
# Symmetrically orchestrates physical Spot compute capacity across GKE Standard Node Pools
# and regional Google Compute Engine Managed Instance Groups (MIGs).

variable "orchestration_engine" {
  description = "Execution engine paradigm: 'gke' (Kubernetes pods) or 'mig' (GCE regional instance groups)"
  type        = string
  default     = "gke"
  validation {
    condition     = contains(["gke", "mig"], var.orchestration_engine)
    error_message = "Engine must be either 'gke' or 'mig'."
  }
}

variable "cluster_id" {
  description = "GKE Cluster ID (Required when orchestration_engine == 'gke')"
  type        = string
  default     = ""
}

variable "region" {
  description = "Google Cloud region for regional MIG provisioning"
  type        = string
  default     = "us-east4"
}

variable "zone" {
  description = "Google Cloud zone for zonal instance template allocations"
  type        = string
  default     = "us-east4-a"
}

variable "network" {
  description = "Target VPC network link"
  type        = string
  default     = "default"
}

variable "service_account" {
  description = "Least-privilege runtime Service Account email"
  type        = string
}

variable "silicon_arch" {
  description = "Target CPU silicon architecture ('c4a', 'c3d', 't2d')"
  type        = string
  default     = "c3d"
}

variable "image" {
  description = "Compute boot image link (e.g. debian-cloud/debian-12 or debian-12-arm64)"
  type        = string
  default     = "debian-cloud/debian-12"
}

# ─── Leaf Worker Fleet Configuration ───────────────────────────────────

variable "leaf_machine_type" {
  description = "Compute machine shape for Leaf Worker STARK generators (e.g. c3d-highcpu-180)"
  type        = string
  default     = "c3d-highcpu-180"
}

variable "leaf_disk_type" {
  description = "Boot disk storage class (e.g. hyperdisk-balanced or pd-balanced)"
  type        = string
  default     = "hyperdisk-balanced"
}

variable "leaf_disk_size_gb" {
  description = "Leaf worker boot disk capacity in gigabytes"
  type        = number
  default     = 100
}

variable "leaf_node_count" {
  description = "Initial physical node count for the Leaf Worker tier"
  type        = number
  default     = 6
}

# ─── Reduction Tree Aggregator Fleet Configuration ─────────────────────

variable "agg_machine_type" {
  description = "Compute machine shape for recursive FRI tree aggregators (e.g. c3d-highcpu-30)"
  type        = string
  default     = "c3d-highcpu-30"
}

variable "agg_disk_type" {
  description = "Aggregator boot disk storage class"
  type        = string
  default     = "hyperdisk-balanced"
}

variable "agg_disk_size_gb" {
  description = "Aggregator boot disk capacity in gigabytes"
  type        = number
  default     = 50
}

variable "agg_node_count" {
  description = "Initial physical node count for the Aggregator tier"
  type        = number
  default     = 2
}

# ─── Fungible Pool Node Topology (issue #302) ──────────────────────────────
# The fungible `prover-node work` pool (one pod shape, role-per-message) is
# autoscaled by KEDA on Pub/Sub backlog (see infra-as-code/kubernetes/
# fungible_pool.yaml + keda_scaledobject.yaml). Per ADR §7 it runs BASELOAD +
# BURST, NOT scale-to-zero:
#   * baseload pool = dedicated/COMMITTED (NOT Spot), always-on (~60% of peak
#     parallel width). Carries the `dedicated=zkp-prover:NoSchedule` taint and
#     the `role=fungible-worker` label the Deployment's nodeSelector targets.
#   * burst pool = SPOT, scales 0..N to absorb backlog bursts cheaply. Carries
#     the same `role=fungible-worker` label plus the standard GKE spot taint.
# Both are gated on orchestration_engine == "gke"; the MIG path is unaffected.
# Set `enable_fungible_pool = false` (the default) to keep these pools off so
# this slice changes nothing until the operator opts in.

variable "enable_fungible_pool" {
  description = "Provision the fungible baseload(committed)+burst(spot) node pools (GKE only). Off by default so existing topology is unchanged until opted in."
  type        = bool
  default     = false
}

variable "fungible_machine_type" {
  description = "Compute machine shape for the fungible prover pool (sized for the heaviest role; e.g. c3d-highcpu-30)"
  type        = string
  default     = "c3d-highcpu-30"
}

variable "fungible_disk_type" {
  description = "Fungible pool boot disk storage class"
  type        = string
  default     = "hyperdisk-balanced"
}

variable "fungible_disk_size_gb" {
  description = "Fungible pool boot disk capacity in gigabytes"
  type        = number
  default     = 100
}

variable "fungible_baseload_node_count" {
  description = "Fixed node count for the COMMITTED baseload pool (~60% of peak parallel width). Always-on, NOT Spot."
  type        = number
  default     = 6
}

variable "fungible_burst_max_node_count" {
  description = "Max node count for the SPOT burst pool. Autoscales 0..N to absorb Pub/Sub backlog bursts."
  type        = number
  default     = 80
}
