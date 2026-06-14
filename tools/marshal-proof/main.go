// Copyright (c) Elliot Technologies, Inc.
// SPDX-License-Identifier: BUSL-1.1

// Command marshal-proof (issue #118) loads a gnark BN254 PLONK proof
// (final::<digest>.proof, produced by the #117 prove path's proof.WriteTo) and
// re-serializes it into the *Solidity calldata* layout via gnark's
// proof.MarshalSolidity(). The native WriteTo binary form and the Solidity form
// differ: the on-chain gnark PlonkVerifier.Verify(bytes,uint256[]) expects the
// MarshalSolidity layout (curve points as 32-byte coords + the BSB22 commitment
// and its opening), NOT the compact WriteTo form.
//
// It also derives the single public input the verifier checks: the big-endian
// uint256 of the 32 public-input bytes of the outer-wrapper proof
// (the keccak256 batch_commitment), reduced mod the BN254 scalar field.
//
// Output (stdout): the Solidity proof hex and the public-input value, so a
// read-only eth_call to verifyProof/Verify can be assembled. No value is
// fabricated — both come from the real proof artifacts.
//
// Usage:
//
//	go run ./tools/marshal-proof <final::<d>.proof> <outer-wrapper-proof::<d>.json>
package main

import (
	"encoding/hex"
	"encoding/json"
	"fmt"
	"math/big"
	"os"

	"github.com/consensys/gnark-crypto/ecc"
	"github.com/consensys/gnark/backend/plonk"
)

// bn254ScalarField is the BN254 scalar field modulus r.
var bn254ScalarField, _ = new(big.Int).SetString(
	"21888242871839275222246405745257275088548364400416034343698204186575808495617", 10)

func main() {
	if len(os.Args) < 3 {
		fmt.Fprintln(os.Stderr, "usage: marshal-proof <final.proof> <outer-wrapper-proof.json>")
		os.Exit(2)
	}
	proofPath := os.Args[1]
	pisPath := os.Args[2]

	// Load the gnark BN254 proof and re-serialize to the Solidity layout.
	proof := plonk.NewProof(ecc.BN254)
	f, err := os.Open(proofPath)
	if err != nil {
		panic(err)
	}
	defer f.Close()
	if _, err := proof.ReadFrom(f); err != nil {
		panic(err)
	}
	sm, ok := proof.(interface{ MarshalSolidity() []byte })
	if !ok {
		panic("proof does not implement MarshalSolidity")
	}
	solProof := sm.MarshalSolidity()

	// Derive the public input: big-endian uint256 of the 32 public-input bytes
	// of the plonky2 outer-wrapper proof, reduced mod r (mirrors the in-circuit
	// commitment accumulation in snark/circuit/circuit.go).
	var pis struct {
		PublicInputs []uint64 `json:"public_inputs"`
	}
	pb, err := os.ReadFile(pisPath)
	if err != nil {
		panic(err)
	}
	if err := json.Unmarshal(pb, &pis); err != nil {
		panic(err)
	}
	if len(pis.PublicInputs) != 32 {
		panic(fmt.Sprintf("expected 32 public inputs, got %d", len(pis.PublicInputs)))
	}
	commit := new(big.Int)
	for _, b := range pis.PublicInputs {
		commit.Lsh(commit, 8)
		commit.Or(commit, new(big.Int).SetUint64(b))
	}
	commit.Mod(commit, bn254ScalarField)

	fmt.Printf("SOLIDITY_PROOF_LEN=%d\n", len(solProof))
	fmt.Printf("SOLIDITY_PROOF_HEX=%s\n", hex.EncodeToString(solProof))
	fmt.Printf("PUBLIC_INPUT_DEC=%s\n", commit.String())
	fmt.Printf("PUBLIC_INPUT_HEX=0x%s\n", commit.Text(16))
}
