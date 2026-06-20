# Technical Implementation Plan: Unmocked Distributed Proving Infrastructure

## Goal Description
Permanently eliminate all deterministic mock simulation sleeps (`sleep 12`) across Lighter Prover's orchestration scripts (`cloud.sh`) and microservice daemons (`prover_node.rs`), replacing them with **100% authentic physical distributed cryptographic proving** across live Google Compute Engine (GCE) Spot MIGs and Google Kubernetes Engine (GKE Autopilot) clusters managed declaratively via Terraform.

---

## Resolved Design Decisions & Institutional Corrections ✅

> [!IMPORTANT]
> **Cross-Host Proof Backplane (GCS IPC Store)**: Per user review (*“How can we use file-based if workers and aggregators are not on the same host?”*), local disk files cannot cross VM boundaries. We codify **Google Cloud Storage (GCS Object Store)** as the universal distributed proof IPC fabric (`gs://${BENCH_BUCKET}/proof_store/block_1042/`). Leaf workers stream serialized proof bytes directly to GCS, where parent reduction aggregators poll and dequeue them.
> **Declarative Terraform Mandate**: Per user review (*“This needs to be done via Terraform not kubectl”*), imperative `kubectl apply` shell scripts are prohibited. All GKE Autopilot namespaces, KEDA scalers, and distributed proving Kubernetes Jobs will be provisioned and managed strictly via **Terraform (`terraform apply -auto-approve`)** in `infra-as-code/terraform/`.

---

## Proposed Changes

### 1. Cryptographic Microservice Daemon (`prover_node.rs`)
Replace log simulation strings with real Plonky2 Goldilocks proof generation and GCS IPC storage transfer.

#### [MODIFY] bench/src/bin/prover_node.rs
- Import `circuit::block_tx_constraints::BlockTxCircuit` and `circuit::binary_tree_chain_constraints::BinaryTreeChainCircuit`.
- **LeafWorker Subcommand**: Load `bench_test.json`, synthesize Goldilocks witness chunk `chunk_idx`, execute `circuit.prove()`, and stream `ProofWithPublicInputs` bytes to GCS URI `gs://${BENCH_BUCKET}/proofs/leaf_${chunk_idx}.bin`.
- **TreeNode Subcommand**: Poll GCS URI for child proof pair `(2*I, 2*I+1)`, evaluate recursive Plonk constraints inside `BinaryTreeChainCircuit::prove()`, and upload parent proof to GCS.

---

### 2. Declarative IaC Orchestration (`cloud.sh` & `terraform/`)
Replace `sleep 12` mock placeholders with real Terraform execution across cloud silicon.

#### [MODIFY] infra-as-code/scripts/cloud.sh
- Refactor `cloud_run_distributed_cluster()`:
  * **GKE Mode (Default)**: Execute `terraform -chdir=infra-as-code/terraform apply -var="execution_mode=gke_distributed" -auto-approve` to declaratively spin up GKE Autopilot prover jobs, poll GCS object store for final finality assertion, and record exact real-world proof telemetry.
  * **MIG Mode (`--engine=mig`)**: Execute `terraform -chdir=infra-as-code/terraform apply -var="execution_mode=mig_distributed" -auto-approve` across remote GCE Spot fleet.
  * Mandatory zero-billing teardown: `terraform destroy -auto-approve`.

---

## Verification Plan

### Automated Tests
1. Execute `make test-distributed-fast` locally to confirm unmocked `prover-node` binary crunches real proofs.
2. Execute `make cloud-run-distributed-cluster` to confirm declarative Terraform GKE distributed execution.
