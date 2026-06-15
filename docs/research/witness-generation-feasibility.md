# Witness Generation: Feasibility Study

**Status**: Findings recorded
**Date**: 2026-06-13
**Tracking issue**: #120

## Context / Motivation

The codebase proves a witness but does not generate one. This study determines
whether witness generation can be reverse-engineered so the prover can be driven
with **varied** data (not just the single bundled `bench/bench_test.json`) for
throughput benchmarking.

**Verdict: viable.** The cryptographic core needed to reconstruct witnesses is
reproducible with public tooling, confirmed empirically (see "Make-or-break
finding" below).

## How the prover consumes witnesses

- The prover only **deserializes → assigns → proves** a finished witness.
  `generate_witness()` is pure assignment (`circuit/src/block_tx_constraints.rs:183-219`);
  it never computes tree roots, Merkle proofs, deltas, or signatures.
- The sample `bench/bench_test.json` is one complete witness for a 500-tx block
  (block 186974592). Every Merkle proof, root, delta, and signature is pre-baked.
- The witness schema is recoverable from serde `#[serde(rename=...)]` attributes:
  block fields in `circuit/src/block.rs:36-125`; per-tx witness in
  `circuit/src/tx.rs:69-371`; market details (`mib`) in
  `circuit/src/types/market_details.rs:30-91`; assets (`aab`) in
  `circuit/src/types/asset.rs`.

## The prover is a strict verifier (synthetic data crashes)

- `BlockTxCircuit::prove()` calls `circuit.verify(proof)`
  (`circuit/src/block_tx_constraints.rs:176`). Even without it, plonky2 **panics**
  during witness generation on any unsatisfied constraint
  (`bench/src/bin/bench.rs:664-666`). Inconsistent / synthetic / naively-duplicated
  witnesses crash; they do not run slowly.
- All Merkle proofs are verified in-circuit (`circuit/src/merkle_helpers.rs:84-98`
  via `connect_hashes`); state-root transitions are enforced; signatures are
  verified (Schnorr/EdDSA for L2 via `circuit/src/.../tx_type.rs:439-445` and
  `schnorr.rs:147-169`; ECDSA for L1); nonces are bound
  (`nonce == api_key_before_nonce`, `tx_type.rs:425-429`).
- Naive tx-duplication fails because: (1) the circuit increments the api-key nonce
  after each tx, so a duplicate carries a stale nonce; (2) every touched leaf was
  already mutated, so the carried roots no longer match the duplicate's `*_before`
  leaves + proofs; (3) replaying a cancel against an already-cancelled order fails
  the unconditional order-book checks. Even the empty/padding tx
  (type 0, `TX_TYPE_EMPTY=0`) runs all unconditional Merkle verifications, so its
  leaves/proofs must be genuine current-state values.

## The sequencer and witness generator are closed-source

- Per the architecture docs
  (<https://docs.lighter.xyz/about-lighter/technical-architecture-lighter-core.md>),
  a Sequencer feeds data to Witness Generator services that produce circuit inputs;
  both are proprietary internal services.
- The public `elliottech` GitHub org has **no** sequencer / witness-generator repo —
  only SDKs (`lighter-go`, `lighter-python`), contracts (`lighter-contracts`), the
  prover (`lighter-prover`), crypto (`poseidon_crypto`), and proving-system forks
  (`plonky2`, `gnark-plonky2-verifier`, `zeknox`). No runnable node/sequencer Docker
  image or testnet node is published.
- One architecturally interesting public data source: the **Escape Hatch / Ethereum
  data blobs**. The docs state each state-update proposal posts compressed,
  per-account state-transition data sufficient for users to reconstruct state
  on-chain in Escape Hatch mode. However, the blob encoding/compression format, the
  Merkle tree construction, and leaf hashing are **not** publicly documented, and the
  escape-hatch artifact is a proof-of-ownership-for-withdrawal, not a full
  block-execution witness.

## Make-or-break finding (CONFIRMED): public Poseidon2 lib matches the prover

This is the pivotal result that makes reconstruction viable.

- The public Go library `github.com/elliottech/poseidon_crypto` v0.0.17 (package
  `poseidon2_goldilocks_plonky2`) reproduces the prover's Poseidon2 tree hashes
  **bit-for-bit**.
- **Empirical**: a 434-element multi-chunk Poseidon2 sponge (`all_assets_hash`;
  preimage = the sample's `aab` array) matched the prover's ground-truth output
  exactly — all 4 Goldilocks limbs identical:
  `[9776559263212475433, 712092433400043299, 4339523838532887526, 10233466513964060619]`.
- **Structural proof**: the Go lib's Poseidon2 was ported from the **same Plonky3
  commit** (`eeb4e37b20127c4daa871b2bad0df30a7c7380db`) that the prover's
  `Cargo.lock` pins — identical round constants and `MATRIX_DIAG_12` diagonal.
- **Scheme**: Poseidon2 over Goldilocks (p = 2^64 − 2^32 + 1 = 18446744069414584321);
  WIDTH=12, RATE=8, capacity=4, output=4 elements (HashOut = 32 bytes); S-box x^7;
  8 full + 22 partial rounds; external M4 circulant linear layer, internal Plonky3
  Goldilocks diagonal. `hash_no_pad` = overwrite-mode sponge (copy each 8-element
  chunk into `state[0..len]`, permute, no padding; output = first 4 elements).
  two-to-one compress = left → `state[0..4]`, right → `state[4..8]`, rest zero,
  permute, take first 4. `hash_n_to_one` = left-fold of two-to-one.
- **Prover config aliases**: `circuit/src/types/config.rs:12-16`
  (`C = Poseidon2GoldilocksConfig`, `F = GoldilocksField`,
  `PoseidonHash = Poseidon2Hash`). A native (non-circuit) reference that reproduces
  the JSON state root `osr` bit-for-bit already exists at `bench/src/seed.rs`
  (notable spans `:45-58` fold, `:96-135` leaf params, `:235-277` state-root
  assembly). HashOut JSON encoding (`circuit/src/deserializers.rs:451-458`): a
  4-element array of decimal u64 limbs, limb i = element i (no byte reversal).

## Public building blocks

| Building block | Source | Status |
|---|---|---|
| Poseidon2 hash (leaf + Merkle node) | `elliottech/poseidon_crypto` (Go) | Verified bit-identical to prover |
| Tx serialization + tx-hash + signing | `elliottech/lighter-go` SDK | Public reference impl |
| State-transition rules | this repo's circuit constraints | Executable spec |
| Leaf encodings + hash recipes | `bench/src/seed.rs` (native reference) | Already in repo |
| State-root commitment + blob anchor | `elliottech/lighter-contracts` (Solidity, BSL 1.1) | Readable |
| Raw tx stream + public market/account state | `lighter-python` SDK + explorer/REST API | Seeds inputs |

## Assessment

- Witness **generation** can be reconstructed: the hard cryptographic primitive
  (Poseidon2) is solved via a verified-compatible public lib; tx encoding/signing
  has a public reference (`lighter-go`); the state-transition rules are an executable
  spec in this repo's circuit constraints; and a working native hash reference
  (`bench/src/seed.rs`) plus ground-truth values (`bench_test.json`) de-risk
  validation. The remaining work is **transcription of known rules, not discovery of
  unknown ones**.
- The genuinely closed pieces (sequencer, witness-generator service) do not block
  reconstruction because their **rules** are recoverable from the public artifacts
  above.
- On the "historical API": the in-repo feeder (`bench/feeder/feeder.py`) fetches only
  block-arrival timing and tx-counts (not witnesses); the streaming benchmark replays
  the bundled `bench_test.json` at live cadence. Raw per-block tx content and public
  state from the SDKs / explorer API can **seed** reconstruction inputs, but the
  cryptographic scaffolding (proofs, roots, intra-block deltas) must be re-derived.

## Remaining work to produce a novel VALID witness

1. Build the Merkle trees (account, asset, market, order-book, delta) with the
   verified Poseidon2 — transcribe tree layout from `circuit/src/merkle_helpers.rs`
   and the `*_root` fields.
2. Reimplement state transitions for the targeted tx types (circuit constraints =
   exact spec; `bench/src/seed.rs` = pattern).
3. Generate Merkle proofs for each touched leaf at each intra-block step (mechanical
   once trees exist).
4. Encode every leaf in canonical fixed-point form (e.g. i64 → little-endian 32-bit
   limb split; recipe in `asset.rs` / `seed.rs`).
5. Validate against `bench_test.json` at each layer (ground-truth roots/leaves
   available).

For the throughput-benchmarking goal, covering the dominant tx types + valid padding
(validated against the sample) is likely sufficient; full tx-type coverage is not
required.

## Proposed next steps

- [ ] Complete a full state-root (`osr`) end-to-end reproduction via the Go lib
      (reverse-engineer `mib`/`pmda` leaf encodings) — last PoC before build.
- [ ] Thin-slice PoC: reconstruct ONE leaf update + its Merkle proof for ONE simple
      tx in Go, validated against `bench_test.json`.
- [ ] Design doc + phased plan for a Go witness reconstructor
      (trees → state transitions → proofs → validation).

## References

- Architecture: <https://docs.lighter.xyz/about-lighter/technical-architecture-lighter-core.md>
- Poseidon2 Go lib: <https://github.com/elliottech/poseidon_crypto>
- Signing/hashing SDK: <https://github.com/elliottech/lighter-go>
- Contracts: <https://github.com/elliottech/lighter-contracts>
- Tracking issue: #120
