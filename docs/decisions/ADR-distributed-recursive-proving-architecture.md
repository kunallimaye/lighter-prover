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
- **Use the GCS native API `ifGenerationMatch=0`** for the atomic write —
  verified to elect exactly one winner (12 racers → 1 success, 11 precondition-
  failed). Do **not** rely on GCS-Fuse `O_EXCL`; Fuse atomicity is unverified.
- **Set `ack_deadline ≈ 2×P99`** (the sweet spot: ~0.1% double-exec on heavy
  tails, zero for tight or bimodal distributions, ~50 s recovery). **Extend the
  deadline while proving, and ack only after the result is durably committed —
  never ack on pull.**

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

## Open items

These are explicitly **not yet resolved**:

- **(a) Real prove-time P99 on Spot.** `ack_deadline = 2×P99` is currently
  derived from *simulated* distributions. A measurement spike for real leaf and
  fold P99 on Spot is in progress and is required to finalize the deadline.
- **(b) Cryptographer review** of: spans-of-spans continuity completeness at
  level ≥ 2; the heterogeneous-VK (Option A) vs cyclic-tree (Option B) choice;
  and dummy/base-proof padding soundness at interior levels. These are the three
  questions posted in discussion #287.
- **(c) GCS-Fuse atomicity** remains unverified — use the native GCS API
  (`ifGenerationMatch=0`) for the idempotent guard until proven otherwise.
