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
use circuit::types::constants::{ACCOUNT_MERKLE_LEVELS, EMPTY_DELTA_TREE_HASHES};
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
