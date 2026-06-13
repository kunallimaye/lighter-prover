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
use circuit::delta::account_delta_full_leaf::AccountDeltaFullLeaf;
use circuit::delta::cyclic_delta_circuit::{Circuit as _, CyclicDeltaCircuit};
use circuit::delta::delta_constraints::{Circuit as _, DeltaCircuit, DeltaWitness};
use circuit::types::config::{C, CIRCUIT_CONFIG, D, F};
use circuit::types::constants::{
    ACCOUNT_MERKLE_LEVELS, EMPTY_ACCOUNT_DELTA_TREE_ROOT, EMPTY_DELTA_TREE_HASHES,
};
use plonky2::hash::hash_types::HashOut;
use plonky2::plonk::circuit_data::CircuitData;
use plonky2::plonk::proof::ProofWithPublicInputs;

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
pub fn prove_empty_l5_chain() -> Result<Vec<ProofWithPublicInputs<F, C, D>>> {
    // Assert the target invariant is well-defined before doing any work, so the
    // (eventual) construction is checked against the same constant the wrapper
    // pins. EMPTY_ACCOUNT_DELTA_TREE_ROOT is the root of a 48-level all-zero
    // delta tree (EMPTY_DELTA_TREE_HASHES[ACCOUNT_MERKLE_LEVELS]); the merged
    // L5 batch's new_account_delta_tree_root must equal this exactly.
    debug_assert_eq!(
        EMPTY_ACCOUNT_DELTA_TREE_ROOT, EMPTY_DELTA_TREE_HASHES[ACCOUNT_MERKLE_LEVELS],
        "EMPTY_ACCOUNT_DELTA_TREE_ROOT must be the empty 48-level delta-tree root"
    );

    anyhow::bail!(
        "empty-delta-tree-root L5 chain not yet constructible honestly (issue #129): \
         WrapperCircuit::prove_inner requires 8 L5 chain proofs whose merged batch has \
         new_account_delta_tree_root == EMPTY_ACCOUNT_DELTA_TREE_ROOT, i.e. an L5 chain over \
         TX_TYPE_EMPTY blocks anchored to a fully-empty genesis state. That witness (a complete \
         Tx<F> with ~10 merkle-proof arrays consistent against empty trees + a native empty \
         genesis state) is not yet available in-repo and was NOT fabricated or forced. See the \
         doc-comment on l6drive::prove_empty_l5_chain and issue #129 for the precise construction."
    )
}

#[cfg(test)]
mod tests {
    use plonky2::field::types::Field;
    use plonky2::hash::hash_types::HashOut;

    use super::*;

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
}
