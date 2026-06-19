---
name: distributed_proving_topology_plan
description: Deep research, architectural trade-off analysis, and experimental design for splitting proof generation layers across distributed workers, containers, and VMs.
---

# Research & Implementation Plan - Distributed Layer-Split Proving Architecture

## Goal Description
Currently, Lighter Prover executes the entire vertical proving stack (Layers 1 through 4) inside individual monolithic daemons or VM containers (`bench.rs`). While Phase 2 pipelining overlaps leaf generation with recursive aggregation, running both massive circuits on shared CPU cores induces heavy L2/L3 cache eviction thrashing.

This document presents deep architectural research, formal trade-off analysis (Pros vs. Cons), and experimental validation designs for **Splitting Proving Layers across Distributed Workers, Cells, Containers, and VMs** over a high-speed networking fabric.

---

## 1. Architectural Blueprint: The 3-Tier Distributed Topology

```mermaid
graph TD
    classDef t1 fill:#0f172a,stroke:#38bdf8,stroke-width:2px,color:#fff;
    classDef t2 fill:#0f172a,stroke:#4ade80,stroke-width:2px,color:#fff;
    classDef t3 fill:#0f172a,stroke:#facc15,stroke-width:2px,color:#fff;
    classDef bus fill:#1e293b,stroke:#94a3b8,stroke-width:2px,color:#fff;

    A[Ingest Rollup Block: 500 Txs] --> T1["Tier 1: Upfront Scalar State Sequencer Pod<br>(c4-highcpu-2 @ 3.8 GHz x86_64)<br>Executes BlockPreExec & Witness Batching"]:::t1

    T1 -->|125 x 4.1 KB PartialWitness Jobs| BROKER[("High-Speed Backplane Fabric<br>Redis Stream / gRPC / PubSub / NATS")]:::bus

    subgraph Elastic Stateless Leaf Fleet (Tier 2 Workers)
    BROKER --> W1["Leaf Worker 1 (c4a-72 Spot / GPU)"]:::t2
    BROKER --> W2["Leaf Worker 2 (c4a-72 Spot / GPU)"]:::t2
    BROKER --> WN["Leaf Worker N (c4a-72 Spot / GPU)"]:::t2
    end

    W1 & W2 & WN -->|125 x 150 KB STARK Proofs| STORE[("Proof Artifact Store<br>Redis / GCS / Memcached")]:::bus

    subgraph Dedicated Aggregation Tree (Tier 3 Consumer)
    STORE --> AG["Tier 3: Recursive Chain Aggregator Pod<br>(m4-highmem Dedicated NUMA Node)"]:::t3
    AG --> FINAL["Verifiable Block Rollup Proof"]
    end
```

### Byte-Precision Network Bandwidth Physics 📡

1.  **Tier 1 $\rightarrow$ Tier 2 (`PartialWitness` Ingest)**:
    *   In Plonky2, `PartialWitness` stores wire assignments for public input targets (`num_public_inputs_tx = 520`).
    *   $520 \text{ GoldilocksField elements} \times 8 \text{ bytes} = \mathbf{4,160 \text{ bytes}}$ ($\sim 4.1 \text{ KB}$) per leaf chunk.
    *   Transmitting all 125 chunks for a 500-tx block equals **$512 \text{ KB total payload}$**. Over a 100 Gbps GCP VPC network, transmission takes **$0.04 \text{ milliseconds}$** (zero network drag).
2.  **Tier 2 $\rightarrow$ Tier 3 (`ProofWithPublicInputs` Output)**:
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
3.  **Sequential Chain Tail Stalls**: If Tier 3 aggregates chain proofs linearly ($P_i = \text{Agg}(P_{i-1}, L_i)$), Tier 3 still processes step-by-step. To unlock sub-second aggregation, Tier 3 must transition to **Log-Depth Binary Tree Reduction** ($P_{0..3} = \text{Agg}(\text{Agg}(L_0, L_1), \text{Agg}(L_2, L_3))$).

---

## 3. Hypothesis Testing: Proposed Experiments 🛠️🧪

To rigorously prove or disprove this distributed concept without premature infrastructure over-engineering, we propose three incremental validation experiments:

### Experiment A: The IPC Shared-Memory Prototype (Local Machine)
*   **Concept**: Spawn two distinct OS processes (`prover_producer` vs. `prover_consumer`) on a single bare-metal flagship instance (`c4a-highcpu-72`).
*   **Mechanism**: Transmit `ProofWithPublicInputs` payloads over POSIX shared memory (`shm_open` / UNIX domain sockets). Use `numactl --cpunodebind=0` vs. `1` to lock Producer and Consumer to **isolated hardware NUMA sockets**.
*   **Validation**: Proves whether physical NUMA/cache separation eliminates the $\sim 10\%$ Rayon thread thrashing penalty observed in Phase 2.

### Experiment B: Lightweight gRPC / Redis Stream Backplane
*   **Concept**: Build `distributed_leaf_worker.rs` and `distributed_aggregator.rs` using `tonic` (gRPC) or `redis`.
*   **Mechanism**: Orchestrate 2 separate Docker containers communicating across a local docker network bridge. Measure serialization (`bincode` / `serde`) overhead.

### Experiment C: Binary Reduction Tree Aggregation Study
*   **Concept**: Author a feasibility report refactoring `BlockTxChainCircuit` from sequential linear recursion to a log-depth binary reduction tree.

---

## User Review Required & Open Questions 🛑

> [!IMPORTANT]
> **Production Backplane Alignment**: Which networking messaging broker does your Google Cloud engineering ecosystem prefer for low-latency internal RPCs? (e.g., Google Cloud PubSub, NATS Core, Redis Stream, or direct gRPC peer-to-peer)?

> [!WARNING]
> **Smart Contract Verifier Compatibility**: Does transitioning from linear recursive chaining (`BlockTxChainCircuit`) to binary reduction trees impact the verification logic of Lighter's Ethereum / L1 settlement smart contract verifier?

---

## Verification Plan

### Automated Tests
1. Compile distributed proof crates: `cargo check --workspace --benches`
2. Run local NUMA IPC benchmark validation: `make local-bench-numa-ipc` *(To be implemented)*

### Manual Verification
Review byte-serialization benchmarking ratios across `PartialWitness` and `ProofWithPublicInputs` payloads.
