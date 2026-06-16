# ─────────────────────────────────────────────────────────────────────
# SCALE-0.5% config — c4a-highcpu-16 CELL machine-class VARIANT (issue #270).
#
# This is a one-axis VARIANT of scale-0p5pct.tfvars for the Phase A
# machine-class comparison (#214 runbook). EVERYTHING is identical to the
# proven S=9 Phase A config EXCEPT:
#   (1) the CELL machine type is changed c4a-highcpu-48 → c4a-highcpu-16
#       (cell_cpu_request 43 → 15, cell_memory_request 44Gi → 12Gi). The
#       coordinator + fold-worker machine types stay c4a-highcpu-48 (the
#       comparison axis is the CELL machine ONLY). S stays 9, k stays 8.
#   (2) ALL tier-prefixed names use a UNIQUE `hc16` prefix (cluster_name,
#       all Pub/Sub topics/subs, and a dedicated proof_store_bucket) so this
#       run NEVER collides with the parallel c4a-highcpu-32 Phase A variant
#       sharing the same GCP project. The TF state prefix is isolated via the
#       GKE_TF_PREFIX env knob at apply time (lighter-prover/gke-scale-hc16).
#
# cell_cpu_request DERIVATION for c4a-highcpu-16 (mirrors the 48→43 logic in
# docs/live-benchmark-results.md FINDING B): Autopilot bin-packs the pod onto
# the smallest c4a-highcpu SKU whose allocatable CPU ≥ the request. The c4a
# ladder is …-4, -8, -16, -32, -48… A request of 15 exceeds a -8 node's
# allocatable (~7.5 vCPU) and fits within a -16 node's allocatable (16 vCPU
# minus ~0.5-1 vCPU Autopilot system reservation ≈ ~15 schedulable), so the
# pod lands one-per-node on a c4a-highcpu-16 and CPU-saturates it — exactly
# how 43 lands one pod per c4a-highcpu-48. memory 12Gi keeps the pod on the
# LEAN-MEMORY highcpu SKU (2 GB/vCPU); a too-large request bumps Autopilot to
# the higher-RAM c4a-standard-16 (4 GB/vCPU) — see the cell_memory_request
# calibration note below for the attempt-1 finding.
#
# Validation-ladder tier 3 of 3 (0.2% / 0.3% / 0.5%). Sizes are 0.5% of
# the full-scale steady fleet sized in docs/fleet-sizing-full-scale.md from
# the MEASURED #95 model (scripts/fleet-size.py) against the real G1 load
# (#128):
#
#   full-scale steady point = real mean 11.08 blocks/s, 500-tx cap, S=9,
#   c4a-highcpu-64 -> 1894 cells, 51 coordinators (BY CLASS, never summed).
#
#   0.5% => cells        = round(1894 * 0.005) = round(9.470) = 9
#           coordinators = floor-at-1(51 * 0.005 = 0.255) = 1  (see below)
#
#   NOTE on cell rounding: 9.470 rounds to 9 under round-half-up
#   (0.470 < 0.5). Not 10.
#
# COORDINATOR FLOOR — DELIBERATE, STATED DESIGN DECISION:
#   The strict coordinator count at 0.5% is 0.255 (< 1). You cannot run a
#   zero-coordinator fold service, AND the NON-NEGOTIABLE eviction
#   mitigation (safe-to-evict=false + PodDisruptionBudget, minAvailable>=1)
#   requires at least one coordinator to protect. The coordinator class is
#   therefore NOT scaled below 1 by design — it is intentionally FLAT at 1
#   across all three ladder tiers (operational floor + the mandatory
#   eviction mitigation). This is a stated decision, not an oversight.
#
# Matching synthetic load to drive (proportional): mean 0.05540 blk/s,
# p99 0.125, peak 0.205 blk/s. See docs/fleet-sizing-full-scale.md §4.1.
#
# Refs #95 #144 #75 #113. NO APPLY here — staged plan only (runbook §6).
# project_id / region are passed on the CLI (no secrets in this file).
# ─────────────────────────────────────────────────────────────────────

cluster_name        = "lighter-prover-hc16"
release_channel     = "REGULAR"
deletion_protection = false # scaled TEST config — keep teardown clean

resource_labels = {
  managed = "terraform"
  purpose = "gke-scale-ladder"
  issue   = "270"
  tier    = "hc16"
}

# ── Machine CLASS 1: chunk-prover CELLS (ADR-0006 §1.2) ──
# VARIANT (#270): CPU-saturating whole-machine pods on c4a-highcpu-16
# (Axion/neoverse-v2, arm64) instead of the baseline c4a-highcpu-48. One cell
# saturates a whole 16-vCPU box. Same Performance+c4a Autopilot class (real
# Axion arm64). cell_cpu_request=15 / cell_memory_request=28Gi pin the pod to
# a c4a-highcpu-16 node (derivation in the header comment, mirroring the
# 48→43 FINDING B logic). cell_replicas stays 9 (same fleet width as baseline).
cell_replicas       = 9
cell_compute_class  = "Performance" # Autopilot: Performance+c4a = real Axion; Scale-Out=t2a/neoverse-n1 SIGILLs the neoverse-v2 binary (live finding)
cell_machine_family = "c4a"
cell_arch           = "arm64"
cell_cpu_request    = "15"   # whole c4a-highcpu-16 minus Autopilot system reservation (mirrors 48→43; lands one pod per 16-vCPU node)
# MEMORY-REQUEST CALIBRATION (attempt-1 finding, #270): a first apply with
# cell_memory_request=28Gi landed the cells on c4a-STANDARD-16 (16 vCPU / 64 GB)
# instead of c4a-HIGHCPU-16 (16 vCPU / 32 GB) — 28Gi exceeded the highcpu-16's
# post-reservation allocatable memory, so Autopilot upgraded to the higher-RAM
# standard SKU to satisfy the request (the highcpu family is the lean-memory
# 2 GB/vCPU shape; standard is 4 GB/vCPU). Since the measured S=9 cell RSS is
# only ~5.3 GiB, request 12Gi (0.8 GB/vCPU) — well above the workload peak with
# headroom, comfortably within highcpu-16's allocatable, so Autopilot keeps the
# pod on a c4a-HIGHCPU-16 node (one pod per node, CPU-saturating).
cell_memory_request = "12Gi" # leans onto c4a-highcpu-16 (32 GB / 2-GB-per-vCPU SKU); >2x the measured ~5.3 GiB S=9 cell RSS
# Issue #216: a REAL arm64 (neoverse-v2/Axion) bench image that already EXISTS
# in Artifact Registry — no build needed, just this pinned SHA. Re-pin to a
# newer cicd/cloudbuild.yaml output as the bench binary advances.
cell_image = "us-central1-docker.pkg.dev/kunal-scratch/lighter-prover/bench:b0c84cb3bb1d8e799bf7b291bcf9e9b4560ea947-neoverse-v2"
# Issue #209: the cell ships its REAL L2 leaf proof to the shared proof store
# so the coordinator can DOWNLOAD it and run the REAL merge tree + L4.
# Issue #216: --project and --proof-bucket are NO LONGER literal placeholders
# here — terraform injects LIGHTER_PROJECT (= var.project_id) and
# LIGHTER_PROOF_BUCKET (= the resolved proof-store bucket name) as env vars the
# bench binary reads (main.tf local.prover_wiring_env). So this command needs
# no per-arg PROJECT/bucket hand-editing. --proof-mount-path is the gcsfuse
# mount enable_proof_mount = true creates (issue #206 transport, faster than
# the gcloud-cp fallback).
cell_command = [
  "/usr/local/bin/prover", "--mode", "cell",
  "--chunk-subscription", "lighter-prover-hc16-chunk-sub",
  "--results-topic", "lighter-prover-hc16-results",
  "--proof-mount-path", "/mnt/proof-store",
  "--tx-per-proof", "9",
  "--poll-interval-s", "2",
]

# ── Machine CLASS 2: COORDINATORS (ADR-0006 §1.1, §2) — DISTINCT class ──
# Fold L2 merge tree + prove L4. Sized SEPARATELY, NEVER summed with cells.
# coord_service = merge_tree + L4 = 1.6506 + 2.928 = 4.579 s/block (k=56).
# concurrency = 1 (per-coordinator concurrency PROMISING-NOT-PROVEN, #113).
# FLOORED AT 1 BY DESIGN: strict count 51*0.005 = 0.255 < 1; the class is
# NOT scaled below 1 (operational floor + the mandatory eviction
# mitigation). Intentionally FLAT at 1 across all three tiers.
coordinator_replicas       = 1
coordinator_compute_class  = "Performance"
coordinator_machine_family = "c4a"
coordinator_arch           = "arm64"
coordinator_cpu_request    = "43"   # whole box; coordinator-specific profile UNMODELED (#113) — proxy
coordinator_memory_request = "44Gi" # resident L4/L5 keys; coordinator-specific RSS UNMODELED — worker proxy
# Issue #216: same REAL arm64 (neoverse-v2) bench image as the cells.
coordinator_image = "us-central1-docker.pkg.dev/kunal-scratch/lighter-prover/bench:b0c84cb3bb1d8e799bf7b291bcf9e9b4560ea947-neoverse-v2"
# Issue #209: --proof-bucket / --proof-mount-path point the coordinator at
# the SAME bucket the cells uploaded their L2 leaf proofs to, so
# real_fold_enabled = TRUE and the coordinator runs the REAL merge tree +
# REAL BlockCircuit L4 (merge_source / l4_source = "measured"). Without
# --proof-bucket the coordinator would fall back to the accounting-only
# fold (no real merge, no real L4) — that path is now an explicit opt-in
# (--allow-accounting-only-fold), not the silent default. --fold-distributed
# selects the cross-machine FoldTopology::Distributed (issue #198: leader
# emits merge tasks, fold workers competing-pull) so the merge plane
# enable_merge_plane creates is actually used.
# Issue #216: --project and --proof-bucket are injected as LIGHTER_PROJECT /
# LIGHTER_PROOF_BUCKET env vars by terraform (main.tf local.prover_wiring_env)
# from var.project_id + the resolved bucket name — no literal placeholders.
#
# Issue #232: --fold-distributed makes the leader PUBLISH merge tasks; for the
# fold-worker pool (added below) to pull them, the leader and workers must
# agree on the merge-plane names. The leader is wired with the merge-TASK topic
# it publishes to + the merge-RESULT subscription it barriers on; the fold
# workers (fold_worker_command) take the matching merge-task SUBSCRIPTION +
# merge-result TOPIC.
# Issue #233: these merge names are now TIER-PREFIXED (lighter-prover-scale-
# 0p5pct-merge-*), exactly like the chunk/results planes, instead of the generic
# enable_merge_plane defaults — so two scale tiers running concurrently get
# tier-isolated merge planes and never collide. The merge_* variable overrides
# below (next to enable_merge_plane) provision Pub/Sub names that MATCH these
# flag values.
coordinator_command = [
  "/usr/local/bin/prover", "--mode", "coordinator",
  "--dispatch-subscription", "lighter-prover-hc16-dispatch-sub",
  "--chunk-topic", "lighter-prover-hc16-chunk",
  "--results-subscription", "lighter-prover-hc16-results-sub",
  "--proof-mount-path", "/mnt/proof-store",
  "--fold-distributed",
  "--merge-task-topic", "lighter-prover-hc16-merge-task",
  "--merge-result-subscription", "lighter-prover-hc16-merge-result-sub",
  "--native-merge-plane",
  "--tx-per-proof", "9",
  "--poll-interval-s", "2",
]

# ── HARD DAY-1 mitigation (NON-NEGOTIABLE at EVERY scale) ──
# safe-to-evict=false + the PDB are hardwired in main.tf; this var only
# tunes the PDB threshold. With a single (floored) coordinator,
# minAvailable=1 pins it fully un-evictable so a bin-pack/eviction never
# takes an in-flight, key-resident coordinator.
coordinator_pdb_min_available = 1

# ── Machine CLASS 3: FOLD WORKERS (issue #232 — the consumer of #198) ──
# The coordinator above runs --fold-distributed, so the leader publishes one
# merge task per merge pair. WITHOUT a worker pool to competing-pull them, the
# per-level barrier times out and the run fails. The #198 governing principle
# is "one merge per worker, scale by worker count" — so the lever is
# fold_worker_replicas, NOT a bigger box. A handful of workers lets a depth-6
# block's wide level-1 actually fan out. Sized SEPARATELY from cells +
# coordinators (ADR-0006: never summed). Same full-core c4a/Axion shape + REAL
# bench image as the coordinator, run in --mode fold-worker. NO safe-to-evict/
# PDB by design: an evicted mid-merge task is redelivered (the #198
# at-least-once contract).
enable_fold_workers        = true
fold_worker_replicas       = 8 # scales with the tier (cells=9); scale the fold by worker count (#198)
fold_worker_compute_class  = "Performance"
fold_worker_machine_family = "c4a"
fold_worker_arch           = "arm64"
fold_worker_cpu_request    = "43"   # whole c4a-highcpu-64 minus Autopilot reservation; a merge runs on the FULL core budget (#198)
fold_worker_memory_request = "44Gi" # resident merge proving key + headroom (worker RSS proxy)
fold_worker_image          = "us-central1-docker.pkg.dev/kunal-scratch/lighter-prover/bench:b0c84cb3bb1d8e799bf7b291bcf9e9b4560ea947-neoverse-v2"
# The worker competing-pulls the merge-task subscription, proves ONE merge,
# transits the output through the gcsfuse proof mount, and reports on the
# merge-result topic. --project/--proof-bucket are injected as LIGHTER_PROJECT/
# LIGHTER_PROOF_BUCKET env vars by terraform. --native-merge-plane selects the
# native manual-ack streaming-pull client (#205). Names match the leader above.
fold_worker_command = [
  "/usr/local/bin/prover", "--mode", "fold-worker",
  "--merge-task-subscription", "lighter-prover-hc16-merge-task-sub",
  "--merge-result-topic", "lighter-prover-hc16-merge-result",
  "--proof-mount-path", "/mnt/proof-store",
  "--native-merge-plane",
  "--tx-per-proof", "9",
  "--poll-interval-s", "2",
]

# ── Autoscaling on Pub/Sub backlog ──
# ── Inner chunk-dispatch + results planes (#172, the real coordination) ──
enable_chunk_plane   = true
chunk_topic          = "lighter-prover-hc16-chunk"
chunk_subscription   = "lighter-prover-hc16-chunk-sub"
results_topic        = "lighter-prover-hc16-results"
results_subscription = "lighter-prover-hc16-results-sub"

# ── Issue #209: REAL coordinator-side fold (merge tree + L4) ──
# Before #209 the committed scale tfvars set only `enable_chunk_plane = true`
# and the coordinator silently ran the "accounting-only fold" (no real
# merge, no real L4 — merge_source/l4_source = "modeled", merge_ms/l4_ms
# = 0). A terraform apply therefore measured only HALF the pipeline. The
# three flags below + the --proof-bucket/--proof-mount-path/--fold-distributed
# args in {cell,coordinator}_command above flip the coordinator into the
# REAL merge + L4 path BY DEFAULT (the #209 acceptance criterion).
#
#  - enable_proof_store: provisions the shared GCS bucket cells upload L2
#    leaf proofs to and the coordinator downloads them from (issue #179).
#  - enable_proof_mount: gcsfuse-mounts that bucket into the coordinator
#    pod at /mnt/proof-store so storage.rs uses file I/O instead of the
#    slower `gcloud storage cp` subprocess transport (issue #206).
#  - enable_merge_plane: provisions the merge-task / merge-result Pub/Sub
#    pairs the distributed leader+workers transit per-pair fold tasks over
#    (issue #198 cross-machine fold fan-out), matched by --fold-distributed
#    in coordinator_command above.
enable_proof_store = true
enable_proof_mount = true
enable_merge_plane = true
# VARIANT (#270) — PARALLEL-RUN ISOLATION: the proof-store bucket name
# defaults to "<project_id>-lighter-prover-proofs" (variables.tf:371-375), a
# PROJECT-derived name — NOT tier-derived. Two concurrent Phase A variants in
# the SAME project (kunal-scratch) would therefore share ONE proof bucket and
# cross-feed each other's L2 leaf proofs (a hard collision). Pin a UNIQUE
# bucket so the hc16 run and the parallel hc32 run never share proof storage.
# proof_store_force_destroy defaults true (variables.tf:414-418) so teardown
# removes this bucket + its objects cleanly.
proof_store_bucket = "kunal-scratch-lighter-prover-proofs-hc16"
# Issue #235: spread pods across zones so a single-zone c4a (Axion) stockout
# doesn't strand this multi-node tier. FINDING C: c4a stocked out across ALL
# us-central1 zones; us-east4 confirmed working. ScheduleAnyway (in main.tf) so
# a real N-1-zone stockout never blocks scheduling — spread preferred, not forced.
enable_zone_spread = true
# Issue #233: tier-prefix the provisioned merge-plane Pub/Sub names (like the
# chunk/results overrides above) so the PROVISIONED topic/sub names MATCH the
# tier-prefixed flag values in coordinator_command/fold_worker_command, and so
# concurrent scale tiers get tier-isolated merge planes.
merge_task_topic          = "lighter-prover-hc16-merge-task"
merge_task_subscription   = "lighter-prover-hc16-merge-task-sub"
merge_result_topic        = "lighter-prover-hc16-merge-result"
merge_result_subscription = "lighter-prover-hc16-merge-result-sub"

pubsub_topic        = "lighter-prover-hc16-dispatch"
pubsub_subscription = "lighter-prover-hc16-dispatch-sub"
hpa_target_class    = "cells"
hpa_min_replicas    = 9  # steady = tier cell count
hpa_max_replicas    = 36 # burst ceiling: ceil(7006 * 0.005 = 35.030) = full-scale peak (41 blk/s) at 0.5%
hpa_backlog_target  = 56 # ~k chunks per 500-tx block (ceil(500/9)); one block's worth of backlog per replica

enable_workloads        = true
metrics_adapter_enabled = true
