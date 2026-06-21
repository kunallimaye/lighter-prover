# Copyright (c) Elliot Technologies, Inc.
# SPDX-License-Identifier: BUSL-1.1

output "leaf_fleet_id" {
  description = "Identifier of the provisioned Leaf Worker fleet (Node Pool or MIG)"
  value       = var.orchestration_engine == "gke" ? google_container_node_pool.leaf_worker_pool[0].id : google_compute_region_instance_group_manager.leaf_mig_fleet[0].id
}

output "aggregator_fleet_id" {
  description = "Identifier of the provisioned Tree Aggregator fleet"
  value       = var.orchestration_engine == "gke" ? google_container_node_pool.aggregator_pool[0].id : ""
}
