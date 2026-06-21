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
