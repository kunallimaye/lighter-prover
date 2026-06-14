// Standalone Go module for the Phase-0 witness-reconstructor replay/validation
// harness (epic #121, issue #122). Kept as its own module so it does NOT
// perturb the repo-root go.mod (the gnark plonky2 verifier build) or
// `make local-test`.
module github.com/kunallimaye/lighter-prover/tools/witness-reconstructor

go 1.24.0

require github.com/elliottech/poseidon_crypto v0.0.17

require (
	github.com/bits-and-blooms/bitset v1.14.2 // indirect
	github.com/consensys/gnark-crypto v0.12.2-0.20240215234832-d72fcb379d3e // indirect
)
