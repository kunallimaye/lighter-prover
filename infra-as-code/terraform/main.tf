# ─── Phase 1 scope: AR repo for the lighter bench image ──────────────
#
# This Terraform manages the minimal infrastructure needed to land the
# Phase 1 deliverable (issue #2):
#
#   1. An Artifact Registry repository in the build project that
#      cloudbuild.yaml pushes the bench image to.
#   2. A least-privilege IAM grant so the Cloud Build default service
#      account can push to that repo.
#
# Out of scope for Phase 1 (deferred to Phase 2 or follow-up issues):
#   * The runtime SA + Cloud Run service. Phase 1 invokes Cloud Run
#     Jobs ad-hoc from `scripts/cloud.sh`; we don't need a long-lived
#     Cloud Run service yet.
#   * External HTTPS LB + DNS. The bench produces stdout/stderr only,
#     not a network endpoint.
#
# Resource-construction discipline (per issue #141 lesson 1): this
# module never enables APIs or grants project-wide IAM. Those are
# Owner-tier operations and belong in admin-cloud-init.

# ─── Artifact Registry repository ────────────────────────────────────
#
# Bench image lives here. Format = DOCKER (vs MAVEN/NPM/PYTHON/etc.).
# Region matches the build region by default; cross-region pulls are
# slower and cost more, so callers wanting cross-region should override
# build_region rather than introduce a separate runtime_ar_region.

resource "google_artifact_registry_repository" "bench" {
  provider      = google.build
  project       = var.build_project_id
  location      = var.build_region
  repository_id = var.ar_repo
  description   = "Lighter prover bench images (Phase 1)"
  format        = "DOCKER"

  labels = {
    project = "lighter-prover"
    phase   = "1"
    managed = "terraform"
  }
}

# ─── IAM: let the Cloud Build default SA push to the repo ───────────
#
# The default Cloud Build SA (<PROJECT_NUMBER>@cloudbuild.gserviceaccount.com)
# is what `gcloud builds submit` runs as unless a custom service account is
# provided. Granting artifactregistry.writer on the specific repo (not
# project-wide) keeps the blast radius tight.
#
# We pass the SA email in as a variable so callers using a custom builder
# SA (admin-cloud-init flow) bind the right identity.

resource "google_artifact_registry_repository_iam_member" "builder_writer" {
  provider   = google.build
  project    = var.build_project_id
  location   = google_artifact_registry_repository.bench.location
  repository = google_artifact_registry_repository.bench.repository_id
  role       = "roles/artifactregistry.writer"
  member     = "serviceAccount:${var.builder_sa_email}"
}

# Optional: when a separate runtime SA is provided (split-project
# tenancy), grant it reader access so Cloud Run Jobs in the runtime
# project can pull images from the build-project AR repo. When
# runtime_sa_email is empty (single-project default), this binding is
# skipped — runtime workloads in the same project have implicit reader
# access via the AR repo's project-level discovery.

resource "google_artifact_registry_repository_iam_member" "runtime_reader" {
  count      = var.runtime_sa_email == "" ? 0 : 1
  provider   = google.build
  project    = var.build_project_id
  location   = google_artifact_registry_repository.bench.location
  repository = google_artifact_registry_repository.bench.repository_id
  role       = "roles/artifactregistry.reader"
  member     = "serviceAccount:${var.runtime_sa_email}"
}
