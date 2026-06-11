// Copyright (c) Elliot Technologies, Inc.
// SPDX-License-Identifier: BUSL-1.1

//! Issue #72 (cell slice A): witness-native base-proof seed derivation
//! for the tree-fold L2 driver.
//!
//! The tree-fold driver in `bench.rs` proves a LEAF chain proof per L1
//! chunk, each seeded with a fresh cyclic base proof. Chunk k's base
//! proof needs the state and validium roots BEFORE chunk k (the chain
//! circuit's `perform_sanity_checks` recomputes them from the chunk's
//! `old_*` witness inputs and connects them to the chained values).
//!
//! Before #72, chunk 0 took these from L3 (pre-exec) and chunk k > 0
//! took them from leaf k-1's PROVEN outputs -- a sequential seam that
//! blocked parallel leaf proving (cell slice B).
//!
//! This module computes those roots NATIVELY from witness data,
//! mirroring the in-circuit computation in
//! `circuit::block_tx_chain_constraints::perform_sanity_checks` (and
//! the analogous post-state computation done at the end of `define`).
//! The hashes use the same Poseidon2 permutation as the in-circuit
//! `hash_n_to_hash_no_pad::<Poseidon2Hash>` / `hash_n_to_one` helpers
//! (`Poseidon2Hash::hash_no_pad` for vector→hash, `two_to_one` for
//! the pairwise tree).

use circuit::poseidon2::Poseidon2Hash;
use circuit::types::asset::Asset;
use circuit::types::config::{BIG_U64_LIMBS, BIGU16_U64_LIMBS, F};
use circuit::types::constants::{
    ASSET_LIST_SIZE, MAX_ASSET_INDEX, MIN_ASSET_INDEX, POSITION_LIST_SIZE,
};
use circuit::types::market_details::MarketDetails;
use circuit::types::register::RegisterStack;
use circuit::types::state_metadata::StateMetadata;
use circuit::types::system_config::SystemConfig;
use num::bigint::Sign;
use plonky2::field::types::{Field, Field64};
use plonky2::hash::hash_types::HashOut;
use plonky2::plonk::config::Hasher;

/// Native equivalent of the in-circuit `hash_n_to_one` defined in
/// `circuit::hash_utils` (a left-fold of `hash_two_to_one`, which itself
/// calls `permute_swapped(swap=false)` -- semantically identical to
/// `Poseidon2Hash::two_to_one`).
fn hash_n_to_one(elements: &[HashOut<F>]) -> HashOut<F> {
    assert!(
        !elements.is_empty(),
        "hash_n_to_one needs at least one input"
    );
    if elements.len() == 1 {
        return elements[0];
    }
    let mut acc = Poseidon2Hash::two_to_one(elements[0], elements[1]);
    for e in &elements[2..] {
        acc = Poseidon2Hash::two_to_one(acc, *e);
    }
    acc
}

/// Native equivalent of `SystemConfigTarget::hash` -- a 4-field
/// `hash_no_pad` over (liquidity_pool_index, staking_pool_index,
/// liquidity_pool_cooldown_period, staking_pool_lockup_period). Field
/// embedding mirrors `set_system_config_target` (i64 -> from_canonical_i64).
pub fn system_config_hash(sc: &SystemConfig) -> HashOut<F> {
    Poseidon2Hash::hash_no_pad(&[
        F::from_canonical_i64(sc.liquidity_pool_index),
        F::from_canonical_i64(sc.staking_pool_index),
        F::from_canonical_i64(sc.liquidity_pool_cooldown_period),
        F::from_canonical_i64(sc.staking_pool_lockup_period),
    ])
}

/// Native equivalent of `RegisterStackTarget::hash`: concatenate every
/// register's hash parameters, hash them, and substitute the zero hash
/// when every register is empty (matches the in-circuit
/// `select_hash(is_empty, zero, non_empty_hash)` shortcut).
pub fn register_stack_hash(rs: &RegisterStack) -> HashOut<F> {
    let mut all_empty = true;
    let mut elements = Vec::new();
    for r in rs.iter() {
        elements.extend(r.get_hash_parameters());
        if !r.is_empty() {
            all_empty = false;
        }
    }
    if all_empty {
        HashOut::ZERO
    } else {
        Poseidon2Hash::hash_no_pad(&elements)
    }
}

/// Pack a 64-bit unsigned value into BIG_U64_LIMBS u32 limbs, mirroring
/// the BigUint witness layout used by `Asset` (`set_biguint_target`
/// resizes to `BIG_U64_LIMBS` zero limbs).
fn push_u64_as_u32_limbs(elements: &mut Vec<F>, value: u64) {
    // Native Asset stores its biguints as i64 -- the in-circuit limb
    // layout is little-endian u32 (`U32Target`). `set_biguint_target`
    // truncates beyond `min(actual_limbs, BIG_U64_LIMBS)` to zero
    // (`limbs.resize(BIG_U64_LIMBS, zero_u32())`), so 2 u32 limbs
    // covers the full 64-bit value.
    elements.push(F::from_canonical_u32(value as u32));
    elements.push(F::from_canonical_u32((value >> 32) as u32));
    assert_eq!(BIG_U64_LIMBS, 2);
}

/// Native equivalent of `Asset::get_hash_parameters`:
/// `[margin_mode, em_low, em_high, mta_low, mta_high, mwa_low, mwa_high]`.
fn asset_hash_parameters(a: &Asset) -> Vec<F> {
    let mut elements = Vec::with_capacity(1 + 3 * BIG_U64_LIMBS);
    elements.push(F::from_canonical_u8(a.margin_mode));
    // Asset's three BigUint witnesses are written as `BigUint::from_u64(value as u64)`
    // (see `set_asset_target` in circuit/src/types/asset.rs); negative
    // i64s are cast through the u64 round trip with sign preserved by
    // two's complement, but in practice these fields are non-negative.
    push_u64_as_u32_limbs(&mut elements, a.extension_multiplier as u64);
    push_u64_as_u32_limbs(&mut elements, a.min_transfer_amount as u64);
    push_u64_as_u32_limbs(&mut elements, a.min_withdrawal_amount as u64);
    elements
}

/// Native equivalent of `all_assets_hash` (asset.rs:299): iterate
/// `MIN_ASSET_INDEX..=MAX_ASSET_INDEX` (NOT the full slot range),
/// concatenate raw `get_hash_parameters` for each (no per-asset
/// empty-check shortcut here -- empty assets contribute zero limbs),
/// then hash with `hash_no_pad`.
pub fn all_assets_hash(assets: &[Asset; ASSET_LIST_SIZE]) -> HashOut<F> {
    let mut elements = Vec::with_capacity(
        (MAX_ASSET_INDEX - MIN_ASSET_INDEX + 1) as usize * (1 + 3 * BIG_U64_LIMBS),
    );
    for i in MIN_ASSET_INDEX..=MAX_ASSET_INDEX {
        elements.extend(asset_hash_parameters(&assets[i as usize]));
    }
    Poseidon2Hash::hash_no_pad(&elements)
}

/// Native equivalent of `MarketDetailsTarget::get_hash_parameters`,
/// preserving the exact field order and embedding used by
/// `set_market_details_target` (so the resulting hash matches the
/// in-circuit one bit-for-bit).
fn market_details_hash_parameters(md: &MarketDetails) -> [F; 16] {
    [
        F::from_canonical_u16(md.default_initial_margin_fraction),
        F::from_canonical_u16(md.min_initial_margin_fraction),
        F::from_canonical_u16(md.maintenance_margin_fraction),
        F::from_canonical_u16(md.close_out_margin_fraction),
        // aggregate_premium_sum is a SignedTarget -- `set_signed_target`
        // writes `F::from_noncanonical_i64(value)`. Mirror that here.
        F::from_noncanonical_i64(md.aggregate_premium_sum),
        F::from_canonical_u32(md.interest_rate),
        F::from_canonical_u32(md.impact_ask_price),
        F::from_canonical_u32(md.impact_bid_price),
        F::from_canonical_u32(md.impact_price),
        F::from_canonical_i64(md.open_interest),
        F::from_canonical_u32(md.index_price),
        F::from_canonical_u8(md.status),
        F::from_canonical_u32(md.funding_clamp_small),
        F::from_canonical_u32(md.funding_clamp_big),
        F::from_canonical_u64(md.open_interest_limit),
        F::from_canonical_u8(md.strategy_index),
    ]
}

/// Native equivalent of `all_market_details_hash` (market_details.rs:768).
pub fn all_market_details_hash(market_details: &[MarketDetails; POSITION_LIST_SIZE]) -> HashOut<F> {
    let mut elements = Vec::with_capacity(POSITION_LIST_SIZE * 16);
    for md in market_details.iter() {
        elements.extend(market_details_hash_parameters(md));
    }
    Poseidon2Hash::hash_no_pad(&elements)
}

/// Native equivalent of `all_public_market_details_hash`
/// (market_details.rs:779). Per market: 4 u16 limbs of
/// `funding_rate_prefix_sum.abs` (resized to `BIGU16_U64_LIMBS`,
/// matching `set_bigint_u16_target` -> `set_biguint_u16_target`), then
/// the sign (i64 0/+1/-1 via `F::from_noncanonical_i64`), mark_price
/// (u32), quote_multiplier (u32).
pub fn all_public_market_details_hash(
    market_details: &[MarketDetails; POSITION_LIST_SIZE],
) -> HashOut<F> {
    assert_eq!(BIGU16_U64_LIMBS, 4);
    let mut elements = Vec::with_capacity(POSITION_LIST_SIZE * (BIGU16_U64_LIMBS + 3));
    for md in market_details.iter() {
        let (sign, abs) = md.funding_rate_prefix_sum.clone().into_parts();
        // Little-endian u16 limbs of |funding_rate_prefix_sum|, padded
        // to BIGU16_U64_LIMBS limbs (matches
        // `set_biguint_u16_target`'s implicit zero-padding).
        let abs_u64: u64 = abs.try_into().unwrap_or_else(|e| {
            panic!(
                "funding_rate_prefix_sum.abs ({:?}) does not fit in u64: {:?}",
                md.funding_rate_prefix_sum, e
            )
        });
        elements.push(F::from_canonical_u16(abs_u64 as u16));
        elements.push(F::from_canonical_u16((abs_u64 >> 16) as u16));
        elements.push(F::from_canonical_u16((abs_u64 >> 32) as u16));
        elements.push(F::from_canonical_u16((abs_u64 >> 48) as u16));
        // Sign embedding mirrors `set_bigint_u16_target`: Plus->1,
        // Minus->-1, NoSign->0, written with `from_noncanonical_i64`.
        let sign_i64 = match sign {
            Sign::Plus => 1i64,
            Sign::Minus => -1i64,
            Sign::NoSign => 0i64,
        };
        elements.push(F::from_noncanonical_i64(sign_i64));
        elements.push(F::from_canonical_u32(md.mark_price));
        elements.push(F::from_canonical_u32(md.quote_multiplier));
    }
    Poseidon2Hash::hash_no_pad(&elements)
}

/// The pre-chunk seed roots that feed a tree-fold leaf's cyclic base
/// proof: the state and validium roots BEFORE the chunk, plus the
/// range-start `old_account_delta_tree_root` (the +4 PI introduced by
/// PR #69's tree-fold work).
///
/// Derived natively from witness data -- no proven outputs required,
/// so leaves are provable in any order (issue #72, cell slice A).
#[derive(Debug, Clone, Copy)]
pub struct ChunkSeed {
    pub pre_state_root: HashOut<F>,
    pub pre_validium_root: HashOut<F>,
    pub pre_delta_root: HashOut<F>,
}

/// Compute the (state_root, validium_root) pair for a snapshot of the
/// ledger -- mirrors the in-circuit recomputation in
/// `BlockTxChainCircuit::perform_sanity_checks` (the "old" branch, which
/// hashes the chunk's `old_*` witness inputs) and at the end of
/// `BlockTxChainCircuit::define` (the symmetric "new" branch). The two
/// branches share the same hash recipe, just over `old_*` vs `new_*`
/// inputs, so a single native helper covers both.
#[allow(clippy::too_many_arguments)]
pub fn compute_state_and_validium_roots(
    register_stack: &RegisterStack,
    account_tree_root: HashOut<F>,
    account_pub_data_tree_root: HashOut<F>,
    market_tree_root: HashOut<F>,
    all_assets: &[Asset; ASSET_LIST_SIZE],
    all_market_details: &[MarketDetails; POSITION_LIST_SIZE],
    state_metadata: &StateMetadata,
    system_config: &SystemConfig,
) -> (HashOut<F>, HashOut<F>) {
    let register_stack_hash = register_stack_hash(register_stack);
    let assets_hash = all_assets_hash(all_assets);
    let market_details_hash = all_market_details_hash(all_market_details);
    let public_market_details_hash = all_public_market_details_hash(all_market_details);
    let state_metadata_hash = state_metadata.hash();
    let system_config_hash = system_config_hash(system_config);

    // validium_root = hash_n_to_one([
    //     register_stack_hash, account_tree_root, market_tree_root,
    //     all_assets_hash, all_market_details_hash, state_metadata_hash,
    //     system_config_hash,
    // ])
    let validium_root = hash_n_to_one(&[
        register_stack_hash,
        account_tree_root,
        market_tree_root,
        assets_hash,
        market_details_hash,
        state_metadata_hash,
        system_config_hash,
    ]);

    // state_root = hash_n_to_one([
    //     account_pub_data_tree_root, all_public_market_details_hash, validium_root,
    // ])
    let state_root = hash_n_to_one(&[
        account_pub_data_tree_root,
        public_market_details_hash,
        validium_root,
    ]);

    (state_root, validium_root)
}

/// Roll the rolling chunk-input state forward by absorbing a chunk's
/// L1-proven outputs (`BlockTxWitness`) and emit a fresh `ChunkSeed`
/// for the NEXT chunk's leaf base proof. Used to precompute every
/// chunk's seed in a single sweep before any leaf is proven.
///
/// The witness data threaded here (`all_assets_after`,
/// `all_market_details_after`, `register_stack_after`,
/// `new_*_tree_root`, etc.) already lives in the rolling-state
/// variables the tree-fold driver maintains for each chunk; this
/// helper exists so callers can build the seed table in one place.
#[allow(clippy::too_many_arguments)]
pub fn seed_from_state(
    register_stack: &RegisterStack,
    account_tree_root: HashOut<F>,
    account_pub_data_tree_root: HashOut<F>,
    market_tree_root: HashOut<F>,
    account_delta_tree_root: HashOut<F>,
    all_assets: &[Asset; ASSET_LIST_SIZE],
    all_market_details: &[MarketDetails; POSITION_LIST_SIZE],
    state_metadata: &StateMetadata,
    system_config: &SystemConfig,
) -> ChunkSeed {
    let (state_root, validium_root) = compute_state_and_validium_roots(
        register_stack,
        account_tree_root,
        account_pub_data_tree_root,
        market_tree_root,
        all_assets,
        all_market_details,
        state_metadata,
        system_config,
    );
    ChunkSeed {
        pre_state_root: state_root,
        pre_validium_root: validium_root,
        pre_delta_root: account_delta_tree_root,
    }
}
