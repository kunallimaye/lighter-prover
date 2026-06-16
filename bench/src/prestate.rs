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

/// The four honest empty-index account-family sibling-paths captured at one tx
/// position, together with the ADAPTIVELY-CHOSEN empty leaf indices they belong
/// to. Each path is leaf-first (the `merkle_helpers::recalculate_root` fold
/// order). The account / account_pub_data / account_delta trees are depth
/// `ACCOUNT_MERKLE_LEVELS = 48`; the market tree is depth
/// `MARKET_MERKLE_LEVELS = 12`.
///
/// ## Why the indices are carried (issue #263 fix)
///
/// The empty index is no longer the fixed constant 2 (whose untouched
/// neighbouring subtrees could never be harvested). Instead, each position's
/// empty index is derived from a real touched account's coherent proof
/// ([`crate::account_family_tree::empty_path_from_proof`]) — a guaranteed-empty
/// leaf in the descended empty subtree. The three account-family trees share
/// one `account_index`; the market tree has its own `market_index`. The empty
/// padding tx (`empty_witness::mid_block_empty_tx`) sets its leaves to these
/// indices so the in-circuit path bits (`split_le(account_index)`) match the
/// emitted siblings.
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

/// Historical fixed empty ACCOUNT-FAMILY leaf index from #243. Index 2 is a
/// non-special index never touched by any of the 500 txs in
/// `bench/bench_test.json`, so its leaf is genuinely empty mid-block. NOTE
/// (issue #263): the sweep no longer harvests THIS fixed index — its untouched
/// neighbouring subtrees ({0,1} treasury/insurance, index 3) can never be
/// reconstructed from per-tx proofs. The empty index is now derived ADAPTIVELY
/// per position from a real touched account's coherent proof
/// (`capture_empty_paths` /
/// [`crate::account_family_tree::empty_path_from_proof`]) and carried in
/// [`EmptyIndexSiblingPaths::account_index`]. Retained as the documented default
/// for `empty_witness::EMPTY_ACCOUNT_INDEX`.
#[allow(dead_code)]
pub const EMPTY_INDEX: u128 = 2;

/// Historical fixed empty MARKET leaf index from #243 (`NIL_MARKET_INDEX = 255`).
/// Superseded by the adaptive [`EmptyIndexSiblingPaths::market_index`] (issue
/// #263); retained for documentation.
#[allow(dead_code)]
pub const EMPTY_MARKET_INDEX: u128 = circuit::types::constants::NIL_MARKET_INDEX as u128;

/// Like [`sweep_per_tx_snapshots`] but ALSO captures, at every tx position, the
/// honest mid-block Merkle sibling-paths for an ADAPTIVELY-chosen empty leaf
/// index (issue #263) against the four
/// account-family trees (`account`, `account_pub_data`, `account_delta`,
/// `market`) — issue #243.
///
/// ## How paths are captured (issue #263 — coherent single-proof reconstruction)
///
/// Path capture piggybacks on the SAME forward sweep at zero extra prove cost.
/// At each position the sweep already sees the tx's `accounts_before` /
/// `accounts_delta_before` / `market_before` (honest leaf CONTENTS) and the
/// matching `*_tree_merkle_proofs` (honest siblings against the state at that
/// position). The leaf hashes are the NATIVE account-family hashes
/// ([`crate::account_family_native`]), verified bit-for-bit against the circuit.
///
/// Rather than UNION scattered per-tx proofs for a FIXED empty index (which the
/// original #243 harvester did — and which could never cover index 2's untouched
/// neighbouring subtrees, and mixed nodes across incoherent evolving roots; see
/// [`crate::account_family_tree::empty_path_from_proof`]), each position derives
/// an ADAPTIVE empty leaf index + its full sibling-path from ONE real touched
/// account's coherent proof at THAT position. The chosen index is genuinely
/// empty (it lives in an empty subtree the touched account branches away from)
/// and its path folds a ZERO leaf to the position's root with NO accumulation.
///
/// ## Coherence guard (honest-failure)
///
/// Every captured path is folded back from the chosen empty index's EMPTY (ZERO)
/// leaf and asserted to equal the position's pre-state tree root before it is
/// stored. A path that does not fold is reported via `None` — never a silently
/// wrong path. A consumer needing a path at a `None` position falls back, never
/// to a fabricated path. (This guard returning `None` on a genuine mismatch is
/// CORRECT behaviour — the fix makes the emitted paths correct, not the guard
/// lenient.)
///
/// Whether `index` is a valid index for an EMPTY account leaf used by the
/// padding tx (issue #263). Excludes the reserved/special indices the circuit
/// treats non-generically: treasury (0) and insurance-fund (1) are NEVER empty
/// (`account_hash::is_empty` excludes treasury, and both are populated), and the
/// `NIL_ACCOUNT_INDEX = 2^48 - 1` sentinel marks "no account" (using it as a
/// real empty leaf trips the circuit's NIL handling — observed as a witness
/// "set twice" conflict). A valid empty index is `2 ..= MAX_ACCOUNT_INDEX`.
fn is_valid_empty_account_index(index: u128) -> bool {
    use circuit::types::constants::MAX_ACCOUNT_INDEX;
    // Index 2 is the canonical first non-special empty index; anything in
    // `[2, MAX_ACCOUNT_INDEX]` is a normal account slot.
    index >= 2 && index <= MAX_ACCOUNT_INDEX as u128
}

/// Reconstruct the four account-family empty-index sibling-paths for ONE tx
/// position, COHERENTLY from that tx's own honest proofs against the pre-state
/// roots `state` (issue #263). Returns `None` (never a wrong path) when no real
/// touched account at this position yields a guaranteed-empty leaf in all three
/// account trees, or when the market leaf has no empty sibling subtree.
///
/// ## Strategy
///
/// The three account-family trees (`account`, `account_pub_data`,
/// `account_delta`) share one `account_index`. We scan the tx's real (non-NIL,
/// non-empty) touched accounts; for each we find the LOWEST branch level empty
/// in ALL THREE account proofs simultaneously
/// ([`crate::account_family_tree::common_empty_branch_level`]) and reconstruct a
/// single empty index + its three paths from that account's coherent proofs
/// ([`crate::account_family_tree::empty_path_from_proof`]). The market tree is
/// reconstructed independently from `market_before`'s honest proof.
///
/// Each path is fold-validated against the corresponding pre-state root before
/// being accepted.
fn capture_empty_paths(tx: &Tx<F>, state: &ChunkPreState, pos: usize) -> Option<EmptyIndexSiblingPaths> {
    use crate::account_family_native::{
        account_delta_leaf_hash, account_hash_native, market_leaf_hash,
    };
    use crate::account_family_tree::{
        common_empty_branch_levels, empty_path_from_proof, AccountFamilyTree,
    };
    use circuit::types::constants::{
        ACCOUNT_MERKLE_LEVELS, MARKET_MERKLE_LEVELS, NIL_MARKET_INDEX,
    };

    // ── Account / pub_data / delta: one shared empty index from one account ──
    // (empty_index, account_path, pub_data_path, delta_path).
    type AccountFamilyEmptyPaths = (
        u128,
        [HashOut<F>; ACCOUNT_MERKLE_LEVELS],
        [HashOut<F>; ACCOUNT_MERKLE_LEVELS],
        [HashOut<F>; ACCOUNT_MERKLE_LEVELS],
    );
    let mut account_paths: Option<AccountFamilyEmptyPaths> = None;

    'accounts: for (i, account) in tx.accounts_before.iter().enumerate() {
        let idx = account.account_index;
        if idx < 0 {
            continue; // NIL / unused account slot.
        }
        let (acc_hash, pd_hash, is_empty) = account_hash_native(account);
        if is_empty {
            continue; // Need a REAL account to borrow a coherent subtree root.
        }
        let idx = idx as u128;

        // The matching delta leaf (delta is indexed by accounts_delta_before).
        let dpos = tx
            .accounts_delta_before
            .iter()
            .position(|d| d.account_index == account.account_index);
        let Some(dpos) = dpos else { continue };
        let delta_hash = account_delta_leaf_hash(&tx.accounts_delta_before[dpos]);

        let acc_proof = &tx.account_tree_merkle_proofs[i];
        let pd_proof = &tx.account_pub_data_tree_merkle_proofs[i];
        let delta_proof = &tx.account_delta_tree_merkle_proofs[dpos];

        // Every branch level empty in ALL THREE account trees. Try shallow
        // first, but skip levels whose descended index is reserved/special
        // (treasury 0, insurance 1, or the NIL sentinel 2^48-1) — those are
        // NOT valid empty account leaves in the circuit.
        for b in common_empty_branch_levels(&[acc_proof, pd_proof, delta_proof]) {
            let (Some(e_acc), Some(e_pd), Some(e_delta)) = (
                empty_path_from_proof(idx, acc_hash, acc_proof, b),
                empty_path_from_proof(idx, pd_hash, pd_proof, b),
                empty_path_from_proof(idx, delta_hash, delta_proof, b),
            ) else {
                continue;
            };

            // All three derive the SAME empty index (same idx, same b).
            debug_assert_eq!(e_acc.index, e_pd.index);
            debug_assert_eq!(e_acc.index, e_delta.index);

            if !is_valid_empty_account_index(e_acc.index) {
                continue; // reserved/special index — try a deeper branch level.
            }

            // Fold-validate each against its pre-state root.
            let acc_ok = AccountFamilyTree::<ACCOUNT_MERKLE_LEVELS>::fold(
                e_acc.index,
                HashOut::ZERO,
                &e_acc.path,
            ) == state.account_tree_root;
            let pd_ok = AccountFamilyTree::<ACCOUNT_MERKLE_LEVELS>::fold(
                e_pd.index,
                HashOut::ZERO,
                &e_pd.path,
            ) == state.account_pub_data_tree_root;
            let delta_ok = AccountFamilyTree::<ACCOUNT_MERKLE_LEVELS>::fold(
                e_delta.index,
                HashOut::ZERO,
                &e_delta.path,
            ) == state.account_delta_tree_root;

            if acc_ok && pd_ok && delta_ok {
                account_paths = Some((e_acc.index, e_acc.path, e_pd.path, e_delta.path));
                break 'accounts;
            }
        }
    }

    let Some((account_index, account, account_pub_data, account_delta)) = account_paths else {
        log::debug!(
            "sweep path-capture: position {pos} no coherent account-family empty index \
             (no real touched account yields a common empty subtree); storing None"
        );
        return None;
    };

    // ── Market: independent empty index from the tx's market leaf ────────────
    // The empty tx's `market_before.market_index` must resolve to NIL in the
    // circuit (index > MAX_PERPS_MARKET_INDEX = 254 ⇒ not a perps market), so
    // the chosen empty market index must be > 254 (and != the reserved NIL
    // sentinel pattern is fine for market since NIL_MARKET_INDEX = 255 IS the
    // intended empty slot). We require `index >= NIL_MARKET_INDEX` so the leaf
    // is treated as the always-empty non-perps market slot.
    let mkt_idx = tx.market_before.market_index as u128;
    let mkt_hash = market_leaf_hash(&tx.market_before);
    let mkt_proof = &tx.market_tree_merkle_proof;
    let market_empty = common_empty_branch_levels(&[mkt_proof])
        .into_iter()
        .filter_map(|b| empty_path_from_proof(mkt_idx, mkt_hash, mkt_proof, b))
        .find(|e| {
            e.index >= NIL_MARKET_INDEX as u128
                && AccountFamilyTree::<MARKET_MERKLE_LEVELS>::fold(
                    e.index,
                    HashOut::ZERO,
                    &e.path,
                ) == state.market_tree_root
        });

    let Some(market_empty) = market_empty else {
        log::debug!(
            "sweep path-capture: position {pos} market empty path not reconstructible \
             (no empty non-perps market slot >= NIL_MARKET_INDEX); storing None"
        );
        return None;
    };

    Some(EmptyIndexSiblingPaths {
        account_index,
        market_index: market_empty.index,
        account,
        account_pub_data,
        account_delta,
        market: market_empty.path,
    })
}

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
    let mut snapshots: Vec<ChunkPreState> = Vec::with_capacity(txs.len() + 1);
    let mut cur = initial;

    for (pos, tx) in txs.iter().enumerate() {
        // Reconstruct the empty-index paths COHERENTLY from THIS tx's own honest
        // proofs against the CURRENT (pre-state) roots `cur`. No cross-position
        // accumulation — the #263 fix derives a guaranteed-empty leaf index from
        // a single touched account, so every emitted path folds to `cur`'s root.
        let mut snap = cur.clone();
        snap.empty_index_sibling_paths = capture_empty_paths(tx, &cur, pos);
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
