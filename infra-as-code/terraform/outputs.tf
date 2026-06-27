output "ar_repository_id" {
  description = "Artifact Registry repository ID."
  value       = var.ar_repo
}

output "ar_repository_url" {
  description = "Artifact Registry pull/push URL (region-docker.pkg.dev/PROJECT/REPO)."
  value       = "${var.ar_region != "" ? var.ar_region : var.build_region}-docker.pkg.dev/${var.build_project_id}/${var.ar_repo}"
}

output "bench_image_uri_template" {
  description = "Template for tagging bench images. Append :<tag> to push or pull (e.g., :latest, :sha-<sha>)."
  value       = "${var.ar_region != "" ? var.ar_region : var.build_region}-docker.pkg.dev/${var.build_project_id}/${var.ar_repo}/bench"
}
