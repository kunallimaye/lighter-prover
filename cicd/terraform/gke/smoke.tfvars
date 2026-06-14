# ─────────────────────────────────────────────────────────────────────
# SMOKE config — tiny counts, trivial no-op workloads.
#
# This is what the smoke-validation cycle ACTUALLY applies. It proves the
# AUTOMATION works (topology deploys, both classes come up, the eviction
# mitigation holds, the HPA reacts to a synthetic backlog) WITHOUT real
# proving load and WITHOUT production sizes. Real sizes are fed later via
# production.tfvars (gated on G2 + sizing #95) with NO structural change.
#
# project_id / region are passed on the command line / via Cloud Build
# substitutions so this file carries no environment secrets.
# ─────────────────────────────────────────────────────────────────────

cluster_name        = "lighter-prover-smoke"
release_channel     = "REGULAR"
deletion_protection = false

# ── Machine CLASS 1: chunk-prover cells (tiny + trivial) ──
# At smoke scale we use the default Autopilot class (amd64) + the pause
# image: we are validating the automation/topology, not building the
# arm64 prover. The c4a/Axion arm64 path is exercised in production.tfvars.
cell_replicas       = 1
cell_compute_class  = ""
cell_arch           = "amd64"
cell_cpu_request    = "250m"
cell_memory_request = "512Mi"
cell_image          = "registry.k8s.io/pause:3.10"

# ── Machine CLASS 2: coordinators (tiny, DISTINCT class) ──
# Uses a real sleeping container (busybox) rather than pause, so we can
# exercise drain/eviction against a pod that actually holds a process —
# the safe-to-evict=false + PDB mitigation is proven against it.
coordinator_replicas       = 1
coordinator_compute_class  = ""
coordinator_arch           = "amd64"
coordinator_cpu_request    = "250m"
coordinator_memory_request = "512Mi"
coordinator_image          = "busybox:1.36"
coordinator_command        = ["sh", "-c", "echo coordinator-stub up; sleep 100000"]

# ── HARD DAY-1 mitigation: keep the single coordinator un-evictable ──
coordinator_pdb_min_available = 1

# ── Autoscaling on Pub/Sub backlog ──
pubsub_topic        = "lighter-prover-smoke-dispatch"
pubsub_subscription = "lighter-prover-smoke-dispatch-sub"
hpa_target_class    = "cells"
hpa_min_replicas    = 1
hpa_max_replicas    = 5
# Small target so a hand-published backlog of ~30 messages drives
# desiredReplicas up (30 undelivered / 5 per replica ⇒ wants 6, capped at 5).
hpa_backlog_target = 5

enable_workloads        = true
metrics_adapter_enabled = true
