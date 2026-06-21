# Copyright (c) Elliot Technologies, Inc.
# SPDX-License-Identifier: BUSL-1.1
#
# Reusable Terraform Module: Proving Pod Collaborative Fleet
# Provisions identical silicon hardware and disk topologies across GKE and bare MIG engines.

terraform {
  required_providers {
    google = {
      source  = "hashicorp/google"
      version = ">= 5.0"
    }
  }
}

# ═══════════════════════════════════════════════════════════════════════════
# PARADIGM 1: GKE Standard Spot Node Pools (ENGINE == "gke")
# ═══════════════════════════════════════════════════════════════════════════

resource "google_container_node_pool" "leaf_worker_pool" {
  count    = var.orchestration_engine == "gke" ? 1 : 0
  name     = "lighter-leaf-${var.silicon_arch}"
  cluster  = var.cluster_id
  location = var.zone

  initial_node_count = var.leaf_node_count

  autoscaling {
    min_node_count = 0
    max_node_count = 120
  }

  management {
    auto_repair  = true
    auto_upgrade = true
  }

  node_config {
    preemptible  = true # Enforces Google Cloud Spot VMs for maximum cost arbitrage!
    machine_type = var.leaf_machine_type
    disk_type    = var.leaf_disk_type
    disk_size_gb = var.leaf_disk_size_gb

    service_account = var.service_account
    oauth_scopes    = ["https://www.googleapis.com/auth/cloud-platform"]

    # Starvation Prevention & Topology Affinity
    gcfs_config {
      enabled = true
    }

    kubelet_config {
      cpu_manager_policy = "static" # Pin STARK threads exclusively to host NUMA cores!
      cpu_cfs_quota      = false    # Purge CFS period throttling!
    }

    labels = {
      role         = "leaf-worker"
      silicon-arch = var.silicon_arch
    }

    taint {
      key    = "dedicated"
      value  = "zkp-prover"
      effect = "NO_SCHEDULE"
    }
  }
}

resource "google_container_node_pool" "aggregator_pool" {
  count    = var.orchestration_engine == "gke" ? 1 : 0
  name     = "lighter-agg-${var.silicon_arch}"
  cluster  = var.cluster_id
  location = var.zone

  initial_node_count = var.agg_node_count

  autoscaling {
    min_node_count = 0
    max_node_count = 40
  }

  node_config {
    preemptible  = true
    machine_type = var.agg_machine_type
    disk_type    = var.agg_disk_type
    disk_size_gb = var.agg_disk_size_gb

    service_account = var.service_account
    oauth_scopes    = ["https://www.googleapis.com/auth/cloud-platform"]

    labels = {
      role         = "tree-node"
      silicon-arch = var.silicon_arch
    }
  }
}

# ═══════════════════════════════════════════════════════════════════════════
# PARADIGM 2: Bare GCE Managed Instance Groups (ENGINE == "mig")
# ═══════════════════════════════════════════════════════════════════════════

resource "google_compute_instance_template" "leaf_mig_template" {
  count        = var.orchestration_engine == "mig" ? 1 : 0
  name_prefix  = "lighter-leaf-${var.silicon_arch}-"
  machine_type = var.leaf_machine_type

  scheduling {
    preemptible        = true
    automatic_restart  = false
    provisioning_model = "SPOT"
  }

  disk {
    source_image = var.image
    auto_delete  = true
    boot         = true
    disk_type    = var.leaf_disk_type
    disk_size_gb = var.leaf_disk_size_gb
  }

  network_interface {
    network = var.network
  }

  service_account {
    email  = var.service_account
    scopes = ["cloud-platform"]
  }

  metadata = {
    role         = "leaf-worker"
    silicon-arch = var.silicon_arch
  }

  lifecycle {
    create_before_destroy = true
  }
}

resource "google_compute_region_instance_group_manager" "leaf_mig_fleet" {
  count              = var.orchestration_engine == "mig" ? 1 : 0
  name               = "lighter-leaf-${var.silicon_arch}-mig"
  base_instance_name = "lighter-leaf-${var.silicon_arch}"
  region             = var.region

  version {
    instance_template = google_compute_instance_template.leaf_mig_template[0].id
  }

  target_size = var.leaf_node_count
}
