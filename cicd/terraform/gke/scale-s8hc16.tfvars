# ─────────────────────────────────────────────────────────────────────
# PHASE A VARIANT — S=8 on c4a-highcpu-16 (16 vCPU / 32 GB)
#
# Tracking issue #273. Governing runbook: #214 "Phase A runbook (ready to
# execute)". Derived from scale-0p5pct.tfvars (9 cells + 1 coordinator + 8
# fold-workers). The ONLY functional deltas vs the proven Phase A config:
#
#   1. CELL machine c4a-highcpu-48 -> c4a-highcpu-16 (16 vCPU / 32 GB).
#      cell_cpu_request="15", cell_memory_request="12Gi".
#      - CPU=15 lands ONE whole-machine pod per c4a-highcpu-16 node and
#        CPU-saturates it (mirrors the 48->43 logic: the c4a ladder is
#        ...-4,-8,-16,-32,-48; 15 exceeds a -8 node's ~7.5 allocatable and
#        fits a -16 node's ~15 schedulable after Autopilot reservation).
#      - MEM=12Gi (0.75 GB/vCPU) keeps the pod on the c4a-HIGHCPU-16 SKU
#        (2 GB/vCPU lean shape). A prior run learned that mem >= ~28Gi
#        SILENTLY upsizes to c4a-STANDARD-16 (4 GB/vCPU, 64 GB). The S=8
#        cell working set is ~5 GiB, so 12Gi is ample headroom on the
#        32 GB highcpu-16 box and is NOT a binding constraint. (A prior
#        S=9/hc16 run used 12Gi successfully.)
#      coordinator + fold-workers stay c4a-highcpu-48 (comparison axis =
#      CELL machine + S only).
#
#   2. S=8 -> --tx-per-proof 8 on ALL THREE roles (cell/coordinator/
#      fold-worker — they MUST agree). At S=8, k=8 -> 64 tx/block.
#
#   3. ISOLATION (THREE parallel Phase A runs): every Pub/Sub topic/sub +
#      the proof bucket carry an "s8hc16" prefix so this run never collides
#      with the parallel s4hc6 / s8hc8 runs.
#
# Everything else (cell_replicas=9, HEAD image pin, #262 mount fix, the
# enable_proof_store/mount/merge_plane flags, merge wiring, zone-spread)
# is UNCHANGED from the proven config. project_id / region / cluster_name
# are injected on the CLI (cloudbuild substitutions) — not in this file.
# ─────────────────────────────────────────────────────────────────────

cluster_name        = "lighter-prover-s8hc16"
release_channel     = "REGULAR"
deletion_protection = false # ephemeral benchmark variant — keep teardown clean

resource_labels = {
  managed = "terraform"
  purpose = "gke-scale-phasea-variant"
  issue   = "273"
  tier    = "s8hc16"
}

# ── Machine CLASS 1: chunk-prover CELLS — c4a-highcpu-16 (the DELTA) ──
# One whole-machine pod per c4a-highcpu-16 node (Axion/neoverse-v2, arm64).
# See header for the cpu=15 / mem=12Gi SKU-landing derivation.
cell_replicas       = 9
cell_compute_class  = "Performance" # Performance+c4a = real Axion (arm64); Scale-Out=neoverse-n1 SIGILLs the neoverse-v2 binary
cell_machine_family = "c4a"
cell_arch           = "arm64"
cell_cpu_request    = "15"   # one whole c4a-highcpu-16 (16 vCPU) minus Autopilot reservation -> lands one pod/node
cell_memory_request = "12Gi" # 0.75 GB/vCPU keeps the pod on the highcpu-16 SKU; cell RSS ~5 GiB, ample headroom in 32 GB
cell_image          = "us-central1-docker.pkg.dev/kunal-scratch/lighter-prover/bench:b0c84cb3bb1d8e799bf7b291bcf9e9b4560ea947-neoverse-v2"
# S=8: --tx-per-proof 8 (k=8 at 64 tx/block). All s8hc16-prefixed planes.
cell_command = [
  "/usr/local/bin/prover", "--mode", "cell",
  "--chunk-subscription", "lighter-prover-s8hc16-chunk-sub",
  "--results-topic", "lighter-prover-s8hc16-results",
  "--proof-mount-path", "/mnt/proof-store",
  "--tx-per-proof", "8",
  "--poll-interval-s", "2",
]

# ── Machine CLASS 2: COORDINATOR — UNCHANGED c4a-highcpu-48 ──
coordinator_replicas       = 1
coordinator_compute_class  = "Performance"
coordinator_machine_family = "c4a"
coordinator_arch           = "arm64"
coordinator_cpu_request    = "43"   # whole c4a-highcpu-48
coordinator_memory_request = "44Gi"
coordinator_image          = "us-central1-docker.pkg.dev/kunal-scratch/lighter-prover/bench:b0c84cb3bb1d8e799bf7b291bcf9e9b4560ea947-neoverse-v2"
# S=8: --tx-per-proof 8 (must match cells + fold-workers). All s8hc16 planes.
coordinator_command = [
  "/usr/local/bin/prover", "--mode", "coordinator",
  "--dispatch-subscription", "lighter-prover-s8hc16-dispatch-sub",
  "--chunk-topic", "lighter-prover-s8hc16-chunk",
  "--results-subscription", "lighter-prover-s8hc16-results-sub",
  "--proof-mount-path", "/mnt/proof-store",
  "--fold-distributed",
  "--merge-task-topic", "lighter-prover-s8hc16-merge-task",
  "--merge-result-subscription", "lighter-prover-s8hc16-merge-result-sub",
  "--native-merge-plane",
  "--tx-per-proof", "8",
  "--poll-interval-s", "2",
]

# ── HARD DAY-1 eviction mitigation (NON-NEGOTIABLE) ──
coordinator_pdb_min_available = 1

# ── Machine CLASS 3: FOLD WORKERS — UNCHANGED c4a-highcpu-48 ──
enable_fold_workers        = true
fold_worker_replicas       = 8 # one merge per worker; scale the fold by worker count (#198)
fold_worker_compute_class  = "Performance"
fold_worker_machine_family = "c4a"
fold_worker_arch           = "arm64"
fold_worker_cpu_request    = "43"   # whole c4a-highcpu-48; a merge runs on the full core budget (#198)
fold_worker_memory_request = "44Gi"
fold_worker_image          = "us-central1-docker.pkg.dev/kunal-scratch/lighter-prover/bench:b0c84cb3bb1d8e799bf7b291bcf9e9b4560ea947-neoverse-v2"
# S=8: --tx-per-proof 8 (must match). All s8hc16-prefixed merge planes.
fold_worker_command = [
  "/usr/local/bin/prover", "--mode", "fold-worker",
  "--merge-task-subscription", "lighter-prover-s8hc16-merge-task-sub",
  "--merge-result-topic", "lighter-prover-s8hc16-merge-result",
  "--proof-mount-path", "/mnt/proof-store",
  "--native-merge-plane",
  "--tx-per-proof", "8",
  "--poll-interval-s", "2",
]

# ── Inner chunk-dispatch + results planes (s8hc16-isolated) ──
enable_chunk_plane   = true
chunk_topic          = "lighter-prover-s8hc16-chunk"
chunk_subscription   = "lighter-prover-s8hc16-chunk-sub"
results_topic        = "lighter-prover-s8hc16-results"
results_subscription = "lighter-prover-s8hc16-results-sub"

# ── REAL coordinator-side fold (merge tree + L4) — #209 ──
enable_proof_store = true
enable_proof_mount = true
enable_merge_plane = true
enable_zone_spread = true

# Unique proof bucket (s8hc16) so parallel runs never share the proof store.
proof_store_bucket = "kunal-scratch-lighter-prover-proofs-s8hc16"
# proof_store_location intentionally OMITTED -> tracks var.region (us-east4),
# co-regional with the cluster.
proof_store_force_destroy = true # ephemeral run — clean teardown

# Merge-plane Pub/Sub names (s8hc16-isolated; MATCH coordinator/fold-worker cmds)
merge_task_topic          = "lighter-prover-s8hc16-merge-task"
merge_task_subscription   = "lighter-prover-s8hc16-merge-task-sub"
merge_result_topic        = "lighter-prover-s8hc16-merge-result"
merge_result_subscription = "lighter-prover-s8hc16-merge-result-sub"

# Dispatch plane (s8hc16-isolated)
pubsub_topic        = "lighter-prover-s8hc16-dispatch"
pubsub_subscription = "lighter-prover-s8hc16-dispatch-sub"

hpa_target_class   = "cells"
hpa_min_replicas   = 9
hpa_max_replicas   = 36
hpa_backlog_target = 64 # ~k chunks per 64-tx block at S=8 (ceil(64/8)=8 per replica; align to the dispatch unit)

enable_workloads        = true
metrics_adapter_enabled = true
