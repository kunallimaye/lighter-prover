# ADR-0005: L6 inner-wrapper KZG sidecar + drive path

- **Status**: Accepted
- **Date**: 2026-06-13
- **Tracking issue**: [#83](https://github.com/kunallimaye/lighter-prover/issues/83)
- **Related issues**: [#116](https://github.com/kunallimaye/lighter-prover/issues/116) (L7 outer wrapper), [#117](https://github.com/kunallimaye/lighter-prover/issues/117) (L8 gnark prove), [#118](https://github.com/kunallimaye/lighter-prover/issues/118) (on-chain self-attestation), [#119](https://github.com/kunallimaye/lighter-prover/issues/119) (mainnet witness generation), [#10](https://github.com/kunallimaye/lighter-prover/issues/10)/[ADR-0002](.) (SRS/key + memory budgeting)

## Context

`WrapperCircuit::prove_inner`
(`circuit/src/recursion/wrapper_circuit.rs:703`) is never called anywhere in
the repo. Driving it requires producing three inputs that no in-repo code
produced before this issue:

1. `delta_chain_proof` — a `CyclicDeltaCircuit` proof. The circuit was only ever
   `define()`d (`circuit/src/bin/build_delta_recursion_circuit.rs`); `prove` had
   zero call sites.
2. `blob_evaluation_proof` — a `BlobEvaluationCircuit` proof. Same: defined only,
   never proved.
3. A KZG `WrapperInput` — `kzg_versioned_hash`, `blob_polynomial_opening_x` (`x`),
   `blob_polynomial_opening_y` (`y`), `batch_commitment`, `blob_bytes`. Only ever
   constructed via serde `Deserialize`; no Rust code computed one. There was **no
   Rust KZG library** in the tree.

## Decisions

### 1. KZG library: `c-kzg` (c-kzg-4844), public Ethereum ceremony setup

We adopt **`c-kzg`** (the canonical Ethereum EIP-4844 reference crate,
BLS12-381 / blst-backed) for the one thing it is needed for: computing the
blob's **KZG versioned hash**
(`kzg_versioned_hash = 0x01 || SHA-256(commitment)[1..]`).

The trusted setup is the **public Ethereum KZG ceremony** output
(`bench/assets/trusted_setup.txt`, 4096 G1 + 65 G2 points,
`sha256 = d39b9f2d047cc9dca2de58f264b6a09448ccd34db967881a6713eacacf0f26b7`),
vendored from the `c-kzg` crate's bundled copy of the mainnet ceremony setup.
EIP-4844's trusted setup is public, so this needs **no Lighter cooperation**.

`c-kzg` requires a C toolchain (`cc`/`gcc`/`clang`) at build time for its blst
bindings — verified present in CI. If a future CI environment lacks one, swap to
the pure-Rust `kzg-rs` with identical semantics behind the same `bench::kzg`
module; all `c-kzg` calls are isolated there.

### 2. BLS12-381 (blob KZG) vs BN254 (L8 gnark wrap) — different curves

The blob KZG commitment is over **BLS12-381** (the EIP-4844 curve; the repo's
`WrapperInput.blob_polynomial_opening_x/y` are BLS12-381 scalar field elements,
`circuit/src/blob/bls12_381_scalar_field.rs`, and the blob layout follows the
consensus-specs `blob_to_polynomial`). This is a **different curve** from the
**BN254** Aztec Ignition SRS auto-downloaded in `snark/main.go` — that SRS is for
the *unrelated L8 gnark wrap* (#117) and is **not** used by the blob sidecar.

### 3. The evaluation point is custom Poseidon2, NOT the EIP-4844 challenge

This is the central cryptographic finding. The in-circuit proof-of-commitment-
equivalence (PCE) check (`BlobEvaluationCircuit::verify_pce_evaluation`,
`circuit/src/blob/blob_constraints.rs:273-394`) does **not** derive the
evaluation point `x` from the EIP-4844 standard Fiat-Shamir challenge
(`compute_challenge`, SHA-256 over blob+commitment). It uses a **custom
Poseidon2 transcript**:

```text
blob_data_hash = Poseidon2(reserved_hash, market_data_hash, account_delta_tree_root)
challenge_bits = Poseidon2(blob_data_hash.elements || kzg_versioned_hash.bytes)
x              = reduce_to_BLS12381Scalar(challenge_bits)   # 4 Goldilocks elems -> 8 LE u32 limbs -> mod r
y              = p(x)                                        # barycentric eval of the blob polynomial
```

Therefore `c-kzg::compute_kzg_proof` **cannot** produce a matching `(x, y)`. The
sidecar (`bench/src/kzg.rs`) instead **replicates the circuit's logic
off-circuit** using the same primitives the circuit uses in-circuit:

- `Poseidon2Hash` (identical permutation) for the transcript,
- the existing plain-Rust `BLS12381Scalar` `Field`/`PrimeField` arithmetic for
  the barycentric polynomial evaluation,
- the same bit-reversal-permuted `ROOTS_OF_UNITY` constants and 31-byte-per-limb
  blob packing (`BlobPolynomialTarget::from_bytes`).

**Correctness is enforced by construction, never stubbed.** If `x` or `y` is
wrong, the `connect_nonnative` constraints in `verify_pce_evaluation` fail and
`BlobEvaluationCircuit::prove` returns an error. The `--blob-prove` bench mode
and the `test_blob_evaluation_prove` smoke test are faithful gates on this.

### 4. Drivers as bench modes (no new binaries)

New prove flows are added as bench modes in `bench/src/bin/bench.rs`, following
the `run_l5_segment_check` / `--l5-fold tree` patterns:

| Mode | Produces | Acceptance criterion |
|---|---|---|
| `--delta-prove` | `delta_chain_proof` (`DeltaCircuit` -> `CyclicDeltaCircuit`) | #1 |
| `--blob-prove` | `blob_evaluation_proof` + KZG `WrapperInput` | #2, #3 |
| `--l6-inner` | assembles all three inputs + builds the inner wrapper | #4 |

Helpers live in `bench/src/kzg.rs` (KZG sidecar), `bench/src/blob_encode.rs`
(blob layout), and `bench/src/l6drive.rs` (delta-chain driver). Smoke tests:
`test_delta_chain_prove`, `test_blob_evaluation_prove` (both `#[ignore]`d as
heavy plonky2 proves; run with `--ignored` + a large stack, or via the bench
modes).

### 5. Target data: correctly-shaped synthesized batch (not mainnet)

Per acceptance criterion #4, the target is a **correctly-shaped synthesized**
empty batch (`EMPTY_ACCOUNT_DELTA_TREE_ROOT`, no market updates), not real
mainnet data. Real-mainnet witness generation is closed-source and deferred to
#119. Open Questions 1/2 (mainnet circuit-shape / vk-digest pin) gate on-chain
attestation and are deferred to #118; the repo's current default shape
(`TX_PER_PROOF`, `CHAIN_ID`, etc. from `build_circuits.sh`) is used here.

## Status of acceptance criteria

- **#1 (delta chain)** — DONE. `--delta-prove` + `test_delta_chain_prove`:
  `CyclicDeltaCircuit::prove` driven, `data.verify` passes.
- **#2 (blob evaluation)** — DONE. `--blob-prove` + `test_blob_evaluation_prove`:
  `BlobEvaluationCircuit::prove` driven, `data.verify` passes.
- **#3 (KZG `WrapperInput` accepted by the PCE check)** — DONE. The sidecar's
  `(x, y)` is accepted by `verify_pce_evaluation` (proven by #2's prove).
- **#4 (verifying inner-wrapper proof over a correctly-shaped batch)** —
  **PARTIAL.** `--l6-inner` produces and verifies the `delta_chain_proof` and
  `blob_evaluation_proof`, derives the wrapper-consistent delta evaluation point
  off-circuit, computes the KZG `WrapperInput`, and builds the L5 (2^15) and
  inner-wrapper (2^18) circuits. The remaining step is producing **8 L5 chain
  proofs whose merged batch has `new_account_delta_tree_root ==
  EMPTY_ACCOUNT_DELTA_TREE_ROOT`** — i.e. an L5 chain over genuinely no-op
  blocks, mutually consistent with the empty delta chain and empty blob across
  `verify_aggregated_delta` and `verify_delta_polynomial_evaluation`. The
  existing `run_l5_segment_check` driver synthesizes blocks with real txs whose
  `new_account_delta_tree_root` is non-empty, so it cannot directly feed
  `prove_inner` without first constructing a consistent-empty (or fully
  mutually-consistent non-empty) batch. We did **not** fabricate KZG values or
  relax any constraint to force a terminating prove; the consistent-empty L5
  chain is tracked as the closing step for #83.
- **#5 (SRS/key + memory budget documented)** — this ADR + `bench/README.md`.

## Memory budget (debug build, AMD EPYC 7B13, 32c / 125 GiB)

Measured during the bench modes (debug profile; release will be lower wall but
similar peak shape):

| Stage | Wall (debug) | Notes |
|---|---|---|
| `--delta-prove` (delta leaf + 1 cyclic fold) | ~34 s | two recursive circuits |
| `--blob-prove` (KZG sidecar + blob-eval prove) | ~7 min | 4096 BLS12-381 nonnative elems; 128351 PIs |
| `--l6-inner` build (blob + delta + L5 + inner wrapper) | ~11 min | L5 degree 2^15, inner wrapper degree 2^18 |

The blob-evaluation and inner-wrapper circuit builders are deep enough to
overflow the default 8 MiB main-thread stack; the bench modes run them on a
dedicated 4 GiB-stack thread (`run_on_big_stack`). Unit tests require
`RUST_MIN_STACK=4294967296`.

## Consequences

- The repo gains its first off-circuit KZG tooling and its first drivers for the
  delta-chain and blob-evaluation prove paths.
- A C toolchain is now a build-time requirement (for `c-kzg`/blst); documented
  with a pure-Rust fallback path.
- The remaining consistent-empty L5 chain step is the only thing between this
  work and a terminating `prove_inner` for criterion #4; it is in-repo work (no
  Lighter dependency).
