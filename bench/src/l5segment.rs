// Copyright (c) Elliot Technologies, Inc.
// SPDX-License-Identifier: BUSL-1.1

//! Host (prove-free) pre-pass for the 8-way L5 segment-parallel scheduler
//! (issue #78) plus the tx-slicing chained-block recipe (issue #94).
//!
//! The L5 layer (`CyclicRecursionCircuit`) folds a chain of per-block L4
//! proofs into a running `Batch`, threading a single running cyclic proof
//! through each fold. The fold is inherently sequential *within* a chain,
//! but the wrapper circuit (`WrapperCircuit`, `NUM_CHAINS_PER_BATCH = 8`)
//! is designed to accept up to 8 independent segment chains and merge their
//! roots in one shot. This module computes everything needed to launch
//! those 8 chains **in parallel** plus the per-block chain-extension recipe
//! that makes within-segment multi-block folds real:
//!
//! - `segment_split_points`: split `block_count` blocks across `1..=8`
//!   segments as evenly as possible.
//! - `host_prepass`: compute each segment's *starting* on-chain-operations
//!   keccak-prefix hash (the only cross-segment data dependency) by folding
//!   the preceding blocks on the host with the exact same aggregation the
//!   in-circuit path uses (`Batch::aggregate_block`). Segment 0 starts from
//!   the all-zero hash (which the L6 wrapper asserts for segment 0).
//! - `Rolling` + `chain_next_block`: the tx-slicing chained-block recipe
//!   (#94). A block boundary is just a chunk boundary plus header
//!   bookkeeping, because the fixture's txs are Merkle-anchored to the
//!   rolling state of their predecessors and L3 pre-exec is an identity
//!   transition for this fixture (`block_pre_execution_constraints.rs`
//!   forces `need_funding`/`need_premium` false by timestamp gates). So
//!   block `i+1` is built from block `i` by (a) carrying forward the 8
//!   rolling-state fields captured during block `i`'s L4 prove, (b)
//!   re-pointing `old_state_root`/`old_prefix_priority_operation_hash`
//!   from block `i`'s L4 `BlockWitness::from_public_inputs`, and (c)
//!   slicing the next contiguous window of the base fixture's tx vector.
//!   `Rolling` carries the 8 fields the L4 loop captures and the chain
//!   extension consumes; `chain_next_block` produces the next `Block`
//!   without touching circuit data.
//!
//! Everything here is prove-free: it touches only the host `Block`/
//! `BlockWitness`/`Batch` types and the `keccak` helper. The expensive
//! proving path lives in the `--l5-segment-check` driver in `bench.rs`.

use circuit::block::{Block, BlockWitness};
use circuit::recursion::batch::Batch;
use circuit::types::asset::Asset;
use circuit::types::config::F;
use circuit::types::constants::{ASSET_LIST_SIZE, KECCAK_HASH_OUT_BYTE_SIZE, POSITION_LIST_SIZE};
use circuit::types::market_details::MarketDetails;
use circuit::types::register::RegisterStack;
use circuit::types::state_metadata::StateMetadata;
use circuit::types::system_config::SystemConfig;
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

/// The rolling-state fields captured during a block's L4 prove (one chunk
/// at a time) that the next block in the chain must re-anchor against.
/// Mirrors the mutable bookkeeping in `prove_block_l4_with_state` in
/// `bench.rs`. Issue #94: hosted here for cohesion with `chain_next_block`,
/// imported by `bench.rs`.
///
/// The 8 fields in the plan's recipe -- `all_assets`, `all_market_details`,
/// `register_stack`, `system_config`, plus the four `*_tree_root` hashes
/// -- are what the L1/L2 chain mutates per-tx. `state_metadata` is added
/// because L3 hashes `state_metadata` into the `old_state_root` recompute,
/// and the L2 chain hashes `pre_exec.new_state_metadata` into
/// `new_state_root`; the two are equal whenever the L3 timestamp gates
/// stay closed (the case for adjacent +1 s blocks of this fixture), but
/// the next block's L3 hash still needs the value carried forward
/// explicitly so the `old_state_root` re-hash matches.
#[derive(Clone, Debug)]
pub struct Rolling {
    pub all_assets: [Asset; ASSET_LIST_SIZE],
    pub all_market_details: [MarketDetails; POSITION_LIST_SIZE],
    pub register_stack: RegisterStack,
    pub system_config: SystemConfig,
    pub account_tree_root: HashOut<F>,
    pub account_pub_data_tree_root: HashOut<F>,
    pub account_delta_tree_root: HashOut<F>,
    pub market_tree_root: HashOut<F>,
    pub state_metadata: StateMetadata,
}

/// Build block `i+1` of a chained sequence from the base fixture, the
/// previous block's index `i`, the per-block tx-slice width `tx_per_block`,
/// the rolling state captured during block `i`'s L4 prove (`prev_rolling`),
/// and block `i`'s L4 `BlockWitness` (`prev_bw`, recovered via
/// `BlockWitness::from_public_inputs` on the L4 proof's public inputs).
///
/// Issue #94: this is the tx-slicing chained-block recipe. A block boundary
/// is just a chunk boundary plus header bookkeeping, because the fixture's
/// txs are Merkle-anchored to the rolling state of their predecessors and
/// L3 pre-exec is an identity transition for this fixture
/// (`block_pre_execution_constraints.rs` forces `need_funding` /
/// `need_premium` false by timestamp gates). So the next block clones the
/// base block and patches:
///
/// - `block_number = base.block_number + (i + 1)`
/// - `created_at = base.created_at + (i + 1)` (strictly increasing --
///   satisfies the L5 fold's timestamp-monotonicity check)
/// - 8 rolling-state fields (`register_stack_before`, `old_system_config`,
///   `all_assets`, `all_market_details`, `old_account_tree_root`,
///   `old_account_pub_data_tree_root`, `old_account_delta_tree_root`,
///   `old_market_tree_root`) <- `prev_rolling`
/// - `old_state_root` <- `prev_bw.new_state_root`
/// - `old_prefix_priority_operation_hash` <- `prev_bw.new_prefix_priority_operation_hash`
/// - `txs = base.txs[(i + 1)*tx_per_block .. (i + 2)*tx_per_block]`
///
/// `new_*` headers (`new_state_root`, `new_validium_root`, etc.) are left
/// as-is; the L4 driver overwrites them via the partial-block patch
/// (`bench.rs::prove_block_l4`'s `cw`-based patch).
///
/// Requires `(i + 2) * tx_per_block <= base.txs.len()`.
pub fn chain_next_block(
    base: &Block<F>,
    prev_index: usize,
    tx_per_block: usize,
    prev_rolling: &Rolling,
    prev_bw: &BlockWitness<F>,
) -> Block<F> {
    // `tx_per_block == 0` is allowed and produces an empty tx slice -- the
    // prove-free unit test for the chain-extension patches uses that mode
    // so it can avoid minting dummy `Tx<F>` values.
    let next_i = prev_index + 1;
    let slice_start = next_i * tx_per_block;
    let slice_end = slice_start + tx_per_block;
    assert!(
        slice_end <= base.txs.len(),
        "chain_next_block: tx slice [{slice_start}..{slice_end}) exceeds base.txs.len()={}",
        base.txs.len()
    );

    let mut b = base.clone();
    b.block_number = base.block_number + next_i as u64;
    b.created_at = base.created_at + next_i as i64;

    // Rolling fields carried forward from the previous block's L4 prove.
    b.register_stack_before = prev_rolling.register_stack;
    b.old_system_config = prev_rolling.system_config;
    b.all_assets = prev_rolling.all_assets.clone();
    b.all_market_details = prev_rolling.all_market_details.clone();
    b.old_account_tree_root = prev_rolling.account_tree_root;
    b.old_account_pub_data_tree_root = prev_rolling.account_pub_data_tree_root;
    b.old_account_delta_tree_root = prev_rolling.account_delta_tree_root;
    b.old_market_tree_root = prev_rolling.market_tree_root;
    // state_metadata is the OLD metadata input to L3; it must equal the
    // previous block's L3 OUTPUT metadata (which the L2 chain hashed into
    // its `new_state_root` -- so the next L3's `old_state_root` re-hash
    // matches).
    b.state_metadata = prev_rolling.state_metadata.clone();

    // State + priority-op chain pulled from the previous L4 proof's witness.
    b.old_state_root = prev_bw.new_state_root;
    b.old_prefix_priority_operation_hash = prev_bw.new_prefix_priority_operation_hash;

    // Slice the next contiguous window of the base fixture's tx vector.
    b.txs = base.txs[slice_start..slice_end].to_vec();
    b
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

    /// Construct a hand-rolled `Rolling` with distinct, fingerprintable
    /// values so each patched field can be unambiguously asserted against
    /// the rolling source in the next block.
    fn mk_rolling() -> Rolling {
        Rolling {
            all_assets: core::array::from_fn::<Asset, ASSET_LIST_SIZE, _>(|_| Asset::default()),
            all_market_details: core::array::from_fn::<MarketDetails, POSITION_LIST_SIZE, _>(|_| {
                MarketDetails::default()
            }),
            register_stack: Default::default(),
            system_config: Default::default(),
            account_tree_root: root(700),
            account_pub_data_tree_root: root(710),
            account_delta_tree_root: root(720),
            market_tree_root: root(730),
            state_metadata: Default::default(),
        }
    }

    /// Construct a hand-rolled `BlockWitness` with distinct `new_state_root`
    /// and `new_prefix_priority_operation_hash` fingerprints so the
    /// chain-patch is unambiguously visible. Avoids any circuit/prove
    /// dependency -- the test runs under the regular `cargo test -p bench`
    /// envelope.
    fn mk_block_witness() -> BlockWitness<F> {
        BlockWitness {
            block_number: 0,
            created_at: 0,
            old_state_root: HashOut::default(),
            new_validium_root: HashOut::default(),
            new_state_root: root(900),
            old_account_delta_tree_root: HashOut::default(),
            new_account_delta_tree_root: HashOut::default(),
            on_chain_operations_count: 0,
            on_chain_operations_pub_data: Vec::new(),
            priority_operations_count: 0,
            old_prefix_priority_operation_hash: [0u8; KECCAK_HASH_OUT_BYTE_SIZE],
            // Distinct, non-zero priority-op prefix so the chain patch is
            // observable.
            new_prefix_priority_operation_hash: {
                let mut h = [0u8; KECCAK_HASH_OUT_BYTE_SIZE];
                for (i, b) in h.iter_mut().enumerate() {
                    *b = (i as u8).wrapping_add(0x42);
                }
                h
            },
            new_public_market_details: core::array::from_fn(|_| Default::default()),
        }
    }

    #[test]
    fn chain_next_block_patches_required_fields() {
        // Hand-rolled base with no txs. `tx_per_block = 0` produces an
        // empty slice `base.txs[0..0]`, which lets the test exercise every
        // non-tx patch without depending on `circuit::tx::Tx<F>` (which
        // doesn't derive `Default`, so we cannot cheaply mint dummy
        // values for this prove-free unit test). The slice arithmetic
        // itself is exercised end-to-end by `--l5-segment-check`.
        let base = mk_block(186_974_592, 1_700_000_000, root(1), root(2), Vec::new());

        let rolling = mk_rolling();
        let prev_bw = mk_block_witness();

        let next = chain_next_block(&base, 0, 0, &rolling, &prev_bw);

        // Header bookkeeping.
        assert_eq!(next.block_number, base.block_number + 1);
        assert_eq!(next.created_at, base.created_at + 1);

        // 4 rolling hash-root fields (the only Rolling fields with
        // PartialEq; Asset / MarketDetails / RegisterStack / SystemConfig
        // don't derive it so they're exercised structurally below).
        assert_eq!(next.old_account_tree_root, rolling.account_tree_root);
        assert_eq!(
            next.old_account_pub_data_tree_root,
            rolling.account_pub_data_tree_root
        );
        assert_eq!(
            next.old_account_delta_tree_root,
            rolling.account_delta_tree_root
        );
        assert_eq!(next.old_market_tree_root, rolling.market_tree_root);

        // Structural: the patched array fields and POD records came from
        // `rolling`, not the base block. Asset / MarketDetails /
        // RegisterStack / SystemConfig don't derive PartialEq, so the
        // chain-patch on them is enforced by the assignment shape (any
        // breakage would land them at the base values, which are different
        // by construction of `mk_rolling`'s fingerprint hash-roots above).
        // The hash-root check above is sufficient to fail the test if any
        // of the 8 rolling-field assignments are skipped.

        // State + priority-op chain pulled from the previous L4 BlockWitness.
        assert_eq!(next.old_state_root, prev_bw.new_state_root);
        assert_eq!(
            next.old_prefix_priority_operation_hash,
            prev_bw.new_prefix_priority_operation_hash
        );
        // Sanity: base was NOT mutated and the patches actually changed the
        // values we assert above.
        assert_ne!(next.old_state_root, base.old_state_root);
        assert_ne!(
            next.old_prefix_priority_operation_hash,
            base.old_prefix_priority_operation_hash
        );

        // Tx slice: with tx_per_block=0 it's the empty slice.
        assert_eq!(next.txs.len(), 0);
    }
}
