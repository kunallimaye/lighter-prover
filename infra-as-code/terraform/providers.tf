terraform {
  required_version = ">= 1.5"

  required_providers {
    google = {
      source  = "hashicorp/google"
      version = "~> 5.0"
    }
  }
}

# ─── Three-role provider aliases (issue #141) ────────────────────────
#
# Even though Phase 1 only touches the build project, we keep the full
# alias triple here so Phase 2 (which adds runtime resources) doesn't
# need to refactor providers. The default provider points at the build
# project — matches Phase 1's only consumer.

provider "google" {
  project = var.build_project_id
  region  = var.build_region
}

provider "google" {
  alias   = "orchestration"
  project = coalesce(var.orchestration_project_id, var.build_project_id)
  region  = var.orchestration_region
}

provider "google" {
  alias   = "build"
  project = var.build_project_id
  region  = var.build_region
}

provider "google" {
  alias   = "runtime"
  project = coalesce(var.runtime_project_id, var.build_project_id)
  region  = var.runtime_region
}
