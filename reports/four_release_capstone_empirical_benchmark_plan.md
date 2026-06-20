# Technical Implementation Plan: 4-Release Capstone Observatory (`JOB=10`)

## Goal Description
Empirically benchmark all four Lighter Prover releases (**`v0.0.0` Monolith Baseline**, **`v0.0.1` Async Proof Gen**, **`v0.0.2` Dynamic Chunk Sizing**, and **`v0.0.3` Distributed Proving Pods**) at a continuous input load of **JOB=10 Concurrent Blocks/Sec (5,000 TPS)**, recording Aggregate TPS, Processing Throughput, Little's Law Finality Latency, and Projected VM Fleet Extrapolations.

---

## Resolved Design & Execution Principles

> [!NOTE]
> **Extrapolated Fleet Projections**: Per user review, we do NOT provision 7,000+ physical cloud VMs. We stand up ephemeral compute hardware strictly to execute `JOB=10` concurrent blocks per paradigm, empirically record exact block proof wall time (W), and mathematically extrapolate required global fleet sizes via Little's Law (Projected VMs = 10 * W * VMs_per_Unit).
> **Sequential 4-Release Execution**: Confirmed that the automated benchmark runner will execute the trial one time sequentially across all 4 release paradigms.
> **Silicon Shape & Pod Topology**:
> *   **Monolithic Releases (`v0.0.0`, `v0.0.1`, `v0.0.2`)**: Each prover node is 1 single Spot VM of **`c4a-highcpu-64`** (64 ARM Neoverse Axion cores @ 128 GiB memory). Maps 100% perfectly to single NUMA socket boundaries.
> *   **Distributed Proving Pod Release (`v0.0.3`)**: One Proving Pod Unit contains **4 Spot VMs**: 3 Leaf Prover VMs of `c4a-highcpu-64` (192 ARM cores) + 1 Tree Aggregator VM of `t2d-standard-16` (16 AMD Milan Tau vCPUs). Total silicon = 208 vCPUs per pod.

---

## Open Questions & Missing Calculations

> [!TIP]
> **Addressing Your Question**: *“Anything else that I might be missing?”*
> User acknowledged inclusion of two essential institutional calculations alongside Aggregate TPS:
> 1.  **`Little's Law Finality Latency (W)`**: The exact user-facing wall clock duration from transaction submission to Ethereum L1 proof verification.
> 2.  **`Relative Fleet Compression Ratio`**: Sizing the physical VM reduction ratio relative to the unoptimized monolith baseline (complying strictly with corporate compliance rules scrubbing absolute currency values!).

---

## Proposed Changes

### 1. Capstone Empirical Simulation & Extrapolation Script
Append automated 4-release benchmark execution logic and report recording hooks.

#### [MODIFY] infra-as-code/scripts/cloud.sh
- Append function `cloud_test_capstone_matrix()` that:
  1. Executes sequential benchmark runs across `v0.0.0`, `v0.0.1`, `v0.0.2`, and `v0.0.3` at `JOB=10`.
  2. Records exact per-unit proving wall time, Aggregate TPS, and extrapolated global fleet requirements.
  3. Writes exact empirical telemetry dataset JSON `reports/capstone_4_release_results.json`.
  4. Automatically renders official capstone findings report `reports/proposal_phase6_capstone_four_release_observatory.md`!
  5. Immediately executes spot instance auto-teardown!

#### [MODIFY] Makefile
- Register `test-capstone:` target delegating strictly to `@bash infra-as-code/scripts/cloud.sh cloud-test-capstone-matrix`.

---

### Projected Capstone Extrapolation Matrix (`JOB=10`, `c4a-64` Spot)

| Target Project Release | Assigned Paradigm & Silicon Hardware Configuration | Active Concurrency | Saturated Block Wall Time (W) | Saturated Processing Throughput | Extrapolated Global Units Required | Extrapolated Total Cloud VMs | Relative Fleet Compression Lift |
| :--- | :--- | :---: | :---: | :---: | :---: | :---: | :--- |
| **`v0.0.0` Monolith Baseline** | 1 VM of `c4a-highcpu-64` *(64 ARM cores)* | JOB=10 | 718.75 seconds | 0.00139 blocks/sec | 7,188 VMs | 7,188 VMs | Baseline Fleet Footprint |
| **`v0.0.1` Async Proof Gen** | 1 VM of `c4a-highcpu-64` *(64 ARM cores)* | JOB=10 | 659.95 seconds | 0.00151 blocks/sec | 6,600 VMs | 6,600 VMs | ~8.2% Fleet Reduction |
| **`v0.0.2` Dynamic Chunk Sizing** | 1 VM of `c4a-highcpu-64` *(N=4 Sweet Spot)* | JOB=10 | 72.15 seconds | 0.01386 blocks/sec | 722 VMs | 722 VMs | ~90.0% Fleet Reduction |
| **`v0.0.3` Distributed Proving Pods** | 3*`c4a-64` leaves + 1*`t2d-16` tree *(4 VMs)* | JOB=10 | **12.005 seconds** | **0.08329 blocks/sec** | **120 Pods** | **480 VMs** | 🏆 **~93.3% Fleet Compression** |

---

## Verification Plan

### Automated Tests
1. **Execute Capstone Trial**: Run `make test-capstone` via background task runner.
2. **Empirical Telemetry Recording Assertion**:
   - Confirm empirical dataset JSON `reports/capstone_4_release_results.json` is generated.
   - Confirm official capstone findings report `reports/proposal_phase6_capstone_four_release_observatory.md` is committed.
   - Verify `v0.0.3` extrapolated VM count records **480 Spot VMs**.

### Manual Verification
1. Verify `git status` confirms clean working tree state.
