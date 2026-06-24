# Implementation Plan - Master Capstone Benchmark Harness (Direct Jetski Orchestration, Canary Preflights & Instant Resumability)

## Goal Description
Jetski will directly orchestrate and execute the comprehensive unmocked comparative STARK proving study in parallel across all configured virtual machines and Kubernetes architectures under unified partition `benchmark-id-ALL-2026-06-25_04-42-22`. 

To catch failures early and make resuming frictionless, execution incorporates **Fail-Fast Canary Preflight Probes** (testing basic container health in $< 30\text{ seconds}$ upfront) and **Local State Sentinel Checkpointing** (skipping completed jobs instantly without network overhead).

---

## User Review Required
> [!IMPORTANT]
> **Canary Preflight Verification**: Before launching full concurrency sweeps (`JOBS=1..10`), Jetski runs a rapid Canary Test (`JOBS=1`, `txs=10`) across all targets. If an image has broken arguments, missing libraries, or OOM issues, it is caught immediately.

> [!TIP]
> **Sentinel Checkpoint Cache**: Alongside verifying GCS (`gs://.../bench_summary.json`), Jetski maintains a local sentinel file `reports/.checkpoint_benchmark-id-ALL-2026-06-25_04-42-22.json`. Interrupted sweeps resume instantly reading local state.

---

## Open Questions
> [!NOTE]
> No blocking open questions. Ready for user confirmation.

---

## Proposed Execution Strategy (Fail-Fast Orchestration & Resumability)

### Step 1: Persistent Storage Partitioning
Lock unified identifier:
```bash
export BENCHMARK_ID="benchmark-id-ALL-2026-06-25_04-42-22"
```

### Step 2: Fail-Fast Canary Preflight Probes (Early Failure Detection)
Before committing multi-hour computing sweeps, directly execute rapid Canary preflights:
1. **Silicon Stockout & Quota Audit**: Check `gcloud compute machine-types list` across all 19 target VM families. If any VM (`prover-c4a-72`) is out of stock in its current zone, relocate it in `config.toml` upfront and apply via `make cloud-deploy`.
2. **Canary Execution (`JOBS=1`, `txs=10`)**: Run a single 1-concurrency canary test across all releases (`v0.0.1`, `v0.0.2`, `v0.0.3`, `radix-16`). If a container rejects CLI arguments or crashes upon boot, abort immediately so alternatives can be configured before committing full resources.

### Step 3: Parallel Resumable GCE VM Benchmarking (`VM=all`, `JOBS=1..10`)
Directly dispatch parallel benchmark execution across all healthy VM profiles. For each target VM, STARK release (`v0.0.1`, `v0.0.2`), and concurrency level (`j=1..10`):
1. **Local Sentinel Check**: Check `reports/.checkpoint_benchmark-id-ALL-2026-06-25_04-42-22.json` and GCS. If marked `DONE`, skip instantly (`[RESUME] Job <j> already recorded — skipping`).
2. **Execution & Checkpointing**: Run Job `<j>`. Upon conclusion, record exact status (`DONE`, `FAILED_OOM`, `SKIPPED_STOCKOUT`) in local sentinel cache.
3. **Dynamic Zone Relocation**: If `ZONE_RESOURCE_POOL_EXHAUSTED` occurs mid-sweep, dynamically relocate zone in `config.toml`, apply via `make cloud-deploy`, and resume missing jobs.

### Step 4: Parallel Resumable GKE Cluster Benchmarking (`BLOCKS=1..10`)
In parallel with Step 3, dispatch collaborative GKE validium proving cycles across all 4 architectures (`c3d`, `c4a`, `c4d`, `t2d`) across `v0.0.3-distributed-proving` and `radix-16-reduction-trees` across `BLOCKS=1..10`.
* **Sentinel Checkpoint**: Skip any GKE cycle where the corresponding summary object exists in GCS or local cache.

### Step 5: Unified Metric Extraction
Once all parallel VM and GKE workloads complete (or confirm 100% checkpoint existence), directly run:
```bash
python3 infra-as-code/scripts/extract_gcs_metrics.py \
  --gcs-prefix="gs://kunal-scratch-tfstate/benchmark-reports/benchmark-id-ALL-2026-06-25_04-42-22" \
  --output-json="reports/capstone_benchmark-id-ALL-2026-06-25_04-42-22.json" \
  --output-csv="reports/capstone_benchmark-id-ALL-2026-06-25_04-42-22.csv"
```

### Step 6: Google Sheets Ingestion
Confirm automated creation and table ingestion (featuring the renamed **Concurrent Jobs or Blocks** column) into Google Spreadsheet ID `1z8bIeeKaEnXP6UZW52pGLll0XrwjoLS0aBJOvs1qqd0` under sheet tab `2026-06-25_04-42-22`.

---

## Proposed Code Changes
None. No harness scripts will be modified. All canary probing, sentinel tracking, stockout recovery, and parallel orchestration are managed directly by Jetski.

---

## Verification Plan

### Manual Verification
1. Review the recommended Canary preflight and local sentinel tracking updates in `capstone_benchmark_master_plan.md`.
2. Click **Proceed** to approve and launch the fail-fast resumable benchmark suite.
