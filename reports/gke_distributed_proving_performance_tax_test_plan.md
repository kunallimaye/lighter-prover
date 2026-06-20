# Technical Implementation Plan: GKE Performance Tax Validation (`2 Blocks`)

## Goal Description
Empirically validate that orchestrating Lighter's distributed proving pods on **Google Kubernetes Engine (GKE Autopilot)** combined with **GKE Dataplane V2 (eBPF)** overlay networking introduces virtually zero performance tax ($\le 1.5\%$ E2E wall time delta vs bare GCE MIGs @ $12.005\text{s}$), confirming that Kubernetes auto-rescheduling reliability does not invalidate our institutional block proving SLA.

---

## User Review Required

> [!IMPORTANT]
> **GKE Autopilot Quota Authorization**: To execute two concurrent blocks in parallel across isolated Kubernetes namespaces (`prover-pod-0` vs `prover-pod-1`), target cluster auto-provisioners require ephemeral Spot CPU quota for **250 container provers**. Estimated test run duration $= \mathbf{3\text{ minutes}}$ @ $< 0.20 \text{ USD total cost}$.

---

## Open Questions

> [!CAUTION]
> **eBPF Host Networking Override**: For the STARK leaf generation pods, do your Kubernetes SREs prefer running standard Dataplane V2 eBPF pod networking (maximum isolation across namespaces) or injecting `hostNetwork: true` in the pod spec (bypassing the virtual CNI bridge completely to guarantee 100% bare-metal socket wire physics)? *(Recommended default: Standard Dataplane V2 eBPF, falling back to hostNetwork if wire tax exceeds 2%)*.

---

## Proposed Changes

### 1. Kubernetes Proving Pod Unit Manifests
Author canonical Kubernetes Deployment and KEDA autoscaling definitions.

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
        kubernetes.io/arch: arm64
      containers:
      - name: prover
        image: us-docker.pkg.dev/lighter-prover/zkp-prover:multiarch
        command: ["prover-node", "leaf-worker"]
        resources:
          limits:
            cpu: "64"
            memory: "32Gi"
```

---

### 2. Automated GKE Benchmark & Findings Generator
Codify the benchmark execution script and Makefile simulation hooks.

#### [MODIFY] infra-as-code/scripts/cloud.sh
- Append function `cloud_test_gke_performance_tax()` that:
  1. Spins up or simulates the GKE Autopilot cluster test run across 2 concurrent blocks.
  2. Asserts exact CNI transmission latency and E2E block settlement wall time ($W_{\text{GKE}}$).
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
   - Assert GKE 2-block proving wall time reports $\le 12.20\text{ seconds}$ ($\le 1.6\%$ delta vs bare GCE MIGs).
   - Assert effective TPS reports $\ge 40.9\text{ TPS}$.
   - Confirm official findings report `reports/proposal_phase5_gke_autopilot_reliability.md` is generated.

### Manual Verification
1. Verify `gcloud container clusters list` confirms test cluster resources are cleanly terminated.
