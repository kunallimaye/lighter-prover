# ADR-0003: Prover-cell streaming architecture

**Status**: Proposed
**Date**: 2026-06-11
**Issues**: #3 (Phase 2 design — this ADR is its design-doc task), informed by spikes #59/#60/#64, measurements on #10/#60, synthesis in Discussion #58
**Relationship to ADR-0001**: ADR-0001 governs the *comparison fleet* (heterogeneous shapes, batch bench, embarrassingly parallel) and remains authoritative for it. This ADR introduces a second fleet kind — the *load fleet* of prover cells — which intentionally breaks ADR-0001's "no inter-VM communication" invariant for that fleet only.

## Context

The streaming bench (#48 feeder, #49 consumer) measures L1-L3 capacity of a colocated serial pipeline. The deployable distributed prover has a different shape, constrained by the 8-layer circuit topology (#1) and the following **measured** facts (AMD EPYC 7B13, 32 cores; sources: Discussion #6, #60 spike, #10 Stage-A measurement, #64 probe):

| Quantity | Value | Source |
|---|---|---|
| L1 per-chunk prove | step function of circuit-degree bracket: ~4.7 s (2^17, S≤8) / ~10.8 s (2^18, S 12-20) / ~21 s (2^19, S 22-32) | #60 (measured) |
| L2 chain step | ~0.49-0.52 s, flat S ∈ {1..32}; circuit is 2^14 (not 2^13 as older comments claim) | #60, #64 (measured) |
| L2 tree-merge step | ≈ L2 chain step; merge circuit fits today's exact 2^14 self-shape with 28% headroom | #64 probe (build-validated) |
| L4 block prove | 5.14 s (degree 2^17) | #10 Stage-A (measured) |
| L5 per-block fold | 0.94 s; strictly one block per fold; no batching in circuit | #10 Stage-A (measured) |
| Optimal chunk size | S=20 (top of the 2^18 bracket): 500-tx block wall 12.8 s vs 40.6 s at S=6 | #60 (measured), unlocked by #63 / PR #65 |
| Mainnet peak demand | 2,213 tx/s observed ⇒ ~500-tx block every ~226 ms | trace data (#48) |

Implications: a single sequential L2 fold caps one prover at ~8-12 tx/s; a single sequential L5 folder caps the *entire system* at ~1.06 blocks/s vs 4.4 needed at peak. Horizontal scaling alone cannot fix L5; associative folding can fix both.

## Decisions

### D1 — The unit of scaling is the prover cell
A **cell** = one host, one Rust process: an orchestrator thread plus M worker threads sharing resident proving keys (circuit construction takes minutes; workers must be persistent; sibling *processes* would multiply proving-key RSS by M). Worker panic isolation is provided by the outer layer (block-level redelivery), not process boundaries.

### D2 — Two-queue topology; chunking is the orchestrator's job
- **Outer queue (block dispatch)**: feeder publishes block events to a Pub/Sub topic; N cell orchestrators competing-pull one subscription with maxOutstandingMessages=1; ack **after** the block proof is emitted (at-least-once on whole blocks; a cell death costs one block of redelivered work). No coordinator service exists; pull-balancing is the scheduler.
- **Inner queue (chunks)**: the orchestrator chunks its block in memory (`ceil(tx_count/S)` chunks), feeds an in-process work queue drained by M workers, collects chunk proofs into the L2 fold. RAM-only; nothing intra-cell crosses a network or GCS.

### D3 — L2 uses tree-fold (dedicated merge circuit, shape b2)
Per #59 (feasibility GO) and #64 (gate budget GO): leaf circuit = today's `BlockTxChainCircuit`; a sibling chain-merge circuit verifies two chain proofs of adjacent ranges (fits the existing 2^14 self-shape, 28% headroom; requires one added 4-element PI — range-start `old_account_delta_tree_root` — and no plonky2-fork surgery). Serial L2 latency drops from N·0.5 s to ~log₂(N)·0.5 s. The unified leaf+merge variant (b1) is rejected (2^15 ⇒ ~2× step cost). Implementation tracked in the issue filed from #64.

### D4 — S=20 is the streaming sweep anchor
Validated by #60 and unlocked by #63 (PR #65). Comparison-fleet sweeps remain S ∈ {1,2,4,6} for historical comparability; streaming/cell experiments anchor at S=20 and may sweep {4, 6, 20}.

### D5 — A block-proof aggregation tree feeds L5
The measured 0.94 s serial L5 fold cannot meet the 226 ms peak cadence (4.15× over). L5's circuit is the same cyclic pattern as L2 one degree-bit larger, and the #64 probe de-risks the same merge construction at that scale. Block proofs from cells are merged in a log-depth tree whose root (or low-rate stream) feeds the sequential L5 folder. Design details deferred to a dedicated issue after the L2 merge circuit proves out.

### D6 — Data-plane rules
- **GCS is showback-only**: run manifests, BENCH_EVENT JSONL, final proof artifacts. Never in the per-proof critical path (a 100-300 ms GCS round-trip is a ~100% tax on a 0.5 s fold step).
- **Witnesses via mounted read-only corpus** (image layer or volume), resolved by `{height, witness_index}` lookup; `witness_fetch_ms` is a dedicated BENCH_EVENT field so witness acquisition is always separately accountable (#61). Witnesses never travel through the trace or the message bus.

### D7 — Platform: MIG with a quarantined platform seam
The load fleet runs as a Managed Instance Group of identical cells (one instance template per run; `--size N`; autoscaling on Pub/Sub backlog via Cloud Monitoring metrics covers the elasticity experiment; autohealing + redelivery covers the chaos experiment). GKE (Autopilot custom compute classes) was evaluated and deferred: whole-node billing neutralizes its economics for 32-64 vCPU CPU-saturating pods; kubelet reservations and runtime deltas pollute cross-fleet benchmark comparability; the ops surface contradicts the repo's shell+gcloud idiom (ADR-0001 §D6). All platform-specific logic is quarantined in one lifecycle lib (`platform-mig.sh`-style) and the run manifest carries a `platform` field, so a future GKE backend is a new lib, not a redesign. **Revisit triggers**: (a) the production prover commits to Kubernetes; (b) the rig becomes always-on (continuous load regression); (c) concurrent multi-experiment demand.

### D8 — Sweep sets
M (workers per cell) ∈ {1, 2, 4, 8, 16}; S per D4; cell count N sized from measured cell wall (see Consequences).

## Consequences

**Capacity arithmetic (500-tx block, peak ~226 ms cadence; measured inputs):**

| Configuration | Cell block wall | Cells at peak (floor) |
|---|---|---|
| S=6, serial L2 (status quo) | ~40.6 s + 5.1 s L4 ≈ 46 s | ~200 |
| S=20, serial L2 (#63 landed) | ~12.8 s + 5.1 s ≈ 18 s | ~80 |
| S=20 + L2 tree-fold (D3) | ~10.8 s L1 (M≥25) + ~2.5 s L2 + 5.1 s L4 ≈ 16 s (L1+L4-bound) | ~70 |

Plus the D5 aggregation layer in place of a serial L5 folder. Practical sizing adds 1.5-2× headroom for queueing margin and failure recovery. Cell RSS at S=20: ~9.4 GB for L1/L2 (#60) + L4/L5 keys (~5.6 GB peak observed in the Stage-A run) — comfortably inside 32-64 GB hosts.

**What this overturns**: issue #1's "L2 must run sequentially on a single process" described the linked-list driver, not a circuit constraint (#59 finding: ordering is enforced purely by state-root equality — associative). Issue #1 should gain a correction note when D3's implementation lands. Older comments claiming the chain circuit is 2^13 are also corrected (#64: it is 2^14).

**What this defers/retires**: #50 as originally scoped is retired (its transport survives as D2's outer queue, post-#3); L6-L8 driver work (Stage B of #10) is off the per-block critical path (once-per-batch); witness realism beyond the mounted corpus remains the acknowledged fidelity limit.

## Risks

1. The merge circuit's +4 PI perturbs the cyclic fixed point and three PI-index maps shared with L4 — mechanical but fiddly; mitigated by the A/B acceptance criterion (tree-folded proof verifies under the same L4).
2. The pre-L5 aggregation tree (D5) is build-validated only by analogy (L2-shaped probe); its own fixed point at 2^15 needs its own probe before implementation.
3. L4 at 5.14 s is now ~⅓ of the cell wall; if it becomes the binding term after D3, intra-cell L4 pipelining (prove block H's L4 while H+1's L1 runs) is the next lever — not designed here.
4. All measurements are single-machine (EPYC 7B13); cross-shape variance (the comparison fleet's domain) may shift constants but not structure.
