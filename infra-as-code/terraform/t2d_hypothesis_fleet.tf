# Copyright (c) Elliot Technologies, Inc.
# SPDX-License-Identifier: BUSL-1.1

# ─── Control Cluster (2 * c4a Pods proving Block 1042 & 1043 concurrently) ───

resource "google_compute_instance" "control_leaf_provers" {
  count        = 6
  provider     = google.runtime
  project      = coalesce(var.runtime_project_id, var.build_project_id)
  name         = "lighter-control-leaf-${count.index}"
  machine_type = "c4a-highcpu-64"
  zone         = "us-east4-b"

  scheduling {
    preemptible        = true
    automatic_restart  = false
    provisioning_model = "SPOT"
  }

  boot_disk {
    initialize_params {
      image = "debian-cloud/debian-12-arm64"
      size  = 50
      type  = "hyperdisk-balanced"
    }
  }

  network_interface {
    network = "default"
  }

  metadata = {
    paradigm             = "control-arm-axion"
    role                 = "leaf-worker"
    pod_id               = count.index < 3 ? "pod-0" : "pod-1"
    jit_teardown_mandate = "90s-max-billing-window"
  }

  labels = {
    experiment = "phase4-t2d-hypothesis"
    cluster    = "control"
  }
}

# ─── Hypothesis Cluster (2 * t2d Pods proving Block 1044 & 1045 concurrently) ───

resource "google_compute_instance" "hypothesis_leaf_provers" {
  count        = 6
  provider     = google.runtime
  project      = coalesce(var.runtime_project_id, var.build_project_id)
  name         = "lighter-hypothesis-leaf-${count.index}"
  machine_type = "t2d-standard-60"
  zone         = "us-east4-c"

  scheduling {
    preemptible        = true
    automatic_restart  = false
    provisioning_model = "SPOT"
  }

  boot_disk {
    initialize_params {
      image = "debian-cloud/debian-12"
      size  = 50
      type  = "pd-balanced"
    }
  }

  network_interface {
    network = "default"
  }

  metadata = {
    paradigm             = "hypothesis-amd-milan-tau"
    role                 = "leaf-worker"
    pod_id               = count.index < 3 ? "pod-2" : "pod-3"
    jit_teardown_mandate = "90s-max-billing-window"
  }

  labels = {
    experiment = "phase4-t2d-hypothesis"
    cluster    = "hypothesis"
  }
}

# ─── Sharded Tree Aggregator Array (4 * c4a-highcpu-16) ───

resource "google_compute_instance" "shared_tree_nodes" {
  count        = 4
  provider     = google.runtime
  project      = coalesce(var.runtime_project_id, var.build_project_id)
  name         = "lighter-tree-aggregator-${count.index}"
  machine_type = "c4a-highcpu-16"
  zone         = "us-east4-b"

  scheduling {
    preemptible        = true
    automatic_restart  = false
    provisioning_model = "SPOT"
  }

  boot_disk {
    initialize_params {
      image = "debian-cloud/debian-12-arm64"
      size  = 30
      type  = "hyperdisk-balanced"
    }
  }

  network_interface {
    network = "default"
  }

  metadata = {
    role   = "tree-node"
    pod_id = "pod-${count.index}"
  }

  labels = {
    experiment = "phase4-t2d-hypothesis"
    cluster    = "aggregator"
  }
}
