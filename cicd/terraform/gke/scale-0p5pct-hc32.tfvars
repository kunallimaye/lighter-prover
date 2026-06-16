# ─────────────────────────────────────────────────────────────────────
# SCALE-0.5% config — Phase A VARIANT: S=9 on c4a-highcpu-32 (issue #269).
#
# A machine-class COMPARISON variant of scale-0p5pct.tfvars. The ONLY
# functional deltas vs the proven Phase A baseline (c4a-highcpu-48 @ S=9,
# recorded in #214) are:
#   1. CELL machine type → c4a-highcpu-32 (32 vCPU / 64 GB) instead of
#      c4a-highcpu-48 (48 vCPU / 96 GB). cell_cpu_request / cell_memory_request
#      re-derived for the 32-vCPU shape (one cell pod per node, CPU-saturating).
#   2. ALL Pub/Sub topic/sub names + the proof bucket carry a UNIQUE "hc32"
#      tier prefix so this run is fully ISOLATED from the parallel
#      c4a-highcpu-16 variant (which runs simultaneously) and the committed
#      scale-0p5pct default. cluster_name also set to lighter-prover-hc32.
#
# Coordinator + fold-worker machine types are DELIBERATELY UNCHANGED
# (c4a-highcpu-48) — the comparison axis is the CELL machine ONLY.
# S stays 9 (--tx-per-proof 9 on all roles); cell_replicas=9.
#
# NOT MERGED to main — reproducibility artifact only (S=9/c4a-highcpu-48
# remains the calibrated committed default). Refs #214 (governing runbook),
# #269 (this run), goal:G4.
# ─────────────────────────────────────────────────────────────────────

cluster_name        = "lighter-prover-hc32"
release_channel     = "REGULAR"
deletion_protection = false # scaled TEST config — keep teardown clean

resource_labels = {
  managed = "terraform"
  purpose = "gke-scale-ladder"
  issue   = "269"
  tier    = "0p5pct-hc32"
}

# ── Machine CLASS 1: chunk-prover CELLS (ADR-0006 §1.2) — c4a-highcpu-32 ──
# VARIANT: one CPU-saturating whole-machine pod per c4a-highcpu-32 node
# (32 vCPU / 64 GB, Axion/neoverse-v2, arm64). Derivation mirrors the
# baseline 48→43 logic: a 32-vCPU Autopilot node schedules ~30-31 allocatable
# vCPU after the system reservation, so request 30 vCPU → exactly one cell pod
# per c4a-highcpu-32 node, fully CPU-saturating. Memory request 30Gi is well
# above the measured S=9 cell RSS (~5.3 GiB peak) + L4/L5 keys + headroom and
# fits comfortably on the 64 GB box (one pod per node). compute_class /
# machine_family / arch UNCHANGED from baseline.
cell_replicas       = 9
cell_compute_class  = "Performance" # Autopilot: Performance+c4a = real Axion; Scale-Out=t2a/neoverse-n1 SIGILLs the neoverse-v2 binary (live finding)
cell_machine_family = "c4a"
cell_arch           = "arm64"
cell_cpu_request    = "30"   # whole c4a-highcpu-32 (32 vCPU) minus ~2 vCPU Autopilot reservation → one cell pod per 32-vCPU node, CPU-saturating
cell_memory_request = "30Gi" # measured cell RSS ~5.3 GiB at S=9 + L4/L5 keys + headroom; fits c4a-highcpu-32's 64 GB physical
cell_image = "us-central1-docker.pkg.dev/kunal-scratch/lighter-prover/bench:b0c84cb3bb1d8e799bf7b291bcf9e9b4560ea947-neoverse-v2"
cell_command = [
  "/usr/local/bin/prover", "--mode", "cell",
  "--chunk-subscription", "lighter-prover-hc32-chunk-sub",
  "--results-topic", "lighter-prover-hc32-results",
  "--proof-mount-path", "/mnt/proof-store",
  "--tx-per-proof", "9",
  "--poll-interval-s", "2",
]

# ── Machine CLASS 2: COORDINATORS (ADR-0006 §1.1, §2) — UNCHANGED c4a-highcpu-48 ──
# DELIBERATELY UNCHANGED from baseline (comparison axis = CELL machine only).
coordinator_replicas       = 1
coordinator_compute_class  = "Performance"
coordinator_machine_family = "c4a"
coordinator_arch           = "arm64"
coordinator_cpu_request    = "43"   # whole c4a-highcpu-48 (UNCHANGED)
coordinator_memory_request = "44Gi" # UNCHANGED
coordinator_image = "us-central1-docker.pkg.dev/kunal-scratch/lighter-prover/bench:b0c84cb3bb1d8e799bf7b291bcf9e9b4560ea947-neoverse-v2"
coordinator_command = [
  "/usr/local/bin/prover", "--mode", "coordinator",
  "--dispatch-subscription", "lighter-prover-hc32-dispatch-sub",
  "--chunk-topic", "lighter-prover-hc32-chunk",
  "--results-subscription", "lighter-prover-hc32-results-sub",
  "--proof-mount-path", "/mnt/proof-store",
  "--fold-distributed",
  "--merge-task-topic", "lighter-prover-hc32-merge-task",
  "--merge-result-subscription", "lighter-prover-hc32-merge-result-sub",
  "--native-merge-plane",
  "--tx-per-proof", "9",
  "--poll-interval-s", "2",
]

coordinator_pdb_min_available = 1

# ── Machine CLASS 3: FOLD WORKERS (issue #232) — UNCHANGED c4a-highcpu-48 ──
# DELIBERATELY UNCHANGED from baseline (comparison axis = CELL machine only).
enable_fold_workers        = true
fold_worker_replicas       = 8
fold_worker_compute_class  = "Performance"
fold_worker_machine_family = "c4a"
fold_worker_arch           = "arm64"
fold_worker_cpu_request    = "43"   # whole c4a-highcpu-48 (UNCHANGED)
fold_worker_memory_request = "44Gi" # UNCHANGED
fold_worker_image          = "us-central1-docker.pkg.dev/kunal-scratch/lighter-prover/bench:b0c84cb3bb1d8e799bf7b291bcf9e9b4560ea947-neoverse-v2"
fold_worker_command = [
  "/usr/local/bin/prover", "--mode", "fold-worker",
  "--merge-task-subscription", "lighter-prover-hc32-merge-task-sub",
  "--merge-result-topic", "lighter-prover-hc32-merge-result",
  "--proof-mount-path", "/mnt/proof-store",
  "--native-merge-plane",
  "--tx-per-proof", "9",
  "--poll-interval-s", "2",
]

# ── Inner chunk-dispatch + results planes (#172) — hc32-isolated names ──
enable_chunk_plane   = true
chunk_topic          = "lighter-prover-hc32-chunk"
chunk_subscription   = "lighter-prover-hc32-chunk-sub"
results_topic        = "lighter-prover-hc32-results"
results_subscription = "lighter-prover-hc32-results-sub"

# ── Issue #209: REAL coordinator-side fold (merge tree + L4) ──
enable_proof_store = true
enable_proof_mount = true
enable_merge_plane = true
enable_zone_spread = true

# hc32-isolated merge-plane names (match coordinator/fold-worker commands above)
merge_task_topic          = "lighter-prover-hc32-merge-task"
merge_task_subscription   = "lighter-prover-hc32-merge-task-sub"
merge_result_topic        = "lighter-prover-hc32-merge-result"
merge_result_subscription = "lighter-prover-hc32-merge-result-sub"

# hc32-isolated outer dispatch plane (feeder publishes to this topic)
pubsub_topic        = "lighter-prover-hc32-dispatch"
pubsub_subscription = "lighter-prover-hc32-dispatch-sub"

# ── ISOLATION: dedicated proof bucket (default is project-derived + SHARED
# across all runs → would COLLIDE with the parallel hc16 run and the
# baseline). Override with an hc32-unique, globally-unique bucket name so
# this run's L2 leaf proofs + merge artifacts never cross-feed another run.
proof_store_bucket = "kunal-scratch-lighter-prover-hc32-proofs"

hpa_target_class    = "cells"
hpa_min_replicas    = 9  # steady = tier cell count
hpa_max_replicas    = 36 # burst ceiling (unchanged from baseline)
hpa_backlog_target  = 56 # unchanged from baseline (S=9-tuned)

enable_workloads        = true
metrics_adapter_enabled = true
