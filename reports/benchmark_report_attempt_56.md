# Benchmark Report: Attempt 56 (`BLOCKS=1`, Radix-16 Hex, Unified Pool, Image Streaming Disabled)

## Executive Summary
Attempt 56 evaluated **Radix-16 Hex Subtree Proving** across **88 unified worker pods** on GKE (`c3d-highcpu-8` architecture, AMD EPYC 9454D) with **Image Streaming Disabled** and an upgraded **10Gi per-pod memory limit**.

The benchmark successfully completed end-to-end proof aggregation to the root (`tree_L2_N0.proof`), resolving the OOM limit constraint during unbaked 8-way root fold circuit compilation (which peaked at 7.98 GiB RSS).

---

## Benchmark Configuration
- **Benchmark ID**: `attempt-56`
- **Blocks**: `1` (500 total transactions)
- **Leaves / Block**: 125 leaves (`chunk_size=4`)
- **Fold Strategy**: `hex` (Radix-16)
- **Pool Topology**: `unified` (88 worker pods on `lighter-fungible-prover`)
- **Memory Limit / Pod**: `10Gi` (upgraded from 6Gi default)
- **Image Streaming**: `Disabled` (`--no-enable-image-streaming`)
- **GKE Cluster**: `lighter-prover-cluster-c3d` (`us-east4-b`)

---

## End-to-End Performance Breakdown

| Stage | Sub-Stage / Proof | Count | Proving Latency (Avg/Range) | GCS Write | Peak RSS | Wall Time / Status |
| :--- | :--- | :--- | :--- | :--- | :--- | :--- |
| **Stage 1** | STARK Leaf Proofs | 125 / 125 | ~5.2s – 10.4s | ~0.76s – 1.9s | ~2.8 GiB – 3.3 GiB | **~35s** (All 125 Committed) |
| **Stage 2** | Level 1 16-way Hex Folds | 8 / 8 | 65.7s – 80.2s | ~0.91s – 2.17s | 4.83 GiB – 5.12 GiB | **~79s** (All 8 Committed) |
| **Stage 3** | Level 2 8-way Root Fold | 1 / 1 | **49.68s** | 2.01s | **7.98 GiB** | **Root Proof Reached** |

---

## Key Performance Insights

1. **Memory Ceiling Validation**:
   - Compiling and proving the final 8-way root aggregation fold (`tree_L2_N0.proof`) consumes **7.98 GiB Peak RSS**.
   - Upgrading `default_unified_mem` to `10Gi` prevented pod OOMKilled restarts, allowing the root fold to complete smoothly in **49.68s**.

2. **Image Streaming Disabled Impact**:
   - Dynamic Rust/plonky2 circuit compilation latency remained under **80s per 16-way hex node fold** (compared to 10+ minutes with image streaming enabled).

3. **High Concurrency Parallel Execution**:
   - With 88 worker pods active on the unified pool, all 125 leaf proofs finished in under 35 seconds, and all 8 Level 1 subtree folds executed concurrently across available workers.

---

## Status & Next Steps
- **Attempt 56**: **PASSED** (Root STARK proof `tree_L2_N0.proof` verified in GCS).
- **Next Stage**: Proceed directly to **Attempt 57** (`BLOCKS=10` stress test across 88 unified worker pods).
