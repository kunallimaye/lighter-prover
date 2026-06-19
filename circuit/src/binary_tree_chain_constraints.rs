// Copyright (c) Elliot Technologies, Inc.
// SPDX-License-Identifier: BUSL-1.1

use anyhow::{Ok, Result};
use log::Level;
use plonky2::iop::witness::{PartialWitness, WitnessWrite};
use plonky2::plonk::circuit_data::{
    CircuitConfig, CircuitData, CommonCircuitData, VerifierCircuitTarget,
};
use plonky2::plonk::proof::{ProofWithPublicInputs, ProofWithPublicInputsTarget};
use plonky2::timed;
use plonky2::util::timing::TimingTree;

use crate::types::config::{Builder, C, D, F};

pub struct BinaryTreeChainTarget<const D: usize> {
    pub left_child: ProofWithPublicInputsTarget<D>,
    pub right_child: ProofWithPublicInputsTarget<D>,
    pub verifier_data: VerifierCircuitTarget,
}

pub struct BinaryTreeChainCircuit {
    pub builder: Builder,
    pub target: BinaryTreeChainTarget<D>,
}

impl BinaryTreeChainCircuit {
    pub fn define(
        config: CircuitConfig,
        child_common_data: &CommonCircuitData<F, D>,
    ) -> Self {
        let mut builder = Builder::new(config);
        let left_child = builder.add_virtual_proof_with_pis(child_common_data);
        let right_child = builder.add_virtual_proof_with_pis(child_common_data);
        let verifier_data = builder.add_virtual_verifier_data(child_common_data.config.fri_config.cap_height);

        builder.verify_proof::<C>(&left_child, &verifier_data, child_common_data);
        builder.verify_proof::<C>(&right_child, &verifier_data, child_common_data);

        Self {
            builder,
            target: BinaryTreeChainTarget {
                left_child,
                right_child,
                verifier_data,
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
        pw.set_proof_with_pis_target(&target.left_child, left_proof).unwrap();
        pw.set_proof_with_pis_target(&target.right_child, right_proof).unwrap();
        pw.set_verifier_data_target(&target.verifier_data, &circuit_data.verifier_only).unwrap();

        let mut timing = TimingTree::new("Binary tree recursive prove", Level::Debug);
        let proof = timed!(timing, "prove", circuit_data.prove(pw));
        timing.print();
        Ok(proof?)
    }
}
