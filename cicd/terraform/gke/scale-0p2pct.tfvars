# ─────────────────────────────────────────────────────────────────────
# SCALE-0.2% config — 0.2% of the full-scale mainnet fleet.
#
# Validation-ladder tier 1 of 3 (0.2% / 0.3% / 0.5%). Sizes are 0.2% of
# the full-scale steady fleet sized in docs/fleet-sizing-full-scale.md from
# the MEASURED #95 model (scripts/fleet-size.py) against the real G1 load
# (#128):
#
#   full-scale steady point = real mean 11.08 blocks/s, 500-tx cap, S=9,
#   c4a-highcpu-64 -> 1894 cells, 51 coordinators (BY CLASS, never summed).
#
#   0.2% => cells        = round(1894 * 0.002) = round(3.788) = 4
#           coordinators = floor-at-1(51 * 0.002 = 0.102) = 1  (see below)
#
# COORDINATOR FLOOR — DELIBERATE, STATED DESIGN DECISION:
#   The strict coordinator count at 0.2% is 0.102 (< 1). You cannot run a
#   zero-coordinator fold service, AND the NON-NEGOTIABLE eviction
#   mitigation (safe-to-evict=false + PodDisruptionBudget, minAvailable>=1)
#   requires at least one coordinator to protect. The coordinator class is
#   therefore NOT scaled below 1 by design — it is intentionally FLAT at 1
#   across all three ladder tiers (operational floor + the mandatory
#   eviction mitigation). This is a stated decision, not an oversight.
#
# Matching synthetic load to drive (proportional): mean 0.02216 blk/s,
# p99 0.050, peak 0.082 blk/s. See docs/fleet-sizing-full-scale.md §4.1.
#
# Refs #95 #144 #75 #113. NO APPLY here — staged plan only (runbook §6).
# project_id / region are passed on the CLI (no secrets in this file).
# ─────────────────────────────────────────────────────────────────────

cluster_name        = "lighter-prover-scale-0p2pct"
release_channel     = "REGULAR"
deletion_protection = false # scaled TEST config — keep teardown clean

resource_labels = {
  managed = "terraform"
  purpose = "gke-scale-ladder"
  issue   = "169"
  tier    = "0p2pct"
}

# ── Machine CLASS 1: chunk-prover CELLS (ADR-0006 §1.2) ──
# CPU-saturating whole-machine pods on c4a-highcpu-64 (Axion/neoverse-v2,
# arm64). One cell saturates a whole 64-vCPU box (ADR-0003). Scale-Out
# Autopilot class selects Axion (arm64) nodes.
cell_replicas       = 4
cell_compute_class  = "Scale-Out"
cell_arch           = "arm64"
cell_cpu_request    = "62"    # whole c4a-highcpu-64 minus Autopilot system reservation
cell_memory_request = "16Gi"  # measured cell RSS 5.1 GiB (peak_rss_mb=5266) + L4/L5 keys + headroom
# DEPENDENCY: replace with the REAL arm64 prover image tag
# (<sha>-neoverse-v2) emitted by cicd/cloudbuild.yaml. Placeholder until built.
cell_image = "us-central1-docker.pkg.dev/PROJECT/lighter-prover/bench:SHA-neoverse-v2"
# /usr/local/bin/prover is the bench binary, symlinked in cicd/Containerfile
# (#172). The cell pulls chunk refs and publishes results over the inner
# planes; the Pub/Sub config is passed as flags (the binary also accepts the
# equivalent LIGHTER_* env vars). Replace PROJECT with the real project id.
cell_command = [
  "/usr/local/bin/prover", "--mode", "cell",
  "--project", "PROJECT",
  "--chunk-subscription", "lighter-prover-scale-0p2pct-chunk-sub",
  "--results-topic", "lighter-prover-scale-0p2pct-results",
  "--poll-interval-s", "2",
]

# ── Machine CLASS 2: COORDINATORS (ADR-0006 §1.1, §2) — DISTINCT class ──
# Fold L2 merge tree + prove L4. Sized SEPARATELY, NEVER summed with cells.
# coord_service = merge_tree + L4 = 1.6506 + 2.928 = 4.579 s/block (k=56).
# concurrency = 1 (per-coordinator concurrency PROMISING-NOT-PROVEN, #113).
# FLOORED AT 1 BY DESIGN: strict count 51*0.002 = 0.102 < 1; the class is
# NOT scaled below 1 (operational floor + the mandatory eviction
# mitigation). Intentionally FLAT at 1 across all three tiers.
coordinator_replicas       = 1
coordinator_compute_class  = "Scale-Out"
coordinator_arch           = "arm64"
coordinator_cpu_request    = "62"    # whole box; coordinator-specific profile UNMODELED (#113) — proxy
coordinator_memory_request = "16Gi"  # resident L4/L5 keys; coordinator-specific RSS UNMODELED — worker proxy
# DEPENDENCY: same real arm64 prover image tag from cicd/cloudbuild.yaml.
coordinator_image = "us-central1-docker.pkg.dev/PROJECT/lighter-prover/bench:SHA-neoverse-v2"
# The coordinator pulls blocks from the dispatch subscription, fans chunk refs
# to the chunk topic, and gathers results from the results subscription (#172).
coordinator_command = [
  "/usr/local/bin/prover", "--mode", "coordinator",
  "--project", "PROJECT",
  "--dispatch-subscription", "lighter-prover-scale-0p2pct-dispatch-sub",
  "--chunk-topic", "lighter-prover-scale-0p2pct-chunk",
  "--results-subscription", "lighter-prover-scale-0p2pct-results-sub",
  "--poll-interval-s", "2",
]

# ── HARD DAY-1 mitigation (NON-NEGOTIABLE at EVERY scale) ──
# safe-to-evict=false + the PDB are hardwired in main.tf; this var only
# tunes the PDB threshold. With a single (floored) coordinator,
# minAvailable=1 pins it fully un-evictable so a bin-pack/eviction never
# takes an in-flight, key-resident coordinator.
coordinator_pdb_min_available = 1

# ── Inner chunk-dispatch + results planes (#172, the real coordination) ──
# Beyond the outer block-dispatch backlog signal, the distributed run needs
# the chunk plane (coordinator -> cells) and the results plane (cells ->
# coordinator). enable_chunk_plane creates both topic/subscription pairs.
enable_chunk_plane   = true
chunk_topic          = "lighter-prover-scale-0p2pct-chunk"
chunk_subscription   = "lighter-prover-scale-0p2pct-chunk-sub"
results_topic        = "lighter-prover-scale-0p2pct-results"
results_subscription = "lighter-prover-scale-0p2pct-results-sub"

# ── Autoscaling on Pub/Sub backlog ──
pubsub_topic        = "lighter-prover-scale-0p2pct-dispatch"
pubsub_subscription = "lighter-prover-scale-0p2pct-dispatch-sub"
hpa_target_class    = "cells"
hpa_min_replicas    = 4  # steady = tier cell count
hpa_max_replicas    = 15 # burst ceiling: ceil(7006 * 0.002 = 14.012) = full-scale peak (41 blk/s) at 0.2%
hpa_backlog_target  = 56 # ~k chunks per 500-tx block (ceil(500/9)); one block's worth of backlog per replica

enable_workloads        = true
metrics_adapter_enabled = true
