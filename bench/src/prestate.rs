// Copyright (c) Elliot Technologies, Inc.
// SPDX-License-Identifier: BUSL-1.1

//! Per-transaction POSITIONAL pre-state snapshots — the FINDING D fix
//! (issue #177).
//!
//! ## The bug this fixes
//!
//! The distributed cell (`bench --mode cell`) used to build EVERY chunk's
//! `BlockTx` from the block's INITIAL ledger state while selecting a
//! mid-block tx slice. Only chunk 0's pre-state matches block-initial; chunks
//! `1..k-1` need the CHAINED intermediate state (the ledger after all PRIOR
//! chunks applied). The mismatch trips the L1 circuit's wire-consistency
//! assertion (`Partition containing Wire(...) was set twice with different
//! values`), so only `witness_index ≡ 0 (mod pool_total)` proved and the other
//! `k-1` chunks failed (FINDING D, `docs/live-benchmark-results.md`).
//!
//! ## The settled design (issue #177, Decision 1)
//!
//! Pre-state is a property of a POSITION in the tx sequence, not of a chunk.
//! Chunk boundaries are an overlay the coordinator chooses via `S =
//! tx_per_proof` at dispatch time (`split_k(tx_count, S) = ceil(tx_count/S)`).
//! Therefore we snapshot the 8-field ledger pre-state at EVERY transaction
//! position (the state having applied txs `0..N`), so that for ANY chunk size
//! `S`, chunk `k`'s pre-state is simply `snapshot[S * k]`.
//!
//! This keeps the coordinator free to re-tune `S` without regenerating the
//! corpus (ADR-0006 §1.2: the coordinator owns SPLIT). Storing per-CHUNK
//! snapshots would bake `S` into the corpus — a design smell that would force
//! corpus regeneration to benchmark a different `S`.
//!
//! ## How the snapshots are produced (offline, off the critical path)
//!
//! There is NO prove-free host-side L1 transition function in the workspace
//! today (no `apply_block_tx`, no `host_tx_transition`). The ONLY way to
//! advance ledger state across a transaction is to PROVE that step's L1 and
//! read the `*_after` fields from its public inputs via
//! [`BlockTxWitness::from_public_inputs`]. So [`sweep_per_tx_snapshots`] runs
//! the sequential L1 sweep at `S = 1` (one tx per L1 prove), capturing the
//! pre-state BEFORE each tx and rolling forward from each proof's outputs.
//!
//! This sweep is intrinsically SERIAL but is done OFFLINE — entirely off the
//! benchmark critical path — so the k-way parallel PROVE the distributed
//! prover measures is fully preserved. The STEADY-STATE production design is a
//! coordinator computing pre-states live via a host-side prove-free transition
//! function (future work, issue #178), OR a live witness service (#119,
//! parked). Until then, pre-state DELIVERY cost is a SEPARATE,
//! currently-unmeasured production term.
//!
//! Refs #75 #172 #174 #165 #61 #72.

use circuit::block_tx::{BlockTx, BlockTxWitness};
use circuit::block_tx_constraints::{BlockTxCircuit, BlockTxTarget, Circuit as _};
use circuit::tx::Tx;
use circuit::types::asset::Asset;
use circuit::types::config::{C, D, F};
use circuit::types::constants::{ASSET_LIST_SIZE, POSITION_LIST_SIZE};
use circuit::types::market_details::MarketDetails;
use circuit::types::register::RegisterStack;
use circuit::types::system_config::SystemConfig;
use plonky2::hash::hash_types::HashOut;
use plonky2::plonk::circuit_data::CircuitData;
use plonky2::plonk::proof::ProofWithPublicInputs;

/// A snapshot of the 8 ledger pre-state fields the L1 `BlockTx` constructor
/// needs, captured at one POSITION in the tx sequence (the state having
/// applied txs `0..position`).
///
/// These are the SAME 8 fields the single-process tree-fold path snapshots
/// (`bench/src/bin/bench.rs` `ChunkPreState`, issue #72), promoted to the
/// library so the distributed cell fix and the offline corpus generator share
/// one definition. (Distinct from `seed::ChunkSeed`, which is only the 3 roots
/// the LEAF cyclic-base needs — NOT the full pre-state the L1 prove needs.)
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

    /// Issue #243: OPTIONAL honest mid-block sibling-paths for the chosen empty
    /// leaf index (`EMPTY_ACCOUNT_INDEX = 2`) against THIS position's
    /// account-family trees. `None` for snapshots produced by the roots-only
    /// sweep ([`sweep_per_tx_snapshots`]); `Some` when produced by the
    /// path-capturing sweep ([`sweep_per_tx_snapshots_with_paths`]). These feed
    /// the empty padding txs that pad the final chunk to a full `S`
    /// (`empty_witness::mid_block_empty_tx`).
    pub empty_index_sibling_paths: Option<EmptyIndexSiblingPaths>,
}

/// The four honest empty-index (index 2) account-family sibling-paths captured
/// at one tx position. Each is leaf-first (the `merkle_helpers::recalculate_root`
/// fold order). The account / account_pub_data / account_delta trees are depth
/// `ACCOUNT_MERKLE_LEVELS = 48`; the market tree is depth
/// `MARKET_MERKLE_LEVELS = 12`.
#[derive(Debug, Clone)]
pub struct EmptyIndexSiblingPaths {
    pub account: [HashOut<F>; circuit::types::constants::ACCOUNT_MERKLE_LEVELS],
    pub account_pub_data: [HashOut<F>; circuit::types::constants::ACCOUNT_MERKLE_LEVELS],
    pub account_delta: [HashOut<F>; circuit::types::constants::ACCOUNT_MERKLE_LEVELS],
    pub market: [HashOut<F>; circuit::types::constants::MARKET_MERKLE_LEVELS],
}

impl ChunkPreState {
    /// Build the L1 `BlockTx` for a chunk from THIS positional pre-state plus
    /// the chunk's tx slice. This is the seam the distributed cell now uses
    /// instead of block-initial state — identical regardless of who fills the
    /// pre-state in (offline generator here; live coordinator in production).
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
/// `at(S, k) == snapshots[S * k]`.
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

/// Run the sequential L1 sweep at `S = 1` over `txs`, capturing the per-tx
/// positional pre-state BEFORE each tx and rolling state forward from each
/// L1 proof's public inputs.
///
/// `initial` is the block-initial pre-state (`snapshots[0]`). The returned
/// corpus has `txs.len() + 1` snapshots: one before each tx plus the final
/// post-state.
///
/// This is the OFFLINE generator core (issue #177). It PROVES every single-tx
/// step — no proof is ever stubbed and no state is fabricated; every rolled
/// field comes from a real proof's [`BlockTxWitness::from_public_inputs`].
///
/// `on_step` is an optional progress/diagnostics hook called after each step
/// with `(position, prove_wall_ms)`.
pub fn sweep_per_tx_snapshots<Hook: FnMut(usize, u64)>(
    height: u64,
    created_at: i64,
    initial: ChunkPreState,
    txs: &[Tx<F>],
    l1_data: &CircuitData<F, C, D>,
    bt: &BlockTxTarget,
    mut on_step: Hook,
) -> PreStateSnapshots {
    let mut snapshots: Vec<ChunkPreState> = Vec::with_capacity(txs.len() + 1);
    let mut cur = initial;

    for (pos, tx) in txs.iter().enumerate() {
        // Snapshot the pre-state BEFORE applying this tx.
        snapshots.push(cur.clone());

        // Prove this single-tx step at S=1 to obtain the post-state. The ONLY
        // host-side way to advance the ledger today (no prove-free transition).
        let block_tx = cur.block_tx(created_at, vec![tx.clone()]);
        let t = std::time::Instant::now();
        let tx_proof: ProofWithPublicInputs<F, C, D> =
            BlockTxCircuit::prove(l1_data, &block_tx, bt).unwrap_or_else(|err| {
                panic!("sweep: L1 prove failed at position {pos} (height {height}): {err:?}")
            });
        let wall_ms = t.elapsed().as_millis() as u64;

        // Roll forward from the proof's public inputs — the same data plane
        // the tree-fold driver uses (`bench.rs` line 2048).
        let w = BlockTxWitness::from_public_inputs(&tx_proof.public_inputs);
        cur = ChunkPreState {
            register_stack: w.register_stack_after,
            all_assets: w.all_assets_after.clone(),
            all_market_details: w.all_market_details_after.clone(),
            system_config: w.new_system_config,
            account_tree_root: w.new_account_tree_root,
            account_pub_data_tree_root: w.new_account_pub_data_tree_root,
            account_delta_tree_root: w.new_account_delta_tree_root,
            market_tree_root: w.new_market_tree_root,
            empty_index_sibling_paths: None,
        };

        on_step(pos, wall_ms);
    }

    // Final post-state (after all txs) — `snapshots[tx_count]`.
    snapshots.push(cur);

    PreStateSnapshots::new(height, created_at, snapshots)
}

/// The empty ACCOUNT-FAMILY leaf index whose honest mid-block sibling-paths the
/// padding empties carry for the account / account_pub_data / account_delta
/// trees (issue #243). Index 2 is a non-special index confirmed NEVER touched by
/// any of the 500 txs in `bench/bench_test.json`, so its leaf is genuinely empty
/// mid-block (`empty_witness::EMPTY_ACCOUNT_INDEX`).
pub const EMPTY_INDEX: u128 = 2;

/// The empty MARKET leaf index the padding empties' `market_before` sits at
/// (`NIL_MARKET_INDEX = 255` — the always-empty market slot the empty tx uses).
/// The market sibling-path is captured for THIS index so it matches the empty
/// tx's `market_before.market_index`.
pub const EMPTY_MARKET_INDEX: u128 = circuit::types::constants::NIL_MARKET_INDEX as u128;

/// Like [`sweep_per_tx_snapshots`] but ALSO captures, at every tx position, the
/// honest mid-block Merkle sibling-paths for [`EMPTY_INDEX`] against the four
/// account-family trees (`account`, `account_pub_data`, `account_delta`,
/// `market`) — issue #243.
///
/// ## How paths are captured
///
/// Path capture piggybacks on the SAME forward sweep at zero extra prove cost.
/// At each position the sweep already sees the tx's `accounts_before` /
/// `accounts_delta_before` / `market_before` (honest leaf CONTENTS) and the
/// matching `*_tree_merkle_proofs` (honest siblings against the state at that
/// position). Each `(leaf, proof)` pair pins the honest node hashes on that
/// leaf's root-path; unioned via a [`PathHarvester`](crate::account_family_tree::PathHarvester)
/// they reconstruct [`EMPTY_INDEX`]'s sibling-path wherever a touched account
/// shares a subtree with it (everywhere else the sibling is the empty-subtree
/// hash). The leaf hashes are the NATIVE account-family hashes
/// ([`crate::account_family_native`]), verified bit-for-bit against the circuit.
///
/// ## Coherence guard (honest-failure)
///
/// The harvesters accumulate across positions; a node-hash CONFLICT (two proofs
/// disagreeing) aborts the sweep. Additionally, every captured path is folded
/// back from [`EMPTY_INDEX`]'s EMPTY (ZERO) leaf and asserted to equal the
/// position's PROVEN tree root before it is stored. A path that does not fold to
/// the proven root is a fatal `panic` — never a silently wrong path. (For the
/// account-delta / market trees the empty leaf is ZERO at index 2 as well, since
/// index 2 is untouched.)
///
/// Off the per-chunk prove path: this is part of the one-time offline sweep.
pub fn sweep_per_tx_snapshots_with_paths<Hook: FnMut(usize, u64)>(
    height: u64,
    created_at: i64,
    initial: ChunkPreState,
    txs: &[Tx<F>],
    l1_data: &CircuitData<F, C, D>,
    bt: &BlockTxTarget,
    mut on_step: Hook,
) -> PreStateSnapshots {
    use crate::account_family_tree::PathHarvester;
    use circuit::types::constants::{ACCOUNT_MERKLE_LEVELS, MARKET_MERKLE_LEVELS};

    let mut snapshots: Vec<ChunkPreState> = Vec::with_capacity(txs.len() + 1);
    let mut cur = initial;

    // One harvester per account-family tree. The three account-family trees are
    // depth 48; the market tree is depth 12.
    let mut h_account = PathHarvester::<ACCOUNT_MERKLE_LEVELS>::new();
    let mut h_pub_data = PathHarvester::<ACCOUNT_MERKLE_LEVELS>::new();
    let mut h_delta = PathHarvester::<ACCOUNT_MERKLE_LEVELS>::new();
    let mut h_market = PathHarvester::<MARKET_MERKLE_LEVELS>::new();

    // Record one tx's touched account-family leaves + their honest proofs into
    // the harvesters. Records the PRE-state leaves (accounts_before) against the
    // PRE-state roots — the same root the snapshot at this position carries.
    fn record_tx(
        h_account: &mut PathHarvester<ACCOUNT_MERKLE_LEVELS>,
        h_pub_data: &mut PathHarvester<ACCOUNT_MERKLE_LEVELS>,
        h_delta: &mut PathHarvester<ACCOUNT_MERKLE_LEVELS>,
        h_market: &mut PathHarvester<MARKET_MERKLE_LEVELS>,
        tx: &Tx<F>,
    ) {
        use crate::account_family_native::{
            account_delta_leaf_hash, account_hash_native, market_leaf_hash,
        };
        for (i, account) in tx.accounts_before.iter().enumerate() {
            let idx = account.account_index;
            if idx < 0 {
                continue; // NIL / unused account slot.
            }
            let idx = idx as u128;
            let (acc_hash, pd_hash, _is_empty) = account_hash_native(account);
            h_account.record_proof(idx, acc_hash, &tx.account_tree_merkle_proofs[i]);
            h_pub_data.record_proof(idx, pd_hash, &tx.account_pub_data_tree_merkle_proofs[i]);
        }
        for (i, delta) in tx.accounts_delta_before.iter().enumerate() {
            let idx = delta.account_index;
            if idx < 0 {
                continue;
            }
            let delta_hash = account_delta_leaf_hash(delta);
            h_delta.record_proof(idx as u128, delta_hash, &tx.account_delta_tree_merkle_proofs[i]);
        }
        // Market: one leaf per tx (market_before) at index market_index.
        let mkt_idx = tx.market_before.market_index as u128;
        let mkt_hash = market_leaf_hash(&tx.market_before);
        h_market.record_proof(mkt_idx, mkt_hash, &tx.market_tree_merkle_proof);
    }

    // Capture EMPTY_INDEX's four paths against the CURRENT (pre-state) roots,
    // folding each back from the empty leaf to validate against the proven root.
    fn capture(
        h_account: &PathHarvester<ACCOUNT_MERKLE_LEVELS>,
        h_pub_data: &PathHarvester<ACCOUNT_MERKLE_LEVELS>,
        h_delta: &PathHarvester<ACCOUNT_MERKLE_LEVELS>,
        h_market: &PathHarvester<MARKET_MERKLE_LEVELS>,
        state: &ChunkPreState,
        pos: usize,
    ) -> Option<EmptyIndexSiblingPaths> {
        let account = h_account.path(EMPTY_INDEX);
        let account_pub_data = h_pub_data.path(EMPTY_INDEX);
        let account_delta = h_delta.path(EMPTY_INDEX);
        let market = h_market.path(EMPTY_MARKET_INDEX);

        // HONEST-FAILURE: each path must fold the EMPTY (ZERO) leaf back to the
        // position's PROVEN tree root. If a single tx's observed proofs did not
        // reveal every populated sibling on EMPTY_INDEX's path (a
        // data-availability gap at this position), the fold will NOT match and
        // we return `None` rather than emit a WRONG path. A consumer that needs
        // a path at a position with `None` falls back (run_cell) — never to a
        // fabricated path. This is the GATE-2 (#243 pilot) coherence guard made
        // a per-position assertion.
        let account_ok = PathHarvester::<ACCOUNT_MERKLE_LEVELS>::fold(
            EMPTY_INDEX,
            HashOut::ZERO,
            &account,
        ) == state.account_tree_root;
        let pub_data_ok = PathHarvester::<ACCOUNT_MERKLE_LEVELS>::fold(
            EMPTY_INDEX,
            HashOut::ZERO,
            &account_pub_data,
        ) == state.account_pub_data_tree_root;
        let delta_ok = PathHarvester::<ACCOUNT_MERKLE_LEVELS>::fold(
            EMPTY_INDEX,
            HashOut::ZERO,
            &account_delta,
        ) == state.account_delta_tree_root;
        let market_ok = PathHarvester::<MARKET_MERKLE_LEVELS>::fold(
            EMPTY_MARKET_INDEX,
            HashOut::ZERO,
            &market,
        ) == state.market_tree_root;

        if account_ok && pub_data_ok && delta_ok && market_ok {
            Some(EmptyIndexSiblingPaths {
                account,
                account_pub_data,
                account_delta,
                market,
            })
        } else {
            log::debug!(
                "sweep path-capture: position {pos} path incomplete \
                 (account_ok={account_ok} pub_data_ok={pub_data_ok} \
                 delta_ok={delta_ok} market_ok={market_ok}); storing None"
            );
            None
        }
    }

    for (pos, tx) in txs.iter().enumerate() {
        // Record THIS tx's touched leaves+proofs (against the pre-state roots)
        // BEFORE capturing, so the capture at `pos` sees this position's data
        // (last-writer-wins; cumulative across positions).
        record_tx(&mut h_account, &mut h_pub_data, &mut h_delta, &mut h_market, tx);

        // Snapshot the pre-state BEFORE applying this tx, WITH captured paths.
        let mut snap = cur.clone();
        snap.empty_index_sibling_paths =
            capture(&h_account, &h_pub_data, &h_delta, &h_market, &cur, pos);
        snapshots.push(snap);

        // Prove this single-tx step to obtain the post-state.
        let block_tx = cur.block_tx(created_at, vec![tx.clone()]);
        let t = std::time::Instant::now();
        let tx_proof: ProofWithPublicInputs<F, C, D> =
            BlockTxCircuit::prove(l1_data, &block_tx, bt).unwrap_or_else(|err| {
                panic!("sweep: L1 prove failed at position {pos} (height {height}): {err:?}")
            });
        let wall_ms = t.elapsed().as_millis() as u64;

        let w = BlockTxWitness::from_public_inputs(&tx_proof.public_inputs);
        // Harvesters accumulate ACROSS positions with last-writer-wins (no
        // reset): nodes touched by a later tx overwrite their stale value, so the
        // target index's path coverage grows over the sweep. Stale (never-
        // re-touched) nodes are caught by the per-position fold-back guard in
        // `capture`, which emits `None` rather than a wrong path.

        cur = ChunkPreState {
            register_stack: w.register_stack_after,
            all_assets: w.all_assets_after.clone(),
            all_market_details: w.all_market_details_after.clone(),
            system_config: w.new_system_config,
            account_tree_root: w.new_account_tree_root,
            account_pub_data_tree_root: w.new_account_pub_data_tree_root,
            account_delta_tree_root: w.new_account_delta_tree_root,
            market_tree_root: w.new_market_tree_root,
            empty_index_sibling_paths: None,
        };

        on_step(pos, wall_ms);
    }

    // Final post-state (after all txs). No tx follows, so no proofs are observed
    // against this root; we cannot capture a validated path here. It carries
    // `None` (the final snapshot is never a chunk boundary that needs padding —
    // the padded chunk's pre-state is `snapshots[S * 55]`, a mid-block position).
    snapshots.push(cur);

    PreStateSnapshots::new(height, created_at, snapshots)
}
