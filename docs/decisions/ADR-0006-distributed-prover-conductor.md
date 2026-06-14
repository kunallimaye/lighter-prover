# ADR-0006: The distributed-prover conductor — operational distribution design

**Status**: Proposed
**Date**: 2026-06-13
**Verified-at-tip**: `1211ffc2155f97cab3f877e8add93989bed03cc1`
**Issues**: refs #75 #113 #61 #101 #95 #82 (operationalizes ADR-0004; consumes ADR-0003 §D2/§D6/§D7)

> **Scope banner.** **Design-only; build nothing.** This ADR designs the
> *operational form* of the distribution primitive; **ADR-0004 supplies the
> model it executes** (the governing equation and `lag(c, l)`). No code is
> written, no container is built, nothing is provisioned, no benchmark is
> run by this ADR.

> **Numbering note.** This ADR takes **0006** — the next free number.
> `ADR-0005` was claimed by the **L6-inner-wrapper/KZG-sidecar** doc
> (`ADR-0005-l6-inner-wrapper-kzg-sidecar.md`) while this conductor design sat
> open as PR #115; to avoid a duplicate `ADR-0005`, the conductor was
> renumbered to **0006** at merge. `docs/decisions/` still contains **two**
> files numbered `ADR-0001` (`ADR-0001-gcp-fleet-bench-architecture.md` and
> `ADR-0001-container-topology.md`); resolving that collision is **#68** and
> remains **open** (PR #106 explicitly does not close it). This ADR is **not
> blocked on** #68. `ADR-0002` is **reserved** for #10's
> `ADR-0002-l4-l8-driver.md` and is deliberately left free — **not taken
> here**. (Existing: 0003 = prover-cell streaming architecture; 0004 =
> unified recursive distribution primitive + governing equation; 0005 =
> L6 inner-wrapper KZG sidecar.)

---

## 0. What this ADR is, and what it is not

ADR-0004 answered *what the system must satisfy* (the governing equation)
and *what shape the work has* (one recursive primitive, two grains, a
measured `lag(c, l)`). It deliberately stopped at the model. **This ADR
designs the conductor** — the concrete operational arrangement of machines,
queues, dispatch, witness resolution, and fleet substrate that *executes*
that model.

**The governing equation (ADR-0004 §0, quoted verbatim — the north star):**

```
    lag_p50(c, l) ≤ 20 s    AND    lag_p99(c, l) ≤ 40 s,
    sustained at l ≥ 5 blocks/s
```

where `c` = machines counted **by class**, `l = (blocks/s, tx/block)`, and
`lag` = block-arrival-at-tip → proof-ready (the **L1→L4 block proof**).

**The rule ADR-0004 imposes, honored here.** Every design choice below
states **which of `c` (capacity) / `l` (load) / `lag` it serves.** A choice
that moves none of the three does not earn its place.

**Machine discipline (Discussion #58 honesty norm, carried from ADR-0004
§3).** Every numeric constant is tagged to a machine and **never mixed**.
The deployment candidate is **`c4a-highcpu-64`** (Axion, neoverse-v2). The
only AMD **EPYC 7B13** numbers that appear (the 156.0 KiB chunk-proof wire
size; the L5/L6 batch-finalization constants) are labeled as such and
**never transplanted onto c4a**. UNMODELED terms are **named, not invented**.

---

## 1. The two-tier dispatch

**Question answered:** how does one block's chunks, and the stream of
blocks, actually flow across machines? **Serves:** `lag` + capacity(`c`).

The conductor instantiates the single ADR-0004 §2 primitive —
`SPLIT → DISPATCH → PROVE → GATHER → FOLD` — at **two grains**. ADR-0004
§2.1's parameter table is the authority for which rules are UNIFIED vs
DIVERGENT; the table below renders the two tiers concretely.

### 1.1 Outer tier — BATCH → blocks → coordinator pool

| Step | Operational form | ADR ref |
|---|---|---|
| SPLIT | the feeder publishes one **block event** per arriving block | ADR-0003 §D2 |
| DISPATCH | **pull** — competing-pull Pub/Sub, one subscription, `maxOutstandingMessages=1`, **ack after the block proof is emitted** | ADR-0004 §2.1 dispatch row (block grain = pull); ADR-0003 §D2 |
| PROVE | a **coordinator** drives the inner tier (§1.2) to produce the L1→L4 block proof | ADR-0004 §2 (block-grain PROVE = the chunk-grain output) |
| GATHER | the Pub/Sub layer balances blocks across the coordinator pool | ADR-0003 §D2 pull-balancing |
| FOLD | (batch grain, separate cadence) L5 segments + L6 — ADR-0004 §5, **not** on the per-block path | ADR-0004 §5 |
| **Failure / redelivery unit** | **whole block** — a coordinator death → one block redelivered | ADR-0004 §2.1 DIVERGENT redelivery row; ADR-0003 §D2 |

The outer tier is **the existing §D2 competing-pull block-dispatch layer**.
The coordinators are a **second consumer class** on that same layer — **not
new machinery** (#113). This is ADR-0003 §D2's pull-balancing scheduler,
reused unchanged; the only change is *who pulls* (a coordinator, which is a
prover, not the k=1 cell that owned a whole block in the superseded model).

### 1.2 Inner tier — one BLOCK → chunks → cells

| Step | Operational form | ADR ref |
|---|---|---|
| SPLIT | the coordinator partitions its block into `k = ceil(tx/S)` chunks (k up to ~1000 for a 9000-tx block at S=9) | ADR-0004 §2 (chunk grain); ADR-0003 §D2 amendment |
| DISPATCH | **push** — the coordinator **owns its block's chunk set and fans out** to k cells (small M, large k) | ADR-0004 §2.1 dispatch row (chunk grain = push) |
| PROVE | each cell proves its chunk(s) (L1, embarrassingly parallel across k cells) | ADR-0004 §2 |
| GATHER | chunk proofs travel **over the wire** back to the coordinator (leaf-grain transport ≈ 0.020% of prove — ADR-0004 §2.1 transport row) | ADR-0004 §2.1; ADR-0003 §D2 amendment |
| FOLD | the **coordinator** folds the L2 merge tree + proves L4 **locally** (merges stay co-located — merge-grain transport tax, ADR-0003 §D6 reaffirmed) | ADR-0004 §2.1 fold-location row (UNIFIED: coordinator) |
| **Failure / redelivery unit** | a failed **chunk**, re-dispatched by the coordinator (sub-block redelivery) | ADR-0004 §2.1 DIVERGENT redelivery row |

The redelivery unit follows the dispatch owner (ADR-0004 §2.1): the outer
tier redelivers a **whole block** because Pub/Sub owns the block; the inner
tier redelivers a **single chunk** because the coordinator owns the chunk
set.

### 1.3 The wire fingerprint (UNIFIED, both tiers)

Every proof on the wire carries a **circuit-shape/version fingerprint**
(ADR-0004 §2.1 transport row, UNIFIED). Deserialization is shape-driven and
**not self-validating** — a mismatched or tampered proof parses fine and
only fails at verify, so the fingerprint is the one cheap line of defense
("deserialization is not validation", Discussion #58 norm). This is one wire
envelope, one rule, both grains.

---

## 2. The coordinator pool (#113)

**Question answered:** how does L4/fold throughput keep up at sustained load
without a single-coordinator bottleneck or SPOF? **Serves:** capacity(`c`)
at sustained load(`l`).

A **pool of dedicated coordinators**, each of which folds one block's L2
merge tree (`ceil(log2(k)) · 0.2751 s`) **plus** proves L4 (2.928 s) — at
k=1000 that is **≈ 5.7 s of real proving per block** (`c4a-highcpu-64`;
ADR-0004 §6.2). A coordinator is therefore a **compute node, not a
dispatcher** (ADR-0004 §6.2); it holds the merge + L4 proving keys resident.

**Why a pool works (proven property).** Coordinator work (fold + L4) is
**stateless and independent per block** (ADR-0004 §6.1, L4-scheduling spike
#112). Independent per-block work scales horizontally exactly like the
chunk-prover cells. Blocks are distributed across the pool by the **outer
dispatch** — the existing §D2 competing-pull layer, with coordinators as a
**second consumer class** (#113). No new infrastructure.

**Failure model.** A pool removes the single-coordinator SPOF. One
coordinator death = **one block redelivered** (the same unit as the cell
tier's outer-tier redelivery; §1.1). Coordinator-recovery *latency* is
UNMODELED — see §6.

**The two levers (#113).** The design must accommodate **either** outcome:

| Lever | Status | Effect on the pool |
|---|---|---|
| **Pool** (primary) | **PROVEN** — independent per-block work scales horizontally | ~30 coordinators serial at ≥5 blocks/s and zero concurrency (≈0.17 blocks/s/coordinator at ~5.7 s/block; #113) |
| **Per-coordinator concurrency** (secondary) | **PROMISING, NOT PROVEN** | fewer coordinators if concurrency reaches ~3–5× |

The concurrency lever is a **build-time hypothesis, not a current
measurement** — no per-coordinator utilization profile has been captured
yet. The intuition: if a single block's fold leaves cores idle outside its
all-core L4 burst, running several blocks per coordinator at once could
slot each block's burst into another block's quieter phases. But it is
**NOT PROVEN**: it depends on L4 bursts **interleaving** rather than
**colliding**, and prior concurrency scaling was poor at the L2/L5 layers.
The utilization profile that would confirm or refute this is itself the
build-time measurement called for in §7b. **Validate at build, do not
assume.**

**Sizing is a function, not a number.** Pool size is handed to **#95** as
**`f(block rate, per-coordinator concurrency)`** — never a fixed count, and
**never summed with the cell count** (ADR-0004 §6.2: `c` is counted by
class; #95 sizes **two** pools). The pool is ~1% of the ~800–900 cell fleet
(ADR-0004 §4.1) — negligible infra, but a distinct class.

---

## 3. The witness plane (#61)

**Question answered:** how does a cell obtain its chunk's witness + seed
roots, without that acquisition polluting the proving path? **Serves:**
`lag` (the `witness_move` term).

The witness `witness_move` is the **one UNMODELED term** in ADR-0004 §3.1's
`per_block_lag`. This ADR designs the **seam**, names the term UNMODELED,
and **invents no number** for it.

**The source is a PARAMETERIZED input.** The distribution design must hold
**regardless of source**:

- **Today (k=1 building block):** a **local mounted read-only corpus**
  resolved by `{height, witness_index}` via **local indexed lookup**
  (ADR-0003 §D6; #61). GCS is **showback-only** (never in the per-proof
  critical path — a 100–300 ms round-trip is ~100% tax on a ~0.5 s fold
  step; ADR-0003 §D6). The witness **never travels the trace or the message
  bus** (#61).
- **Possibly later:** a Lighter witness service. An in-flight spike **#83**
  is mapping that boundary, so the witness **SOURCE is TBD-by-#83**.

**The seam the conductor designs (source-independent):**

- **Cell-side resolution seam.** A cell, given its chunk's
  `{height, witness_index}`, resolves the witness + seed roots through a
  local lookup interface — the same call shape whether it is backed by the
  mounted corpus today or a witness service later. Per the ADR-0003 §D6
  amendment, a single block's witnesses must be **partitionable across the
  cells** proving its chunks (the whole-block mount is the k=1 case);
  dispatch carries **witness references**, not witness bytes (§1.2; #75
  coordinator spec).
- **Instrumentation seam.** A dedicated **`witness_fetch_ms`** BENCH_EVENT
  field (`bench/src/events.rs`; #61) so witness acquisition is **always
  separately accountable** and subtractable from prover metrics — which is
  exactly what would eventually let `witness_move` be measured (ADR-0004
  §3.2: "implement #61's corpus + `witness_fetch_ms`, then read it
  directly").

Until #61 lands the corpus + field and #83 fixes the source, `witness_move`
stays **UNMODELED** (ADR-0004 §3.1/§3.2). Named here; no number invented.

---

## 4. Straggler / tail (#101)

**Question answered:** how does k-wide chunk fan-out hit `lag_p99 ≤ 40 s`
when a fold round waits on its slowest input? **Serves:** `lag` (the p99
tail).

A FOLD round completes at the **max** over its k chunk proofs (max-of-N
statistics; ADR-0004 §2.1 UNIFIED straggler row + §3.4). Under always-split,
k reaches **~1000** for a 9000-tx block at S=9 — max-of-1000 pushes p99
well above p50 even with a tight per-chunk distribution.

**The seam the conductor designs.** A place where a tail-mitigation
mechanism plugs into the inner-tier dispatch (§1.2): the coordinator, which
owns the chunk set, is the natural site for **hedged dispatch / speculative
last-decile / work-stealing**. The **detailed mechanism is DEFERRED to
#101** (ADR-0004 §3.4 names it #101's design); this ADR provides the seam,
not the mechanism.

**Budget (from ADR-0004 §3.4 / §4.3).** The median slack **is** the
straggler budget: at `c4a-highcpu-64`, S=9, 9000-tx, slack ≈ **11.27 s**
under the 20 s p50 bound and ≈ **31.27 s** under the 40 s p99 bound.

**Caveat (carried, not papered over).** The p99 arithmetic needs a **wider
per-chunk variance sample than the current n=3** (S=9 L1 stdev 20.7 ms,
n=3 — thin; ADR-0004 §3.4 flags this) before p99 is trusted.

---

## 5. The fleet substrate (ADR-0003 §D7)

**Question answered:** what runs the two machine classes elastically?
**Serves:** capacity(`c`).

A **Managed Instance Group (MIG) of EACH machine class** — one for the
chunk-prover **cells**, one for the **coordinators** — each **autoscaling on
Pub/Sub backlog** via Cloud Monitoring metrics (ADR-0003 §D7). Two pools,
two MIGs, sized separately (ADR-0004 §6.2).

**GKE stays DEFERRED** (ADR-0003 §D7): whole-node billing neutralizes its
economics for 32–64 vCPU CPU-saturating pods, and kubelet reservations
pollute cross-fleet benchmark comparability. Platform-specific logic stays
quarantined in one lifecycle lib with a `platform` field on the run
manifest, so a future GKE backend is a new lib, not a redesign. **Revisit
triggers** (ADR-0003 §D7, quoted): (a) the production prover commits to
Kubernetes; (b) the rig becomes always-on; (c) concurrent multi-experiment
demand.

**GCS is showback-only** (ADR-0003 §D6): run manifests, BENCH_EVENT JSONL,
final proof artifacts — never in the per-proof critical path.

---

## 6. How `lag(c, l)` is realized by this design

**Question answered:** does the conductor provably *target* the governing
equation, and where does each lag term come from? **Serves:** all three
(the synthesis).

ADR-0004 §3.1's central-path `lag` function maps onto the concrete
operational path as follows. Each term names the **design element that
produces it**:

| `lag(c, l)` term (ADR-0004 §3.1) | Produced by (this design) | Value (c4a-highcpu-64, S=9) | Label |
|---|---|---|---|
| `witness_move` | the **witness plane** (§3) | **UNMODELED** | not invented (#61/#83) |
| `max_over_chunks(chunk_prove)` (L1) | **cells**, in parallel across k (§1.2) | **3.051 s** | measured (ADR-0004 §3.2) |
| `ceil(log2(k)) · merge_step` (L2) | the **coordinator**'s L2 tree fold (§2) | k=1000 → 10 × **0.2751 s** ≈ 2.751 s | measured (ADR-0004 §3.2) |
| `L4` (block prove, serial) | the **coordinator**, serial — the **dominant** term (ADR-0004 §6.1) | **2.928 s** | measured (ADR-0004 §3.2) |
| straggler / recovery **TAIL** | the **straggler seam** (§4) + coordinator recovery (§2) | straggler partly measurable; recovery UNMODELED | #101 / not invented |

**The design provably targets the equation.** For the worst block size
(9000 tx → k=1000) on `c4a-highcpu-64` at S=9, the **central path** is

```
    3.051 (L1) + 2.751 (merge, k=1000) + 2.928 (L4) ≈ 8.73 s
```

against the **20 s p50 bound** — **11.27 s of slack** (ADR-0004 §3.3/§4.3).
This reproduces, to the millisecond, the committed
`single_machine_wall_9000 = 8.730` / `slo_slack_min = 11.270` in
`calibration/c4a-highcpu-64.json` (the k=1 lower bound of the cross-cell
wall; ADR-0004 §3.3). The **block-size-independence** of the floor (~8–9 s
across an 18× tx/block range; ADR-0004 §4.2) is the headline property:
the conductor's lag is set by `L1 + log-depth-merge + L4`, not by block
size. The risk is **not** the central path; it is whether the **tail** (§4)
consumes the slack — #101's question, once per-chunk variance is sampled
wider than n=3.

**What stays UNMODELED (named, not invented):**

- **`witness_move`** — pending #61 (corpus + `witness_fetch_ms`) and #83
  (source). (ADR-0004 §3.1.)
- **Coordinator-recovery latency** — no coordinator exists yet (#75); a
  death mid-fold costs a re-gather + re-fold (bounded by one block's fold
  cost ≈ 5.7 s at k=1000) **plus** re-dispatch latency, which is unmeasured.
  (ADR-0004 §3.4.)
- **p99 variance sample** — n=3 is thin (ADR-0004 §3.4).

No number is invented for any UNMODELED term.

---

## 7. Open questions / deferred

Each item lists **what unblocks it.**

**a. Design/cost review gate before any cloud spend.** This is real spend;
the gate is retained on #75. **Unblocked by:** maintainer review of **this
ADR** (and #75's re-scoped design). Nothing is provisioned until then.

**b. Per-coordinator-concurrency validation (#113 secondary lever).**
PROMISING-NOT-PROVEN; **gated to the coordinator build** (not now).
**Unblocked by:** two build-time measurements on one `c4a-highcpu-64` —
(1) a single-unit utilization profile (the per-coordinator baseline, not
yet captured), and (2) a several-blocks-at-once run (does throughput climb,
where is the burst-collision knee). The result tunes the
pool-size-vs-concurrency mix and feeds #95.

**c. Witness SOURCE (local corpus vs Lighter service).** The seam (§3) holds
either way. **Unblocked / reshaped by:** the **#83** spike mapping the
witness-service boundary.

**d. L4 reduction — the derived next structural lever (ADR-0004 §6.1).**
After the conductor is built, L4 is the largest single term, serial per
block, and untouched by either distribution grain. **L4 streaming is
structurally impossible** (the spike: `BlockCircuit` must verify the
*complete* folded chain proof — a hard data dependency), so reducing the
single-block / cold-start / burst-tail **floor** would be **circuit surgery
on `BlockCircuit`'s two verify subgraphs**, GATED/parked behind the
pre-committed trigger (ADR-0004 §6.1: tighten p99 below `wall(L3)+wall(L4)`
on the best attainable coordinator shape, or renegotiate the bound).
Sustained-load L4 throughput is **already answered** by the coordinator pool
(§2; #113). **No issue exists yet for the circuit-surgery lever** — filing
one (do-not-start-until-triggered) is the recommended follow-up.

---

## 8. Honesty ledger (Discussion #58 norms)

- **Every constant is machine-tagged and never mixed.** The §6 central-path
  constants (`chunk_prove` 3.051 s, `merge_step` 0.2751 s, `L4` 2.928 s,
  coordinator ≈ 5.7 s/block) are **`c4a-highcpu-64`** (deployment
  candidate). The only **EPYC 7B13** numbers referenced — the 156.0 KiB
  chunk-proof wire size (ADR-0004 §2.1/§3.2) and the L5/L6 batch-finalization
  constants (ADR-0004 §5) — are labeled EPYC and **not transplanted onto
  c4a**.
- **Designed-from-measured vs designed-from-assumption is distinguished.**
  The central path (§6) is **designed from measured** c4a constants
  (reproduces `calibration/c4a-highcpu-64.json` to the ms). The
  per-coordinator-concurrency multiplier (§2) is **designed from
  assumption** (PROMISING-NOT-PROVEN) and is explicitly held as a
  to-validate knob, with the design accommodating either outcome.
- **UNMODELED terms are named, not invented:** `witness_move` (#61/#83),
  coordinator-recovery latency (#75), the p99 variance sample (n=3). Each
  carries what would measure it.
- **Design only.** No code is built, no test is run, nothing is
  provisioned by this ADR. It is a target for the build (#75), not a
  measurement of a running system.
