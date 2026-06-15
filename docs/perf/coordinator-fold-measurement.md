# Coordinator L2 merge fold — single-box measurement note

> **Status:** measurement record (issue #195). Corrects the framing in PR
> #194's close-out without retracting the implementation, which stays merged
> and is correct.

## Topology distinction (production vs single-box harness)

In the production distributed prover, the work is split across two role
classes (ADR-0006 §2 / §6):

- **Cells** prove L1 (`BlockTxCircuit`) + L2-leaf (`BlockTxChainCircuit`)
  on their own boxes.
- **Coordinator** is the box that ONLY runs the k–1 merges
  (`BlockTxChainMergeCircuit`) plus the one L4 (`BlockCircuit`).
  It does NOT prove cells.

The single-box harness (`bench/tests/distributed_fold_e2e.rs`,
`make e2e`) runs **both** roles in a single process: the cell phase
proves k real L1+L2-leaf proofs, then the coordinator phase folds them.
The fold timing is measured separately from the cell phase, so the cell
work does not overlap with the fold in wall time. The single-box harness
is therefore a correct reproduction of the coordinator's INPUT (real
leaves from real cells), but it cannot reproduce the coordinator's
DEPLOYMENT TOPOLOGY (the coordinator is the only thing on its box, and
multi-node fan-out gives each concurrent merge its own box).

## What PR #194 changed and what it measured

[PR #194](https://github.com/kunallimaye/lighter-prover/pull/194) (squash
`50e10e3`) parallelized `fold_merge_tree` in `bench/src/bin/bench.rs`:
each tree LEVEL is proven concurrently across an owned rayon pool, with
the final proof asserted bit-identical to the serial fold for any worker
count (determinism). `--l2-workers 1` keeps the byte-for-byte serial
path; `--l2-workers > 1` opts into the parallel path.

PR #194's close-out reported single-box numbers from `distributed_fold_e2e`
at K=8 and K=16 and concluded "no realized speedup on a single 32-core
box; parallel path opt-in." That framing conflates two things:

1. **(true)** On a single 32-core box, parallel is slower than serial.
2. **(misleading)** This implies the parallel path is not useful for the
   production coordinator topology. It actually IS the structural
   prerequisite for multi-node coordinator fan-out (issue #113); the
   single-box numbers do not falsify the design, they just measure the
   wrong thing for the production claim.

## Why parallel loses on a single 32-core box

Plonky2's prover dispatches its internal `par_iter` calls into rayon's
CURRENT thread pool (via `plonky2_maybe_rayon`). With no scoping that is
the process-wide GLOBAL pool of all cores, and a single merge already
saturates the box. When the parallel fold runs N=workers merges
concurrently via `pool.install(...)` on an outer N-thread pool, plonky2's
intra-merge `par_iter` calls see the OUTER pool as the current pool and
get throttled to N threads SHARED across all N concurrent merges. The
result: N merges each effectively get ~1 core, vs 32 cores serially.
Even capping each merge in its own `num_cpus / N`-thread inner pool (so
N concurrent merges land on disjoint core slabs) does not recover the
serial wall on a single box — plonky2 does not parallelize the merge
prove perfectly linearly with cores, so 8 × (merge @ 4 cores) > 1 ×
(merge @ 32 cores).

The win for the parallel layout is **multi-node coordinator fan-out**
(issue #113), where each concurrent merge runs on its OWN coordinator
box with its OWN 32 cores. There is no contention; the per-merge wall
stays ~constant; and the fold critical path collapses from `(k–1)` serial
merges to `depth × per_merge` (`log2(k)` × ~0.5s vs `(k–1)` × ~0.5s).

## Corrected coordinator-only single-box numbers

Measured on a single 32-core box with the `coordinator_fold_bench`
micro-bench (`bench/tests/coordinator_fold_bench.rs`,
`COORD_FOLD_BENCH=1 ...`). The micro-bench builds k REAL L2-leaf proofs
once, then times JUST `fold_merge_tree` over the same leaves three ways
(serial, parallel-uncapped, parallel-capped-per-merge). The leaf-build
phase is EXCLUDED from every fold timing. The cell phase has fully
completed before any fold timing begins, so there is no concurrent
cell-vs-fold contention — this is a true coordinator-only profile in
wall-time terms (though still a single-box process).

Configuration: S=4, num_cpus=32, workers=8, per_merge_threads=4 (= 32/8).

| K  | depth | merges | serial (ms) | parallel-uncapped (ms) | parallel-capped (ms) |
|----|-------|--------|-------------|------------------------|----------------------|
| 8  | 3     | 7      | 3251        | 6876 (0.47×)           | 6195 (0.52×)         |
| 16 | 4     | 15     | 6863        | 14537 (0.47×)          | 8534 (0.80×)         |

The per-merge cap closes most of the gap to serial at K=16 (from 0.47×
uncapped to 0.80× capped) but does not cross over to a true win on this
32-core box. The cap consistently outperforms the uncapped variant; if a
parallel path is opted into on a single box, the capped scheduling is
strictly better than the uncapped one.

### Honest conclusion

On a single 32-core box, **neither parallel variant beats serial**, even
with the per-merge pool cap. The cap meaningfully helps — at K=16 it
goes from 0.47× (uncapped, 14.5s) to 0.80× (capped, 8.5s) vs the 6.9s
serial baseline — but does not cross over to a true win, because
plonky2 does not parallelize the merge prove perfectly linearly with
cores: 8 concurrent merges each on a 4-core slab still wall-clock-loses
to 1 merge at a time on the full 32-core box. This is consistent with
PR #194's "no single-box speedup" finding, and it sharpens the framing:

- The single-box bench shows what the **parallel scheduler costs** when
  you actually run it on one box (it shouldn't be the default there).
  `--l2-workers 1` is the correct setting for any single-box deployment.
  If a parallel path IS used on a single box, the per-merge cap is
  strictly better than the uncapped scheduling.
- The parallel scheduler's REAL VALUE is the structural unlock for
  **multi-node coordinator fan-out** (issue #113). On N coordinator
  boxes, each running ONE concurrent merge at a time on its OWN full
  32 cores, the per-merge wall stays serial-like and the critical path
  collapses from `(k-1) × per_merge` to `depth × per_merge`.
- The per-merge pool cap remains a useful knob if a single
  larger-than-32-core box (e.g. 96 cores) hosts the coordinator and we
  want to overlap a small number of merges. That is also future work.

### What is NOT in this measurement

- We did NOT measure on a >32-core box. The per-merge pool cap might pay
  off there. Filing as follow-up under issue #195 if needed.
- We did NOT measure multi-node coordinator fan-out. That is issue
  #113's scope and requires real distributed infra.
- We did NOT change the default `--l2-workers` value. Any change to the
  default is its own decision and a separate PR.

## Reproduce the measurement

```sh
# Coordinator-only fold bench at K=8 (default; single-machine, ~75 s):
COORD_FOLD_BENCH=1 COORD_FOLD_BENCH_S=4 COORD_FOLD_BENCH_K=8 \
  cargo test -p bench --release --test coordinator_fold_bench \
  -- --ignored --nocapture --test-threads=1

# At K=16 (~3 min on 32 cores):
COORD_FOLD_BENCH=1 COORD_FOLD_BENCH_S=4 COORD_FOLD_BENCH_K=16 \
  cargo test -p bench --release --test coordinator_fold_bench \
  -- --ignored --nocapture --test-threads=1
```

Look for the `[coord-fold-bench] RESULT ...` and
`[coord-fold-bench] SPEEDUP ...` lines.

## Refs

- PR #194 — parallel fold implementation (stays merged; correct).
- Issue #193 — original tracking issue for PR #194 (closed; not reopened).
- Issue #195 — this measurement correction (re-measure on a
  coordinator-only profile and update the record).
- Issue #113 — multi-node coordinator pool (the design slice the
  parallel fold actually unlocks).
- Issue #179 — distributed coordinator WS this lives in.
- ADR-0006 — distributed-prover-conductor (production topology source).
