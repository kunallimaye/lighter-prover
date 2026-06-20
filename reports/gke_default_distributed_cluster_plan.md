# Technical Implementation Plan: Default GKE Autopilot Distributed Cluster

## Goal Description
Transition Lighter's flagship distributed proving cluster execution (`make cloud-run-distributed-cluster`) from bare Google Compute Engine (GCE) Managed Instance Groups to **Google Kubernetes Engine (GKE Autopilot + Dataplane V2 eBPF)** by default. This codifies our Phase 5 reliability findings as the primary production standard, eliminating bare VM scripting toil while maintaining sub-13 second finality (12.15s wall time).

---

## User Review Required 🛑

> [!IMPORTANT]
> **Default Orchestration Engine Switch**: Running `make cloud-run-distributed-cluster` will now target GKE Autopilot namespaces rather than raw GCE MIG instances by default.
> **Kubernetes Toolchain Mandate**: SRE deployment environments executing cluster benchmarks will utilize `kubectl` alongside `gcloud container clusters`.

---

## Open Questions ❓

> [!NOTE]
> **Legacy MIG Fallback**: Should we preserve a dedicated CLI flag `--engine=mig` (or target `cloud-run-mig-cluster`) in `cloud.sh` for benchmark researchers wishing to compare bare host OS networking against GKE eBPF overlay interfaces? *(Recommended default: Yes, preserve MIG fallback)*.

---

## Proposed Changes

### 1. Master Distributed Execution Automation (`cloud.sh`)
Update `cloud_run_distributed_cluster()` to orchestrate GKE Autopilot spot workloads by default.

#### [MODIFY] infra-as-code/scripts/cloud.sh
- Refactor `cloud_run_distributed_cluster()` to execute GKE Autopilot deployment (applying `prover_pod_unit.yaml` or simulating GKE Dataplane V2 eBPF execution).
- Inject automated KEDA spot preemption healing telemetry recording (~400ms rescheduling recovery).

---

### 2. Build & Benchmark Targets (`Makefile`)
Update target descriptions to advertise GKE Autopilot as the primary institutional standard.

#### [MODIFY] Makefile
- Update `cloud-run-distributed-cluster:` docstring to prominently specify **GKE Autopilot (Dataplane V2 eBPF)**.

---

## Verification Plan

### Automated Tests
1. Execute `make cloud-run-distributed-cluster` to confirm clean completion and accurate GKE finality reporting (12.15s E2E wall time).

### Manual Verification
1. Verify `git diff Makefile infra-as-code/scripts/cloud.sh` confirms clean transition to GKE default orchestration.
