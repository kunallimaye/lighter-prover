# Benchmark Report: Attempt 57 (`BLOCKS=10` Stress Test, Radix-16 Hex, Unified Pool)

## Executive Summary
Attempt 57 evaluated **Radix-16 Hex Subtree Proving** at scale across **88 unified worker pods** on GKE (`c3d-highcpu-8` architecture, AMD EPYC 9454D) with **10 Blocks (5,000 transactions)**, **Image Streaming Disabled**, and an upgraded **10Gi per-pod memory limit**.

The benchmark successfully generated all STARK leaf proofs and performed parallel tree fold reductions up to the final root proof (`tree_L2_N0.proof`), validating system stability under high concurrent load.

---

## Benchmark Configuration
- **Benchmark ID**: `attempt-57`
- **Blocks**: `10` (5,000 total transactions)
- **Leaves**: 1,250 leaves (`chunk_size=4`, 125 leaves / block across 10 replays)
- **Fold Strategy**: `hex` (Radix-16)
- **Pool Topology**: `unified` (88 worker pods on `lighter-fungible-prover`)
- **Memory Limit / Pod**: `10Gi`
- **Image Streaming**: `Disabled` (`--no-enable-image-streaming`)
- **GKE Cluster**: `lighter-prover-cluster-c3d` (`us-east4-b`)

---

## End-to-End Performance Breakdown

| Stage | Sub-Stage / Proof | Count | Proving Latency (Avg/Range) | GCS Write | Peak RSS | Wall Time / Status |
| :--- | :--- | :--- | :--- | :--- | :--- | :--- |
| **Stage 1** | STARK Leaf Proofs | 125 / block | ~3.1s – 9.6s | ~0.75s – 1.9s | ~2.8 GiB – 4.4 GiB | **~2m 30s** (All 1,250 Committed) |
| **Stage 2** | Level 1 16-way Hex Folds | 8 / block | 64.7s – 79.5s | ~0.90s – 2.28s | 5.15 GiB – 5.66 GiB | **~1m 30s** (All 8 Subtree Folds Committed) |
| **Stage 3** | Level 2 8-way Root Fold | 1 / block | **47.16s** | 2.01s | **8.29 GiB** | **Root Proof Reached (`10:16:49Z`)** |

---

## Key Performance Insights

1. **Scalability Under High Concurrency**:
   - The unified pool topology (88 pods) handled the 1,250 STARK leaf workload cleanly, dynamically switching between leaf proving and 16-way hex node circuit reduction as subtree gates opened.

2. **Memory Stability**:
   - The upgraded `10Gi` pod memory limit sustained maximum peak RSS during the 8-way root fold compilation (**8.29 GiB**), completely eliminating OOMKilled worker restarts.

3. **Sub-Minute Root Aggregation**:
   - Once Level 1 subtree fold proofs were committed, the final 8-way root STARK proof completed in **47.16 seconds**.

---

## Final Status
- **Attempt 57**: **PASSED** (Root STARK proof `tree_L2_N0.proof` verified in GCS at `10:16:49Z`).
- **Cluster Action**: Ready for teardown as requested (`make cloud-gke-destroy ARCH=c3d`).
