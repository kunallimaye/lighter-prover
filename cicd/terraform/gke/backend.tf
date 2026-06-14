# State is stored in GCS, same bucket as the Phase-1 module but a
# DISTINCT prefix (passed via -backend-config in the GKE Cloud Build
# pipelines). Keeping the GKE state separate means a `terraform destroy`
# of the smoke cluster can never corrupt or race the Artifact Registry
# state.
terraform {
  backend "gcs" {
    # bucket and prefix are set via -backend-config in the cloudbuild YAML
    # (e.g. prefix=lighter-prover/gke).
  }
}
