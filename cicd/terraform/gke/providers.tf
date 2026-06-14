# Default google providers point at the GKE project/region.
provider "google" {
  project = var.project_id
  region  = var.region
}

provider "google-beta" {
  project = var.project_id
  region  = var.region
}

# Short-lived OAuth token for the kubernetes provider. Cloud Build runs
# as tf-service-account (owner), so this token authenticates kubectl-style
# access to the freshly-created Autopilot cluster.
data "google_client_config" "default" {}

# The kubernetes provider is configured from the cluster created in this
# same root. Terraform resolves the dependency ordering: the
# google_container_cluster is created first, then its endpoint/CA feed
# the provider for the workload resources.
provider "kubernetes" {
  host                   = "https://${google_container_cluster.autopilot.endpoint}"
  token                  = data.google_client_config.default.access_token
  cluster_ca_certificate = base64decode(google_container_cluster.autopilot.master_auth[0].cluster_ca_certificate)
}
