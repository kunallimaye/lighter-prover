// Copyright (c) Elliot Technologies, Inc.
// SPDX-License-Identifier: BUSL-1.1

package builder

import (
	"fmt"
	"math/big"

	"github.com/consensys/gnark-crypto/ecc"
	"github.com/consensys/gnark/constraint"
	"github.com/consensys/gnark/frontend"
	"github.com/consensys/gnark/frontend/cs/scs"
	"github.com/elliottech/gnark-plonky2-verifier/fri"
	gl "github.com/elliottech/gnark-plonky2-verifier/goldilocks"
	"github.com/elliottech/gnark-plonky2-verifier/types"
	"github.com/elliottech/gnark-plonky2-verifier/variables"

	"github.com/elliottech/lighter-prover/snark/circuit"
)

func PlaceHolderPublicInputs(numOfPublicInputs uint64) []gl.Variable {
	return make([]gl.Variable, numOfPublicInputs)
}

func PlaceHolderCommitPhaseMerkleCaps(capHeight uint64, numReductionArityBits int) []variables.FriMerkleCap {
	result := make([]variables.FriMerkleCap, numReductionArityBits)
	for i := range result {
		result[i] = variables.NewFriMerkleCap(capHeight)
	}
	return result
}

func PlaceHolderQueryRoundProofs(circuitData types.CommonCircuitData) []variables.FriQueryRound {
	numWires, friConfig, friParams := circuitData.Config.NumWires, circuitData.Config.FriConfig, circuitData.FriParams

	result := make([]variables.FriQueryRound, friParams.Config.NumQueryRounds)
	for i := range result {
		steps := make([]variables.FriQueryStep, len(friParams.ReductionArityBits))
		capHeight := friParams.Config.CapHeight
		codewordLenBits := friParams.LdeBits()
		for j := range steps {
			codewordLenBits -= int(friParams.ReductionArityBits[j])
			steps[j] = variables.NewFriQueryStep(friParams.ReductionArityBits[j], uint64(codewordLenBits)-capHeight)
		}

		result[i] = variables.FriQueryRound{
			InitialTreesProof: variables.NewFriInitialTreeProof([]variables.FriEvalProof{ // len equal to len(Oracles) = 4
				variables.NewFriEvalProof(make([]gl.Variable, fri.NumPreprocessedPolys(&circuitData)), variables.NewFriMerkleProof(friParams.DegreeBits+friConfig.RateBits-friConfig.CapHeight)),
				variables.NewFriEvalProof(make([]gl.Variable, numWires), variables.NewFriMerkleProof(friParams.DegreeBits+friConfig.RateBits-friConfig.CapHeight)),
				variables.NewFriEvalProof(make([]gl.Variable, fri.NumZSPartialProductsPolys(&circuitData)), variables.NewFriMerkleProof(friParams.DegreeBits+friConfig.RateBits-friConfig.CapHeight)),
				variables.NewFriEvalProof(make([]gl.Variable, fri.NumQuotientPolys(&circuitData)), variables.NewFriMerkleProof(friParams.DegreeBits+friConfig.RateBits-friConfig.CapHeight)),
			}),
			Steps: steps,
		}
	}
	return result
}

func PlaceHolderProof(circuitData types.CommonCircuitData) (variables.Proof, []gl.Variable) {
	return variables.Proof{
		WiresCap:                  variables.NewFriMerkleCap(circuitData.Config.FriConfig.CapHeight),
		PlonkZsPartialProductsCap: variables.NewFriMerkleCap(circuitData.Config.FriConfig.CapHeight),
		QuotientPolysCap:          variables.NewFriMerkleCap(circuitData.Config.FriConfig.CapHeight),
		Openings: variables.OpeningSet{
			Constants:       make([]gl.QuadraticExtensionVariable, circuitData.NumConstants),
			PlonkSigmas:     make([]gl.QuadraticExtensionVariable, circuitData.Config.NumRoutedWires),
			Wires:           make([]gl.QuadraticExtensionVariable, circuitData.Config.NumWires),
			PlonkZs:         make([]gl.QuadraticExtensionVariable, circuitData.Config.NumChallenges),
			PlonkZsNext:     make([]gl.QuadraticExtensionVariable, circuitData.Config.NumChallenges),
			PartialProducts: make([]gl.QuadraticExtensionVariable, circuitData.Config.NumChallenges*circuitData.NumPartialProducts),
			QuotientPolys:   make([]gl.QuadraticExtensionVariable, circuitData.Config.NumChallenges*circuitData.QuotientDegreeFactor),
		},
		OpeningProof: variables.FriProof{
			CommitPhaseMerkleCaps: PlaceHolderCommitPhaseMerkleCaps(circuitData.Config.FriConfig.CapHeight, len(circuitData.FriParams.ReductionArityBits)),
			QueryRoundProofs:      PlaceHolderQueryRoundProofs(circuitData),
			FinalPoly:             variables.NewPolynomialCoeffs(uint64(circuitData.FriParams.FinalPolyLen())),
			PowWitness:            gl.Variable{},
		},
	}, PlaceHolderPublicInputs(circuitData.NumPublicInputs)
}

// Returns the R1CS and the circuit digest that is going to be verified. It uses circuit data to generate a place holder proof.
func BuildCircuitPlaceHolder(commonCircuitDataPath, verifierCircuitDataPath string) (constraint.ConstraintSystem, string, error) {
	commonCircuitData := types.ReadCommonCircuitData(commonCircuitDataPath)
	verifierOnlyCircuitDataRaw := types.ReadVerifierOnlyCircuitData(verifierCircuitDataPath)
	verifierOnlyCircuitData := variables.DeserializeVerifierOnlyCircuitData(verifierOnlyCircuitDataRaw)
	proof, publicInputs := PlaceHolderProof(commonCircuitData)

	circuit := circuit.VerifierCircuit{
		Commitment:              frontend.Variable(0),
		PublicInputs:            publicInputs,
		Proof:                   proof,
		VerifierOnlyCircuitData: verifierOnlyCircuitData,
		CommonCircuitData:       commonCircuitData,
	}

	builder := scs.NewBuilder[constraint.U64]
	r1cs, err := frontend.Compile(ecc.BN254.ScalarField(), builder, &circuit)
	if err != nil {
		return nil, "", fmt.Errorf("failed to compile circuit: %v", err)
	}

	return r1cs, verifierOnlyCircuitDataRaw.CircuitDigest, nil
}

// Returns the R1CS and the circuit digest that is going to be verified. It uses real proof to generate the place holder proof.
func BuildCircuit(commonCircuitDataPath, verifierCircuitDataPath, proofPath string) (constraint.ConstraintSystem, string, error) {
	commonCircuitData := types.ReadCommonCircuitData(commonCircuitDataPath)
	verifierOnlyCircuitDataRaw := types.ReadVerifierOnlyCircuitData(verifierCircuitDataPath)
	verifierOnlyCircuitData := variables.DeserializeVerifierOnlyCircuitData(verifierOnlyCircuitDataRaw)
	proofWithPis := variables.DeserializeProofWithPublicInputs(types.ReadProofWithPublicInputs(proofPath))

	circuit := circuit.VerifierCircuit{
		Commitment:              frontend.Variable(0),
		PublicInputs:            proofWithPis.PublicInputs,
		Proof:                   proofWithPis.Proof,
		VerifierOnlyCircuitData: verifierOnlyCircuitData,
		CommonCircuitData:       commonCircuitData,
	}

	builder := scs.NewBuilder[constraint.U64]
	r1cs, err := frontend.Compile(ecc.BN254.ScalarField(), builder, &circuit)
	if err != nil {
		return nil, "", fmt.Errorf("failed to compile circuit: %v", err)
	}

	return r1cs, verifierOnlyCircuitDataRaw.CircuitDigest, nil
}

// BuildVerifierAssignment (issue #117) deserializes the SAME inputs as
// BuildCircuit (the outer-wrapper proof + its common/verifier circuit data) and
// returns the populated VerifierCircuit assignment plus the circuit digest.
//
// The returned assignment is what the gnark witness for plonk.Prove is built
// from. Unlike a frontend solve, frontend.NewWitness takes assignment values
// literally, so we must set the public Commitment to the value the circuit
// constrains it to in VerifierCircuit.Define (circuit.go): the 32 public-input
// bytes interpreted as a big-endian 256-bit integer (the circuit accumulates
// pubInput[i] << (8*(31-i)) in the BN254 field, which reduces mod the scalar
// field). This is the exact value the on-chain verifier checks.
//
// No field is fabricated — the Commitment is derived deterministically from the
// real proof's own public inputs (the keccak256 batch_commitment bytes).
func BuildVerifierAssignment(commonCircuitDataPath, verifierCircuitDataPath, proofPath string) (*circuit.VerifierCircuit, string, error) {
	commonCircuitData := types.ReadCommonCircuitData(commonCircuitDataPath)
	verifierOnlyCircuitDataRaw := types.ReadVerifierOnlyCircuitData(verifierCircuitDataPath)
	verifierOnlyCircuitData := variables.DeserializeVerifierOnlyCircuitData(verifierOnlyCircuitDataRaw)
	proofRaw := types.ReadProofWithPublicInputs(proofPath)
	proofWithPis := variables.DeserializeProofWithPublicInputs(proofRaw)

	if len(proofRaw.PublicInputs) != 32 {
		return nil, "", fmt.Errorf("expected 32 public inputs, got %d", len(proofRaw.PublicInputs))
	}
	// commitment = big-endian uint256 of the 32 public-input bytes (mirrors
	// VerifierCircuit.Define's MulAcc loop; the BN254 field reduces it mod p).
	commitment := new(big.Int)
	for _, b := range proofRaw.PublicInputs {
		commitment.Lsh(commitment, 8)
		commitment.Or(commitment, new(big.Int).SetUint64(b))
	}

	assignment := &circuit.VerifierCircuit{
		Commitment:              frontend.Variable(commitment),
		PublicInputs:            proofWithPis.PublicInputs,
		Proof:                   proofWithPis.Proof,
		VerifierOnlyCircuitData: verifierOnlyCircuitData,
		CommonCircuitData:       commonCircuitData,
	}

	return assignment, verifierOnlyCircuitDataRaw.CircuitDigest, nil
}
