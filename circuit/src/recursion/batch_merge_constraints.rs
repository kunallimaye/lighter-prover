// Copyright (c) Elliot Technologies, Inc.
// SPDX-License-Identifier: BUSL-1.1

//! Issue #82: pre-L5 block-proof aggregation MERGE circuit (ADR-0003 §D5).
//!
//! Verifies TWO L5 batch proofs covering adjacent block ranges and produces a
//! proof whose public-input surface is byte-for-byte the **L5 self-shape**
//! ([`crate::recursion::cyclic_circuit::CyclicRecursionCircuit`] — `Batch` +
//! `SegmentInfo` + verifier data). Because the root proof's PI surface equals
//! the L5 self-shape, the tree root is consumable anywhere an L5 proof is
//! (e.g. the L6 [`crate::recursion::wrapper_circuit`] inner wrapper). This
//! enables log-depth folding of a batch's per-block L5 proofs: today's serial
//! L5 cyclic fold proves N steps back-to-back; the tree fold proves N L5 leaf
//! proofs (parallelizable) and ~log2(N) levels of merges on the critical path.
//!
//! This mirrors the working L2 tree-fold precedent
//! [`crate::block_tx_chain_merge_constraints`] (issue #67 / PR #69) but
//! operates on the richer L5 PI surface, reusing the already-validated L5/L6
//! merge primitives instead of an inline field merge:
//!
//! * [`BatchTarget::conditionally_merge_consecutive`] — contiguity, monotonic
//!   timestamps, state/delta-root chaining and the priority-op keccak-output
//!   prefix chain (`circuit/src/recursion/batch.rs`). Used unconditionally
//!   here (`cond = true`) because both children of a tree-fold merge node are
//!   always real, adjacent proofs.
//! * [`SegmentInfoTarget::connect_segments`] — the on-chain-ops keccak
//!   start-digest stitch (escape hatch iii). The merge node threads the
//!   on-chain-ops hash chain across the seam exactly as L6 does at
//!   `wrapper_circuit.rs:196-200`, which is what addresses the
//!   non-associativity concern recorded in ADR-0003 §D5.
//!
//! VK selection (variant "per-child conditional VK"): each child is either an
//! L5 LEAF proof (the serial-fold output, verified against the L5 cyclic
//! circuit's constant VK) or a MERGE proof of THIS circuit's own shape
//! (verified cyclically against the verifier data carried in the public
//! inputs), selected per child by a witness boolean. See
//! [`crate::builder::Builder::verify_leaf_or_cyclic_proof`] for why a
//! two-circuit `merge_leaves`/`merge_nodes` split cannot compose under
//! plonky2's single-VK cyclic constraint.
//!
//! Shape: the merge circuit reuses the L5 cyclic circuit's already-closed
//! `CommonCircuitData` (the goal-asserted 2^15 cyclic fixed point) as its
//! self-referential shape, so the merge node fits inside the L5 budget. The
//! custom conditional-VK helper cannot set the fork's `pub(crate)`
//! `goal_common_data`, so callers MUST assert `merge.common == l5.common`
//! after building — exactly as the L2 driver does at
//! `bench/src/bin/bench.rs:970-979`. See [`BatchMergeCircuit::define`].

use anyhow::{Ok, Result};
use log::Level;
use plonky2::field::extension::Extendable;
use plonky2::hash::hash_types::RichField;
use plonky2::iop::target::BoolTarget;
use plonky2::iop::witness::{PartialWitness, WitnessWrite};
use plonky2::plonk::circuit_data::{CircuitConfig, CircuitData, VerifierCircuitTarget};
use plonky2::plonk::config::GenericConfig;
use plonky2::plonk::proof::{ProofWithPublicInputs, ProofWithPublicInputsTarget};
use plonky2::timed;
use plonky2::util::timing::TimingTree;

use super::batch::{BATCH_TARGET_INDEX, BatchTarget, SEGMENT_INFO_INDEX, SegmentInfoTarget};
use crate::keccak::keccak::CircuitBuilderKeccak;
use crate::types::config::{Builder, C, D, F};

pub trait Circuit<C: GenericConfig<D, F = F>, F: RichField + Extendable<D>, const D: usize> {
    /// Defines the merge circuit against the L5 cyclic circuit's data. The L5
    /// circuit's `CommonCircuitData` (an already-closed cyclic fixed point) is
    /// reused as this circuit's self-referential shape, and the L5 verifier
    /// key is embedded as the constant non-cyclic VK option (the "leaf" VK).
    fn define(config: CircuitConfig, l5_cyclic_circuit: &CircuitData<F, C, D>) -> Self;

    /// Fills the partial witness: the two child proofs (covering adjacent
    /// block ranges, left before right) and, per child, whether it is a merge
    /// proof (`true`) or an L5 leaf proof (`false`).
    fn generate_witness(
        target: &BatchMergeTarget,
        circuit_data: &CircuitData<F, C, D>,
        left_proof: &ProofWithPublicInputs<F, C, D>,
        left_is_merge: bool,
        right_proof: &ProofWithPublicInputs<F, C, D>,
        right_is_merge: bool,
    ) -> Result<PartialWitness<F>>;

    fn prove(
        target: &BatchMergeTarget,
        circuit_data: &CircuitData<F, C, D>,
        left_proof: &ProofWithPublicInputs<F, C, D>,
        left_is_merge: bool,
        right_proof: &ProofWithPublicInputs<F, C, D>,
        right_is_merge: bool,
    ) -> Result<ProofWithPublicInputs<F, C, D>>;
}

#[derive(Debug)]
pub struct BatchMergeCircuit {
    pub builder: Builder,
    pub target: BatchMergeTarget,
}

#[derive(Debug)]
pub struct BatchMergeTarget {
    pub left_proof: ProofWithPublicInputsTarget<D>, // L5 batch proof of the left (earlier) range
    pub right_proof: ProofWithPublicInputsTarget<D>, // L5 batch proof of the right (later) range

    pub left_is_merge: BoolTarget, // true: left child is a merge proof; false: an L5 leaf proof
    pub right_is_merge: BoolTarget, // ditto for the right child

    pub self_verifier_data: VerifierCircuitTarget, // verifier data for this circuit (cyclic)

    pub new_batch: BatchTarget,          // public witness — combined range
    pub segment_info: SegmentInfoTarget, // public witness — stitched on-chain-ops start digest
}

impl Circuit<C, F, D> for BatchMergeCircuit {
    fn define(config: CircuitConfig, l5_cyclic_circuit: &CircuitData<F, C, D>) -> Self {
        let mut builder = Builder::new(config);

        // Public inputs: identical sequence (and therefore identical layout)
        // to CyclicRecursionCircuit::new — the Batch fields, then SegmentInfo,
        // then the verifier-data PIs last (see cyclic_circuit.rs:106-108).
        // Registering fresh public targets and binding the merged values to
        // them via `connect_*` (below) guarantees the PI surface is byte-for-
        // byte the L5 self-shape regardless of the internal field ordering of
        // `BatchTarget::new_public`.
        let new_batch = BatchTarget::new_public(&mut builder);
        let segment_info = SegmentInfoTarget::new_public(&mut builder);
        let self_verifier_data = builder.add_verifier_data_public_inputs();

        // Self-referential shape: the L5 cyclic circuit's common data IS the
        // closed 2^15 cyclic fixed point (it builds with goal_common_data
        // match asserted), so reusing it both guarantees L5 leaf proofs fit
        // the child-proof targets and defines the shape this circuit must
        // build to. The build-time equality `merge.common == l5.common` is
        // asserted by callers (the custom verify helper cannot set the fork's
        // pub(crate) goal_common_data).
        let self_common = l5_cyclic_circuit.common.clone();
        assert_eq!(
            builder.num_public_inputs(),
            self_common.num_public_inputs,
            "merge circuit PI surface must match the L5 cyclic circuit's"
        );

        let left_is_merge = builder.add_virtual_bool_target_safe();
        let right_is_merge = builder.add_virtual_bool_target_safe();
        let left_proof = builder.add_virtual_proof_with_pis(&self_common);
        let right_proof = builder.add_virtual_proof_with_pis(&self_common);

        // Per-child conditional-VK verification (L5 leaf constant VK vs own
        // cyclic VK) — see the module docs and the helper's docs.
        let leaf_verifier_data = builder.constant_verifier_data(&l5_cyclic_circuit.verifier_only);
        builder.verify_leaf_or_cyclic_proof::<C>(
            left_is_merge,
            &left_proof,
            &self_verifier_data,
            &leaf_verifier_data,
            &self_common,
        );
        builder.verify_leaf_or_cyclic_proof::<C>(
            right_is_merge,
            &right_proof,
            &self_verifier_data,
            &leaf_verifier_data,
            &self_common,
        );

        // ---- Reconstruct both children's L5 PI surface.
        // The `Batch` section occupies `[..BATCH_TARGET_INDEX]` and the
        // `SegmentInfo` section `[BATCH_TARGET_INDEX..SEGMENT_INFO_INDEX]`
        // (the trailing verifier-data PIs follow), exactly as
        // cyclic_circuit.rs:161-166 and wrapper_circuit.rs:161-170 parse them.
        let left_batch =
            BatchTarget::from_public_inputs(&left_proof.public_inputs[..BATCH_TARGET_INDEX]);
        let right_batch =
            BatchTarget::from_public_inputs(&right_proof.public_inputs[..BATCH_TARGET_INDEX]);
        let left_segment = SegmentInfoTarget::from_public_inputs(
            &left_proof.public_inputs[BATCH_TARGET_INDEX..SEGMENT_INFO_INDEX],
        );
        let right_segment = SegmentInfoTarget::from_public_inputs(
            &right_proof.public_inputs[BATCH_TARGET_INDEX..SEGMENT_INFO_INDEX],
        );

        // ---- On-chain-ops seam (escape hatch iii): the left child's running
        // on-chain-ops hash must equal the right child's segment-start digest.
        // This is the exact stitch L6 performs at wrapper_circuit.rs:221-225
        // before the merge, and is what makes the keccak chain associative
        // across the tree (ADR-0003 §D5). The merge always combines two real,
        // contiguous children, so the seam check is unconditional.
        let always = builder._true();
        builder.conditional_assert_eq_keccak_output(
            always,
            left_batch.on_chain_operations_pub_data_hash,
            right_segment.old_on_chain_operations_pub_data_hash,
        );

        // ---- Merge glue: combine the two batches. With `cond = true` this
        // enforces contiguity (end_block_number/batch_size), monotonic
        // timestamps, state-root and account-delta-root continuity, and the
        // priority-op keccak-output prefix chain, then selects the merged
        // fields (old_* from left, new_* from right, counts summed).
        let merged_batch = BatchTarget::conditionally_merge_consecutive(
            &mut builder,
            always,
            &left_batch,
            &right_batch,
        );

        // ---- SegmentInfo start-digest threading: the merged node carries the
        // LEFT child's segment-start on-chain-ops digest (so the running hash
        // chain remains anchored at the segment's true start across merges),
        // mirroring `connect_segments` and the L6 form. The right child's
        // segment-start was already constrained to equal the left child's
        // running on-chain-ops hash by the seam assertion above.
        let merged_segment = left_segment.clone();

        // ---- Bind the merged values to the registered L5-layout public
        // inputs. `connect_batches`/`connect_segments` constrain the fresh
        // public targets to equal the computed merge result; this is the same
        // idiom CyclicRecursionCircuit uses (cyclic_circuit.rs:375-383) and
        // guarantees the root PI surface equals the L5 self-shape.
        new_batch.connect_batches(&mut builder, &merged_batch);
        segment_info.connect_segments(&mut builder, &merged_segment);

        builder.perform_registered_range_checks();

        // Pad to the L5 self-shape's pre-build row budget. The two cyclic
        // child verifications plus the merge glue leave the raw circuit at
        // ~2^14, but the L5 cyclic fixed point closes at 2^15 (its
        // `common_data_for_recursion` pads to `1 << 14` gates and the cyclic
        // verification machinery then spills past it to 2^15). Pad to the same
        // `1 << 14` pre-build gate floor so the merge circuit blinds/pads into
        // the EXACT 2^15 self-shape — the equality the driver asserts
        // (`merge.common == l5.common`). Mirrors the L2 merge's pad-to-`1<<13`
        // at `block_tx_chain_merge_constraints.rs:336-342`.
        while builder.num_gates() < 1 << 14 {
            builder.add_gate(plonky2::gates::noop::NoopGate, vec![]);
        }

        Self {
            target: BatchMergeTarget {
                left_proof,
                right_proof,
                left_is_merge,
                right_is_merge,
                self_verifier_data,
                new_batch,
                segment_info,
            },
            builder,
        }
    }

    fn generate_witness(
        target: &BatchMergeTarget,
        circuit_data: &CircuitData<F, C, D>,
        left_proof: &ProofWithPublicInputs<F, C, D>,
        left_is_merge: bool,
        right_proof: &ProofWithPublicInputs<F, C, D>,
        right_is_merge: bool,
    ) -> Result<PartialWitness<F>> {
        let mut pw = PartialWitness::new();

        pw.set_proof_with_pis_target(&target.left_proof, left_proof)?;
        pw.set_proof_with_pis_target(&target.right_proof, right_proof)?;
        pw.set_bool_target(target.left_is_merge, left_is_merge)?;
        pw.set_bool_target(target.right_is_merge, right_is_merge)?;
        pw.set_verifier_data_target(&target.self_verifier_data, &circuit_data.verifier_only)?;

        // All public-input targets (new_batch, segment_info) receive their
        // values through copy constraints from the child proofs' public inputs.
        Ok(pw)
    }

    fn prove(
        target: &BatchMergeTarget,
        circuit_data: &CircuitData<F, C, D>,
        left_proof: &ProofWithPublicInputs<F, C, D>,
        left_is_merge: bool,
        right_proof: &ProofWithPublicInputs<F, C, D>,
        right_is_merge: bool,
    ) -> Result<ProofWithPublicInputs<F, C, D>> {
        let mut timing = TimingTree::new("BatchMergeCircuit", Level::Debug);

        let pw = timed!(timing, "witness", {
            Self::generate_witness(
                target,
                circuit_data,
                left_proof,
                left_is_merge,
                right_proof,
                right_is_merge,
            )?
        });
        let proof = circuit_data.prove(pw)?;
        timed!(timing, "verify", { circuit_data.verify(proof.clone())? });

        timing.print();

        Ok(proof)
    }
}
