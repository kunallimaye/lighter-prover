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
  count         = var.enable_shared_resources ? 1 : 0
  provider      = google.build
  project       = var.build_project_id
  location      = var.ar_region != "" ? var.ar_region : var.build_region
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
  count      = var.enable_shared_resources ? 1 : 0
  provider   = google.build
  project    = var.build_project_id
  location   = var.ar_region != "" ? var.ar_region : var.build_region
  repository = var.ar_repo
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
  count      = (var.enable_shared_resources && var.runtime_sa_email != "") ? 1 : 0
  provider   = google.build
  project    = var.build_project_id
  location   = var.ar_region != "" ? var.ar_region : var.build_region
  repository = var.ar_repo
  role       = "roles/artifactregistry.reader"
  member     = "serviceAccount:${var.runtime_sa_email}"
}

# ─── Prover GCE VMs (from config.toml [vms]) ─────────────────────────

resource "google_compute_instance" "prover_vms" {
  for_each     = var.enable_static_vms ? var.vms : {}
  provider     = google-beta.runtime_beta
  name         = each.key
  machine_type = each.value.machine_type
  zone         = each.value.zone
  project      = var.runtime_project_id != "" ? var.runtime_project_id : var.build_project_id

  boot_disk {
    initialize_params {
      image = each.value.image
      size  = each.value.disk_size_gb
      type  = each.value.disk_type
    }
  }

  dynamic "advanced_machine_features" {
    for_each = each.value.turbo_mode != "" ? [1] : []
    content {
      turbo_mode = each.value.turbo_mode
    }
  }

  network_interface {
    network = "default"
    access_config {
      # Ephemeral external IP
    }
  }

  service_account {
    email  = each.value.service_account != "" ? each.value.service_account : (var.runtime_sa_email != "" ? var.runtime_sa_email : "${var.runtime_project_id != "" ? var.runtime_project_id : var.build_project_id}-compute@developer.gserviceaccount.com")
    scopes = ["cloud-platform"]
  }

  labels = {
    project = "lighter-prover"
    managed = "terraform"
  }
}

# ─── Pub/Sub Work Queues with Role Filtering ──────────────────────────

data "google_pubsub_topic" "work_topic" {
  provider = google-beta.runtime_beta
  name     = "prover-work-topic"
  project  = var.runtime_project_id != "" ? var.runtime_project_id : var.build_project_id
}

resource "google_pubsub_subscription" "leaf_sub" {
  provider = google-beta.runtime_beta
  name     = "prover-leaf-work-sub"
  topic    = data.google_pubsub_topic.work_topic.name
  project  = data.google_pubsub_topic.work_topic.project

  # 180s ack deadline (matching default ack_deadline in config.toml)
  ack_deadline_seconds = 180

  # Only route leaf proving tasks to this subscription
  filter = "attributes.role = \"leaf\""

  # Retain acked messages for 1 day for safety/debugging
  retain_acked_messages = true
  message_retention_duration = "86400s"

  expiration_policy {
    ttl = "" # Never expire
  }
}

resource "google_pubsub_subscription" "agg_sub" {
  provider = google-beta.runtime_beta
  name     = "prover-agg-work-sub"
  topic    = data.google_pubsub_topic.work_topic.name
  project  = data.google_pubsub_topic.work_topic.project

  ack_deadline_seconds = 180

  # Route ALL folding tasks here, for BOTH fold strategies:
  #   * hex fold descriptors carry attributes.role = "tree-node"
  #     (Role::TreeNode.as_str()); and
  #   * (#321 Phase 9) order-free REDUCTION fold descriptors carry
  #     attributes.role = "reduction-fold" (Role::ReductionFold.as_str()).
  # BUG B1: this filter previously matched only "tree-node", so reduction folds
  # (the DEFAULT GKE strategy since #321 Phase 8) matched NEITHER this sub NOR
  # the leaf sub and were silently dropped — workers stalled and no root was ever
  # produced (the attempt-46 GKE failure). Both role names must be accepted.
  #
  # OPERATOR CAVEAT — Pub/Sub subscription filters are IMMUTABLE after creation.
  # Changing this string forces Terraform to REPLACE (destroy + recreate) the
  # subscription. A run against a PRE-EXISTING subscription that still carries the
  # OLD "tree-node"-only filter will KEEP dropping reduction folds — so a GKE
  # reduction run MUST use a subscription created (or replaced) with this updated
  # filter. Verify with `terraform plan` that `agg_sub` is being replaced, not
  # left in place, before the next reduction run.
  filter = "attributes.role = \"tree-node\" OR attributes.role = \"reduction-fold\""

  retain_acked_messages = true
  message_retention_duration = "86400s"

  expiration_policy {
    ttl = "" # Never expire
  }
}

# ─── UNIFIED fungible-pool subscription (#321 — unified topology) ──────
#
# The UNIFIED pool topology (opt-in, selected via --pool-topology=unified /
# POOL_TOPOLOGY=unified) runs a SINGLE fungible Deployment where EVERY pod pulls
# BOTH leaf and fold work from this one subscription and self-balances the
# leaf-vs-fold mix moment-to-moment. This contrasts with the fixed SPLIT path
# (leaf_sub + agg_sub above), which statically partitions the fleet into leaf
# pods and agg pods and idles two-thirds of the fleet during whichever phase is
# running. Both paths stay valid so we can A/B them; SPLIT remains the default.
#
# NO role filter: ALL work (leaf, hex tree-node, reduction-fold) routes to this
# one subscription so a single pool drains everything. (An equivalent explicit
# filter would be `attributes.role = "leaf" OR attributes.role = "tree-node" OR
# attributes.role = "reduction-fold"`, but no-filter is simpler and future-proof
# to new roles.)
#
# ack_deadline_seconds = 180: a unified pod may pull a FOLD job (the bigger,
# slower work — up to ~180s here, matching agg_sub's fold deadline), so the
# unified deadline must be the MAX of leaf/fold, i.e. the fold deadline. A
# too-short deadline would redeliver in-flight folds and duplicate work.
#
# OPERATOR CAVEAT — Pub/Sub subscription filters are IMMUTABLE after creation
# (same caveat as leaf_sub/agg_sub above; see Phase 9 / #336). `unified_sub` is
# NEW, so its first `terraform apply` only CREATES it (no replace needed for it).
# But if you ever change THIS filter (e.g. add an explicit role filter) later,
# Terraform must REPLACE (destroy + recreate) the subscription — verify with
# `terraform plan` before applying.
resource "google_pubsub_subscription" "unified_sub" {
  provider = google-beta.runtime_beta
  name     = "prover-unified-work-sub"
  topic    = data.google_pubsub_topic.work_topic.name
  project  = data.google_pubsub_topic.work_topic.project

  # Max of leaf/fold deadlines: a unified pod may pull a fold (180s).
  ack_deadline_seconds = 180

  # No filter: route ALL work (leaf + every fold role) to this one subscription.

  retain_acked_messages = true
  message_retention_duration = "86400s"

  expiration_policy {
    ttl = "" # Never expire
  }
}

