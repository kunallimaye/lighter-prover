// Copyright (c) Elliot Technologies, Inc.
// SPDX-License-Identifier: BUSL-1.1

//! Issue #67: L2 tree-fold chain-merge circuit (shape b2 from the #59
//! feasibility spike, gate budget validated by the #64 probe).
//!
//! Verifies TWO chain proofs of adjacent chunk ranges and produces a chain
//! proof of the combined range, exposing the exact same public-input surface
//! as [`crate::block_tx_chain_constraints::BlockTxChainCircuit`] (the LEAF
//! circuit). This enables log-depth folding of a block's chunk proofs:
//! today's serial fold proves N chain steps back-to-back (~0.5 s each); the
//! tree fold proves N leaf proofs (parallelizable) and ~log2(N) levels of
//! merges on the critical path.
//!
//! VK selection (variant "per-child conditional VK"): each child is either a
//! LEAF chain proof (verified against the leaf circuit's constant VK) or a
//! merge proof of THIS circuit's own shape (verified cyclically against the
//! verifier data carried in the public inputs), selected per child by a
//! witness boolean. See
//! [`crate::builder::Builder::verify_leaf_or_cyclic_proof`] for why the
//! two-circuit alternative (separate `merge_leaves`/`merge_nodes`) does not
//! compose: plonky2's cyclic machinery forces every proof in a cycle to
//! share one VK, so a `merge_nodes` circuit could never consume
//! `merge_leaves` proofs.
//!
//! Because both children are always REAL proofs (leaf or merge in any mix --
//! odd counts at any tree level are handled by carrying the odd proof up a
//! level), no dummy proofs or dummy circuits are needed anywhere, which also
//! sidesteps the fork's `dummy_circuit()` ConstantGate shape limitation
//! documented in #64.
//!
//! Shape: the merge circuit reuses the leaf chain circuit's already-closed
//! `CommonCircuitData` (the 2^14 cyclic fixed point, 1,613 PIs including the
//! #67 range-start delta root) as its self-referential shape. Callers MUST
//! assert `merge.common == leaf.common` after building -- see
//! [`BlockTxChainMergeCircuit::define`].

use anyhow::{Ok, Result};
use log::Level;
use plonky2::field::extension::Extendable;
use plonky2::hash::hash_types::{HashOutTarget, RichField};
use plonky2::iop::target::BoolTarget;
use plonky2::iop::witness::{PartialWitness, WitnessWrite};
use plonky2::plonk::circuit_data::{CircuitConfig, CircuitData, VerifierCircuitTarget};
use plonky2::plonk::config::GenericConfig;
use plonky2::plonk::proof::{ProofWithPublicInputs, ProofWithPublicInputsTarget};
use plonky2::timed;
use plonky2::util::timing::TimingTree;

use crate::block_tx_chain::BlockTxChainWitnessTarget;
use crate::block_tx_chain_constraints::select_on_chain_pub_data;
use crate::types::change_pub_key::ChangePubKeyMessageTarget;
use crate::types::config::{Builder, C, D, F};
use crate::types::state_metadata::{
    STATE_METADATA_SIZE, StateMetadataTarget, connect_state_metadata_target,
};
use crate::types::transfer::TransferMessageTarget;
use crate::utils::CircuitBuilderUtils;

pub trait Circuit<C: GenericConfig<D, F = F>, F: RichField + Extendable<D>, const D: usize> {
    /// Defines the merge circuit against the LEAF chain circuit's data. The
    /// leaf circuit's `CommonCircuitData` (an already-closed cyclic fixed
    /// point) is reused as this circuit's self-referential shape, and the
    /// leaf's verifier key is embedded as the constant non-cyclic VK option.
    fn define(
        config: CircuitConfig,
        leaf_chain_circuit: &CircuitData<F, C, D>,
        on_chain_operations_limit: usize,
    ) -> Self;

    /// Fills the partial witness: the two child proofs (covering adjacent
    /// chunk ranges, left before right) and, per child, whether it is a
    /// merge proof (`true`) or a leaf chain proof (`false`).
    fn generate_witness(
        target: &BlockTxChainMergeTarget,
        circuit_data: &CircuitData<F, C, D>,
        left_proof: &ProofWithPublicInputs<F, C, D>,
        left_is_merge: bool,
        right_proof: &ProofWithPublicInputs<F, C, D>,
        right_is_merge: bool,
    ) -> Result<PartialWitness<F>>;

    fn prove(
        target: &BlockTxChainMergeTarget,
        circuit_data: &CircuitData<F, C, D>,
        left_proof: &ProofWithPublicInputs<F, C, D>,
        left_is_merge: bool,
        right_proof: &ProofWithPublicInputs<F, C, D>,
        right_is_merge: bool,
    ) -> Result<ProofWithPublicInputs<F, C, D>>;
}

#[derive(Debug)]
pub struct BlockTxChainMergeCircuit {
    pub builder: Builder,
    pub target: BlockTxChainMergeTarget,
    /// Size of the chain-witness PI section (same as the leaf circuit's
    /// `block_tx_witness_size`); the state metadata, range-start delta root
    /// and verifier data follow it.
    pub block_tx_witness_size: usize,
}

#[derive(Debug)]
pub struct BlockTxChainMergeTarget {
    pub left_proof: ProofWithPublicInputsTarget<D>, // chain proof of the left (earlier) range
    pub right_proof: ProofWithPublicInputsTarget<D>, // chain proof of the right (later) range

    pub left_is_merge: BoolTarget, // true: left child is a merge proof; false: a leaf chain proof
    pub right_is_merge: BoolTarget, // ditto for the right child

    pub self_verifier_data: VerifierCircuitTarget, // Verifier data for this circuit (cyclic)

    pub new_block: BlockTxChainWitnessTarget, // Public witness - combined range
    pub state_metadata_target: StateMetadataTarget, // Public witness - same for every proof of a block
    // Issue #67: range-start `old_account_delta_tree_root` of the combined
    // range (= the left child's range start). Same PI position as in the
    // leaf circuit -- see `BlockTxChainTarget`.
    pub old_account_delta_tree_root_range_start: HashOutTarget, // Public witness
}

impl Circuit<C, F, D> for BlockTxChainMergeCircuit {
    fn define(
        config: CircuitConfig,
        leaf_chain_circuit: &CircuitData<F, C, D>,
        on_chain_operations_limit: usize,
    ) -> Self {
        // The slot-shift network below inserts at most ONE right-child
        // on-chain op into the merged slots; a general merge needs an
        // O(limit^2) select network. The bench/production configuration is
        // limit = 1 (see ON_CHAIN_OPERATIONS_LIMIT usage in bench).
        assert_eq!(
            on_chain_operations_limit, 1,
            "BlockTxChainMergeCircuit currently supports on_chain_operations_limit = 1 only (issue #67)"
        );

        let mut builder = Builder::new(config);

        // Public inputs: identical sequence (and therefore identical layout)
        // to BlockTxChainCircuit::new -- chain witness, state metadata,
        // range-start delta root (#67), then the verifier-data PIs last.
        let new_block =
            BlockTxChainWitnessTarget::new_public(&mut builder, on_chain_operations_limit);
        let state_metadata_target = StateMetadataTarget::new_public(&mut builder);
        let old_account_delta_tree_root_range_start = builder.add_virtual_hash_public_input();
        let self_verifier_data = builder.add_verifier_data_public_inputs();

        // Self-referential shape: the leaf chain circuit's common data IS the
        // closed 2^14 cyclic fixed point (it builds with goal_common_data
        // match asserted), so reusing it both guarantees leaf proofs fit the
        // child-proof targets and defines the shape this circuit must build
        // to. The build-time equality `merge.common == leaf.common` is
        // asserted by callers (our custom verify helper cannot set the
        // fork's pub(crate) goal_common_data).
        let self_common = leaf_chain_circuit.common.clone();
        assert_eq!(
            builder.num_public_inputs(),
            self_common.num_public_inputs,
            "merge circuit PI surface must match the leaf chain circuit's"
        );

        let left_is_merge = builder.add_virtual_bool_target_safe();
        let right_is_merge = builder.add_virtual_bool_target_safe();
        let left_proof = builder.add_virtual_proof_with_pis(&self_common);
        let right_proof = builder.add_virtual_proof_with_pis(&self_common);

        // Per-child conditional-VK verification (leaf constant VK vs own
        // cyclic VK) -- see the module docs and the helper's docs.
        let leaf_verifier_data = builder.constant_verifier_data(&leaf_chain_circuit.verifier_only);
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

        // ---- Parse both children's public inputs.
        let (left, block_pis_size) = BlockTxChainWitnessTarget::from_public_inputs(
            &left_proof.public_inputs,
            on_chain_operations_limit,
            1,
        );
        let (right, _) = BlockTxChainWitnessTarget::from_public_inputs(
            &right_proof.public_inputs,
            on_chain_operations_limit,
            1,
        );

        let left_meta = StateMetadataTarget {
            last_funding_round_timestamp: left_proof.public_inputs[block_pis_size],
            last_oracle_price_timestamp: left_proof.public_inputs[block_pis_size + 1],
            last_premium_timestamp: left_proof.public_inputs[block_pis_size + 2],
        };
        let right_meta = StateMetadataTarget {
            last_funding_round_timestamp: right_proof.public_inputs[block_pis_size],
            last_oracle_price_timestamp: right_proof.public_inputs[block_pis_size + 1],
            last_premium_timestamp: right_proof.public_inputs[block_pis_size + 2],
        };

        // Issue #67 range-start delta roots (PI layout documented on
        // `BlockTxChainTarget`).
        let left_range_start = HashOutTarget::try_from(
            &left_proof.public_inputs
                [block_pis_size + STATE_METADATA_SIZE..block_pis_size + STATE_METADATA_SIZE + 4],
        )
        .unwrap();
        let right_range_start = HashOutTarget::try_from(
            &right_proof.public_inputs
                [block_pis_size + STATE_METADATA_SIZE..block_pis_size + STATE_METADATA_SIZE + 4],
        )
        .unwrap();

        // ---- Range continuity.
        //
        // State-root continuity: the right child's whole-range start equals
        // the left child's end. This subsumes validium-root continuity:
        // state_root = H(account_pub_data_tree_root, public_market_details,
        // validium_root) is recomputed in-circuit by the right child's
        // first leaf step from its full pre-state, so pinning the state root
        // pins the validium root (the chain PI surface intentionally has no
        // old_validium_root; the #64 probe's left/right new_validium connect
        // was a gate-count stand-in only and is NOT a real constraint).
        builder.connect_hashes(left.new_state_root, right.old_state_root);
        // Account-delta continuity through the #67 range-start PI: the delta
        // tree is committed in neither validium_root nor state_root, so it
        // needs its own link.
        builder.connect_hashes(left.new_account_delta_tree_root, right_range_start);
        // Both children fold the same block.
        builder.connect(left.block_number, right.block_number);
        builder.connect(left.created_at, right.created_at);

        // State metadata never changes within a block: equal across children
        // and exposed unchanged.
        connect_state_metadata_target(&mut builder, &left_meta, &right_meta);
        connect_state_metadata_target(&mut builder, &right_meta, &state_metadata_target);

        // ---- "At most one per block" fields: exclusive-select + not-both,
        // mirroring the per-step pattern in block_tx_chain_constraints.
        let right_has_change_pub_key =
            builder.is_not_zero(right.change_pub_key_message.account_index);
        builder.conditional_assert_zero(
            right_has_change_pub_key,
            left.change_pub_key_message.account_index,
        );
        let merged_change_pub_key = ChangePubKeyMessageTarget::select(
            &mut builder,
            right_has_change_pub_key,
            &right.change_pub_key_message,
            &left.change_pub_key_message,
        );

        let right_has_transfer = builder.is_not_zero(right.transfer_message.from_account_index);
        builder
            .conditional_assert_zero(right_has_transfer, left.transfer_message.from_account_index);
        let merged_transfer = TransferMessageTarget::select(
            &mut builder,
            right_has_transfer,
            &right.transfer_message,
            &left.transfer_message,
        );

        let right_has_priority = builder.is_not_zero(right.priority_operations_count);
        let merged_priority_pub_data = builder.select_arr_u8(
            right_has_priority,
            &right.priority_operations_pub_data,
            &left.priority_operations_pub_data,
        );
        builder.conditional_assert_zero(right_has_priority, left.priority_operations_count);
        let merged_priority_count = builder.add(
            left.priority_operations_count,
            right.priority_operations_count,
        );
        builder.assert_bool(BoolTarget::new_unsafe(merged_priority_count));

        // ---- On-chain ops: shift the right child's slots by left.count
        // (trivial at limit = 1: at most one slot exists block-wide).
        let right_has_on_chain = builder.is_not_zero(right.on_chain_operations_count);
        let mut merged_on_chain_pub_data = left.on_chain_operations_pub_data.clone();
        select_on_chain_pub_data(
            &mut builder,
            on_chain_operations_limit,
            left.on_chain_operations_count,
            &mut merged_on_chain_pub_data,
            &right.on_chain_operations_pub_data[0],
            right_has_on_chain,
        );
        let limit_t = builder.constant_usize(on_chain_operations_limit);
        builder.conditional_assert_not_eq(
            right_has_on_chain,
            left.on_chain_operations_count,
            limit_t,
        );
        let merged_on_chain_count = builder.add(
            left.on_chain_operations_count,
            right.on_chain_operations_count,
        );
        builder.assert_bool(BoolTarget::new_unsafe(merged_on_chain_count));

        // ---- Outputs: old_*/range-start from the left child, new_* and
        // market details from the right child, counts summed.
        let calculated_new_block = BlockTxChainWitnessTarget {
            block_number: left.block_number,
            created_at: left.created_at,
            old_state_root: left.old_state_root,

            new_validium_root: right.new_validium_root,
            new_state_root: right.new_state_root,
            new_account_delta_tree_root: right.new_account_delta_tree_root,

            change_pub_key_message: merged_change_pub_key,
            transfer_message: merged_transfer,

            on_chain_operations_count: merged_on_chain_count,
            on_chain_operations_pub_data: merged_on_chain_pub_data,

            priority_operations_count: merged_priority_count,
            priority_operations_pub_data: merged_priority_pub_data,

            new_public_market_details: right.new_public_market_details,
        };
        // (connect_block_witness also connects new_public_market_details --
        // the "take right" rule -- via connect_public_market_details.)
        new_block.connect_block_witness(&mut builder, &calculated_new_block);

        builder.connect_hashes(old_account_delta_tree_root_range_start, left_range_start);

        builder.perform_registered_range_checks();

        // Pad to the self-shape's pre-build row budget, mirroring
        // `common_data_for_recursion`: hashing the ~1.6k PIs at build time
        // then spills past 8,192 rows and the circuit blinds/pads to 2^14 --
        // the leaf chain circuit's degree (#64 calibration).
        while builder.num_gates() < 1 << 13 {
            builder.add_gate(plonky2::gates::noop::NoopGate, vec![]);
        }

        Self {
            target: BlockTxChainMergeTarget {
                left_proof,
                right_proof,
                left_is_merge,
                right_is_merge,
                self_verifier_data,
                new_block,
                state_metadata_target,
                old_account_delta_tree_root_range_start,
            },
            builder,
            block_tx_witness_size: block_pis_size,
        }
    }

    fn generate_witness(
        target: &BlockTxChainMergeTarget,
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

        // All public-input targets (new_block, state metadata, range-start
        // delta root) receive their values through copy constraints from the
        // child proofs' public inputs.
        Ok(pw)
    }

    fn prove(
        target: &BlockTxChainMergeTarget,
        circuit_data: &CircuitData<F, C, D>,
        left_proof: &ProofWithPublicInputs<F, C, D>,
        left_is_merge: bool,
        right_proof: &ProofWithPublicInputs<F, C, D>,
        right_is_merge: bool,
    ) -> Result<ProofWithPublicInputs<F, C, D>> {
        let mut timing = TimingTree::new("BlockTxChainMergeCircuit", Level::Debug);

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
