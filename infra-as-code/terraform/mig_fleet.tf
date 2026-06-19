# Copyright (c) Elliot Technologies, Inc.
# SPDX-License-Identifier: BUSL-1.1

resource "google_compute_instance_template" "leaf_prover_template" {
  name_prefix  = "lighter-leaf-prover-"
  machine_type = "c4a-highcpu-72"

  scheduling {
    preemptible       = true
    automatic_restart = false
    provisioning_model = "SPOT"
  }

  disk {
    source_image = "debian-cloud/debian-12"
    auto_delete  = true
    boot         = true
    disk_type    = "pd-balanced"
    disk_size_gb = 50
  }

  network_interface {
    network = "default"
  }

  metadata = {
    role                 = "leaf-worker"
    backplane_engine     = "google-cloud-pubsub"
    jit_teardown_mandate = "90s-max-billing-window"
  }

  lifecycle {
    create_before_destroy = true
  }
}

resource "google_compute_region_instance_group_manager" "leaf_prover_fleet" {
  name               = "lighter-leaf-prover-mig"
  base_instance_name = "leaf-worker"
  target_size        = 63

  version {
    instance_template = google_compute_instance_template.leaf_prover_template.id
  }
}
