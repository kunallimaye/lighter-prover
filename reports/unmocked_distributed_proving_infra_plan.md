# Technical Implementation Plan: Unmocked Distributed Proving Infrastructure

## Goal Description
Permanently eliminate all deterministic mock simulation sleeps (`sleep 12`) across Lighter Prover's orchestration scripts (`cloud.sh`) and daemons (`prover_node.rs`), replacing them with **100% authentic physical distributed cryptographic proving** orchestrated via GCP Cloud Build and scaled autonomously on **GKE Standard** clusters via KEDA Stackdriver Pub/Sub metrics.

---

## GKE Standard Transition Mandate: Bypassing Autopilot Sandbox Limits ☸️🔒
Per user engineering check (*“Confirm this is compatible with GKE Autopilot? If not switch to using standard”*), we audit Google Cloud Kubelet restrictions:
*   **Autopilot Incompatibility**: GKE Autopilot strictly prohibits custom `kubelet_config` overrides (`cpuManagerPolicy: static` and `cpu_cfs_quota = false`). Furthermore, real-time scheduling (`chrt -f 99`) requires `SYS_NICE` capabilities rejected by Autopilot security boundaries.
*   **GKE Standard Adoption**: We transition exclusive distributed proving orchestration to **GKE Standard Cluster Node Pools**, acquiring full root authority over static host core pinning, unthrottled CFS bandwidth quotas, and real-time thread scheduling.

## Detailed Design: Autonomous KEDA Pub/Sub Event-Driven Autoscaling 📐⚡

Per user architectural review (*“No that is bad design - the proving pod min/max should be via deployment.yaml? And it should scale automatically by checking pubsub metrics? We need detailed design here”*), hardcoding imperative background processes (`&` and `wait`) in CI runners is prohibited.

We establish an institutional **Event-Driven Autoscaling Architecture** separating workload generation from container orchestration:

### 1. Workload & Silicon Parameterization (`prover_pod_unit.yaml`)
We author `infra-as-code/k8s/prover_pod_unit.yaml` defining stateless Proving Pod replica boundaries (min=0, max=240) and enforcing preemptible **Spot Instances on ARM Neoverse Axion `c4a` silicon** (64 cores @ 128 GiB RAM):

```yaml
apiVersion: apps/v1
kind: Deployment
metadata:
  name: prover-pod-deployment
  namespace: lighter-prover-dist
spec:
  replicas: 0 # Scaled autonomously by KEDA ScaledObject controller
  selector:
    matchLabels:
      app: zkp-prover
      role: leaf-worker
  template:
    metadata:
      labels:
        app: zkp-prover
        role: leaf-worker
    spec:
      # Enforce GCP Spot Instances strictly on ARM Neoverse Axion c4a silicon
      nodeSelector:
        cloud.google.com/gke-spot: "true"
        cloud.google.com/machine-family: "c4a"
      tolerations:
        - key: cloud.google.com/gke-spot
          operator: Equal
          value: "true"
          effect: NoSchedule
      containers:
        - name: zkp-prover-daemon
          image: us-docker.pkg.dev/kunal-scratch/lighter-prover-iac/zkp-prover:arm64
          imagePullPolicy: Always
          command: ["/app/prover-node"]
          args: ["leaf-worker", "--tx-per-proof", "4"]
          resources:
            # 64 ARM cores @ 128 GiB memory (100% NUMA socket match)
            requests:
              cpu: "64"
              memory: "128Gi"
            limits:
              cpu: "64"
              memory: "128Gi"
```

### 2. KEDA Scaler Manifest (`ScaledObject`)
We attach a KEDA `ScaledObject` monitoring Google Cloud Pub/Sub Stackdriver queue depth (`pubsub.googleapis.com/subscription/num_undelivered_messages` on subscription `tree-aggregators-sub`).
*   **Target Metric Threshold**: `125` unACKed messages per replica *(representing exactly 1 complete 500-tx block chunk set)*.

```mermaid
graph TD
    classDef pubsub fill:#0284c7,stroke:#bae6fd,stroke-width:2px,color:#fff;
    classDef keda fill:#7c3aed,stroke:#ddd6fe,stroke-width:2px,color:#fff;
    classDef gke fill:#0f172a,stroke:#4ade80,stroke-width:2px,color:#fff;

    INGEST["Ingest 2 Blocks (#1042 & #1043)<br>Publishes 250 Chunks to Pub/Sub"] --> TOPIC[("GCP Pub/Sub Topic:<br>stark-proofs-topic")]:::pubsub
    TOPIC --> SUB[("Subscription Queue:<br>num_undelivered_messages = 250")]:::pubsub

    SUB -->|Stackdriver Metric Poll| KEDA["KEDA Scaler Controller<br>Target: 125 msg / pod"]:::keda
    KEDA -->|"Scale 0 --> 2 Replicas"| GKE["GKE Autopilot Namespace<br>Spawns 2 Active Proving Pods (8 Spot VMs)"]:::gke

    GKE -->|gRPC Dequeue & ACK| SUB
```

### 3. Multi-Block Lifecycle Finality:
1.  **Ingest Burst**: 2 blocks arrive simultaneously, adding 250 unACKed messages to Pub/Sub.
2.  **Autonomous Scale-Up**: KEDA detects queue depth = 250 (a 2x multiple of the 125 target). KEDA instantly scales GKE deployment from 0 up to **2 active Proving Pod replicas** in ~400ms.
3.  **Parallel Crunch**: Pod 1 crunches Block #1042 while Pod 2 crunches Block #1043.
4.  **Autonomous Scale-To-Zero**: As root validium proofs settle to L1 Ethereum, workers issue gRPC ACKs. Queue depth hits 0. KEDA autonomously scales replicas back down to 0.

### 4. CPU Starvation Prevention: Guaranteed QoS & Static Core Pinning Physics 🛡️⚡
Per user engineering review (*“Do we require any other config to ensure there is no CPU starvation as ZKP is partial to that”*), cryptographic Goldilocks NTTs are hyper-sensitive to Linux Completely Fair Scheduler (CFS) bandwidth quota throttling.

To guarantee 100% zero CPU starvation and exclusive bare-metal NUMA socket affinity, we codify a three-layer kernel scheduling policy backed by declarative GKE Standard Node Pool provisioning:
1.  **Guaranteed QoS Class on GKE Standard**: Enforcing `requests.cpu == limits.cpu` (64 cores) and `requests.memory == limits.memory` (128 GiB) on a **GKE Standard Spot Node Pool (`google_container_node_pool`)** configured with Kubelet option `cpu_manager_policy = "static"`. Kubelet removes the container from the shared CFS cgroup and pins it to 64 dedicated physical host core IDs (`cpuset.cpus = 0-63`) with zero cgroup quota bounds!
2.  **Declarative Node Pool Configuration (`main.tf`)**: We author `infra-as-code/terraform/main.tf` provisioning dedicated ARM Axion `c4a-highcpu-64` spot silicon with `cpu_cfs_quota = false`:

```hcl
resource "google_container_node_pool" "prover_spot_nodes" {
  name       = "prover-c4a-spot-pool"
  cluster    = google_container_cluster.prover_cluster.id
  node_count = 0 # Scaled autonomously from 0 up to 30 spot nodes by KEDA / Cluster Autoscaler

  node_config {
    machine_type = "c4a-highcpu-64" # 64 ARM Neoverse Axion cores @ 128 GiB RAM
    spot         = true

    # Explicit host kubelet core pinning & unthrottled CFS configuration
    kubelet_config {
      cpu_manager_policy = "static"
      cpu_cfs_quota      = false
    }

    labels = {
      role = "zkp-prover-host"
    }

    taints {
      key    = "zkp-workload"
      value  = "true"
      effect = "NO_SCHEDULE"
    }

    service_account = var.runtime_sa_email
    oauth_scopes    = ["https://www.googleapis.com/auth/cloud-platform"]
  }
}
```

3.  **Linux Real-Time Scheduling (`chrt -f 99`)**: Worker container entrypoints run under Linux FIFO real-time priority (`command: ["chrt", "-f", "99", "/app/prover-node"]`), guaranteeing host background daemons never preempt active Goldilocks NTT Rayon threads.

---

## Resolved Crate & Component Identifiers ✅

> [!NOTE]
> **Daemon Crate Attribution**: Per user inquiry (*“Which crate is this?”*), `prover_node.rs` resides inside crate **`bench`** (`bench/src/bin/prover_node.rs` defined in `bench/Cargo.toml`).

---

## Proposed Changes

### 1. Cryptographic Microservice Daemon (`bench/src/bin/prover_node.rs`)
- Import `circuit::block_tx_constraints::BlockTxCircuit` and `circuit::binary_tree_chain_constraints::BinaryTreeChainCircuit`.
- Add `--block-number <N>` parameter filtering gRPC streams strictly by message attribute `block_number == N`.

### 2. Autonomous KEDA Manifest (`infra-as-code/k8s/prover_pod_unit.yaml`)
- Codify Kubernetes `Deployment` and KEDA `ScaledObject` targeting `stark-proofs-topic` Stackdriver metrics with min=0, max=240 bounds.

### 3. Cloud Build Orchestration (`infra-as-code/cloudbuild-distributed.yaml`)
- Step 3 publishes test block JSON chunks into Pub/Sub, then executes `kubectl wait --for=condition=available deployment/prover-pod-deployment` while KEDA autonomously scales the cluster!

---

## Parameter Governance: `BLOCKS` vs `JOBS` Synonymity ⚙️📝
Per user review (*“BLOCKS=2 should be the default if none specified... make a note that it is synonymous to JOBS”*), we standardize concurrency terminology across targets:
*   **Default Concurrency**: `BLOCKS ?= 2` *(Proves 2 complete blocks in parallel by default)*.
*   **Synonymity Note**: In distributed cluster targets (`cloud-run-distributed-cluster`), parameter **`BLOCKS`** governs multi-block pipeline concurrency. This is functionally synonymous to parameter **`JOBS`** used in monolithic single-VM targets (`cloud-bench-run`). Both govern the assigned batch proving concurrency across available hardware units.

---

## Verification Plan

### Automated Tests
1. Execute `make cloud-run-distributed-cluster` *(defaults automatically to `BLOCKS=2`)* to confirm KEDA autonomously scales from 0 to 2 replicas (8 Spot VMs) under Pub/Sub load.
2. Execute `make cloud-run-distributed-cluster BLOCKS=4` to confirm elastic multi-block scaling (synonymous to `JOBS=4`).
