# ─────────────────────────────────────────────────────────────────────
# PHASE A VARIANT — S=8 on c4a-highcpu-8 (8 vCPU / 16 GB) — ISOLATED RUN
#
# Derived from scale-0p5pct.tfvars (the proven Phase A config). This is a
# machine-class/S sweep variant recorded on issue #214 as a labeled
# comparison run ("Phase A — S=8 on c4a-highcpu-8 (8 vCPU/16 GB)").
#
# DELTA vs the proven Phase A config (everything else identical):
#   1. CELL machine class → c4a-highcpu-8 (8 vCPU / 16 GB), down from
#      c4a-highcpu-64 (64 vCPU / 44 GB-class). Coordinator + fold-worker
#      machines are UNCHANGED (still c4a-highcpu-64 / 43 vCPU).
#   2. --tx-per-proof 8 on ALL THREE roles (S=8 instead of S=9), so a
#      k=8 block = 8 tx/proof × 8 chunks = 64 tx/block.
#   3. ISOLATION (THREE Phase A runs in parallel — must not collide):
#      - cluster_name        = lighter-prover-s8hc8 (unique)
#      - ALL Pub/Sub topics/subs prefixed lighter-prover-s8hc8-* (unique)
#      - proof_store_bucket  = kunal-scratch-lighter-prover-proofs-s8hc8
#        (unique — the default <project>-lighter-prover-proofs is SHARED;
#         a unique bucket keeps this run's L2 leaf proofs isolated)
#      - merge-plane names prefixed lighter-prover-s8hc8-merge-* (unique)
#      Separate TF state (GKE_TF_PREFIX=gke-scale-s8hc8) + dedicated
#      kubeconfig (/tmp/kubeconfig-s8hc8.yaml) are set out-of-band on the
#      CLI/env, not in this file.
#
# EXACT CELL SIZING (chosen for c4a-highcpu-8 = 8 vCPU / 16 GB):
#   cell_cpu_request    = "7"    → one whole-machine pod per 8-vCPU node;
#                                  ~1 vCPU left for the Autopilot system
#                                  reservation (mirrors 64→43 / 48→43 on
#                                  the big box: take the full machine minus
#                                  the node-daemon headroom).
#   cell_memory_request = "12Gi" → clears the measured prove peak RSS
#                                  (~5.2 GB) with comfortable headroom, and
#                                  stays ≤ ~2 GB/vCPU (12/7 = 1.71 GB/vCPU)
#                                  so Autopilot keeps the node on the
#                                  HIGHCPU SKU (a too-high mem request
#                                  silently upsizes to a standard SKU — a
#                                  prior-run finding). 16 GB box ⇒ 12Gi
#                                  request leaves ~4 GB for system + cache.
# ─────────────────────────────────────────────────────────────────────

cluster_name        = "lighter-prover-s8hc8"
release_channel     = "REGULAR"
deletion_protection = false # scaled TEST config — keep teardown clean

resource_labels = {
  managed = "terraform"
  purpose = "gke-scale-ladder"
  issue   = "214"
  tier    = "s8hc8"
  variant = "phasea-s8-c4a-highcpu-8" # GCP labels must be lowercase ([a-z0-9_-])
}

# ── Machine CLASS 1: chunk-prover CELLS — VARIANT: c4a-highcpu-8 ──
# DELTA: small 8-vCPU / 16-GB box (vs the proven highcpu-64). S=8 has
# comfortable RSS headroom here (prove peak ~5.2 GB vs ~12 GB schedulable).
# Performance + c4a = real Axion (arm64 / neoverse-v2); Scale-Out=t2a
# SIGILLs the neoverse-v2 binary (live finding — keep Performance).
cell_replicas       = 9
cell_compute_class  = "Performance"
cell_machine_family = "c4a"
cell_arch           = "arm64"
cell_cpu_request    = "7"    # whole c4a-highcpu-8 (8 vCPU) minus ~1 vCPU Autopilot system reservation
cell_memory_request = "12Gi" # clears ~5.2 GB prove peak; 12/7 = 1.71 GB/vCPU ≤ 2 GB/vCPU so it stays on the highcpu SKU (not upsized to standard)
cell_image = "us-central1-docker.pkg.dev/kunal-scratch/lighter-prover/bench:b0c84cb3bb1d8e799bf7b291bcf9e9b4560ea947-neoverse-v2"
cell_command = [
  "/usr/local/bin/prover", "--mode", "cell",
  "--chunk-subscription", "lighter-prover-s8hc8-chunk-sub",
  "--results-topic", "lighter-prover-s8hc8-results",
  "--proof-mount-path", "/mnt/proof-store",
  "--tx-per-proof", "8",
  "--poll-interval-s", "2",
]

# ── Machine CLASS 2: COORDINATORS — UNCHANGED (c4a-highcpu-64 / 43 vCPU) ──
coordinator_replicas       = 1
coordinator_compute_class  = "Performance"
coordinator_machine_family = "c4a"
coordinator_arch           = "arm64"
coordinator_cpu_request    = "43"
coordinator_memory_request = "44Gi"
coordinator_image = "us-central1-docker.pkg.dev/kunal-scratch/lighter-prover/bench:b0c84cb3bb1d8e799bf7b291bcf9e9b4560ea947-neoverse-v2"
coordinator_command = [
  "/usr/local/bin/prover", "--mode", "coordinator",
  "--dispatch-subscription", "lighter-prover-s8hc8-dispatch-sub",
  "--chunk-topic", "lighter-prover-s8hc8-chunk",
  "--results-subscription", "lighter-prover-s8hc8-results-sub",
  "--proof-mount-path", "/mnt/proof-store",
  "--fold-distributed",
  "--merge-task-topic", "lighter-prover-s8hc8-merge-task",
  "--merge-result-subscription", "lighter-prover-s8hc8-merge-result-sub",
  "--native-merge-plane",
  "--tx-per-proof", "8",
  "--poll-interval-s", "2",
]

coordinator_pdb_min_available = 1

# ── Machine CLASS 3: FOLD WORKERS — UNCHANGED (c4a-highcpu-64 / 43 vCPU) ──
enable_fold_workers        = true
fold_worker_replicas       = 8
fold_worker_compute_class  = "Performance"
fold_worker_machine_family = "c4a"
fold_worker_arch           = "arm64"
fold_worker_cpu_request    = "43"
fold_worker_memory_request = "44Gi"
fold_worker_image          = "us-central1-docker.pkg.dev/kunal-scratch/lighter-prover/bench:b0c84cb3bb1d8e799bf7b291bcf9e9b4560ea947-neoverse-v2"
fold_worker_command = [
  "/usr/local/bin/prover", "--mode", "fold-worker",
  "--merge-task-subscription", "lighter-prover-s8hc8-merge-task-sub",
  "--merge-result-topic", "lighter-prover-s8hc8-merge-result",
  "--proof-mount-path", "/mnt/proof-store",
  "--native-merge-plane",
  "--tx-per-proof", "8",
  "--poll-interval-s", "2",
]

# ── Inner chunk-dispatch + results planes — s8hc8-prefixed (isolated) ──
enable_chunk_plane   = true
chunk_topic          = "lighter-prover-s8hc8-chunk"
chunk_subscription   = "lighter-prover-s8hc8-chunk-sub"
results_topic        = "lighter-prover-s8hc8-results"
results_subscription = "lighter-prover-s8hc8-results-sub"

# ── REAL coordinator-side fold (merge tree + L4) — same proven flags ──
enable_proof_store = true
enable_proof_mount = true
enable_merge_plane = true
enable_zone_spread = true

# ── UNIQUE proof-store bucket (isolated — default is SHARED across runs) ──
# The default derived name (<project>-lighter-prover-proofs) is the SAME for
# all three parallel runs. Override with an s8hc8-suffixed name so this run's
# L2 leaf proofs + intermediate merge proofs never collide with the other two.
proof_store_bucket = "kunal-scratch-lighter-prover-proofs-s8hc8"

# ── merge-plane Pub/Sub names — s8hc8-prefixed (isolated) ──
merge_task_topic          = "lighter-prover-s8hc8-merge-task"
merge_task_subscription   = "lighter-prover-s8hc8-merge-task-sub"
merge_result_topic        = "lighter-prover-s8hc8-merge-result"
merge_result_subscription = "lighter-prover-s8hc8-merge-result-sub"

# ── Outer dispatch plane — s8hc8-prefixed (isolated) ──
pubsub_topic        = "lighter-prover-s8hc8-dispatch"
pubsub_subscription = "lighter-prover-s8hc8-dispatch-sub"
hpa_target_class    = "cells"
hpa_min_replicas    = 9
hpa_max_replicas    = 36
hpa_backlog_target  = 56

enable_workloads        = true
metrics_adapter_enabled = true
