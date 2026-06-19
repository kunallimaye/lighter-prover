---
name: proposal_phase2_async_pipelining_improvements
description: Definitive engineering proposal for Phase 2 asynchronous pipelined STARK proof generation based on Phase 1 side-by-side layer comparison.
---

# Engineering Proposal - Phase 2 Asynchronous Pipelined Proving Architecture

## Executive Summary
Following the deployment of **Phase 1 (Micro-Architectural Telemetry Instrumentation)**, we executed side-by-side empirical profiling benchmarks across Google Cloud ARM Neoverse V2 flagship infrastructure (`c4a-highcpu-72`).

Comparing **Run 1 (`JOBS=1`, uncontended baseline)** against **Run 2 (`JOBS=10`, high-concurrency bus contention)** isolates the exact time spent at every layer of proof generation. Crucially, our telemetry proves that **pure scalar state witness generation takes an identical $2.52\text{ ms}$ regardless of thread allocation**, whereas multithreaded FRI STARK layers experience heavy scaling contention.

This proposal outlines **Phase 2 (Asynchronous Pipelining)**, decoupling upfront scalar state execution from cryptographic STARK proving via bounded stream channels.

---

## 1. Side-by-Side Layer Breakdown (`JOBS=1` vs `JOBS=10`)

Across $500\text{ transactions}$ ($125\text{ leaf chunks}$, $4\text{ txs/leaf}$) on `c4a-highcpu-72`, Phase 1 telemetry banked the following comparative empirical spectrum:

| Proving Layer / Cryptographic Phase | Run 1 (`JOBS=1`, $72\text{ threads/job}$) | Run 2 (`JOBS=10`, $7\text{ threads/job}$) | Scaling Contention & Thread Sensitivity |
| :--- | :---: | :---: | :---: |
| **Layer 1: Block Setup (`BlockPreExec`)** | $277.28\text{ ms}$ | $1,091.09\text{ ms}$ | $+293\%$ latency ($3.93\times$ slow down) |
| **Layer 2: State Witness Gen (`generate_witness`)** | **$2.52\text{ ms / leaf}$** ($0.315\text{s total}$) | **$2.52\text{ ms / leaf}$** ($0.315\text{s total}$) | **$1.00\times$ (Zero Thread Scaling)** ⚡ |
| **Layer 3: STARK Leaf Proving (`BlockTxCircuit`)** | **$1,256.66\text{ ms / leaf}$** ($157.08\text{s total}$) | **$5,231.27\text{ ms / leaf}$** ($653.91\text{s total}$) | $+316\%$ latency ($4.16\times$ slower per daemon) |
| **Layer 4: Recursive Aggregation (`BlockTxChain`)** | **$278.37\text{ ms / step}$** ($34.80\text{s total}$) | **$990.59\text{ ms / step}$** ($123.82\text{s total}$) | $+256\%$ latency ($3.56\times$ slower per daemon) |
| **Total Daemon Proving Wall Time ($500\text{ txs}$)** | **$192.45\text{ seconds}$** ($\sim 3.21\text{m}$) | **$779.13\text{ seconds}$** ($\sim 12.99\text{m}$) | $4.05\times$ daemon latency increase |
| **Individual Daemon Speed** | $2.598\text{ TPS}$ | $0.642\text{ TPS}$ | |
| **Aggregate Fleet Instance Capacity** | **$2.598\text{ Aggregate TPS}$** | **$6.420\text{ Aggregate TPS}$** | **$+147\%$ Total Hardware Bandwidth Gain** 🏆 |

### Micro-Architectural Takeaways 📐🔬

1.  **Strictly Scalar Witness Invariance**: Layer 2 (`generate_witness`) executes purely sequential CPU instructions (wire assignments, order book matching, account deltas). Because it does not utilize Rayon multi-threading or Goldilocks NTT transforms, its runtime is **locked at $2.52\text{ ms}$** whether allocated 72 threads or 7 threads.
2.  **The Aggregation Bottleneck Stall**: In high-concurrency production deployments (`JOBS=10`), daemons spend **$123.82\text{ seconds}$ ($15.9\%$ of total CPU time)** sitting in sequential stalls computing Layer 4 (`BlockTxChainCircuit`) aggregation proofs.

---

## 2. Phase 2 Technical Blueprint (Decoupled Stream Channels)

We propose refactoring `bench/src/bin/bench.rs` into an asynchronous **Producer-Consumer Channel Pipeline**:

```mermaid
graph TD
    classDef wit fill:#0f172a,stroke:#38bdf8,stroke-width:2px,color:#fff;
    classDef pool fill:#0f172a,stroke:#4ade80,stroke-width:2px,color:#fff;
    classDef agg fill:#0f172a,stroke:#facc15,stroke-width:2px,color:#fff;
    classDef root fill:#0f172a,stroke:#f43f5e,stroke-width:2px,color:#fff;

    A["Layer 1: Block Setup (BlockPreExec)"] --> B["Layer 2: Upfront Scalar Witness Batcher"]:::wit
    B -->|0.315s Total Wall Time| C["Layer 2 Output: Witness State Buffer (125 Buffers)"]

    subgraph Rayon Concurrent Proving Pool (Layer 3 Producer Workers)
    C --> P1["Layer 3: STARK Leaf Worker 1 (BlockTxCircuit::prove)"]:::pool
    C --> P2["Layer 3: STARK Leaf Worker 2 (BlockTxCircuit::prove)"]:::pool
    C --> P8["Layer 3: STARK Leaf Worker N (BlockTxCircuit::prove)"]:::pool
    end

    P1 -->|tx_proof 0| CH((mpsc::channel))
    P2 -->|tx_proof 1| CH
    P8 -->|tx_proof N| CH

    subgraph Dedicated Recursive Aggregator (Layer 4 Consumer Thread)
    CH --> AG["Layer 4: Asynchronous Chain Aggregator (BlockTxChainCircuit::prove)"]:::agg
    AG --> FINAL["Verifiable Rollup Block Proof"]:::root
    end
```

### Layer 2: Upfront Scalar Witness Batcher ($0.315\text{s}$)
*   Execute all 125 `BlockTxCircuit::generate_witness()` calls upfront upon block ingestion.
*   Because witness generation is strictly scalar ($2.52\text{ms}$), batching all 125 chunks takes only **$0.315\text{ seconds}$ total**.

### Layer 3: Bounded Rayon Leaf Proving Pool
*   Dispatch leaf STARK proving tasks (`plonky2::prove`) across concurrent Rayon threads.
*   Transmit completed `(chunk_index, tx_proof)` cryptographic artifacts over a bounded channel.

### Layer 4: Dedicated Asynchronous Recursive Aggregator
*   A dedicated background worker thread dequeues leaf proofs in index sequence (`chunk 0, chunk 1...`) and continuously computes `BlockTxChainCircuit::prove`.

---

## 3. Projected Fleet Bandwidth Lift 📈🏎️

By completely hiding the **$123.82\text{ seconds}$** of aggregation critical path behind concurrent STARK leaf generation, Phase 2 eliminates sequential stalls:

| Proving Layer / Cryptographic Phase | Phase 1 Sequential Baseline (`JOBS=10`) | Projected Phase 2 Decoupled Pipeline | Net Latency Delta & Architectural Lift |
| :--- | :---: | :---: | :---: |
| **Layer 1: Block Setup (`BlockPreExec`)** | $1,091.09\text{ ms}$ | $1,091.09\text{ ms}$ | $\pm 0.0\%$ (Unchanged setup) |
| **Layer 2: State Witness Gen (`generate_witness`)** | $2.52\text{ ms / leaf}$ ($0.315\text{s total}$) | $2.52\text{ ms / leaf}$ ($0.315\text{s total}$) | Executed upfront in batch ($0.315\text{s}$) |
| **Layer 3: STARK Leaf Proving (`BlockTxCircuit`)** | $5,231.27\text{ ms / leaf}$ ($653.91\text{s total}$) | $5,231.27\text{ ms / leaf}$ ($653.91\text{s total}$) | $100\%$ overlapping Rayon producer stream |
| **Layer 4: Recursive Aggregation (`BlockTxChain`)** | $990.59\text{ ms / step}$ ($123.82\text{s total}$) | **$0.00\text{ ms}$** *(Completely Hidden)* | **$-123.82\text{s}$ (100% Async Consumer Overlap)** ⚡ |
| **Total Daemon Proving Wall Time ($500\text{ txs}$)** | **$779.13\text{ seconds}$** ($\sim 12.99\text{m}$) | **$655.31\text{ seconds}$** ($\sim 10.92\text{m}$) | **$-15.9\%$ Total Wall Time Reduction** |
| **Individual Daemon Effective Speed** | $0.642\text{ TPS}$ | **$0.763\text{ TPS}$** | **$+18.8\%$ Proving Speed Boost** |
| **Aggregate Fleet Instance Capacity ($72\text{ cores}$)** | $6.420\text{ Aggregate TPS}$ | **$7.630\text{ Aggregate TPS}$** | **$+1.21\text{ Net TPS / Node}$** 🏆 |

---

## 4. Formal Review Gate & Next Steps 🛑

Per your directive:
> *5. Await my review before implementing phase 2*

**Zero Phase 2 code refactoring has been executed**. Please review this comparative empirical proposal. Upon your approval, we will proceed to implement the asynchronous producer-consumer channel architecture in `bench.rs`!
