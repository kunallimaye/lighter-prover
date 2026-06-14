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
        };

        on_step(pos, wall_ms);
    }

    // Final post-state (after all txs) — `snapshots[tx_count]`.
    snapshots.push(cur);

    PreStateSnapshots::new(height, created_at, snapshots)
}
