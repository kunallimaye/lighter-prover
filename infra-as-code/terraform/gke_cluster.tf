# Copyright (c) Elliot Technologies, Inc.
# SPDX-License-Identifier: BUSL-1.1

resource "google_container_cluster" "primary" {
  count    = var.orchestration_engine == "gke" ? 1 : 0
  provider = google-beta.runtime_beta
  name     = "lighter-prover-cluster-${var.silicon_arch}"
  project  = var.runtime_project_id != "" ? var.runtime_project_id : var.build_project_id
  location = "${var.runtime_region != "" ? var.runtime_region : var.region}-b"

  deletion_protection = false

  # Remove default node pool immediately to use custom pools
  remove_default_node_pool = true
  initial_node_count       = 1

  network = "default"

  ip_allocation_policy {}

  workload_identity_config {
    workload_pool = "${var.runtime_project_id != "" ? var.runtime_project_id : var.build_project_id}.svc.id.goog"
  }

  release_channel {
    channel = "REGULAR"
  }

  addons_config {
    http_load_balancing {
      disabled = true
    }
    gce_persistent_disk_csi_driver_config {
      enabled = true
    }
    gcs_fuse_csi_driver_config {
      enabled = true
    }
  }

  resource_labels = {
    project = "lighter-prover"
    managed = "terraform"
  }
}

resource "google_service_account_iam_member" "workload_identity_binding" {
  count              = var.orchestration_engine == "gke" ? 1 : 0
  provider           = google-beta.runtime_beta
  service_account_id = "projects/${var.runtime_project_id != "" ? var.runtime_project_id : var.build_project_id}/serviceAccounts/${var.runtime_sa_email != "" ? var.runtime_sa_email : var.builder_sa_email}"
  role               = "roles/iam.workloadIdentityUser"
  member             = "serviceAccount:${var.runtime_project_id != "" ? var.runtime_project_id : var.build_project_id}.svc.id.goog[default/prover-sa]"
}

resource "google_container_node_pool" "system_pool" {
  count    = var.orchestration_engine == "gke" ? 1 : 0
  provider = google-beta.runtime_beta
  name     = "lighter-system-pool"
  cluster  = google_container_cluster.primary[0].id
  location = google_container_cluster.primary[0].location
  project  = google_container_cluster.primary[0].project

  node_count = 1

  management {
    auto_repair  = true
    auto_upgrade = true
  }

  node_config {
    preemptible  = true
    machine_type = "e2-medium"

    service_account = var.runtime_sa_email != "" ? var.runtime_sa_email : var.builder_sa_email
    oauth_scopes    = ["https://www.googleapis.com/auth/cloud-platform"]

    labels = {
      role = "system"
    }
  }
}

