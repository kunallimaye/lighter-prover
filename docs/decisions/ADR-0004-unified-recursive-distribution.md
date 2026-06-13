# ADR-0004: The unified recursive distribution primitive and the governing equation

**Status**: Proposed
**Date**: 2026-06-13
**Verified-at-tip**: `6b71f0d873179959e091e1bb88b75bb7b002dc8f` (fresh clone; every file:line and constant below re-verified at this SHA)
**Issues**: design capstone for the parallel-proof-generation story (#75); recasts #61 #101 #83 #95 as parameters; generalizes the k=1 bound flagged by #107; supersedes the record amended by #99
**Companion sessions (referenced, NOT duplicated)**: the #107 remediation (Tier-1 SLO-model relabel, items #19–22) and the #99 ADR-0003 amendment run in parallel; this ADR cites their outputs rather than restating them.

> **Numbering note (collision, proceed anyway).** `docs/decisions/` still contains **two** files numbered `ADR-0001` (`ADR-0001-gcp-fleet-bench-architecture.md` and `ADR-0001-container-topology.md`); resolving that collision is **#68** and remains open (PR #106 explicitly does not close it). `ADR-0002` is **reserved** for #10's `ADR-0002-l4-l8-driver.md` and is deliberately left free. The next un-taken number is therefore **0004** (0003 = prover-cell streaming architecture). This ADR takes 0004 and notes the unresolved #68 collision rather than waiting on it.

---

## 0. THE GOVERNING EQUATION (normative north star)

The system exists to satisfy one relation:

```
    lag_p50(c, l) ≤ 20 s    AND    lag_p99(c, l) ≤ 40 s,
    sustained at l ≥ 5 blocks/s
```

where

- **c** = infrastructure capacity — machines, *counted by class* (a worker
  cell is not a coordinator; never sum them as one pool).
- **l** = load — `(blocks/s, tx/block)`.
- **lag** = wall-clock time from **block-arrival-at-tip** to **proof-ready**.
  "Proof-ready" is the **block proof (L1→L4)**. Batch finalization
  (L5→L6, and the unmeasured L6→L8 BN254 wrap) is a **separate cadence**,
  stated in §5, never folded into the block-proof lag.

The proof-lag bound itself was **decided** in Discussion #77 (p50 ≤ 20 s,
p99 ≤ 40 s, scoped to L1→L4, measured from block arrival) and is the
activation gate behind #99/#97. This ADR does not re-decide it; it makes
it the function every other decision is measured against.

**The rule this ADR imposes on all future work.** Every future PR or
measurement MUST declare **which of `c`, `l`, `lag` it moves, and in which
direction.** Work that moves none of the three does not earn its place
(the Discussion #77 principle "a measurement only earns its compute if it
changes a decision," stated as a structural law).

**The former three outcomes are three readings of this one relation.**
"Speed," "capacity," and "cost" are not independent goals:

| Old framing | Reading of `lag(c, l)` |
|---|---|
| **Speed** | hold `c`, read `lag` at a given `l` |
| **Capacity / cost** | hold `lag` at the bound, solve for the minimum `c` given `l` (fleet sizing → #95) |
| **Headroom / peak** | push `l` to peak, verify the bound still holds |

§4 solves the equation all three ways.

---

## 1. Context

The deployable prover is the **prover cell** of ADR-0003 (one host, one
Rust process, orchestrator + M worker threads on resident proving keys).
ADR-0003 §D2:31 froze a sentence that the architecture has since
outgrown — *"in-process work queue drained by M workers … RAM-only;
nothing intra-cell crosses a network or GCS."* The lag bound forces a
single block's chunks to be split **across machines** ("always-split"):
at 20 s even a 500-tx block needs k ≈ 13+ cells (Discussion #77), and the
size-threshold hybrid dissolves into "always split wide" (#99 comment
thread; #97 activation gate).

The **#107 audit** (verified at this same tip) catalogs how pervasively
the repo still hard-codes the **k=1, one-block-one-cell** instance as if
it were the whole architecture — 26 items across ADR wording, code, the
SLO model, and operator docs. Its triage splits them into three tiers:

- **Tier 1 (live defect, #107 items #19–22):** the `single_machine_wall`
  term (renamed from `full_split_wall` by #108) in
  `scripts/s-calibrate-report.py:28-29` uses the *word* "always-split"
  but computes the whole split block's wall **from one machine's constants
  with no network / cross-cell term** — the opposite meaning. Those
  verdicts are baked into versioned `calibration/*.json` and asserted by
  CI, and **#95 will consume them.** **This ADR resolves the conceptual
  gap that Tier 1 flags:** §4 writes the cross-cell `lag(c, l)` that
  `single_machine_wall` is *not*, and names the existing single-machine
  formula as the **k=1 lower bound** of it. The Tier-1 *rename landed in
  **#108 (merged)*** — `single_machine_wall` now exists in the
  `calibration/*.json` data and in `scripts/s-calibrate-report.py`. This
  ADR is the home of the cross-cell wall that `single_machine_wall`
  lower-bounds (it is that wall's k=1 lower bound).
- **Tier 2 (amend the record):** ADR-0003 §D1/§D2/§D6 wording — folded
  into **#99's** authorized in-place amendment, which **landed in #109 +
  #111 (both merged)** (NOT re-litigated here).
- **Tier 3 (not defects, #75 scope):** the in-process engine + the
  known-unbuilt coordinator plumbing. The merged circuit work (PR #69 L2
  tree-fold; #82 pre-L5 merge tree, build-validated as `BatchMergeCircuit`;
  PR #92 intra-cell tree scheduler) composes cross-cell **by design** and
  **must not be removed.**

ADR-0003 D5 already established the deep structural fact this ADR
generalizes: **folding is associative**, so block→chunk and batch→block
are the *same* recursive shape at two grains. This ADR names that shape
once and writes the lag function over it.

---

## 2. Decision: ONE recursive primitive, TWO grains

We define a single distribution-and-folding primitive:

```
    SPLIT → DISPATCH → PROVE → GATHER → FOLD
```

- **SPLIT** — partition a unit of work into independent sub-units.
- **DISPATCH** — route sub-units to provers.
- **PROVE** — produce a recursive proof per sub-unit (embarrassingly parallel).
- **GATHER** — collect sub-unit proofs.
- **FOLD** — compose them into one proof via a same-shape merge tree
  (associative; log-depth critical path).

It is instantiated at **two grains**:

| | **Chunk grain** | **Block grain** |
|---|---|---|
| SPLIT | a BLOCK → chunks (`ceil(tx/S)`) | a BATCH → blocks |
| PROVE | chunk proof (L1) | block proof (L1→L4, itself the chunk-grain output) |
| FOLD | L2 merge tree + L4 (on a coordinator) | L5 segment folds + L6 |
| Merge circuit | `BlockTxChainMergeCircuit` (PR #69; `circuit/src/block_tx_chain_merge_constraints.rs`) | `BatchMergeCircuit` (#82; `circuit/src/recursion/batch_merge_constraints.rs:98`) |

**Principle: UNIFIED unless measured constants force divergence.** The
#107 audit is the cautionary tale — accidental divergence (treating the
k=1 chunk-grain instance as the architecture) is exactly the cost we are
paying down. Every divergence below is therefore evidence-backed; the
default is one primitive, one set of rules, both grains.

### 2.1 Parameter table (UNIFIED / DIVERGENT)

For each design parameter, the primitive is **UNIFIED** across grains
unless a measured constant forces **DIVERGENT**; divergences cite the
measurement.

| Parameter | Verdict | Basis |
|---|---|---|
| **Witness / work movement** | **DIVERGENT** | Chunk grain: witness is small → dispatch the **witness**, prove locally. Block grain: the "witness" of a block-grain fold is **a chunk/block proof** (159,740 B = 156.0 KiB raw; Discussion #97 spike, EPYC 7B13) → move the **proof**. The mounted read-only corpus by `{height, witness_index}` is the chunk-grain witness plane (ADR-0003 §D6 / #61). **UNMODELED caveat:** the corpus + `witness_fetch_ms` field are **ADR-design, not implemented** at this tip (witnesses load from `bench_test.json`; no `{height, witness_index}` resolver, no `witness_fetch_ms` in `bench/src/events.rs`). See §4 `witness_move` = UNMODELED. |
| **Dispatch: push vs pull** | **DIVERGENT** | Block grain (BATCH→blocks): **pull** — competing-pull Pub/Sub, `maxOutstandingMessages=1`, ack after proof (ADR-0003 §D2; #75). Chunk grain (BLOCK→chunks under always-split): **push** from a coordinator — the coordinator owns the block's chunk set and fans out to k cells (small M, large k; #97 M-vs-k resolution). Reason: pull-balancing is the right scheduler for whole independent blocks; a single block's k chunks need a coordinator that knows the fan-in target. |
| **Straggler policy** | **UNIFIED** | A FOLD round completes at the **max** over its inputs at both grains; max-of-N statistics drive p99 identically. One mechanism (hedged dispatch / speculative tail) serves both. Sizing differs only by N. Design owner: **#101** (§6). |
| **Failure / redelivery unit** | **DIVERGENT** | Block grain: **whole block** (cell death → one block redelivered; ADR-0003 §D2). Chunk grain: a failed **chunk** is re-dispatched by the coordinator (sub-block redelivery), because under always-split the cell no longer owns the whole block. Reason: the redelivery unit follows the dispatch owner. |
| **Fold location** | **UNIFIED (one role: coordinator)** | Both grains fold on a **compute node, not a dispatcher**: chunk grain folds the L2 merge tree + L4; block grain folds L5 segments + L6. Same role, same "a coordinator is a machine class in `c`" consequence (§3). |
| **Proof transport** | **UNIFIED** | Recursive proofs serialize the same way at both grains: `to_bytes` (156.0 KiB raw leaf; 0.12/0.23/5.1 ms write/read/verify, Discussion #97, EPYC 7B13), build-free verify with ~2 KB pinned key material (`CommonCircuitData` 1,453 B + `VerifierOnlyCircuitData` 552 B). Transport ≈ **0.020%** of prove (1.28 ms @ 1 Gbps vs a 6.4 s c4a-highcpu-64 leaf; #97). One wire envelope, one rule: **carry a circuit-shape/version fingerprint** (deserialization is shape-driven, not self-validating — #58 norm "deserialization is not validation"). |

**Reading of the table.** Four of six parameters are unified; the two
"DIVERGENT" dispatch/redelivery rows diverge for one reason (who owns the
unit — a self-contained block vs a coordinator-owned chunk set), and the
witness-movement row diverges because the *thing moved* differs in size
by grain. None of the divergences is accidental; each is a measured or
structural fact. This is the antidote to the #107 failure mode.

---

## 3. The lag function `lag(c, l)` — the heart of the document

`lag` is a **distribution, not a scalar.** We compose its central path
from measured per-level constants, then characterize its tail.

**Machine discipline.** All constants below are tagged to a machine and
**never mixed**. The deployment candidate is **`c4a-highcpu-64`** (Axion,
neoverse-v2); its constants are the **measured** registry entry
`calibration/c4a-highcpu-64.json` (`measured_at_sha e87152b`, circuit hash
`f634a649afd2`, run-id `20260613-011326-i73y5n`, PR #105). Where a number
exists only on **AMD EPYC 7B13** (32c), it is labeled as such and not
transplanted onto c4a.

### 3.1 Per-block lag (chunk grain)

```
per_block_lag ≈ witness_move
              + max_over_chunks(chunk_prove)            (L1, parallel across k cells)
              + ceil(log2(chunks)) · merge_step         (L2 tree fold)
              + L4                                       (block prove; serial, on coordinator)
```

Block proofs then compose into the batch by **the same primitive** at the
block grain (L5 segments + L6); that cadence is §5, deliberately separate.

### 3.2 The measured constants (c4a-highcpu-64 unless noted)

| Term | Value | Label | Source (verified at tip) |
|---|---|---|---|
| `witness_move` | **UNMODELED** | — | No `witness_fetch_ms`, no mounted-corpus resolver in code; witnesses load from `bench_test.json` (`bench/src/bin/bench.rs:3007`). To measure: implement #61's corpus + `witness_fetch_ms` BENCH_EVENT field, then read it directly. Until then this term is **unknown — do not invent a number.** |
| `chunk_prove` (L1, S=9) | **3.051 s** | measured | `calibration/c4a-highcpu-64.json` per-S table, S=9 `l1_wall_ms=3051` (n=3, stdev 20.7 ms). S=9 is the SLO-slack winner on every shape (#102/#105 ladder). |
| `merge_step` (L2/MERGE_S) | **0.2751 s** | measured | `calibration/c4a-highcpu-64.json` `constants.merge_s=0.2751`. |
| `L4` (block prove) | **2.928 s** | measured | `calibration/c4a-highcpu-64.json` `constants.l4_wall_s=2.928`. **Dominant term — see §4.3.** Per-shape range **2.789 s (-72) … 16.087 s (-4)** across the ladder. |
| chunk proof wire size | **156.0 KiB** (159,740 B raw) | measured | Discussion #97 spike, **EPYC 7B13** (leaf `BlockTxChainCircuit`, 2^14, 1,613 PIs). Transport derived: 1.28 ms @ 1 Gbps = **0.020%** of a 6.4 s c4a-64 leaf. |

**L4-per-shape (the dominant-term spread), measured, c4a ladder (#105):**

| shape | L4_wall (s) | merge_s (s) | SLO verdict @ S=9 |
|---|---:|---:|:--|
| c4a-highcpu-4 | 16.087 | 1.563 | INFEASIBLE (slack −29.18 s) |
| c4a-highcpu-8 | 9.033 | 0.869 | INFEASIBLE (slack −7.37 s) |
| c4a-highcpu-16 | 5.639 | 0.527 | FEASIBLE (slack +3.09 s) ← min-viable worker |
| c4a-highcpu-32 | 3.911 | 0.364 | FEASIBLE (slack +8.33 s) |
| **c4a-highcpu-64** | **2.928** | **0.275** | **FEASIBLE (slack +11.27 s)** ← deployment candidate |
| c4a-highcpu-72 | 2.789 | 0.263 | FEASIBLE (slack +11.67 s) |

SLO-feasibility knee: **between 8 and 16 vCPU.** Parallel-efficiency knee:
**16 vCPU** (91→80→73→68% across 4→8→16→32→64).

### 3.3 The central-path value (c4a-highcpu-64, S=9, the typical path → p50)

```
witness_move        = UNMODELED  (omitted from the sum; see caveat below)
max_over_chunks(L1) ≈ 3.051 s    (k cells in parallel; max ≈ one chunk's prove)
log2-merge term     ≈ ceil(log2(k)) · 0.2751 s
L4                  = 2.928 s
```

For a 9,000-tx block at S=9, k = ceil(9000/9) = 1,000 chunks → merge depth
`ceil(log2(1000)) = 10` → merge term ≈ 2.751 s, giving a central path of
**3.051 + 2.751 + 2.928 ≈ 8.73 s.** This reproduces, to the millisecond,
the committed `single_machine_wall_9000 = 8.730` and `slo_slack_min = 11.270`
in `calibration/c4a-highcpu-64.json` (S=9 row).

> **This is exactly the #107 Tier-1 quantity, correctly named.** The
> committed `single_machine_wall(S, B)` (the formula lives in
> `scripts/s-calibrate-report.py`, docstring lines 28–29 at this tip after
> the #108 rename) is `L1_chunk_wall + ceil(log2(B/S))·MERGE_S + L4_WALL`
> — **one machine's constants, no network / cross-cell term.** It is a
> **legitimate number:
> the k=1 LOWER BOUND** of `lag(c, l)` — the wall a *single* machine would
> hit if it could hold all k chunks (the perfect-parallel, zero-transport,
> zero-straggler floor). `lag(c, l)` is that floor **plus** `witness_move`
> (UNMODELED), **plus** real cross-cell transport (≈0.02% of prove, #97 —
> negligible but real), **plus** the straggler/recovery tail (§3.4). The
> #107 Tier-1 PR (#108, merged) renamed `full_split_wall` to
> `single_machine_wall`, the single-machine lower bound; **this ADR is the
> home of the cross-cell wall itself.**

### 3.4 The tail (p99) — model lag as a distribution

p50 is the central path above. **p99 is governed by two tail sources:**

1. **Straggler tail (#101).** A FOLD round completes at the **max** over k
   chunk proves. Under always-split k reaches ~1,000 (9,000-tx block, S=9)
   or ~900 (Discussion #77 sizing); max-of-N pushes p99 well above p50 even
   with a tight per-chunk distribution. The **median slack is the entire
   straggler budget**: at c4a-highcpu-64, S=9, 9,000-tx, slack ≈ 11.27 s
   under the 20 s p50 bound, ≈ 31.27 s under the 40 s p99 bound. The
   mechanism (hedged dispatch / speculative last-decile / work-stealing) is
   **#101's** design; its p99 arithmetic needs the measured per-chunk
   variance (S=9 L1 stdev 20.7 ms, n=3 — **thin; flag as needing a wider
   sample** before p99 is trusted).
2. **Coordinator-failure recovery.** A coordinator death mid-fold costs a
   re-gather + re-fold of the in-flight block (sub-block redelivery, §2.1).
   This is bounded by one block's fold cost (merge term + L4 ≈ 5.7 s at
   k=1000) **plus** re-dispatch latency. **Recovery latency is UNMODELED**
   (no coordinator exists yet — #75); name it, do not invent it.

p99 = p50 + straggler-max-tail (#101, partially measurable now) +
coordinator-recovery (UNMODELED until #75 plumbing exists).

---

## 4. Solving the equation three ways

### 4.1 Hold `lag` at the bound, solve for `c` given `l` → the fleet-sizing model (hand to #95)

Throughput is **Little's law** over the cell pool, independent of the
per-block lag: at `l = 5 blocks/s × 500 tx` (and up to the 9,000-tx case),
the fleet is sized by `cells ≈ arrival_rate × service_time / concurrency`.
Discussion #77 / #97 re-derive **~800–900 c4a-highcpu-64-equivalent worker
cells** at peak (Little's law, ~801 re-derived; ~900 with headroom). The
**minimum-viable worker class is c4a-highcpu-16** (first FEASIBLE rung;
~$1.17 / 1000 leaf-proofs); c4a-highcpu-8 is leaf-cheap ($0.94/1000) but
whole-block **INFEASIBLE** (slack −7.4 s) — capacity-per-dollar is a
function of the *block* wall, not the *leaf* wall.

This is the **input** to **#95**, not its replacement: #95 turns
`lag(c, l) = bound` into `c = f(blocks/s, tx/block, S, M, segments)` and
emits the cell/RAM/segment-folder topology. **This ADR hands #95 the
function; #95 owns the solver.** (The fleet count is a *worker-cell* count;
coordinators are a **separate class in `c`** — §4.3.)

### 4.2 Hold `c`, read `lag`

For the deployment candidate `c4a-highcpu-64` at S=9 (measured, §3.3):

| block size | k (chunks) | central-path lag (L1→L4) | p50 slack vs 20 s |
|---|---:|---:|---:|
| 500 tx | 56 | 7.630 s | +12.37 s |
| 4,000 tx | 445 | 8.455 s | +11.55 s |
| 9,000 tx | 1,000 | 8.730 s | +11.27 s |

(All three rows are the committed `single_machine_wall_*` values in the S=9 row
of `calibration/c4a-highcpu-64.json` — the k=1 lower bound; the real
cross-cell `lag` is these **plus** UNMODELED `witness_move` and the §3.4
tail.) The **block-size-independence** of the floor (~8–9 s across a 18×
range of tx/block) is the headline property of always-split: lag is set by
`L1 + log-depth-merge + L4`, not by block size.

### 4.3 Push `l` to peak, verify the bound holds

At `l = 5 blocks/s` and the worst block size (9,000 tx → k=1,000), the
central path is **8.73 s** vs the **20 s** p50 bound — **11.27 s of
slack**, the entire straggler + witness_move + recovery budget. **Verdict:
the bound holds on the central path with wide margin on c4a-highcpu-64.**
The risk is **not** the central path; it is whether the §3.4 tail
(straggler max-of-1000 + UNMODELED coordinator recovery) consumes the
11.27 s p50 / 31.27 s p99 slack. That is #101's question, answerable once
per-chunk variance is sampled wider than n=3.

---

## 5. Batch finalization (L5→L6) — a separate cadence, by the same primitive

Block proofs compose into the batch at the **block grain** of the same
SPLIT→…→FOLD primitive:

```
batch_finalization ≈ ceil(log2(blocks_per_batch)) · L5_merge_step       (segment fold tree)
                   + L5_fold (chained)                                   (per the hybrid, ADR-0003 D5)
                   + L6                                                  (wrapper; see UNMODELED gate)
```

Measured constants (**AMD EPYC 7B13** — the only machine these exist on;
**do not transplant onto c4a**):

| Term | Value | Label | Source |
|---|---|---|---|
| L5 chained fold | **1.225 s** | measured | Discussion #77 BENCH-LEDGER, `chained_fold_median` (9 samples, 1106–1661 ms, `not_first_recursion`). **Use this, NOT the 0.94 s base-case datum** (#10 Stage-A 0.94 s was first-recursion only; #95 and #77 both revise it). |
| L5 merge step | **0.993 s** | measured | Discussion #77 BENCH-LEDGER `merge_step` median (1001/994/983 ms); ≈1× the fold step. |
| L5 8-way concurrent fold | ~5.7 s/fold (≈4.75× solo) | measured, load-polluted | Discussion #77 / PR #98 early-contention signal; single-host Path-A 8-way ⇒ ~714 ms/block = **3.2× over** the 226 ms batch cadence. Capacity AT-RISK at the batch grain. |
| L6 verifying prove | **UNMODELED** | — | No verifying L6 inner-wrapper proof exists in-repo (#83): `WrapperCircuit::prove_inner` needs `WrapperInput` KZG sidecar, `delta_chain_proof`, `blob_evaluation_proof` — none produced anywhere. **Bounds every end-to-end batch-proof demonstration.** To measure: #83. |

**Why this is a separate cadence, stated explicitly.** Block-proof lag
(§3) is measured from block arrival and must hold p50 ≤ 20 s **per block**
at ≥5 blocks/s. Batch finalization runs **once per batch** (many blocks),
is off the per-block critical path (ADR-0003 §D5), and terminates in the
UNMODELED L6 gate. Folding the two would both (a) double-count L5 against
the block bound it does not gate, and (b) import an UNMODELED term into a
modeled one. They are reported separately; the governing equation's `lag`
is the **block-proof** quantity.

---

## 6. Derived consequences the equation forces

These are consequences of the arithmetic, surfaced — not opinions.

### 6.1 L4 dominance (the next structural lever)

On c4a-highcpu-64, the central path is `3.051 (L1) + 2.751 (merge,
k=1000) + 2.928 (L4)`. **L4 is the largest single term, it is serial per
block, and it is untouched by either distribution grain** — splitting a
block into more chunks (more k) shrinks L1's `max_over_chunks` and adds at
most log-depth to the merge term, but **does nothing to L4.** Across the
ladder L4 is **34–38% of the lag floor** (Discussion #77 capstone audit)
and ranges **2.789 s … 16.087 s** by machine class.

**Consequence (forced by the math, not chosen):** after the coordinator is
built (#75), **the next structural lever is reducing L4.** This ADR does
**not** design the L4 fix; it names it as the **derived next question** —
the only term left that moving `c` (faster machine class) or `l` (block
size) cannot help, and that the distribution primitive cannot parallelize.
L4 reduction has **no issue yet**; filing one is the recommended follow-up.
Per the L4-streaming spike (below), that L4 work — *when* triggered — is
**circuit surgery on `BlockCircuit`'s two verify subgraphs**, not streaming
or distribution; so the follow-up should be filed as a **GATED / parked
design issue (do-not-start-until-triggered)**, not an active workstream.

**L4 streaming is structurally blocked (spike, 2026-06-13).** A dedicated
spike (workspace `pilot-l4-streaming-spike-69a42a1a`, tip `f935af4`) asked
whether L4 can be streamed, distributed, or otherwise hidden. It settled
the streaming question and parked the structural-lever question:

- **Streaming L4 is impossible.** Beginning L4 before its input fold
  completes cannot be done: `BlockCircuit` must verify the **COMPLETE
  folded chain proof** — a hard data dependency (same class as the L5
  keccak-chain block), **not** an engineering gap. So L4 reduction, if ever
  triggered, means **circuit surgery** — decomposing `BlockCircuit`'s two
  verify subgraphs — not streaming and not distribution.
- **The surviving levers don't touch the tail.** Cross-block pipelining
  (hide L4 behind the next block's chunk-proving) and fast-shape
  coordinators remove L4 from **steady-state** lag but **not** from
  **single-block / cold-start / burst-tail** lag. Those three cases leave
  an **irreducible floor**: `tail_lag ≥ wall(L3) + wall(L4)` on the
  coordinator's shape.
- **This floor binds the p99 tail specifically.** The cold-start /
  burst-tail cases **are** the p99 cases — already the weakest half of the
  governing equation (§0), so the L4 floor lands exactly where there is
  least slack.
- **PRE-COMMITTED TRIGGER (recorded so it is unambiguous).** If `lag_p99`
  must be tighter than `wall(L3) + wall(L4)` on the best attainable
  coordinator shape, then either **(i)** run a dedicated L4-circuit spike
  to split `BlockCircuit`'s two verify subgraphs, or **(ii)** renegotiate
  the lag bound. The per-shape L4 floor for the trigger **is the §3.2
  L4-per-shape table** (2.789 s on c4a-highcpu-72 … 16.087 s on
  c4a-highcpu-4); coordinator-shape choice is therefore a
  **floor-setting decision, not merely a preference.**
- **Recommended approach: (b) pipelining + fast-shape coordinator**, with
  the pre-committed trigger above held in reserve for circuit surgery if
  the SLO proves tighter than the floor allows on attainable hardware.
- **Note (open).** Two further structural spikes are in flight — *is L4
  necessarily per-block?* and *can any L4 work move to chunk level?* If
  either finds an opening, §6.1 gets a follow-up amendment. **This ADR
  ships with the streaming verdict settled; the structural-lever question
  remains open.**

### 6.2 The coordinator is a compute node, not a dispatcher — size it in `c`

The coordinator folds the L2 merge tree (`ceil(log2(k))·0.2751 s`) **plus**
L4 (2.928 s) — at k=1,000 that is ≈ **5.7 s of real proving per block**,
roughly a block's worth of work. It holds the merge + L4 proving keys
resident. Therefore:

- The coordinator is a **distinct machine class in `c`** — never summed
  into the worker-cell count. #95's fleet model must size **two** pools.
- "No coordinator service exists" (ADR-0003 §D2) describes the k=1 past;
  under always-split the coordinator is mandatory and is a prover. (This
  is the #99 §D2 amendment's "block→cell binding becomes the k=1 special
  case," generalized: the coordinator is what k>1 requires.)

---

## 7. Open issues recast as parameters (dependency order)

Each open workstream is **an input the equation needs**, not an
independent project. Recast:

| Issue | Term it supplies | Status of the term |
|---|---|---|
| **#61** | `witness_move` | **UNMODELED** — supplies the only unmodeled term in §3.1; implement corpus + `witness_fetch_ms` to measure it. |
| **#101** | p99 straggler tail (§3.4) | partially measurable now (needs wider per-chunk variance than n=3). |
| **#83** | L5/L6 batch-finalization tail (§5) | **UNMODELED** L6 gate; bounds end-to-end batch demonstration. |
| **#95** | consumes the solved equation (§4.1) | ready once `c`-by-class (§6.2) and `witness_move` (#61) land. |
| **#107 Tier 1** | the **k=1 lower bound** this ADR generalizes (§3.3) | renamed (#108, merged); this ADR is the cross-cell wall it lower-bounds. |
| **#99 / Tier 2** | the ADR-0003 record this ADR supersedes (§1) | amendment landed in-place (#109 + #111, both merged; not a new ADR). |
| **#75** | the coordinator + cross-cell plumbing that *executes* `lag(c,l)` | design-gated; consumes §2 primitive + §6.2 sizing. |

**Resulting dependency order** (what unblocks what):

```
#99 (amend record; permit chunk-grain crossing)              ── record gate
   └─► #107 Tier 1 (renamed full_split_wall→single_machine_wall = k=1 lower bound; #108 merged) ── name gate
          └─► #61 (witness_move term)        ─┐
          └─► #101 (p99 straggler tail)       ├─► #95 (solve lag=bound → c)
          └─► §6.2 coordinator-as-class       ─┘        │
                                                        └─► #75 (build coordinator + cross-cell)
   #83 (L6 gate) ── parallel track; gates batch-finalization (§5), NOT block-proof lag
   [NEW] L4 reduction (§6.1) ── derived next lever; no issue yet; file one.
```

---

## 8. Honesty ledger (Discussion #58 norms)

- Every constant is tagged to a machine; **c4a-highcpu-64** (deployment
  candidate) and **AMD EPYC 7B13** numbers are **never mixed**.
- **Measured vs extrapolated** is labeled at every term. The §3 central
  path is fully measured on c4a-highcpu-64 at this tip. §5 L5/L6 numbers
  are EPYC-only and labeled so.
- **UNMODELED terms are named, not invented:** `witness_move` (#61), L6
  verifying prove (#83), coordinator-failure recovery latency (#75). Each
  carries "what would measure it."
- **156 KiB / 0.02% transport** is measured file-serde (EPYC, #97) with
  network transport **derived** from wire math — labeled as such.
- **Design only.** No code is built or changed by this ADR.

## 9. Consequences

**Positive.** One primitive governs both grains; the parameter table makes
every grain-specific rule defend itself with evidence; `lag(c, l)` is a
concrete, reproducible function (matches `calibration/*.json` to the ms);
the three readings give #95 its solver, give #101 its budget, and surface
L4 as the forced next lever.

**Negative / open.** Three terms are UNMODELED (`witness_move`, L6, coord
recovery); p99 rests on an n=3 variance sample; the coordinator that
*executes* this model is unbuilt (#75). The ADR is a target, not a
measurement of the running system — the central path is measured, the
tail and the witness term are not.

**Supersession.** This ADR supersedes the k=1 framing in ADR-0003 §D2/§D6
(the one-block-one-cell assumption) and the record #99 amended (landed via
#109 + #111, both merged); it does
**not** supersede ADR-0003's measured constants or D5 hybrid, which it
builds on. ADR-0003 remains authoritative for the cell engine; this ADR is
authoritative for the cross-cell distribution model and the governing
equation.
