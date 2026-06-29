// Copyright (c) Elliot Technologies, Inc.
// SPDX-License-Identifier: BUSL-1.1

//! Per-transaction POSITIONAL pre-state snapshots — the READ path (issue #316).
//!
//! ## What this is (and what it deliberately is NOT)
//!
//! This is the trimmed, READ-ONLY port of the per-tx positional pre-state types
//! from `parallel-v0.0.1-alpha`. It carries ONLY the in-memory snapshot types a
//! consumer needs to look up a chunk's pre-state and build its L1 `BlockTx`:
//! [`ChunkPreState`], [`EmptyIndexSiblingPaths`] and [`PreStateSnapshots`].
//!
//! The OFFLINE GENERATOR — the serial S=1 sweep that PROVES every single-tx
//! step to roll ledger state forward (`sweep_per_tx_snapshots*`), the adaptive
//! empty-index sibling-path harvester (`capture_empty_paths` and the
//! `account_family_*` / `empty_witness` helpers it needs) — is NOT ported here.
//! It lives on `parallel-v0.0.1-alpha` and is only needed to MINT a NEW corpus.
//! Replaying the SAME committed block (issue #316's goal) needs only the read
//! path plus the committed dataset (`bench/corpus/cap-block/captured_corpus.gz`).
//!
//! ## The positional model (issue #177, Decision 1)
//!
//! Pre-state is a property of a POSITION in the tx sequence, not of a chunk.
//! Chunk boundaries are an overlay the coordinator chooses via `S = tx_per_proof`
//! at dispatch time. We snapshot the 8-field ledger pre-state at EVERY
//! transaction position (the state having applied txs `0..N`), so that for ANY
//! chunk size `S`, chunk `k`'s pre-state is simply `snapshot[S * k]`
//! ([`PreStateSnapshots::at_chunk`]). This keeps the corpus S-INDEPENDENT — the
//! same per-tx array serves every chunk size without regeneration.

use circuit::block_tx::BlockTx;
use circuit::tx::Tx;
use circuit::types::asset::Asset;
use circuit::types::config::F;
use circuit::types::constants::{ASSET_LIST_SIZE, POSITION_LIST_SIZE};
use circuit::types::market_details::MarketDetails;
use circuit::types::register::RegisterStack;
use circuit::types::system_config::SystemConfig;
use plonky2::hash::hash_types::HashOut;

/// A snapshot of the 8 ledger pre-state fields the L1 `BlockTx` constructor
/// needs, captured at one POSITION in the tx sequence (the state having
/// applied txs `0..position`).
///
/// These are the SAME 8 fields the single-process tree-fold path snapshots
/// (`bench/src/bin/bench.rs` `ChunkPreState`, issue #72), promoted to the
/// library so the distributed cell fix and the offline corpus generator share
/// one definition.
#[derive(Debug, Clone)]
pub struct ChunkPreState {
    pub register_stack: RegisterStack,
    pub all_assets: [Asset; ASSET_LIST_SIZE],
    pub all_market_details: [MarketDetails; POSITION_LIST_SIZE],
    pub system_config: SystemConfig,
    pub account_tree_root: HashOut<F>,
    pub account_pub_data_tree_root: HashOut<F>,
    pub account_delta_tree_root: HashOut<F>,
    pub market_tree_root: HashOut<F>,

    /// Issue #243: OPTIONAL honest mid-block sibling-paths for an
    /// adaptively-chosen empty leaf index against THIS position's
    /// account-family trees. `None` for roots-only snapshots; `Some` when the
    /// corpus was produced by the path-capturing sweep (the committed cap-block
    /// corpus is schema-1.1 and carries paths at positions `0..=tx_count-1`).
    /// These feed the empty padding txs that pad the final chunk to a full `S`.
    pub empty_index_sibling_paths: Option<EmptyIndexSiblingPaths>,
}

/// The four honest empty-index account-family sibling-paths captured at one tx
/// position, together with the ADAPTIVELY-CHOSEN empty leaf indices they belong
/// to. Each path is leaf-first (the `merkle_helpers::recalculate_root` fold
/// order). The account / account_pub_data / account_delta trees are depth
/// `ACCOUNT_MERKLE_LEVELS = 48`; the market tree is depth
/// `MARKET_MERKLE_LEVELS = 12`.
///
/// ## Why the indices are carried (issue #263)
///
/// The empty index is not the fixed constant 2 (whose untouched neighbouring
/// subtrees could never be harvested). Instead, each position's empty index is
/// derived from a real touched account's coherent proof — a guaranteed-empty
/// leaf in the descended empty subtree. The three account-family trees share
/// one `account_index`; the market tree has its own `market_index`. (The
/// generator that DERIVES these lives on `parallel-v0.0.1-alpha`; this read-path
/// port only carries the values back out of the committed corpus.)
#[derive(Debug, Clone)]
pub struct EmptyIndexSiblingPaths {
    /// The shared empty leaf index for the account / pub_data / delta trees.
    pub account_index: u128,
    /// The empty leaf index for the market tree.
    pub market_index: u128,
    pub account: [HashOut<F>; circuit::types::constants::ACCOUNT_MERKLE_LEVELS],
    pub account_pub_data: [HashOut<F>; circuit::types::constants::ACCOUNT_MERKLE_LEVELS],
    pub account_delta: [HashOut<F>; circuit::types::constants::ACCOUNT_MERKLE_LEVELS],
    pub market: [HashOut<F>; circuit::types::constants::MARKET_MERKLE_LEVELS],
}

impl ChunkPreState {
    /// Build the L1 `BlockTx` for a chunk from THIS positional pre-state plus
    /// the chunk's tx slice. This is the seam the distributed leaf proving now
    /// uses instead of block-initial / prefix-replayed state — identical
    /// regardless of who fills the pre-state in (offline generator there; corpus
    /// read here).
    pub fn block_tx(&self, created_at: i64, txs: Vec<Tx<F>>) -> BlockTx<F> {
        BlockTx {
            created_at,
            old_system_config: self.system_config,
            register_stack_before: self.register_stack,
            all_assets_before: self.all_assets.clone(),
            all_market_details_before: self.all_market_details.clone(),
            old_account_tree_root: self.account_tree_root,
            old_account_pub_data_tree_root: self.account_pub_data_tree_root,
            old_account_delta_tree_root: self.account_delta_tree_root,
            old_market_tree_root: self.market_tree_root,
            txs,
        }
    }
}

/// The per-transaction positional pre-state corpus for ONE block.
///
/// `snapshots[i]` is the ledger pre-state having applied txs `0..i`, for
/// `i` in `0..=tx_count` (so `snapshots[0]` is block-initial and
/// `snapshots[tx_count]` is block-final). Chunk `k` at chunk size `S` reads
/// `at_chunk(S, k) == snapshots[S * k]`.
#[derive(Debug, Clone)]
pub struct PreStateSnapshots {
    pub height: u64,
    pub created_at: i64,
    /// One snapshot per tx position `0..=tx_count` (length `tx_count + 1`).
    snapshots: Vec<ChunkPreState>,
}

impl PreStateSnapshots {
    pub fn new(height: u64, created_at: i64, snapshots: Vec<ChunkPreState>) -> Self {
        Self {
            height,
            created_at,
            snapshots,
        }
    }

    /// Number of tx positions captured (= `tx_count + 1`).
    pub fn len(&self) -> usize {
        self.snapshots.len()
    }

    pub fn is_empty(&self) -> bool {
        self.snapshots.is_empty()
    }

    /// The pre-state at tx POSITION `pos` (state having applied txs `0..pos`).
    pub fn at_position(&self, pos: usize) -> Option<&ChunkPreState> {
        self.snapshots.get(pos)
    }

    /// The pre-state for chunk `k` at chunk size `S` — the positional lookup
    /// `snapshots[S * k]` that makes this corpus S-INDEPENDENT (the same
    /// per-tx array serves every chunk size).
    pub fn at_chunk(&self, chunk_size: usize, chunk_index: usize) -> Option<&ChunkPreState> {
        self.at_position(chunk_size * chunk_index)
    }

    /// Raw snapshots (for serialization / corpus storage).
    pub fn snapshots(&self) -> &[ChunkPreState] {
        &self.snapshots
    }
}

/// Historical fixed empty ACCOUNT-FAMILY leaf index from #243. Retained as the
/// documented default; the corpus carries the ADAPTIVE per-position index in
/// [`EmptyIndexSiblingPaths::account_index`].
#[allow(dead_code)]
pub const EMPTY_INDEX: u128 = 2;

/// Historical fixed empty MARKET leaf index from #243 (`NIL_MARKET_INDEX = 255`).
/// Superseded by the adaptive [`EmptyIndexSiblingPaths::market_index`] (issue
/// #263); retained for documentation.
#[allow(dead_code)]
pub const EMPTY_MARKET_INDEX: u128 = circuit::types::constants::NIL_MARKET_INDEX as u128;
