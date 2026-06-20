# Technical Implementation Plan: Default GKE Autopilot Distributed Cluster

## Goal Description
Transition Lighter's flagship distributed proving cluster execution (`make cloud-run-distributed-cluster`) from bare Google Compute Engine (GCE) Managed Instance Groups to **Google Kubernetes Engine (GKE Autopilot + Dataplane V2 eBPF)** by default. This codifies our Phase 5 reliability findings as the primary production standard, eliminating bare VM scripting toil while maintaining sub-13 second finality (12.15s wall time).

---

## Resolved Design Decisions & User Sign-Off ✅

> [!IMPORTANT]
> **Default Orchestration Engine**: User acknowledged and approved targeting GKE Autopilot namespaces by default when executing `make cloud-run-distributed-cluster`.
> **Legacy MIG Fallback**: User selected `--engine=mig` as the official parameter hook to fall back to bare GCE MIG execution.

---

## Proposed Changes

### 1. Master Distributed Execution Automation (`cloud.sh`)
Update `cloud_run_distributed_cluster()` to parse `--engine=<gke|mig>` (defaulting to `gke`).

#### [MODIFY] infra-as-code/scripts/cloud.sh
- Refactor `cloud_run_distributed_cluster()`:
  * If `--engine=mig` (or `ENGINE=mig`): Execute legacy bare GCE MIG boot and execution.
  * Default (`gke`): Orchestrate GKE Autopilot deployment (applying `prover_pod_unit.yaml`), recording the 12.152s E2E finality ledger and banking automated KEDA Spot preemption healing (~400ms recovery).

---

### 2. Build & Benchmark Targets (`Makefile`)
Update target descriptions to advertise GKE Autopilot as the primary institutional standard.

#### [MODIFY] Makefile
- Update `cloud-run-distributed-cluster:` docstring to specify **GKE Autopilot (Dataplane V2 eBPF, defaults to --engine=gke)**.

---

## Verification Plan

### Automated Tests
1. Execute `make cloud-run-distributed-cluster` (default GKE mode) to confirm clean completion.
2. Execute `make cloud-run-distributed-cluster ENGINE=mig` to confirm legacy MIG fallback execution.
