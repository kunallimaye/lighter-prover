# Copyright (c) Elliot Technologies, Inc.
# SPDX-License-Identifier: BUSL-1.1
#
# Modular Proving Pod Instantiation
# Orchestrates physical compute capacity for the bare GCE MIG engine.
#
# NOTE: The GKE engine is NOT provisioned. No `google_container_cluster` resource
# exists in this configuration, so `cluster_id` cannot be bound to a real cluster
# and the GKE node pools in the module remain gated off (count = 0). The GKE
# engine path is intentionally disabled until a cluster resource is added; the
# distributed Cloud Build pipeline fails fast on ENGINE=gke rather than faking a
# deploy. See docs/decisions/ADR-distributed-gke-topology.md and issue #283.

module "proving_pod_fleet" {
  source               = "./modules/proving_pod_node_pool"
  orchestration_engine = var.orchestration_engine
  cluster_id           = var.orchestration_engine == "gke" ? google_container_cluster.primary[0].id : ""
  region               = var.runtime_region != "" ? var.runtime_region : var.region
  zone                 = "${var.runtime_region != "" ? var.runtime_region : var.region}-b"
  service_account      = var.runtime_sa_email != "" ? var.runtime_sa_email : var.builder_sa_email
  silicon_arch         = var.silicon_arch

  image             = var.silicon_arch == "c4a" ? "debian-cloud/debian-12-arm64" : "debian-cloud/debian-12"
  leaf_machine_type = var.silicon_arch == "c4a" ? "c4a-highcpu-16" : (var.silicon_arch == "c3d" ? "c3d-highcpu-30" : (var.silicon_arch == "c4d" ? "c4d-highcpu-16" : "t2d-standard-16"))
  leaf_disk_type    = var.silicon_arch == "t2d" ? "pd-balanced" : "hyperdisk-balanced"
  leaf_disk_size_gb = 100
  leaf_node_count   = 6

  agg_machine_type = var.silicon_arch == "c4a" ? "c4a-highcpu-16" : (var.silicon_arch == "c3d" ? "c3d-highcpu-30" : (var.silicon_arch == "c4d" ? "c4d-highcpu-16" : "t2d-standard-16"))
  agg_disk_type    = var.silicon_arch == "t2d" ? "pd-balanced" : "hyperdisk-balanced"
  agg_disk_size_gb = 50
  agg_node_count   = 2

  # ── Fungible pool (issue #302): baseload(committed)+burst(spot) for the KEDA-
  # autoscaled `prover-node work` pool. OFF by default (enable_fungible_pool =
  # false) so existing topology is unchanged; flip var.enable_fungible_pool to
  # provision it on the GKE engine. The MIG path ignores these entirely.
  enable_fungible_pool       = var.enable_fungible_pool
  fungible_leaf_machine_type = var.fungible_leaf_machine_type
  fungible_leaf_node_count   = var.fungible_leaf_node_count
  fungible_agg_machine_type  = var.fungible_agg_machine_type
  fungible_agg_node_count    = var.fungible_agg_node_count
}
