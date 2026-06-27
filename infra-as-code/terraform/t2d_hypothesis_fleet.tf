# Copyright (c) Elliot Technologies, Inc.
# SPDX-License-Identifier: BUSL-1.1
#
# Phase 4 A/B Hypothesis Fleet Modularization
# Instantiates identical benchmark proving units across ARM Axion (c4a) and AMD Milan (t2d).

module "control_c4a_pod" {
  count                = var.enable_hypothesis_fleet ? 1 : 0
  source               = "./modules/proving_pod_node_pool"
  orchestration_engine = "mig"
  region               = var.runtime_region != "" ? var.runtime_region : var.region
  zone                 = "${var.runtime_region != "" ? var.runtime_region : var.region}-a"
  service_account      = var.runtime_sa_email != "" ? var.runtime_sa_email : var.builder_sa_email
  silicon_arch         = "c4a"
  image                = "debian-cloud/debian-12-arm64"
  leaf_machine_type    = "c4a-highcpu-64"
  leaf_disk_type       = "hyperdisk-balanced"
  agg_machine_type     = "c4a-highcpu-16"
  agg_disk_type        = "hyperdisk-balanced"
}

module "hypothesis_t2d_pod" {
  count                = var.enable_hypothesis_fleet ? 1 : 0
  source               = "./modules/proving_pod_node_pool"
  orchestration_engine = "mig"
  region               = var.runtime_region != "" ? var.runtime_region : var.region
  zone                 = "${var.runtime_region != "" ? var.runtime_region : var.region}-c"
  service_account      = var.runtime_sa_email != "" ? var.runtime_sa_email : var.builder_sa_email
  silicon_arch         = "t2d"
  image                = "debian-cloud/debian-12"
  leaf_machine_type    = "t2d-standard-60"
  leaf_disk_type       = "pd-balanced"
  agg_machine_type     = "t2d-standard-16"
  agg_disk_type        = "pd-balanced"
}
