# GKE Autopilot deployment-automation module (issue #151, G4 enabler).
#
# This is a SEPARATE Terraform root from the Phase-1 module
# (cicd/terraform/) on purpose: it owns its own GCS state prefix so a GKE
# stand-up/teardown cycle never touches the long-lived Artifact Registry
# state. This is the ADR-0003-amendment §5 / ADR-0005 §5 / ADR-0006 §5
# "platform seam" expressed in Terraform: the GKE backend is a NEW root,
# not a redesign of the existing infra. A future GKE-Standard or MIG
# backend would be a sibling root, selected by the `platform` field, not
# a rewrite of this one.

terraform {
  required_version = ">= 1.5"

  required_providers {
    google = {
      source  = "hashicorp/google"
      version = "~> 5.0"
    }
    google-beta = {
      source  = "hashicorp/google-beta"
      version = "~> 5.0"
    }
    kubernetes = {
      source  = "hashicorp/kubernetes"
      version = "~> 2.30"
    }
  }
}
