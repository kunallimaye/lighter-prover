# Full-scale fleet sizing + scaled validation ladder (0.2% / 0.3% / 0.5%)

> Refs #95 #144 #75 #113. **Non-closing.** This is the *idealized* (k=1
> single-machine lower bound) sizing projection (#95) plus the scaled GKE
> validation ladder the conductor (#75) benchmark will run to **measure**
> the G4 contention / real coordinator-utilization unknowns. It does NOT
> apply anything, spend, or prove. It authors numbers + config + a runbook.

## 0. Provenance: which model, and did it self-check

The merged **#95 parametric fleet-sizing model** is `scripts/fleet-size.py`
(found on `main`; closed issue #95, PR #147). It was **used** (not bypassed).
Its self-consistency golden test passes to the millisecond:

```
$ python3 scripts/fleet-size.py --self-check
self-check: c4a-highcpu-64 S=9 9000-tx central path
  computed central path        = 8.730 s
  committed single_machine_wall_9000 = 8.730 s  (calibration/c4a-highcpu-64.json)
  components: L1 3.051 + merge 2.751 (depth 10) + L4 2.928
  match (to the ms): PASS
```

Search trail (for the record): `grep -rli "sizing|fleet|coordinators"` →
`scripts/fleet-size.py`, plus `scripts/bench-fleet/*` (the runner, not the
model). The model is the one used below.

## 1. Measured constants (every one cited)

All from `calibration/c4a-highcpu-64.json` — the deployment-candidate Axion
shape — at **S=9** (the SLO-slack winner, `objectives.slo_slack.s = 9`,
`min_slack = 11.27 s`). sha `e87152ba5e995d5eda3cd323adb0fb016b77917e`,
circuit hash `f634a649afd2f1d03c8647f9720a1ee528b869b9`.

| Constant | Value | JSON key | Label |
|---|---|---|---|
| L1 chunk prove wall | **3.051 s** | `per_s_table[S=9].l1_wall_ms = 3051` | measured |
| L2 merge step | **0.2751 s** | `constants.merge_s.value` | measured |
| L4 block prove (serial, coordinator) | **2.928 s** | `constants.l4_wall_s.value` | measured |
| Cell peak RSS | **5266 MB (5.1 GiB)** | `per_s_table[S=9].peak_rss_mb` | measured |
| Lag bound p50 / p99 | **20 s / 40 s** | `lag_p50_s` / `lag_p99_s` | ADR-0004 §0 |

> The brief mentioned an L5 "chained fold ~1.23 s". That term is **not on
> the block-proof critical path** the governing equation bounds — the L5/L6
> batch-finalize is a SEPARATE cadence on EPYC-only constants terminating in
> the UNMODELED L6 gate (#83), per ADR-0004 §5 and `fleet-size.py`
> `segment_folder_topology()`. We do **not** fold it into the lag sum.
> Reported here as topology only, not invented into a wall.

## 2. The real load to size for (#128 / G1, PR #163)

- Block size **bimodal**: ~73.57% at the **500-tx chain cap**; mean **400.72
  tx/block**. We size at the **500-tx cap** — the worst case the majority of
  blocks actually hit — not the mean. This is the conservative,
  measured-anchored choice (a 500-tx block needs k = ceil(500/9) = **56
  chunks** at S=9).
- Arrival: **mean 11.08 blocks/s**, p99 **25**, max **41**. Rolling peaks
  1s=41 / 3s=77 / 5s=107 / 10s=179 blocks.

### 2.1 The arrival-rate decision (a documented decision, NOT a default)

The governing equation (ADR-0004 §0) names a **>=5 blocks/s SLO floor**. The
real measured load is **mean 11.08/s** — **2.2x above** that floor — with
**peak 41/s (8.2x above)**.

**The 5/s floor is a minimum SLO obligation, not the expected demand.**
Sizing the steady fleet at 5/s would **under-provision by ~2.2x** against
real mean traffic and the lag SLO would be violated whenever load exceeds
the floor (i.e. most of the time).

**DECISION: size the steady cell + coordinator fleet for the real mean
~11.08 blocks/s.** Report the 5/s-floor count as the documented *lower
bound* and the 41/s-peak count as the *HPA burst ceiling* the autoscaler
must be able to reach. Rationale:

- Steady-state pools must hold the SLO at *real* sustained load, not the
  contractual floor.
- p99/peak bursts (25/s, 41/s) are absorbed by the **Pub/Sub-backlog HPA**
  (ADR-0006 §1.1/§5) scaling cells up toward the peak count — that is what
  the `hpa_max_replicas` ceiling is for, not the steady replica count.

This is exactly the "real ~11/s mean, not just the 5/s floor" discrepancy
the task asked to surface — flagged and decided here, to be confirmed by the
benchmark.

## 3. Full-scale calculation (work shown, BY CLASS, never summed)

Two SEPARATE machine classes (ADR-0004 §6.2 / ADR-0006 §2). The model
**never** emits a summed machine count and neither does this doc.

Run (the steady sizing point — real mean):

```
python3 scripts/fleet-size.py --shape c4a-highcpu-64 --s 9 \
    --blocks-per-s 11.08 --tx-per-block 500
```

### 3.1 Central-path lag readout (HOLD c, read lag) — k=1 lower bound

```
per_block_lag = L1_max_over_chunks + ceil(log2(k))*merge_s + L4   [+ witness_move UNMODELED]
k = ceil(500/9) = 56
merge_depth = ceil(log2(56)) = 6
            = 3.051 + 6*0.2751 + 2.928
            = 3.051 + 1.6506 + 2.928
            = 7.630 s   (vs p50 20 s  =>  +12.370 s slack)  FEASIBLE
```

The **+12.37 s slack** is the *entire budget* for the UNMODELED tail
(straggler max-of-k #101, coordinator recovery #75, witness_move #61). At
real load the central path holds the bound with wide margin on the idealized
floor — the open question is how much of that margin G4 contention eats,
which is what the benchmark measures.

### 3.2 Class 1 — chunk-prover CELLS (Little's law over the chunk pool)

```
cells = (blocks/s * k) * L1_wall
      = (11.08 * 56) * 3.051
      = 620.48 chunks/s * 3.051 s
      = 1893.08  =>  ceil = 1894 cells
RAM/cell = 5266 MB (measured peak_rss_mb)   fleet cell RAM ~= 9.74 TiB
```

### 3.3 Class 2 — COORDINATORS (block-grain fold service time) — SEPARATE

```
coord_service = merge_tree + L4 = 1.6506 + 2.928 = 4.579 s/block
coordinators  = blocks/s * coord_service / concurrency
              = 11.08 * 4.579 / 1.0
              = 50.73  =>  ceil = 51 coordinators
```

> **STATED CONSERVATIVE ASSUMPTION (coordinator sizing).**
> `concurrency = 1` block/coordinator. Per-coordinator vertical concurrency
> is **PROMISING but NOT PROVEN** (ADR-0006 §2 / #113) — no per-coordinator
> utilization profile has been measured. Sizing at concurrency=1 is the
> conservative upper bound on the coordinator count; if the benchmark shows
> concurrency ~3–5x, the pool shrinks ~3–5x. **The benchmark validates
> this.** Coordinator-specific RSS is **UNMODELED** — we carry the
> worker-cell 5266 MB envelope as a documented PROXY (model `coord_rss_mb_proxy`),
> not an invented coordinator number.

Sanity check vs ADR-0006 §2: ADR cites "~30 coordinators at >=5 blocks/s,
zero concurrency" using **k=1000** (~5.7 s/block). Our 500-tx cap gives
k=56 → 4.579 s/block, so at 5/s we get **23** (not 30) — consistent: fewer
chunks per block → shorter fold tree → faster coordinator service → fewer
coordinators. At the real 11.08/s the count is **51**. ~51/1894 ≈ **2.7%**
of the cell fleet (ADR-0006 says "~1%"; the difference is the k=56 vs
k=1000 service time and is in the same order — distinct, negligible pool).

### 3.4 Full-scale sizing matrix (S=9, 500-tx cap, c4a-highcpu-64)

| Arrival | Role | Cells (raw) | Coordinators (raw) | Note |
|---|---|---|---|---|
| 5 blk/s | SLO floor (lower bound) | **855** (854.3) | **23** (22.9) | contractual minimum |
| **11.08 blk/s** | **real mean (STEADY SIZING POINT)** | **1894** (1893.1) | **51** (50.7) | the chosen size |
| 41 blk/s | max burst (HPA ceiling) | **7006** (7005.1) | **188** (187.7) | autoscaler target, not steady |

Machine class: **c4a-highcpu-64** (Axion / neoverse-v2, arm64), Autopilot
**Scale-Out** compute class; one cell saturates a whole 64-vCPU box
(ADR-0003). cell_cpu_request ≈ 62 vCPU (whole box minus Autopilot system
reservation); cell_memory_request sized to the 5.1 GiB RSS + L4/L5 keys +
headroom.

## 4. Scaled validation ladder (0.2% / 0.3% / 0.5% of full scale)

Percentages applied to the **steady (11.08/s) full-scale** counts. **Cells**
use **round-half-up** to the nearest integer. **Coordinators** are
**floored at 1** for every tier (see the decision below).

| Tier | Cells = round(1894·p) | math | Coordinators (strict 51·p) | PDB minAvailable |
|---|---|---|---|---|
| **0.2%** | **4** | 1894·0.002 = 3.788 → 4 | **1** (floored; strict 0.102) | 1 |
| **0.3%** | **6** | 1894·0.003 = 5.682 → 6 | **1** (floored; strict 0.153) | 1 |
| **0.5%** | **9** | 1894·0.005 = 9.470 → 9 | **1** (floored; strict 0.255) | 1 |

> **Cell rounding note.** `round-half-up` to the nearest integer (a change
> from the prior `ceil`-based ladder). 0.5% → 9.470 rounds to **9**, not 10
> (0.470 < 0.5). 0.2% → 3.788 → 4; 0.3% → 5.682 → 6.

### Coordinator class floored at 1 — DELIBERATE, STATED DECISION

At these tiny percentages the strict coordinator count is below 1 for **all
three** tiers (0.2%·51 = 0.102, 0.3%·51 = 0.153, 0.5%·51 = 0.255). The
coordinator class is **NOT scaled below 1 by design** — it is intentionally
**FLAT at 1** across the whole ladder, with `coordinator_pdb_min_available =
1` on every tier. Two non-negotiable reasons:

1. **Operational floor.** A zero-coordinator fold service cannot prove
   anything — the L2 merge tree + L4 block-prove has no home. The pool may
   never be empty.
2. **The mandatory eviction mitigation.** The HARD safe-to-evict=false +
   PodDisruptionBudget mitigation (ADR-0003 amendment §3) requires at least
   one coordinator to protect; `minAvailable = 1` pins the single coordinator
   entirely un-evictable — the strictest form of the NON-NEGOTIABLE
   mitigation, so a bin-pack/eviction never takes the in-flight,
   key-resident coordinator.

This is a **deliberate, stated choice, not an oversight**: the ladder scales
the *cell* class down through 4/6/9 while holding the coordinator class at
its operational floor of 1.

### 4.1 Matching synthetic load per tier (the pacer drive level)

Load scales with the **same percentage** as the fleet (keep-pace is the
ratio test: a 0.2% fleet must keep pace with 0.2% of the real arrival). From
the real load (mean 11.08, p99 25, peak 41 blk/s):

| Tier | mean (blk/s) | p99 (blk/s) | peak (blk/s) | math |
|---|---|---|---|---|
| 0.2% | **0.02216** | 0.050 | 0.082 | 11.08·0.002 / 25·0.002 / 41·0.002 |
| 0.3% | **0.03324** | 0.075 | 0.123 | 11.08·0.003 / 25·0.003 / 41·0.003 |
| 0.5% | **0.05540** | 0.125 | 0.205 | 11.08·0.005 / 25·0.005 / 41·0.005 |

**Keep-pace predicate** the benchmark verifies per tier: at the tier's mean
drive rate, with the tier's cell+coordinator counts, the measured block
lag p50 stays <= 20 s and p99 <= 40 s, AND neither pool's backlog grows
unbounded (i.e. observed throughput >= drive rate). The *idealized* model
says yes (central path 7.63 s « 20 s); the benchmark measures the **G4
contention gap** between idealized and real.

## 5. UNMODELED / ASSUMED (named, never invented)

Carried straight from the #95 model's honesty ledger — do NOT fabricate
these (the project retracted a fabricated coordinator-utilization number in
PR #139; we will not repeat it):

- **witness_move** — #61 (no `witness_fetch_ms` in code) — omitted from the lag sum.
- **contention / scaling losses** — G4 — only measurable on a running system (#75); **this is what the ladder benchmark measures.**
- **realistic-data effects** — G2 — synthetic blocks not generated yet.
- **coordinator recovery latency** — #75 — no coordinator exists yet.
- **coordinator-specific RSS** — unmeasured — worker 5266 MB envelope is a documented PROXY.
- **per-coordinator concurrency** — #113 — ASSUMED = 1 (conservative); benchmark validates/corrects.
- **p99 straggler tail coefficient** — #101 — n=3 variance too thin.
- **L6 batch-finalize wrapper** — #83 — excluded from block-proof lag.

This is the **idealized k=1 single-machine projection**. Folding in G4
contention + G2 realism is what turns it from *idealized* into *confident*
(North Star #144, goal G5).

## 6. Runbook — staged plan per tier (AUTHOR ONLY; DO NOT RUN)

> Pre-req **dependency**: the arm64/Axion prover image
> (`<sha>-neoverse-v2`) must be built + pushed by `cicd/cloudbuild.yaml`
> first; the `cell_image` / `coordinator_image` placeholders in the tfvars
> must be replaced with that real tag. Until then these configs schedule
> against a placeholder ref and will not prove. `project_id` / `region`
> are passed on the CLI (no secrets in the tfvars).

For `T in {0p2pct, 0p3pct, 0p5pct}` (files `scale-0p2pct.tfvars`,
`scale-0p3pct.tfvars`, `scale-0p5pct.tfvars`):

1. **Apply** (impersonate the deployer SA — no human-key spend):
   ```
   terraform -chdir=cicd/terraform/gke apply \
     -var-file=scale-Tpct.tfvars \
     -var project_id=kunal-scratch -var region=us-central1 \
     # e.g. -var-file=scale-0p2pct.tfvars
     # gcloud config set auth/impersonate_service_account \
     #   lighter-prover-agent@kunal-scratch.iam.gserviceaccount.com
   ```
2. **Publish scaled load** to the dispatch topic (`<cluster>-dispatch`) at
   the tier's **mean** drive rate (0.2%=0.02216, 0.3%=0.03324,
   0.5%=0.05540 blk/s), then ramp to the tier **peak** (0.082 / 0.123 /
   0.205 blk/s) to exercise the HPA against `hpa_backlog_target`.
3. **Measure** block lag p50/p99, Pub/Sub backlog trend, per-pool
   contention (cells + coordinators sized SEPARATELY — verify neither pool
   is the bottleneck), and coordinator utilization (**the G4/#113 unknown**).
4. **Verify keep-pace** against §4.1: lag p50<=20 s, p99<=40 s, backlog
   stable, observed throughput >= drive rate. Record the idealized-vs-real
   gap into the #95 model as the G4 contention coefficient (feeds G5).
5. **Teardown**: `terraform -chdir=... destroy -var-file=scale-Tpct.tfvars`
   (`deletion_protection=false` on every scaled config makes this clean);
   confirm no leftover Pub/Sub topics/subs via the `resource_labels`.

**NONE of the above is executed by this change.** It is the staged plan.
