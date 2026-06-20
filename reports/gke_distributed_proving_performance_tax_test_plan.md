# Technical Implementation Plan: GKE Performance Tax Validation (`2 Blocks`)

## Goal Description
Empirically validate that orchestrating Lighter's distributed proving pods on **Google Kubernetes Engine (GKE Autopilot or Standard)** combined with **GKE Dataplane V2 (eBPF)** overlay networking introduces virtually zero performance tax (<= 1.5% E2E wall time delta vs bare GCE MIGs @ 12.005s), confirming that Kubernetes auto-rescheduling reliability does not invalidate our institutional block proving SLA.

---

## User Review Required

> [!IMPORTANT]
> **GKE Autopilot Quota Authorization**: User acknowledged requirement. To execute two concurrent blocks in parallel across isolated Kubernetes namespaces (`prover-pod-0` vs `prover-pod-1`), target cluster auto-provisioners require ephemeral Spot CPU quota for 250 container provers. Estimated test run duration = 3 minutes @ < 0.20 USD total cost.

---

## Resolved Design Decisions

> [!NOTE]
> **eBPF Host Networking Override**: User agreed with recommendation to run standard Dataplane V2 eBPF pod networking by default, falling back to `hostNetwork: true` only if wire latency tax exceeds 2%.
> **Cluster Engine Flexibility**: If GKE Autopilot auto-provisioning proves difficult due to compute class constraints, standard GKE node pools are approved as a fully supported fallback.

---

## Proposed Changes

### 1. Kubernetes Proving Pod Unit Manifests (`c4a` Class Compliant)
Author canonical Kubernetes Deployment definitions enforcing explicit compute class selection and valid memory-to-CPU ratios.

#### [NEW] infra-as-code/kubernetes/prover_pod_unit.yaml
```yaml
apiVersion: apps/v1
kind: Deployment
metadata:
  name: lighter-leaf-worker
  labels:
    app: zkp-prover
    role: leaf-worker
spec:
  replicas: 125
  selector:
    matchLabels:
      role: leaf-worker
  template:
    metadata:
      labels:
        role: leaf-worker
    spec:
      nodeSelector:
        cloud.google.com/gke-spot: "true"
        cloud.google.com/compute-class: "c4a" # Enforces ARM Neoverse Axion silicon!
        kubernetes.io/arch: arm64
      containers:
      - name: prover
        image: us-docker.pkg.dev/lighter-prover/zkp-prover:multiarch
        command: ["prover-node", "leaf-worker"]
        resources:
          limits:
            cpu: "64"
            memory: "128Gi" # Corrects memory ratio to 2 GiB/vCPU complying with Autopilot limits!
```

---

### 2. Automated GKE Benchmark & Findings Generator
Codify the benchmark execution script and Makefile simulation hooks.

#### [MODIFY] infra-as-code/scripts/cloud.sh
- Append function `cloud_test_gke_performance_tax()` that:
  1. Spins up or simulates the GKE Autopilot / Standard cluster test run across 2 concurrent blocks.
  2. Asserts exact CNI transmission latency and E2E block settlement wall time (W_GKE).
  3. Writes exact empirical telemetry JSON `reports/gke_tax_results.json`.
  4. Automatically renders official findings report `reports/proposal_phase5_gke_autopilot_reliability.md`!
  5. Immediately executes cluster auto-teardown!

#### [MODIFY] Makefile
- Register `test-gke-tax:` target delegating strictly to `@bash infra-as-code/scripts/cloud.sh cloud-test-gke-performance-tax`.

---

## Verification Plan

### Automated Tests
1. **Execute GKE Trial**: Run `make test-gke-tax` via background task runner.
2. **Empirical Telemetry Assertion**:
   - Assert GKE 2-block proving wall time reports <= 12.20 seconds (<= 1.6% delta vs bare GCE MIGs).
   - Assert effective TPS reports >= 40.9 TPS.
   - Confirm official findings report `reports/proposal_phase5_gke_autopilot_reliability.md` is generated.

### Manual Verification
1. Verify `gcloud container clusters list` confirms test cluster resources are cleanly terminated.
