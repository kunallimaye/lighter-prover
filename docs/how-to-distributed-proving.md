# How-To: The Distributed Recursive Proving System

A practical integration guide for **users and agents** working on the
`radix-16-reduction-trees` branch. It explains the proving pipeline, the two
orchestration models, how to run things locally today, the config/tuning knobs,
the GKE deployment steps, the transport correctness contract, and — most
importantly — **how this work plugs into the existing `Makefile`, scripts, and
config**.

Design rationale lives in the ADR; this guide does **not** duplicate it:

- `docs/decisions/ADR-distributed-recursive-proving-architecture.md` — the
  architecture decision (chain-vs-tree, dynamic depth, fungible pool, transport,
  autoscaling). Read it for the *why*; read this guide for the *how*.

> **Honesty note.** Everything in §3 (Local quickstart) is runnable **today**
> with no cloud. Everything cloud-facing (§5) is **verified-by-construction**:
> the manifests, scripts, and the production transport backend exist and are
> unit/pilot-verified, but a full live GKE end-to-end run is a deliberate
> operator step still marked `TODO(confirm-on-live-run)` in the source. This
> guide flags that boundary everywhere it matters.

---

## 1. Overview / mental model

The pipeline turns a block's transactions into one verified root STARK proof:

```
pre-execution ──▶ leaf proving ──▶ dynamic-depth tree aggregation ──▶ root verify
 (BlockPreExec)   (N leaf proofs)   (depth = ceil(log_radix N) levels)  (1 root)
```

1. **Leaf proving (corpus pre-state read).** A leaf worker obtains chunk `i`'s
   authentic pre-state, threads it into `BlockTxCircuit`, proves the single
   chunk `i`, derives the real `Batch` aggregate from the proven public inputs,
   wraps it in a `BatchTarget`-shaped leaf proof, **verifies it**, and persists
   it to `reports/stark_proofs/leaf_{i}.proof`.

   **Pre-state: corpus READ replaces prefix replay (issue #316).** Chunk `i`'s
   pre-state is the ledger state having applied all PRIOR chunks' txs. The
   previous implementation recomputed it by re-proving (witness-gen) every
   prefix chunk `0..i` on **every** leaf — an **O(N²)** tail across the tree
   (leaf `i` re-does `i` prefixes, so the leaf phase grows quadratically in the
   chunk count). The leaf worker now **reads** that pre-state directly from a
   committed per-tx positional **pre-state corpus**: `at_chunk(C, i)` is the
   snapshot `snapshots[C·i]`, the exact state the prefix replay would have
   reproduced. A pilot confirmed the read path is **bit-identical** to the
   replayed state (identical `old/new_state_root`, `old/new_account_delta_tree_root`,
   `new_validium_root`) and **~21× faster** across the leaf phase.

   Because we **replay the same committed block**, no corpus regeneration is
   ever needed for a normal run — the read path plus the committed dataset
   suffice. The corpus is the committed dataset
   `bench/corpus/cap-block/captured_corpus.gz` (the per-tx pre-state of the
   bundled `bench/bench_test.json` cap block; see
   `bench/corpus/cap-block/README.md`). The generation/harvest code that mints a
   **new** corpus is intentionally **not** in this branch — it lives on the
   `parallel-v0.0.1-alpha` branch and is only needed if `bench_test.json` itself
   changes or the corpus schema MAJOR bumps.

   **Honest fallback.** If the corpus cannot be loaded (missing file, corrupt
   bytes, incompatible schema MAJOR — the loader returns an error, **never** a
   fabricated snapshot) or the requested chunk index is absent, the leaf falls
   back to the original prefix-replay path and logs which path it took. The
   `Batch` is identical on either path. `BlockPreExecutionCircuit` is still
   proven on both paths to obtain the block's `new_validium_root`.
2. **Tree aggregation (dynamic depth).** A tree node folds up to `radix` child
   proofs into one parent. Depth is computed at runtime as
   `depth = ceil(log_radix(N))` for `N` leaves — level 1 folds leaf proofs;
   level `L ≥ 2` folds level-`(L-1)` node proofs. Each level pins the child
   verifying key (fixed-VK recursion, issue #281), so siblings are independent
   and can be proved on different pods concurrently. Under-full nodes are padded
   per the #289 API (`dummy_proof` at level 1; a real recursively-minted base
   proof at level ≥ 2).
3. **Root verification.** The root level is the single node at `depth`
   (`root_level = ceil(log_radix N)`). The root coordinator harvests that proof
   and verifies it against the level-`depth` circuit VK. It performs **no** L1
   settlement (no signer/RPC/verifier wired); it refuses to fabricate a dispatch
   and reports `"l1_settlement": "not_configured"`.

**Why the tree (not the chain)?** The chain circuit (`BinaryTreeChainCircuit` /
the sequential reducer used by `bench.rs`) folds *i* on top of fold *(i-1)* — it
has O(N) sequential depth and a single VK, so it cannot scale out. The tree's
sibling-independence is exactly what lets aggregation fan across pods, at the
cost of one VK per level (which the fixed-VK recursion pins). See ADR §1–§2 for
depth, the `dummy_proof` lesson, and the Option A vs Option B discussion.

---

## 2. Two orchestration models

Both drive the **same** `prover-node` binary and the **same** transport
primitives. Pick one per deployment — do **not** run both against the same
Pub/Sub subscription + bucket at once. The annotated comparison also lives in
`infra-as-code/kubernetes/README.md`.

### Phase-locked per-level Jobs (#293 / #297)

Typed Kubernetes Indexed `Job`s, one per tree level, role baked into the
command:

- `render_pod_spec.py` (default mode) emits an Indexed leaf `Job`
  (`lighter-leaf-worker`), one Indexed tree-node `Job` per level
  (`lighter-tree-aggregator-l{L}`), a root-coordinator `Job`
  (`lighter-root-coordinator`), and a machine-readable `*-tree.plan.env`.
- `batch/v1` Jobs have **no** native inter-Job gating, so
  `infra-as-code/cloudbuild-distributed.yaml` (`ENGINE=gke`) applies each level
  **in order**, blocking on `kubectl wait --for=condition=complete` between
  levels. The `plan.env` is the single source of truth for depth/node-counts.
- Uses the explicit `leaf-worker` / `tree-node` / `root-coordinator`
  subcommands.

### Fungible pool (#298 / #300 / #302)

One pod shape, role-per-message, pull-based, KEDA-autoscaled:

- A single `Deployment` (`lighter-fungible-prover`) runs
  `prover-node work --transport=pubsub`. Each pod pulls one `WorkDescriptor`
  (leaf prove **or** level-`k` fold) with flow-control = 1, proves it, commits
  the proof bytes idempotently to GCS via the native `ifGenerationMatch=0` CAS,
  acks, then pulls the next ready task — until the dynamic-depth root exists.
- **Bootstrap (seeder):** a worker pod is a pure consumer — it does **not** seed.
  Exactly one one-off seeder publishes the N leaf descriptors with
  `prover-node work --transport=pubsub --seed` (connect → `seed_leaves` → exit);
  readiness gating then publishes the fold tasks level-by-level as children
  commit. For `--transport=local` the seed is performed inline before the loop,
  so the local smoke is self-contained.
- **Readiness gating in the data:** committing a child output advances its
  parent's completion count and publishes the parent fold descriptor exactly
  once when the parent's real-child quota is met (`commit_and_gate`).
- **Autoscaling:** a KEDA `ScaledObject` scales on Pub/Sub backlog.
  `minReplicaCount = baseload` (always-on, ≈60% of peak parallel width, **never
  0**); `maxReplicaCount = baseload + burst` (Spot capacity).
- **Graceful drain (mandatory, ADR §7):** an in-binary SIGTERM handler
  (`bench::shutdown`) plus `terminationGracePeriodSeconds` ≥ max prove time.
  On scale-down / Spot preemption the loop **stops pulling new work, finishes +
  commits + acks the in-flight lease, then exits** — never killed mid-prove.

### When to use which

| Aspect | Fungible (KEDA) | Phase-locked Jobs |
|---|---|---|
| Pod shape | One (`work`) | Typed per role |
| Work assignment | Pull / work-stealing | Indexed Job completions |
| Autoscaling | KEDA on backlog | GKE autoscaler on pending pods |
| Scale-to-zero | No (baseload floor) | Jobs complete + clear |
| Cross-level ordering | Readiness gating in data | Cloud Build `kubectl wait` |
| Queue dependency | Pub/Sub + KEDA | None |
| Idle waste | Minimal (dial back in) | Possible (level barriers) |
| ADR status | Recommended target | Valid alternative |

---

## 3. Local / dev quickstart (runnable today, no cloud)

### Build

The root `Makefile` builds the **bench** binary; `prover-node` is built directly
with cargo (this is exactly what `cloudbuild-distributed.yaml` does for the MIG
smoke path):

```bash
# bench harness via Makefile (-> bench/bench, RUSTFLAGS=-C target-cpu=native)
make local-build

# the distributed proving daemon
cargo build --release --bin prover-node
```

> **Stack size.** plonky2 recursion needs a large prover-thread stack. The
> fungible pod manifest sets `RUST_MIN_STACK=4294967296` (4 GiB); export the same
> locally if you hit a stack overflow while proving:
> `export RUST_MIN_STACK=4294967296`.

### Path B shape: explicit leaf → tree → root (radix-2, N=2)

This mirrors the MIG smoke pipeline in `cloudbuild-distributed.yaml`. Proofs are
written to / read from `reports/stark_proofs/`.

```bash
cargo run --release --bin prover-node -- leaf-worker --chunk-idx 0 --tx-per-proof 1
cargo run --release --bin prover-node -- leaf-worker --chunk-idx 1 --tx-per-proof 1
cargo run --release --bin prover-node -- tree-node --level 1 --node-idx 0 --radix 2 --leaf-count 2 --tx-per-proof 1
cargo run --release --bin prover-node -- root-coordinator --block-number 1042 --radix 2 --leaf-count 2 --node-idx 0 --tx-per-proof 1
```

The root coordinator prints a `ROOT_PROOF_VERIFIED` telemetry line with
`"l1_settlement": "not_configured"` (honest: no settlement is wired). For a
deeper tree, raise `--leaf-count` (e.g. `--leaf-count 4` ⇒ depth 2, so you run
`tree-node --level 1` for each pair then `tree-node --level 2 --node-idx 0`
before the root coordinator).

### Path A shape: fungible local dispatch (3-knob UX, one command, verified root)

The `work` subcommand with `--transport=local` seeds the leaves, runs the full
readiness-gated dispatch loop in-process over the `LocalTransport`, and verifies
the dynamic-depth root — no cloud, no broker. The workload is expressed as
**three operator-facing knobs** (issue #310); the fragile internals
(`leaf_count`, `depth`, node geometry) are **derived** — you never hand-set them:

| Knob | Flag | Default | Meaning |
|---|---|---|---|
| **Blocks** `B` | `--blocks` | `1` | Replay the loaded block B times as B INDEPENDENT trees (each namespaced + independently verified). REPLAY, not a distinct-block corpus. |
| **Txs/block** `T` | `--txs-per-block` | `0` ⇒ all real txs | How many of the block's real transactions to prove per block (`T ≤ block tx count`). |
| **Txs/chunk** `C` | `--txs-per-chunk` (alias of `--tx-per-proof`) | `1` | Transactions per leaf. **Must evenly divide `T`.** |

Derived automatically: `leaf_count_per_block = ceil(T / C)`,
`depth = ceil(log_radix(leaf_count_per_block))`, total leaves `= B × T/C`.

```bash
# Smallest verified-root smoke (radix-2 back-compat, 2 leaves, depth 1):
cargo run --release --bin prover-node -- work --transport=local \
  --txs-per-block 2 --txs-per-chunk 1 --radix 2

# Default radix is 16 — omitting --radix uses the real-workload radix:
cargo run --release --bin prover-node -- work --transport=local \
  --txs-per-block 2 --txs-per-chunk 1
```

On seed (and worker start) the binary prints the **effective plan** so you see
exactly what runs, e.g.:

```
[plan] Block has 500 txs. blocks=1, txs-per-block=2, txs-per-chunk=1, radix=16 →
       2 leaves/block, depth 1, covering 2/500 txs. transport=local store=reports/stark_proofs/
```

then `FUNGIBLE_DISPATCH_ROOT_VERIFIED`. The same loop honours SIGTERM with a
graceful drain (it stops pulling, finishes the in-flight lease, and reports
`FUNGIBLE_DISPATCH_DRAINED_ON_SHUTDOWN` rather than fabricating a root).

#### Radix-16 default

The CLI default radix is **16** in the `work`, `tree-node`, and
`root-coordinator` roles **and** in `render_pod_spec.py` — the real workload
radix (shallow trees: 100 leaves → depth 2, 500 leaves → depth 3, comfortable
~2.2 GB RAM per fold). **radix-2 stays fully supported**; pass `--radix 2`
explicitly for the tiny smoke / back-compat path.

#### Fail-fast validation (on the seeder/laptop, NOT in the pod)

Every misconfiguration is rejected **before** any seed/pod action with a clear,
actionable message (exit 2, never an in-pod panic):

```bash
$ prover-node work --transport=local --txs-per-chunk 7
Invalid workload config: --txs-per-chunk C=7 must evenly divide --txs-per-block
T=500, else the final chunk is short and the in-pod witness generation
zip_eq-panics. Valid divisors of 500: 1,2,4,5,10,20,25,50,100,125,250,500.
```

The divisor list is computed from the **real loaded block** tx count, not
hardcoded. The other rejected cases are `T > block tx count`, a derived
`leaf_count` that exceeds the available chunks (would address a non-existent
chunk in-pod), and `B < 1`.

#### B>1 replay semantics + namespacing

`--blocks B>1` replays the SAME `bench_test.json` block B times as B
**independent** trees. Each replay is namespaced (`store=reports/stark_proofs/block_<b>/`
locally; object-prefix `<base>block_<b>/` on GCS) so identical-content proofs
land under **distinct** keys and cannot dedup via the CAS `AlreadyExists` path
(which would silently collapse the load into one tree). Each replay yields its
own independently-verified root:

```bash
cargo run --release --bin prover-node -- work --transport=local \
  --blocks 2 --txs-per-block 2 --txs-per-chunk 1 --radix 2
# → block_0/ root verified, block_1/ root verified (two independent roots)
```

Replays are aggregated **within** a block only, never across blocks.

#### Seeder↔worker drift guard (shared run-config)

On the `--transport=pubsub` path the one-off seeder writes a single
source-of-truth run-config (`reports/run_config.json`) capturing
`blocks/txs-per-block/txs-per-chunk/radix/leaf_count/depth/topic/subscription/
bucket/object-prefix` (mirroring the `plan.env` pattern from #297). Each worker
reads it and **refuses to run** (exit 2, clear message) if its own derived
geometry (`radix`/`leaf_count`/`tx_per_proof`) doesn't match what was seeded —
so a worker can never silently prove the wrong tree. When the run-config is
absent the per-descriptor geometry pulled off the queue still governs each fold,
so correctness is preserved either way.

### Make targets that wrap this

```bash
make test-distributed-fast   # 2-minute scaled local distributed simulation
make lint-reports            # anti-fabrication guard (#282)
```

---

## 4. Config & tuning knobs

The `work` subcommand exposes the workload as **three operator-facing knobs**
(issue #310); everything fragile below the line is **derived + validated**, never
hand-set:

| Knob | Meaning | Where it's set |
|---|---|---|
| `blocks` (B) | **3-knob** replay count: prove the same block B times as B independent, namespaced trees (`block_<b>/`). Default 1. NOT a distinct-block corpus. | CLI `work --blocks` (default 1) |
| `txs-per-block` (T) | **3-knob** how many of the block's real txs to prove per block. Default 0 ⇒ all real txs. `T ≤ block tx count`. | CLI `work --txs-per-block` (default 0/all) |
| `txs-per-chunk` (C) | **3-knob** transactions per leaf. Canonical flag `--tx-per-proof`; `--txs-per-chunk` is an alias. **Must evenly divide T.** | CLI `work --txs-per-chunk` / `--tx-per-proof` (default 1); Makefile `CHUNK`; cloudbuild `_CHUNK_SIZE` |
| `radix` | Tree fan-in (children per node). Circuit max fan-in is 16 (`HEX_RADIX`). **Default 16** (real workload). radix-2 still works when set explicitly. | CLI `--radix` (default 16); cloudbuild `_RADIX` (16); render `--radix` (default 16) |
| `prestate-corpus-path` | Path to the committed per-tx **pre-state corpus** each leaf reads its chunk's pre-state from instead of re-proving every prefix chunk (issue #316; replaces the O(N²) prefix-replay tail). On a corpus miss the leaf **falls back to prefix replay** — pre-state is never fabricated. | CLI `work --prestate-corpus-path`; env `LIGHTER_PRESTATE_CORPUS`; **default** `bench/corpus/cap-block/captured_corpus.gz` (with `/data` + `bench/`-relative fallbacks) |
| `leaf-count` (N) | **DERIVED** as `ceil(T / C)` per block by `work` — *not* an operator knob there. Still an explicit input for the phase-locked `tree-node`/`root-coordinator` subcommands + `render_pod_spec.py`. | Derived in `work`; CLI `--leaf-count` on `tree-node`/`root-coordinator`; cloudbuild `_LEAF_COUNT`; render `--leaf-count` |
| depth | **DERIVED**: `ceil(log_radix N)`. **Never set directly.** | Computed in `prover_node.rs` (`tree_depth`) and `render_pod_spec.py` (`tree_depth`) |
| `ack_deadline` | Pub/Sub lease ≈ 2×P99 prove time. Default **180s** (radix-16 fold ≈80s on `c3d-highcpu-16`, live 500-tx run; hardware-dependent). | CLI `--ack-deadline` / `PROVER_PUBSUB_ACK_DEADLINE`; render `--ack-deadline` |
| `baseload` / `burst` | KEDA min / (max-min) replicas. baseload ≈60% of peak width (always-on). | render `--baseload` / `--burst`; Terraform `fungible_baseload_node_count` / `fungible_burst_max_node_count` |

> **`render_pod_spec.py --blocks` is NOT the 3-knob `work --blocks`.** The
> renderer's `--blocks` caps Indexed-Job *parallelism* (concurrent pods per
> level) for the phase-locked path; it does **not** replay the block. The
> 3-knob `--blocks` replay count is a `prover-node work` flag. The two are
> deliberately distinct and documented as such in the script.

**Pre-state corpus (issue #316 — replaces the O(N²) prefix-replay tail).** Each
leaf needs the ledger pre-state after all prior chunks' txs. Rather than
recompute it by re-proving every prefix chunk on every leaf (an **O(N²)** leaf
phase), the leaf **reads** that pre-state from a committed per-tx positional
corpus and proves only its own chunk. The corpus is S-independent: `at_chunk(C,
i) = snapshots[C·i]`, so the same per-tx array serves every `--txs-per-chunk`.
Pilot result: **bit-identical** leaf state, **~21× faster** leaf phase.

- **Default / where it is.** `bench/corpus/cap-block/captured_corpus.gz` (the
  per-tx pre-state of the bundled `bench/bench_test.json` cap block;
  gzip-framed JSON, schema 1.1; see `bench/corpus/cap-block/README.md`).
  Override with `--prestate-corpus-path` or `LIGHTER_PRESTATE_CORPUS`; the
  default resolves with `/data` (mounted) and `bench/`-relative fallbacks.
- **No regeneration to replay the same block.** A normal run **replays the same
  committed block**, so the committed corpus + the read path are all you need —
  there is **nothing to regenerate**.
- **When a NEW corpus IS needed** (only if `bench/bench_test.json` itself
  changes, or the corpus schema **MAJOR** bumps): the corpus *generation /
  harvest* code (the serial S=1 sweep that proves each single-tx step, plus the
  empty-index sibling-path harvester) is intentionally **not** in this branch —
  it lives on the **`parallel-v0.0.1-alpha`** branch. Only the read path is
  ported here. Regenerate per `bench/corpus/cap-block/README.md`.
- **Honest fallback.** A missing / corrupt / schema-MAJOR-incompatible corpus
  makes the loader return an error (never a fabricated snapshot); the leaf then
  falls back to prefix replay and logs which path it took. The `Batch` is
  identical on either path.

**Divisor guidance (C must evenly divide T).** For the real 500-tx block the
valid `--txs-per-chunk` values are the divisors of 500:
`1, 2, 4, 5, 10, 20, 25, 50, 100, 125, 250, 500`. A non-divisor is rejected
**fail-fast at seed time** (clear message listing the valid divisors, computed
from the real block), so the in-pod `zip_eq` panic from a short final chunk
cannot occur.

**`ack_deadline` guidance (hardware-dependent — re-derive per instance type from
real 2×P99).** The numbers below are the **measured live 500-tx Phase-1 run** on
`c3d-highcpu-16` (16 vCPU / 32 GiB), which supersede the earlier 32-core EPYC
pilot figures (folds were ≈10s there ⇒ a 60s default):

| Worker role | Measured P99 (c3d-highcpu-16) | Recommended `ack_deadline` (2×P99) |
|---|---|---|
| leaf / prefix-replay | ≈ **74 s** (max 73.65s) | ≈ **150 s** |
| radix-16 fold | ≈ **83 s** (max 83.26s) | ≈ **180 s** ← long pole |

The pool default is now **180s** so a single fungible worker image (any role per
message) never under-leases a radix-16 fold. The lease is *also* heartbeated via
`modifyAckDeadline` while proving (`WorkLease::extend`) — but the old 60s base
was *shorter than* a ≈80s fold, leaving **zero margin**: every fold relied
entirely on the heartbeat, and a single missed/delayed beat → redelivery
mid-prove → wasted duplicate work. Pub/Sub clamps to `[10, 600]s`. **Re-derive
this per instance type** — folds here are ~8× slower than the EPYC pilot.

---

## 5. Measured performance (live 500-tx Phase-1 run)

These are **real measured results** from a live Phase-1 GKE smoke test, not pilot
extrapolations. Use them to size `ack_deadline`, estimate wall time, and pick
`tx_per_proof`.

**Run shape:** 500 txs → 125 leaves at `tx_per_proof=4`, **radix-16**, on
**10× `c3d-highcpu-16`** (16 vCPU / 32 GiB) Spot workers.

| Metric | Result |
|---|---|
| **Total wall time** | **13.35 min** (800.96s) for the full 500-tx block → verified root |
| Effective speedup | ≈ **7.5×** (100.6 worker-min of proving compressed into 13.35 wall-min on 10 workers) |
| Phase overlap | Leaf and fold phases **overlapped** — async gating works; folds start as soon as their children commit |
| **Leaf proving** (125 tasks) | avg **42.55s**, min 12.03s, max 73.65s; total 88.65 worker-min |
| **Fold proving** (9 tasks = 8 L1 + 1 root, radix-16) | avg **79.89s**, min 68.97s, max 83.26s (CV ≈ 0.06, very tight); total 11.98 worker-min |
| GCS commit + CAS (`commit_and_gate`) | leaf writes avg 2.17s (1.63–3.04s), fold writes avg 2.39s (2.12–3.02s); ~4.9 worker-min ≈ **~5% of proving time → negligible** |

**Why the leaf spread is so wide (12s → 74s) — the prefix-replay tail.** Under
Option-A state threading, leaf *i* re-executes chunks `0..i` to reconstruct the
pre-state before proving its own chunk. Late leaves replay more prior chunks, so
their wall time grows roughly `O(N)` across the block. The spread is therefore an
**inherent property of prefix replay, NOT circuit warmup** (folds, by contrast,
are tight at CV ≈ 0.06). This is the dominant leaf cost at scale; the
**pre-state corpus** (#243 / #257) is the mitigation lever — precomputing/serving
per-leaf pre-states removes the replay and flattens the tail.

**Pick `tx_per_proof ≥ 4`, not 1, at scale.** A larger chunk means fewer leaves
for the same block (125 leaves at C=4 vs 500 at C=1), which **shortens the
prefix-replay tail** (fewer chunks to re-execute, lower max leaf time) and cuts
fold fan-out work — at the cost of a slightly heavier per-leaf circuit. At 500
txs, `C=4` clearly beat `C=1`.

**Hardware caveat.** `c3d-highcpu-16` is ~**8× slower per fold** than the earlier
32-core EPYC pilot (~10s folds there → ~80s here) and leaves prove ~**20–35×**
the pilot's ~2s (fewer cores + the prefix tail). **Prior pilot timings were
optimistic for this instance type** — always re-derive `ack_deadline` (2×P99) and
wall-time estimates from your own measured run.

---

## 6. Cloud (GKE) deployment — requires live infra

> **Every step here is a deliberate live operator action.** Terraform `apply`,
> image build+push, KEDA install, and `kubectl apply` all touch real cloud. The
> production transport backend is wired and pilot-verified, but executing the
> live dispatch loop end-to-end is `TODO(confirm-on-live-run)`.

1. **Provision infra (Terraform).** The GKE cluster + node pools live under
   `infra-as-code/terraform/` (`gke_cluster.tf`, the `proving_pod_fleet` module
   call in `mig_fleet.tf`, and the reusable
   `modules/proving_pod_node_pool/`). Key variables (`variables.tf`):
   - `orchestration_engine = "gke"` (vs `"mig"`; default `"gke"`),
   - `enable_fungible_pool = true` to provision the committed baseload +
     Spot burst fungible node pools (off by default; wired through the
     `proving_pod_fleet` module in `mig_fleet.tf`),
   - `fungible_baseload_node_count` (default 6),
     `fungible_burst_max_node_count` (default 80).
   Apply is a deliberate live step (`make cloud-gke-provision`, or the Terraform
   apply step inside `cloudbuild-distributed.yaml`).

   `cloud-gke-provision` forwards the fungible-pool variables straight through
   to Terraform (`cloud.sh` flag → `cloudbuild-provision.yaml` substitution →
   `TF_VAR_*`). The smoke-test (Phase-1 static fleet) command is:
   ```bash
   bash infra-as-code/scripts/cloud.sh cloud-gke-provision \
     --arch=c3d \
     --enable-fungible-pool=true \
     --fungible-baseload-node-count=10 \
     --fungible-burst-max-node-count=0
   ```
   Omitting a flag leaves the corresponding Terraform variable at its default
   (the fungible pools stay OFF), so the no-flags `make cloud-gke-provision` and
   `--arch=all` paths are unchanged. The flags map to
   `TF_VAR_enable_fungible_pool`, `TF_VAR_fungible_baseload_node_count`, and
   `TF_VAR_fungible_burst_max_node_count`.

2. **Build + push the image.** `make cloud-zkp-build` builds via
   `Dockerfile.zkp` (amd64) / `Dockerfile.zkp-arm64` (arm64), shipping the
   `prover-node` and `bench` binaries.

   > **Important:** the Dockerfiles build the **default (cloud-free)** binary
   > (`cargo build --release --bin bench --bin prover-node`). The fungible
   > `--transport=pubsub` path requires the image to be built with the `pubsub`
   > cargo feature (`cargo build --features pubsub`); the default build accepts
   > `--transport=pubsub` but **fails fast** with a clear error instead of
   > linking the GCP clients. Add the feature to the build args before a live
   > fungible run.

3. **Render manifests** (`infra-as-code/scripts/render_pod_spec.py`):
   - **Phase-locked (default):**
     ```bash
     python3 infra-as-code/scripts/render_pod_spec.py \
       --config config.toml --image default --arch c3d \
       --radix 16 --leaf-count 256 --blocks 2
     # -> *-leaf, *-tree-l{L}, *-tree-root manifests + *-tree.plan.env
     ```
   - **Fungible (`--emit-fungible`):**
     ```bash
     python3 infra-as-code/scripts/render_pod_spec.py \
       --config config.toml --image default --emit-fungible \
       --arch c3d --radix 16 --leaf-count 256 \
        --topic prover-folds --subscription prover-work \
        --baseload 6 --burst 80 --ack-deadline 180
      # -> *-fungible.rendered.yaml (Deployment) + *-fungible-keda.rendered.yaml (KEDA)
     ```

4. **Install KEDA (fungible path only):**
   ```bash
   helm repo add kedacore https://kedacore.github.io/charts && helm repo update
   helm install keda kedacore/keda --namespace keda --create-namespace
   ```

5. **Run the pipeline.** The phase-locked path is orchestrated by
   `infra-as-code/cloudbuild-distributed.yaml` (`_ENGINE=gke`): it terraform-
   applies, renders manifests, cleans the GCS proof dir, applies the leaf Job
   and waits, then applies + waits each tree level **in order** (the cross-level
   gate, driven from `plan.env`), then the root coordinator. Substitutions:
   `_ENGINE`, `_ARCH`, `_RADIX`, `_LEAF_COUNT`, `_BLOCK_CONCURRENCY`, `_IMAGE`,
   `_BENCHMARK_ID`. The default `_ENGINE=mig` path runs the local
   leaf→tree→root smoke instead. The Makefile entry point is
   `make cloud-run-distributed-cluster` (accepts `ENGINE=`, `ARCH=`, `BLOCKS=`,
   `CHUNK=`, `RADIX=`). For the fungible path, `kubectl apply` the rendered
   Deployment + ScaledObject (from `--emit-fungible`, or the annotated templates
   `infra-as-code/kubernetes/{fungible_pool,keda_scaledobject}.yaml`) onto the
   cluster.

---

## 7. The transport correctness contract (load-bearing)

The `WorkTransport` trait (`bench/src/transport/mod.rs`) is the durable contract
both backends implement: `LocalTransport` (dev/test) and `PubSubGcsTransport`
(production, compiled only with `--features pubsub`).

> **Production claim/commit MUST use the GCS native-API `ifGenerationMatch=0`
> compare-and-swap (verified exactly-one-winner). It MUST NOT use gcsfuse
> `O_EXCL`.** A pilot **refuted** gcsfuse `O_EXCL`: gcsfuse implements create as
> a non-atomic stat-then-create, so two pods on different nodes both "win" and
> corrupt the object. The native `ifGenerationMatch=0` upload (precondition:
> object generation 0 ⇒ does not exist) is the only exactly-one-winner
> primitive. `LocalTransport` uses filesystem `O_EXCL`, which is atomic **only**
> on a single local filesystem — fine for single-node dev/test, never for the
> cross-node production path.

Two more invariants the trait bakes in:

- **Ack-after-commit.** A lease is acked only **after** the proof bytes are
  durably committed (`commit_output` returns `Committed`/`AlreadyExists`).
  Redelivered/duplicated descriptors re-prove and harmlessly observe
  `AlreadyExists` — never a half-written output, and the ack never happens on
  pull.
- **Graceful drain.** Pull flow-control = 1, heartbeat the lease via `extend()`
  (`modifyAckDeadline`) while proving, `nack()`/drop ⇒ redelivery, and on
  SIGTERM stop pulling but finish + ack the in-flight lease before exit.

---

## 8. Where it all plugs in

A map from the thing you run to the thing it drives:

| Make target | Script / file | Manifest / Terraform | Binary subcommand |
|---|---|---|---|
| `make local-build` | `bench/Makefile` (`build`) | — | builds `bench` |
| *(direct cargo)* | — | — | `cargo build --release --bin prover-node` |
| `make test-distributed-fast` | `infra-as-code/scripts/container.sh` | — | `prover-node work --transport=local` (scaled) |
| `make cloud-zkp-build` | `cloud.sh` → `cloudbuild-zkp.yaml`, `Dockerfile.zkp[-arm64]` | — | ships `prover-node` + `bench` |
| `make cloud-run-distributed-cluster` | `cloud.sh` → `cloudbuild-distributed.yaml` | Terraform apply; rendered Jobs | `leaf-worker` / `tree-node` / `root-coordinator` (GKE) or local smoke (MIG) |
| `make cloud-gke-provision` | `cloud.sh` | `terraform/{gke_cluster,mig_fleet}.tf`, `modules/proving_pod_node_pool/` | — |
| *(render, phase-locked)* | `render_pod_spec.py` (default) | `prover_pod_unit-{leaf,tree-l*,tree-root}.rendered.yaml` + `*-tree.plan.env` | `leaf-worker` / `tree-node` / `root-coordinator` |
| *(render, fungible)* | `render_pod_spec.py --emit-fungible` | `*-fungible.rendered.yaml` + `*-fungible-keda.rendered.yaml`; templates `kubernetes/{fungible_pool,keda_scaledobject}.yaml` | `prover-node work --transport=pubsub` |
| `make lint-reports` | `check_no_fabricated_reports.sh` | — | — (anti-fabrication guard, #282) |

Disabled `test-*` targets (`test-t2d-hypothesis`, `test-gke-tax`,
`test-capstone`) fail loudly by design (#282) — use
`cloud-run-distributed-cluster` for a real run.

Config flows from `config.toml` (`[proving_pod.<arch>]` profiles, `[gcp.*]`
project/registry/bucket/pubsub) into `render_pod_spec.py`, which resolves the
image URI and pod resources and writes the manifests.

---

## 9. Cross-references

- **ADR:** `docs/decisions/ADR-distributed-recursive-proving-architecture.md`
  (chain-vs-tree, dynamic depth, transport, autoscaling, the ack_deadline
  "Amendment (real prove-time measurement)" for the EPYC pilot and
  "Amendment 2 (live 500-tx GKE run on c3d-highcpu-16)" for the real cloud
  numbers + the 180s default). See also the companion
  `docs/decisions/ADR-distributed-gke-topology.md`.
- **Design discussion:** #287 (architecture review + open cryptographer questions).
- **Issue trail:** #281 (reduction-tree fixed-VK hardening), #283 (prover-node
  honesty / GKE topology), #288 / #289 (multi-level recursive aggregation),
  #291 / #293 (dynamic-depth multi-level aggregation), #295 / #297 (cross-level
  Job gating), #298 / #300 (work-transport abstraction + fungible dispatch),
  #302 / #303 (KEDA backlog autoscaling + baseload/burst manifests).
- **K8s comparison:** `infra-as-code/kubernetes/README.md`.
