// Copyright (c) Elliot Technologies, Inc.
// SPDX-License-Identifier: BUSL-1.1

//! Off-circuit KZG sidecar for the L6 inner-wrapper drive path (issue #83).
//!
//! `WrapperCircuit::prove_inner` (circuit/src/recursion/wrapper_circuit.rs)
//! needs a [`WrapperInput`] whose `kzg_versioned_hash`, `blob_polynomial_opening_x`
//! (`x`) and `blob_polynomial_opening_y` (`y`) are accepted by the in-circuit
//! proof-of-commitment-equivalence (PCE) check in
//! `BlobEvaluationCircuit::verify_pce_evaluation`
//! (circuit/src/blob/blob_constraints.rs:273-394).
//!
//! ## Why c-kzg alone is NOT sufficient
//!
//! The in-circuit PCE evaluation point `x` is **not** the EIP-4844 standard
//! Fiat-Shamir challenge (`compute_challenge` over the blob + commitment with
//! SHA-256). Instead the circuit derives `x` from a **custom Poseidon2**
//! transcript (`_get_pce_evaluation_point`):
//!
//! ```text
//! blob_data_hash      = Poseidon2(reserved_hash, market_data_hash, account_delta_tree_root)
//! challenge_bits      = Poseidon2(blob_data_hash.elements || kzg_versioned_hash.bytes)
//! x                   = reduce_to_BLS12381Scalar(challenge_bits)   // 4 Goldilocks elems -> 8 LE u32 limbs -> mod r
//! y                   = p(x)                                       // barycentric eval of the blob polynomial
//! ```
//!
//! Therefore `c-kzg`'s `compute_kzg_proof` cannot produce a matching `(x, y)`.
//! We replicate the circuit's logic off-circuit here using the SAME plain-Rust
//! primitives the circuit uses in-circuit:
//!   * [`Poseidon2Hash`] for the transcript (identical permutation),
//!   * [`BLS12381Scalar`] field arithmetic for the polynomial evaluation,
//!   * the SAME bit-reversal-permuted roots of unity (`ROOTS_OF_UNITY`).
//!
//! c-kzg IS used (and only used) to compute `kzg_versioned_hash`, which is the
//! EIP-4844 versioned hash of the BLS12-381 KZG commitment to the blob:
//! `0x01 || SHA-256(commitment)[1..]`.
//!
//! This contract is enforced by construction: if any of these three values is
//! wrong, `BlobEvaluationCircuit::prove` (driven by `--blob-prove` and the
//! `test_blob_evaluation_prove` smoke test) fails its constraints. We never
//! fabricate KZG values or relax the circuit check.

use anyhow::{Context, Result, anyhow};
use circuit::blob::blob_domain::CARDINALITY;
use circuit::blob::bls12_381_scalar_field::BLS12381Scalar;
use circuit::blob::constants::*;
use circuit::blob::roots_of_unity::ROOTS_OF_UNITY;
use circuit::poseidon2::Poseidon2Hash;
use circuit::recursion::wrapper_circuit::WrapperInput;
use circuit::types::config::F;
use circuit::types::constants::KECCAK_HASH_OUT_BYTE_SIZE;
use num::{BigUint, Num, Zero};
use plonky2::field::types::{Field, PrimeField, PrimeField64};
use plonky2::hash::hash_types::HashOut;
use plonky2::plonk::config::Hasher;

/// Number of 32-byte canonical field elements in an EIP-4844 blob.
const FIELD_ELEMENTS_PER_BLOB: usize = BLOB_WIDTH; // 4096
/// Canonical EIP-4844 blob size in bytes (4096 * 32).
const BYTES_PER_BLOB: usize = FIELD_ELEMENTS_PER_BLOB * 32; // 131072

/// Repo-relative default path to the public Ethereum KZG ceremony trusted setup.
pub const DEFAULT_TRUSTED_SETUP_PATH: &str = "bench/assets/trusted_setup.txt";

/// Expand the repo's 31-byte-per-limb packed blob (`BLOB_DATA_BYTES_COUNT`
/// bytes) into the canonical 131072-byte EIP-4844 blob layout (4096 field
/// elements x 32 bytes, each a big-endian canonical BLS12-381 scalar).
///
/// The repo omits the leading zero byte of every 32-byte limb (see
/// `BlobPolynomialTarget::from_bytes`, which prepends a zero byte before
/// interpreting each 31-byte chunk as a big-endian scalar). We reverse that
/// here: each 31-byte chunk becomes `0x00 || chunk`.
pub fn packed_blob_to_canonical(packed: &[u8; BLOB_DATA_BYTES_COUNT]) -> Box<[u8; BYTES_PER_BLOB]> {
    let mut canonical = vec![0u8; BYTES_PER_BLOB];
    for i in 0..FIELD_ELEMENTS_PER_BLOB {
        // Destination element: 32 bytes, leading byte left as 0x00.
        let dst = &mut canonical[i * 32 + 1..i * 32 + 32];
        let src = &packed[i * 31..i * 31 + 31];
        dst.copy_from_slice(src);
    }
    canonical
        .into_boxed_slice()
        .try_into()
        .expect("canonical blob is exactly BYTES_PER_BLOB")
}

/// Compute the EIP-4844 versioned hash of the KZG commitment to `canonical_blob`
/// using the public Ethereum KZG ceremony trusted setup loaded from `setup_path`.
///
/// Returns `0x01 || SHA-256(commitment)[1..32]`.
pub fn kzg_versioned_hash(
    canonical_blob: &[u8; BYTES_PER_BLOB],
    setup_path: &str,
) -> Result<[u8; KECCAK_HASH_OUT_BYTE_SIZE]> {
    use c_kzg::{Blob, KzgSettings};
    use sha2::{Digest, Sha256};

    let settings = KzgSettings::load_trusted_setup_file(std::path::Path::new(setup_path), 0)
        .map_err(|e| anyhow!("load trusted setup from {setup_path}: {e:?}"))
        .with_context(|| {
            format!(
                "the public Ethereum KZG ceremony trusted setup must exist at {setup_path} \
                 (see docs/decisions/ADR-0005)"
            )
        })?;

    let blob = Blob::from_bytes(canonical_blob.as_slice())
        .map_err(|e| anyhow!("c-kzg Blob::from_bytes: {e:?}"))?;
    let commitment = settings
        .blob_to_kzg_commitment(&blob)
        .map_err(|e| anyhow!("c-kzg blob_to_kzg_commitment: {e:?}"))?;

    let commitment_bytes = commitment.to_bytes();
    let digest = Sha256::digest(commitment_bytes.as_slice());

    let mut versioned = [0u8; KECCAK_HASH_OUT_BYTE_SIZE];
    versioned.copy_from_slice(digest.as_slice());
    // EIP-4844 versioned-hash version byte for KZG commitments.
    versioned[0] = 0x01;
    Ok(versioned)
}

/// The BLS12-381 scalar field order `r`.
fn bls_order() -> BigUint {
    BLS12381Scalar::order()
}

/// Off-circuit replica of
/// `BlobEvaluationCircuit::_get_version_and_reserved_bytes_hash`.
fn version_and_reserved_bytes_hash(packed: &[u8; BLOB_DATA_BYTES_COUNT]) -> HashOut<F> {
    let mut limbs: Vec<F> = Vec::new();
    // version: 2 big-endian bytes -> one field element.
    let version =
        (packed[BLOB_VERSION_INDEX] as u64) * (1 << 8) + packed[BLOB_VERSION_INDEX + 1] as u64;
    limbs.push(F::from_canonical_u64(version));
    // reserved: chunks of 4 big-endian bytes -> one field element each.
    for chunk in packed[BLOB_RESERVED_INDEX..BLOB_MARK_PRICE_INDEX].chunks(4) {
        let mut res: u64 = 0;
        for &b in chunk {
            res = res * (1 << 8) + b as u64;
        }
        limbs.push(F::from_canonical_u64(res));
    }
    Poseidon2Hash::hash_no_pad(&limbs)
}

/// Off-circuit replica of `BlobEvaluationCircuit::_get_market_data_hash`.
///
/// `market` must contain, per position slot, `(mark_price, funding_is_negative,
/// funding_limb_hi, funding_limb_lo, quote_multiplier)` exactly as the circuit
/// reads them from `PublicMarketDetailsTarget`.
fn market_data_hash(market: &[MarketLimbs]) -> HashOut<F> {
    let mut limbs: Vec<F> = Vec::new();
    for m in market {
        limbs.push(F::from_canonical_u64(m.mark_price));
    }
    for m in market {
        limbs.push(F::from_canonical_u64(m.funding_is_negative as u64));
        limbs.push(F::from_canonical_u64(m.funding_limb_hi));
        limbs.push(F::from_canonical_u64(m.funding_limb_lo));
    }
    for m in market {
        limbs.push(F::from_canonical_u64(m.quote_multiplier));
    }
    Poseidon2Hash::hash_no_pad(&limbs)
}

/// Per-market-slot scalar limbs as the circuit reads them in `_get_market_data_hash`.
#[derive(Clone, Copy, Debug, Default)]
pub struct MarketLimbs {
    pub mark_price: u64,
    pub funding_is_negative: bool,
    pub funding_limb_hi: u64,
    pub funding_limb_lo: u64,
    pub quote_multiplier: u64,
}

/// Off-circuit replica of `BlobEvaluationCircuit::_get_blob_data_hash`:
/// `Poseidon2(reserved_hash, market_data_hash, account_delta_tree_root)`,
/// folded two-to-one exactly like `hash_n_to_one`.
fn blob_data_hash(
    packed: &[u8; BLOB_DATA_BYTES_COUNT],
    market: &[MarketLimbs],
    account_delta_tree_root: HashOut<F>,
) -> HashOut<F> {
    let reserved = version_and_reserved_bytes_hash(packed);
    let md = market_data_hash(market);
    // hash_n_to_one([a, b, c]) == two_to_one(two_to_one(a, b), c)
    let ab = Poseidon2Hash::two_to_one(reserved, md);
    Poseidon2Hash::two_to_one(ab, account_delta_tree_root)
}

/// Off-circuit replica of `BlobEvaluationCircuit::_get_pce_evaluation_point`.
///
/// Returns the challenge `x` as a canonical BLS12-381 scalar.
fn pce_evaluation_point(
    blob_data_hash: HashOut<F>,
    kzg_versioned_hash: &[u8; KECCAK_HASH_OUT_BYTE_SIZE],
) -> BLS12381Scalar {
    // hash_in = blob_data_hash.elements || kzg_versioned_hash bytes (as field elems)
    let mut hash_in: Vec<F> = Vec::with_capacity(4 + KECCAK_HASH_OUT_BYTE_SIZE);
    hash_in.extend_from_slice(&blob_data_hash.elements);
    for &b in kzg_versioned_hash.iter() {
        hash_in.push(F::from_canonical_u8(b));
    }
    let hash_out = Poseidon2Hash::hash_no_pad(&hash_in);

    // challenge_point_biguint: each of the 4 Goldilocks elements split into two
    // little-endian u32 limbs -> an 8-limb (256-bit) little-endian integer.
    let mut value = BigUint::zero();
    let mut shift = 0u32;
    for elem in hash_out.elements.iter() {
        let v = elem.to_canonical_u64();
        let lo = (v & 0xFFFF_FFFF) as u32;
        let hi = (v >> 32) as u32;
        value += BigUint::from(lo) << shift;
        shift += 32;
        value += BigUint::from(hi) << shift;
        shift += 32;
    }
    // reduce mod r
    let reduced = value % bls_order();
    BLS12381Scalar::from_noncanonical_biguint(reduced)
}

/// Off-circuit replica of `BlobPolynomialTarget::eval_at` for the specific
/// challenge `x` (barycentric evaluation in evaluation form over the
/// bit-reversal-permuted roots of unity).
///
/// `blob_scalars[i]` is the i-th 31-byte limb of `packed` interpreted as a
/// big-endian canonical scalar (matching `BlobPolynomialTarget::from_bytes`).
fn eval_blob_polynomial(
    blob_scalars: &[BLS12381Scalar; BLOB_WIDTH],
    x: BLS12381Scalar,
) -> BLS12381Scalar {
    let roots = brp_roots_of_unity();

    // If x is one of the roots of unity, the result is blob[i] directly.
    let mut result = BLS12381Scalar::ZERO;
    let mut barycentric = BLS12381Scalar::ZERO;
    let mut is_root = false;
    for i in 0..BLOB_WIDTH {
        let denom = x - roots[i];
        if denom.is_zero() {
            result = blob_scalars[i];
            is_root = true;
            // continue accumulating barycentric harmlessly; safe denom == 1
            continue;
        }
        // term_i = roots[i] * blob[i] / (x - roots[i])
        let term = roots[i] * blob_scalars[i] / denom;
        barycentric += term;
    }
    if is_root {
        return result;
    }

    // factor = (x^WIDTH - 1) / WIDTH
    let x_to_width = x.exp_u64(*CARDINALITY as u64);
    let width = BLS12381Scalar::from_canonical_u64(BLOB_WIDTH as u64);
    let factor = (x_to_width - BLS12381Scalar::ONE) / width;
    barycentric * factor
}

/// The bit-reversal-permuted roots of unity used by the circuit
/// (`get_brp_roots_of_unity_as_constant`), as plain-Rust scalars.
fn brp_roots_of_unity() -> Vec<BLS12381Scalar> {
    ROOTS_OF_UNITY
        .split(',')
        .map(|s| {
            let big = BigUint::from_str_radix(s, 10).unwrap();
            BLS12381Scalar::from_noncanonical_biguint(big)
        })
        .collect()
}

/// Interpret the packed blob as `BLOB_WIDTH` big-endian canonical scalars,
/// matching `BlobPolynomialTarget::from_bytes` (each 31-byte chunk prepended
/// with a zero byte).
fn packed_blob_to_scalars(
    packed: &[u8; BLOB_DATA_BYTES_COUNT],
) -> Box<[BLS12381Scalar; BLOB_WIDTH]> {
    let scalars: Vec<BLS12381Scalar> = packed
        .chunks(31)
        .map(|chunk| {
            // big-endian, MSB-padded with a 0 byte.
            let mut be = [0u8; 32];
            be[1..32].copy_from_slice(chunk);
            let big = BigUint::from_bytes_be(&be);
            BLS12381Scalar::from_noncanonical_biguint(big)
        })
        .collect();
    scalars
        .into_boxed_slice()
        .try_into()
        .expect("blob has BLOB_WIDTH scalars")
}

/// Serialize a canonical BLS12-381 scalar as a 32-byte big-endian array
/// (the witness encoding the circuit reads via `biguint_from_bytes_be`).
fn scalar_to_be32(s: &BLS12381Scalar) -> [u8; KECCAK_HASH_OUT_BYTE_SIZE] {
    let be = s.to_canonical_biguint().to_bytes_be();
    let mut out = [0u8; KECCAK_HASH_OUT_BYTE_SIZE];
    // left-pad
    out[KECCAK_HASH_OUT_BYTE_SIZE - be.len()..].copy_from_slice(&be);
    out
}

/// Compute the PCE opening `(x, y)` exactly as the in-circuit check expects.
///
/// Returns the 32-byte big-endian encodings of `x` and `y`.
pub fn compute_pce_opening(
    packed: &[u8; BLOB_DATA_BYTES_COUNT],
    market: &[MarketLimbs],
    account_delta_tree_root: HashOut<F>,
    kzg_versioned_hash: &[u8; KECCAK_HASH_OUT_BYTE_SIZE],
) -> (
    [u8; KECCAK_HASH_OUT_BYTE_SIZE],
    [u8; KECCAK_HASH_OUT_BYTE_SIZE],
) {
    let bdh = blob_data_hash(packed, market, account_delta_tree_root);
    let x = pce_evaluation_point(bdh, kzg_versioned_hash);
    let scalars = packed_blob_to_scalars(packed);
    let y = eval_blob_polynomial(&scalars, x);
    (scalar_to_be32(&x), scalar_to_be32(&y))
}

/// Produce a complete [`WrapperInput`] for the inner wrapper from a packed blob.
///
/// * `kzg_versioned_hash` is the EIP-4844 versioned hash of the BLS12-381 KZG
///   commitment to the (canonicalized) blob, via c-kzg.
/// * `(x, y)` is the custom-Poseidon2 PCE opening matching the in-circuit check.
/// * `batch_commitment` is supplied by the caller (the inner wrapper recomputes
///   and binds it as a public input; the off-circuit driver passes through the
///   value it computed alongside the batch).
#[allow(clippy::too_many_arguments)]
pub fn build_wrapper_input(
    packed: Box<[u8; BLOB_DATA_BYTES_COUNT]>,
    market: &[MarketLimbs],
    account_delta_tree_root: HashOut<F>,
    batch_commitment: [u8; KECCAK_HASH_OUT_BYTE_SIZE],
    setup_path: &str,
) -> Result<Box<WrapperInput>> {
    let canonical = packed_blob_to_canonical(&packed);
    let kvh = kzg_versioned_hash(&canonical, setup_path)?;
    let (x, y) = compute_pce_opening(&packed, market, account_delta_tree_root, &kvh);
    Ok(Box::new(WrapperInput {
        kzg_versioned_hash: kvh,
        batch_commitment,
        blob_bytes: packed,
        blob_polynomial_opening_x: x,
        blob_polynomial_opening_y: y,
    }))
}

/// Off-circuit replica of `WrapperInnerCircuit::_get_blob_pub_data_hash`:
/// Poseidon2 over the blob's account-leaf section (`BLOB_ACCOUNT_OFFSET..`),
/// packed 7 bytes per field element (big-endian), trailing chunk zero-padded.
pub fn blob_pub_data_hash(packed: &[u8; BLOB_DATA_BYTES_COUNT]) -> HashOut<F> {
    let mut elements: Vec<F> = Vec::new();
    for chunk in packed[BLOB_ACCOUNT_OFFSET..].chunks(7) {
        let mut res: u64 = 0;
        for &b in chunk {
            res = res.wrapping_mul(1 << 8).wrapping_add(b as u64);
        }
        elements.push(F::from_canonical_u64(res));
    }
    Poseidon2Hash::hash_no_pad(&elements)
}

/// The quintic evaluation point the inner wrapper binds the aggregated delta to
/// (`WrapperInnerCircuit::verify_aggregated_delta`):
/// `hash_two_to_one(blob_pub_data_hash, account_delta_tree_root)`, returned as a
/// [`HashOut`] (the delta witness lifts it to `QuinticExt([h0,h1,h2,h3,0])`).
///
/// This is the off-circuit derivation of the `--l6-inner` delta-chain
/// evaluation point: feeding any other value to the delta chain makes
/// `prove_inner` fail its `connect_quintic_ext` constraint.
pub fn wrapper_delta_evaluation_point(
    packed: &[u8; BLOB_DATA_BYTES_COUNT],
    account_delta_tree_root: HashOut<F>,
) -> HashOut<F> {
    let pdh = blob_pub_data_hash(packed);
    Poseidon2Hash::two_to_one(pdh, account_delta_tree_root)
}

/// Build a [`BlobEvaluation`] from a packed blob + market limbs + delta-tree root,
/// computing the KZG versioned hash and the matching PCE opening `(x, y)`.
///
/// This is the bridge used by `--blob-prove`: the resulting `BlobEvaluation` is
/// fed directly to `BlobEvaluationCircuit::prove`, whose in-circuit PCE check is
/// the correctness gate on the off-circuit `(x, y)`.
pub fn build_blob_evaluation(
    packed: Box<[u8; BLOB_DATA_BYTES_COUNT]>,
    market: &[MarketLimbs],
    account_delta_tree_root: HashOut<F>,
    public_market_details: [circuit::types::market_details::PublicMarketDetails;
        circuit::types::constants::POSITION_LIST_SIZE],
    setup_path: &str,
) -> Result<circuit::blob::blob_constraints::BlobEvaluation<F>> {
    let canonical = packed_blob_to_canonical(&packed);
    let kvh = kzg_versioned_hash(&canonical, setup_path)?;
    let (x, y) = compute_pce_opening(packed.as_ref(), market, account_delta_tree_root, &kvh);
    Ok(circuit::blob::blob_constraints::BlobEvaluation {
        kzg_versioned_hash: kvh,
        blob_bytes: packed,
        blob_polynomial_opening_x: x,
        blob_polynomial_opening_y: y,
        account_delta_tree_root,
        public_market_details,
    })
}

#[cfg(test)]
mod tests {
    use circuit::blob::blob_constraints::{BlobEvaluationCircuit, Circuit as _};
    use circuit::types::config::{C, CIRCUIT_CONFIG};
    use circuit::types::constants::{EMPTY_ACCOUNT_DELTA_TREE_ROOT, POSITION_LIST_SIZE};
    use circuit::types::market_details::PublicMarketDetails;

    use super::*;
    use crate::blob_encode::{empty_blob, empty_market_limbs};

    fn setup_path() -> String {
        // Tests run with CWD at the crate root (bench/); the asset lives at the
        // workspace root. Try both so `cargo test -p bench` and a
        // workspace-root invocation both work.
        for p in [
            super::DEFAULT_TRUSTED_SETUP_PATH,
            "assets/trusted_setup.txt",
            "../bench/assets/trusted_setup.txt",
        ] {
            if std::path::Path::new(p).exists() {
                return p.to_string();
            }
        }
        super::DEFAULT_TRUSTED_SETUP_PATH.to_string()
    }

    #[test]
    fn test_kzg_versioned_hash_is_versioned() {
        let blob = empty_blob();
        let canonical = packed_blob_to_canonical(&blob);
        let kvh =
            kzg_versioned_hash(&canonical, &setup_path()).expect("versioned hash over empty blob");
        // EIP-4844 versioned hash starts with the 0x01 version byte.
        assert_eq!(
            kvh[0], 0x01,
            "versioned hash must carry the 0x01 version byte"
        );
        // It must not be all-zero (a real SHA-256 of the commitment).
        assert!(
            kvh[1..].iter().any(|&b| b != 0),
            "versioned hash tail is degenerate"
        );
    }

    /// Acceptance criterion #2 + #3: the off-circuit KZG sidecar produces a
    /// `BlobEvaluation` whose `(x, y)` is accepted by the in-circuit PCE check,
    /// and `BlobEvaluationCircuit::prove` verifies. If `x` or `y` were wrong,
    /// the `connect_nonnative` constraints in `verify_pce_evaluation` would fail
    /// and `prove` would return an error — so this is a faithful gate, never a
    /// stub.
    ///
    /// Heavy: builds the full `BlobEvaluationCircuit` (4096 BLS12-381 nonnative
    /// field elements) and runs a real plonky2 prove (~7 min in debug). It is
    /// `#[ignore]`d so the default `cargo test` stays fast for CI; run it
    /// explicitly with a large stack:
    /// `RUST_MIN_STACK=4294967296 cargo test -p bench --lib -- --ignored test_blob_evaluation_prove`.
    /// The same path is also exercised end-to-end by the `--blob-prove` bench mode.
    #[test]
    #[ignore = "heavy plonky2 prove (~7 min debug); run with --ignored, see --blob-prove bench mode"]
    fn test_blob_evaluation_prove() {
        let blob = empty_blob();
        let market = empty_market_limbs();
        let public_market_details: [PublicMarketDetails; POSITION_LIST_SIZE] =
            core::array::from_fn(|_| PublicMarketDetails::default());

        let blob_eval = build_blob_evaluation(
            blob,
            &market,
            EMPTY_ACCOUNT_DELTA_TREE_ROOT,
            public_market_details,
            &setup_path(),
        )
        .expect("build blob evaluation");

        let circuit = BlobEvaluationCircuit::define(CIRCUIT_CONFIG);
        let data = circuit.builder.build::<C>();
        let proof = BlobEvaluationCircuit::prove(&data, &blob_eval, &circuit.target)
            .expect("blob evaluation proof must verify with sidecar-computed (x, y)");
        data.verify(proof)
            .expect("standalone blob-eval proof verifies");
    }
}
