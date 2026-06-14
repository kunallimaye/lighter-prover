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
cell_command = [
  "/usr/local/bin/prover", "--mode", "cell",
  "--project", "PROJECT",
  "--chunk-subscription", "lighter-prover-scale-0p5pct-chunk-sub",
  "--results-topic", "lighter-prover-scale-0p5pct-results",
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
coordinator_command = [
  "/usr/local/bin/prover", "--mode", "coordinator",
  "--project", "PROJECT",
  "--dispatch-subscription", "lighter-prover-scale-0p5pct-dispatch-sub",
  "--chunk-topic", "lighter-prover-scale-0p5pct-chunk",
  "--results-subscription", "lighter-prover-scale-0p5pct-results-sub",
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

pubsub_topic        = "lighter-prover-scale-0p5pct-dispatch"
pubsub_subscription = "lighter-prover-scale-0p5pct-dispatch-sub"
hpa_target_class    = "cells"
hpa_min_replicas    = 9  # steady = tier cell count
hpa_max_replicas    = 36 # burst ceiling: ceil(7006 * 0.005 = 35.030) = full-scale peak (41 blk/s) at 0.5%
hpa_backlog_target  = 56 # ~k chunks per 500-tx block (ceil(500/9)); one block's worth of backlog per replica

enable_workloads        = true
metrics_adapter_enabled = true
