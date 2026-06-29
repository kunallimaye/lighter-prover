# Copyright (c) Elliot Technologies, Inc.
# SPDX-License-Identifier: BUSL-1.1

output "leaf_fleet_id" {
  description = "Identifier of the provisioned Leaf Worker fleet (Node Pool or MIG)"
  value       = try(google_container_node_pool.proving_pool[0].id, try(google_compute_region_instance_group_manager.leaf_mig_fleet[0].id, ""))
}

output "aggregator_fleet_id" {
  description = "Identifier of the provisioned Tree Aggregator fleet"
  value       = try(google_container_node_pool.proving_pool[0].id, "")
}

output "fungible_leaf_pool_id" {
  description = "Identifier of the GKE fungible leaf worker node pool (empty if disabled or not GKE)"
  value       = try(google_container_node_pool.fungible_leaf_pool[0].id, "")
}

output "fungible_agg_pool_id" {
  description = "Identifier of the GKE fungible aggregator node pool (empty if disabled or not GKE)"
  value       = try(google_container_node_pool.fungible_agg_pool[0].id, "")
}
