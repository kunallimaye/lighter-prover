# ADR: Target architecture for distributed recursive proving — dynamic-depth tree + fungible pull pool

- Status: Proposed
- Date: 2026-06-27
- Related: discussion #287 (architecture review + open cryptographer questions),
  issue #281 (reduction-tree fixed-VK hardening), issue #283 (distributed
  prover-node honesty / GKE topology), issue #288 (multi-level recursive
  aggregation validation, merged as PR #289)

## Context

The prover aggregates per-transaction (leaf) proofs into a single block-level
root proof via Plonky2 recursion. Two aggregation shapes exist in the codebase,
and the distributed deployment so far hardcodes a single-level, fixed-fan-in
reduction step. To scale aggregation horizontally — across many pods, on Spot
capacity, at arbitrary block sizes — the work definition, the recursion circuit,
the worker role model, the transport, and the autoscaling policy all have to
change in a coordinated way. This ADR records the target architecture decided
across discussion #287 and validated incrementally by issues #281, #283, and
#288 (PR #289), so that subsequent implementation work has a single, self-
contained reference.

### Two aggregation circuits — chain vs tree

- **`BlockTxChainCircuit`** (the circuit `bench.rs` exercises) uses **cyclic
  recursion**: it verifies a proof of *itself* via `common_data_for_recursion`,
  and verifies two proofs per call (the previous chain proof plus one new leaf).
  Properties: O(N) sequential depth, a single verifier key (VK), naturally
  streamable. But it is **not horizontally scalable** — each fold depends on the
  previous fold's output, so it is an inherently sequential reducer.

- **`HexadecimalTreeChainCircuit` / `BinaryTreeChainCircuit`** (the distributed
  path) use **fixed-VK recursion**: the child VK is pinned via
  `constant_verifier_data` — the hardening landed in issue #281 — and the node
  verifies `radix` proofs per call. Properties: O(log_radix N) depth, but **one
  VK per level**, and crucially **sibling-independence**: two nodes at the same
  level share no data, so they can be aggregated on different pods concurrently.

### What was unproven before #288

The distributed path was previously capped at a single aggregation level. The
node code rejected anything else (`level != 1 -> exit(2)` in `prover_node.rs`),
the pod spec rendered a fixed fan-out (`completions = radix` in
`render_pod_spec.py`), and the radix was a compile-time constant (`RADIX = 16`).
Whether a node could verify-and-fold the proofs of *other nodes* — a "node of
nodes", i.e. true multi-level recursion — was an open question.

## Decision

Adopt a **dynamic-depth recursive tree** as the aggregation topology, served by
a **fungible single-image worker pool** that **pulls** readiness-gated fold
tasks from a managed queue, with proof payloads transported over GCS, guarded by
a cheap idempotent durable-write, and scaled by a baseload-plus-burst policy.
The decisions below are settled (validated by code, pilot data, or both) except
where explicitly flagged as open.

### 1. Use the tree, not the chain, for distributed proving

The chain cannot be scaled out: its fold *i* consumes fold *i−1*'s output, so
the reducer is sequential by construction. The tree's sibling-independence is
exactly the property that lets aggregation fan across pods. We therefore build
distributed proving on the **fixed-VK tree** (`HexadecimalTreeChainCircuit` /
`BinaryTreeChainCircuit`), accepting the cost of one VK per level in exchange
for horizontal parallelism. The chain remains a valid single-process streamable
reducer and is retained for that role (e.g. `bench.rs`), not for scale-out.

### 2. Dynamic-depth recursive aggregation (the keystone unlock)

Tree depth must be computed at **runtime** as `ceil(log_radix(N))` for the
actual leaf count N, not fixed at a single level. This requires removing the
three hardcodes noted above (`level != 1` guard, `completions = radix`,
constant `RADIX`).

**Multi-level composition is validated** (issue #288, merged as PR #289). A node
can verify and fold child *node* proofs — a "node of nodes" — because a node's
output public-input shape equals a child's input shape (both are `BatchTarget`),
and `define(CONFIG, &child_circuit_data)` accepts a node's circuit data as the
child circuit. The recursion therefore composes to arbitrary depth.

**The `dummy_proof` lesson (key cryptographic-engineering result).** Trees with
N that is not an exact power of radix have empty slots at some nodes. Those slots
cannot be padded the way leaf nodes pad:

- Padding a recursive-child slot with `dummy_proof` fails: you cannot synthesize
  a witness for a recursive-verifier circuit, so proof generation aborts with
  "generators weren't run".
- The naive fix — `cyclic_base_proof` / `dummy_circuit` — **panics**, because
  `dummy_circuit` does not reconstruct the `ConstantGate` that
  `constant_verifier_data` bakes into the fixed-VK circuit. A pinned-VK circuit
  and a cyclic-VK dummy are structurally incompatible.
- **The validated fix (#288/#289):** pad empty interior slots with a **real,
  recursively-minted base proof of the child circuit**. Record this as the
  governing lesson: in fixed-VK recursion, every slot — including padding — must
  be a genuine proof of the exact pinned child circuit; there is no synthetic
  shortcut.

**Two implementation options for arbitrary depth:**

- **Option A — heterogeneous fixed-VK levels.** A distinct circuit (and VK) per
  level; the orchestrator drives depth by selecting the right level's circuit
  for each task. Reachable with the code as it stands today. **Recommended as
  the near-term path.**
- **Option B — homogeneous cyclic self-verifying tree node ("Way B").** A single
  circuit (one VK) that self-verifies at any depth. More circuit engineering,
  but it unifies the design and is the elegant target — and it is the enabler
  for a fully fungible worker (any pod folds at any level; see §4).

Decision: ship **A first**, treat **B** as the target end-state.

### 3. Chunks (`tx_per_proof`) are a tuning knob, not a scaling lever

Before dynamic-depth recursion, the leaf chunk size (`tx_per_proof`) doubled as
a scaling mechanism. With a runtime-sized tree it is demoted to a **local prove-
efficiency knob**: batching several transactions into one leaf amortizes the
leaf circuit's fixed cost and avoids paying a recursive verification per
transaction. The break-even chunk size S is where adding one more transaction to
a leaf costs about the same as aggregating that transaction as its own leaf
(i.e. one extra recursive-verify in the tree). Chunks are therefore **not
redundant** — they suppress per-transaction recursive-verify cost the tree would
otherwise incur — but their *structural* importance shrinks: depth and width now
come from the tree, not from the chunk size.

### 4. Fungible worker pool

Run **one container image / one binary (`prover-node`)**, with the role (leaf
prover vs level-k folder) chosen **per message at runtime**, not baked into a
deploy-time command. Benefits: a single pod shape, a single autoscaling knob, a
single Spot pool, trivial bin-packing, and "dial back in" — a worker that
finishes a leaf immediately pulls the next ready aggregation task rather than
idling. This pairs naturally with **Option B** (a homogeneous circuit lets any
pod fold at any level).

**Keystone prerequisite:** decouple **N** (leaf count) from **radix** (fan-in)
from **pod count**. Today `completions = radix` bakes capacity into the work
definition. That coupling must be lifted so the block volumetric stays a free
tuning knob rather than a deployment-time constant.

### 5. Transport (settled)

- **GCS is the durable payload transport.** Proof bytes (~200 KB each) are
  written to and read from GCS, which is already wired. Keep it.
- **A managed queue carries small work descriptors** — pointers, not bytes — and
  owns atomic claim and event-driven wakeup. **Do not turn GCS into the queue**;
  that would force hand-rolled polling and lock acrobatics.
- **Pull, not push.** Fold/leaf tasks are long (seconds to minutes) and saturate
  a pod one at a time, so a pod must take work only when it is free. Use
  **streaming / async pull with flow control set to one outstanding message**:
  readiness *is* the flow-control credit the pod extends to the broker. This
  gives event-driven wakeup **without** busy-polling, and **without** a self-
  maintained pod registry (which would amount to building a scheduler).
- **Readiness gating is our domain logic.** A fold task for a parent node is
  publishable only when all its children exist; track this with a per-parent
  child counter. This is ours to own regardless of transport.

### 6. Claim guard — empirically settled (pilot data)

> **CORRECTION (see "Amendment (real prove-time measurement)" below).** The
> "heavy-tailed" and "irreducible by threshold tuning" framing in this section
> came from a **simulation**, not a measurement. A later pilot measured the
> **real circuits** and found them **tight** (CV 0.13–0.25), *not* heavy-tailed.
> The conclusion of this section — *the idempotent-output guard is mandatory* —
> **still holds**, but the *reason* changes: the guard is justified by
> **operational** tail (Spot preemption + noisy-neighbour contention), not by
> a **circuit-inherent** tail. The original simulated reasoning is retained
> below for traceability; read it together with the Amendment.

A pilot simulated a queue-plus-worker system under realistic, heavy-tailed prove
durations (noisy-neighbour Spot behaviour). Results:

- **Drop the heavyweight pre-claim lock.** It prevents at most ~1.7% of compute
  (heavy-tail at 2×P99) and ~0.5% at 3×P99, while the dominant ~5% waste is
  dead-worker partial runs that *no* guard prevents. Not worth the added
  complexity or the lock-holder-death failure mode.
- **Keep the cheap idempotent-output guard — mandatory, not optional.** Use an
  atomic create-if-not-exists with a deterministic output name. On heavy-tailed
  durations, double-execution equals the distribution's tail mass
  `P(dur > ack_deadline)` and is **irreducible** by threshold tuning: it
  asymptotes toward zero but never reaches it, and chasing zero costs ~5-minute
  recovery latency. Concurrent double-writes *will* happen; the idempotent write
  is what prevents torn or corrupt outputs.
  *[CORRECTED: the real circuits are tight, so on dedicated hardware
  double-execution is **not** irreducible — threshold tuning alone would keep it
  rare. The guard remains mandatory for the **operational** tail instead; see
  the Amendment.]*
- **Use the GCS native API `ifGenerationMatch=0`** for the atomic write —
  verified to elect exactly one winner (12 racers → 1 success, 11 precondition-
  failed). Do **not** rely on GCS-Fuse `O_EXCL`; Fuse atomicity is unverified.
- **Set `ack_deadline ≈ 2×P99`** (the sweet spot: ~0.1% double-exec on heavy
  tails, zero for tight or bimodal distributions, ~50 s recovery). **Extend the
  deadline while proving, and ack only after the result is durably committed —
  never ack on pull.** *[UPDATED: real per-circuit `ack_deadline` values
  (from real 2×P99) are now recorded in the Amendments — on the 32-core EPYC
  pilot leaf/pre-exec ≈ 8 s, radix-2 fold ≈ 6 s, radix-16 fold ≈ 30 s; on the
  live `c3d-highcpu-16` GKE run (Amendment 2) leaf ≈ 150 s, radix-16 fold
  ≈ 180 s. **The default is now 180s** (issue #312); all values are
  hardware-dependent — re-derive per instance type.]*

### 7. Autoscaling / capacity

Run **baseload plus burst — not scale-to-zero.** Pre-provision ~60% of the
**peak parallel width** (the operator's target; the volumetric is a tunable
knob) on dedicated / committed nodes (not Spot), and burst the top ~40% on Spot
via **KEDA on the Pub/Sub backlog** (`num_undelivered_messages`).

**Graceful drain is mandatory:** `terminationGracePeriodSeconds ≥ max prove
time`, and on SIGTERM a pod finishes its in-flight prove and then acks, so
scale-down or preemption never kills a mid-prove pod. Note that backlog
underestimates the **narrow aggregation tail** (few parallel fold tasks exist at
the upper levels), so **do not scale to zero before the root proof exists.**

> **UPDATED (see Amendments).** "max prove time" can now be grounded in real
> data. On the measured 32-core EPYC bare-metal host, max observed prove times
> were ≈ 4.4 s (leaf, contended), ≈ 2.7 s (pre-exec), ≈ 2.2 s (radix-2 fold)
> and ≈ 12.6 s (radix-16 fold). On the **live `c3d-highcpu-16` GKE run**
> (Amendment 2) they were far higher — leaf max ≈ 73.65 s and radix-16 fold max
> ≈ 83.26 s — driving `terminationGracePeriodSeconds` to ≈360 s there. These are
> **hardware-dependent** — slower cloud instances scale proportionally, so set
> `terminationGracePeriodSeconds` from the *target* instance type, not the EPYC
> figures. The real peak-RSS figures (radix-2 fold ≈ 0.31 GB, radix-16 fold
> ≈ 2.2 GB) are also recorded in the first Amendment for bin-packing the
> fungible pool.

The block volumetric remains a free, forgiving knob **only if all four
invariants hold**: (1) fungible pods, (2) pull-based work-stealing, (3)
idempotent + claim-guarded durable tasks, and (4) a readiness-gated DAG rather
than a fixed pod-count assumption.

## Consequences

- The distributed prover gains true horizontal scaling: aggregation width and
  depth track the block size at runtime instead of a hardcoded single level.
- One image, one pod shape, one Spot pool, one autoscaling knob — operationally
  simple, cheap to bin-pack, and resilient to preemption via graceful drain.
- Exactly-once *effect* (not exactly-once execution) is achieved cheaply: double
  execution is tolerated and rendered harmless by the idempotent
  `ifGenerationMatch=0` write, avoiding the complexity and failure modes of a
  distributed pre-claim lock.
- Implementation proceeds in two stages: Option A (heterogeneous fixed-VK
  levels) unblocks dynamic depth now; Option B (homogeneous cyclic tree node)
  is the end-state that fully realises pod fungibility.
- Padding correctness is a standing constraint: interior empty slots must be
  filled with real recursively-minted base proofs of the pinned child circuit;
  synthetic/dummy padding is unsound for fixed-VK recursion.
- KEDA-on-backlog plus a never-scale-to-zero floor for the aggregation tail
  means some committed capacity is always paid for; this is the deliberate cost
  of bounded latency and root-completion safety.

## Amendment (real prove-time measurement)

- Status of amendment: empirical update — corrects the *rationale* in §6/§7,
  not the *decisions*.
- Date: 2026-06-27
- Supersedes: the **simulated** "heavy-tailed / irreducible" framing in §6.

The §6 claim-guard reasoning was built on a **simulation** that assumed
heavy-tailed prove durations. A subsequent pilot measured the **real circuits**
and found the **opposite shape**: the actual prove-time distributions are
**tight**, not heavy-tailed. This amendment records the real data and corrects
the rationale **without** changing the conclusion — the idempotent-output guard
**remains mandatory**, but for a different reason.

### Real measured prove-time distribution

Hardware/build: **AMD EPYC 7B13, 32-core, x86_64, bare metal, release build.**
These numbers are **hardware-dependent** — they characterize a fast, dedicated
32-core host and must be re-derived per target instance type.

| Stage | Circuit | Sample | P50 | P90 | P99 / max | CV | Shape |
|---|---|---|---|---|---|---|---|
| Leaf prove (isolated) | `BlockTxCircuit::prove`, `tx_per_proof=1` | clean per-worker, N=30 | 1.91 s | 2.16 s | P99 ≈ 3.26 s (max-proxy at small N); min 1.21 s, mean 1.87 s | **0.22** | **TIGHT** |
| Leaf prove (bench, pipelined) | same | N=500, contended w/ concurrent chain prover | 0.89 s | 2.74 s | P99 3.42 s, max 4.43 s | **0.60** | wider tail = **contention artifact**, not circuit-inherent |
| Pre-execution prove | `BlockPreExecutionCircuit::prove`, each leaf-worker runs once | — | 1.71 s | — | max 2.74 s | **0.25** | tight |
| Fold radix-2 | `BinaryTreeChainCircuit`, 2 children | N=8 (directional) | 2.03 s | — | max 2.23 s | **0.13** | very tight; peak RSS ≈ 0.31 GB |
| Fold radix-16 | `HexadecimalTreeChainCircuit`, 16 children | N=8 (directional) | 7.47 s | — | max 12.57 s, mean 9.0 s | **0.22** | tight, **mildly bimodal** (~7 s vs ~12 s clusters); ≈ 3× the cost of 2× the leaves vs radix-2; peak RSS ≈ 2.2 GB |

Notes:

- **Witness-gen is negligible (~1 ms)**; leaf time ≈ pure STARK prove.
- The only fat tail observed (leaf, CV=0.60) appears **only under host
  contention** (concurrent chain prover on the same box) — it is an operational
  artifact, not a property of the circuit. A multi-tenant worker host behaves
  more like this contended view than like the clean isolated view.

### Corrected rationale for the claim guard

1. **The original "irreducible by threshold tuning" claim does NOT transfer to
   the real circuit on dedicated hardware.** It was a property of the *simulated*
   heavy-tailed distribution. The real circuits are tight (CV 0.13–0.25, with
   max/P50 ≈ 1.1–1.7×) and have **no fat tail**. On dedicated hardware, setting
   `ack_deadline` from real P99 would keep duplicate executions **rare** —
   threshold tuning alone is effective, so the "irreducible" framing is wrong for
   this regime.
2. **The idempotent-output guard nevertheless REMAINS MANDATORY — now justified
   by OPERATIONAL tail, not circuit-inherent tail:**
   - **(i) Spot preemption + noisy-neighbour variance.** This bare-metal run
     cannot capture real cloud Spot behaviour; real Spot would be **more**
     heavy-tailed, not less.
   - **(ii) Contention is real.** The contended bench view already shows host
     concurrency stretching the leaf tail to **CV=0.60 / max÷P50 ≈ 5×**. A
     multi-tenant worker host behaves like that contended view.
   - **(iii) radix-16 mild bimodality** (~7 s vs ~12 s clusters) hints the tail
     is **not perfectly stable**.
   - The guard therefore stays as **cheap correctness insurance** for the
     Spot-induced + contention tail — the conclusion of §6 is unchanged.

### Real `ack_deadline` recommendations (from real 2×P99)

Derived as **2×P99** on the measured host. **Hardware-dependent** — re-derive
per target instance type; slower cloud instances scale proportionally.

| Worker role | Recommended `ack_deadline` |
|---|---|
| Leaf prover + pre-exec workers | ≈ **8 s** |
| radix-2 fold | ≈ **6 s** |
| radix-16 fold | ≈ **30 s** |

### Confidence

- **HIGH** — "tight on dedicated hardware" (well-sampled leaf/pre-exec, clear CVs).
- **MEDIUM** — extrapolating to Spot: a single bare-metal machine with **no real
  preemption variance**, and the fold samples are only **N=8 (directional)**.
- This dedicated-hardware measurement **partially closes** open item (a)
  (real-Spot prove-time P99) but does **not fully close** it; a real-Spot
  measurement is still pending.

## Amendment 2 (live 500-tx GKE run on c3d-highcpu-16)

- Status of amendment: empirical update — the first **real cloud (GKE/Spot)**
  measurement. Corrects the prove-time / `ack_deadline` *figures* in the first
  Amendment (which used the 32-core EPYC pilot) for the actual cloud instance
  type; **decisions unchanged**.
- Date: 2026-06-28
- Supersedes: the EPYC-derived `ack_deadline` numbers (leaf ≈8s, radix-2 ≈6s,
  radix-16 ≈30s) **for the `c3d-highcpu-16` target**. The EPYC figures remain
  valid as a *fast dedicated-host* data point; they were **optimistic for this
  instance type**.

### Run shape

A live Phase-1 GKE smoke test: **500 txs → 125 leaves at `tx_per_proof=4`,
radix-16, 10× `c3d-highcpu-16`** (16 vCPU / 32 GiB) **Spot** workers. This is the
first measurement on real cloud Spot hardware (vs the bare-metal EPYC pilot).

### Real measured timings (live)

| Metric | Result |
|---|---|
| Total wall time | **13.35 min** (800.96s) for the full 500-tx block → verified root; leaf+fold phases **overlapped** (async gating works) |
| Effective speedup | ≈ **7.5×** (100.6 worker-min of proving in 13.35 wall-min on 10 workers) |
| Leaf prove (125) | avg **42.55s**, min 12.03s, max **73.65s**; total 88.65 worker-min |
| Fold prove (9 = 8 L1 + 1 root, radix-16) | avg **79.89s**, min 68.97s, max **83.26s** (CV ≈ 0.06, tight); total 11.98 worker-min |
| GCS commit + CAS (`commit_and_gate`) | leaf avg 2.17s, fold avg 2.39s; ~4.9 worker-min ≈ **~5% of proving time → negligible** |

Hardware delta: `c3d-highcpu-16` folds are ~**8× slower** than the EPYC pilot
(~10s → ~80s) and leaves prove ~**20–35×** the pilot's ~2s (fewer cores + the
prefix-replay tail below).

### The prefix-replay tail is the dominant leaf cost at scale

The wide leaf spread (12s → 74s, vs the tight fold CV ≈ 0.06) is driven by the
**Option-A prefix-replay tail**, **not** circuit warmup. Under Option-A state
threading, leaf *i* re-executes chunks `0..i` to reconstruct its pre-state
before proving, so late leaves replay more prior chunks and grow roughly `O(N)`
across the block. This is now the **dominant leaf cost at scale**. The mitigation
lever is the **pre-state corpus** (#243 / #257): precomputing/serving per-leaf
pre-states removes the replay and flattens the tail. A larger `tx_per_proof`
(≥4) also shortens the tail by reducing the leaf count (125 at C=4 vs 500 at
C=1) — `C=4` beat `C=1` at 500 txs.

### Corrected `ack_deadline` recommendations (real cloud 2×P99)

Derived as **2×P99** on the measured `c3d-highcpu-16` run. **Hardware-dependent**
— re-derive per target instance type.

| Worker role | Measured P99 (c3d-highcpu-16) | Recommended `ack_deadline` (2×P99) |
|---|---|---|
| Leaf prover + prefix-replay | ≈ **74 s** | ≈ **150 s** |
| radix-16 fold | ≈ **83 s** | ≈ **180 s** ← long pole |

**The default `ack_deadline` is raised 60s → 180s** (issue #312). The old 60s
default was *shorter than* a ≈80s fold: every fold exceeded its base Pub/Sub
lease and survived only on the `modifyAckDeadline` heartbeat with **zero
margin** — a single missed/delayed beat → redelivery mid-prove → wasted
duplicate work. 180s = 2×P99 of the radix-16 fold long pole, still well within
the Pub/Sub `[10, 600]s` range, and `terminationGracePeriodSeconds` is sized
accordingly (≈360s). The guard (§6) remains mandatory regardless.

### Confidence

- **HIGH** — the fold distribution is tight and well-characterized (CV ≈ 0.06).
- **MEDIUM** — leaf P99 is dominated by the prefix-replay tail, which is
  workload-shape-dependent (it grows with leaf count / block size); re-derive for
  a different N. This run did not stress real Spot **preemption** mid-prove.
- This run **substantially closes** open item (a) (real-Spot prove-time P99) for
  this instance type and workload shape; preemption-variance characterization is
  the remaining gap.

## Open items

These are explicitly **not yet resolved**:

- **(a) Real prove-time P99 on Spot.** *Partially closed* — see "Amendment
  (real prove-time measurement)". `ack_deadline = 2×P99` was originally derived
  from *simulated* distributions; the Amendment records the **real** prove-time
  distribution measured on **dedicated bare-metal hardware** (32-core EPYC),
  which is tight (CV 0.13–0.25). This closes the *dedicated-hardware* half of
  the question. It does **not** close the **real-Spot** half: this run had no
  real preemption or sustained noisy-neighbour variance, and the fold samples
  are small (N=8, directional only). A real-Spot prove-time/P99 measurement is
  therefore **still pending** before the Spot-targeted `ack_deadline` and burst
  policy are finalized.
- **(b) Cryptographer review** of: spans-of-spans continuity completeness at
  level ≥ 2; the heterogeneous-VK (Option A) vs cyclic-tree (Option B) choice;
  and dummy/base-proof padding soundness at interior levels. These are the three
  questions posted in discussion #287.
- **(c) GCS-Fuse atomicity** remains unverified — use the native GCS API
  (`ifGenerationMatch=0`) for the idempotent guard until proven otherwise.

## Implementation status — autoscaling slice (issue #302)

The §7 autoscaling decision is now wired (manifests + Terraform definitions +
config + the one needed code change), **without any live cloud action** — the
live end-to-end run on a real cluster is `TODO(confirm-on-live-run)`.

**Landed (verified locally):**

- **Fungible-pool path (Path A).** `infra-as-code/kubernetes/fungible_pool.yaml`
  (Deployment running `prover-node work --transport=pubsub`, Workload-Identity
  `prover-sa`, GCS Fuse volume for proof bytes, `nodeSelector`/tolerations for
  the prover node pools, per-arch resources, Pub/Sub env/flags) and
  `keda_scaledobject.yaml` (KEDA `gcp-pubsub` backlog trigger, `minReplicaCount`
  = baseload **> 0**, `maxReplicaCount` = baseload + burst, scale-down
  stabilization + `pod-deletion-cost` favouring idle pods). This is **distinct
  from** the phase-locked per-level Job path (#293/#297), which remains valid;
  see `infra-as-code/kubernetes/README.md` for the side-by-side comparison.
- **Graceful drain.** `bench/src/shutdown.rs` adds a
  process-global SIGTERM/SIGINT handler; `run_dispatch_loop` checks it at the top
  of each iteration and, on shutdown, **stops pulling new work, finishes +
  commits + acks the in-flight lease, and exits** — never killed mid-prove.
  Unit-tested (`bench/src/bin/prover_node.rs` drain tests) without raising real
  signals via the policy/mechanism split. Default build stays cloud-free + green.
- **Live Pub/Sub worker dispatch wiring (#306).** `run_dispatch_loop` is now
  generic — `run_dispatch_loop<T: WorkTransport>` — and drives queue/store ops
  through the trait only, so the SAME loop runs over `LocalTransport` (default
  build) and `PubSubGcsTransport` (under `--features pubsub`). `commit_and_gate`
  is promoted onto the `WorkTransport` trait (both backends impl it natively).
  Seeding is removed from the loop body and made an explicit step: `--transport=
  pubsub --seed` is a one-off seeder (connect → `seed_leaves` → exit); the worker
  path (no `--seed`) runs the real pull→prove→commit→ack loop (the prior
  `BACKEND_WIRED_NO_LIVE_RUN` no-op stub is gone). Per-iteration `[instrumentation]`
  logs carry a pod identity (`worker=HOSTNAME|pid`) plus PULL/PROVE/COMMIT(+outcome)/
  ACK/LOOP latencies for CAS-winner attribution across the pool. The default
  build still gives a clear "rebuild with `--features pubsub`" error (never a fake
  success); the live broker/bucket run remains `TODO(confirm-on-live-run)`.
  Verified locally: default + `--features pubsub` builds, `cargo test -p bench`
  (incl. a generic-loop test over a non-Local transport double), and the
  `--transport=local` e2e (verified root, back-compat).
- **Node topology.** `proving_pod_node_pool` gains `fungible_baseload_pool`
  (committed, NOT Spot) + `fungible_burst_pool` (Spot, autoscaling 0..N), gated
  on `orchestration_engine == "gke"` **and** `enable_fungible_pool` (default
  off). The MIG path is unchanged. `terraform fmt -check` + `validate` clean.
- **Rendering.** `render_pod_spec.py --emit-fungible` emits the filled-in
  Deployment + ScaledObject from `config.toml`; the default (phase-locked) render
  is unchanged.

**Remaining for the live end-to-end run (`TODO(confirm-on-live-run)`):**

- Install KEDA on the cluster (Helm; documented, not installed here) and bind the
  scaler's Workload Identity.
- Provision the GKE cluster + fungible node pools (`enable_fungible_pool = true`,
  `terraform apply`) and the Pub/Sub topic/subscription + GCS bucket.
- Run the live pull→prove→commit→ack loop (real redelivery, cross-node GCS CAS),
  confirm KEDA scales burst on real backlog, and confirm graceful drain on real
  scale-down + Spot preemption (incl. the live runner patching
  `pod-deletion-cost` on lease acquire/release).
- Close open item **(a)** (real-Spot prove-time P99) with this live run.
