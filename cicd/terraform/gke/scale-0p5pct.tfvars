# ─────────────────────────────────────────────────────────────────────
# SCALE-0.5% config — 0.5% of the full-scale mainnet fleet.
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

cluster_name        = "lighter-prover-scale-0p5pct"
release_channel     = "REGULAR"
deletion_protection = false # scaled TEST config — keep teardown clean

resource_labels = {
  managed = "terraform"
  purpose = "gke-scale-ladder"
  issue   = "169"
  tier    = "0p5pct"
}

# ── Machine CLASS 1: chunk-prover CELLS (ADR-0006 §1.2) ──
# CPU-saturating whole-machine pods on c4a-highcpu-64 (Axion/neoverse-v2,
# arm64). One cell saturates a whole 64-vCPU box (ADR-0003). Scale-Out
# Autopilot class selects Axion (arm64) nodes.
cell_replicas       = 9
cell_compute_class  = "Performance" # Autopilot: Performance+c4a = real Axion; Scale-Out=t2a/neoverse-n1 SIGILLs the neoverse-v2 binary (live finding)
cell_machine_family = "c4a"
cell_arch           = "arm64"
cell_cpu_request    = "43"   # whole c4a-highcpu-64 minus Autopilot system reservation
cell_memory_request = "44Gi" # measured cell RSS 5.1 GiB (peak_rss_mb=5266) + L4/L5 keys + headroom
# DEPENDENCY: replace with the REAL arm64 prover image tag
# (<sha>-neoverse-v2) emitted by cicd/cloudbuild.yaml. Placeholder until built.
cell_image = "us-central1-docker.pkg.dev/PROJECT/lighter-prover/bench:SHA-neoverse-v2"
# Issue #209: the cell ships its REAL L2 leaf proof to the shared proof
# store (--proof-bucket / LIGHTER_PROOF_BUCKET) so the coordinator can
# DOWNLOAD it and run the REAL merge tree + L4. The bucket name follows the
# deterministic default in variables.tf (proof_store_bucket = "" =>
# "<project_id>-lighter-prover-proofs"). --proof-mount-path is the gcsfuse
# mount enable_proof_mount = true creates (issue #206 transport, faster
# than the gcloud-cp fallback). REPLACE "PROJECT" with the real project id
# at apply time (same placeholder convention as --project, above).
cell_command = [
  "/usr/local/bin/prover", "--mode", "cell",
  "--project", "PROJECT",
  "--chunk-subscription", "lighter-prover-scale-0p5pct-chunk-sub",
  "--results-topic", "lighter-prover-scale-0p5pct-results",
  "--proof-bucket", "PROJECT-lighter-prover-proofs",
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
# DEPENDENCY: same real arm64 prover image tag from cicd/cloudbuild.yaml.
coordinator_image = "us-central1-docker.pkg.dev/PROJECT/lighter-prover/bench:SHA-neoverse-v2"
# Issue #209: --proof-bucket / --proof-mount-path point the coordinator at
# the SAME bucket the cells uploaded their L2 leaf proofs to, so
# real_fold_enabled = TRUE and the coordinator runs the REAL merge tree +
# REAL BlockCircuit L4 (merge_source / l4_source = "measured"). Without
# --proof-bucket the coordinator would fall back to the accounting-only
# fold (no real merge, no real L4) — that path is now an explicit opt-in
# (--allow-accounting-only-fold), not the silent default. --fold-distributed
# selects the cross-machine FoldTopology::Distributed (issue #198: leader
# emits merge tasks, fold workers competing-pull) so the merge plane
# enable_merge_plane creates is actually used. REPLACE "PROJECT" with the
# real project id at apply time.
coordinator_command = [
  "/usr/local/bin/prover", "--mode", "coordinator",
  "--project", "PROJECT",
  "--dispatch-subscription", "lighter-prover-scale-0p5pct-dispatch-sub",
  "--chunk-topic", "lighter-prover-scale-0p5pct-chunk",
  "--results-subscription", "lighter-prover-scale-0p5pct-results-sub",
  "--proof-bucket", "PROJECT-lighter-prover-proofs",
  "--proof-mount-path", "/mnt/proof-store",
  "--fold-distributed",
  "--tx-per-proof", "9",
  "--poll-interval-s", "2",
]

# ── HARD DAY-1 mitigation (NON-NEGOTIABLE at EVERY scale) ──
# safe-to-evict=false + the PDB are hardwired in main.tf; this var only
# tunes the PDB threshold. With a single (floored) coordinator,
# minAvailable=1 pins it fully un-evictable so a bin-pack/eviction never
# takes an in-flight, key-resident coordinator.
coordinator_pdb_min_available = 1

# ── Autoscaling on Pub/Sub backlog ──
# ── Inner chunk-dispatch + results planes (#172, the real coordination) ──
enable_chunk_plane   = true
chunk_topic          = "lighter-prover-scale-0p5pct-chunk"
chunk_subscription   = "lighter-prover-scale-0p5pct-chunk-sub"
results_topic        = "lighter-prover-scale-0p5pct-results"
results_subscription = "lighter-prover-scale-0p5pct-results-sub"

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

pubsub_topic        = "lighter-prover-scale-0p5pct-dispatch"
pubsub_subscription = "lighter-prover-scale-0p5pct-dispatch-sub"
hpa_target_class    = "cells"
hpa_min_replicas    = 9  # steady = tier cell count
hpa_max_replicas    = 36 # burst ceiling: ceil(7006 * 0.005 = 35.030) = full-scale peak (41 blk/s) at 0.5%
hpa_backlog_target  = 56 # ~k chunks per 500-tx block (ceil(500/9)); one block's worth of backlog per replica

enable_workloads        = true
metrics_adapter_enabled = true
