// Copyright (c) Elliot Technologies, Inc.
// SPDX-License-Identifier: BUSL-1.1

use anyhow::{Ok, Result};
use log::Level;
use plonky2::iop::target::BoolTarget;
use plonky2::iop::witness::{PartialWitness, WitnessWrite};
use plonky2::plonk::circuit_data::{CircuitConfig, CircuitData};
use plonky2::plonk::proof::{ProofWithPublicInputs, ProofWithPublicInputsTarget};
use plonky2::timed;
use plonky2::util::timing::TimingTree;

use crate::recursion::batch::{BATCH_TARGET_INDEX, Batch, BatchTarget, BatchTargetWitness};
use crate::types::config::{Builder, C, D, F};

pub struct BinaryTreeChainTarget<const D: usize> {
    pub left_child: ProofWithPublicInputsTarget<D>,
    pub right_child: ProofWithPublicInputsTarget<D>,
    /// Whether the right child slot holds a real proof. When `false`, the right
    /// child is padding and the fold preserves the left child's aggregate.
    pub right_is_real: BoolTarget,
    /// Aggregated batch registered as this node's public inputs.
    pub aggregated_batch: BatchTarget,
}

pub struct BinaryTreeChainCircuit {
    pub builder: Builder,
    pub target: BinaryTreeChainTarget<D>,
}

impl BinaryTreeChainCircuit {
    /// Builds a radix-2 reduction-tree aggregation node.
    ///
    /// Verifies both child proofs against the *pinned* child verifying key,
    /// extracts each child's [`BatchTarget`] public inputs, folds left+right
    /// enforcing state-root / block-number / timestamp / delta-root continuity,
    /// and registers the folded aggregate as this node's public inputs.
    pub fn define(config: CircuitConfig, child_circuit: &CircuitData<F, C, D>) -> Self {
        let mut builder = Builder::new(config);

        // Register the aggregated batch as this node's public inputs up front.
        let aggregated_batch = BatchTarget::new_public(&mut builder);

        // Pin the child verifying key (binds this node to one child circuit).
        let child_verifier_data = builder.constant_verifier_data(&child_circuit.verifier_only);
        let child_common_data = &child_circuit.common;

        let right_is_real = builder.add_virtual_bool_target_safe();

        let left_child = builder.add_virtual_proof_with_pis(child_common_data);
        let right_child = builder.add_virtual_proof_with_pis(child_common_data);

        builder.verify_proof::<C>(&left_child, &child_verifier_data, child_common_data);
        builder.verify_proof::<C>(&right_child, &child_verifier_data, child_common_data);

        // Extract child public inputs.
        let left_batch =
            BatchTarget::from_public_inputs(&left_child.public_inputs[..BATCH_TARGET_INDEX]);
        let right_batch =
            BatchTarget::from_public_inputs(&right_child.public_inputs[..BATCH_TARGET_INDEX]);

        // Fold: merge right into left, enforcing adjacency continuity when the
        // right child is real.
        let acc = BatchTarget::conditionally_merge_consecutive(
            &mut builder,
            right_is_real,
            &left_batch,
            &right_batch,
        );

        // Register the folded aggregate as this node's public inputs.
        aggregated_batch.connect_batches(&mut builder, &acc);

        builder.perform_registered_range_checks();

        Self {
            builder,
            target: BinaryTreeChainTarget {
                left_child,
                right_child,
                right_is_real,
                aggregated_batch,
            },
        }
    }

    pub fn prove(
        target: &BinaryTreeChainTarget<D>,
        circuit_data: &CircuitData<F, C, D>,
        left_proof: &ProofWithPublicInputs<F, C, D>,
        right_proof: &ProofWithPublicInputs<F, C, D>,
    ) -> Result<ProofWithPublicInputs<F, C, D>> {
        let mut pw = PartialWitness::new();
        pw.set_proof_with_pis_target(&target.left_child, left_proof)?;
        pw.set_proof_with_pis_target(&target.right_child, right_proof)?;
        // Both proofs are real children here; padding uses a separate prove path.
        pw.set_bool_target(target.right_is_real, true)?;

        // The aggregated public-output BatchTarget is `connect`ed (not generated)
        // to the in-circuit fold, so its witness must be supplied here — mirroring
        // `CyclicRecursionCircuit::generate_witness` which calls `set_batch_target`
        // on its public `new_batch`.
        let left_batch = Batch::<F>::from_public_inputs(&left_proof.public_inputs[..BATCH_TARGET_INDEX]);
        let right_batch =
            Batch::<F>::from_public_inputs(&right_proof.public_inputs[..BATCH_TARGET_INDEX]);
        let aggregated = fold_consecutive(&left_batch, &right_batch, true);
        pw.set_batch_target(&target.aggregated_batch, &aggregated)?;

        let mut timing = TimingTree::new("Binary tree recursive prove", Level::Debug);
        let proof = timed!(timing, "prove", circuit_data.prove(pw))?;
        timed!(timing, "verify", circuit_data.verify(proof.clone())?);
        timing.print();
        Ok(proof)
    }
}

/// Host-side analogue of [`BatchTarget::conditionally_merge_consecutive`].
/// When `cond` is false (padding slot), the accumulator `a` is returned
/// unchanged. When true, `b` is merged into `a` (its new_* fields adopted and
/// batch_size / counts summed), matching the in-circuit `select`/`mul_add`
/// behaviour exactly so the witnessed public output equals the proven fold.
pub(crate) fn fold_consecutive(a: &Batch<F>, b: &Batch<F>, cond: bool) -> Batch<F> {
    if !cond {
        return a.clone();
    }
    Batch::<F> {
        end_block_number: b.end_block_number,
        batch_size: a.batch_size + b.batch_size,
        first_created_at: a.first_created_at,
        last_created_at: b.last_created_at,
        old_state_root: a.old_state_root,
        new_validium_root: b.new_validium_root,
        new_state_root: b.new_state_root,
        old_account_delta_tree_root: a.old_account_delta_tree_root,
        new_account_delta_tree_root: b.new_account_delta_tree_root,
        on_chain_operations_pub_data_hash: b.on_chain_operations_pub_data_hash,
        priority_operations_count: a.priority_operations_count + b.priority_operations_count,
        old_prefix_priority_operation_hash: a.old_prefix_priority_operation_hash,
        new_prefix_priority_operation_hash: b.new_prefix_priority_operation_hash,
        new_public_market_details: b.new_public_market_details.clone(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::config::CIRCUIT_CONFIG;
    use plonky2::field::types::Field;
    use plonky2::hash::hash_types::HashOut;

    struct LeafCircuit {
        data: CircuitData<F, C, D>,
        batch_target: BatchTarget,
    }

    fn build_leaf() -> LeafCircuit {
        let mut builder = Builder::new(CIRCUIT_CONFIG);
        let batch_target = BatchTarget::new_public(&mut builder);
        builder.perform_registered_range_checks();
        let data = builder.build::<C>();
        LeafCircuit { data, batch_target }
    }

    fn build_leaf_variant() -> LeafCircuit {
        let mut builder = Builder::new(CIRCUIT_CONFIG);
        let batch_target = BatchTarget::new_public(&mut builder);
        // Add an unrelated constant constraint (needs no extra witness) to
        // perturb the circuit so it builds to a DISTINCT verifying key.
        let one = builder.one();
        let two = builder.constant(F::from_canonical_u64(2));
        let sum = builder.add(one, one);
        builder.connect(sum, two);
        builder.perform_registered_range_checks();
        let data = builder.build::<C>();
        LeafCircuit { data, batch_target }
    }

    fn prove_leaf(leaf: &LeafCircuit, batch: &Batch<F>) -> ProofWithPublicInputs<F, C, D> {
        let mut pw = PartialWitness::new();
        pw.set_batch_target(&leaf.batch_target, batch).unwrap();
        leaf.data.prove(pw).unwrap()
    }

    /// Runs a proving closure, returning whether it produced a valid proof.
    /// Returns `false` if the closure returned `false` OR panicked (Plonky2 may
    /// abort witness generation with a panic on an invalid recursive proof).
    /// The default panic hook is silenced for the duration to avoid noisy output.
    fn run_prove_no_panic(f: impl FnOnce() -> bool) -> bool {
        let prev = std::panic::take_hook();
        std::panic::set_hook(Box::new(|_| {}));
        let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(f));
        std::panic::set_hook(prev);
        result.unwrap_or(false)
    }

    fn chained_batch(block_number: u64, old_root: u64, new_root: u64) -> Batch<F> {
        Batch::<F> {
            end_block_number: block_number,
            batch_size: 1,
            first_created_at: 100 + block_number as i64,
            last_created_at: 100 + block_number as i64,
            old_state_root: HashOut::from([F::from_canonical_u64(old_root); 4]),
            new_state_root: HashOut::from([F::from_canonical_u64(new_root); 4]),
            ..Batch::<F>::default()
        }
    }

    #[test]
    fn test_binary_tree_chain_define_registers_public_inputs() {
        let leaf = build_leaf();
        let circuit = BinaryTreeChainCircuit::define(CIRCUIT_CONFIG, &leaf.data);
        assert!(
            circuit.builder.num_public_inputs() > 0,
            "tree node must expose aggregated public inputs"
        );
    }

    #[test]
    fn test_binary_tree_chain_positive() {
        let leaf = build_leaf();

        let left = prove_leaf(&leaf, &chained_batch(1, 10, 20));
        let right = prove_leaf(&leaf, &chained_batch(2, 20, 30));

        let circuit = BinaryTreeChainCircuit::define(CIRCUIT_CONFIG, &leaf.data);
        let target = circuit.target;
        let data = circuit.builder.build::<C>();

        let proof = BinaryTreeChainCircuit::prove(&target, &data, &left, &right).unwrap();

        let parent_batch =
            Batch::<F>::from_public_inputs(&proof.public_inputs[..BATCH_TARGET_INDEX]);
        assert_eq!(
            parent_batch.old_state_root,
            HashOut::from([F::from_canonical_u64(10); 4])
        );
        assert_eq!(
            parent_batch.new_state_root,
            HashOut::from([F::from_canonical_u64(30); 4])
        );
        assert_eq!(parent_batch.batch_size, 2);
    }

    #[test]
    fn test_binary_tree_chain_negative_mismatched_state_roots() {
        let leaf = build_leaf();

        // left: 10->20, right: 99->30 — adjacency 20 != 99 must be rejected.
        let left = prove_leaf(&leaf, &chained_batch(1, 10, 20));
        let right = prove_leaf(&leaf, &chained_batch(2, 99, 30));

        let circuit = BinaryTreeChainCircuit::define(CIRCUIT_CONFIG, &leaf.data);
        let target = circuit.target;
        let data = circuit.builder.build::<C>();

        let result = BinaryTreeChainCircuit::prove(&target, &data, &left, &right);
        assert!(
            result.is_err(),
            "mismatched adjacent state roots must fail proving (continuity guard)"
        );
    }

    #[test]
    fn test_binary_tree_chain_negative_wrong_vk() {
        let leaf_a = build_leaf();
        let leaf_b = build_leaf_variant();

        let left = prove_leaf(&leaf_a, &chained_batch(1, 10, 20));
        // Proof produced under leaf_b's VK, not leaf_a's.
        let right = prove_leaf(&leaf_b, &chained_batch(2, 20, 30));

        let circuit = BinaryTreeChainCircuit::define(CIRCUIT_CONFIG, &leaf_a.data);
        let target = circuit.target;
        let data = circuit.builder.build::<C>();

        // Proving against a child produced under a different VK must NOT yield a
        // valid proof: Plonky2 either returns Err or aborts witness generation
        // with a panic. Both outcomes prove the `constant_verifier_data` pinning
        // is enforced; a clean Ok would be the failure mode.
        let succeeded = run_prove_no_panic(|| {
            BinaryTreeChainCircuit::prove(&target, &data, &left, &right).is_ok()
        });
        assert!(
            !succeeded,
            "child proof under a different VK must fail (VK-pinning guard)"
        );
    }
}
