# Lighter Prover circuit topology: the 8 layers

**Status**: Living reference (information-only). Update via PR when the
upstream circuit topology changes.

This is the canonical reference for the recursive-proof topology of the
Lighter Prover: 8 layers from per-chunk transaction proofs up to the
gnark BN254 PLONK wrapper that Solidity verifies on Ethereum L1. It is
cited by the containerization, distributed-proving, and benchmarking work
to reason about where parallelism is possible and where serial recursion
is required.

> Originally tracked as issue #1 (an editable issue body). Promoted into
> the repo so the reference is durable and reviewable: changes now arrive
> as PRs rather than silent issue-body edits.

## Source of truth

- `bench/src/bin/bench.rs` — the working end-to-end L1-L3 pipeline
- `circuit/src/bin/build_block_circuit.rs`
- `circuit/src/bin/build_recursion_circuit.rs`
- `circuit/src/bin/build_wrapper_circuit.rs`
- `circuit/src/recursion/wrapper_circuit.rs` — L6/L7 `prove_inner` / `prove_outer`
- `snark/main.go` — L8 gnark BN254 PLONK wrapper

> **Line numbers below were verified against `main`.** They drift as the
> code moves; treat the symbol names as the stable anchor and the line
> numbers as a convenience.

## The 8 layers

| # | Layer | Circuit / code | Aggregates | Parallel? | Notes |
|---|---|---|---|---|---|
| 1 | Per-chunk tx proof | `BlockTxCircuit::prove` (`bench/src/bin/bench.rs:740`) | `tx_per_proof` (S) transactions | **Yes — embarrassingly parallel across chunks** | Smallest unit of provable work; the natural sharding boundary |
| 2 | Tx chain | `BlockTxChainCircuit::prove` (`bench/src/bin/bench.rs:780`) | All layer-1 proofs of a block, linked-list style | **No — serial recursion** (tree-fold variant in ADR-0003 §D3) | Each step consumes the previous chain proof + next tx proof |
| 3 | Pre-execution | `BlockPreExecutionCircuit::prove` (`bench/src/bin/bench.rs:625`) | State setup before tx execution | Independent of layers 1-2 | Can run in parallel with layer 1 |
| 4 | Block | `BlockCircuit` (`circuit/src/bin/build_block_circuit.rs`) | Wraps layers 2 + 3 into one block proof | One per block | |
| 5 | Cyclic recursion | `CyclicRecursionCircuit` (`circuit/src/bin/build_recursion_circuit.rs`) | Many block proofs into one | Serial fold (8-way segment parallelism + tree-fold in ADR-0003 §D5) | Cyclic / IVC-style |
| 6 | Inner wrapper | `WrapperCircuit::prove_inner` (`circuit/src/recursion/wrapper_circuit.rs:703`) | Re-proves layer 5 in a BN128-friendly config; natively merges up to `NUM_CHAINS_PER_BATCH = 8` L5 chains (`wrapper_circuit.rs:44`) | One-shot | KZG `WrapperInput` sidecar detailed in ADR-0005 |
| 7 | Outer wrapper | `WrapperCircuit::prove_outer` (`circuit/src/recursion/wrapper_circuit.rs:733`) | Finalizes for gnark consumption | One-shot | Emits JSON sidecars for layer 8 |
| 8 | Gnark PLONK wrapper | `snark/main.go` `runProve` (Go, gnark BN254 PLONK `plonk.Prove`) | Re-proves layer 7 inside a BN254 PLONK circuit so Solidity verifies it on Ethereum L1 | One-shot per final proof | Produces `.pk`, `.vk`, `.r1cs`, `.sol` |

## Implications for distributed proving

- **Layer 1 is the only natural fan-out point.** N workers each prove a
  tx chunk; results are serialized `ProofWithPublicInputs` blobs.
- **Layer 2 must run sequentially** on a single process — each step
  depends on the previous chain proof. An aggregator / coordinator
  process owns this (the tree-fold variant in ADR-0003 §D3 reduces the
  serial depth to ~log₂(N) steps).
- **Layer 3 (pre-execution) can run on the orchestrator in parallel with
  layer-1 workers** since it does not depend on transaction proofs.
- Layers 4-8 are one-shot, single-process steps that only run if/when a
  full block (or batch of blocks) needs to produce an on-chain proof.
  They are *not* part of throughput benchmarking.

## Container / role topology implication

A distributed prover needs:

- **Worker role**: pure layer-1 prover. Inputs: `(block_data,
  chunk_index)`. Output: serialized tx-chunk proof.
- **Orchestrator role**: dispatches layer-1 work, runs layer 3 locally,
  then runs layer 2 serially as worker outputs arrive in order.
  Optionally runs layers 4-8.

Both roles can ship in the same container image since they share all
circuit data; the role is selected at runtime. See ADR-0001 (container
topology) and ADR-0006 (the distributed-prover conductor).

## Where each layer is elaborated

The decision records build on this topology; consult them for the
per-layer engineering detail and measured constants:

| Layers | Decision record |
|---|---|
| L1-L8 streaming-cell shape, L2 tree-fold (§D3), L5 hybrid parallelism (§D5) | `docs/decisions/ADR-0003-prover-cell-streaming-architecture.md` |
| L4/L5 recursion as a unified distribution primitive + governing equation | `docs/decisions/ADR-0004-unified-recursive-distribution.md` |
| L6 inner-wrapper KZG sidecar + drive path | `docs/decisions/ADR-0005-l6-inner-wrapper-kzg-sidecar.md` |
| Operational distribution / conductor across L4-L8 | `docs/decisions/ADR-0006-distributed-prover-conductor.md` |

## Measurement coverage

Per-layer instrumentation status. Structured per-layer telemetry
(`BENCH_EVENT` JSONL: wall, CPU, peak RSS) landed in #9.

| # | Layer | Exercised by open-source code? | Driver status |
|---|---|---|---|
| 1 | `BlockTxCircuit::prove` | yes (`bench.rs:740`) | measured (structured + RSS + CPU, #9) |
| 2 | `BlockTxChainCircuit::prove` | yes (`bench.rs:780`) | measured (structured + RSS + CPU, #9) |
| 3 | `BlockPreExecutionCircuit::prove` | yes (`bench.rs:625`) | measured (structured + RSS + CPU, #9) |
| 4 | `BlockCircuit` | yes | L4 prove measured (#10 Stage-A) |
| 5 | `CyclicRecursionCircuit` | yes | L5 fold measured (#10 Stage-A); tree-fold build-validated (#82) |
| 6 | `WrapperCircuit::prove_inner` | yes | inner prove over verified chain (#129/#130); KZG sidecar (ADR-0005) |
| 7 | `WrapperCircuit::prove_outer` | yes | outer prove over verified inner proof (#116/#136) |
| 8 | `snark/main.go` gnark BN254 PLONK | yes | final-proof `plonk.Prove` path landed (#117/#142) |

> **Historical note.** When this reference was first filed, layers 4-8
> had prove *methods* on their types but **no driver** wiring them
> together end-to-end — the driver work was tracked in spike #10. That
> driver gap has since been progressively closed (see the Driver-status
> column). Any throughput benchmark from this repo still covers **layers
> 1-3**; L4-L8 are one-shot finalization steps, not throughput stages.
