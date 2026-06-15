// Copyright (c) Elliot Technologies, Inc.
// SPDX-License-Identifier: BUSL-1.1

//! Cross-machine merge-tree fold fan-out (issue #198) — the DISTRIBUTED fold
//! topology.
//!
//! ## Why this lives in the library (not the bench binary)
//!
//! The single-box fold (`fold_merge_tree` in `bench/src/bin/bench.rs`) lives in
//! the BINARY and is unreachable from an integration test. To make the
//! distributed fold's KEY correctness property — *the distributed-path fold is
//! bit-identical to the in-process fold of the same leaves and VERIFIES* —
//! genuinely testable through the real task-emitter + proof-store transit path,
//! the distributed driver lives HERE, in the library, behind two seams:
//!
//!   - [`FoldTransport`]: moves intermediate proofs between coordinators. The
//!     real implementation is Pub/Sub (merge-task plane) + the GCS proof store
//!     (`{height}/m/{level}/{index}` keys). The hermetic test implementation
//!     ([`InMemoryFoldTransport`]) keeps proofs in a map and runs each merge
//!     task on its OWN worker thread — exercising the SAME emitter / transit /
//!     barrier / re-sort path, just without a network or live GCS.
//!   - [`MergeFn`]: the SINGLE merge implementation, supplied by the caller.
//!     The binary passes a closure that calls `prove_merge_pair`; there is
//!     never a second copy of the merge circuit.
//!
//! ## Governing principle (issue #198)
//!
//! > Coordination and ACTUAL proof generation belong on INDEPENDENT workers so
//! > they are not fighting for the same CPU/resources.
//!
//! Each merge task is proven by ONE worker on its FULL core budget (plonky2
//! saturates all cores per individual proof). We scale by adding MORE workers,
//! NOT by cramming several proofs onto one box. There is **no in-process thread
//! rationing** on this path — the deprecated per-merge thread-cap is not used.
//!
//! ## Determinism contract (#193, preserved verbatim)
//!
//! The leader forms each level's pairs preserving the odd-proof carry-up
//! exactly as the in-process fold does, emits a merge task per pair, awaits the
//! level's results, then **re-sorts by the stable in-level `index`** before
//! forming the next level. The final folded proof is therefore BIT-IDENTICAL
//! regardless of which worker proved which merge or in what order results
//! arrive.
//!
//! ## Honest failure (#179, preserved)
//!
//! A failed/missing merge result returns `Err` and aborts the fold. No proof is
//! ever fabricated and a bad node is never carried up.
//!
//! ## NFR is instrumented, not gated (issue #198)
//!
//! The leader records per-level barrier walls + straggler deltas and the
//! transit (proof-store PUT/GET) cost into [`DistributedFoldOutcome`]. These
//! are measured to learn/tune; nothing here gates on a latency number.

use std::collections::HashMap;
use std::sync::Mutex;
use std::time::{Duration, Instant};

use crate::conductor::storage::merge_object_key;

/// A proof that can transit the fold's proof store: it must be (de)serializable
/// (the real transport ships `serde_json` of `ProofWithPublicInputs`, exactly
/// the #117 export format the cells already use) and cloneable (the in-process
/// transport stores it by value). Blanket-implemented for any qualifying type,
/// so the binary's `ProofWithPublicInputs<F, C, D>` satisfies it for free and
/// the hermetic test can use a tiny fake proof type.
pub trait MergeProof:
    Clone + Send + Sync + serde::Serialize + serde::de::DeserializeOwned + 'static
{
}
impl<T> MergeProof for T where
    T: Clone + Send + Sync + serde::Serialize + serde::de::DeserializeOwned + 'static
{
}

/// The SINGLE merge implementation, supplied by the caller (issue #198 — one
/// merge impl, never duplicated). Given the two input proofs and whether each
/// is itself a merge (`is_merge`), produce the merged proof. The binary's
/// closure calls `prove_merge_pair`; the hermetic test uses a deterministic
/// fake. `Send + Sync` so workers can run it concurrently sharing immutable
/// circuit data by reference.
///
/// The lifetime parameter lets the closure BORROW circuit data (the binary's
/// closure captures `&merge_target`/`&merge_data` by reference — those are
/// `Send + Sync` and live for the whole fold but are NOT `'static`).
///
/// Returns `Err` on an honest failure — never a fabricated proof.
pub type MergeFn<'a, P> = dyn Fn(&P, bool, &P, bool) -> anyhow::Result<P> + Send + Sync + 'a;

/// One node in the merge tree: a proof plus whether it is a merge (`true`) or a
/// leaf chain proof (`false`). Mirrors the binary's `TreeNode`.
type Node<P> = (P, bool);

/// The transport that moves intermediate proofs between coordinators and runs
/// merge tasks on independent workers. Abstracts Pub/Sub (merge-task plane) +
/// the GCS proof store so the SAME emitter/transit/barrier/re-sort path can be
/// exercised hermetically in-process.
///
/// The contract is the leader's view of one tree LEVEL: given a batch of merge
/// tasks (each already staged in the proof store under its input keys), run
/// them on independent workers and return each task's `(index, output_key,
/// is_merge, prove_ms)` — re-sorting and carry-up are the LEADER's job, not the
/// transport's. The transport never fabricates a result: a failed merge or a
/// missing input must surface as `Err`.
pub trait FoldTransport<P: MergeProof>: Send + Sync {
    /// Stage `proof` into the proof store under `key` (the PUT half of
    /// transit). Returns the measured round-trip on success.
    fn put(&self, key: &str, proof: &P) -> anyhow::Result<Duration>;

    /// Fetch the proof stored under `key` (the GET half of transit). Returns
    /// the proof and the measured round-trip on success.
    fn get(&self, key: &str) -> anyhow::Result<(P, Duration)>;

    /// Run one LEVEL's merge tasks on INDEPENDENT workers and return, per task,
    /// `(in_level_index, output_key, is_merge=true, prove_ms)`. Each task names
    /// its input keys and its output key; the worker GETs the inputs, applies
    /// `merge_fn` on its full core budget, PUTs the output, and reports. The
    /// leader supplies `merge_fn` so there is exactly one merge implementation.
    ///
    /// Honest-failure: if ANY task fails (missing input, merge error, failed
    /// PUT) the whole level returns `Err`.
    fn run_level(
        &self,
        tasks: &[LevelTask],
        merge_fn: &MergeFn<'_, P>,
    ) -> anyhow::Result<Vec<TaskResult>>;
}

/// One merge task the leader emits for a level (issue #198). `index` is the
/// stable in-level pair index the #193 determinism re-sort keys on.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LevelTask {
    pub height: u64,
    pub level: u64,
    pub index: u64,
    pub left_key: String,
    pub left_is_merge: bool,
    pub right_key: String,
    pub right_is_merge: bool,
    pub output_key: String,
}

/// One merge task's outcome, reported back to the leader by the transport.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TaskResult {
    pub index: u64,
    pub output_key: String,
    pub prove_ms: u64,
}

/// Per-level barrier instrumentation (issue #198, measured-not-gated). Emitted
/// from the leader's level loop.
#[derive(Debug, Clone, PartialEq)]
pub struct LevelBarrierMetric {
    /// 1-based merge level.
    pub level: u64,
    /// Number of merge tasks (pairs) in this level.
    pub tasks: usize,
    /// Whether an odd node was carried up at this level.
    pub odd_carry: bool,
    /// Wall from level release to all results in (the barrier wall).
    pub barrier_ms: u64,
    /// Slowest merge prove wall in the level (ms).
    pub slowest_prove_ms: u64,
    /// Median merge prove wall in the level (ms).
    pub median_prove_ms: u64,
    /// Straggler delta = slowest - median (ms). The cost of waiting for the
    /// slow worker at the barrier.
    pub straggler_ms: u64,
}

/// Outcome of a distributed fold (issue #198). Carries the final folded proof
/// plus FIRST-CLASS instrumentation (measured, never a gate).
#[derive(Debug)]
pub struct DistributedFoldOutcome<P> {
    /// The single block-chain proof produced by folding the leaves across
    /// workers — bit-identical to the in-process fold of the same leaves.
    pub final_proof: P,
    /// `true` when at least one merge fired (final proof carries the merge VK).
    pub final_is_merge: bool,
    /// Tree depth (number of merge levels). `0` for a single leaf.
    pub depth: usize,
    /// Total merge nodes proven across all levels.
    pub merges: usize,
    /// Summed per-merge prove wall across all workers (TOTAL WORK, not wall).
    pub merge_prove_total: Duration,
    /// Per-level barrier instrumentation (one per merge level).
    pub level_metrics: Vec<LevelBarrierMetric>,
    /// Total proof-store transit (PUT + GET) wall measured by the leader and
    /// workers — the cost of moving intermediate proofs between coordinators.
    pub transit_total: Duration,
    /// Largest intermediate-proof size (bytes) observed across all levels — to
    /// confirm the constant ~412 KB recursive-proof size holds up the tree.
    pub max_intermediate_bytes: usize,
}

/// Issue #198 (cross-machine fold fan-out): fold `leaves` into ONE block-chain
/// proof by emitting each merge pair as a TASK to independent workers and
/// transiting intermediate proofs through the proof store, instead of proving
/// every merge in this one process.
///
/// `height` keys the proof-store transit namespace. `leaf_keys` are the
/// proof-store keys the leaves are ALREADY stored under (leaf keys at level 0 —
/// the cells uploaded these); the leader stages intermediate outputs under
/// `{height}/m/{level}/{index}` ([`merge_object_key`]). `merge_fn` is the
/// single shared merge implementation.
///
/// The leader's level loop (which IS the M2 level-barrier — level n+1 is only
/// released once every level-n result has landed):
///   1. forms the level's pairs preserving the odd-proof carry-up exactly as
///      the in-process fold does;
///   2. emits a merge task per pair to the transport (the workers GET inputs,
///      prove on full cores, PUT outputs);
///   3. awaits the level's results (the barrier), records barrier/straggler;
///   4. RE-SORTS the results by stable in-level `index` (the #193 determinism
///      step) and forms the next level — carrying any odd node up unchanged.
///
/// Honest-failure: a failed/missing result returns `Err`; no proof is
/// fabricated and a bad node is never carried up.
pub fn fold_distributed<P: MergeProof>(
    height: u64,
    leaves: Vec<P>,
    leaf_keys: Vec<String>,
    transport: &dyn FoldTransport<P>,
    merge_fn: &MergeFn<'_, P>,
) -> anyhow::Result<DistributedFoldOutcome<P>> {
    if leaves.is_empty() {
        anyhow::bail!("distributed fold: no leaf proofs to fold");
    }
    if leaves.len() != leaf_keys.len() {
        anyhow::bail!(
            "distributed fold: {} leaves but {} leaf keys (must match 1:1)",
            leaves.len(),
            leaf_keys.len()
        );
    }

    // Each tree node tracks its (proof, is_merge) AND the proof-store key its
    // bytes live under, so the next level's task can name its inputs by key.
    // Level 0 nodes are the leaves under the keys the cells uploaded.
    let mut level: Vec<(Node<P>, String)> = leaves
        .into_iter()
        .zip(leaf_keys)
        .map(|(p, k)| ((p, false), k))
        .collect();

    let mut depth = 0usize;
    let mut merges = 0usize;
    let mut merge_prove_total = Duration::ZERO;
    let mut level_metrics: Vec<LevelBarrierMetric> = Vec::new();
    let mut transit_total = Duration::ZERO;
    let mut max_intermediate_bytes = 0usize;

    while level.len() > 1 {
        depth += 1;
        let this_level = depth as u64;

        // ---- Form pairs, preserving the odd-proof carry-up EXACTLY as the
        // in-process fold (the `while let Some(left) = iter.next()` block).
        let mut tasks: Vec<LevelTask> = Vec::with_capacity(level.len() / 2 + 1);
        // Carried node (odd at this level) -> promoted to the next level under
        // its EXISTING key, identical to the in-process `None` arm.
        let mut carry: Option<(Node<P>, String)> = None;
        let mut iter = level.into_iter();
        let mut pair_idx = 0u64;
        while let Some((left_node, left_key)) = iter.next() {
            match iter.next() {
                Some((right_node, right_key)) => {
                    tasks.push(LevelTask {
                        height,
                        level: this_level,
                        index: pair_idx,
                        left_key,
                        left_is_merge: left_node.1,
                        right_key,
                        right_is_merge: right_node.1,
                        output_key: merge_object_key(height, this_level, pair_idx),
                    });
                    pair_idx += 1;
                }
                None => {
                    // Odd node: carried up unchanged (NOT a task).
                    carry = Some(((left_node.0, left_node.1), left_key));
                }
            }
        }

        // ---- Release the level + barrier: workers prove the pairs on their
        // OWN cores; the leader awaits ALL results (this await IS the M2 level
        // barrier — level n+1 cannot start until every level-n result lands).
        let release = Instant::now();
        let mut results = transport.run_level(&tasks, merge_fn)?;
        let barrier_ms = release.elapsed().as_millis() as u64;

        // ---- Honest-failure: every emitted task MUST have produced a result.
        if results.len() != tasks.len() {
            anyhow::bail!(
                "distributed fold: level {this_level} emitted {} tasks but got {} results \
                 (honest-partial — refusing to fold an incomplete level)",
                tasks.len(),
                results.len()
            );
        }

        // ---- Determinism (#193): re-sort by stable in-level index so the next
        // level's node order — and hence the final proof — is bit-identical
        // regardless of which worker finished first.
        results.sort_by_key(|r| r.index);

        // Straggler instrumentation (measured, not gated).
        let mut proves: Vec<u64> = results.iter().map(|r| r.prove_ms).collect();
        proves.sort_unstable();
        let slowest_prove_ms = proves.last().copied().unwrap_or(0);
        let median_prove_ms = if proves.is_empty() {
            0
        } else {
            proves[proves.len() / 2]
        };
        let straggler_ms = slowest_prove_ms.saturating_sub(median_prove_ms);

        // ---- Build the next level by GETting each merge output by key (the
        // transit GET) in DETERMINISTIC index order, then appending the odd
        // carry. The output proof's bytes also give us the intermediate size.
        let mut next: Vec<(Node<P>, String)> = Vec::with_capacity(results.len() + 1);
        for (r, task) in results.iter().zip(tasks.iter()) {
            debug_assert_eq!(r.index, task.index, "results sorted to match task order");
            let (proof, get_dt) = transport.get(&r.output_key)?;
            transit_total += get_dt;
            // Measure the intermediate-proof wire size (issue #198 open
            // measurement #3: confirm constant ~412 KB to depth 6).
            if let Ok(bytes) = serde_json::to_vec(&proof) {
                max_intermediate_bytes = max_intermediate_bytes.max(bytes.len());
            }
            merges += 1;
            merge_prove_total += Duration::from_millis(r.prove_ms);
            next.push(((proof, true), r.output_key.clone()));
        }
        let odd_carry = carry.is_some();
        if let Some(carried) = carry {
            next.push(carried);
        }

        level_metrics.push(LevelBarrierMetric {
            level: this_level,
            tasks: tasks.len(),
            odd_carry,
            barrier_ms,
            slowest_prove_ms,
            median_prove_ms,
            straggler_ms,
        });

        level = next;
    }

    let ((final_proof, final_is_merge), _final_key) = level
        .pop()
        .expect("distributed fold produced no final proof");

    Ok(DistributedFoldOutcome {
        final_proof,
        final_is_merge,
        depth,
        merges,
        merge_prove_total,
        level_metrics,
        transit_total,
        max_intermediate_bytes,
    })
}

/// A HERMETIC in-process [`FoldTransport`] (issue #198 test seam). It keeps
/// proofs in an in-memory map keyed exactly like the real proof store
/// (`{height}/m/{level}/{index}`) and runs each level's merge tasks on its OWN
/// independent worker thread — exercising the SAME emitter / transit / barrier
/// / determinism-re-sort path the production Pub/Sub + GCS transport drives,
/// without a network or live GCS.
///
/// This is NOT a stub of the fold logic: the real [`fold_distributed`] leader
/// runs unchanged on top of it. Only the wire (Pub/Sub) and the bucket (GCS)
/// are replaced by an in-memory map + spawned threads, so the merges genuinely
/// fan out across independent workers and intermediate proofs genuinely transit
/// the (in-memory) store by key.
pub struct InMemoryFoldTransport<P: MergeProof> {
    store: Mutex<HashMap<String, P>>,
    /// Cap on concurrent worker threads per level (each prove on its own
    /// thread). Mirrors "scale by worker COUNT". `0` means one thread per task.
    max_workers: usize,
}

impl<P: MergeProof> InMemoryFoldTransport<P> {
    /// Build the transport pre-seeded with the leaves under their leaf keys
    /// (the cells' uploads, in the hermetic world). `max_workers` caps the
    /// independent worker threads per level (`0` = one per task).
    pub fn with_leaves(leaf_keys: &[String], leaves: &[P], max_workers: usize) -> Self {
        let mut store = HashMap::new();
        for (k, p) in leaf_keys.iter().zip(leaves.iter()) {
            store.insert(k.clone(), p.clone());
        }
        Self {
            store: Mutex::new(store),
            max_workers,
        }
    }

    fn read(&self, key: &str) -> Option<P> {
        self.store.lock().unwrap().get(key).cloned()
    }

    fn write(&self, key: &str, proof: P) {
        self.store.lock().unwrap().insert(key.to_string(), proof);
    }
}

impl<P: MergeProof> FoldTransport<P> for InMemoryFoldTransport<P> {
    fn put(&self, key: &str, proof: &P) -> anyhow::Result<Duration> {
        let t = Instant::now();
        self.write(key, proof.clone());
        Ok(t.elapsed())
    }

    fn get(&self, key: &str) -> anyhow::Result<(P, Duration)> {
        let t = Instant::now();
        let proof = self
            .read(key)
            .ok_or_else(|| anyhow::anyhow!("in-memory transit: missing key '{key}'"))?;
        Ok((proof, t.elapsed()))
    }

    fn run_level(
        &self,
        tasks: &[LevelTask],
        merge_fn: &MergeFn<'_, P>,
    ) -> anyhow::Result<Vec<TaskResult>> {
        if tasks.is_empty() {
            return Ok(Vec::new());
        }
        // One proof per worker, full cores (here: one thread per task, capped).
        // We chunk the tasks into waves of `max_workers` so several merges
        // genuinely run on INDEPENDENT threads in parallel — the cross-machine
        // fan-out, simulated. Each worker GETs its inputs, applies the SHARED
        // merge_fn, and PUTs its output by key.
        let width = if self.max_workers == 0 {
            tasks.len()
        } else {
            self.max_workers.max(1)
        };

        let mut results: Vec<TaskResult> = Vec::with_capacity(tasks.len());
        for wave in tasks.chunks(width) {
            // Spawn one worker thread per task in this wave (independent
            // workers — no shared rayon pool, no thread rationing).
            let wave_results: Vec<anyhow::Result<TaskResult>> = std::thread::scope(|scope| {
                let handles: Vec<_> = wave
                    .iter()
                    .map(|task| {
                        scope.spawn(move || -> anyhow::Result<TaskResult> {
                            // GET the two inputs by key (transit).
                            let (left, _) = self.get(&task.left_key)?;
                            let (right, _) = self.get(&task.right_key)?;
                            // PROVE the merge on this worker (full cores).
                            let t = Instant::now();
                            let out =
                                merge_fn(&left, task.left_is_merge, &right, task.right_is_merge)
                                    .map_err(|e| {
                                        anyhow::anyhow!(
                                    "distributed fold: merge task #{} (level {}) failed: {e}",
                                    task.index,
                                    task.level
                                )
                                    })?;
                            let prove_ms = t.elapsed().as_millis() as u64;
                            // PUT the output by key (transit).
                            self.put(&task.output_key, &out)?;
                            Ok(TaskResult {
                                index: task.index,
                                output_key: task.output_key.clone(),
                                prove_ms,
                            })
                        })
                    })
                    .collect();
                handles
                    .into_iter()
                    .map(|h| {
                        h.join()
                            .unwrap_or_else(|_| Err(anyhow::anyhow!("worker panicked")))
                    })
                    .collect()
            });
            for r in wave_results {
                results.push(r?);
            }
        }
        Ok(results)
    }
}

#[cfg(test)]
mod tests {
    use serde::{Deserialize, Serialize};

    use super::*;

    /// A tiny deterministic FAKE proof: a list of leaf ids in fold order, plus
    /// an `is_merge`-shaped tag. Merging concatenates the two operands' ids in
    /// LEFT-then-RIGHT order — so the final proof's ids encode the EXACT fold
    /// order. If the distributed re-sort/carry-up ever diverged from the
    /// in-process fold, the final ids would differ → the equivalence assert
    /// fails. This is the hermetic analogue of "bit-identical public inputs".
    #[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
    struct FakeProof {
        ids: Vec<u64>,
    }

    fn leaf(id: u64) -> FakeProof {
        FakeProof { ids: vec![id] }
    }

    /// The SINGLE merge implementation under test (the fake analogue of
    /// `prove_merge_pair`): concatenate left then right ids. Used by BOTH the
    /// in-process reference fold and the distributed fold so there is one merge
    /// impl in the test too.
    fn fake_merge(
        left: &FakeProof,
        _l_is_merge: bool,
        right: &FakeProof,
        _r_is_merge: bool,
    ) -> anyhow::Result<FakeProof> {
        let mut ids = left.ids.clone();
        ids.extend_from_slice(&right.ids);
        Ok(FakeProof { ids })
    }

    /// The IN-PROCESS reference fold: byte-for-byte the serial level loop with
    /// the same odd-carry. The distributed fold must match this exactly.
    fn fold_inprocess(leaves: Vec<FakeProof>) -> (FakeProof, bool, usize, usize) {
        let mut level: Vec<(FakeProof, bool)> = leaves.into_iter().map(|p| (p, false)).collect();
        let mut depth = 0usize;
        let mut merges = 0usize;
        while level.len() > 1 {
            depth += 1;
            let mut iter = level.into_iter();
            let mut next: Vec<(FakeProof, bool)> = Vec::new();
            while let Some(left) = iter.next() {
                match iter.next() {
                    Some(right) => {
                        let m = fake_merge(&left.0, left.1, &right.0, right.1).unwrap();
                        next.push((m, true));
                        merges += 1;
                    }
                    None => next.push(left),
                }
            }
            level = next;
        }
        let (p, is_merge) = level.pop().unwrap();
        (p, is_merge, depth, merges)
    }

    fn leaf_keys(height: u64, k: usize) -> Vec<String> {
        (0..k as u64)
            .map(|i| crate::conductor::storage::proof_object_key(height, i))
            .collect()
    }

    fn run_distributed(height: u64, k: usize, workers: usize) -> DistributedFoldOutcome<FakeProof> {
        let leaves: Vec<FakeProof> = (0..k as u64).map(leaf).collect();
        let keys = leaf_keys(height, k);
        let transport = InMemoryFoldTransport::with_leaves(&keys, &leaves, workers);
        let merge_fn: Box<MergeFn<FakeProof>> = Box::new(fake_merge);
        fold_distributed(height, leaves, keys, &transport, merge_fn.as_ref())
            .expect("distributed fold")
    }

    #[test]
    fn distributed_equals_inprocess_for_k4_k8_and_odd_sizes() {
        // The KEY correctness property (hermetic analogue of the #193
        // bit-identical contract): the distributed fold == the in-process fold
        // of the same leaves, for k >= 4 AND odd / non-power-of-two sizes that
        // exercise the carry-up.
        for k in [4usize, 5, 6, 7, 8, 9, 16] {
            let leaves: Vec<FakeProof> = (0..k as u64).map(leaf).collect();
            let (ref_proof, ref_is_merge, ref_depth, ref_merges) = fold_inprocess(leaves.clone());

            // Run distributed across several worker counts; ALL must match the
            // single reference (determinism regardless of scheduling).
            for workers in [1usize, 2, 3, 4, 8] {
                let out = run_distributed(7, k, workers);
                assert_eq!(
                    out.final_proof, ref_proof,
                    "k={k} workers={workers}: distributed final proof != in-process \
                     (DETERMINISM/EQUIVALENCE VIOLATION)"
                );
                assert_eq!(out.final_is_merge, ref_is_merge, "k={k}: final_is_merge");
                assert_eq!(out.depth, ref_depth, "k={k}: depth");
                assert_eq!(out.merges, ref_merges, "k={k}: merges");
            }
        }
    }

    #[test]
    fn final_ids_are_in_leaf_order() {
        // The fold must preserve left-to-right leaf order end-to-end (the fold
        // is a left-to-right balanced tree). k=8: ids 0..8 in order.
        let out = run_distributed(100, 8, 4);
        assert_eq!(out.final_proof.ids, vec![0, 1, 2, 3, 4, 5, 6, 7]);
        assert!(out.final_is_merge);
    }

    #[test]
    fn odd_carry_up_preserved() {
        // k=5: level 1 pairs (0,1)(2,3) carry 4; level 2 pairs ((01),(23))
        // carry (4); level 3 merges -> 01234. The carry must stay in order.
        let out = run_distributed(100, 5, 2);
        assert_eq!(out.final_proof.ids, vec![0, 1, 2, 3, 4]);
    }

    #[test]
    fn single_leaf_no_merge() {
        let out = run_distributed(100, 1, 4);
        assert_eq!(out.final_proof.ids, vec![0]);
        assert!(!out.final_is_merge, "single leaf is not a merge");
        assert_eq!(out.depth, 0);
        assert_eq!(out.merges, 0);
    }

    #[test]
    fn intermediate_proofs_transit_under_merge_key_namespace() {
        // After a fold, every merge output must live under the `{height}/m/...`
        // key namespace in the store — proving the transit really happened by
        // key (coordinator A's output is readable by coordinator B).
        let height = 42u64;
        let k = 8usize;
        let leaves: Vec<FakeProof> = (0..k as u64).map(leaf).collect();
        let keys = leaf_keys(height, k);
        let transport = InMemoryFoldTransport::with_leaves(&keys, &leaves, 4);
        let merge_fn: Box<MergeFn<FakeProof>> = Box::new(fake_merge);
        fold_distributed(height, leaves, keys, &transport, merge_fn.as_ref()).unwrap();

        let store = transport.store.lock().unwrap();
        // Level 1 has 4 merges, level 2 has 2, level 3 has 1 => 7 merge keys.
        let merge_keys: Vec<&String> = store.keys().filter(|k| k.contains("/m/")).collect();
        assert_eq!(merge_keys.len(), 7, "expected 7 intermediate merge outputs");
        for mk in merge_keys {
            assert!(
                mk.starts_with(&format!("{height}/m/")),
                "merge output key must be in the {height}/m/ namespace: {mk}"
            );
        }
    }

    #[test]
    fn instrumentation_is_populated() {
        let out = run_distributed(7, 8, 4);
        // One barrier metric per merge level (depth=3 for k=8).
        assert_eq!(out.level_metrics.len(), out.depth);
        assert_eq!(out.level_metrics[0].level, 1);
        assert_eq!(out.level_metrics[0].tasks, 4);
        // Transit and intermediate-size measurements are recorded.
        assert!(out.max_intermediate_bytes > 0, "intermediate size measured");
        // straggler = slowest - median, never negative.
        for m in &out.level_metrics {
            assert!(m.slowest_prove_ms >= m.median_prove_ms);
            assert_eq!(m.straggler_ms, m.slowest_prove_ms - m.median_prove_ms);
        }
    }

    #[test]
    fn honest_failure_on_merge_error_aborts_fold() {
        // A merge that returns Err must abort the whole fold (no fabricated
        // proof, no bad node carried up).
        let height = 7u64;
        let k = 4usize;
        let leaves: Vec<FakeProof> = (0..k as u64).map(leaf).collect();
        let keys = leaf_keys(height, k);
        let transport = InMemoryFoldTransport::with_leaves(&keys, &leaves, 2);
        let merge_fn: Box<MergeFn<FakeProof>> =
            Box::new(|_l, _lm, _r, _rm| anyhow::bail!("simulated merge failure"));
        let err = fold_distributed(height, leaves, keys, &transport, merge_fn.as_ref())
            .expect_err("fold must fail when a merge fails");
        assert!(
            err.to_string().contains("failed") || err.to_string().contains("merge"),
            "honest failure must surface the merge error: {err}"
        );
    }

    #[test]
    fn honest_failure_on_missing_leaf_key() {
        // If a leaf isn't in the store, the GET surfaces Err — never a
        // fabricated input.
        let height = 7u64;
        let leaves: Vec<FakeProof> = (0..4u64).map(leaf).collect();
        // Provide WRONG keys (not the ones seeded) so the first GET misses.
        let seeded = leaf_keys(height, 4);
        let transport = InMemoryFoldTransport::with_leaves(&seeded, &leaves, 2);
        let wrong_keys: Vec<String> = (0..4u64).map(|i| format!("{height}/wrong/{i}")).collect();
        let merge_fn: Box<MergeFn<FakeProof>> = Box::new(fake_merge);
        let err = fold_distributed(height, leaves, wrong_keys, &transport, merge_fn.as_ref())
            .expect_err("fold must fail when an input key is missing");
        assert!(err.to_string().contains("missing key"), "got: {err}");
    }

    #[test]
    fn empty_leaves_is_error() {
        let transport = InMemoryFoldTransport::<FakeProof>::with_leaves(&[], &[], 1);
        let merge_fn: Box<MergeFn<FakeProof>> = Box::new(fake_merge);
        let err = fold_distributed(7, vec![], vec![], &transport, merge_fn.as_ref())
            .expect_err("empty leaves must error");
        assert!(err.to_string().contains("no leaf proofs"));
    }
}
