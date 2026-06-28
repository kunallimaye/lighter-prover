# Copyright (c) Elliot Technologies, Inc.
# SPDX-License-Identifier: BUSL-1.1

output "leaf_fleet_id" {
  description = "Identifier of the provisioned Leaf Worker fleet (Node Pool or MIG)"
  value       = try(google_container_node_pool.leaf_worker_pool[0].id, try(google_compute_region_instance_group_manager.leaf_mig_fleet[0].id, ""))
}

output "aggregator_fleet_id" {
  description = "Identifier of the provisioned Tree Aggregator fleet"
  value       = try(google_container_node_pool.aggregator_pool[0].id, "")
}

output "fungible_baseload_pool_id" {
  description = "Identifier of the committed baseload fungible node pool (empty if disabled or not GKE)"
  value       = try(google_container_node_pool.fungible_baseload_pool[0].id, "")
}

output "fungible_burst_pool_id" {
  description = "Identifier of the Spot burst fungible node pool (empty if disabled or not GKE)"
  value       = try(google_container_node_pool.fungible_burst_pool[0].id, "")
}
