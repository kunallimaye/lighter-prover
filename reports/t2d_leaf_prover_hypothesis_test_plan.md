# Technical Implementation Plan: Empirical AB Hypothesis Test (`t2d` vs `c4a`)

## Goal Description
Empirically validate the economic and performance hypothesis that substituting **AMD EPYC Milan Tau (`t2d-standard-60` @ $\$0.0042\text{/vCPU/hr}$)** for **ARM Neoverse V2 (`c4a-highcpu-64` @ $\$0.0111\text{/core/hr}$)** in the Leaf Prover role reduces Lighter's annual proving spot billings by **$\ge 58\%$ ($\sim \$1.38\text{M / yr}$ across 10 BPS)** while maintaining an acceptable **$\le 13.25\text{ second block settlement wall time}$** ($C=125$ chunks).

---

## User Review Required

> [!IMPORTANT]
> **Spot Quota Prerequisite**: To execute this comparative AB trial, your GCP project (`kunal-scratch`) requires active GCE Spot CPU Quota in `us-central1-a` for **180 AMD Milan vCPUs** (`t2d-standard-60`) and **208 ARM Neoverse CPU cores** (`c4a`). Total estimated spot cost for the 3-minute benchmark run is **$\le \$0.18\text{ total}$**.

---

## Open Questions

> [!CAUTION]
> **Compiler Target Flags**: For the AMD Milan (`x86_64`) container build, do your cryptographic engineers prefer compiling with generic `-C target-feature=+avx2,+bmi2,+adx` flags (maximum compatibility across AMD Milan and EPYC Rome) or strict `-C target-cpu=znver3` flags (optimized specifically for AMD Milan Zen 3 L1/L2 cache prefetchers)? *(Recommended default: `-C target-cpu=znver3` for peak Goldilocks throughput)*.

---

## Proposed Changes

### 1. Multi-Architecture Container CI/CD Pipeline
Enable dual-architecture compilation so the exact same container image runs natively across AMD EPYC Milan (`t2d`) and ARM Neoverse (`c4a`).

#### [MODIFY] infra-as-code/cloudbuild-zkp.yaml
- Inject `docker buildx create --use` and build against `--platform linux/amd64,linux/arm64` tagging `zkp-prover:multiarch`.

---

### 2. Infrastructure-as-Code AB Fleet Topology
Author isolated Terraform templates defining the head-to-head comparative proving arrays.

#### [NEW] infra-as-code/terraform/t2d_hypothesis_fleet.tf
```hcl
# Control Pod A (Flagship All-ARM Triad @ $2.314/hr)
resource "google_compute_instance" "control_leaf_provers" {
  count        = 3
  machine_type = "c4a-highcpu-64"
  zone         = "us-east4-b"
  # ... Spot JIT preemption bindings
}

# Hypothesis Pod B (Tau Milan Triad @ $0.934/hr)
resource "google_compute_instance" "hypothesis_leaf_provers" {
  count        = 3
  machine_type = "t2d-standard-60"
  zone         = "us-central1-a"
  # ... Spot JIT preemption bindings
}

# Shared Tree Aggregator Shape (c4a-highcpu-16)
resource "google_compute_instance" "shared_tree_nodes" {
  count        = 2 # 1 for Control A, 1 for Hypothesis B
  machine_type = "c4a-highcpu-16"
  # ...
}
```

---

### 3. Automated AB Race Orchestrator
Codify the benchmark execution script and Makefile simulation hooks.

#### [MODIFY] infra-as-code/scripts/cloud.sh
- Append function `cloud_test_t2d_hypothesis()` that:
  1. Boots Pod A (`c4a`) and Pod B (`t2d`) simultaneously.
  2. Dispatches 125 simultaneous `LeafWorker` microservice pods across both clusters over isolated Pub/Sub channels (`stark-proofs-control` vs `stark-proofs-hypothesis`).
  3. Records exact comparative settlement wall times ($W_A$ vs $W_B$) and effective spot burn rates.
  4. Immediately executes `cloud_vm_stop "all"` upon conclusion!

#### [MODIFY] Makefile
- Register `test-t2d-hypothesis:` target delegating strictly to `@bash infra-as-code/scripts/cloud.sh cloud-test-t2d-hypothesis`.

---

## Verification Plan

### Automated Tests
1. **Execute AB Trial**: Run `make test-t2d-hypothesis` via background task runner.
2. **Empirical Telemetry Assertion**:
   - Verify Control Pod A (`c4a`) settles Block #1042 in $\sim 12.00\text{s}$.
   - Verify Hypothesis Pod B (`t2d`) settles Block #1042 in $\le 13.25\text{s}$.
   - Verify Pod B spot burn rate reports $\le \$0.95\text{ / hr}$ (**$\ge 59\%$ Cost Lift!**).

### Manual Verification
1. Verify `gcloud compute instances list` reports $100\%$ of all AB test VMs in `STATUS: TERMINATED` upon completion.
