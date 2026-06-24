// Copyright (c) Elliot Technologies, Inc.
// SPDX-License-Identifier: BUSL-1.1

use anyhow::{Ok, Result};
use hashbrown::HashMap;
use log::Level;
use plonky2::iop::witness::{PartialWitness, WitnessWrite};
use plonky2::plonk::circuit_data::{
    CircuitConfig, CircuitData, CommonCircuitData, VerifierCircuitTarget,
};
use plonky2::plonk::proof::{ProofWithPublicInputs, ProofWithPublicInputsTarget};
use plonky2::recursion::dummy_circuit::dummy_proof;
use plonky2::timed;
use plonky2::util::timing::TimingTree;

use crate::types::config::{Builder, C, D, F};

pub struct HexadecimalTreeChainTarget<const D: usize> {
    pub children: [ProofWithPublicInputsTarget<D>; 16],
    pub verifier_data: VerifierCircuitTarget,
}

pub struct HexadecimalTreeChainCircuit {
    pub builder: Builder,
    pub target: HexadecimalTreeChainTarget<D>,
}

impl HexadecimalTreeChainCircuit {
    pub fn define(
        config: CircuitConfig,
        child_common_data: &CommonCircuitData<F, D>,
    ) -> Self {
        let mut builder = Builder::new(config);
        let verifier_data = builder.add_virtual_verifier_data(child_common_data.config.fri_config.cap_height);

        // Unroll 16 parallel FRI verifier folding proofs
        let children = std::array::from_fn(|_| {
            let child = builder.add_virtual_proof_with_pis(child_common_data);
            builder.verify_proof::<C>(&child, &verifier_data, child_common_data);
            child
        });

        Self {
            builder,
            target: HexadecimalTreeChainTarget {
                children,
                verifier_data,
            },
        }
    }

    pub fn prove(
        target: &HexadecimalTreeChainTarget<D>,
        circuit_data: &CircuitData<F, C, D>,
        child_proofs: &[ProofWithPublicInputs<F, C, D>],
    ) -> Result<ProofWithPublicInputs<F, C, D>> {
        let mut pw = PartialWitness::new();
        let dummy = dummy_proof::<F, C, D>(circuit_data, HashMap::new())?;
        for i in 0..16 {
            let proof = child_proofs.get(i).unwrap_or(&dummy);
            pw.set_proof_with_pis_target(&target.children[i], proof)?;
        }
        pw.set_verifier_data_target(&target.verifier_data, &circuit_data.verifier_only)?;

        let mut timing = TimingTree::new("Hexadecimal tree recursive prove", Level::Debug);
        let proof = timed!(timing, "prove", circuit_data.prove(pw));
        timing.print();
        Ok(proof?)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use plonky2::plonk::circuit_builder::CircuitBuilder;
    use crate::types::config::CIRCUIT_CONFIG;

    #[test]
    fn test_hexadecimal_tree_chain_define() {
        let mut child_builder = CircuitBuilder::<F, D>::new(CIRCUIT_CONFIG);
        let x = child_builder.add_virtual_target();
        child_builder.register_public_input(x);
        let child_data = child_builder.build::<C>();

        let hex_circuit = HexadecimalTreeChainCircuit::define(CIRCUIT_CONFIG, &child_data.common);
        assert_eq!(hex_circuit.target.children.len(), 16);
    }
}
