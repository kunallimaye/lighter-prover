output "cluster_name" {
  description = "Name of the GKE Autopilot cluster."
  value       = google_container_cluster.autopilot.name
}

output "cluster_location" {
  description = "Region of the Autopilot cluster (used by get-credentials)."
  value       = google_container_cluster.autopilot.location
}

output "cluster_endpoint" {
  description = "Autopilot cluster API endpoint."
  value       = google_container_cluster.autopilot.endpoint
  sensitive   = true
}

output "get_credentials_command" {
  description = "Command to fetch kubeconfig for this cluster."
  value       = "gcloud container clusters get-credentials ${google_container_cluster.autopilot.name} --region ${google_container_cluster.autopilot.location} --project ${var.project_id}"
}

output "pubsub_topic" {
  description = "Pub/Sub topic for the dispatch backlog signal."
  value       = google_pubsub_topic.dispatch.name
}

output "pubsub_subscription" {
  description = "Pub/Sub subscription the backlog HPA watches."
  value       = google_pubsub_subscription.dispatch.name
}

output "chunk_plane" {
  description = "The inner chunk-dispatch + results Pub/Sub planes (issue #172). null fields when enable_chunk_plane = false."
  value = {
    enabled              = var.enable_chunk_plane
    chunk_topic          = var.enable_chunk_plane ? google_pubsub_topic.chunk[0].name : null
    chunk_subscription   = var.enable_chunk_plane ? google_pubsub_subscription.chunk[0].name : null
    results_topic        = var.enable_chunk_plane ? google_pubsub_topic.results[0].name : null
    results_subscription = var.enable_chunk_plane ? google_pubsub_subscription.results[0].name : null
  }
}

output "hpa_target_class" {
  description = "Which machine class the backlog HPA scales."
  value       = var.hpa_target_class
}

output "machine_classes" {
  description = "The two ADR-0006 machine classes and their (separately-sized) replica counts."
  value = {
    chunk_prover_cells = {
      replicas      = var.cell_replicas
      arch          = var.cell_arch
      compute_class = var.cell_compute_class
      cpu_request   = var.cell_cpu_request
      image         = var.cell_image
    }
    coordinators = {
      replicas          = var.coordinator_replicas
      arch              = var.coordinator_arch
      compute_class     = var.coordinator_compute_class
      cpu_request       = var.coordinator_cpu_request
      image             = var.coordinator_image
      safe_to_evict     = "false (hardwired — ADR-0003 amendment §3)"
      pdb_min_available = var.coordinator_pdb_min_available
    }
  }
}
