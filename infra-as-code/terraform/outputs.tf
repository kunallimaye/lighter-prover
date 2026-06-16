output "ar_repository_id" {
  description = "Artifact Registry repository ID."
  value       = google_artifact_registry_repository.bench.repository_id
}

output "ar_repository_url" {
  description = "Artifact Registry pull/push URL (region-docker.pkg.dev/PROJECT/REPO)."
  value       = "${google_artifact_registry_repository.bench.location}-docker.pkg.dev/${var.build_project_id}/${google_artifact_registry_repository.bench.repository_id}"
}

output "bench_image_uri_template" {
  description = "Template for tagging bench images. Append :<tag> to push or pull (e.g., :latest, :sha-<sha>)."
  value       = "${google_artifact_registry_repository.bench.location}-docker.pkg.dev/${var.build_project_id}/${google_artifact_registry_repository.bench.repository_id}/bench"
}
