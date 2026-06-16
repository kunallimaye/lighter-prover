// Copyright (c) Elliot Technologies, Inc.
// SPDX-License-Identifier: BUSL-1.1

//! COORDINATOR-ONLY fold micro-benchmark (issue #195) — re-measures the
//! parallel L2 merge fold added by PR #194 on a coordinator-only profile so
//! we report the production topology's actual fold wall, not the single-box
//! co-tenant artifact PR #194's close-out reported.
//!
//! ## Why this exists
//!
//! PR #194 reported single-box numbers from `distributed_fold_e2e` and
//! concluded "no realized speedup on a single 32-core box; parallel path
//! opt-in." That framing conflates a test-harness artifact with the
//! coordinator architecture itself:
//!
//!   - In production, CELLS prove L1+L2-leaf on their own boxes; the
//!     COORDINATOR is the box doing ONLY the k-1 merges (plus one L4).
//!   - Plonky2's prover dispatches into rayon's CURRENT thread pool. With
//!     no scoping, that is the process-wide GLOBAL pool of all cores. So N
//!     concurrent merges in the parallel fold each independently try to
//!     saturate ALL cores via that global pool → massive contention,
//!     which is expected to be slower than running them serially.
//!   - The lever the issue #195 body proposes is the CAPPED PER-MERGE pool:
//!     wrap each individual merge in a `rayon::ThreadPool` of
//!     `num_cpus / workers` threads via `pool.install(...)`. Because plonky2
//!     uses rayon's current pool, this caps each merge's per-call
//!     parallelism, so N concurrent merges land on disjoint cores.
//!
//! ## What this measures
//!
//! Builds k REAL L2 leaf proofs ONCE (mirroring the cell-side of
//! `distributed_fold_e2e`), then times JUST the fold three ways over the
//! SAME k leaves:
//!
//!   A. SERIAL          — `workers=1`, byte-for-byte the pre-#194 path.
//!   B. PARALLEL-UNCAPPED — `workers=N`, each merge uses plonky2's GLOBAL
//!                        rayon pool (the PR #194 behaviour as shipped).
//!   C. PARALLEL-CAPPED   — `workers=N`, each merge wrapped in a per-merge
//!                        `rayon::ThreadPool` of `num_cpus/N` threads (the
//!                        per-merge pool cap the issue proposes).
//!
//! Determinism is asserted (B and C must produce a bit-identical final proof
//! to A). The leaf-build phase is EXCLUDED from every fold-wall timing —
//! this is the "coordinator-only" framing: we measure the fold alone, not
//! the e2e.
//!
//! ## EXPENSIVE — opt-in only
//!
//! Real proving of k leaves + k-1 merges is slow, so this is double-gated
//! exactly like `distributed_fold_e2e`: `#[ignore]` AND early-return unless
//! `COORD_FOLD_BENCH=1`. Run it explicitly:
//!
//! ```sh
//! COORD_FOLD_BENCH=1 COORD_FOLD_BENCH_S=4 COORD_FOLD_BENCH_K=8 \
//!   cargo test -p bench --release --test coordinator_fold_bench \
//!   -- --ignored --nocapture --test-threads=1
//! ```
//!
//! `COORD_FOLD_BENCH_S` (default 4) and `COORD_FOLD_BENCH_K` (default 8)
//! pick the chunk width and chunk count. `COORD_FOLD_BENCH_WORKERS`
//! (default `min(k, num_cpus)` capped at 8 — matches the production
//! coordinator's typical pool size) picks the parallel-fold concurrency.
//!
//! Refs #195 #194 #193 #179 #113.

use std::time::Instant;

use bench::prestate::{ChunkPreState, sweep_per_tx_snapshots};
use bench::seed::seed_from_state;
use circuit::block::Block;
use circuit::block_pre_execution::{BlockPreExec, BlockPreExecWitness};
use circuit::block_pre_execution_constraints::{BlockPreExecutionCircuit, Circuit as _};
use circuit::block_tx_chain_constraints::{BlockTxChainCircuit, Circuit as _};
use circuit::block_tx_chain_merge_constraints::{BlockTxChainMergeCircuit, Circuit as _};
use circuit::block_tx_constraints::{BlockTxCircuit, Circuit as _};
use circuit::types::config::{C, CIRCUIT_CONFIG, D, F};
use plonky2::plonk::proof::ProofWithPublicInputs;
use plonky2::recursion::dummy_circuit::dummy_circuit;

const CHAIN_ID: u32 = 304;

/// A merge-tree node: the proof plus whether it is a merge (`true`) or a
/// leaf chain proof (`false`). Mirrors the binary's `TreeNode`.
type TreeNode = (ProofWithPublicInputs<F, C, D>, bool);

/// Prove ONE pairwise merge. Mirrors the binary's shared `prove_merge_pair`
/// helper so all three fold variants below invoke the EXACT same merge
/// circuit code — the only thing the variants differ on is SCHEDULING
/// (serial vs parallel + global pool vs per-merge capped pool).
fn prove_pair(
    merge_target: &circuit::block_tx_chain_merge_constraints::BlockTxChainMergeTarget,
    merge_data: &plonky2::plonk::circuit_data::CircuitData<F, C, D>,
    left: &TreeNode,
    right: &TreeNode,
) -> TreeNode {
    let proof = BlockTxChainMergeCircuit::prove(
        merge_target,
        merge_data,
        &left.0,
        left.1,
        &right.0,
        right.1,
    )
    .expect("merge prove");
    (proof, true)
}

/// (A) SERIAL fold — byte-for-byte the pre-#194 path.
fn fold_serial(
    merge_target: &circuit::block_tx_chain_merge_constraints::BlockTxChainMergeTarget,
    merge_data: &plonky2::plonk::circuit_data::CircuitData<F, C, D>,
    leaves: &[ProofWithPublicInputs<F, C, D>],
) -> (TreeNode, usize, usize) {
    let mut level: Vec<TreeNode> = leaves.iter().map(|p| (p.clone(), false)).collect();
    let mut depth = 0usize;
    let mut merges = 0usize;
    while level.len() > 1 {
        depth += 1;
        let mut iter = level.into_iter();
        let mut next: Vec<TreeNode> = Vec::new();
        while let Some(left) = iter.next() {
            match iter.next() {
                Some(right) => {
                    next.push(prove_pair(merge_target, merge_data, &left, &right));
                    merges += 1;
                }
                None => next.push(left),
            }
        }
        level = next;
    }
    let node = level.pop().expect("serial fold produced a final proof");
    (node, depth, merges)
}

/// (B) PARALLEL-UNCAPPED fold — `workers` rayon workers; each pair calls
/// `prove_pair` directly, so each individual merge dispatches into plonky2's
/// GLOBAL rayon pool of all cores. N concurrent merges therefore contend on
/// the same physical cores. This is the PR #194 behaviour as shipped.
fn fold_parallel_uncapped(
    merge_target: &circuit::block_tx_chain_merge_constraints::BlockTxChainMergeTarget,
    merge_data: &plonky2::plonk::circuit_data::CircuitData<F, C, D>,
    leaves: &[ProofWithPublicInputs<F, C, D>],
    workers: usize,
) -> (TreeNode, usize, usize) {
    use rayon::prelude::*;
    let pool = rayon::ThreadPoolBuilder::new()
        .num_threads(workers)
        .build()
        .expect("build uncapped fold pool");
    let mut level: Vec<TreeNode> = leaves.iter().map(|p| (p.clone(), false)).collect();
    let mut depth = 0usize;
    let mut merges = 0usize;
    while level.len() > 1 {
        depth += 1;
        let mut pairs: Vec<(TreeNode, Option<TreeNode>)> =
            Vec::with_capacity(level.len() / 2 + 1);
        let mut iter = level.into_iter();
        while let Some(left) = iter.next() {
            match iter.next() {
                Some(right) => pairs.push((left, Some(right))),
                None => pairs.push((left, None)),
            }
        }
        let mut indexed: Vec<(usize, TreeNode, bool)> = pool.install(|| {
            pairs
                .into_par_iter()
                .enumerate()
                .map(|(i, (left, right_opt))| match right_opt {
                    Some(right) => (i, prove_pair(merge_target, merge_data, &left, &right), true),
                    None => (i, left, false),
                })
                .collect()
        });
        indexed.sort_by_key(|(i, _, _)| *i);
        let mut next: Vec<TreeNode> = Vec::with_capacity(indexed.len());
        for (_, node, was_merge) in indexed {
            if was_merge {
                merges += 1;
            }
            next.push(node);
        }
        level = next;
    }
    let node = level
        .pop()
        .expect("parallel-uncapped fold produced a final proof");
    (node, depth, merges)
}

/// (C) PARALLEL-CAPPED fold — `workers` rayon workers; each individual merge
/// is WRAPPED in a per-merge `rayon::ThreadPool` of `per_merge_threads`
/// threads via `pool.install(...)`. Because plonky2 uses rayon's CURRENT
/// pool for its `par_iter` calls (via `plonky2_maybe_rayon`), this caps the
/// per-merge parallelism so N concurrent merges land on disjoint cores
/// rather than fighting for the same global pool.
///
/// `per_merge_threads` defaults to `num_cpus::get() / workers` (rounded down,
/// minimum 1) so the cap matches the cores actually available to each
/// concurrent merge on the box.
fn fold_parallel_capped(
    merge_target: &circuit::block_tx_chain_merge_constraints::BlockTxChainMergeTarget,
    merge_data: &plonky2::plonk::circuit_data::CircuitData<F, C, D>,
    leaves: &[ProofWithPublicInputs<F, C, D>],
    workers: usize,
    per_merge_threads: usize,
) -> (TreeNode, usize, usize) {
    use rayon::prelude::*;
    let outer = rayon::ThreadPoolBuilder::new()
        .num_threads(workers)
        .build()
        .expect("build capped outer fold pool");
    let mut level: Vec<TreeNode> = leaves.iter().map(|p| (p.clone(), false)).collect();
    let mut depth = 0usize;
    let mut merges = 0usize;
    while level.len() > 1 {
        depth += 1;
        let mut pairs: Vec<(TreeNode, Option<TreeNode>)> =
            Vec::with_capacity(level.len() / 2 + 1);
        let mut iter = level.into_iter();
        while let Some(left) = iter.next() {
            match iter.next() {
                Some(right) => pairs.push((left, Some(right))),
                None => pairs.push((left, None)),
            }
        }
        let mut indexed: Vec<(usize, TreeNode, bool)> = outer.install(|| {
            pairs
                .into_par_iter()
                .enumerate()
                .map(|(i, (left, right_opt))| match right_opt {
                    Some(right) => {
                        // Per-merge inner pool: caps plonky2's intra-merge
                        // parallelism so concurrent merges share cores
                        // fairly instead of all 32 threads each.
                        let inner = rayon::ThreadPoolBuilder::new()
                            .num_threads(per_merge_threads)
                            .build()
                            .expect("build per-merge inner pool");
                        let node = inner
                            .install(|| prove_pair(merge_target, merge_data, &left, &right));
                        (i, node, true)
                    }
                    None => (i, left, false),
                })
                .collect()
        });
        indexed.sort_by_key(|(i, _, _)| *i);
        let mut next: Vec<TreeNode> = Vec::with_capacity(indexed.len());
        for (_, node, was_merge) in indexed {
            if was_merge {
                merges += 1;
            }
            next.push(node);
        }
        level = next;
    }
    let node = level
        .pop()
        .expect("parallel-capped fold produced a final proof");
    (node, depth, merges)
}

fn enabled() -> bool {
    std::env::var("COORD_FOLD_BENCH")
        .map(|v| v == "1")
        .unwrap_or(false)
}

fn env_usize(name: &str, default: usize) -> usize {
    std::env::var(name)
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(default)
}

fn load_block() -> Block<F> {
    let path = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("bench_test.json");
    let data = std::fs::read_to_string(&path)
        .unwrap_or_else(|e| panic!("cannot read {}: {e}", path.display()));
    serde_json::from_str(&data).expect("bench_test.json parses as Block")
}

#[test]
#[ignore = "EXPENSIVE coordinator-only fold bench; run with COORD_FOLD_BENCH=1 ... -- --ignored"]
fn coordinator_fold_bench_serial_vs_parallel_uncapped_vs_parallel_capped() {
    if !enabled() {
        eprintln!(
            "SKIP coordinator_fold_bench (set COORD_FOLD_BENCH=1 to run; it really proves a \
             small multi-chunk block's L2 leaves and then times the fold three ways)"
        );
        return;
    }
    // plonky2's prover is stack-hungry; run on a large-stack thread exactly
    // as the FINDING D gate and `distributed_fold_e2e` do.
    std::thread::Builder::new()
        .stack_size(4 * 1024 * 1024 * 1024)
        .spawn(run_bench)
        .expect("spawn large-stack bench thread")
        .join()
        .expect("bench thread panicked");
}

fn run_bench() {
    let t_total = Instant::now();

    let s = env_usize("COORD_FOLD_BENCH_S", 4);
    let k = env_usize("COORD_FOLD_BENCH_K", 8);
    assert!(
        k >= 2,
        "COORD_FOLD_BENCH_K must be >= 2 (multi-chunk is required to exercise the fold)"
    );
    let n_tx = s * k;

    let num_cpus = num_cpus::get();
    // Default workers mirrors the production coordinator's typical pool
    // sizing: min(k, cores) capped at 8 so this run remains comparable to
    // the PR #194 numbers (which used the same cap in the e2e harness).
    let workers = env_usize(
        "COORD_FOLD_BENCH_WORKERS",
        std::cmp::min(std::cmp::max(2, k), std::cmp::min(num_cpus, 8)),
    );
    let per_merge_threads = env_usize(
        "COORD_FOLD_BENCH_PER_MERGE_THREADS",
        std::cmp::max(1, num_cpus / workers),
    );

    let mut block = load_block();
    assert!(
        block.txs.len() >= n_tx,
        "bench_test.json has only {} txs; need {} for S={s} k={k}",
        block.txs.len(),
        n_tx
    );
    block.txs.truncate(n_tx);
    let height = block.block_number;
    let created_at = block.created_at;
    println!(
        "[coord-fold-bench] block height={height} truncated to {n_tx} txs => S={s} k={k} chunks; \
         num_cpus={num_cpus} workers={workers} per_merge_threads={per_merge_threads}"
    );

    // ---- Shared resident circuits (same shapes the cell + coordinator use).
    let pre_exec_circuit = BlockPreExecutionCircuit::define(CIRCUIT_CONFIG);
    let pbt = pre_exec_circuit.target;
    let pre_exec_data = pre_exec_circuit.builder.build::<C>();
    let bpe = BlockPreExec::from_block(&block);
    let pre_proof =
        BlockPreExecutionCircuit::prove(&pre_exec_data, &bpe, &pbt).expect("pre-exec prove");
    let pre_exec_witness = BlockPreExecWitness::from_public_inputs(&pre_proof.public_inputs);
    let state_metadata = pre_exec_witness.new_state_metadata.clone();

    // L1 at this S.
    let l1 = BlockTxCircuit::define(CIRCUIT_CONFIG, s, CHAIN_ID);
    let bt = l1.target;
    let l1_data = l1.builder.build::<C>();

    // L2 leaf chain + cyclic base scaffolding.
    let chain_circuit = BlockTxChainCircuit::define(CIRCUIT_CONFIG, &l1_data, s, 1);
    let chain_t = chain_circuit.target;
    let chain_data = chain_circuit.builder.build::<C>();
    let block_tx_witness_size = chain_circuit.block_tx_witness_size;
    let dummy_chain = dummy_circuit(&chain_data.common);
    let dummy_proof = circuit::builder::custom::cyclic_base_proof(
        &chain_data.common,
        &chain_data.verifier_only,
        &dummy_chain,
        Vec::<F>::new().iter().copied().enumerate().collect(),
    )
    .expect("cyclic base proof");

    // Merge circuit (coordinator's fold), built into the leaf chain's exact
    // self-shape (the closed cyclic fixed point).
    let merge_circuit = BlockTxChainMergeCircuit::define(CIRCUIT_CONFIG, &chain_data, 1);
    let merge_target = merge_circuit.target;
    let merge_data = merge_circuit.builder.build::<C>();
    assert!(
        merge_data.common == chain_data.common,
        "merge circuit must build into the leaf chain's exact self-shape (issue #67)"
    );

    // ---- S=1 positional pre-state sweep (same FINDING D seam the cell uses).
    let initial = ChunkPreState {
        register_stack: block.register_stack_before,
        all_assets: block.all_assets.clone(),
        all_market_details: pre_exec_witness.new_market_details.clone(),
        system_config: block.old_system_config,
        account_tree_root: block.old_account_tree_root,
        account_pub_data_tree_root: block.old_account_pub_data_tree_root,
        account_delta_tree_root: block.old_account_delta_tree_root,
        market_tree_root: block.old_market_tree_root,
        empty_index_sibling_paths: None,
    };
    let l1_s1 = BlockTxCircuit::define(CIRCUIT_CONFIG, 1, CHAIN_ID);
    let bt_s1 = l1_s1.target;
    let l1_s1_data = l1_s1.builder.build::<C>();
    println!("[coord-fold-bench] running S=1 positional pre-state sweep over {n_tx} txs...");
    let sweep_start = Instant::now();
    let snapshots = sweep_per_tx_snapshots(
        height,
        created_at,
        initial,
        &block.txs,
        &l1_s1_data,
        &bt_s1,
        |_pos, _wall_ms| {},
    );
    println!(
        "[coord-fold-bench]   sweep done in {:?}",
        sweep_start.elapsed()
    );

    // ---- LEAF BUILD (excluded from fold timings): k real L1 + L2-leaf proofs.
    println!("[coord-fold-bench] LEAF BUILD: proving {k} real L2 leaf proofs...");
    let leaves_start = Instant::now();
    let mut leaves: Vec<ProofWithPublicInputs<F, C, D>> = Vec::with_capacity(k);
    for chunk_idx in 0..k {
        let lo = chunk_idx * s;
        let hi = lo + s;
        let txs: Vec<_> = block.txs[lo..hi].to_vec();

        let pos_pre = snapshots
            .at_chunk(s, chunk_idx)
            .unwrap_or_else(|| panic!("snapshot for chunk {chunk_idx} (pos {}) missing", lo));

        let block_tx = pos_pre.block_tx(created_at, txs.clone());
        let l1_proof: ProofWithPublicInputs<F, C, D> =
            BlockTxCircuit::prove(&l1_data, &block_tx, &bt)
                .unwrap_or_else(|e| panic!("chunk {chunk_idx}: L1 prove failed: {e:?}"));

        let seed = seed_from_state(
            &pos_pre.register_stack,
            pos_pre.account_tree_root,
            pos_pre.account_pub_data_tree_root,
            pos_pre.market_tree_root,
            pos_pre.account_delta_tree_root,
            &pos_pre.all_assets,
            &pos_pre.all_market_details,
            &state_metadata,
            &pos_pre.system_config,
        );
        let base = BlockTxChainCircuit::cyclic_base_proof(
            &chain_data,
            &dummy_chain,
            height,
            created_at,
            seed.pre_state_root,
            seed.pre_state_root,
            seed.pre_validium_root,
            seed.pre_delta_root,
            block_tx_witness_size,
            &state_metadata,
        );
        let leaf_proof: ProofWithPublicInputs<F, C, D> =
            BlockTxChainCircuit::prove(&chain_t, &chain_data, 0, &base, &dummy_proof, &l1_proof)
                .unwrap_or_else(|e| panic!("chunk {chunk_idx}: L2 leaf prove failed: {e:?}"));
        leaves.push(leaf_proof);
        println!("[coord-fold-bench]   leaf {}/{} built", chunk_idx + 1, k);
    }
    let leaves_ms = leaves_start.elapsed().as_millis() as u64;
    println!(
        "[coord-fold-bench] LEAF BUILD done: {k} leaves in {leaves_ms} ms (EXCLUDED from fold \
         timings below)"
    );

    // =====================================================================
    // FOLD TIMINGS — coordinator-only, over the SAME leaves, three ways.
    // No other work (leaf proving, L4) runs concurrently with these timings.
    // =====================================================================

    // (A) SERIAL — byte-for-byte the pre-#194 path. Baseline.
    println!("[coord-fold-bench] (A) timing SERIAL fold (workers=1)...");
    let t_a = Instant::now();
    let ((serial_proof, serial_is_merge), serial_depth, serial_merges) =
        fold_serial(&merge_target, &merge_data, &leaves);
    let serial_ms = t_a.elapsed().as_millis() as u64;
    println!("[coord-fold-bench]   SERIAL fold wall: {serial_ms} ms");

    // (B) PARALLEL-UNCAPPED — PR #194's shipped behaviour (each merge uses
    //     plonky2's GLOBAL rayon pool of all cores → contention).
    println!(
        "[coord-fold-bench] (B) timing PARALLEL-UNCAPPED fold (workers={workers}, each merge \
         uses plonky2's GLOBAL rayon pool)..."
    );
    let t_b = Instant::now();
    let ((par_unc_proof, par_unc_is_merge), par_unc_depth, par_unc_merges) =
        fold_parallel_uncapped(&merge_target, &merge_data, &leaves, workers);
    let par_unc_ms = t_b.elapsed().as_millis() as u64;
    println!("[coord-fold-bench]   PARALLEL-UNCAPPED fold wall: {par_unc_ms} ms");

    // (C) PARALLEL-CAPPED — per-merge pool of `per_merge_threads` threads.
    println!(
        "[coord-fold-bench] (C) timing PARALLEL-CAPPED fold (workers={workers}, \
         per_merge_threads={per_merge_threads})..."
    );
    let t_c = Instant::now();
    let ((par_cap_proof, par_cap_is_merge), par_cap_depth, par_cap_merges) =
        fold_parallel_capped(&merge_target, &merge_data, &leaves, workers, per_merge_threads);
    let par_cap_ms = t_c.elapsed().as_millis() as u64;
    println!("[coord-fold-bench]   PARALLEL-CAPPED fold wall: {par_cap_ms} ms");

    // ---- DETERMINISM: all three folds must produce a bit-identical final proof.
    assert_eq!(
        serial_depth, par_unc_depth,
        "SERIAL vs PARALLEL-UNCAPPED disagree on depth ({serial_depth} != {par_unc_depth})"
    );
    assert_eq!(
        serial_depth, par_cap_depth,
        "SERIAL vs PARALLEL-CAPPED disagree on depth ({serial_depth} != {par_cap_depth})"
    );
    assert_eq!(serial_merges, par_unc_merges);
    assert_eq!(serial_merges, par_cap_merges);
    assert_eq!(serial_is_merge, par_unc_is_merge);
    assert_eq!(serial_is_merge, par_cap_is_merge);
    assert_eq!(
        serial_proof.public_inputs, par_unc_proof.public_inputs,
        "DETERMINISM: SERIAL and PARALLEL-UNCAPPED disagree on final proof public inputs"
    );
    assert_eq!(
        serial_proof.public_inputs, par_cap_proof.public_inputs,
        "DETERMINISM: SERIAL and PARALLEL-CAPPED disagree on final proof public inputs"
    );

    // ---- Structured summary line — easy to grep, easy to paste into PRs.
    println!(
        "[coord-fold-bench] RESULT s={s} k={k} num_cpus={num_cpus} workers={workers} \
         per_merge_threads={per_merge_threads} depth={serial_depth} merges={serial_merges} \
         leaves_ms={leaves_ms} serial_ms={serial_ms} parallel_uncapped_ms={par_unc_ms} \
         parallel_capped_ms={par_cap_ms}"
    );
    let speedup_unc = serial_ms as f64 / par_unc_ms.max(1) as f64;
    let speedup_cap = serial_ms as f64 / par_cap_ms.max(1) as f64;
    println!(
        "[coord-fold-bench] SPEEDUP vs serial: parallel_uncapped={:.2}x parallel_capped={:.2}x",
        speedup_unc, speedup_cap
    );
    println!(
        "[coord-fold-bench] PASS coordinator-only fold bench (k={k}, total wall {:?})",
        t_total.elapsed()
    );
}
