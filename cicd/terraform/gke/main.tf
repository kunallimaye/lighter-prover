# ─────────────────────────────────────────────────────────────────────
# GKE Autopilot deployment automation for the distributed prover.
#
# Encodes the ADR-0006 deployment TOPOLOGY (two machine classes) and the
# ADR-0003-amendment platform decision (GKE Autopilot) + its HARD DAY-1
# eviction mitigation, parametrised so the same module accepts a smoke
# config now and a production config (sizing #95) later.
#
# What lives here:
#   1. A GKE Autopilot cluster.
#   2. Pub/Sub topic + subscription (the block-dispatch backlog signal).
#   3. Workload Identity binding for the metrics adapter.
#   4. The custom-metrics-stackdriver-adapter (external-metrics path).
#   5. The two machine-class workloads (cells + coordinators).
#   6. The coordinator PodDisruptionBudget + safe-to-evict annotation
#      (the NON-NEGOTIABLE day-1 mitigation).
#   7. The HPA driven by the Pub/Sub num_undelivered_messages metric.
# ─────────────────────────────────────────────────────────────────────

locals {
  # The coordinator pod annotation is HARDWIRED, not configurable. A bad
  # tfvars cannot drop the day-1 mitigation. (ADR-0003 amendment §3.)
  coordinator_safe_to_evict_annotation = {
    "cluster-autoscaler.kubernetes.io/safe-to-evict" = "false"
  }

  workloads_on = var.enable_workloads ? 1 : 0
  adapter_on   = var.enable_workloads && var.metrics_adapter_enabled ? 1 : 0

  # Which deployment the backlog HPA targets.
  hpa_target_deployment = var.hpa_target_class == "coordinator" ? "coordinator" : "cells"

  proof_store_on = var.enable_proof_store ? 1 : 0

  # Resolved proof-store bucket name: an explicit override, else a
  # deterministic project-derived name (bucket names are globally unique).
  proof_store_bucket_name = var.proof_store_bucket != "" ? var.proof_store_bucket : "${var.project_id}-lighter-prover-proofs"

  # Issue #206: gcsfuse-mount the proof bucket into the coordinator pod. Only
  # meaningful when the bucket + pod-GSA permission exist (enable_proof_store),
  # so AND the two so a stray enable_proof_mount can't try to mount a bucket
  # that was never created.
  proof_mount_on = var.enable_proof_mount && var.enable_proof_store

  # The gcsfuse CSI driver is opted in per-pod via this annotation; merged with
  # the hardwired safe-to-evict annotation on the coordinator template.
  coordinator_annotations = merge(
    local.coordinator_safe_to_evict_annotation,
    local.proof_mount_on ? { "gke-gcsfuse/volumes" = "true" } : {}
  )
}

# ─── 1. GKE Autopilot cluster ────────────────────────────────────────
#
# Autopilot mode: Google manages nodes, node pools, and bin-packing.
# That bin-packing is exactly what makes the coordinator eviction
# mitigation (below) mandatory. We do NOT define node pools — on
# Autopilot the two machine CLASSES are expressed as the two workloads +
# their scheduling constraints (compute class / arch nodeSelector /
# resource requests), not as managed node pools.

resource "google_container_cluster" "autopilot" {
  provider = google-beta
  name     = var.cluster_name
  location = var.region
  project  = var.project_id

  enable_autopilot = true

  release_channel {
    channel = var.release_channel
  }

  # Smoke clusters must be destroyable.
  deletion_protection = var.deletion_protection

  resource_labels = var.resource_labels

  # Workload Identity is on by default for Autopilot; declaring it makes
  # the dependency explicit for the metrics-adapter binding.
  workload_identity_config {
    workload_pool = "${var.project_id}.svc.id.goog"
  }
}

# ─── 2. Pub/Sub: the block-dispatch backlog signal ───────────────────
# ADR-0006 §1.1 outer tier: competing-pull Pub/Sub. The HPA watches this
# subscription's num_undelivered_messages (the backlog). At smoke scale
# we hand-publish messages to make the backlog climb on demand.

resource "google_pubsub_topic" "dispatch" {
  name    = var.pubsub_topic
  project = var.project_id
  labels  = var.resource_labels
}

resource "google_pubsub_subscription" "dispatch" {
  name    = var.pubsub_subscription
  topic   = google_pubsub_topic.dispatch.id
  project = var.project_id
  labels  = var.resource_labels

  # Long ack-deadline so hand-published smoke messages stay
  # un-acked (backlog visible) long enough for the HPA to react.
  ack_deadline_seconds = 600
}

# ─── 2b. Pub/Sub: the INNER chunk-dispatch plane (issue #172) ────────
# ADR-0006 §1.2 inner tier: the coordinator SPLITs each block into chunks
# and fans the chunk REFERENCES out to the cell pods over this topic. Cells
# competing-pull the chunk subscription. References only — the witness bytes
# never travel the bus (ADR-0008 §1.2). Created only when
# `enable_chunk_plane = true` so the smoke automation (which only validates
# the topology + backlog HPA) is unchanged by default.

resource "google_pubsub_topic" "chunk" {
  count   = var.enable_chunk_plane ? 1 : 0
  name    = var.chunk_topic
  project = var.project_id
  labels  = var.resource_labels
}

resource "google_pubsub_subscription" "chunk" {
  count   = var.enable_chunk_plane ? 1 : 0
  name    = var.chunk_subscription
  topic   = google_pubsub_topic.chunk[0].id
  project = var.project_id
  labels  = var.resource_labels

  # A chunk's L1+L2 prove is multi-second; give cells ample ack headroom so
  # an in-flight chunk is not redelivered mid-prove. A future native
  # manual-ack cell client would ack after the proof is emitted.
  ack_deadline_seconds = var.chunk_ack_deadline_seconds
}

# ─── 2c. Pub/Sub: the chunk-RESULTS plane (issue #172) ───────────────
# Cells report each proven chunk's result (prove_ms, witness_fetch_ms, ok)
# back to the coordinator over this topic; the coordinator pulls the results
# subscription to GATHER/FOLD per block (ADR-0006 §1.2 GATHER).

resource "google_pubsub_topic" "results" {
  count   = var.enable_chunk_plane ? 1 : 0
  name    = var.results_topic
  project = var.project_id
  labels  = var.resource_labels
}

resource "google_pubsub_subscription" "results" {
  count   = var.enable_chunk_plane ? 1 : 0
  name    = var.results_subscription
  topic   = google_pubsub_topic.results[0].id
  project = var.project_id
  labels  = var.resource_labels

  ack_deadline_seconds = 60
}

# ─── 2c-bis. Pub/Sub: the MERGE-TASK plane (issue #198) ──────────────
# Cross-machine fold fan-out: to shard ONE block's merge tree across separate
# coordinator machines, the block's owning coordinator (the leader) emits one
# MergeTaskMessage per merge pair to this topic; idle fold-worker pods
# competing-pull the merge-task subscription, prove ONE merge each on their
# FULL core budget (no thread rationing), upload the output to the proof store
# under {height}/m/{level}/{index}, and report on the merge-RESULT plane.
# Scale the fold by adding MORE fold workers, not bigger boxes (the governing
# principle). Gated by `enable_merge_plane` so smoke automation that does not
# exercise the distributed fold can skip provisioning it.

resource "google_pubsub_topic" "merge_task" {
  count   = var.enable_merge_plane ? 1 : 0
  name    = var.merge_task_topic
  project = var.project_id
  labels  = var.resource_labels
}

resource "google_pubsub_subscription" "merge_task" {
  count   = var.enable_merge_plane ? 1 : 0
  name    = var.merge_task_subscription
  topic   = google_pubsub_topic.merge_task[0].id
  project = var.project_id
  labels  = var.resource_labels

  # A single merge prove is ~1.6 s on a c4a-standard-4 (the pilot fact); give
  # workers ample ack headroom so an in-flight merge is not redelivered
  # mid-prove. A future native manual-ack client tightens this.
  ack_deadline_seconds = var.merge_ack_deadline_seconds
}

# ─── 2c-ter. Pub/Sub: the MERGE-RESULT plane (issue #198) ────────────
# Fold workers report each proven (or honestly-failed) merge back to the
# leader over this topic; the leader pulls the merge-result subscription to
# barrier each tree level (level n+1 releases only once every level-n result
# has landed) and re-sort by stable in-level index for the #193 determinism
# contract.

resource "google_pubsub_topic" "merge_result" {
  count   = var.enable_merge_plane ? 1 : 0
  name    = var.merge_result_topic
  project = var.project_id
  labels  = var.resource_labels
}

resource "google_pubsub_subscription" "merge_result" {
  count   = var.enable_merge_plane ? 1 : 0
  name    = var.merge_result_subscription
  topic   = google_pubsub_topic.merge_result[0].id
  project = var.project_id
  labels  = var.resource_labels

  ack_deadline_seconds = 60
}

# ─── 2d. Shared proof store: the L2-leaf-proof bucket (issue #179) ────
# The fan-IN half of the distributed prover (issue #179) needs proof BYTES
# to cross from cells to the coordinator. Pub/Sub message-size limits make
# inline L2 proofs impractical, so cells write each L2 leaf proof to this
# shared GCS bucket keyed by {height, witness_index}; ChunkResultMessage
# carries the object key. The coordinator (a LATER slice of #179) fetches
# the k proofs from here and runs the real merge tree + L4.
#
# Gated behind enable_proof_store so the default smoke topology is unchanged.
# Uniform bucket-level access (IAM only — no legacy ACLs) and versioning
# off (proofs are write-once, keyed; re-prove overwrites idempotently).

resource "google_storage_bucket" "proof_store" {
  count = local.proof_store_on

  name     = local.proof_store_bucket_name
  project  = var.project_id
  location = var.proof_store_location
  labels   = var.resource_labels

  # IAM-only access; no object ACLs (the pod GSA is granted objectAdmin via
  # the bucket-scoped IAM member below — least privilege, bucket not project).
  uniform_bucket_level_access = true

  # Smoke/scale validation buckets must be destroyable even with proofs in
  # them (parametrised — set false to retain proofs).
  force_destroy = var.proof_store_force_destroy
}

# Grant the EXISTING pod GSA objectAdmin on the proof-store bucket ONLY.
# Bucket-scoped (google_storage_bucket_iam_member), NOT a project-wide
# google_project_iam_member — least privilege per issue #179 WS1. The SA
# already exists (created out-of-band with the pubsub roles); we do NOT
# create it here, we only add this one bucket binding.
resource "google_storage_bucket_iam_member" "proof_store_pod_object_admin" {
  count = local.proof_store_on

  bucket = google_storage_bucket.proof_store[0].name
  role   = "roles/storage.objectAdmin"
  member = "serviceAccount:${var.proof_store_pod_gsa_email}"
}

# ─── 3. Workload Identity for the metrics adapter ────────────────────
# The custom-metrics-stackdriver-adapter reads Cloud Monitoring. On
# Autopilot it runs under Workload Identity; bind its KSA
# (custom-metrics/custom-metrics-stackdriver-adapter) to a GSA with
# monitoring.viewer. We use the project's default compute SA (owner-tier
# Cloud Build already has monitoring), kept minimal via the viewer role.

resource "google_project_iam_member" "adapter_monitoring_viewer" {
  count   = local.adapter_on
  project = var.project_id
  role    = "roles/monitoring.viewer"
  member  = "serviceAccount:${var.project_id}.svc.id.goog[custom-metrics/custom-metrics-stackdriver-adapter]"
}

# ─── 4. The two machine-class workloads ──────────────────────────────
#
# CLASS 1 — chunk-prover CELLS. CPU-saturating whole-machine pods in
# production (c4a-highcpu-64 / Axion). The machine class is selected by
# the arch nodeSelector (+ optional Autopilot compute class) and the
# resource requests. At smoke scale this is a trivial no-op workload.

resource "kubernetes_deployment" "cells" {
  count = local.workloads_on

  metadata {
    name      = "prover-cells"
    namespace = "default"
    labels = {
      app           = "prover-cells"
      machine-class = "chunk-prover-cell"
    }
  }

  spec {
    replicas = var.cell_replicas

    selector {
      match_labels = {
        app = "prover-cells"
      }
    }

    template {
      metadata {
        labels = {
          app           = "prover-cells"
          machine-class = "chunk-prover-cell"
        }
      }

      spec {
        node_selector = merge(
          { "kubernetes.io/arch" = var.cell_arch },
          var.cell_compute_class == "" ? {} : { "cloud.google.com/compute-class" = var.cell_compute_class },
          var.cell_machine_family == "" ? {} : { "cloud.google.com/machine-family" = var.cell_machine_family }
        )

        container {
          name    = "cell"
          image   = var.cell_image
          command = length(var.cell_command) > 0 ? var.cell_command : null

          resources {
            requests = {
              cpu    = var.cell_cpu_request
              memory = var.cell_memory_request
            }
            limits = {
              cpu    = var.cell_cpu_request
              memory = var.cell_memory_request
            }
          }
        }
      }
    }
  }

  # The provider needs the cluster ready; the token is short-lived so we
  # also depend on the cluster explicitly.
  depends_on = [google_container_cluster.autopilot]
}

# CLASS 2 — COORDINATORS. A DISTINCT compute class (fold L2 + prove L4).
# This is where the HARD DAY-1 mitigation lives.

resource "kubernetes_deployment" "coordinator" {
  count = local.workloads_on

  metadata {
    name      = "prover-coordinator"
    namespace = "default"
    labels = {
      app           = "prover-coordinator"
      machine-class = "coordinator"
    }
  }

  spec {
    replicas = var.coordinator_replicas

    selector {
      match_labels = {
        app = "prover-coordinator"
      }
    }

    template {
      metadata {
        labels = {
          app           = "prover-coordinator"
          machine-class = "coordinator"
        }
        # ── HARD DAY-1 REQUIREMENT (ADR-0003 amendment §3) ──
        # safe-to-evict=false so Autopilot will not evict an in-flight,
        # key-resident coordinator for bin-packing. HARDWIRED via locals
        # — a bad tfvars cannot remove it. Issue #206: when the proof bucket
        # is gcsfuse-mounted, the `gke-gcsfuse/volumes=true` opt-in is merged
        # in (local.coordinator_annotations).
        annotations = local.coordinator_annotations
      }

      spec {
        node_selector = merge(
          { "kubernetes.io/arch" = var.coordinator_arch },
          var.coordinator_compute_class == "" ? {} : { "cloud.google.com/compute-class" = var.coordinator_compute_class },
          var.coordinator_machine_family == "" ? {} : { "cloud.google.com/machine-family" = var.coordinator_machine_family }
        )

        container {
          name    = "coordinator"
          image   = var.coordinator_image
          command = length(var.coordinator_command) > 0 ? var.coordinator_command : null

          # Issue #206: point the bench binary at the gcsfuse mount so
          # storage.rs selects mount-mode file I/O (LIGHTER_PROOF_MOUNT).
          # Only set when the bucket is actually mounted below.
          dynamic "env" {
            for_each = local.proof_mount_on ? [1] : []
            content {
              name  = "LIGHTER_PROOF_MOUNT"
              value = var.proof_mount_path
            }
          }

          # Issue #206: mount the gcsfuse CSI volume into the container at the
          # path the bench binary reads from.
          dynamic "volume_mount" {
            for_each = local.proof_mount_on ? [1] : []
            content {
              name       = "proof-store"
              mount_path = var.proof_mount_path
              # read_write: fold workers WRITE intermediate merge proofs +
              # READ inputs through this mount (the #206 transit surface).
              read_only = false
            }
          }

          resources {
            requests = {
              cpu    = var.coordinator_cpu_request
              memory = var.coordinator_memory_request
            }
            limits = {
              cpu    = var.coordinator_cpu_request
              memory = var.coordinator_memory_request
            }
          }
        }

        # Issue #206: the gcsfuse CSI ephemeral inline volume backed by the
        # SAME proof-store bucket #179 created. The pod GSA already holds
        # objectAdmin on it (proof_store_pod_object_admin), so NO new IAM is
        # needed. implicit_dirs lets the `{height}/m/{level}/` key prefixes
        # resolve as directories on the bucket.
        dynamic "volume" {
          for_each = local.proof_mount_on ? [1] : []
          content {
            name = "proof-store"
            csi {
              driver    = "gcsfuse.csi.storage.gke.io"
              read_only = false
              volume_attributes = {
                bucketName   = local.proof_store_bucket_name
                mountOptions = "implicit-dirs"
              }
            }
          }
        }
      }
    }
  }

  depends_on = [google_container_cluster.autopilot]
}

# ── HARD DAY-1 REQUIREMENT (ADR-0003 amendment §3) ──
# PodDisruptionBudget for the coordinator pool. Combined with the
# safe-to-evict annotation, this blocks a voluntary eviction / drain of
# an in-flight coordinator. minAvailable defaults to all replicas.
resource "kubernetes_pod_disruption_budget_v1" "coordinator" {
  count = local.workloads_on

  metadata {
    name      = "prover-coordinator-pdb"
    namespace = "default"
    labels = {
      app           = "prover-coordinator"
      machine-class = "coordinator"
    }
  }

  spec {
    min_available = var.coordinator_pdb_min_available
    selector {
      match_labels = {
        app = "prover-coordinator"
      }
    }
  }

  depends_on = [kubernetes_deployment.coordinator]
}

# ─── 5. The custom-metrics-stackdriver-adapter (external-metrics path) ─
# Installs the adapter that exposes Cloud Monitoring metrics (including
# pubsub.googleapis.com|subscription|num_undelivered_messages) to the
# Kubernetes external-metrics API the HPA reads. Applied as a raw
# manifest via kubectl in the deploy pipeline (Terraform's
# kubernetes_manifest can't apply a multi-doc remote URL cleanly), so
# this resource only records the intent + the WI binding above. The
# pipeline (scripts/gke-smoke.sh) applies the manifest URL.
#
# NOTE: kept as a null marker so `terraform output` can report whether
# the adapter is expected; the actual `kubectl apply -f <url>` happens in
# the deploy script where kubeconfig is available with retries.

# ─── 6. The Pub/Sub-backlog HPA ──────────────────────────────────────
# Scales the target workload on the subscription's num_undelivered_messages
# external metric. Target value is per-replica (averageValue), so a
# hand-published backlog of N messages with a small target demonstrably
# raises desiredReplicas — proving the metric path is REAL.

resource "kubernetes_horizontal_pod_autoscaler_v2" "backlog" {
  count = local.adapter_on

  metadata {
    name      = "prover-backlog-hpa"
    namespace = "default"
    labels    = { app = "prover-${local.hpa_target_deployment}" }
  }

  spec {
    min_replicas = var.hpa_min_replicas
    max_replicas = var.hpa_max_replicas

    scale_target_ref {
      api_version = "apps/v1"
      kind        = "Deployment"
      name        = local.hpa_target_deployment == "coordinator" ? "prover-coordinator" : "prover-cells"
    }

    metric {
      type = "External"
      external {
        metric {
          name = "pubsub.googleapis.com|subscription|num_undelivered_messages"
          selector {
            match_labels = {
              "resource.labels.subscription_id" = var.pubsub_subscription
            }
          }
        }
        target {
          type          = "AverageValue"
          average_value = tostring(var.hpa_backlog_target)
        }
      }
    }
  }

  depends_on = [
    kubernetes_deployment.cells,
    kubernetes_deployment.coordinator,
  ]
}
