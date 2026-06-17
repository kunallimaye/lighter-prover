terraform {
  required_version = ">= 1.5"

  required_providers {
    google = {
      source  = "hashicorp/google"
      version = ">= 5.0"
    }
    google-beta = {
      source  = "hashicorp/google-beta"
      version = ">= 5.0"
    }
  }
}

# ─── Three-role provider aliases (issue #141) ────────────────────────

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

provider "google-beta" {
  alias   = "runtime_beta"
  project = coalesce(var.runtime_project_id, var.build_project_id)
  region  = var.runtime_region
}
