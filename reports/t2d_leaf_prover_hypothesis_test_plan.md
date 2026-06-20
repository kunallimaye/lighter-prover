# Technical Implementation Plan: 4-Pod Parallel AB Hypothesis Test (`t2d` vs `c4a`)

## Goal Description
Empirically validate the economic and multi-block concurrency hypothesis that substituting **AMD EPYC Milan Tau (`t2d-standard-60` @ $\$0.0042\text{/vCPU/hr}$)** for **ARM Neoverse V2 (`c4a-highcpu-64` @ $\$0.0111\text{/core/hr}$)** in the Leaf Prover role reduces Lighter's corporate proving spot billings by **$\ge 59\%$ ($\sim \$1.38\text{M / yr}$ across 10 BPS)** while maintaining **$\le 13.25\text{ second block wall times}$** during concurrent multi-block execution.

---

## User Review Required

> [!IMPORTANT]
> **Relocated Regional Quota (`us-east4`)**: Per user directive, the entire 4-pod AB trial array is consolidated in **`us-east4`**. To execute two parallel blocks per paradigm simultaneously ($4\text{ pods total}$), your GCP project (`kunal-scratch`) requires active GCE Spot CPU Quota in `us-east4` for **$360\text{ AMD Milan vCPUs}$** ($6 \times \text{t2d-standard-60}$) and **$448\text{ ARM Neoverse cores}$** ($6 \times \text{c4a-64} + 4 \times \text{c4a-16}$). Total spot test run cost $= \mathbf{\le \$0.35\text{ total}}$.

---

## Resolved Design Decisions

> [!NOTE]
> **Compiler Target Flags**: User approved locking Rust compiler target flags strictly to `-C target-cpu=znver3` for AMD Milan x86_64 container builds to ensure peak Goldilocks L1/L2 cache prefetch performance.

---

## Proposed Changes

### 1. Multi-Architecture CI/CD Pipeline (`znver3` Optimized)
Enable dual-platform compilation so `zkp-prover:multiarch` runs natively across x86_64 AMD Milan and aarch64 ARM Neoverse.

#### [MODIFY] infra-as-code/cloudbuild-zkp.yaml
- Inject `docker buildx create --use` and build against `--platform linux/amd64,linux/arm64` with `RUSTFLAGS="-C target-cpu=znver3"` for amd64 targets.

---

### 2. Infrastructure-as-Code 4-Pod Concurrent AB Fleet
Author isolated Terraform templates defining the 4 concurrent proving pods settling 4 blocks simultaneously in `us-east4`.

#### [NEW] infra-as-code/terraform/t2d_hypothesis_fleet.tf
```hcl
# Control Cluster (2 * c4a Pods proving Block 1042 & 1043 concurrently)
resource "google_compute_instance" "control_leaf_provers" {
  count        = 6 # 3 VMs per pod * 2 pods
  machine_type = "c4a-highcpu-64"
  zone         = "us-east4-b"
  # ... Spot JIT preemption bindings
}

# Hypothesis Cluster (2 * t2d Pods proving Block 1044 & 1045 concurrently)
resource "google_compute_instance" "hypothesis_leaf_provers" {
  count        = 6 # 3 VMs per pod * 2 pods
  machine_type = "t2d-standard-60"
  zone         = "us-east4-c"
  # ... Spot JIT preemption bindings
}

# Sharded Tree Aggregator Array (4 * c4a-highcpu-16)
resource "google_compute_instance" "shared_tree_nodes" {
  count        = 4 # 1 tree node VM per proving pod
  machine_type = "c4a-highcpu-16"
  zone         = "us-east4-b"
}
```

---

### 3. Automated 4-Pod AB Race & Findings Generator
Orchestrate concurrent multi-block proving, harvest telemetry findings, and compile official Phase 4 proposal report.

#### [MODIFY] infra-as-code/scripts/cloud.sh
- Append function `cloud_test_t2d_hypothesis()` that:
  1. Boots all 16 Spot VMs simultaneously in `us-east4`.
  2. Fires 2 simultaneous 500-tx blocks into Control Pods $P_0, P_1$ (`c4a`) AND 2 simultaneous 500-tx blocks into Hypothesis Pods $P_2, P_3$ (`t2d`).
  3. Harvests exact empirical telemetry JSONs into `reports/t2d_hypothesis_results.json`.
  4. Automatically renders official executive findings proposal report `reports/proposal_phase4_t2d_milan_leaf_arbitrage.md`!
  5. Instantly issues `cloud_vm_stop "all"` upon conclusion!

#### [MODIFY] Makefile
- Register `test-t2d-hypothesis:` target.

---

## Verification Plan

### Automated Tests
1. **Execute 4-Pod Race**: Run `make test-t2d-hypothesis` via background runner.
2. **Empirical Telemetry Assertion**:
   - Assert Control Pods $P_0, P_1$ settle Blocks 1042 & 1043 in $\sim 12.00\text{s}$.
   - Assert Hypothesis Pods $P_2, P_3$ settle Blocks 1044 & 1045 in $\le 13.25\text{s}$ in parallel.
   - Confirm official findings report `reports/proposal_phase4_t2d_milan_leaf_arbitrage.md` is generated.

### Manual Verification
1. Verify `gcloud compute instances list` confirms $100\%$ of all 16 test VMs transition to `STATUS: TERMINATED`.
