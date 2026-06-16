// Copyright (c) Elliot Technologies, Inc.
// SPDX-License-Identifier: BUSL-1.1

//! Issue #243: NATIVE (off-circuit) leaf hashes for the four account-family
//! state trees — `account`, `account_pub_data`, `account_delta`, and `market`.
//!
//! ## Why this module exists
//!
//! To emit honest mid-block Merkle sibling-paths for an empty leaf index
//! (`EMPTY_ACCOUNT_INDEX = 2`) we must maintain an OFF-CIRCUIT sparse Merkle
//! tree (see [`crate::account_family_tree`]) whose leaves are hashed EXACTLY
//! as the in-circuit `*Target::hash` does — bit-for-bit. There is no native
//! `Account::hash` in the workspace today (only the in-circuit
//! `AccountTarget::hash`), so this module ports the closed-form hash to plain
//! Rust over `Poseidon2Hash`, mirroring [`crate::seed`]'s native-hashing
//! pattern.
//!
//! ## Faithfulness — how each native hash mirrors the circuit
//!
//! Every helper here is a line-for-line port of its in-circuit twin:
//!
//! - `hash_n_to_hash_no_pad::<Poseidon2Hash>(vec)` ⇒ `Poseidon2Hash::hash_no_pad(&[F])`.
//! - `builder.zero_hash_out()` ⇒ `HashOut::ZERO`.
//! - `builder.select_hash(is_empty, empty, non_empty)` ⇒ `if is_empty { ZERO } else { non_empty }`.
//! - BigInt (`BIG_U*_LIMBS` u32 limbs) is written by `set_bigint_target` as
//!   little-endian u32 abs limbs (`to_u32_digits`, zero-padded to the limb
//!   count) plus a sign field embedded `from_noncanonical_i64(-1|0|1)` — see
//!   `circuit::bigint::bigint::set_bigint_target`.
//! - BigIntU16 (`BIGU16_U*_LIMBS` u16 limbs) is written by
//!   `set_bigint_u16_target` as little-endian u16 abs limbs plus the same
//!   sign embedding — see `circuit::bigint::big_u16::bigint_u16`.
//! - The `Account::hash` empty special-case is `hash == EMPTY_ACCOUNT_HASH`
//!   AND NOT treasury (`account_index == TREASURY_ACCOUNT_INDEX`) — see
//!   `circuit::types::account::account_hash::is_empty`.
//!
//! These are VERIFIED bit-for-bit against the in-circuit hashes by cheap
//! one-shot extractor proves in the tests below (the same extractor pattern
//! `empty_witness.rs` uses), and the empty leaf is additionally checked to
//! equal `EMPTY_ACCOUNT_HASH` / `HashOut::ZERO`.

use circuit::poseidon2::Poseidon2Hash;
use circuit::types::account::Account;
use circuit::types::account_delta::AccountDelta;
use circuit::types::account_position::AccountPosition;
use circuit::types::config::{
    BIG_U96_LIMBS, BIG_U160_LIMBS, BIGU16_U64_LIMBS, F,
};
use circuit::types::constants::{
    EMPTY_ACCOUNT_HASH, POSITION_HASH_BUCKET_COUNT, POSITION_HASH_BUCKET_SIZE, POSITION_LIST_SIZE,
    TREASURY_ACCOUNT_INDEX,
};
use circuit::types::market::Market;
use num::bigint::Sign;
use num::{BigInt, BigUint, Signed};
use plonky2::field::types::{Field, Field64};
use plonky2::hash::hash_types::HashOut;
use plonky2::plonk::config::Hasher;

// ─── limb / sign encoders (mirror the circuit witness setters exactly) ──────

/// Push the little-endian u32 abs limbs of a `BigInt`, zero-padded to
/// `num_limbs`, mirroring `set_bigint_target` -> `set_biguint_target`
/// (`value.abs().to_u32_digits()` then `resize(num_limbs, 0)`).
fn push_bigint_abs_u32_limbs(elements: &mut Vec<F>, value: &BigInt, num_limbs: usize) {
    let abs = value.abs().to_biguint().unwrap_or_else(BigUint::default);
    let mut limbs = abs.to_u32_digits();
    assert!(
        limbs.len() <= num_limbs,
        "bigint abs has {} u32 limbs, exceeds {num_limbs}",
        limbs.len()
    );
    limbs.resize(num_limbs, 0);
    for limb in limbs {
        elements.push(F::from_canonical_u32(limb));
    }
}

/// Push the little-endian u16 abs limbs of a `BigInt`, zero-padded to
/// `num_limbs`, mirroring `set_bigint_u16_target` -> `set_biguint_u16_target`
/// (each u32 digit is split into `[lo16, hi16]`, then `resize(num_limbs, 0)`).
fn push_bigint_abs_u16_limbs(elements: &mut Vec<F>, value: &BigInt, num_limbs: usize) {
    let abs = value.abs().to_biguint().unwrap_or_else(BigUint::default);
    let mut limbs: Vec<u16> = abs
        .to_u32_digits()
        .iter()
        .flat_map(|d| [*d as u16, (*d >> 16) as u16])
        .collect();
    assert!(
        limbs.len() <= num_limbs,
        "bigint abs has {} u16 limbs, exceeds {num_limbs}",
        limbs.len()
    );
    limbs.resize(num_limbs, 0);
    for limb in limbs {
        elements.push(F::from_canonical_u16(limb));
    }
}

/// Push the little-endian u32 limbs of a `BigUint`, zero-padded to `num_limbs`
/// (mirrors `set_biguint_target`). Used for `l1_address` (BIG_U160_LIMBS).
fn push_biguint_u32_limbs(elements: &mut Vec<F>, value: &BigUint, num_limbs: usize) {
    let mut limbs = value.to_u32_digits();
    assert!(
        limbs.len() <= num_limbs,
        "biguint has {} u32 limbs, exceeds {num_limbs}",
        limbs.len()
    );
    limbs.resize(num_limbs, 0);
    for limb in limbs {
        elements.push(F::from_canonical_u32(limb));
    }
}

/// The sign field of a `BigInt`/`BigIntU16`, embedded exactly as
/// `set_bigint_target`: `from_noncanonical_i64(Plus=>1, Minus=>-1, NoSign=>0)`.
fn sign_field(value: &BigInt) -> F {
    let s = match value.sign() {
        Sign::Plus => 1i64,
        Sign::Minus => -1i64,
        Sign::NoSign => 0i64,
    };
    F::from_noncanonical_i64(s)
}

/// `select_hash(is_empty, ZERO, non_empty)`.
fn select_empty(is_empty: bool, non_empty: HashOut<F>) -> HashOut<F> {
    if is_empty { HashOut::ZERO } else { non_empty }
}

// ─── AccountPosition: append the in-circuit hash params (full + pub-data) ────

/// Native twin of `AccountPositionTarget::append_position_hash_params`
/// (`allocated_margin` abs u32 limbs (BIG_U96), its sign, then margin_mode,
/// entry_quote, initial_margin_fraction, total_order_count,
/// total_position_tied_order_count).
fn append_position_hash_params(p: &AccountPosition, elements: &mut Vec<F>) {
    push_bigint_abs_u32_limbs(elements, &p.allocated_margin, BIG_U96_LIMBS);
    elements.push(sign_field(&p.allocated_margin));
    elements.push(F::from_canonical_u8(p.margin_mode));
    elements.push(F::from_canonical_i64(p.entry_quote));
    elements.push(F::from_canonical_u16(p.initial_margin_fraction));
    elements.push(F::from_canonical_i64(p.total_order_count));
    elements.push(F::from_canonical_i64(p.total_position_tied_order_count));
}

/// Native twin of `AccountPositionTarget::append_position_pub_data_hash_params`
/// (last_funding_rate_prefix_sum abs u16 limbs (BIGU16_U64) + sign, then
/// position abs u16 limbs (BIGU16_U64) + sign).
fn append_position_pub_data_hash_params(p: &AccountPosition, elements: &mut Vec<F>) {
    push_bigint_abs_u16_limbs(elements, &p.last_funding_rate_prefix_sum, BIGU16_U64_LIMBS);
    elements.push(sign_field(&p.last_funding_rate_prefix_sum));
    push_bigint_abs_u16_limbs(elements, &p.position, BIGU16_U64_LIMBS);
    elements.push(sign_field(&p.position));
}

/// Native twin of `AccountTarget::get_position_bucket_hash`: pub-data bucket
/// hash first, then the full bucket hash with the pub-data hash prepended.
/// Returns `[full_hash, pub_data_hash]`.
fn position_bucket_hash(bucket: &[AccountPosition]) -> [HashOut<F>; 2] {
    let mut pub_data_params = Vec::new();
    for pos in bucket {
        append_position_pub_data_hash_params(pos, &mut pub_data_params);
    }
    let pub_data_bucket_hash = Poseidon2Hash::hash_no_pad(&pub_data_params);

    let mut hash_params = pub_data_bucket_hash.elements.to_vec();
    for pos in bucket {
        append_position_hash_params(pos, &mut hash_params);
    }
    [
        Poseidon2Hash::hash_no_pad(&hash_params),
        pub_data_bucket_hash,
    ]
}

/// Native twin of `AccountTarget::get_position_bucket_hashes`: extend the
/// `POSITION_LIST_SIZE` positions with ONE extra empty position, chunk into
/// `POSITION_HASH_BUCKET_SIZE` buckets, and split into `[full, pub_data]`
/// arrays of `POSITION_HASH_BUCKET_COUNT` buckets each.
fn position_bucket_hashes(
    positions: &[AccountPosition; POSITION_LIST_SIZE],
) -> [[HashOut<F>; POSITION_HASH_BUCKET_COUNT]; 2] {
    let mut positions_ext: Vec<AccountPosition> = positions.to_vec();
    positions_ext.push(AccountPosition::default());

    let bucket_hashes: Vec<[HashOut<F>; 2]> = positions_ext
        .chunks(POSITION_HASH_BUCKET_SIZE)
        .map(position_bucket_hash)
        .collect();
    assert_eq!(
        bucket_hashes.len(),
        POSITION_HASH_BUCKET_COUNT,
        "expected {POSITION_HASH_BUCKET_COUNT} position buckets"
    );
    [
        core::array::from_fn(|i| bucket_hashes[i][0]),
        core::array::from_fn(|i| bucket_hashes[i][1]),
    ]
}

// ─── Account: native (account_hash, account_pub_data_hash) ──────────────────

/// Native twin of `AccountTarget::partial_hash`. Returns
/// `[partial_hash, partial_hash_for_pub_data]`.
fn account_partial_hash(
    account: &Account<F>,
    position_bucket_hashes: &[[HashOut<F>; POSITION_HASH_BUCKET_COUNT]; 2],
) -> [HashOut<F>; 2] {
    // pub_data_elements: bucket[1] flattened, then per-share
    // [public_pool_index, share_amount], then [total_shares, operator_shares].
    let mut pub_data_elements: Vec<F> = Vec::new();
    for h in position_bucket_hashes[1].iter() {
        pub_data_elements.extend_from_slice(&h.elements);
    }
    for pps in account.public_pool_shares.iter() {
        pub_data_elements.push(F::from_canonical_i64(pps.public_pool_index));
        pub_data_elements.push(F::from_canonical_i64(pps.share_amount));
    }
    pub_data_elements.push(F::from_canonical_i64(account.public_pool_info.total_shares));
    pub_data_elements.push(F::from_canonical_i64(
        account.public_pool_info.operator_shares,
    ));
    let pub_data_elements_hash = Poseidon2Hash::hash_no_pad(&pub_data_elements);

    // elements: pub_data_elements_hash, bucket[0] flattened, per-share
    // [principal_amount, entry_timestamp], then [status,
    // min_operator_share_rate, operator_fee], then pending_unlocks hash.
    let mut elements: Vec<F> = pub_data_elements_hash.elements.to_vec();
    for h in position_bucket_hashes[0].iter() {
        elements.extend_from_slice(&h.elements);
    }
    for pps in account.public_pool_shares.iter() {
        elements.push(F::from_canonical_i64(pps.principal_amount));
        elements.push(F::from_canonical_i64(pps.entry_timestamp));
    }
    elements.push(F::from_canonical_u8(account.public_pool_info.status));
    elements.push(F::from_canonical_i64(
        account.public_pool_info.min_operator_share_rate,
    ));
    elements.push(F::from_canonical_i64(account.public_pool_info.operator_fee));

    // pending_unlocks hash: per unlock
    // [unlock_timestamp, asset_index, amount.limbs[0..3]] (the BIG_U96 u32
    // limbs of the BigUint amount). NB: the in-circuit code reads
    // `pw.amount.limbs[0..3]` directly, which is the BIG_U96 little-endian u32
    // layout.
    let mut pending_unlock_params: Vec<F> = Vec::new();
    for pw in account.pending_unlocks.iter() {
        pending_unlock_params.push(F::from_canonical_i64(pw.unlock_timestamp));
        pending_unlock_params.push(F::from_canonical_i64(pw.asset_index));
        push_biguint_u32_limbs(&mut pending_unlock_params, &pw.amount, BIG_U96_LIMBS);
    }
    let pending_unlocks_hash = Poseidon2Hash::hash_no_pad(&pending_unlock_params);
    elements.extend_from_slice(&pending_unlocks_hash.elements);

    [
        Poseidon2Hash::hash_no_pad(&elements),
        pub_data_elements_hash,
    ]
}

/// Native twin of `AccountTarget::hash_from_partial_hash`. Returns
/// `(account_hash, account_pub_data_hash, is_empty)`.
fn account_hash_from_partial(
    account: &Account<F>,
    partial_hash: &[HashOut<F>; 2],
) -> (HashOut<F>, HashOut<F>, bool) {
    // pub_data_elements: partial[1], l1_address u32 limbs (BIG_U160),
    // account_type, aggregated_balances_root.
    let mut pub_data_elements: Vec<F> = partial_hash[1].elements.to_vec();
    push_biguint_u32_limbs(&mut pub_data_elements, &account.l1_address, BIG_U160_LIMBS);
    pub_data_elements.push(F::from_canonical_u8(account.account_type));
    pub_data_elements.extend_from_slice(&account.aggregated_balances_root.elements);

    // elements: partial[0], master_account_index, l1_address u32 limbs,
    // account_type, collateral abs u32 limbs (BIG_U96) + sign, strategy_hash,
    // [total_order_count, total_non_cross_order_count, cancel_all_time,
    // account_trading_mode], then api_key_root, account_orders_root,
    // asset_root.
    let mut elements: Vec<F> = partial_hash[0].elements.to_vec();
    elements.push(F::from_canonical_i64(account.master_account_index));
    push_biguint_u32_limbs(&mut elements, &account.l1_address, BIG_U160_LIMBS);
    elements.push(F::from_canonical_u8(account.account_type));
    push_bigint_abs_u32_limbs(&mut elements, &account.collateral, BIG_U96_LIMBS);
    elements.push(sign_field(&account.collateral));

    // strategy_hash: per strategy [abs u32 limbs (BIG_U96), sign].
    let mut strategy_params: Vec<F> = Vec::new();
    for strategy in account.public_pool_info.strategies.iter() {
        push_bigint_abs_u32_limbs(&mut strategy_params, strategy, BIG_U96_LIMBS);
        strategy_params.push(sign_field(strategy));
    }
    let strategy_hash = Poseidon2Hash::hash_no_pad(&strategy_params);
    elements.extend_from_slice(&strategy_hash.elements);

    elements.push(F::from_canonical_i64(account.total_order_count));
    elements.push(F::from_canonical_i64(account.total_non_cross_order_count));
    elements.push(F::from_canonical_i64(account.cancel_all_time));
    elements.push(F::from_canonical_u8(account.account_trading_mode));

    for h in [
        &account.api_key_root,
        &account.account_orders_root,
        &account.asset_root,
    ] {
        elements.extend_from_slice(&h.elements);
    }

    let non_empty_hash = Poseidon2Hash::hash_no_pad(&elements);
    let non_empty_pub_data_hash = Poseidon2Hash::hash_no_pad(&pub_data_elements);

    // is_empty = (non_empty_hash == EMPTY_ACCOUNT_HASH) AND NOT treasury.
    let is_empty_account_hash = non_empty_hash == EMPTY_ACCOUNT_HASH;
    let is_treasury = account.account_index == TREASURY_ACCOUNT_INDEX as i64;
    let is_empty = is_empty_account_hash && !is_treasury;

    (
        select_empty(is_empty, non_empty_hash),
        select_empty(is_empty, non_empty_pub_data_hash),
        is_empty,
    )
}

/// Native twin of `AccountTarget::hash`. Returns
/// `(account_hash, account_pub_data_hash, is_empty)`. The `account` tree leaf
/// is the first; the `account_pub_data` tree leaf is the second.
pub fn account_hash_native(account: &Account<F>) -> (HashOut<F>, HashOut<F>, bool) {
    let pbh = position_bucket_hashes(&account.positions);
    let partial = account_partial_hash(account, &pbh);
    account_hash_from_partial(account, &partial)
}

/// Convenience: just the `account` tree leaf hash.
pub fn account_leaf_hash(account: &Account<F>) -> HashOut<F> {
    account_hash_native(account).0
}

/// Convenience: just the `account_pub_data` tree leaf hash.
pub fn account_pub_data_leaf_hash(account: &Account<F>) -> HashOut<F> {
    account_hash_native(account).1
}

// ─── AccountDelta: native leaf hash ─────────────────────────────────────────

/// Native twin of `AccountDeltaTarget::partial_hash`. The in-circuit version
/// concatenates per-share `[public_pool_index, shares_delta]`,
/// `[total_shares_delta, operator_shares_delta]`, the `l1_address` u32 limbs,
/// `account_type`, and the `position_delta_root`, hashes them, and substitutes
/// `ZERO` when EVERY contributing field is zero / the position_delta_root is
/// the empty-position-delta root.
fn account_delta_partial_hash(delta: &AccountDelta<F>) -> HashOut<F> {
    use circuit::types::constants::EMPTY_POSITION_DELTA_TREE_ROOT;

    let mut elements: Vec<F> = Vec::new();
    let mut all_empty = true;

    for share in delta.public_pool_shares_delta.iter() {
        // public_pool_index is a plain Target (from_canonical_i64); shares_delta
        // is a SignedTarget (from_noncanonical_i64 so negatives round-trip).
        elements.push(F::from_canonical_i64(share.public_pool_index));
        elements.push(F::from_noncanonical_i64(share.shares_delta));
        if share.shares_delta != 0 {
            all_empty = false;
        }
    }

    // total_shares_delta / operator_shares_delta are SignedTargets.
    elements.push(F::from_noncanonical_i64(
        delta.public_pool_info_delta.total_shares_delta,
    ));
    elements.push(F::from_noncanonical_i64(
        delta.public_pool_info_delta.operator_shares_delta,
    ));
    if delta.public_pool_info_delta.total_shares_delta != 0 {
        all_empty = false;
    }
    if delta.public_pool_info_delta.operator_shares_delta != 0 {
        all_empty = false;
    }

    // l1_address u32 limbs (BIG_U160) — each zero limb contributes an is_zero
    // flag in the circuit.
    let l1_limbs_before = elements.len();
    push_biguint_u32_limbs(&mut elements, &delta.l1_address, BIG_U160_LIMBS);
    if elements[l1_limbs_before..].iter().any(|f| *f != F::ZERO) {
        all_empty = false;
    }

    elements.push(F::from_canonical_u8(delta.account_type));
    if delta.account_type != 0 {
        all_empty = false;
    }

    elements.extend_from_slice(&delta.position_delta_root.elements);
    if delta.position_delta_root != EMPTY_POSITION_DELTA_TREE_ROOT {
        all_empty = false;
    }

    if all_empty {
        HashOut::ZERO
    } else {
        Poseidon2Hash::hash_no_pad(&elements)
    }
}

/// Native twin of `AccountDeltaTarget::hash`. Folds the partial hash with the
/// `asset_delta_root` and applies the empty special-case (partial_hash == ZERO
/// AND asset_delta_root == EMPTY_ASSET_TREE_ROOT).
pub fn account_delta_leaf_hash(delta: &AccountDelta<F>) -> HashOut<F> {
    use circuit::types::constants::EMPTY_ASSET_TREE_ROOT;

    let partial_hash = account_delta_partial_hash(delta);

    let mut elements: Vec<F> = partial_hash.elements.to_vec();
    elements.extend_from_slice(&delta.asset_delta_root.elements);
    let non_empty_hash = Poseidon2Hash::hash_no_pad(&elements);

    let is_empty =
        partial_hash == HashOut::ZERO && delta.asset_delta_root == EMPTY_ASSET_TREE_ROOT;
    select_empty(is_empty, non_empty_hash)
}

// ─── Market: native leaf hash (the simplest — flat 19-element) ──────────────

/// Native twin of `MarketTarget::hash`: a flat `hash_no_pad` over 19 elements
/// (15 scalar fields + the 4 `order_book_root` elements), with the empty
/// special-case (`is_empty` ⇒ ZERO) mirrored from `MarketTarget::is_empty`.
pub fn market_leaf_hash(market: &Market<F>) -> HashOut<F> {
    let elements = [
        F::from_canonical_u8(market.market_type),
        F::from_canonical_u8(market.status),
        F::from_canonical_u16(market.base_asset_id),
        F::from_canonical_u16(market.quote_asset_id),
        F::from_canonical_i64(market.ask_nonce),
        F::from_canonical_i64(market.bid_nonce),
        F::from_canonical_u32(market.taker_fee),
        F::from_canonical_u32(market.maker_fee),
        F::from_canonical_u32(market.liquidation_fee),
        F::from_canonical_u64(market.min_base_amount),
        F::from_canonical_u64(market.min_quote_amount),
        F::from_canonical_i64(market.order_quote_limit),
        F::from_canonical_i64(market.total_order_count),
        F::from_canonical_i64(market.size_extension_multiplier),
        F::from_canonical_i64(market.quote_extension_multiplier),
        market.order_book_root.elements[0],
        market.order_book_root.elements[1],
        market.order_book_root.elements[2],
        market.order_book_root.elements[3],
    ];
    let non_empty_hash = Poseidon2Hash::hash_no_pad(&elements);

    // is_empty mirrors MarketTarget::is_empty (all the listed scalar fields are
    // zero). order_book_root is NOT part of the empty check in the circuit.
    let is_empty = market.ask_nonce == 0
        && market.bid_nonce == 0
        && market.taker_fee == 0
        && market.maker_fee == 0
        && market.liquidation_fee == 0
        && market.min_base_amount == 0
        && market.min_quote_amount == 0
        && market.status == 0
        && market.order_quote_limit == 0
        && market.total_order_count == 0
        && market.market_type == 0
        && market.base_asset_id == 0
        && market.quote_asset_id == 0
        && market.size_extension_multiplier == 0
        && market.quote_extension_multiplier == 0;
    select_empty(is_empty, non_empty_hash)
}

#[cfg(test)]
mod tests {
    use super::*;
    use circuit::types::constants::{
        EMPTY_ACCOUNT_ORDERS_TREE_ROOT, EMPTY_API_KEY_TREE_ROOT, EMPTY_ASSET_TREE_ROOT,
        EMPTY_ORDER_BOOK_TREE_ROOT, EMPTY_POSITION_DELTA_TREE_ROOT, NIL_MARKET_INDEX,
    };

    // ── In-circuit extractor proves (cheap one-shot) ─────────────────────────
    //
    // Each extractor builds a tiny circuit that computes the IN-CIRCUIT hash,
    // registers it as a public input, proves over the given native value, and
    // reads the hash back. Comparing the extracted in-circuit hash to the
    // native hash is the bit-for-bit verification the issue requires. These are
    // intentionally minimal one-shot proves (no S=9 chunk, no sweep).

    fn extract_account_hashes(account: &Account<F>) -> (HashOut<F>, HashOut<F>) {
        use circuit::types::account::{AccountTarget, AccountTargetWitness};
        use circuit::types::config::{C, CIRCUIT_CONFIG};
        use plonky2::hash::hash_types::NUM_HASH_OUT_ELTS;

        let mut builder = circuit::builder::Builder::new(CIRCUIT_CONFIG);
        let target = AccountTarget::new(&mut builder);
        let pbh = target.get_position_bucket_hashes(&mut builder);
        let (acc_hash, pd_hash, _is_empty) = target.hash(&mut builder, &pbh);
        builder.register_public_hashout(acc_hash);
        builder.register_public_hashout(pd_hash);
        let data = builder.build::<C>();

        let mut pw = plonky2::iop::witness::PartialWitness::<F>::new();
        pw.set_account_target(&target, account).expect("set account");
        let proof = data.prove(pw).expect("account extractor proves");

        let pis = &proof.public_inputs;
        let acc = HashOut {
            elements: core::array::from_fn(|j| pis[j]),
        };
        let pd = HashOut {
            elements: core::array::from_fn(|j| pis[NUM_HASH_OUT_ELTS + j]),
        };
        (acc, pd)
    }

    fn extract_account_delta_hash(delta: &AccountDelta<F>) -> HashOut<F> {
        use circuit::types::account_delta::account_delta::{
            AccountDeltaTarget, AccountDeltaTargetWitness,
        };
        use circuit::types::config::{C, CIRCUIT_CONFIG};

        let mut builder = circuit::builder::Builder::new(CIRCUIT_CONFIG);
        let target = AccountDeltaTarget::new(&mut builder);
        let h = target.hash(&mut builder);
        builder.register_public_hashout(h);
        let data = builder.build::<C>();

        let mut pw = plonky2::iop::witness::PartialWitness::<F>::new();
        pw.set_account_delta_target(&target, delta)
            .expect("set account delta");
        let proof = data.prove(pw).expect("account delta extractor proves");
        HashOut {
            elements: core::array::from_fn(|j| proof.public_inputs[j]),
        }
    }

    fn extract_market_hash(market: &Market<F>) -> HashOut<F> {
        use circuit::types::config::{C, CIRCUIT_CONFIG};
        use circuit::types::market::{MarketTarget, MarketTargetWitness};

        let mut builder = circuit::builder::Builder::new(CIRCUIT_CONFIG);
        let target = MarketTarget::new(&mut builder);
        let h = target.hash(&mut builder);
        builder.register_public_hashout(h);
        let data = builder.build::<C>();

        let mut pw = plonky2::iop::witness::PartialWitness::<F>::new();
        pw.set_market_target(&target, market).expect("set market");
        let proof = data.prove(pw).expect("market extractor proves");
        HashOut {
            elements: core::array::from_fn(|j| proof.public_inputs[j]),
        }
    }

    fn empty_account() -> Account<F> {
        Account::<F> {
            account_index: 2,
            api_key_root: EMPTY_API_KEY_TREE_ROOT,
            account_orders_root: EMPTY_ACCOUNT_ORDERS_TREE_ROOT,
            asset_root: EMPTY_ASSET_TREE_ROOT,
            aggregated_balances_root: EMPTY_ASSET_TREE_ROOT,
            ..Account::<F>::default()
        }
    }

    /// The empty account leaf is the canonical empty-tree leaf: native hash is
    /// ZERO and equals EMPTY_ACCOUNT_HASH's select result.
    #[test]
    fn empty_account_native_leaf_is_zero() {
        let (acc, pd, is_empty) = account_hash_native(&empty_account());
        assert!(is_empty, "empty account must be flagged empty");
        assert_eq!(acc, HashOut::ZERO, "empty account leaf hash is ZERO");
        assert_eq!(pd, HashOut::ZERO, "empty account pub-data leaf hash is ZERO");
    }

    /// Bit-for-bit: the native empty-account hash equals the in-circuit hash.
    #[test]
    fn empty_account_native_matches_circuit() {
        let account = empty_account();
        let (n_acc, n_pd, _) = account_hash_native(&account);
        let (c_acc, c_pd) = extract_account_hashes(&account);
        assert_eq!(n_acc, c_acc, "account leaf hash native != circuit");
        assert_eq!(n_pd, c_pd, "account pub-data leaf hash native != circuit");
    }

    /// Bit-for-bit on a POPULATED account exercising every limb-encoded field
    /// (collateral sign, l1_address limbs, a non-empty position with a negative
    /// funding-rate prefix sum, public pool shares/info, a strategy, a pending
    /// unlock). This is the discriminating case for the limb encodings.
    #[test]
    fn populated_account_native_matches_circuit() {
        use circuit::types::pending_unlock::PendingUnlock;
        use circuit::types::public_pool::{PublicPoolInfo, PublicPoolShare};

        let mut account = Account::<F> {
            account_index: 7,
            master_account_index: 3,
            account_type: 1,
            account_trading_mode: 1,
            collateral: BigInt::from(-123_456_789i64),
            l1_address: BigUint::from(0x1234_5678_9abc_def0u64) << 64,
            total_order_count: 11,
            total_non_cross_order_count: 5,
            cancel_all_time: 99,
            api_key_root: EMPTY_API_KEY_TREE_ROOT,
            account_orders_root: EMPTY_ACCOUNT_ORDERS_TREE_ROOT,
            asset_root: EMPTY_ASSET_TREE_ROOT,
            aggregated_balances_root: EMPTY_ASSET_TREE_ROOT,
            ..Account::<F>::default()
        };
        account.aggregated_balances[0] = BigInt::from(42i64);
        account.aggregated_balances[1] = BigInt::from(-7i64);

        // One populated position (index 4) with signed bigint-u16 fields.
        account.positions[4] = AccountPosition {
            last_funding_rate_prefix_sum: BigInt::from(-987_654i64),
            position: BigInt::from(123_456i64),
            entry_quote: 555,
            initial_margin_fraction: 250,
            total_order_count: 2,
            total_position_tied_order_count: 1,
            margin_mode: 1,
            allocated_margin: BigInt::from(-9_000i64),
        };

        account.public_pool_shares[0] = PublicPoolShare {
            public_pool_index: 3,
            share_amount: 1000,
            principal_amount: 500,
            entry_timestamp: 1_700_000_000,
        };
        account.public_pool_info = PublicPoolInfo {
            status: 1,
            operator_fee: 10,
            min_operator_share_rate: 2,
            total_shares: 1000,
            operator_shares: 50,
            strategies: core::array::from_fn(|i| {
                if i == 0 {
                    BigInt::from(-321i64)
                } else {
                    BigInt::ZERO
                }
            }),
        };
        account.pending_unlocks[0] = PendingUnlock {
            unlock_timestamp: 1_700_000_500,
            asset_index: 0,
            amount: BigUint::from(424_242u64),
        };

        let (n_acc, n_pd, is_empty) = account_hash_native(&account);
        assert!(!is_empty, "populated account must not be empty");
        let (c_acc, c_pd) = extract_account_hashes(&account);
        assert_eq!(n_acc, c_acc, "populated account leaf hash native != circuit");
        assert_eq!(
            n_pd, c_pd,
            "populated account pub-data leaf hash native != circuit"
        );
    }

    fn empty_account_delta() -> AccountDelta<F> {
        AccountDelta::<F> {
            account_index: 2,
            asset_delta_root: EMPTY_ASSET_TREE_ROOT,
            position_delta_root: EMPTY_POSITION_DELTA_TREE_ROOT,
            ..AccountDelta::<F>::default()
        }
    }

    #[test]
    fn empty_account_delta_native_is_zero_and_matches_circuit() {
        let delta = empty_account_delta();
        let n = account_delta_leaf_hash(&delta);
        assert_eq!(n, HashOut::ZERO, "empty account-delta leaf hash is ZERO");
        let c = extract_account_delta_hash(&delta);
        assert_eq!(n, c, "empty account-delta native != circuit");
    }

    #[test]
    fn populated_account_delta_native_matches_circuit() {
        let mut delta = empty_account_delta();
        delta.account_type = 1;
        delta.l1_address = BigUint::from(0xdead_beefu64);
        delta.aggregated_asset_deltas[0] = BigInt::from(123i64);
        delta.aggregated_asset_deltas[1] = BigInt::from(-456i64);
        delta.public_pool_info_delta.total_shares_delta = 10;
        delta.public_pool_info_delta.operator_shares_delta = -3;

        let n = account_delta_leaf_hash(&delta);
        let c = extract_account_delta_hash(&delta);
        assert_eq!(n, c, "populated account-delta native != circuit");
    }

    fn empty_market() -> Market<F> {
        Market::<F> {
            market_index: NIL_MARKET_INDEX as u16,
            order_book_root: EMPTY_ORDER_BOOK_TREE_ROOT,
            ..Market::<F>::default()
        }
    }

    #[test]
    fn empty_market_native_is_zero_and_matches_circuit() {
        let market = empty_market();
        let n = market_leaf_hash(&market);
        assert_eq!(n, HashOut::ZERO, "empty market leaf hash is ZERO");
        let c = extract_market_hash(&market);
        assert_eq!(n, c, "empty market native != circuit");
    }

    #[test]
    fn populated_market_native_matches_circuit() {
        let market = Market::<F> {
            market_index: 3,
            market_type: 1,
            status: 1,
            base_asset_id: 2,
            quote_asset_id: 1,
            ask_nonce: 100,
            bid_nonce: 200,
            taker_fee: 30,
            maker_fee: 10,
            liquidation_fee: 50,
            size_extension_multiplier: 1000,
            quote_extension_multiplier: 2000,
            total_order_count: 7,
            min_base_amount: 5,
            min_quote_amount: 9,
            order_quote_limit: 1_000_000,
            order_book_root: EMPTY_ORDER_BOOK_TREE_ROOT,
        };
        let n = market_leaf_hash(&market);
        let c = extract_market_hash(&market);
        assert_eq!(n, c, "populated market native != circuit");
    }
}
