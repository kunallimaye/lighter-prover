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

    /// Aggregates `child_proofs` (up to [`RADIX`]) into a single parent proof.
    ///
    /// Empty slots are padded so the parent always verifies exactly [`RADIX`]
    /// children. The padding strategy depends on whether the child circuit is
    /// **recursive** (its own circuit contains an inner `verify_proof`, e.g. a
    /// level-1 tree node) or a **non-recursive leaf**:
    ///
    /// - **Non-recursive leaf child** (`padding_proof == None`): we synthesise
    ///   the pad with [`dummy_proof`]. This is sound because the leaf circuit
    ///   has no recursive verifier whose witness generators would be left
    ///   unrun. This is the original level-1 behaviour and must not regress.
    ///
    /// - **Recursive child** (`padding_proof == Some(p)`): `dummy_proof` cannot
    ///   synthesise a witness for the child's inner `verify_proof` (it would
    ///   fail with `"generators weren't run"`), so the caller must supply a
    ///   **real, satisfiable base proof** `p` of the actual child circuit,
    ///   produced under the *pinned* child VK. The pad's public inputs are
    ///   irrelevant to the aggregate (its slot folds with `cond = false`), but
    ///   the proof MUST verify against the pinned child VK because the
    ///   in-circuit `verify_proof` runs unconditionally for every slot.
    ///
    /// The caller mints `padding_proof` recursively: a level-2 pad is a real
    /// level-1 node proof (which itself pads with real leaf proofs), bottoming
    /// out at the non-recursive leaf where `dummy_proof` works.
    pub fn prove(
        target: &HexadecimalTreeChainTarget<D>,
        circuit_data: &CircuitData<F, C, D>,
        child_proofs: &[ProofWithPublicInputs<F, C, D>],
        child_circuit_data: &CircuitData<F, C, D>,
        padding_proof: Option<&ProofWithPublicInputs<F, C, D>>,
    ) -> Result<ProofWithPublicInputs<F, C, D>> {
        assert!(
            child_proofs.len() <= RADIX,
            "too many child proofs for a radix-{RADIX} node"
        );

        let mut pw = PartialWitness::new();
        // Proof used to pad empty slots. For a non-recursive leaf child a
        // `dummy_proof` suffices; for a recursive child the caller must supply a
        // real base proof (see the doc comment). Padding slots fold with
        // `cond = false`, so the pad's public inputs never reach the aggregate.
        let owned_dummy: Option<ProofWithPublicInputs<F, C, D>> = match padding_proof {
            Some(_) => None,
            None => Some(dummy_proof::<F, C, D>(child_circuit_data, HashMap::new())?),
        };
        let pad: &ProofWithPublicInputs<F, C, D> = match padding_proof {
            Some(p) => p,
            None => owned_dummy.as_ref().expect("dummy padding proof minted"),
        };
        for i in 0..RADIX {
            let is_real = i < child_proofs.len();
            let proof = if is_real { &child_proofs[i] } else { pad };
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
                    pad
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
            HexadecimalTreeChainCircuit::prove(&target, &data, &child_proofs, &leaf.data, None)
                .unwrap();

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
            HexadecimalTreeChainCircuit::prove(&target, &data, &child_proofs, &leaf.data, None);
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
            HexadecimalTreeChainCircuit::prove(&target, &data, &child_proofs, &leaf_a.data, None)
                .is_ok()
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

    // ─────────────────────────────────────────────────────────────────────
    // Level-2 ("node of nodes") multi-level composition.
    //
    // A level-1 node is a `HexadecimalTreeChainCircuit` over the non-recursive
    // `LeafCircuit`. A level-2 node is a `HexadecimalTreeChainCircuit` over the
    // *level-1 node's* circuit — i.e. a recursive child. Because the level-1
    // child contains an inner `verify_proof`, `dummy_proof` cannot synthesise a
    // witness for an empty slot ("generators weren't run"); the level-2 prove
    // must pad with a REAL level-1 base proof instead. These tests exercise that
    // path end-to-end: build, prove, independently verify, correct span,
    // padding, and continuity soundness.
    // ─────────────────────────────────────────────────────────────────────

    /// A built level-1 reduction-tree node: its target + circuit data, plus the
    /// leaf circuit data its children are pinned to.
    struct Level1Node {
        target: HexadecimalTreeChainTarget<D>,
        data: CircuitData<F, C, D>,
        leaf: LeafCircuit,
    }

    /// Build a level-1 node over the shared `LeafCircuit`.
    fn build_level1_node() -> Level1Node {
        let leaf = build_leaf();
        let circuit = HexadecimalTreeChainCircuit::define(CIRCUIT_CONFIG, &leaf.data);
        let target = circuit.target;
        let data = circuit.builder.build::<C>();
        Level1Node { target, data, leaf }
    }

    /// Prove a level-1 node folding `leaf_batches` (real leaf children). Empty
    /// slots are padded with `dummy_proof` (the leaf child is non-recursive).
    fn prove_level1(node: &Level1Node, leaf_batches: &[Batch<F>]) -> ProofWithPublicInputs<F, C, D> {
        let child_proofs: Vec<ProofWithPublicInputs<F, C, D>> = leaf_batches
            .iter()
            .map(|b| prove_leaf(&node.leaf, b))
            .collect();
        HexadecimalTreeChainCircuit::prove(
            &node.target,
            &node.data,
            &child_proofs,
            &node.leaf.data,
            None,
        )
        .unwrap()
    }

    /// Mint a real level-1 base proof suitable as padding for a level-2 node:
    /// a single trivial leaf child, remaining slots `dummy_proof`-padded. It is
    /// produced under the level-1 VK so it verifies against the pinned child VK
    /// in the level-2 circuit; its public inputs are irrelevant (padding folds
    /// with `cond = false`).
    fn mint_level1_base(node: &Level1Node) -> ProofWithPublicInputs<F, C, D> {
        prove_level1(node, &[chained_batch(1, 1, 2)])
    }

    #[test]
    fn test_hexadecimal_tree_level2_define_builds_over_recursive_child() {
        // A level-2 node must `define` over the level-1 node's CircuitData (a
        // recursive child) and expose aggregated public inputs.
        let node1 = build_level1_node();
        let level2 = HexadecimalTreeChainCircuit::define(CIRCUIT_CONFIG, &node1.data);
        assert_eq!(level2.target.children.len(), RADIX);
        assert!(
            level2.builder.num_public_inputs() > 0,
            "level-2 node must expose aggregated public inputs"
        );
    }

    #[test]
    fn test_hexadecimal_tree_level2_positive_compose_verify_and_span() {
        // Two level-1 children with chaining spans:
        //   node A folds leaves 10->20, 20->30  => span 10->30, batch_size 2
        //   node B folds leaves 30->40, 40->50  => span 30->50, batch_size 2
        // A.new (30) == B.old (30), so the level-2 fold chains: 10->50, size 4.
        let node1 = build_level1_node();
        let child_a = prove_level1(
            &node1,
            &[chained_batch(1, 10, 20), chained_batch(2, 20, 30)],
        );
        let child_b = prove_level1(
            &node1,
            &[chained_batch(3, 30, 40), chained_batch(4, 40, 50)],
        );

        // Build the level-2 node over the recursive level-1 child.
        let level2 = HexadecimalTreeChainCircuit::define(CIRCUIT_CONFIG, &node1.data);
        let target = level2.target;
        let data = level2.builder.build::<C>();

        // Pad empty slots with a REAL level-1 base proof (dummy_proof would fail
        // with "generators weren't run" on the recursive child).
        let pad = mint_level1_base(&node1);
        let proof = HexadecimalTreeChainCircuit::prove(
            &target,
            &data,
            &[child_a, child_b],
            &node1.data,
            Some(&pad),
        )
        .expect("level-2 compose must prove (real-base-proof padding)");

        // Independently verify against the level-2 VK (prove() also verifies,
        // but assert it explicitly per the acceptance criteria).
        data.verify(proof.clone())
            .expect("level-2 proof must verify against the level-2 VK");

        // The aggregated span must cover the full leaf set: 10 -> 50, size 4.
        let root = Batch::<F>::from_public_inputs(&proof.public_inputs[..BATCH_TARGET_INDEX]);
        assert_eq!(
            root.old_state_root,
            HashOut::from([F::from_canonical_u64(10); 4]),
            "level-2 old root must equal the first leaf's old root"
        );
        assert_eq!(
            root.new_state_root,
            HashOut::from([F::from_canonical_u64(50); 4]),
            "level-2 new root must equal the last real leaf's new root"
        );
        assert_eq!(
            root.batch_size, 4,
            "level-2 batch_size must span all four real leaves"
        );
    }

    #[test]
    fn test_hexadecimal_tree_level2_padding_some_real_some_padded() {
        // One real level-1 child (span 10->30, batch_size 2) + 15 padded slots.
        // The single real child seeds the accumulator; padded slots fold with
        // cond=false and are neutralised, so the root equals the lone child.
        let node1 = build_level1_node();
        let child_a = prove_level1(
            &node1,
            &[chained_batch(1, 10, 20), chained_batch(2, 20, 30)],
        );

        let level2 = HexadecimalTreeChainCircuit::define(CIRCUIT_CONFIG, &node1.data);
        let target = level2.target;
        let data = level2.builder.build::<C>();

        let pad = mint_level1_base(&node1);
        let proof = HexadecimalTreeChainCircuit::prove(
            &target,
            &data,
            &[child_a],
            &node1.data,
            Some(&pad),
        )
        .expect("level-2 with padding must prove");

        data.verify(proof.clone())
            .expect("padded level-2 proof must verify");

        let root = Batch::<F>::from_public_inputs(&proof.public_inputs[..BATCH_TARGET_INDEX]);
        assert_eq!(
            root.old_state_root,
            HashOut::from([F::from_canonical_u64(10); 4])
        );
        assert_eq!(
            root.new_state_root,
            HashOut::from([F::from_canonical_u64(30); 4])
        );
        assert_eq!(
            root.batch_size, 2,
            "padded slots must not contribute to batch_size"
        );
    }

    #[test]
    fn test_hexadecimal_tree_level2_negative_nonchaining_children() {
        // node A spans 10->30, node B spans 99->50. A.new (30) != B.old (99),
        // so the level-2 continuity assert must reject the fold.
        let node1 = build_level1_node();
        let child_a = prove_level1(
            &node1,
            &[chained_batch(1, 10, 20), chained_batch(2, 20, 30)],
        );
        let child_b = prove_level1(
            &node1,
            &[chained_batch(3, 99, 40), chained_batch(4, 40, 50)],
        );

        let level2 = HexadecimalTreeChainCircuit::define(CIRCUIT_CONFIG, &node1.data);
        let target = level2.target;
        let data = level2.builder.build::<C>();
        let pad = mint_level1_base(&node1);

        // Non-chaining children must NOT yield a valid proof: Plonky2 either
        // returns Err or aborts witness generation with a panic. Both prove the
        // continuity guard holds at level 2.
        let succeeded = run_prove_no_panic(|| {
            HexadecimalTreeChainCircuit::prove(
                &target,
                &data,
                &[child_a, child_b],
                &node1.data,
                Some(&pad),
            )
            .is_ok()
        });
        assert!(
            !succeeded,
            "non-chaining level-2 children must fail proving (continuity guard)"
        );
    }
}
