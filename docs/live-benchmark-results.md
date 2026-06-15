# Live distributed GKE benchmark — 0.2% / 0.3% / 0.5% ladder (real Axion arm64)

> **⚠️ SUPERSEDED — HISTORICAL RUN RECORD (build SHA ~`1d44b35`, PR #173).**
> The measurements below are preserved verbatim as the ground-truth record of
> the pre-fix #173 live Axion run; they were accurate AT THE TIME OF THAT RUN.
> Two gaps this run surfaced have since been CLOSED on main:
> - **FINDING D ("1 of 56 chunks prove") was FIXED in #177** (per-tx positional
>   pre-state; every chunk now proves). Evidence:
>   `docs/layer0-evidence/finding-d-gate.md`; design:
>   `docs/per-tx-prestate-corpus.md`.
> - **The coordinator "accounting-fold only" L2→L4 step was REPLACED by a real
>   L2→L4 merge + L4 prove+verify in #179** (PRs #182/#183/#187/#188; opt-in via
>   `--proof-bucket`, emitting a `coordinator_fold` BENCH_EVENT with measured
>   `merge_ms`/`l4_ms`). Local end-to-end gate:
>   `bench/tests/distributed_fold_e2e.rs` (`make e2e`).
>
> Read the numbers below as a dated snapshot, NOT the current capability.

> **Refs #75 #172 #144 #95 #128 #113 #171.** This is the REAL, executed
> run on live GKE Autopilot (Axion / neoverse-v2 / arm64) in project
> `kunal-scratch`. Every number here is measured against ground truth
> (gcloud/kubectl), not modeled. Where a tier fell short, that is recorded
> honestly as the finding.

## Build commit under test

- SHA: `1d44b35d5ee150373a6bc5b44fd64714babcce4d` (PR #173 — genuine
  distributed coordinator/cell entrypoint over real Pub/Sub).
- Image: `us-central1-docker.pkg.dev/kunal-scratch/lighter-prover/bench:1d44b35d5ee150373a6bc5b44fd64714babcce4d-neoverse-v2`

## Identity / IAM set up for this run (ledger → #171)

- **Operator identity:** `ai-workstation-runtime@kl-ai-workstation.iam.gserviceaccount.com`
  (has `roles/owner` on `kunal-scratch`, verified). Terraform + kubectl + gcloud
  run directly as this owner identity.
- **Image build SA:** `lighter-prover-builder@kunal-scratch.iam.gserviceaccount.com`
  (`artifactregistry.admin`, `storage.admin`, `logging.logWriter`) via
  `gcloud builds submit --service-account=...`.
- **Pod Workload Identity GSA (NEW):** `lighter-prover-pods@kunal-scratch.iam.gserviceaccount.com`
  - `roles/pubsub.subscriber` (project) — cells/coordinators pull chunk/dispatch/results subs
  - `roles/pubsub.publisher` (project) — coordinators publish chunks, cells publish results
  - `roles/pubsub.viewer` (project) — list/describe for diagnostics
  - WI binding: `roles/iam.workloadIdentityUser` granted to principal
    `serviceAccount:kunal-scratch.svc.id.goog[default/default]` (the pods run as
    the `default` KSA in the `default` namespace — terraform sets no
    `service_account_name`).
  - Per-tier runtime step: annotate the `default` KSA with
    `iam.gke.io/gcp-service-account=lighter-prover-pods@kunal-scratch.iam.gserviceaccount.com`
    after each cluster comes up, then roll the deployments so pods pick up WI.

## Assumptions (every one — the user authorized assumptions IF documented)

1. **Focused image build.** Built ONLY the `neoverse-v2` arm64 variant (the only
   image the GKE pods consume) via a transient single-variant cloudbuild config
   (`/tmp/opencode/cloudbuild-neoverse-v2-only.yaml`), not all 5 variants from
   `cicd/cloudbuild.yaml`. Same Containerfile, same `TARGET_CPU=neoverse-v2`,
   same `LIGHTER_REF`, same arch-verify. Saves ~60-80 min/cost; committed
   pipeline untouched.
2. **Operate as owner, not impersonation.** Ran terraform/kubectl directly as the
   owner `ai-workstation-runtime` rather than impersonating `lighter-prover-agent`.
   Owner is reliable and present; the agent SA may lack `container.admin`.
   Documented per brief's privilege-separation note.
3. **Drive terraform locally (not via Cloud Build pipeline).** The smoke pipelines
   (`cloudbuild-gke-*.yaml`) bake `-var-file=smoke.tfvars` + a fixed state prefix
   and have no live load/measure phase. The benchmark needs long interactive
   kubectl observation, so I installed terraform v1.9.8 locally and drove
   apply→load→measure→teardown directly, with a PER-TIER GCS state prefix
   (`lighter-prover/gke-scale-<tier>`) so tiers never race each other's state.
   Same module, same HCL, same backend bucket (`kunal-scratch-lighter-prover-gke-state`).
4. **Pod KSA = `default/default`.** main.tf does not set `service_account_name` on
   the cell/coordinator pod specs, so they run as the `default` KSA. WI is wired to
   that principal (above) rather than a dedicated KSA, to avoid editing the
   committed module mid-run.
5. **Block-job source + schema.** Dispatch messages are JSON `{"height":u64,"tx_count":u64}`
   (the `BlockMessage` schema in `bench/src/conductor/pubsub.rs`), published to the
   tier's `-dispatch` topic. `tx_count` = 500 (the 500-tx chain cap = the worst-case
   block the fleet is sized for, and exactly the size of the `bench_test.json`
   fixture the cells mount). height = a synthetic monotonically-increasing block
   number. This drives the coordinator to SPLIT k=ceil(500/9)=56 chunks per block.
6. **S (tx-per-proof) = 9 — OVERRIDDEN for model fidelity.** The image default is
   `LIGHTER_TX_PER_PROOF=4`, but the #95 sizing model and the tfvars' fleet-sizing
   comments are all built around **S=9** (the SLO-slack winner; k=ceil(500/9)=56
   chunks/block). To make the measured per-block service time directly comparable
   to the model, I added `--tx-per-proof 9` to BOTH the cell and coordinator
   commands in the resolved tfvars. This is the single config delta vs the committed
   tfvars and it is a measurement-fidelity improvement (it makes the live run match
   the model's S, so the data feeds `fleet-size.py --s 9` cleanly).
7. **Lag definition.** Primary measured lag = the coordinator's per-block
   `StreamSummary` BENCH_EVENT `lag_p50_ms`/`lag_p95_ms`, which is the
   block-pull→all-chunks-gathered wall (ADR-0004 lag(c,l) at the L1→L2 chunk
   granularity). I ALSO track dispatch-subscription `num_undelivered_messages`
   (backlog) over time as the keep-pace / unbounded-growth signal. Per-chunk
   `wall_ms`/`cpu_ms`/`witness_fetch_ms` come from the cells' `ChunkProven`
   BENCH_EVENTs. All scraped from `kubectl logs` (stdout `BENCH_EVENT <json>` lines).
8. **k=1 witness corpus.** Every cell mounts the same `bench_test.json` block
   (Assumption 4 of distributed-prover-runtime.md). Prove COST is real; witness
   CONTENT repeats. This does NOT invalidate contention/lag measurement (the real
   load is the CPU-bound prove + the real Pub/Sub round-trips).
9. **Coordinator L2→L4 merge is accounting-fold only** (Assumption 7 of the runtime
   doc): cells do REAL per-chunk L1+L2; the coordinator sums timings + emits per-block
   lag but does not recursively merge cell proofs. Scope boundary, not a defect —
   the cells carry the real per-chunk load/contention this benchmark measures.

## Real-infra findings discovered DURING the run (ground truth)

These are genuine constraints the live run surfaced — they are the valuable
output of actually running on real infra (vs. the idealized model).

- **FINDING A — Autopilot "Scale-Out" compute class = T2A (Ampere/neoverse-n1),
  NOT Axion.** The tfvars set `cell_compute_class="Scale-Out"` expecting Axion.
  Autopilot scheduled the arm64 pods on `t2a-standard-48` (Ampere Altra =
  neoverse-n1). The `-C target-cpu=neoverse-v2` binary **SIGILLs (exit code 132)**
  on neoverse-n1 — it emits v2-only instructions. Fix: pin Axion with
  `compute_class="Performance"` + a new `cloud.google.com/machine-family=c4a`
  nodeSelector (added to main.tf as `cell_machine_family`/`coordinator_machine_family`
  variables). On C4A (`c4a-standard-48`, Axion Gen-1) the SAME binary runs clean.
  This corrects a wrong assumption baked into the committed tfvars/docs
  ("Scale-Out selects Axion") and is the most important deploy-time finding.

- **FINDING B — Autopilot per-pod CPU ceiling is 43 vCPU, not 62.** The tfvars
  requested `cpu=62` (whole c4a-highcpu-64 minus reservation). GKE Warden rejected
  it: "Total cpu requested ... higher than the Autopilot maximum of '43'." Also the
  Scale-Out memory ratio forced memory to 248Gi (>172Gi max) from a 16Gi request.
  Fix: `cpu=43`, `memory=44Gi`. (On the Performance/C4A node Autopilot still set the
  effective memory limit to ~172Gi from the whole-node allocation; that is benign.)
  Net: a "whole-machine cell" on Autopilot is 43 vCPU schedulable, not 62 —
  the model's per-cell vCPU should use 43, slightly more cells for the same throughput.

- **FINDING C — C4A (Axion) STOCKOUT in us-central1 (all zones a/b/c/f).** After the
  FIRST c4a-standard-48 node came up, every subsequent C4A scale-up failed with
  `OutOfResource.RESOURCE_POOL_EXHAUSTED` / `STOCKOUT` across all 4 us-central1
  zones. This is a real-time CAPACITY stockout (NOT quota — C4A CPU quota was
  18/256 used). A throwaway `gcloud compute instances create c4a-standard-4` probe
  in **us-east4-a SUCCEEDED**, confirming Axion capacity exists there. **Decision:
  pivot the benchmark region from us-central1 → us-east4** to obtain real Axion
  capacity for the multi-node tiers. (The single us-central1 C4A node DID run the
  cell binary correctly first — the architecture/image is proven on real Axion;
  the pivot is purely about getting ENOUGH Axion nodes.)

- **FINDING D — The merged distributed prover (#173) fails 55 of 56 chunks per
  500-tx block: only the slice-0 chunk proves.**
  > **SUPERSEDED:** this finding was FIXED in #177 — per-tx positional pre-state
  > now lets ALL k chunks prove (evidence: `docs/layer0-evidence/finding-d-gate.md`;
  > design: `docs/per-tx-prestate-corpus.md`). The description below records the
  > pre-fix behaviour at build SHA ~`1d44b35`.
  >
  On a real k=56 block, the cells
  return `ok=true` for ONLY `witness_index ≡ 0 (mod pool_total)`; every other slice
  fails the circuit witness-consistency check:
  `Partition containing Wire(...) was set twice with different values`.
  Root cause: the cell's independent-chunk-prove path (bench.rs run_cell, ~line
  1606, documented as Assumption 7 of distributed-prover-runtime.md) seeds EVERY
  chunk from the BLOCK'S INITIAL STATE (`all_assets_before = block.all_assets`,
  initial roots, etc.) while selecting witness slice `witness_index % pool_total`.
  Only slice 0's pre-state equals the block initial state; slices 1..k-1 are
  mid-block and their witness data is inconsistent with an initial-state seed.
  The failed proves still run the full L1 witness generation (real CPU, ~2-5s each)
  before the assertion fails, so they DO contribute real cost to block lag — but the
  coordinator correctly reports `block_partial` (ok=1, collected=56). The ONE
  successful slice-0 prove is genuine and its cost matches the model: wall ~3.9-4.2s,
  cpu ~80-86s (≈20x parallelism on the 48-core Axion), peak RSS ~5.0 GB
  (model L1@S=9 = 3.051s; measured slightly higher on c4a-highcpu-48 vs the
  c4a-highcpu-64 calibration — consistent). **This is the headline correctness gap
  the live benchmark surfaced.** It means the merged distributed entrypoint can
  currently only prove 1 chunk per block end-to-end; the per-chunk independent
  seeding needs each slice's real pre-state (the multi-height corpus / per-slice
  witness seed, #72/#165) before a full 56-chunk block proves clean.

## Measurement strategy given FINDING D (documented decision)

Because the k=56 full-block path only proves slice-0 clean (FINDING D), I measured
the keep-pace / lag of the **WORKING prove path** by driving blocks with
`tx_count=9` → **k = ceil(9/9) = 1 chunk per block** (exactly the slice-0 chunk that
proves end-to-end). This measures the REAL Axion L1+L2 prove cost + the REAL Pub/Sub
coordinator↔cell round-trip contention (the G4 unknown) on the path that actually
completes (`block_complete`, ok=1/1). I ALSO publish a k=56 (tx_count=500) probe per
tier to record the full-block `block_partial` behaviour honestly. The drive RATE per
tier is still the model's per-tier mean (0.02216 / 0.03324 / 0.05540 blk/s); only the
per-block CHUNK COUNT is reduced to 1 so the proven path is exercised cleanly.
This is the honest, useful measurement: it characterizes real per-block coordinated
prove lag on Axion, which is what feeds the G4 contention coefficient.

## Per-tier results

(region = us-east4 after the FINDING C pivot; cluster c4a-highcpu-48 Axion Gen-1)

### Tier 0.2% (4 cells + 1 coordinator, c4a-highcpu-48 Axion)

- **Pods started:** YES — 4 cells + 1 coordinator all `1/1 Running` on real Axion
  (`c4a-highcpu-48`, 48 cores, neoverse-v2 binary, git_sha=1d44b35…, S=9). No SIGILL,
  no Pub/Sub auth errors (Workload Identity worked). Cells reached
  "circuits resident; entering chunk-prove loop"; coordinator subscribing.
- **k=56 full-block probe (tx_count=500):** `block_partial` — dispatched=56,
  collected=56, **ok=1** (only slice-0 proves; see FINDING D). block_wall=51.8s.
  The 55 failures are real CPU-consuming L1 attempts that fail the witness-consistency
  check. Honest result: the merged distributed prover completes only 1/56 chunks/block.
- **Working-path keep-pace (k=1, tx_count=9, 12 blocks at the tier mean cadence):**
  - All **12 blocks `block_complete`** (ok=1/1, dropped=0).
  - **Block lag: p50 = 5.35 s, p99 = 5.82 s** (min 5.12, max 5.82, mean 5.42 s).
  - Per-chunk (cell): wall p50 = **4.09 s**, cpu p50 = **79.5 CPU-s** (~19x parallel
    on 48 cores), witness_fetch p50 = **0 ms** (local-resolve floor, k=1 corpus),
    peak RSS = **5.46 GB** (model proxy was 5.27 GB — matches).
  - **SLO (ADR-0004): p50 5.35 s « 20 s, p99 5.82 s « 40 s → HELD with wide margin.**
  - **Coordination overhead = block lag − chunk wall ≈ 5.35 − 4.09 = ~1.26 s**
    (Pub/Sub chunk-publish + competing-pull + result-publish + 2 s-interval gather).
    This is the REAL G4 contention/coordination cost on the working path.
  - **KEEP-PACE VERDICT: KEPT PACE.** Lag stable across all 12 blocks (no unbounded
    growth); dispatch backlog drained to 0; observed throughput ≥ drive rate.

### Tier 0.3% (6 cells + 1 coordinator, c4a-highcpu-48 Axion)

- **Pods started:** YES — 6 cells + 1 coordinator all `1/1 Running` on real Axion
  (`c4a-highcpu-48`, 8 C4A nodes). No SIGILL, no Pub/Sub auth errors (WI worked).
- **Working-path keep-pace (k=1, 12 blocks at the tier mean cadence):**
  - All **12 blocks `block_complete`** (ok=1/1).
  - **Block lag: p50 = 5.88 s, p99 = 6.78 s** (min 5.23, max 6.78, mean 6.00 s).
  - Per-chunk (cell): wall p50 = **4.07 s**, cpu p50 = **79.3 CPU-s**, peak RSS = **5.29 GB**.
  - **SLO: p50 5.88 s « 20 s, p99 6.78 s « 40 s → HELD.**
  - Coordination overhead ≈ 5.88 − 4.07 = **~1.81 s** (up from 0.2%'s ~1.26 s — the
    single coordinator's serial per-block GATHER + 2 s poll cadence shows mild queueing
    as the drive rate rises; lags drift 5.23 s → 6.78 s within the window but do NOT run
    away).
  - **KEEP-PACE VERDICT: KEPT PACE.** Dispatch backlog drained to 0 after the run;
    observed throughput ≥ drive rate; SLO held with wide margin.
- **k=56 full-block path:** same FINDING D limitation applies (only slice-0 proves);
  not re-probed at this tier (the limitation is code-level, tier-independent).

### Tier 0.5% (9 cells + 1 coordinator, c4a-highcpu-48 Axion)

- **Pods started:** YES — 9 cells + 1 coordinator all `1/1 Running` on real Axion
  (10 × `c4a-highcpu-48` nodes). No SIGILL, no Pub/Sub auth errors (WI worked).
- **Working-path keep-pace (k=1, 14 blocks at the tier mean cadence):**
  - All **14 blocks `block_complete`** (ok=1/1).
  - **Block lag: p50 = 5.86 s, p99 = 6.60 s** (min 5.28, max 6.60, mean 5.89 s).
  - Per-chunk (cell): wall p50 = **4.05 s**, cpu p50 = **79.2 CPU-s**, peak RSS = **5.38 GB**.
  - **SLO: p50 5.86 s « 20 s, p99 6.60 s « 40 s → HELD.**
  - Coordination overhead ≈ 5.86 − 4.05 = **~1.81 s** (same band as 0.3%; the single
    coordinator is the shared serialization point across all tiers — cell count does NOT
    change per-block lag because each k=1 block is one chunk on one cell; the 9 cells give
    headroom, not lower single-block lag).
  - **KEEP-PACE VERDICT: KEPT PACE.** Dispatch backlog drained to 0; throughput ≥ drive
    rate; SLO held with wide margin.

### Cross-tier summary (working path, k=1)

| Tier | cells | drive mean (blk/s) | block lag p50 | block lag p99 | chunk wall p50 | coord overhead | keep-pace | SLO |
|---|---|---|---|---|---|---|---|---|
| 0.2% | 4 | 0.02216 | 5.35 s | 5.82 s | 4.09 s | ~1.26 s | KEPT | HELD |
| 0.3% | 6 | 0.03324 | 5.88 s | 6.78 s | 4.07 s | ~1.81 s | KEPT | HELD |
| 0.5% | 9 | 0.05540 | 5.86 s | 6.60 s | 4.05 s | ~1.81 s | KEPT | HELD |

**Per-chunk prove cost is FLAT across tiers (~4.05–4.09 s wall, ~79 CPU-s, ~5.3 GB RSS)** —
the cell prove is CPU-bound and tier-independent (expected). **Block lag is dominated by the
single coordinator's serial GATHER + 2 s poll cadence (~1.3–1.8 s overhead), NOT by cell
contention** on the k=1 path. All three tiers kept pace with the SLO held by a >3x margin.

## Refined G5 projection — real data vs the idealized model

### How the real data corrects the model

The single directly-measured constant the live run produced is the **real cell L1+L2 chunk
prove wall on the Axion shape Autopilot actually provisions** (`c4a-highcpu-48`, Axion Gen-1):
**~4.05 s p50**, vs the model's calibrated `L1+L2 = 3.051 + 0.303 = 3.354 s` on `c4a-highcpu-64`.
**Measured slowdown factor = 4.05 / 3.354 = 1.2075** (the deployment shape is 48 vCPU / Gen-1,
not the 64 vCPU calibration shape; this is a real G4-adjacent correction). The witness fetch is
0 ms (k=1 local-resolve floor, as documented). merge_s and L4 were NOT directly measured this
run (the coordinator does accounting-fold only — FINDING D / Assumption 9), so they are carried
as the same-silicon proxy (scaled by the measured 1.2075 factor) and flagged as such.

I rebuilt a measured calibration (`c4a-highcpu-48-axion-gen1-MEASURED`, transient — derived from
the live data, scaled L1=3684 ms, L2=366 ms, merge=0.3322 s, L4=3.5356 s, peak_rss=5464 MB) and
re-ran the #95 model (`scripts/fleet-size.py`) at the real steady load (11.08 blk/s, 500-tx cap, S=9).

### Idealized vs measured-Axion full-scale fleet (steady, 11.08 blk/s, 500-tx, S=9)

| Quantity | Idealized (#95, c4a-highcpu-64) | **Measured-Axion (c4a-highcpu-48 Gen-1, live)** | Δ |
|---|---|---|---|
| central-path block lag (k=1 LB) | 7.63 s | **9.21 s** | +1.58 s (+21%) |
| CELLS @ 11.08 blk/s | 1894 | **2286** | +392 (+20.7%) |
| COORDINATORS (concurrency=1) | 51 | **62** | +11 |
| SLO p50 slack | +12.37 s | **+10.79 s** | still FEASIBLE |

**The idealized 1894/51 was optimistic by ~21% on cells.** Correcting for the real Axion shape
the deployment lands on, the steady fleet is **~2286 cells + ~62 coordinators** (BY CLASS, never
summed). The SLO still holds on the central path with +10.8 s of slack.

### Coordinator-concurrency sensitivity (the #113 unknown the run informs)

The measured per-block lag is dominated by the **single coordinator's serial GATHER + 2 s poll
cadence** (~1.3–1.8 s coordination overhead, flat across the 4/6/9-cell tiers — cell count did
not change single-block lag). This strongly motivates per-coordinator concurrency >1. Model
sensitivity on the measured-Axion constants:

| coord concurrency | coordinators needed |
|---|---|
| 1 (conservative, current) | **62** |
| 3 | **21** |
| 5 | **13** |

If the coordinator can fold ~3–5 blocks concurrently (PROMISING-NOT-PROVEN, #113), the
coordinator pool shrinks ~3–5x to **13–21** — but this run did NOT prove concurrency >1 (the
coordinator loop is serial today), so the **conservative G5 number is 62 coordinators**.

### Two real costs the idealized model still OMITS (named, not invented)

1. **Pub/Sub coordination overhead ≈ 1.3–1.8 s/block** (chunk publish + competing-pull +
   result publish + 2 s gather poll). Measured directly here; the model's central path ignores
   transport. At k=56 this overhead is amortized differently (one SPLIT + GATHER per block, not
   per chunk), so 1.3–1.8 s is a reasonable per-block additive — well inside the +10.8 s slack.
2. **The k=56 full-block path does not yet complete** (FINDING D): only 1/56 chunks prove. So the
   measured per-block lag is the WORKING (k=1) path; the k=56 central-path lag (9.21 s modeled)
   is NOT yet end-to-end verified on real infra because 55/56 slices fail the witness check.
   **This is the single biggest caveat on the G5 confidence: the projection's per-block lag is
   modeled (corrected by the real per-chunk constant), not fully measured at k=56.**

### G5 confidence statement (honest)

- **HIGH confidence:** real per-chunk L1+L2 prove cost on Axion (4.05 s), real cell RSS (5.3 GB),
  real Pub/Sub coordination overhead (~1.3–1.8 s/block), real keep-pace at the 0.2/0.3/0.5% drive
  rates on the WORKING path, the +21% cell-count correction, and the Autopilot/Axion deployment
  realities (FINDINGS A/B/C).
- **MEDIUM confidence:** the corrected 2286/62 full-scale numbers (built from the real per-chunk
  constant + model formulas; merge/L4 are same-silicon proxies, not re-measured).
- **NOT YET CONFIRMED:** end-to-end k=56 block lag on real infra (blocked by FINDING D), and
  coordinator concurrency >1 (would shrink the 62 to 13–21).

## Final teardown verification (ground truth — actual gcloud output)

After all three tiers, verified ZERO orphaned lighter-prover resources:

```
GKE clusters (ALL)                         : midnight-dev-gke (us-central1) ONLY — untouched, not ours
Compute instances ~lighter-prover-scale    : NONE
Compute instances ~gke-lighter-prover      : NONE
Persistent disks ~lighter-prover-scale     : NONE
Pub/Sub topics ~lighter                    : NONE
Pub/Sub subscriptions ~lighter             : NONE
C4A capacity-probe instance                : deleted
```

Each tier was also independently destroy-verified (12 resources destroyed per tier: cluster +
3 topics + 3 subs + 2 deployments + PDB + HPA + monitoring IAM binding). The us-central1
stocked-out 0.2% cluster was also destroyed (7 resources) before the us-east4 pivot. The
unrelated `midnight-dev-gke` cluster was never touched.

**Persistent IAM intentionally left in place** (least-privilege, re-usable): the
`lighter-prover-pods` GSA + its 3 pubsub roles + the WI binding (documented in #171). To remove:
`gcloud iam service-accounts delete lighter-prover-pods@kunal-scratch.iam.gserviceaccount.com`.

## What was REAL vs what was NOT (honesty ledger)

- **REAL:** image built+pushed for the exact SHA; cluster/pods/Pub/Sub on live GKE Autopilot;
  pods on real Axion (C4A) nodes; genuine L1+L2 ZK proves on the cells (4.05 s wall, 79 CPU-s,
  5.3 GB RSS — no stubbing); real Pub/Sub coordinator↔cell round-trips; real per-block lag from
  the coordinator's own BENCH_EVENT stdout; real keep-pace (backlog drained) at all 3 tiers; real
  Autopilot/Axion constraints discovered (FINDINGS A–D); real teardown verified by gcloud.
- **NOT REAL / NOT CLAIMED:** the k=56 full-block path does NOT complete (only 1/56 chunks prove —
  FINDING D); the keep-pace lag is measured on the k=1 WORKING path, not k=56; merge_s/L4 were NOT
  re-measured (coordinator does accounting-fold only); witness CONTENT repeats (k=1 corpus, prove
  COST is real); coordinator concurrency >1 was NOT tested. None of these are papered over.
