# ─────────────────────────────────────────────────────────────────────
# SCALE-3% config — 3% of the full-scale mainnet fleet.
#
# Validation-ladder tier 2 of 3 (1% / 3% / 5%). Sizes are 3% of the
# full-scale steady fleet sized in docs/fleet-sizing-full-scale.md from the
# MEASURED #95 model (scripts/fleet-size.py) against the real G1 load (#128):
#
#   full-scale steady point = real mean 11.08 blocks/s, 500-tx cap, S=9,
#   c4a-highcpu-64 -> 1894 cells, 51 coordinators (BY CLASS, never summed).
#
#   3% => cells = ceil(1894 * 0.03) = 57
#         coordinators = ceil(51 * 0.03) = 2
#
# Matching synthetic load to drive (proportional): mean 0.3324 blk/s,
# p99 0.75, peak 1.23 blk/s. See docs/fleet-sizing-full-scale.md §4.1.
#
# Refs #95 #144 #75 #113. NO APPLY here — staged plan only (runbook §6).
# project_id / region are passed on the CLI (no secrets in this file).
# ─────────────────────────────────────────────────────────────────────

cluster_name        = "lighter-prover-scale-3pct"
release_channel     = "REGULAR"
deletion_protection = false # scaled TEST config — keep teardown clean

resource_labels = {
  managed = "terraform"
  purpose = "gke-scale-ladder"
  issue   = "169"
  tier    = "3pct"
}

# ── Machine CLASS 1: chunk-prover CELLS (ADR-0006 §1.2) ──
# CPU-saturating whole-machine pods on c4a-highcpu-64 (Axion/neoverse-v2,
# arm64). One cell saturates a whole 64-vCPU box (ADR-0003). Scale-Out
# Autopilot class selects Axion (arm64) nodes.
cell_replicas       = 57
cell_compute_class  = "Scale-Out"
cell_arch           = "arm64"
cell_cpu_request    = "62"    # whole c4a-highcpu-64 minus Autopilot system reservation
cell_memory_request = "16Gi"  # measured cell RSS 5.1 GiB (peak_rss_mb=5266) + L4/L5 keys + headroom
# DEPENDENCY: replace with the REAL arm64 prover image tag
# (<sha>-neoverse-v2) emitted by cicd/cloudbuild.yaml. Placeholder until built.
cell_image   = "us-central1-docker.pkg.dev/PROJECT/lighter-prover/bench:SHA-neoverse-v2"
cell_command = ["/usr/local/bin/prover", "--mode", "cell"]

# ── Machine CLASS 2: COORDINATORS (ADR-0006 §1.1, §2) — DISTINCT class ──
# Fold L2 merge tree + prove L4. Sized SEPARATELY, NEVER summed with cells.
# coord_service = merge_tree + L4 = 1.6506 + 2.928 = 4.579 s/block (k=56).
# concurrency = 1 (per-coordinator concurrency PROMISING-NOT-PROVEN, #113).
coordinator_replicas       = 2
coordinator_compute_class  = "Scale-Out"
coordinator_arch           = "arm64"
coordinator_cpu_request    = "62"    # whole box; coordinator-specific profile UNMODELED (#113) — proxy
coordinator_memory_request = "16Gi"  # resident L4/L5 keys; coordinator-specific RSS UNMODELED — worker proxy
# DEPENDENCY: same real arm64 prover image tag from cicd/cloudbuild.yaml.
coordinator_image   = "us-central1-docker.pkg.dev/PROJECT/lighter-prover/bench:SHA-neoverse-v2"
coordinator_command = ["/usr/local/bin/prover", "--mode", "coordinator"]

# ── HARD DAY-1 mitigation (NON-NEGOTIABLE at EVERY scale) ──
# safe-to-evict=false + the PDB are hardwired in main.tf; this var only
# tunes the PDB threshold. minAvailable = replicas - 1 keeps at least one
# coordinator un-disruptable while allowing one-at-a-time rolling headroom.
coordinator_pdb_min_available = 1

# ── Autoscaling on Pub/Sub backlog ──
pubsub_topic        = "lighter-prover-scale-3pct-dispatch"
pubsub_subscription = "lighter-prover-scale-3pct-dispatch-sub"
hpa_target_class    = "cells"
hpa_min_replicas    = 57  # steady = tier cell count
hpa_max_replicas    = 211 # burst ceiling: ceil(7006 * 0.03) = full-scale peak (41 blk/s) at 3%
hpa_backlog_target  = 56  # ~k chunks per 500-tx block (ceil(500/9)); one block's worth of backlog per replica

enable_workloads        = true
metrics_adapter_enabled = true
