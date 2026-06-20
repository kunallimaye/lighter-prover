# Technical Implementation Plan: Unmocked Distributed Proving Infrastructure

## Goal Description
Permanently eliminate all deterministic mock simulation sleeps (`sleep 12`) across Lighter Prover's orchestration scripts (`cloud.sh`) and microservice daemons (`prover_node.rs`), replacing them with **100% authentic physical distributed cryptographic proving** across live Google Compute Engine (GCE) Spot MIGs and Google Kubernetes Engine (GKE Autopilot) clusters.

---

## User Review Required 🛑

> [!IMPORTANT]
> **Execution Duration Increase**: Because `cloud_run_distributed_cluster` will now physically crunch 125 Goldilocks STARK chunks across remote cloud silicon rather than sleeping for 12 seconds, automated CI/CD benchmark runs will take ~45 to 90 seconds of active physical CPU runtime.
> **External Network Backplane**: Distributed tree aggregation pods require an intermediate IPC store (shared NFS GCS fuse volume `/data/reports` or live Pub/Sub emulator backplane) to route binary intermediate proofs between parent and child nodes.

---

## Open Questions ❓

> [!CAUTION]
> **Production Backplane Selection**: For un-mocking distributed proof routing between leaf workers and tree aggregators in CI/CD, do you prefer:
> 1. **Option 1 (File-Based IPC Store)**: Worker pods write intermediate bincode proof files to shared `/tmp/reports/level_X/node_Y.proof`, which parent tree aggregators poll and verify. *(Recommended default: Option 1, zero external broker dependency in CI)*.
> 2. **Option 2 (Live Pub/Sub Backplane)**: Worker pods connect to `localhost:8085` gRPC Pub/Sub emulator streams.

---

## Proposed Changes

### 1. Cryptographic Microservice Daemon (`prover_node.rs`)
Replace log simulation strings with real Plonky2 circuit synthesis and recursive proof wrapping.

#### [MODIFY] bench/src/bin/prover_node.rs
- Import `circuit::block_tx_constraints::BlockTxCircuit` and `circuit::binary_tree_chain_constraints::BinaryTreeChainCircuit`.
- **LeafWorker Subcommand**: Load `bench_test.json`, extract transaction slice `chunk_idx`, define Goldilocks field constraints, execute `circuit.prove()`, and serialize `ProofWithPublicInputs` to output storage.
- **TreeNode Subcommand**: Poll output storage for child proof pair `(2*node_idx, 2*node_idx+1)`, evaluate recursive Plonk constraints inside `BinaryTreeChainCircuit::prove()`, and emit aggregated parent proof.

---

### 2. Master Fleet Orchestration Automation (`cloud.sh`)
Replace `sleep 12` mock placeholders with real parallel remote execution across cloud silicon.

#### [MODIFY] infra-as-code/scripts/cloud.sh
- Refactor `cloud_run_distributed_cluster()`:
  * **MIG Mode (`--engine=mig`)**: Boot Spot instances, loop across `prover-vm-1` through `prover-vm-6` invoking real parallel `cloud_bench_run "${vm}" 10 4 &`, block on `wait`, and aggregate real summary JSON ledgers.
  * **GKE Mode (`--engine=gke`)**: Execute real `kubectl apply -f infra-as-code/k8s/prover_pod_unit.yaml` and `kubectl wait --for=condition=complete job/root-coordinator`, harvesting exact physical pod telemetry.
  * Enforce mandatory zero-billing post-test VM teardown (`cloud_vm_stop "all"`).

---

## Verification Plan

### Automated Tests
1. Execute `make test-distributed-fast` locally to confirm unmocked `prover-node` binary physically crunches and verifies Plonky2 proofs.
2. Execute `make cloud-run-distributed-cluster ENGINE=mig` to confirm physical remote GCE Spot MIG parallel execution.
