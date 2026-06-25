// Copyright (c) Elliot Technologies, Inc.
// SPDX-License-Identifier: BUSL-1.1

use anyhow::{Ok, Result};
use hashbrown::HashMap;
use log::Level;
use plonky2::iop::target::BoolTarget;
use plonky2::iop::witness::{PartialWitness, WitnessWrite};
use plonky2::plonk::circuit_data::{CircuitConfig, CircuitData};
use plonky2::plonk::proof::{ProofWithPublicInputs, ProofWithPublicInputsTarget};
use plonky2::recursion::dummy_circuit::dummy_proof;
use plonky2::timed;
use plonky2::util::timing::TimingTree;

use crate::binary_tree_chain_constraints::fold_consecutive;
use crate::recursion::batch::{BATCH_TARGET_INDEX, Batch, BatchTarget, BatchTargetWitness};
use crate::types::config::{Builder, C, D, F};

/// Number of children aggregated by a single radix-16 reduction-tree node.
pub const RADIX: usize = 16;

pub struct HexadecimalTreeChainTarget<const D: usize> {
    pub children: [ProofWithPublicInputsTarget<D>; RADIX],
    /// One flag per child slot indicating whether the slot holds a real child
    /// proof (`true`) or padding/dummy proof (`false`). Real slots are folded
    /// into the aggregate; padding slots fold with `cond = false` so they never
    /// corrupt the state-root continuity chain.
    pub is_real_child: [BoolTarget; RADIX],
    /// Aggregated batch registered as this node's public inputs.
    pub aggregated_batch: BatchTarget,
}

pub struct HexadecimalTreeChainCircuit {
    pub builder: Builder,
    pub target: HexadecimalTreeChainTarget<D>,
}

impl HexadecimalTreeChainCircuit {
    /// Builds a radix-16 reduction-tree aggregation node.
    ///
    /// The node verifies each of its 16 child proofs against the *pinned* child
    /// verifying key (`constant_verifier_data`), extracts each child's
    /// [`BatchTarget`]-shaped public inputs, folds them left-to-right enforcing
    /// state-root / block-number / timestamp / delta-root continuity between
    /// adjacent children, and registers the folded aggregate as this node's own
    /// public inputs. This makes the node a 16-ary analogue of the binary
    /// `BatchTarget::conditionally_merge_consecutive` fold used by the
    /// production recursion circuits.
    pub fn define(config: CircuitConfig, child_circuit: &CircuitData<F, C, D>) -> Self {
        let mut builder = Builder::new(config);

        // Register the aggregated batch as this node's public inputs up front,
        // mirroring `CyclicRecursionCircuit::new`. We connect the folded
        // accumulator to this public target after folding all children.
        let aggregated_batch = BatchTarget::new_public(&mut builder);

        // Pin the child verifying key. Using a constant (rather than a virtual
        // verifier-data witness) binds this node to exactly one child circuit so
        // an attacker cannot substitute a proof produced under a different VK.
        let child_verifier_data = builder.constant_verifier_data(&child_circuit.verifier_only);
        let child_common_data = &child_circuit.common;

        // Per-slot "is this a real child?" flags. Driven from witness in `prove`.
        let is_real_child: [BoolTarget; RADIX] =
            core::array::from_fn(|_| builder.add_virtual_bool_target_safe());

        // Verify each child proof and extract its BatchTarget public inputs.
        let mut child_batches: Vec<BatchTarget> = Vec::with_capacity(RADIX);
        let children: [ProofWithPublicInputsTarget<D>; RADIX] = core::array::from_fn(|_| {
            let child = builder.add_virtual_proof_with_pis(child_common_data);
            builder.verify_proof::<C>(&child, &child_verifier_data, child_common_data);
            child_batches.push(BatchTarget::from_public_inputs(
                &child.public_inputs[..BATCH_TARGET_INDEX],
            ));
            child
        });

        // Fold children left-to-right. The accumulator starts as the first
        // child's batch; each subsequent child is merged with
        // `conditionally_merge_consecutive`, which enforces
        // `a.new_state_root == b.old_state_root` (plus block-number, timestamp
        // and delta-root continuity) whenever `cond` is true. Padding slots use
        // `cond = is_real_child[i] = false`, so they are skipped by the fold.
        let mut acc = child_batches[0].clone();
        for i in 1..RADIX {
            acc = BatchTarget::conditionally_merge_consecutive(
                &mut builder,
                is_real_child[i],
                &acc,
                &child_batches[i],
            );
        }

        // Register the folded aggregate as this node's public inputs. Without
        // this connect the node would prove nothing about the aggregated state.
        aggregated_batch.connect_batches(&mut builder, &acc);

        builder.perform_registered_range_checks();

        Self {
            builder,
            target: HexadecimalTreeChainTarget {
                children,
                is_real_child,
                aggregated_batch,
            },
        }
    }

    pub fn prove(
        target: &HexadecimalTreeChainTarget<D>,
        circuit_data: &CircuitData<F, C, D>,
        child_proofs: &[ProofWithPublicInputs<F, C, D>],
        child_circuit_data: &CircuitData<F, C, D>,
    ) -> Result<ProofWithPublicInputs<F, C, D>> {
        assert!(
            child_proofs.len() <= RADIX,
            "too many child proofs for a radix-{RADIX} node"
        );

        let mut pw = PartialWitness::new();
        // Dummy proof of the CHILD circuit used to pad empty slots. This is a
        // legitimate Plonky2 primitive; padding slots fold with cond=false.
        let dummy = dummy_proof::<F, C, D>(child_circuit_data, HashMap::new())?;
        for i in 0..RADIX {
            let is_real = i < child_proofs.len();
            let proof = if is_real { &child_proofs[i] } else { &dummy };
            pw.set_proof_with_pis_target(&target.children[i], proof)?;
            pw.set_bool_target(target.is_real_child[i], is_real)?;
        }

        // Compute the host-side fold so the public-output BatchTarget (which is
        // `connect`ed, not generated, in-circuit) can be witnessed — mirroring
        // `CyclicRecursionCircuit::generate_witness::set_batch_target`.
        // Slot 0 always seeds the accumulator (is_real_child[0] is not used as a
        // fold condition, matching the in-circuit loop that starts at i=1).
        let child_batches: Vec<Batch<F>> = (0..RADIX)
            .map(|i| {
                let proof = if i < child_proofs.len() {
                    &child_proofs[i]
                } else {
                    &dummy
                };
                Batch::<F>::from_public_inputs(&proof.public_inputs[..BATCH_TARGET_INDEX])
            })
            .collect();
        let mut acc = child_batches[0].clone();
        for i in 1..RADIX {
            let cond = i < child_proofs.len();
            acc = fold_consecutive(&acc, &child_batches[i], cond);
        }
        pw.set_batch_target(&target.aggregated_batch, &acc)?;

        let mut timing = TimingTree::new("Hexadecimal tree recursive prove", Level::Debug);
        let proof = timed!(timing, "prove", circuit_data.prove(pw))?;
        timed!(timing, "verify", circuit_data.verify(proof.clone())?);
        timing.print();
        Ok(proof)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::config::CIRCUIT_CONFIG;
    use plonky2::field::types::Field;
    use plonky2::hash::hash_types::HashOut;

    /// A tiny leaf circuit that emits `BatchTarget`-shaped public inputs.
    /// Used by the tree tests to produce children with controllable
    /// (and chainable) state roots.
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

    /// Prove a leaf with the given batch as its public output.
    fn prove_leaf(leaf: &LeafCircuit, batch: &Batch<F>) -> ProofWithPublicInputs<F, C, D> {
        let mut pw = PartialWitness::new();
        pw.set_batch_target(&leaf.batch_target, batch).unwrap();
        leaf.data.prove(pw).unwrap()
    }

    /// Runs a proving closure, returning whether it produced a valid proof.
    /// Returns `false` if the closure returned `false` OR panicked (Plonky2 may
    /// abort witness generation with a panic on an invalid recursive proof).
    fn run_prove_no_panic(f: impl FnOnce() -> bool) -> bool {
        let prev = std::panic::take_hook();
        std::panic::set_hook(Box::new(|_| {}));
        let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(f));
        std::panic::set_hook(prev);
        result.unwrap_or(false)
    }

    /// Build a `Batch` whose old/new state roots can be chained.
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
    fn test_hexadecimal_tree_chain_define_registers_public_inputs() {
        let leaf = build_leaf();
        let hex_circuit = HexadecimalTreeChainCircuit::define(CIRCUIT_CONFIG, &leaf.data);
        assert_eq!(hex_circuit.target.children.len(), RADIX);
        // The defect was that the circuit registered NO public inputs. Guard it.
        assert!(
            hex_circuit.builder.num_public_inputs() > 0,
            "tree node must expose aggregated public inputs"
        );
    }

    #[test]
    fn test_hexadecimal_tree_chain_positive() {
        let leaf = build_leaf();

        // Two real children with chained state roots: child0: 10->20, child1: 20->30.
        let child0 = prove_leaf(&leaf, &chained_batch(1, 10, 20));
        let child1 = prove_leaf(&leaf, &chained_batch(2, 20, 30));
        let child_proofs = vec![child0, child1];

        let hex_circuit = HexadecimalTreeChainCircuit::define(CIRCUIT_CONFIG, &leaf.data);
        let target = hex_circuit.target;
        let data = hex_circuit.builder.build::<C>();

        let proof =
            HexadecimalTreeChainCircuit::prove(&target, &data, &child_proofs, &leaf.data).unwrap();

        // Parent's aggregated batch should span child0.old -> child1.new.
        let parent_batch = Batch::<F>::from_public_inputs(&proof.public_inputs[..BATCH_TARGET_INDEX]);
        assert_eq!(
            parent_batch.old_state_root,
            HashOut::from([F::from_canonical_u64(10); 4])
        );
        assert_eq!(
            parent_batch.new_state_root,
            HashOut::from([F::from_canonical_u64(30); 4])
        );
        assert_eq!(parent_batch.batch_size, 2);
        // verify() is already called inside prove(); a successful return is the assertion.
    }

    #[test]
    fn test_hexadecimal_tree_chain_negative_mismatched_state_roots() {
        let leaf = build_leaf();

        // child0: 10->20, child1: 99->30 — adjacency 20 != 99 must be rejected
        // by `conditionally_merge_consecutive`'s state-root continuity assert.
        let child0 = prove_leaf(&leaf, &chained_batch(1, 10, 20));
        let child1 = prove_leaf(&leaf, &chained_batch(2, 99, 30));
        let child_proofs = vec![child0, child1];

        let hex_circuit = HexadecimalTreeChainCircuit::define(CIRCUIT_CONFIG, &leaf.data);
        let target = hex_circuit.target;
        let data = hex_circuit.builder.build::<C>();

        let result =
            HexadecimalTreeChainCircuit::prove(&target, &data, &child_proofs, &leaf.data);
        assert!(
            result.is_err(),
            "mismatched adjacent state roots must fail proving (continuity guard)"
        );
    }

    #[test]
    fn test_hexadecimal_tree_chain_negative_wrong_vk() {
        // The tree node pins `leaf_a`'s VK. A proof produced by a DIFFERENT
        // circuit (`leaf_b`) — even though it is BatchTarget-shaped — must be
        // rejected because `constant_verifier_data` binds the child VK.
        let leaf_a = build_leaf();
        let leaf_b = build_leaf_variant();

        let child0 = prove_leaf(&leaf_a, &chained_batch(1, 10, 20));
        // Produced under leaf_b's VK, not leaf_a's.
        let child1 = prove_leaf(&leaf_b, &chained_batch(2, 20, 30));
        let child_proofs = vec![child0, child1];

        let hex_circuit = HexadecimalTreeChainCircuit::define(CIRCUIT_CONFIG, &leaf_a.data);
        let target = hex_circuit.target;
        let data = hex_circuit.builder.build::<C>();

        // Proving against a child produced under a different VK must NOT yield a
        // valid proof: Plonky2 either returns Err or aborts witness generation
        // with a panic. Both prove the `constant_verifier_data` pinning holds.
        let succeeded = run_prove_no_panic(|| {
            HexadecimalTreeChainCircuit::prove(&target, &data, &child_proofs, &leaf_a.data).is_ok()
        });
        assert!(
            !succeeded,
            "child proof under a different VK must fail (VK-pinning guard)"
        );
    }

    /// A leaf circuit with the SAME public-input layout but a different
    /// constraint set so it builds to a distinct verifying key.
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
}
