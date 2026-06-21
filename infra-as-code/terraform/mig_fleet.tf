# Copyright (c) Elliot Technologies, Inc.
# SPDX-License-Identifier: BUSL-1.1
#
# Modular Proving Pod Instantiation
# Universally orchestrates physical compute capacity across GKE node pools or bare MIGs.

module "proving_pod_fleet" {
  source               = "./modules/proving_pod_node_pool"
  orchestration_engine = var.orchestration_engine
  cluster_id           = "" # Bound dynamically when GKE cluster resource is instantiated
  region               = var.runtime_region != "" ? var.runtime_region : var.region
  zone                 = "${var.runtime_region != "" ? var.runtime_region : var.region}-a"
  service_account      = var.runtime_sa_email != "" ? var.runtime_sa_email : var.builder_sa_email
  silicon_arch         = var.silicon_arch

  image             = var.silicon_arch == "c4a" ? "debian-cloud/debian-12-arm64" : "debian-cloud/debian-12"
  leaf_machine_type = var.silicon_arch == "c4a" ? "c4a-highcpu-64" : (var.silicon_arch == "c3d" ? "c3d-highcpu-180" : "t2d-standard-60")
  leaf_disk_type    = var.silicon_arch == "t2d" ? "pd-balanced" : "hyperdisk-balanced"
  leaf_disk_size_gb = 100
  leaf_node_count   = 6

  agg_machine_type = var.silicon_arch == "c4a" ? "c4a-highcpu-16" : (var.silicon_arch == "c3d" ? "c3d-highcpu-30" : "t2d-standard-16")
  agg_disk_type    = var.silicon_arch == "t2d" ? "pd-balanced" : "hyperdisk-balanced"
  agg_disk_size_gb = 50
  agg_node_count   = 2
}
