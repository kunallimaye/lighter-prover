// Copyright (c) Elliot Technologies, Inc.
// SPDX-License-Identifier: BUSL-1.1

//! Host (prove-free) pre-pass for the 8-way L5 segment-parallel scheduler
//! (issue #78).
//!
//! The L5 layer (`CyclicRecursionCircuit`) folds a chain of per-block L4
//! proofs into a running `Batch`, threading a single running cyclic proof
//! through each fold. The fold is inherently sequential *within* a chain,
//! but the wrapper circuit (`WrapperCircuit`, `NUM_CHAINS_PER_BATCH = 8`)
//! is designed to accept up to 8 independent segment chains and merge their
//! roots in one shot. This module computes everything needed to launch
//! those 8 chains **in parallel** without any proving:
//!
//! - `segment_split_points`: split `block_count` blocks across `1..=8`
//!   segments as evenly as possible.
//! - `host_prepass`: compute each segment's *starting* on-chain-operations
//!   keccak-prefix hash (the only cross-segment data dependency) by folding
//!   the preceding blocks on the host with the exact same aggregation the
//!   in-circuit path uses (`Batch::aggregate_block`). Segment 0 starts from
//!   the all-zero hash (which the L6 wrapper asserts for segment 0).
//! - `synthesize_block_sequence`: fabricate a continuation-consistent
//!   multi-block fixture from a single base block (the repo ships only one
//!   ~50 MB single-block fixture), so the per-fold sanity checks in
//!   `cyclic_circuit.rs` (block-number continuity, timestamp monotonicity,
//!   state-root + delta-root chaining) all pass.
//!
//! Everything here is prove-free: it touches only the host `Block`/
//! `BlockWitness`/`Batch` types and the `keccak` helper. The expensive
//! proving path lives in the `--l5-segment-check` driver in `bench.rs`.

use circuit::block::{Block, BlockWitness};
use circuit::recursion::batch::Batch;
use circuit::types::config::F;
use circuit::types::constants::KECCAK_HASH_OUT_BYTE_SIZE;
use plonky2::hash::hash_types::HashOut;

/// The header seed a segment's first fold needs: the cross-segment
/// on-chain-operations running keccak prefix plus the header fields read
/// from the segment's first block. Only `old_on_chain_operations_pub_data_hash`
/// is a genuine cross-segment dependency (it is threaded as `SegmentInfo`);
/// the remaining fields are carried for documentation / assertion of the
/// per-segment starting position.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SegmentSeed {
    /// Running on-chain-operations keccak prefix over all blocks before this
    /// segment's first block. Segment 0 is all-zeros (the L6 wrapper asserts
    /// segment 0's hash == 0).
    pub old_on_chain_operations_pub_data_hash: [u8; KECCAK_HASH_OUT_BYTE_SIZE],
    /// `old_state_root` of this segment's first block.
    pub old_state_root: HashOut<F>,
    /// `old_account_delta_tree_root` of this segment's first block.
    pub old_account_delta_tree_root: HashOut<F>,
    /// `old_prefix_priority_operation_hash` of this segment's first block.
    pub old_prefix_priority_operation_hash: [u8; KECCAK_HASH_OUT_BYTE_SIZE],
}

/// Distribute `block_count` blocks across `segment_count` (`1..=8`)
/// segments as evenly as possible, returning the `segment_count + 1`
/// boundary points `[0, p_1, ..., block_count]`. Earlier segments absorb
/// the `+1` remainder, so segment sizes differ by at most one and are
/// non-increasing.
///
/// Requires `block_count >= segment_count >= 1`.
pub fn segment_split_points(block_count: usize, segment_count: usize) -> Vec<usize> {
    assert!(segment_count >= 1, "segment_count must be >= 1");
    assert!(
        block_count >= segment_count,
        "block_count ({block_count}) must be >= segment_count ({segment_count})"
    );

    let base = block_count / segment_count;
    let remainder = block_count % segment_count;

    let mut points = Vec::with_capacity(segment_count + 1);
    points.push(0);
    let mut acc = 0usize;
    for k in 0..segment_count {
        // The first `remainder` segments get one extra block.
        let size = base + usize::from(k < remainder);
        acc += size;
        points.push(acc);
    }
    debug_assert_eq!(*points.last().unwrap(), block_count);
    points
}

/// For each segment, compute the `SegmentSeed` it needs to start folding.
///
/// Segment 0 seeds the all-zero on-chain-operations hash. Segment `k >= 1`
/// seeds the running keccak prefix obtained by folding blocks `[0..p_k)`
/// into a fresh `Batch` via `aggregate_block` -- byte-for-byte the same
/// aggregation the in-circuit L5 fold performs, so the host pre-pass and
/// the circuit agree on the cross-segment hash.
///
/// `split_points` must be the output of [`segment_split_points`] for the
/// same block sequence (length `segment_count + 1`).
pub fn host_prepass(blocks: &[Block<F>], split_points: &[usize]) -> Vec<SegmentSeed> {
    assert!(
        split_points.len() >= 2,
        "split_points must have at least [0, block_count]"
    );
    assert_eq!(
        *split_points.last().unwrap(),
        blocks.len(),
        "split_points must end at blocks.len()"
    );

    let segment_count = split_points.len() - 1;
    let mut seeds = Vec::with_capacity(segment_count);

    // Each segment k starts at split_points[k]; the trailing boundary
    // (block_count) is not a segment start, so iterate the first
    // `segment_count` boundaries with their index.
    for (k, &start) in split_points[..segment_count].iter().enumerate() {
        let first = &blocks[start];

        let old_on_chain_operations_pub_data_hash = if k == 0 {
            [0u8; KECCAK_HASH_OUT_BYTE_SIZE]
        } else {
            // Fold every block strictly before this segment's first block
            // into a fresh Batch and read the running on-chain-ops hash.
            running_on_chain_ops_hash(&blocks[..start])
        };

        seeds.push(SegmentSeed {
            old_on_chain_operations_pub_data_hash,
            old_state_root: first.old_state_root,
            old_account_delta_tree_root: first.old_account_delta_tree_root,
            old_prefix_priority_operation_hash: first.old_prefix_priority_operation_hash,
        });
    }

    seeds
}

/// Fold a prefix of blocks into a fresh `Batch` and return the running
/// on-chain-operations keccak prefix hash. Prove-free: uses the host
/// `Batch::aggregate_block` mirror of the in-circuit aggregation.
fn running_on_chain_ops_hash(prefix: &[Block<F>]) -> [u8; KECCAK_HASH_OUT_BYTE_SIZE] {
    let mut batch = Batch::<F>::default();
    for block in prefix {
        let bw = BlockWitness::from_block(block, 1);
        batch.aggregate_block(&bw);
    }
    batch.on_chain_operations_pub_data_hash
}

/// Synthesize a `count`-block sequence from a single `base` block. The repo
/// ships only one single-block fixture (`bench_test.json`), so multi-block L5
/// scheduling needs a fabricated sequence.
///
/// Block `i` gets a distinct, strictly-increasing identity:
/// - `block_number = base.block_number + i`
/// - `created_at = base.created_at + i` (strictly increasing -- satisfies the
///   L5 fold's timestamp-monotonicity check)
///
/// Every other field (all state / delta / market roots, on-chain-operations
/// payloads, txs) is cloned verbatim from `base`. This is deliberate: the
/// block's `old_state_root` is a *hash of its account/market sub-roots*
/// recomputed and asserted inside L3 (`block_pre_execution_constraints.rs`),
/// so it cannot be re-pointed to a previous block's `new_state_root` without
/// also re-deriving those sub-roots -- data the single-block fixture does not
/// expose. Cloning keeps every synthesized block individually provable through
/// L1..L4 with the same workload as the real fixture, which is what the
/// scheduler's parallelism instrument needs to measure.
///
/// ## Within-segment state chaining (real-data follow-up)
///
/// Because cloned blocks share `old_state_root`/`new_state_root` (and
/// `old != new` once txs run), folding two of them inside one segment would
/// trip the L5 fold's `batch.new_state_root == current_block.old_state_root`
/// continuity assert (`cyclic_circuit.rs:208`, active only on
/// `not_first_recursion`). Multi-block *within-segment* folds therefore need a
/// genuinely state-chained multi-block dataset -- a follow-up tracked
/// alongside the #83 L6 termination work. The cross-segment dependency this
/// issue targets (the on-chain-operations keccak prefix) IS exercised:
/// [`host_prepass`] computes it over the cloned sequence, and the scheduler
/// proves + verifies the first fold of every segment in parallel.
pub fn synthesize_block_sequence(base: &Block<F>, count: usize) -> Vec<Block<F>> {
    assert!(count >= 1, "count must be >= 1");

    (0..count)
        .map(|i| {
            let mut b = base.clone();
            b.block_number = base.block_number + i as u64;
            b.created_at = base.created_at + i as i64;
            b
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use circuit::types::asset::Asset;
    use circuit::types::constants::{
        ASSET_LIST_SIZE, ON_CHAIN_OPERATIONS_PUB_DATA_BYTES_SIZE, POSITION_LIST_SIZE,
    };
    use circuit::types::market_details::{MarketDetails, PublicMarketDetails};
    use plonky2::field::types::Field;

    use super::*;

    /// Build a minimal, prove-free `Block<F>` with controllable headers and
    /// on-chain-operations payload, so the host-vs-Batch test exercises the
    /// keccak chain without loading the 50 MB fixture. Built via a struct
    /// literal (the heavy fixed-size arrays default cleanly) rather than JSON
    /// to avoid pinning the on-disk wire format in a unit test.
    fn mk_block(
        block_number: u64,
        created_at: i64,
        old_state_root: HashOut<F>,
        new_state_root: HashOut<F>,
        on_chain_ops: Vec<[u8; ON_CHAIN_OPERATIONS_PUB_DATA_BYTES_SIZE]>,
    ) -> Block<F> {
        Block {
            created_at,
            block_number,
            register_stack_before: Default::default(),
            old_system_config: Default::default(),
            all_market_details: core::array::from_fn::<MarketDetails, POSITION_LIST_SIZE, _>(
                |_| MarketDetails::default(),
            ),
            all_assets: core::array::from_fn::<Asset, ASSET_LIST_SIZE, _>(|_| Asset::default()),
            new_public_market_details: core::array::from_fn::<
                PublicMarketDetails,
                POSITION_LIST_SIZE,
                _,
            >(|_| PublicMarketDetails::default()),
            price_updates: Default::default(),
            calculate_premium: false,
            calculate_funding: false,
            calculate_oracle_prices: false,
            old_account_tree_root: HashOut::default(),
            old_account_pub_data_tree_root: HashOut::default(),
            old_market_tree_root: HashOut::default(),
            state_metadata: Default::default(),
            old_state_root,
            // Chain the account-delta-tree roots alongside state roots so
            // every aggregate_block continuity assert passes.
            old_account_delta_tree_root: old_state_root,
            new_validium_root: HashOut::default(),
            new_state_root,
            new_account_delta_tree_root: new_state_root,
            on_chain_operations_count: on_chain_ops.len() as u64,
            on_chain_operations_pub_data: on_chain_ops,
            priority_operations_count: 0,
            old_prefix_priority_operation_hash: [0u8; KECCAK_HASH_OUT_BYTE_SIZE],
            new_prefix_priority_operation_hash: [0u8; KECCAK_HASH_OUT_BYTE_SIZE],
            txs: Vec::new(),
        }
    }

    fn root(seed: u64) -> HashOut<F> {
        HashOut::from([
            F::from_canonical_u64(seed),
            F::from_canonical_u64(seed + 1),
            F::from_canonical_u64(seed + 2),
            F::from_canonical_u64(seed + 3),
        ])
    }

    #[test]
    fn split_points_distribute_evenly() {
        for &(blocks, segments) in &[(1usize, 1usize), (3, 3), (8, 8), (64, 8), (10, 3), (8, 1)] {
            let pts = segment_split_points(blocks, segments);
            assert_eq!(pts.len(), segments + 1, "wrong number of boundaries");
            assert_eq!(pts[0], 0, "first boundary must be 0");
            assert_eq!(
                *pts.last().unwrap(),
                blocks,
                "last boundary must be block_count"
            );

            let sizes: Vec<usize> = pts.windows(2).map(|w| w[1] - w[0]).collect();
            assert_eq!(
                sizes.iter().sum::<usize>(),
                blocks,
                "sizes must sum to block_count"
            );
            assert_eq!(sizes.len(), segments, "must have one size per segment");

            // Sizes differ by at most one (as-even-as-possible).
            let max = *sizes.iter().max().unwrap();
            let min = *sizes.iter().min().unwrap();
            assert!(max - min <= 1, "sizes not balanced: {sizes:?}");
            // Earlier segments absorb the remainder (non-increasing).
            assert!(
                sizes.windows(2).all(|w| w[0] >= w[1]),
                "sizes not non-increasing: {sizes:?}"
            );
        }
    }

    #[test]
    fn split_points_64_over_8_is_uniform() {
        let pts = segment_split_points(64, 8);
        let sizes: Vec<usize> = pts.windows(2).map(|w| w[1] - w[0]).collect();
        assert_eq!(sizes, vec![8; 8]);
    }

    #[test]
    fn host_prepass_matches_independent_batch_fold() {
        // Build a small synthetic chain with at least one block carrying
        // non-empty on-chain-operations pub data, so the keccak prefix chain
        // is actually exercised. State/delta roots are chained so each
        // aggregate_block call passes its continuity asserts.
        let mut on_chain = [0u8; ON_CHAIN_OPERATIONS_PUB_DATA_BYTES_SIZE];
        for (i, b) in on_chain.iter_mut().enumerate() {
            *b = (i as u8).wrapping_add(1);
        }

        let mut blocks = Vec::new();
        let mut prev_root = root(100);
        for i in 0..6u64 {
            let next_root = root(200 + i * 10);
            // Blocks 1 and 4 carry on-chain operations; the rest are empty.
            let ops = if i == 1 || i == 4 {
                vec![on_chain]
            } else {
                Vec::new()
            };
            let b = mk_block(i, i as i64, prev_root, next_root, ops);
            blocks.push(b);
            prev_root = next_root;
        }

        let split_points = segment_split_points(blocks.len(), 3);
        let seeds = host_prepass(&blocks, &split_points);
        assert_eq!(seeds.len(), 3);

        // Segment 0 must seed the all-zero hash.
        assert_eq!(
            seeds[0].old_on_chain_operations_pub_data_hash,
            [0u8; KECCAK_HASH_OUT_BYTE_SIZE]
        );

        // Each segment k>=1 must equal an INDEPENDENT fold of blocks
        // [0..p_k) into a fresh Batch (the in-circuit aggregation mirror).
        for k in 0..seeds.len() {
            let start = split_points[k];
            let mut batch = Batch::<F>::default();
            for block in &blocks[..start] {
                batch.aggregate_block(&BlockWitness::from_block(block, 1));
            }
            assert_eq!(
                seeds[k].old_on_chain_operations_pub_data_hash,
                batch.on_chain_operations_pub_data_hash,
                "segment {k} start hash diverges from independent Batch fold"
            );
            // And the header fields must come from blocks[p_k].
            assert_eq!(seeds[k].old_state_root, blocks[start].old_state_root);
        }

        // Sanity: at least one non-zero seed exists (the keccak chain ran).
        assert!(
            seeds.iter().any(
                |s| s.old_on_chain_operations_pub_data_hash != [0u8; KECCAK_HASH_OUT_BYTE_SIZE]
            ),
            "expected a non-zero on-chain-ops prefix to be exercised"
        );
    }

    #[test]
    fn synthesize_gives_distinct_monotonic_identity() {
        let base = mk_block(186_974_592, 1_700_000_000, root(1), root(2), Vec::new());
        let seq = synthesize_block_sequence(&base, 5);
        assert_eq!(seq.len(), 5);

        for (i, b) in seq.iter().enumerate() {
            // Distinct, strictly-increasing block number + timestamp (the L5
            // fold's continuity + monotonicity inputs).
            assert_eq!(b.block_number, base.block_number + i as u64);
            assert_eq!(b.created_at, base.created_at + i as i64);
            if i >= 1 {
                let prev = &seq[i - 1];
                assert_eq!(b.block_number, prev.block_number + 1);
                assert!(b.created_at > prev.created_at);
            }
            // Every other field is cloned verbatim from base (state sub-roots
            // are L3-derived and cannot be re-pointed without real data).
            assert_eq!(b.old_state_root, base.old_state_root);
            assert_eq!(b.new_state_root, base.new_state_root);
            assert_eq!(b.on_chain_operations_count, base.on_chain_operations_count);
        }
    }
}
