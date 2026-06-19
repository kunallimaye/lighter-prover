---
name: distributed_proving_topology_plan
description: Deep research, architectural trade-off analysis, and experimental design for splitting proof generation layers across distributed workers, containers, and VMs.
---

# Research & Implementation Plan - Distributed Layer-Split Proving Architecture

## Goal Description
Currently, Lighter Prover executes the entire vertical proving stack (Layers 1 through 4) inside individual monolithic daemons or VM containers (`bench.rs`). While Phase 2 pipelining overlaps leaf generation with recursive aggregation, running both massive circuits on shared CPU cores induces heavy L2/L3 cache eviction thrashing.

This document presents deep architectural research, formal trade-off analysis (Pros vs. Cons), and experimental validation designs for **Splitting Proving Layers across Distributed Workers, Cells, Containers, and VMs** over a high-speed networking fabric.

---

## 1. Architectural Blueprint: Distributed Layer-Split Topology

```mermaid
graph TD
    classDef t1 fill:#0f172a,stroke:#38bdf8,stroke-width:2px,color:#fff;
    classDef t2 fill:#0f172a,stroke:#4ade80,stroke-width:2px,color:#fff;
    classDef t3 fill:#0f172a,stroke:#facc15,stroke-width:2px,color:#fff;
    classDef bus fill:#1e293b,stroke:#94a3b8,stroke-width:2px,color:#fff;

    A[Ingest Block: 500 Txs] --> L12["Layer 1 & 2 Pod: Scalar State Sequencer<br>(c4-highcpu-2 @ 3.8 GHz x86_64)<br>Executes BlockPreExec & Witness Batching"]:::t1

    L12 -->|125 x 4.1 KB PartialWitness Jobs| BROKER[("High-Speed Backplane Fabric<br>Redis Stream / gRPC / PubSub / NATS")]:::bus

    subgraph Elastic Stateless Leaf Fleet (Layer 3 Workers)
    BROKER --> W1["Leaf Worker 1 (c4a-72 Spot / GPU)"]:::t2
    BROKER --> W2["Leaf Worker 2 (c4a-72 Spot / GPU)"]:::t2
    BROKER --> WN["Leaf Worker N (c4a-72 Spot / GPU)"]:::t2
    end

    W1 & W2 & WN -->|125 x 150 KB STARK Proofs| STORE[("Proof Artifact Store<br>Redis / GCS / Memcached")]:::bus

    subgraph Dedicated Aggregation Tree (Layer 4 Consumer)
    STORE --> AG["Layer 4 Pod: Recursive Chain Aggregator<br>(m4-highmem Dedicated NUMA Node)"]:::t3
    AG --> FINAL["Verifiable Block Rollup Proof"]
    end
```

### Byte-Precision Network Bandwidth Physics 📡

1.  **Layer 1 & 2 $\rightarrow$ Layer 3 (`PartialWitness` Ingest)**:
    *   In Plonky2, `PartialWitness` stores wire assignments for public input targets (`num_public_inputs_tx = 520`).
    *   $520 \text{ GoldilocksField elements} \times 8 \text{ bytes} = \mathbf{4,160 \text{ bytes}}$ ($\sim 4.1 \text{ KB}$) per leaf chunk.
    *   Transmitting all 125 chunks for a 500-tx block equals **$512 \text{ KB total payload}$**. Over a 100 Gbps GCP VPC network, transmission takes **$0.04 \text{ milliseconds}$** (zero network drag).
2.  **Layer 3 $\rightarrow$ Layer 4 (`ProofWithPublicInputs` Output)**:
    *   A Goldilocks STARK proof at FRI blowup factor 8 consumes $\sim \mathbf{150 \text{ KB}}$.
    *   Transmitting 125 completed leaf proofs equals **$18.75 \text{ MB total payload}$** ($\sim 1.5 \text{ ms}$ over VPC).

---

## 2. Deep Trade-off Analysis (Pros vs. Cons) ⚖️

### 🟢 Advantages & Silicon Gains
1.  **Absolute Cache Isolation**: Leaf provers dedicate 100% of L2/L3 cache lines strictly to NTT Goldilocks FFTs. Zero cache line invalidation or Rayon thread thrashing occurs from competing recursive circuits.
2.  **Heterogeneous Hardware Optimization**:
    *   **`Layer 1 & 2` (Scalar witness)**: Assigned to ultra-high single-thread frequency CPUs (`c4-highcpu-2` / Intel Emerald Rapids).
    *   **`Layer 3` (NTT FFT Leaf Prover)**: Perfectly suited for **GPU Offloading** (CUDA / ICICLE / Cysic / Metal) or massive elastic arrays of preemptible ARM Neoverse V2 Spot VMs.
    *   **`Layer 4` (Recursive Plonk)**: Assigned to dedicated memory-bandwidth compute instances (`m4-highmem`).
3.  **Fault-Tolerant Spot Elasticity**: Leaf proofs are 100% stateless. If Spot VM #47 is preempted by GCP during chunk #89, another worker dequeues chunk #89 with zero disruption to the block timeline.

### 🔴 Challenges & Architectural Complexity
1.  **Distributed Straggler Management**: In monolithic proving, if chunk #42 hangs, the daemon panics. In distributed proving, network packet drop or a single straggler worker on a degraded GCE host stalls final aggregation. Requires strict dead-letter queues and speculative execution duplication.
2.  **DevOps Infrastructure Overhead**: Provisioning, monitoring, and securing distributed message brokers (Redis/NATS/PubSub) increases operational cloud infrastructure complexity.
3.  **Sequential Chain Tail Stalls**: If Layer 4 aggregates chain proofs linearly ($P_i = \text{Agg}(P_{i-1}, L_i)$), Layer 4 still processes step-by-step. To unlock sub-second aggregation, Layer 4 must transition to **Log-Depth Binary Tree Reduction** ($P_{0..3} = \text{Agg}(\text{Agg}(L_0, L_1), \text{Agg}(L_2, L_3))$).

---

## 3. Hypothesis Testing: Isolated Validation Experiments 🛠️🧪

To rigorously prove or disprove this distributed layer-splitting hypothesis **without impacting our primary local working codebase**, we will execute validation experiments inside a 100% isolated branched workspace (`Workspace: "branch"` git worktree clone at `/tmp/lighter-prover-distributed-exp`).

We will report back empirical telemetry findings (cache thrashing reduction, IPC serialization latency, and effective TPS) to refine our master architectural proposal.

### Experiment A: Isolated NUMA/IPC Shared-Memory Study
*   **Concept**: Spawn isolated `prover_producer` and `prover_consumer` OS processes on `c4a-highcpu-72`.
*   **Mechanism**: Transmit `ProofWithPublicInputs` over POSIX shared memory (`shm_open`) or UNIX domain sockets. Lock processes to **isolated virtual hardware NUMA sockets** (`numactl --cpunodebind=0` vs `1`).
*   **Architectural Note (No GPU Required)**: NUMA (Non-Uniform Memory Access) is strictly a multi-socket / multi-die CPU memory architecture. Locking Producer to NUMA node 0 and Consumer to NUMA node 1 guarantees that their L3 cache lines and DDR5 memory controllers are 100% physically separated on silicon, requiring zero GPUs!
*   **Validation**: Proves whether physical NUMA socket separation eliminates the ~10% Rayon core thrashing penalty.

### Experiment B: Lightweight Local Docker Network Backplane Prototype
*   **Concept**: Build prototype leaf worker and aggregator daemons using `tonic` (gRPC).
*   **Mechanism**: Execute inside isolated local Docker container bridge network. Benchmark byte serialization (`bincode`) throughput.

### Experiment C: Binary Reduction Tree Aggregation Study
*   **Concept**: Author mathematical feasibility report transitioning `BlockTxChainCircuit` from linear chaining to log-depth binary reduction trees.

---

## User Review Required & Open Questions 🛑

> [!IMPORTANT]
> **Production Backplane Alignment**: Which networking messaging broker does your Google Cloud engineering ecosystem prefer for low-latency internal RPCs? (e.g., Google Cloud PubSub, NATS Core, Redis Stream, or direct gRPC peer-to-peer)?

> [!WARNING]
> **Smart Contract Verifier Compatibility**: Does transitioning from linear recursive chaining (`BlockTxChainCircuit`) to binary reduction trees impact the verification logic of Lighter's Ethereum / L1 settlement smart contract verifier?

---

## Verification Plan

### Automated Cloud Validation Matrix (Dedicated Temporary Instance)
To eliminate workstation OS noise and prevent benchmark collision with active Phase 3 sweeps on `prover-vm-5`, all validation sweeps will execute on a dedicated temporary GCE VM (**`prover-vm-temp`** or **`prover-vm-4`**, `c4a-highcpu-64/72` in `us-east4-b`):
1. Spawn isolated branched worktree: `git worktree add /tmp/lighter-prover-distributed-exp -b distributed-exp`
2. Start temporary VM: `make cloud-vm-start VM="prover-vm-4"` (or `gcloud compute instances create prover-vm-temp ...`)
3. Compile and execute remote cloud NUMA benchmark matrix: `make cloud-bench-run VM="prover-vm-4" JOBS=10`
4. **Mandatory Auto-Teardown**: Power off and destroy temporary VM immediately upon benchmark conclusion: `make cloud-vm-stop VM="prover-vm-4"`

### Automated Byte Serialization Verification (Zero Manual Checks)
We will completely automate byte serialization ratio benchmarking (`bincode::serialize` vs `serde_json::to_vec` for `PartialWitness` ~4 KB vs `ProofWithPublicInputs` ~150 KB) inside our benchmark reporting binary, exporting exact byte metrics directly in our telemetry findings! Zero manual human checks required!
