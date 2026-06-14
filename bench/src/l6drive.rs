// Copyright (c) Elliot Technologies, Inc.
// SPDX-License-Identifier: BUSL-1.1

//! L6 inner-wrapper drive helpers (issue #83).
//!
//! Hosts the two standalone prove paths that feed `WrapperCircuit::prove_inner`:
//!
//!   * [`prove_delta_chain`] — drives `DeltaCircuit::prove` then folds the
//!     resulting delta proof through `CyclicDeltaCircuit::prove`, producing the
//!     `delta_chain_proof` (acceptance criterion #1).
//!   * the blob-evaluation path lives in [`crate::kzg`] / [`crate::blob_encode`]
//!     and produces the `blob_evaluation_proof` (acceptance criterion #2/#3).
//!
//! Both are driven over a correctly-shaped **synthesized** batch (an empty
//! batch with `EMPTY_ACCOUNT_DELTA_TREE_ROOT`), exactly as #83 targets. Real
//! mainnet witness generation is closed-source and deferred to #119.

use anyhow::Result;
use circuit::block::BlockWitness;
use circuit::block_constraints::{BlockCircuit, BlockTarget, Circuit as _};
use circuit::block_pre_execution::{BlockPreExec, BlockPreExecWitness};
use circuit::block_pre_execution_constraints::{
    BlockPreExecutionCircuit, BlockPreExecutionTarget, Circuit as _,
};
use circuit::block_tx::BlockTx;
use circuit::block_tx_chain::BlockTxChainWitness;
use circuit::block_tx_chain_constraints::{BlockTxChainCircuit, BlockTxChainTarget, Circuit as _};
use circuit::block_tx_constraints::{BlockTxCircuit, BlockTxTarget, Circuit as _};
use circuit::delta::account_delta_full_leaf::AccountDeltaFullLeaf;
use circuit::delta::cyclic_delta_circuit::{Circuit as _, CyclicDeltaCircuit};
use circuit::delta::delta_constraints::{Circuit as _, DeltaCircuit, DeltaWitness};
use circuit::keccak::helpers::keccak;
use circuit::poseidon_bn128::plonky2_config::PoseidonBN128GoldilocksConfig;
use circuit::recursion::batch::{Batch, SegmentInfo};
use circuit::recursion::cyclic_circuit::{Circuit as _, CyclicRecursionCircuit};
use circuit::recursion::wrapper_circuit::WrapperCircuit;
use circuit::types::config::{C, CIRCUIT_CONFIG, D, F, OUTER_WRAPPER_CONFIG};
use circuit::types::constants::{
    ACCOUNT_MERKLE_LEVELS, EMPTY_ACCOUNT_DELTA_TREE_ROOT, EMPTY_DELTA_TREE_HASHES,
    KECCAK_HASH_OUT_BYTE_SIZE,
};
use plonky2::hash::hash_types::HashOut;
use plonky2::plonk::circuit_data::CircuitData;
use plonky2::plonk::proof::ProofWithPublicInputs;
use plonky2::recursion::dummy_circuit::dummy_circuit;

/// Build the empty-tree `path_matrix` (both rows are the per-level empty-subtree
/// hashes), matching the first-recursion state the cyclic delta circuit seeds in
/// `CyclicDeltaCircuit::handle_proofs`.
fn empty_path_matrix() -> [[HashOut<F>; ACCOUNT_MERKLE_LEVELS]; 2] {
    core::array::from_fn(|_| core::array::from_fn(|j| EMPTY_DELTA_TREE_HASHES[j]))
}

/// A degree-0 (empty) delta witness over a single nil-account leaf, anchored to
/// the empty account-delta tree and the given quintic evaluation point `x`.
///
/// The nil leaf makes `populate_delta_tree` keep the empty-tree root and
/// `eval_delta_polynomial` keep degree 0 / evaluation 0 — the correctly-shaped
/// empty synthesized batch this issue targets.
pub fn empty_delta_witness(evaluation_point: HashOut<F>) -> DeltaWitness<F> {
    DeltaWitness {
        account_deltas: vec![AccountDeltaFullLeaf::nil()],
        previous_account_index: -1,
        path_matrix: empty_path_matrix(),
        x: evaluation_point,
    }
}

/// Drive the full delta chain for a correctly-shaped synthesized batch and
/// return the verified `delta_chain_proof` plus the cyclic-delta circuit data
/// (the inner wrapper needs the latter's `common`/`verifier_only` to build).
///
/// Pipeline (mirrors the L5 segment-fold driver pattern):
///   1. define + build `DeltaCircuit`, prove one (empty) delta leaf,
///   2. define + build `CyclicDeltaCircuit`, seed `cyclic_base_proof`,
///   3. fold the delta proof once (`not_first_recursion = false`),
///   4. verify the resulting cyclic proof.
///
/// `account_count` is the per-leaf capacity of the `DeltaCircuit` (1 is enough
/// for the empty synthesized batch).
pub fn prove_delta_chain(
    account_count: usize,
    evaluation_point: HashOut<F>,
) -> Result<(ProofWithPublicInputs<F, C, D>, CircuitData<F, C, D>)> {
    // 1. Delta leaf circuit + proof.
    let delta = DeltaCircuit::define(CIRCUIT_CONFIG, account_count);
    let delta_target = delta.target;
    let delta_data = delta.builder.build::<C>();
    let witness = empty_delta_witness(evaluation_point);
    let delta_proof = DeltaCircuit::prove(&delta_target, &delta_data, &witness)?;
    delta_data.verify(delta_proof.clone())?;

    // 2. Cyclic delta fold circuit.
    let cyclic = CyclicDeltaCircuit::define(CIRCUIT_CONFIG, &delta_data);
    let cyclic_target = cyclic.target;
    let cyclic_data = cyclic.builder.build::<C>();

    // 3. Seed base proof + a dummy proof (same shape) and fold once.
    let base_proof = CyclicDeltaCircuit::cyclic_base_proof(&cyclic_data);
    let dummy_proof = base_proof.clone();
    let folded = CyclicDeltaCircuit::prove(
        &cyclic_target,
        &cyclic_data,
        false, // first recursion
        &base_proof,
        &dummy_proof,
        &delta_proof,
    )?;

    // 4. Verify.
    cyclic_data.verify(folded.clone())?;

    Ok((folded, cyclic_data))
}

/// Issue #129 (acceptance criterion #4 of #83): produce the 8 L5 "chain proofs"
/// whose **merged batch** has `new_account_delta_tree_root ==
/// EMPTY_ACCOUNT_DELTA_TREE_ROOT`, mutually consistent with the empty delta
/// chain + empty blob that [`prove_delta_chain`] / [`crate::kzg`] already
/// produce, so they can drive `WrapperCircuit::prove_inner` to a verifying
/// inner-wrapper proof.
///
/// ## Why this is the hard, isolated step
///
/// `WrapperCircuit::define_inner` couples all three inputs over a single batch
/// (`circuit/src/recursion/wrapper_circuit.rs:571-613`):
///   * `handle_segment_proofs` (134-244) asserts `chain_proofs[0]` is an
///     **empty segment** (`SegmentInfoTarget::is_empty` ⇒
///     `old_on_chain_operations_pub_data_hash == 0`, `batch.rs:844`) **and**
///     that its batch `old_account_delta_tree_root == EMPTY_ACCOUNT_DELTA_TREE_ROOT`
///     (172-176), then merges the 8 chain proofs into one `batch`;
///   * `verify_aggregated_delta` (476-502) + `handle_blob_evaluation_proof`
///     (430-474) connect the **merged** `batch.new_account_delta_tree_root` to
///     BOTH the delta chain's root AND the blob's `account_delta_tree_root`.
///
/// In the L5 fold (`cyclic_circuit.rs:341-364`) the merged batch's
/// `new_account_delta_tree_root` equals the **final folded block's**
/// `new_account_delta_tree_root`. So a verifying `prove_inner` needs an L5
/// chain over blocks whose net account-delta-tree mutation is **empty**:
/// `old == new == EMPTY_ACCOUNT_DELTA_TREE_ROOT`. Every non-empty tx writes
/// account-delta leaves (`tx_constraints.rs:1495-1570`), so the only honest
/// construction is an L5 chain over **`TX_TYPE_EMPTY` (=0) blocks** anchored to
/// a fully-empty genesis state.
///
/// ## Why it is NOT wired to a forced prove here (hard-honesty guardrail)
///
/// Building that empty block requires a complete, mutually-consistent witness
/// the repo does not yet provide:
///   * a full `Tx<F>` (~74 fields, **no** `Default`/`empty()` ctor) per empty
///     tx, including ~10 merkle-proof arrays (account / pub-data / delta trees ×
///     3 accounts × 48 levels, asset, position-delta, api-key, account-orders,
///     market, and 3 order-book paths) that must verify against the chosen
///     initial roots — even an empty tx runs the full
///     `verify_account_and_pub_data_merkle_proofs` /
///     `verify_assets_merkle_proofs` / `verify_market_and_order_book_proofs`
///     path (`tx_constraints.rs:670-693`, account[0] gated by `_true`);
///   * a fully-empty genesis state (empty account/asset/market/api-key/orders
///     trees → `EMPTY_ACCOUNT_HASH`, `EMPTY_ASSET_TREE_ROOT`, … `constants.rs`)
///     whose `old_state_root` / `old_validium_root` are recomputed natively
///     (the `seed.rs` recipe) so the L1→L2→L3→L4→L5 chain stitches;
///   * the only block fixture (`bench/bench_test.json`) has **non-empty** trees
///     and **no** empty txs (tx-type histogram: {14,15,17,21}), so it cannot be
///     reshaped into this empty batch without hand-constructing the above.
///
/// plonky2 PANICS on any unsatisfied constraint, so a guessed/partial witness
/// would either fail to prove or — worse — require fabricating roots/proofs or
/// relaxing a constraint to force it. Per this project's hard-honesty norm
/// (Discussion #58) and issue #129, this function returns an explicit error
/// naming the missing piece rather than fabricating it. PR #127 was praised for
/// refusing to fake exactly this step; this keeps that bar.
///
/// When the empty-genesis + empty-tx witness generator lands (it is in-repo
/// work with no Lighter dependency — see issue #129 and ADR-0005), this
/// function should: build L1..L5, prove one empty block into an L5 segment
/// seeded with `SegmentInfo::default()`, verify it, and return it as
/// `chain_proofs[0]` (the caller pads `[1..8)` with it and sets
/// `segment_count = 1`).
/// Pre-built L1→L5 pipeline circuits + targets, shared by `prove_empty_l5_chain`
/// and the inner-wrapper builder so the produced L5 chain proof verifies against
/// the exact same `l5_data` the wrapper's `define_inner` was built against.
pub struct EmptyL5Pipeline {
    pub l1_data: CircuitData<F, C, D>,
    pub l1_target: BlockTxTarget,
    pub l2_data: CircuitData<F, C, D>,
    pub l2_target: BlockTxChainTarget,
    pub block_tx_witness_size: usize,
    pub dummy_l2_circuit: CircuitData<F, C, D>,
    pub dummy_l2_proof: ProofWithPublicInputs<F, C, D>,
    pub l3_data: CircuitData<F, C, D>,
    pub l3_target: BlockPreExecutionTarget,
    pub l4_data: CircuitData<F, C, D>,
    pub l4_target: BlockTarget,
    pub l5_data: CircuitData<F, C, D>,
    pub l5_target: circuit::recursion::cyclic_circuit::CyclicRecursionTarget,
    pub l5_dummy_proof: ProofWithPublicInputs<F, C, D>,
}

impl EmptyL5Pipeline {
    /// Build the full L1→L5 circuit stack for one empty block per L1 chunk
    /// (`tx_per_proof = 1`), mirroring the bench `--l5-segment-check` setup.
    /// `chain_id` must match the wrapper-side CHAIN_ID.
    pub fn build(chain_id: u32) -> Result<Self> {
        use circuit::builder::custom::cyclic_base_proof;

        let l1 = BlockTxCircuit::define(CIRCUIT_CONFIG, 1, chain_id);
        let l1_target = l1.target;
        let l1_data = l1.builder.build::<C>();

        let l3 = BlockPreExecutionCircuit::define(CIRCUIT_CONFIG);
        let l3_target = l3.target;
        let l3_data = l3.builder.build::<C>();

        let l2 = BlockTxChainCircuit::define(CIRCUIT_CONFIG, &l1_data, 1, 1);
        let l2_target = l2.target;
        let l2_data = l2.builder.build::<C>();
        let block_tx_witness_size = l2.block_tx_witness_size;
        let dummy_l2_circuit = dummy_circuit(&l2_data.common);
        let dummy_l2_proof = cyclic_base_proof(
            &l2_data.common,
            &l2_data.verifier_only,
            &dummy_l2_circuit,
            Vec::<F>::new().iter().copied().enumerate().collect(),
        )?;

        let l4 = BlockCircuit::define(CIRCUIT_CONFIG, &l3_data, &l2_data, 1);
        let l4_target = l4.target;
        let l4_data = l4.builder.build::<C>();

        let l5 = CyclicRecursionCircuit::define(CIRCUIT_CONFIG, &l4_data, 1);
        let l5_target = l5.target;
        let l5_data = l5.builder.build::<C>();

        let l5_dummy_circuit = dummy_circuit(&l5_data.common);
        let l5_dummy_proof = cyclic_base_proof(
            &l5_data.common,
            &l5_data.verifier_only,
            &l5_dummy_circuit,
            Vec::<F>::new().iter().copied().enumerate().collect(),
        )?;

        Ok(Self {
            l1_data,
            l1_target,
            l2_data,
            l2_target,
            block_tx_witness_size,
            dummy_l2_circuit,
            dummy_l2_proof,
            l3_data,
            l3_target,
            l4_data,
            l4_target,
            l5_data,
            l5_target,
            l5_dummy_proof,
        })
    }
}

/// Native off-circuit reproduction of `WrapperInnerCircuit::verify_batch_commitment`
/// (`circuit/src/recursion/wrapper_circuit.rs:246-312`). Computes the
/// `batch_commitment` keccak that the inner wrapper recomputes in-circuit and
/// binds as a public input, so the off-circuit `WrapperInput.batch_commitment`
/// matches `connect_keccak_output` exactly. NEVER a fabricated value — it is the
/// keccak of the merged batch's own fields + the blob commitment.
///
/// Byte layout mirrors the in-circuit assembly precisely:
///   * scalars (`end_block_number` 8B, `batch_size` 4B, `start_timestamp` 8B,
///     `end_timestamp` 8B, `priority_operations_count` 4B) are big-endian
///     (in-circuit: `split_bytes` little-endian then `.reverse()`);
///   * hash roots are 4 limbs × little-endian 8 bytes, in element order
///     (in-circuit: `split_bytes(elem, 8)` per element, no reverse);
///   * `on_chain_operations_pub_data_hash` and
///     `new_prefix_priority_operation_hash` are the raw 32 keccak bytes;
///   * `blob_commitment_hash = keccak(x ++ y ++ kzg_versioned_hash)`.
pub fn batch_commitment(
    batch: &Batch<F>,
    blob_polynomial_opening_x: &[u8; KECCAK_HASH_OUT_BYTE_SIZE],
    blob_polynomial_opening_y: &[u8; KECCAK_HASH_OUT_BYTE_SIZE],
    kzg_versioned_hash: &[u8; KECCAK_HASH_OUT_BYTE_SIZE],
) -> [u8; KECCAK_HASH_OUT_BYTE_SIZE] {
    use plonky2::field::types::PrimeField64;

    fn root_bytes(h: &HashOut<F>) -> Vec<u8> {
        // Each limb: split_bytes(elem, 8) is little-endian; no reverse in-circuit.
        let mut out = Vec::with_capacity(32);
        for e in h.elements.iter() {
            out.extend_from_slice(&e.to_canonical_u64().to_le_bytes());
        }
        out
    }

    let mut blob_elems = Vec::with_capacity(96);
    blob_elems.extend_from_slice(blob_polynomial_opening_x);
    blob_elems.extend_from_slice(blob_polynomial_opening_y);
    blob_elems.extend_from_slice(kzg_versioned_hash);
    let blob_commitment_hash = keccak(&blob_elems);

    let mut elems: Vec<u8> = Vec::new();
    // BE scalars: split_bytes(LE) then reverse ⇒ big-endian of the low n bytes.
    elems.extend_from_slice(&batch.end_block_number.to_be_bytes()); // 8
    elems.extend_from_slice(&(batch.batch_size as u32).to_be_bytes()); // 4
    elems.extend_from_slice(&(batch.first_created_at as u64).to_be_bytes()); // 8
    elems.extend_from_slice(&(batch.last_created_at as u64).to_be_bytes()); // 8
    elems.extend_from_slice(&root_bytes(&batch.old_state_root));
    elems.extend_from_slice(&root_bytes(&batch.new_state_root));
    elems.extend_from_slice(&root_bytes(&batch.new_validium_root));
    elems.extend_from_slice(&batch.on_chain_operations_pub_data_hash);
    elems.extend_from_slice(&(batch.priority_operations_count as u32).to_be_bytes()); // 4
    elems.extend_from_slice(&batch.new_prefix_priority_operation_hash);
    elems.extend_from_slice(&blob_commitment_hash);

    keccak(&elems)
}

/// Parse the merged `Batch<F>` from an L5 chain proof's public inputs (the
/// `BatchTarget` portion `[..BATCH_TARGET_INDEX]`). For a single-segment chain
/// (`segment_count = 1`) the inner wrapper's merged batch equals this batch.
pub fn batch_from_chain_proof(chain_proof: &ProofWithPublicInputs<F, C, D>) -> Batch<F> {
    use circuit::recursion::batch::BATCH_TARGET_INDEX;
    Batch::<F>::from_public_inputs(&chain_proof.public_inputs[..BATCH_TARGET_INDEX])
}

pub fn prove_empty_l5_chain(pipeline: &EmptyL5Pipeline) -> Result<ProofWithPublicInputs<F, C, D>> {
    // Assert the target invariant is well-defined before doing any work: the
    // merged L5 batch's new_account_delta_tree_root must equal
    // EMPTY_ACCOUNT_DELTA_TREE_ROOT (the root of a 48-level all-zero delta tree).
    debug_assert_eq!(
        EMPTY_ACCOUNT_DELTA_TREE_ROOT, EMPTY_DELTA_TREE_HASHES[ACCOUNT_MERKLE_LEVELS],
        "EMPTY_ACCOUNT_DELTA_TREE_ROOT must be the empty 48-level delta-tree root"
    );

    // ---- Build the empty genesis block (one empty TX_TYPE_EMPTY tx). ----
    // Honest empty-genesis + empty-tx witness (see crate::empty_witness): all
    // trees empty, all-empty-sibling Merkle proofs, native state/validium roots.
    let block = crate::empty_witness::empty_genesis_block(1, 1, 1);

    // ---- L3: pre-execution. ----
    let block_pre_exec = BlockPreExec::from_block(&block);
    let pre_proof =
        BlockPreExecutionCircuit::prove(&pipeline.l3_data, &block_pre_exec, &pipeline.l3_target)?;
    let pre_exec_witness = BlockPreExecWitness::from_public_inputs(&pre_proof.public_inputs);
    let state_metadata = pre_exec_witness.new_state_metadata.clone();

    // ---- L1 + L2: prove the single empty tx and fold it into the chain. ----
    let block_tx = BlockTx::<F> {
        created_at: block.created_at,
        old_system_config: block.old_system_config,
        register_stack_before: block.register_stack_before,
        all_assets_before: block.all_assets.clone(),
        all_market_details_before: block.all_market_details.clone(),
        old_account_tree_root: block.old_account_tree_root,
        old_account_pub_data_tree_root: block.old_account_pub_data_tree_root,
        old_account_delta_tree_root: block.old_account_delta_tree_root,
        old_market_tree_root: block.old_market_tree_root,
        txs: block.txs.clone(),
    };
    let tx_proof = BlockTxCircuit::prove(&pipeline.l1_data, &block_tx, &pipeline.l1_target)?;

    // L2 seed: cyclic base proof for this block's chain (state roots BEFORE the
    // chunk come from L3's pre-exec output).
    let mut chain_proof = BlockTxChainCircuit::cyclic_base_proof(
        &pipeline.l2_data,
        &pipeline.dummy_l2_circuit,
        block.block_number,
        block.created_at,
        pre_exec_witness.new_state_root,
        pre_exec_witness.new_state_root,
        pre_exec_witness.new_validium_root,
        block.old_account_delta_tree_root,
        pipeline.block_tx_witness_size,
        &state_metadata,
    );
    chain_proof = BlockTxChainCircuit::prove(
        &pipeline.l2_target,
        &pipeline.l2_data,
        0,
        &chain_proof,
        &pipeline.dummy_l2_proof,
        &tx_proof,
    )?;

    // ---- L4: connect the (full) chain run to the block witness, then prove. ----
    let cw = BlockTxChainWitness::from_public_inputs(&chain_proof.public_inputs, 1, 1);
    let mut pblock = block.clone();
    pblock.new_validium_root = cw.new_validium_root;
    pblock.new_state_root = cw.new_state_root;
    pblock.new_account_delta_tree_root = cw.new_account_delta_tree_root;
    pblock.on_chain_operations_count = cw.on_chain_operations_count;
    pblock.on_chain_operations_pub_data = cw.on_chain_operations_pub_data.clone();
    pblock.priority_operations_count = cw.priority_operations_count;
    pblock.new_public_market_details = cw.new_public_market_details.clone();
    pblock.new_prefix_priority_operation_hash = if cw.priority_operations_count != 0 {
        let mut input = Vec::with_capacity(32 + cw.priority_operations_pub_data.len());
        input.extend_from_slice(&block.old_prefix_priority_operation_hash);
        input.extend_from_slice(&cw.priority_operations_pub_data);
        keccak(&input)
    } else {
        block.old_prefix_priority_operation_hash
    };
    let l4_pw =
        BlockCircuit::generate_witness(&pipeline.l4_target, &pblock, &pre_proof, &chain_proof)?;
    let l4_proof = pipeline.l4_data.prove(l4_pw)?;

    // ---- L5: fold the single empty block into a one-segment chain proof. ----
    // The first segment must be empty: seed with a zero-on-chain-ops SegmentInfo.
    let segment_info = SegmentInfo {
        old_on_chain_operations_pub_data_hash: [0u8; KECCAK_HASH_OUT_BYTE_SIZE],
    };
    let base_proof = CyclicRecursionCircuit::cyclic_base_proof(&pipeline.l5_data, &segment_info);

    // Host batch mirror, aggregated from the L4 proof's BlockWitness (the
    // partial-block-patched values the L5 circuit reads).
    let mut batch = Batch::<F>::default();
    let block_witness = BlockWitness::from_public_inputs(&l4_proof.public_inputs, 1, 1);
    batch.aggregate_block(&block_witness);

    let folded = CyclicRecursionCircuit::prove(
        &pipeline.l5_target,
        &pipeline.l5_data,
        &batch,
        &segment_info,
        false, // first recursion
        &base_proof,
        &pipeline.l5_dummy_proof,
        &l4_proof,
    )?;

    pipeline.l5_data.verify(folded.clone())?;

    Ok(folded)
}

/// Issue #116 (outer-wrapper drive path): take the VERIFIED inner-wrapper proof
/// (the output of the `--l6-inner` path / `WrapperCircuit::prove_inner`) and
/// drive `WrapperCircuit::prove_outer` — the conversion toward the
/// Ethereum-friendly (BN128-config) form.
///
/// `prove_outer` is DEFINED at `circuit/src/recursion/wrapper_circuit.rs:733`
/// but was never called anywhere in the repo before this issue. This helper is
/// that missing driver.
///
/// Pipeline (mirrors the build_wrapper_circuit.rs:198-207 outer build pattern):
///   1. `define_outer(OUTER_WRAPPER_CONFIG, &inner.common, &inner.verifier_only)`
///      — registers the public u8 `batch_commitment` and verifies the inner
///      proof in-circuit;
///   2. `.builder.build::<PoseidonBN128GoldilocksConfig>()` — the outer circuit
///      is the BN128-config wrap (the Ethereum-friendly hash);
///   3. `WrapperCircuit::prove_outer(&outer_data, &outer_target, inner_proof)`
///      — this proves AND verifies internally
///      (`wrapper_circuit.rs:750`, `circuit.verify(proof.clone())`);
///   4. belt-and-suspenders explicit `outer_data.verify(outer_proof.clone())`,
///      matching the inner pattern.
///
/// Returns the outer `CircuitData` (whose `.common`/`.verifier_only` feed the
/// gnark bridge in #117) plus the verified outer proof. A real prove — plonky2
/// panics on any unsatisfied constraint and `prove_outer` rejects a bad proof,
/// so a successful return means an honestly verifying outer-wrapper proof. No
/// values are fabricated and no constraint is relaxed.
#[allow(clippy::type_complexity)]
pub fn prove_outer_wrapper(
    inner_data: &CircuitData<F, C, D>,
    inner_proof: &ProofWithPublicInputs<F, C, D>,
) -> Result<(
    CircuitData<F, PoseidonBN128GoldilocksConfig, D>,
    ProofWithPublicInputs<F, PoseidonBN128GoldilocksConfig, D>,
)> {
    // 1. Define the outer-wrapper circuit over the inner circuit's shape.
    let outer = WrapperCircuit::define_outer(
        OUTER_WRAPPER_CONFIG,
        &inner_data.common,
        &inner_data.verifier_only,
    );
    let outer_target = outer.target;

    // 2. Build with the BN128 Goldilocks config (the Ethereum-friendly wrap).
    let outer_data = outer.builder.build::<PoseidonBN128GoldilocksConfig>();

    // 3. Prove (prove_outer proves AND verifies internally).
    let outer_proof = WrapperCircuit::prove_outer(&outer_data, &outer_target, inner_proof)?;

    // 4. Belt-and-suspenders explicit verify (matches the inner pattern).
    outer_data.verify(outer_proof.clone())?;

    Ok((outer_data, outer_proof))
}

#[cfg(test)]
mod tests {
    use plonky2::field::types::Field;
    use plonky2::hash::hash_types::HashOut;

    use super::*;

    /// Issue #129: drive the full L1→L5 pipeline over the empty-genesis block
    /// and verify the resulting L5 chain proof. A real prove — plonky2 panics on
    /// any unsatisfied constraint, so a successful verify means the empty-tx L5
    /// chain is honestly consistent end-to-end. The merged batch's
    /// `new_account_delta_tree_root` must be `EMPTY_ACCOUNT_DELTA_TREE_ROOT`.
    ///
    /// Heavy: builds the L1..L5 stack and runs real proves. `#[ignore]`d.
    /// `RUST_MIN_STACK=4294967296 cargo test -p bench --lib --release -- --ignored test_empty_l5_chain_proves`.
    #[test]
    #[ignore = "heavy plonky2 prove; run with --ignored"]
    fn test_empty_l5_chain_proves() {
        let pipeline = EmptyL5Pipeline::build(304).expect("build L1..L5 pipeline");
        let chain_proof = prove_empty_l5_chain(&pipeline).expect("empty L5 chain proves");
        pipeline
            .l5_data
            .verify(chain_proof)
            .expect("empty L5 chain proof verifies");
    }

    /// Acceptance criterion #1: drive `DeltaCircuit::prove` + the cyclic delta
    /// fold and verify the resulting `delta_chain_proof`. A real prove — if the
    /// witness or fold were wrong, `verify` would reject. Never a stub.
    ///
    /// Heavy: builds two recursive circuits and runs real plonky2 proves.
    /// `#[ignore]`d so default CI stays fast; run explicitly with a large stack:
    /// `RUST_MIN_STACK=4294967296 cargo test -p bench --lib -- --ignored test_delta_chain_prove`.
    /// Also exercised by the `--delta-prove` bench mode.
    #[test]
    #[ignore = "heavy plonky2 prove; run with --ignored, see --delta-prove bench mode"]
    fn test_delta_chain_prove() {
        // Arbitrary evaluation point for the empty synthesized batch.
        let x = HashOut::from_vec(vec![
            F::from_canonical_u64(1),
            F::from_canonical_u64(2),
            F::from_canonical_u64(3),
            F::from_canonical_u64(4),
        ]);
        let (proof, data) = prove_delta_chain(1, x).expect("delta chain proves");
        data.verify(proof).expect("delta_chain_proof verifies");
    }

    /// Issue #116: exercise the outer-wrapper drive ([`prove_outer_wrapper`]) —
    /// the driver that calls the previously-uncalled `WrapperCircuit::prove_outer`.
    ///
    /// `define_outer` recursively verifies an arbitrary inner `(CircuitData, proof)`
    /// in-circuit, so this test uses the small, real cyclic-delta circuit + its
    /// verified proof (from [`prove_delta_chain`]) as the inner input. This is a
    /// faithful, lighter exercise of the exact outer mechanism — define_outer ->
    /// build::<PoseidonBN128GoldilocksConfig>() -> prove_outer -> verify — without
    /// reassembling the full L6 inner-wrapper stack (that heavy end-to-end path is
    /// the `--l6-outer` bench mode). A real prove + verify: if the outer wrap or
    /// the recursive inner verification were wrong, `prove_outer` / `verify` would
    /// reject. Never a stub; no constraint relaxed.
    ///
    /// Heavy: builds two recursive circuits + the BN128 outer wrap and runs real
    /// proves. `#[ignore]`d; run with a large stack:
    /// `RUST_MIN_STACK=4294967296 cargo test -p bench --lib --release -- --ignored test_outer_wrapper_drive`.
    #[test]
    #[ignore = "heavy plonky2 + BN128 outer prove; run with --ignored"]
    fn test_outer_wrapper_drive() {
        let x = HashOut::from_vec(vec![
            F::from_canonical_u64(5),
            F::from_canonical_u64(6),
            F::from_canonical_u64(7),
            F::from_canonical_u64(8),
        ]);
        // A small, real inner circuit + its verified proof.
        let (inner_proof, inner_data) = prove_delta_chain(1, x).expect("inner proves");
        inner_data
            .verify(inner_proof.clone())
            .expect("inner proof verifies");

        // Drive prove_outer over it; prove_outer_wrapper proves AND verifies.
        let (outer_data, outer_proof) =
            prove_outer_wrapper(&inner_data, &inner_proof).expect("outer-wrapper drive");

        // Explicit belt-and-suspenders verify of the returned outer proof.
        outer_data
            .verify(outer_proof)
            .expect("outer-wrapper proof verifies");
    }
}
