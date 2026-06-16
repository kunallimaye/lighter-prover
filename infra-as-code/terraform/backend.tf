# State is stored in GCS. Backend config values are passed via
# Cloud Build substitutions (-backend-config flags).
terraform {
  backend "gcs" {
    # bucket and prefix are set via -backend-config in cloudbuild YAML
  }
}
