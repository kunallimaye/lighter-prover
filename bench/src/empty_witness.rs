// Copyright (c) Elliot Technologies, Inc.
// SPDX-License-Identifier: BUSL-1.1

//! Issue #129 (acceptance criterion #4 of #83): honest empty-genesis +
//! empty-tx witness construction for driving an L5 chain whose merged batch
//! has `new_account_delta_tree_root == EMPTY_ACCOUNT_DELTA_TREE_ROOT`.
//!
//! Everything in this module is built HONESTLY against the in-circuit
//! constraints — no fabricated roots, proofs, or commitments. The empty-tree
//! Merkle proofs are the all-empty-sibling paths derived from the protocol's
//! own empty-subtree hashes (`EMPTY_DELTA_TREE_HASHES` + the order-book tree's
//! `empty_hashes`), confirmed by `bench/tests/empty_tree_invariants.rs`.
//!
//! ## Why these are the canonical empty paths
//!
//! Every state tree in the protocol is a fixed-depth Poseidon2 Merkle tree with
//! a zero leaf and the `two_to_one(h, h)` fold, so the empty sibling at level
//! `i` is the same value for all trees: `EMPTY_DELTA_TREE_HASHES[i]` (for
//! `i <= ACCOUNT_MERKLE_LEVELS`) and the continued fold beyond that (used by the
//! 60-level account-orders tree). The order-book tree uses a distinct fold and
//! ships its own `OrderBookTree::empty_hashes`. All of this is asserted in the
//! invariants test against the pinned `EMPTY_*_TREE_ROOT` constants.

use circuit::block::Block;
use circuit::order_book_tree_helpers::OrderBookTree;
use circuit::poseidon2::Poseidon2Hash;
use circuit::tx::Tx;
use circuit::types::account::Account;
use circuit::types::account_asset::AccountAsset;
use circuit::types::account_delta::AccountDelta;
use circuit::types::account_order::AccountOrder;
use circuit::types::api_key::ApiKey;
use circuit::types::asset::Asset;
use circuit::types::config::F;
use circuit::types::constants::{
    ACCOUNT_MERKLE_LEVELS, ACCOUNT_ORDERS_MERKLE_LEVELS, API_KEY_MERKLE_LEVELS, ASSET_LIST_SIZE,
    ASSET_MERKLE_LEVELS, EMPTY_ACCOUNT_DELTA_TREE_ROOT, EMPTY_ACCOUNT_ORDERS_TREE_ROOT,
    EMPTY_API_KEY_TREE_ROOT, EMPTY_ASSET_TREE_ROOT, EMPTY_DELTA_TREE_HASHES,
    EMPTY_ORDER_BOOK_TREE_ROOT, EMPTY_POSITION_DELTA_TREE_ROOT, MARKET_MERKLE_LEVELS,
    NB_ACCOUNTS_PER_TX, NB_ASSETS_PER_TX, NIL_ASSET_INDEX, NIL_MARKET_INDEX,
    ORDER_BOOK_MERKLE_LEVELS, POSITION_LIST_SIZE, POSITION_MERKLE_LEVELS,
};
use circuit::types::market::Market;
use circuit::types::market_details::{MarketDetails, PublicMarketDetails};
use circuit::types::order::Order;
use circuit::types::order_book_node::OrderBookNode;
use circuit::types::state_metadata::StateMetadata;
use circuit::types::system_config::SystemConfig;
use plonky2::hash::hash_types::HashOut;
use plonky2::plonk::config::Hasher;

use crate::seed;

/// Empty-tree sibling hash at `level` for any zero-leaf `two_to_one(h, h)`
/// Merkle tree (account, account-pub-data, account-delta, asset, api-key,
/// position-delta, market, account-orders). For levels within the precomputed
/// `EMPTY_DELTA_TREE_HASHES` array we read it directly; beyond that (the
/// 60-level account-orders tree) we continue the identical fold.
fn empty_sibling(level: usize) -> HashOut<F> {
    if level <= ACCOUNT_MERKLE_LEVELS {
        EMPTY_DELTA_TREE_HASHES[level]
    } else {
        let mut h = EMPTY_DELTA_TREE_HASHES[ACCOUNT_MERKLE_LEVELS];
        for _ in ACCOUNT_MERKLE_LEVELS..level {
            h = Poseidon2Hash::two_to_one(h, h);
        }
        h
    }
}

/// All-empty-sibling Merkle proof of depth `L` (the path for index 0 in an
/// empty tree: each sibling is the empty subtree hash at that level).
fn empty_merkle_proof<const L: usize>() -> [HashOut<F>; L] {
    core::array::from_fn(empty_sibling)
}

/// The empty order-book tree's all-empty-sibling path of depth
/// `ORDER_BOOK_MERKLE_LEVELS` for any index (the generator's siblings are
/// index-independent for the empty tree).
fn empty_order_book_path() -> [OrderBookNode<F>; ORDER_BOOK_MERKLE_LEVELS] {
    let tree = OrderBookTree::<ORDER_BOOK_MERKLE_LEVELS>::new();
    tree.proof(0)
}

/// A non-special account index for empty leaves. Index 0 is the TREASURY and
/// index 1 the INSURANCE-FUND operator — both are NEVER treated as empty
/// (`account_hash::is_empty` excludes treasury), so an empty account must sit at
/// a normal index. For an all-empty tree every leaf is empty and every level's
/// sibling is the same empty-subtree hash, so the all-empty-sibling Merkle proof
/// reconstructs the empty root regardless of which (non-special) index we pick.
const EMPTY_ACCOUNT_INDEX: i64 = 2;

/// An empty account leaf whose hash is ZERO (matches the empty account-tree
/// leaf): zeroed account at a non-special index with the per-account sub-tree
/// roots set to their empty roots, exactly as `account_hash::empty_hash_check`
/// exercises (which also uses a non-zero account index).
fn empty_account() -> Account<F> {
    Account::<F> {
        account_index: EMPTY_ACCOUNT_INDEX,
        api_key_root: EMPTY_API_KEY_TREE_ROOT,
        account_orders_root: EMPTY_ACCOUNT_ORDERS_TREE_ROOT,
        asset_root: EMPTY_ASSET_TREE_ROOT,
        aggregated_balances_root: EMPTY_ASSET_TREE_ROOT,
        ..Account::<F>::default()
    }
}

/// Derive the empty account's `[partial_hash, partial_hash_for_pub_data]`
/// HONESTLY from the in-circuit `AccountTarget::partial_hash` computation
/// (the same Poseidon2 the fee-account hash path consumes). The fee account
/// (account[2]) is hashed via `fee_account_hash`, which reads these two hashes
/// straight from the witness rather than recomputing them; for an empty fee
/// account they must equal the empty account's partial hashes so the resulting
/// leaf hash is `EMPTY_ACCOUNT_HASH` (⇒ ZERO leaf), matching the other two
/// accounts that recompute via `hash()`.
///
/// One-shot: builds a tiny circuit, computes the partial hashes over an empty
/// `AccountTarget`, registers them as public inputs, proves, and reads them
/// back. No fabricated values — the hashes are produced by the circuit itself.
fn empty_account_partial_hashes() -> [HashOut<F>; 2] {
    use circuit::types::account::{AccountTarget, AccountTargetWitness};
    use circuit::types::config::{C, CIRCUIT_CONFIG};
    use plonky2::field::types::Field;
    use plonky2::hash::hash_types::NUM_HASH_OUT_ELTS;

    let mut builder = circuit::builder::Builder::new(CIRCUIT_CONFIG);
    let account = AccountTarget::new(&mut builder);
    let pbh = account.get_position_bucket_hashes(&mut builder);
    let partial = account.partial_hash(&mut builder, &pbh);
    for h in partial.iter() {
        builder.register_public_hashout(*h);
    }
    let data = builder.build::<C>();

    let mut pw = plonky2::iop::witness::PartialWitness::<F>::new();
    pw.set_account_target(&account, &empty_account())
        .expect("set empty account target");
    let proof = data.prove(pw).expect("partial-hash extractor proves");

    let pis = &proof.public_inputs;
    let mut out = [HashOut::<F>::ZERO; 2];
    for (i, slot) in out.iter_mut().enumerate() {
        let base = i * NUM_HASH_OUT_ELTS;
        slot.elements = core::array::from_fn(|j| pis[base + j]);
    }
    let _ = F::ZERO; // keep Field import used
    out
}

/// An empty FEE account leaf (account[2]) whose `fee_account_hash` yields the
/// empty leaf. Same empty account as `empty_account()` plus the precomputed
/// empty partial hashes that the fee-account hash path consumes from the
/// witness.
fn empty_fee_account(partial_hashes: [HashOut<F>; 2]) -> Account<F> {
    Account::<F> {
        partial_hash: partial_hashes[0],
        partial_hash_for_pub_data: partial_hashes[1],
        ..empty_account()
    }
}

/// An empty account-delta leaf whose hash is ZERO: zeroed delta with the
/// per-delta sub-tree roots set to their empty roots.
fn empty_account_delta() -> AccountDelta<F> {
    AccountDelta::<F> {
        account_index: EMPTY_ACCOUNT_INDEX,
        asset_delta_root: EMPTY_ASSET_TREE_ROOT,
        position_delta_root: EMPTY_POSITION_DELTA_TREE_ROOT,
        ..AccountDelta::<F>::default()
    }
}

/// Derive the empty account-delta's `partial_hash` HONESTLY from the in-circuit
/// `AccountDeltaTarget::partial_hash` (the same Poseidon2 the fee-account-delta
/// hash path consumes from the witness). For the empty fee delta this must
/// equal the empty delta's partial hash so `fee_account_hash` yields the empty
/// (ZERO) leaf. One-shot extractor; no fabricated values.
fn empty_account_delta_partial_hash() -> HashOut<F> {
    use circuit::types::account_delta::account_delta::{
        AccountDeltaTarget, AccountDeltaTargetWitness,
    };
    use circuit::types::config::{C, CIRCUIT_CONFIG};
    use plonky2::hash::hash_types::NUM_HASH_OUT_ELTS;

    let mut builder = circuit::builder::Builder::new(CIRCUIT_CONFIG);
    let delta = AccountDeltaTarget::new(&mut builder);
    let partial = delta.partial_hash(&mut builder);
    builder.register_public_hashout(partial);
    let data = builder.build::<C>();

    let mut pw = plonky2::iop::witness::PartialWitness::<F>::new();
    // partial_hash() reads position_delta_root from the target; the empty delta
    // sets it to EMPTY_POSITION_DELTA_TREE_ROOT so the partial hash is empty.
    pw.set_account_delta_target(&delta, &empty_account_delta())
        .expect("set empty account delta target");
    let proof = data.prove(pw).expect("delta partial-hash extractor proves");

    let mut out = HashOut::<F>::ZERO;
    out.elements = core::array::from_fn(|j| proof.public_inputs[j]);
    let _ = NUM_HASH_OUT_ELTS;
    out
}

/// The empty FEE account delta (account[2]). The fee-account-delta hash path
/// (`fee_account_hash`) reads `partial_hash` straight from the witness, so it
/// must carry the empty delta's partial hash.
fn empty_fee_account_delta(partial_hash: HashOut<F>) -> AccountDelta<F> {
    AccountDelta::<F> {
        partial_hash,
        ..empty_account_delta()
    }
}

/// The empty market leaf (`is_empty == true` ⇒ leaf hash ZERO). The
/// `order_book_root` is the empty order-book root, and `market_index` selects
/// the always-empty market slot used for empty transactions.
fn empty_market() -> Market<F> {
    // market_index = NIL_MARKET_INDEX (255) ⇒ the in-circuit perps_market_index
    // resolves to NIL (255 > MAX_PERPS_MARKET_INDEX=254), so the empty market is
    // not treated as a perps market and the position-delta path uses NIL_MARKET_INDEX.
    Market::<F> {
        market_index: NIL_MARKET_INDEX as u16,
        order_book_root: EMPTY_ORDER_BOOK_TREE_ROOT,
        ..Market::<F>::default()
    }
}

/// Build the empty `TX_TYPE_EMPTY` transaction. Every state leaf is the empty
/// leaf and every Merkle proof is the all-empty-sibling path, so the in-circuit
/// verify path (which runs in full even for an empty tx) checks each leaf
/// against the empty roots and `recalculate_root` returns the same empty root
/// (the empty tx mutates nothing).
pub fn empty_tx() -> Tx<F> {
    let fee_partial = empty_account_partial_hashes();
    let fee_delta_partial = empty_account_delta_partial_hash();
    empty_tx_with_fee_partial(fee_partial, fee_delta_partial)
}

/// Like [`empty_tx`] but with the (expensive-to-derive) empty fee-account
/// account + delta partial hashes supplied by the caller, so a batch of empty
/// txs can reuse a single derivation.
pub fn empty_tx_with_fee_partial(
    fee_partial: [HashOut<F>; 2],
    fee_delta_partial: HashOut<F>,
) -> Tx<F> {
    use circuit::types::constants::FEE_ACCOUNT_ID;

    let accounts_before: [Account<F>; NB_ACCOUNTS_PER_TX] = core::array::from_fn(|i| {
        if i == FEE_ACCOUNT_ID {
            empty_fee_account(fee_partial)
        } else {
            empty_account()
        }
    });
    let accounts_delta_before: [AccountDelta<F>; NB_ACCOUNTS_PER_TX] = core::array::from_fn(|i| {
        if i == FEE_ACCOUNT_ID {
            empty_fee_account_delta(fee_delta_partial)
        } else {
            empty_account_delta()
        }
    });

    // Account assets: nil asset for the tx asset slot, distinct fee asset slot.
    // `validate_asset_indices` requires asset_indices[TX]=NIL or != fee, and
    // connects asset_indices to account_assets_before[*][*].index_0.
    let asset_indices: [i16; NB_ASSETS_PER_TX] = [NIL_ASSET_INDEX as i16, NIL_ASSET_INDEX as i16];
    let account_assets_before: [[AccountAsset; NB_ASSETS_PER_TX]; NB_ACCOUNTS_PER_TX] =
        core::array::from_fn(|_| {
            core::array::from_fn(|k| AccountAsset::empty(asset_indices[k] as i64))
        });

    let account_tree_merkle_proofs: [[HashOut<F>; ACCOUNT_MERKLE_LEVELS]; NB_ACCOUNTS_PER_TX] =
        core::array::from_fn(|_| empty_merkle_proof::<ACCOUNT_MERKLE_LEVELS>());
    let asset_tree_merkle_proofs: [[[HashOut<F>; ASSET_MERKLE_LEVELS]; NB_ASSETS_PER_TX];
        NB_ACCOUNTS_PER_TX] = core::array::from_fn(|_| {
        core::array::from_fn(|_| empty_merkle_proof::<ASSET_MERKLE_LEVELS>())
    });

    Tx::<F> {
        tx_type: circuit::types::constants::TX_TYPE_EMPTY,
        nonce: 0,
        expired_at: 0,
        taker_fee: 0,
        maker_fee: 0,
        signature: Default::default(),
        l1_signature: None,
        l1_pub_key: None,

        // Per-tx-type payloads are unused for an empty tx; zero-init via Default.
        l1_deposit_tx: Default::default(),
        l1_create_market_tx: Default::default(),
        l1_update_market_tx: Default::default(),
        l1_cancel_all_orders_tx: Default::default(),
        l1_withdraw_tx: Default::default(),
        l1_create_order_tx: Default::default(),
        l1_change_pub_key_tx: Default::default(),
        l1_burn_shares_tx: Default::default(),
        l1_register_asset_tx: Default::default(),
        l1_set_system_config_tx: Default::default(),
        l1_update_asset_tx: Default::default(),
        l2_change_pub_key_tx: Default::default(),
        l2_create_sub_account_tx: Default::default(),
        l2_create_public_pool_tx: Default::default(),
        l2_update_public_pool_tx: Default::default(),
        l2_transfer_tx: Default::default(),
        l2_withdraw_tx: Default::default(),
        l2_create_order_tx: Default::default(),
        l2_cancel_order_tx: Default::default(),
        l2_cancel_all_orders_tx: Default::default(),
        l2_modify_order_tx: Default::default(),
        l2_mint_shares_tx: Default::default(),
        l2_burn_shares_tx: Default::default(),
        l2_update_leverage_tx: Default::default(),
        l2_create_grouped_orders_tx: Default::default(),
        l2_update_margin_tx: Default::default(),
        l2_create_staking_pool_tx: Default::default(),
        l2_stake_assets_tx: Default::default(),
        l2_unstake_assets_tx: Default::default(),
        l2_strategy_transfer_tx: Default::default(),
        l2_update_market_config_tx: Default::default(),
        l2_force_burn_shares_tx: Default::default(),
        l2_update_account_config_tx: Default::default(),
        internal_claim_order_tx: Default::default(),
        internal_cancel_order_tx: Default::default(),
        internal_deleverage_tx: Default::default(),
        internal_exit_position_tx: Default::default(),
        internal_pending_unlock_tx: Default::default(),
        internal_cancel_all_orders_tx: Default::default(),
        internal_liquidate_position_tx: Default::default(),
        internal_create_order_tx: Default::default(),

        // State tree leaves.
        api_key_before: ApiKey::<F>::default(),
        account_order_before: AccountOrder::default(),
        accounts_before,
        accounts_delta_before,
        market_before: empty_market(),
        order_before: Order::default(),
        account_assets_before,
        asset_indices,

        // State tree Merkle proofs (all-empty-sibling paths).
        account_tree_merkle_proofs,
        account_pub_data_tree_merkle_proofs: account_tree_merkle_proofs,
        account_delta_tree_merkle_proofs: account_tree_merkle_proofs,
        asset_tree_merkle_proofs,
        public_asset_tree_merkle_proofs: asset_tree_merkle_proofs,
        asset_delta_tree_merkle_proofs: asset_tree_merkle_proofs,
        position_delta_merkle_proofs: core::array::from_fn(|_| {
            empty_merkle_proof::<POSITION_MERKLE_LEVELS>()
        }),
        api_key_tree_merkle_proof: empty_merkle_proof::<API_KEY_MERKLE_LEVELS>(),
        account_orders_tree_merkle_proof: core::array::from_fn(|_| {
            empty_merkle_proof::<ACCOUNT_ORDERS_MERKLE_LEVELS>()
        }),
        market_tree_merkle_proof: empty_merkle_proof::<MARKET_MERKLE_LEVELS>(),
        order_book_tree_path: empty_order_book_path(),

        // Impact-price helpers (perps-only; empty market is not perps).
        impact_ask_order: Order::default(),
        impact_bid_order: Order::default(),
        impact_ask_order_book_tree_path: empty_order_book_path(),
        impact_bid_order_book_tree_path: empty_order_book_path(),
    }
}

/// Build the fully-empty genesis `Block<F>` with `tx_count` empty txs. All
/// trees are empty (roots = the EMPTY_*_TREE_ROOT constants), and the
/// state/validium roots are computed natively via the `seed.rs` recipe so the
/// L1→L5 chain stitches.
pub fn empty_genesis_block(tx_count: usize, block_number: u64, created_at: i64) -> Block<F> {
    let all_assets: [Asset; ASSET_LIST_SIZE] = core::array::from_fn(|i| Asset::empty(i as i16));
    let all_market_details: [MarketDetails; POSITION_LIST_SIZE] =
        core::array::from_fn(|_| MarketDetails::default());
    let new_public_market_details: [PublicMarketDetails; POSITION_LIST_SIZE] =
        core::array::from_fn(|_| PublicMarketDetails::default());

    let register_stack_before = Default::default();
    let old_system_config = SystemConfig::default();
    let state_metadata = StateMetadata::default();

    let old_account_tree_root = empty_sibling(ACCOUNT_MERKLE_LEVELS);
    let old_account_pub_data_tree_root = empty_sibling(ACCOUNT_MERKLE_LEVELS);
    let old_account_delta_tree_root = EMPTY_ACCOUNT_DELTA_TREE_ROOT;
    let old_market_tree_root = empty_sibling(MARKET_MERKLE_LEVELS);

    // Native state/validium roots over the empty genesis (issue #72 recipe).
    let (old_state_root, old_validium_root) = seed::compute_state_and_validium_roots(
        &register_stack_before,
        old_account_tree_root,
        old_account_pub_data_tree_root,
        old_market_tree_root,
        &all_assets,
        &all_market_details,
        &state_metadata,
        &old_system_config,
    );

    // Empty tx mutates nothing, so new_* == old_*. Derive the fee-account
    // partial hashes once and reuse for every empty tx in the block.
    let fee_partial = empty_account_partial_hashes();
    let fee_delta_partial = empty_account_delta_partial_hash();
    let txs: Vec<Tx<F>> = (0..tx_count)
        .map(|_| empty_tx_with_fee_partial(fee_partial, fee_delta_partial))
        .collect();

    Block::<F> {
        created_at,
        block_number,
        register_stack_before,
        old_system_config,
        all_market_details,
        all_assets,
        new_public_market_details,
        price_updates: Default::default(),
        calculate_premium: false,
        calculate_funding: false,
        calculate_oracle_prices: false,
        old_account_tree_root,
        old_account_pub_data_tree_root,
        old_market_tree_root,
        state_metadata,
        old_state_root,
        old_account_delta_tree_root,
        new_validium_root: old_validium_root,
        new_state_root: old_state_root,
        new_account_delta_tree_root: old_account_delta_tree_root,
        on_chain_operations_count: 0,
        on_chain_operations_pub_data: vec![],
        priority_operations_count: 0,
        old_prefix_priority_operation_hash: [0u8; 32],
        new_prefix_priority_operation_hash: [0u8; 32],
        txs,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn empty_paths_reconstruct_empty_roots() {
        // The all-empty-sibling account path folds back to the empty account
        // delta tree root (index 0, all path bits 0).
        let proof = empty_merkle_proof::<ACCOUNT_MERKLE_LEVELS>();
        let mut h = EMPTY_DELTA_TREE_HASHES[0]; // empty leaf (ZERO)
        for sib in proof.iter() {
            h = Poseidon2Hash::two_to_one(h, *sib);
        }
        assert_eq!(h, EMPTY_ACCOUNT_DELTA_TREE_ROOT);
    }

    #[test]
    fn empty_tx_builds() {
        let tx = empty_tx();
        assert!(tx.is_empty());
        assert_eq!(tx.market_before.order_book_root, EMPTY_ORDER_BOOK_TREE_ROOT);
    }

    #[test]
    fn empty_block_builds() {
        let block = empty_genesis_block(1, 1, 1);
        assert_eq!(
            block.old_account_delta_tree_root,
            EMPTY_ACCOUNT_DELTA_TREE_ROOT
        );
        assert_eq!(
            block.new_account_delta_tree_root,
            EMPTY_ACCOUNT_DELTA_TREE_ROOT
        );
        assert_eq!(block.txs.len(), 1);
    }

    /// Heavy honesty checkpoint: the empty-genesis empty-tx witness must verify
    /// through the L1 `BlockTxCircuit` (the first and cheapest real prove in the
    /// L1→L5 stack). A real prove — plonky2 panics on any unsatisfied
    /// constraint, so a successful verify means every empty-tree Merkle proof
    /// and leaf hash is honestly consistent.
    ///
    /// `#[ignore]`d (heavy); run with:
    /// `RUST_MIN_STACK=4294967296 cargo test -p bench --lib --release -- --ignored empty_tx_l1_proves`
    #[test]
    #[ignore = "heavy plonky2 prove; run with --ignored"]
    fn empty_tx_l1_proves() {
        use circuit::block_tx::BlockTx;
        use circuit::block_tx_constraints::{BlockTxCircuit, Circuit as _};
        use circuit::types::config::{C, CIRCUIT_CONFIG};

        const CHAIN_ID: u32 = 304;

        let circuit = BlockTxCircuit::define(CIRCUIT_CONFIG, 1, CHAIN_ID);
        let target = circuit.target;
        let data = circuit.builder.build::<C>();

        let block = empty_genesis_block(1, 1, 1);
        let block_tx = BlockTx::<F> {
            created_at: block.created_at,
            old_system_config: block.old_system_config,
            register_stack_before: block.register_stack_before,
            all_assets_before: block.all_assets.clone(),
            all_market_details_before: block.all_market_details.clone(),
            old_account_tree_root: block.old_account_tree_root,
            old_account_pub_data_tree_root: block.old_account_pub_data_tree_root,
            old_account_delta_tree_root: block.old_account_delta_tree_root,
            old_market_tree_root: block.old_market_tree_root,
            txs: block.txs.clone(),
        };

        let proof = BlockTxCircuit::prove(&data, &block_tx, &target).expect("L1 empty-tx proves");
        data.verify(proof).expect("L1 empty-tx proof verifies");
    }
}
