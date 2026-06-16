# ─────────────────────────────────────────────────────────────────────
# SCALE-0.5% S=9 VARIANT — CELL forced onto a whole c4a-highcpu-64.
#
# REPRODUCIBILITY ARTIFACT — NOT MERGED TO main (S=9 baseline stays the
# committed default with cell_cpu_request=43). This file is a branch-only
# variant for the Phase A machine-size-curve comparison run tracked in
# issue #279 and recorded on #214.
#
# WHAT DIFFERS FROM scale-0p5pct.tfvars (the comparison axis = CELL ONLY):
#   1. cell_cpu_request 43 -> "61"  : forces a 64-vCPU node (64 minus ~3
#      Autopilot system reservation -> Autopilot schedules ONE cell pod per
#      whole c4a-highcpu-64 box). The committed baseline's 43 vCPU request
#      right-sizes Autopilot DOWN to a c4a-highcpu-48 node (the standing
#      hc48-vs-hc64 ambiguity this run resolves: req=43 lands hc48, NOT
#      hc64, despite the baseline comment).
#   2. cell_memory_request 44Gi -> "60Gi" : a LIVE finding from 3 prior runs
#      is that the MEMORY request drives Autopilot SKU-FAMILY selection —
#      too-high memory silently upsizes off the lean `highcpu` family onto
#      `standard`/`highmem` (a pricier box, label != highcpu-64). 60Gi
#      stays <= ~2 GB/vCPU (61 vCPU * 2 = 122Gi ceiling; 60Gi << 122) so it
#      keeps the highcpu-64 SKU while clearing the measured ~5.3 GB S=9 cell
#      RSS peak with huge headroom. A user-proposed 115Gi would have
#      misfired (>2GB/vCPU -> off highcpu).
#   3. Isolation: unique cluster (lighter-prover-s9hc64, on CLI), TF state
#      prefix (gke-scale-s9hc64, on CLI), and an `s9hc64` tier prefix on ALL
#      Pub/Sub topics/subs + a dedicated proof bucket so this run never
#      collides with any other run's state/resources.
#
#   Coordinator + fold-workers are UNCHANGED from the baseline whole-c4a
#   config (cpu 43 / mem 44Gi -> c4a-highcpu-48) so the only moving part is
#   the CELL machine size. --tx-per-proof 9 on ALL THREE roles (S=9).
# ─────────────────────────────────────────────────────────────────────

cluster_name        = "lighter-prover-s9hc64"
release_channel     = "REGULAR"
deletion_protection = false # scaled TEST config — keep teardown clean

# All labels lowercase (a prior run hit a GCP label-rejection on uppercase).
resource_labels = {
  managed = "terraform"
  purpose = "gke-scale-ladder"
  issue   = "279"
  tier    = "s9hc64"
}

# ── Machine CLASS 1: chunk-prover CELLS — FORCED c4a-highcpu-64 ──
# CPU-saturating whole-machine pods on c4a-highcpu-64 (Axion/neoverse-v2,
# arm64). cell_cpu_request="61" forces the 64-vCPU node; one cell fills it.
cell_replicas       = 9
cell_compute_class  = "Performance" # Autopilot: Performance+c4a = real Axion; Scale-Out=t2a/neoverse-n1 SIGILLs the neoverse-v2 binary (live finding)
cell_machine_family = "c4a"
cell_arch           = "arm64"
cell_cpu_request    = "61"   # 64 vCPU minus ~3 Autopilot reservation -> FORCES a c4a-highcpu-64 node, one pod fills it
cell_memory_request = "60Gi" # <= ~2 GB/vCPU keeps the highcpu-64 SKU; clears the ~5.3 GB S=9 cell RSS peak with huge headroom (do NOT raise above ~60Gi or Autopilot upsizes off highcpu)
cell_image          = "us-central1-docker.pkg.dev/kunal-scratch/lighter-prover/bench:b0c84cb3bb1d8e799bf7b291bcf9e9b4560ea947-neoverse-v2"
cell_command = [
  "/usr/local/bin/prover", "--mode", "cell",
  "--chunk-subscription", "lighter-prover-s9hc64-chunk-sub",
  "--results-topic", "lighter-prover-s9hc64-results",
  "--proof-mount-path", "/mnt/proof-store",
  "--tx-per-proof", "9",
  "--poll-interval-s", "2",
]

# ── Machine CLASS 2: COORDINATORS — UNCHANGED baseline whole-c4a ──
coordinator_replicas       = 1
coordinator_compute_class  = "Performance"
coordinator_machine_family = "c4a"
coordinator_arch           = "arm64"
coordinator_cpu_request    = "43"   # baseline whole box (lands c4a-highcpu-48) — UNCHANGED (comparison axis = CELL only)
coordinator_memory_request = "44Gi"
coordinator_image          = "us-central1-docker.pkg.dev/kunal-scratch/lighter-prover/bench:b0c84cb3bb1d8e799bf7b291bcf9e9b4560ea947-neoverse-v2"
coordinator_command = [
  "/usr/local/bin/prover", "--mode", "coordinator",
  "--dispatch-subscription", "lighter-prover-s9hc64-dispatch-sub",
  "--chunk-topic", "lighter-prover-s9hc64-chunk",
  "--results-subscription", "lighter-prover-s9hc64-results-sub",
  "--proof-mount-path", "/mnt/proof-store",
  "--fold-distributed",
  "--merge-task-topic", "lighter-prover-s9hc64-merge-task",
  "--merge-result-subscription", "lighter-prover-s9hc64-merge-result-sub",
  "--native-merge-plane",
  "--tx-per-proof", "9",
  "--poll-interval-s", "2",
]

coordinator_pdb_min_available = 1

# ── Machine CLASS 3: FOLD WORKERS — UNCHANGED baseline whole-c4a ──
enable_fold_workers        = true
fold_worker_replicas       = 8 # scales with the tier (cells=9); scale the fold by worker count (#198)
fold_worker_compute_class  = "Performance"
fold_worker_machine_family = "c4a"
fold_worker_arch           = "arm64"
fold_worker_cpu_request    = "43"   # baseline whole box (lands c4a-highcpu-48) — UNCHANGED
fold_worker_memory_request = "44Gi"
fold_worker_image          = "us-central1-docker.pkg.dev/kunal-scratch/lighter-prover/bench:b0c84cb3bb1d8e799bf7b291bcf9e9b4560ea947-neoverse-v2"
fold_worker_command = [
  "/usr/local/bin/prover", "--mode", "fold-worker",
  "--merge-task-subscription", "lighter-prover-s9hc64-merge-task-sub",
  "--merge-result-topic", "lighter-prover-s9hc64-merge-result",
  "--proof-mount-path", "/mnt/proof-store",
  "--native-merge-plane",
  "--tx-per-proof", "9",
  "--poll-interval-s", "2",
]

# ── Inner chunk-dispatch + results planes (s9hc64-isolated names) ──
enable_chunk_plane   = true
chunk_topic          = "lighter-prover-s9hc64-chunk"
chunk_subscription   = "lighter-prover-s9hc64-chunk-sub"
results_topic        = "lighter-prover-s9hc64-results"
results_subscription = "lighter-prover-s9hc64-results-sub"

# ── REAL coordinator-side fold (merge tree + L4) — #209/#262 ──
enable_proof_store = true
enable_proof_mount = true
enable_merge_plane = true
enable_zone_spread = true

# Dedicated, isolated proof bucket + location follows the cluster region.
proof_store_bucket   = "kunal-scratch-lighter-prover-s9hc64-proofs"
proof_store_location = "us-east4"

# Merge-plane Pub/Sub names (s9hc64-isolated) — MATCH the flag values above.
merge_task_topic          = "lighter-prover-s9hc64-merge-task"
merge_task_subscription   = "lighter-prover-s9hc64-merge-task-sub"
merge_result_topic        = "lighter-prover-s9hc64-merge-result"
merge_result_subscription = "lighter-prover-s9hc64-merge-result-sub"

pubsub_topic        = "lighter-prover-s9hc64-dispatch"
pubsub_subscription = "lighter-prover-s9hc64-dispatch-sub"
hpa_target_class    = "cells"
hpa_min_replicas    = 9  # steady = tier cell count
hpa_max_replicas    = 36 # burst ceiling
hpa_backlog_target  = 56 # ~k chunks per 500-tx block (ceil(500/9))

enable_workloads        = true
metrics_adapter_enabled = true
