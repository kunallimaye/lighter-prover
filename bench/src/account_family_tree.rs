// Copyright (c) Elliot Technologies, Inc.
// SPDX-License-Identifier: BUSL-1.1

//! Issue #243: an OFF-CIRCUIT sparse Merkle tree for the four account-family
//! state trees (`account`, `account_pub_data`, `account_delta`, `market`).
//!
//! ## Why this exists
//!
//! To pad the final (56th) chunk with `TX_TYPE_EMPTY` txs that prove mid-block,
//! each empty padding tx must carry HONEST current-state Merkle sibling-paths
//! for an empty leaf index (`EMPTY_ACCOUNT_INDEX = 2`) against the CURRENT
//! account-family trees — all-empty/genesis paths fail mid-block (the pilot
//! confirmed "Partition ... set twice": the `0` empty-root vs the real chained
//! mid-block root). The order-book tree is the only tree with a host-side
//! `proof(index)` today (`circuit::order_book_tree_helpers::OrderBookTree`);
//! this is the account-family analogue.
//!
//! ## Design
//!
//! Patterned after `OrderBookTree` but SIMPLER: a plain Poseidon2
//! `two_to_one` fold with NO per-node aggregate data (the account family does
//! not carry the order book's running sums). It is a sparse tree — only
//! inserted leaves and the internal nodes on their paths are materialized; all
//! other subtrees are the precomputed empty-subtree hash for their level.
//!
//! ## Fold rules (mirrors `circuit::merkle_helpers`)
//!
//! - Path bits are `split_le(index, L)` — little-endian, LEAF-FIRST
//!   (`merkle_helpers::account_index_to_merkle_path`).
//! - The fold is `hash_two_to_one_swap(node, sibling, bit)`:
//!   `bit == 0 ⇒ two_to_one(node, sibling)`, `bit == 1 ⇒ two_to_one(sibling,
//!   node)` (`merkle_helpers::recalculate_root`).
//! - The empty sibling at level `i` is the empty-subtree hash for any zero-leaf
//!   `two_to_one(h, h)` tree: `EMPTY_DELTA_TREE_HASHES[i]` for `i <=
//!   ACCOUNT_MERKLE_LEVELS`, the continued fold beyond (same as
//!   `empty_witness::empty_sibling`).
//!
//! A proof of these rules: a freshly-`new()` tree's `proof(index)` folds back
//! to its `root()` for any index, and inserting a leaf then `proof(index)`
//! still folds to the new `root()` (`tests` below).

use std::collections::HashMap;

use circuit::poseidon2::Poseidon2Hash;
use circuit::types::config::F;
use circuit::types::constants::{ACCOUNT_MERKLE_LEVELS, EMPTY_DELTA_TREE_HASHES};
use plonky2::hash::hash_types::HashOut;
use plonky2::plonk::config::Hasher;

/// Empty-subtree sibling hash at `level` for any zero-leaf `two_to_one(h, h)`
/// Merkle tree. Identical to `empty_witness::empty_sibling`, duplicated here so
/// this module is self-contained (and so the tree can be unit-tested without
/// pulling in the witness builder).
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

/// A sparse, fixed-depth (`L`-level) Poseidon2 Merkle tree over `HashOut<F>`
/// leaves, with the plain `two_to_one` fold used by the account-family state
/// trees. Account family trees are depth `ACCOUNT_MERKLE_LEVELS = 48`; the
/// market tree is depth `MARKET_MERKLE_LEVELS = 12`.
#[derive(Debug, Clone)]
pub struct AccountFamilyTree<const L: usize> {
    /// Precomputed empty-subtree hash per level (`empty_hashes[i]` is the root
    /// of an all-empty subtree of height `i`); `empty_hashes[L]` is the empty
    /// tree root.
    empty_hashes: Vec<HashOut<F>>,
    /// Materialized node hashes keyed by heap index (`(1 << L) + leaf_index` at
    /// the leaf level, halved each level up). A node absent from the map is the
    /// empty-subtree hash for its level.
    node_hashes: HashMap<u128, HashOut<F>>,
}

impl<const L: usize> AccountFamilyTree<L> {
    /// A fresh all-empty tree.
    pub fn new() -> Self {
        let empty_hashes: Vec<HashOut<F>> = (0..=L).map(empty_sibling).collect();
        Self {
            empty_hashes,
            node_hashes: HashMap::new(),
        }
    }

    /// The current root (empty-tree root if nothing inserted).
    pub fn root(&self) -> HashOut<F> {
        *self
            .node_hashes
            .get(&1u128)
            .unwrap_or(&self.empty_hashes[L])
    }

    /// Heap key of `leaf_index` at the leaf level.
    fn leaf_key(leaf_index: u128) -> u128 {
        (1u128 << L) + leaf_index
    }

    /// The materialized hash of `key`, or the empty-subtree hash for `key`'s
    /// level. `level` is the height of the subtree rooted at `key` (0 = leaf).
    fn node_or_empty(&self, key: u128, level: usize) -> HashOut<F> {
        *self
            .node_hashes
            .get(&key)
            .unwrap_or(&self.empty_hashes[level])
    }

    /// Insert / overwrite the leaf at `leaf_index` with `leaf_hash`, folding the
    /// new value up to the root. `leaf_index` is in `0..(1 << L)`.
    pub fn insert_leaf(&mut self, leaf_index: u128, leaf_hash: HashOut<F>) {
        let mut key = Self::leaf_key(leaf_index);
        let mut node_hash = leaf_hash;
        self.node_hashes.insert(key, node_hash);

        for level in 0..L {
            let bit = key & 1; // LE leaf-first: low bit selects left/right child.
            let sibling_key = key ^ 1;
            let sibling_hash = self.node_or_empty(sibling_key, level);

            // hash_two_to_one_swap(node, sibling, bit): bit==0 => (node,
            // sibling); bit==1 => (sibling, node). `bit` here is the CURRENT
            // node's position (0 = left child), matching the LE path bit.
            let parent_hash = if bit == 0 {
                Poseidon2Hash::two_to_one(node_hash, sibling_hash)
            } else {
                Poseidon2Hash::two_to_one(sibling_hash, node_hash)
            };

            let parent_key = key >> 1;
            self.node_hashes.insert(parent_key, parent_hash);
            node_hash = parent_hash;
            key = parent_key;
        }
    }

    /// The `L`-level sibling path for `leaf_index`, LEAF-FIRST (index 0 is the
    /// sibling at the leaf level). Folding the leaf with this path under the
    /// LE-bit swap reproduces [`root`](Self::root).
    pub fn proof(&self, leaf_index: u128) -> [HashOut<F>; L] {
        let mut key = Self::leaf_key(leaf_index);
        core::array::from_fn(|level| {
            let sibling_key = key ^ 1;
            let sibling = self.node_or_empty(sibling_key, level);
            key >>= 1;
            sibling
        })
    }

    /// Fold a leaf with its sibling path under the LE-bit swap (the host-side
    /// twin of `merkle_helpers::recalculate_root`). Exposed for tests and for
    /// validating a captured path against a known root.
    pub fn fold(leaf_index: u128, leaf_hash: HashOut<F>, path: &[HashOut<F>; L]) -> HashOut<F> {
        let mut state = leaf_hash;
        let mut key = leaf_index;
        for sibling in path.iter() {
            let bit = key & 1;
            state = if bit == 0 {
                Poseidon2Hash::two_to_one(state, *sibling)
            } else {
                Poseidon2Hash::two_to_one(*sibling, state)
            };
            key >>= 1;
        }
        state
    }
}

impl<const L: usize> Default for AccountFamilyTree<L> {
    fn default() -> Self {
        Self::new()
    }
}

/// Harvests honest Merkle node hashes from per-tx (leaf + sibling-path) proofs
/// to reconstruct the sibling-path of a FIXED target index against the CURRENT
/// state — WITHOUT needing every leaf in the tree.
///
/// ## The data-availability problem this solves
///
/// The block's INITIAL account-family roots are NOT empty (they commit to
/// hundreds of resident accounts whose leaf CONTENTS are not in the static
/// JSON). So we cannot build the tree from scratch by inserting leaves. But the
/// existing offline S=1 sweep (`prestate::sweep_per_tx_snapshots`) iterates
/// every `Tx<F>`, and each tx carries `accounts_before` (touched leaf contents)
/// + `account_*_tree_merkle_proofs` (honest siblings for those leaves against
/// the state at that position). Each such (leaf, proof) pair pins down the
/// honest node hash at EVERY node on that leaf's root-path (the leaf folded up,
/// and each sibling directly). Unioning these across all txs at a position
/// materializes the honest node hashes for the target index's path wherever a
/// touched account shares a subtree with it; everywhere else the node is the
/// empty-subtree hash.
///
/// ## Accumulation & honest-failure
///
/// Nodes are recorded keyed by heap position with LAST-WRITER-WINS semantics:
/// as state evolves across the sweep, a node touched at a later position
/// OVERWRITES its earlier value, so the harvester accumulates the most recent
/// honest node hashes (maximizing the target index's path coverage) rather than
/// erroring on the expected churn. A node NOT re-touched retains a stale value;
/// the per-position fold-back guard catches that — every reconstructed path is
/// folded from the target's (empty) leaf and compared to the position's PROVEN
/// root before it is emitted, so a stale/incomplete path is REJECTED (the
/// consumer stores `None`, never a wrong path). See
/// `prestate::sweep_per_tx_snapshots_with_paths`.
#[derive(Debug, Clone)]
pub struct PathHarvester<const L: usize> {
    empty_hashes: Vec<HashOut<F>>,
    /// Honest node hash per heap key (across all levels), unioned over the
    /// per-tx proofs observed since the last [`reset`](Self::reset).
    nodes: HashMap<u128, HashOut<F>>,
}

impl<const L: usize> PathHarvester<L> {
    pub fn new() -> Self {
        Self {
            empty_hashes: (0..=L).map(empty_sibling).collect(),
            nodes: HashMap::new(),
        }
    }

    /// Forget all harvested nodes (call when state rolls to a new position so a
    /// path is harvested against ONE coherent state, never mixing positions).
    pub fn reset(&mut self) {
        self.nodes.clear();
    }

    fn leaf_key(leaf_index: u128) -> u128 {
        (1u128 << L) + leaf_index
    }

    /// Record one honest `(leaf_hash, leaf_index, sibling_path)` proof against a
    /// SINGLE coherent state: pin the leaf node and every node on its root-path
    /// with LAST-WRITER-WINS (a node observed again OVERWRITES its prior value,
    /// absorbing state churn across positions). `sibling_path` is leaf-first
    /// (the `merkle_helpers::recalculate_root` order).
    ///
    /// Returns `Err` ONLY when `strict` is set and a node disagrees with an
    /// existing value — used by tests to assert coherence WITHIN one fixed
    /// state. The sweep uses the non-strict [`record_proof`](Self::record_proof).
    pub fn record_proof_strict(
        &mut self,
        leaf_index: u128,
        leaf_hash: HashOut<F>,
        sibling_path: &[HashOut<F>; L],
    ) -> Result<(), String> {
        self.record_proof_inner(leaf_index, leaf_hash, sibling_path, true)
    }

    /// Record one honest `(leaf, proof)` with last-writer-wins (no conflict
    /// error). The per-position fold-back guard in the sweep validates the
    /// resulting path, so churn between positions is absorbed safely.
    pub fn record_proof(
        &mut self,
        leaf_index: u128,
        leaf_hash: HashOut<F>,
        sibling_path: &[HashOut<F>; L],
    ) {
        let _ = self.record_proof_inner(leaf_index, leaf_hash, sibling_path, false);
    }

    fn record_proof_inner(
        &mut self,
        leaf_index: u128,
        leaf_hash: HashOut<F>,
        sibling_path: &[HashOut<F>; L],
        strict: bool,
    ) -> Result<(), String> {
        let mut key = Self::leaf_key(leaf_index);
        let mut node_hash = leaf_hash;
        self.insert_node(key, node_hash, 0, strict)?;

        for level in 0..L {
            let bit = key & 1;
            let sibling_key = key ^ 1;
            let sibling = sibling_path[level];
            // Pin the sibling node directly from the proof.
            self.insert_node(sibling_key, sibling, level, strict)?;

            let parent_hash = if bit == 0 {
                Poseidon2Hash::two_to_one(node_hash, sibling)
            } else {
                Poseidon2Hash::two_to_one(sibling, node_hash)
            };
            key >>= 1;
            self.insert_node(key, parent_hash, level + 1, strict)?;
            node_hash = parent_hash;
        }
        Ok(())
    }

    fn insert_node(
        &mut self,
        key: u128,
        hash: HashOut<F>,
        level: usize,
        strict: bool,
    ) -> Result<(), String> {
        if strict {
            if let Some(existing) = self.nodes.get(&key) {
                if *existing != hash {
                    return Err(format!(
                        "PathHarvester node conflict at heap key {key} (level {level}): \
                         {existing:?} vs {hash:?} — incoherent state"
                    ));
                }
            }
        }
        // Last-writer-wins: a node re-observed at a later position overwrites.
        self.nodes.insert(key, hash);
        Ok(())
    }

    /// The reconstructed leaf-first sibling path for `target_index`: each level
    /// reads the harvested sibling node, falling back to the empty-subtree hash
    /// when no observed proof touched that node (i.e. an all-empty subtree).
    pub fn path(&self, target_index: u128) -> [HashOut<F>; L] {
        let mut key = Self::leaf_key(target_index);
        core::array::from_fn(|level| {
            let sibling_key = key ^ 1;
            let sibling = *self
                .nodes
                .get(&sibling_key)
                .unwrap_or(&self.empty_hashes[level]);
            key >>= 1;
            sibling
        })
    }

    /// The harvested root (heap key 1), if any proof reached it; else the empty
    /// tree root. Used to sanity-check coherence against the proven snapshot.
    pub fn harvested_root(&self) -> HashOut<F> {
        *self.nodes.get(&1u128).unwrap_or(&self.empty_hashes[L])
    }

    /// Fold `target`'s (typically empty/ZERO) leaf with a path under the LE-bit
    /// swap — the host twin of `merkle_helpers::recalculate_root`.
    pub fn fold(target_index: u128, leaf_hash: HashOut<F>, path: &[HashOut<F>; L]) -> HashOut<F> {
        AccountFamilyTree::<L>::fold(target_index, leaf_hash, path)
    }
}

impl<const L: usize> Default for PathHarvester<L> {
    fn default() -> Self {
        Self::new()
    }
}

/// A genuinely-empty leaf index together with its honest sibling-path against a
/// single coherent state, reconstructed from ONE touched account's honest proof
/// ([`empty_path_from_proof`]).
#[derive(Debug, Clone, Copy)]
pub struct EmptyLeafPath<const L: usize> {
    /// The chosen empty leaf index (guaranteed empty at the captured state).
    pub index: u128,
    /// Leaf-first sibling-path; folding a ZERO leaf at `index` reproduces the
    /// state root.
    pub path: [HashOut<F>; L],
}

/// Reconstruct an empty leaf index + its honest sibling-path from a SINGLE
/// touched account's coherent proof, REQUIRING the chosen branch level to be
/// shared (empty in every tree that shares the account index).
///
/// ## Why this is correct (and why per-tx accumulation was not)
///
/// The fixed empty index 2 can never be covered by harvesting touched-leaf
/// proofs: its sibling subtrees (the `{0,1}` treasury/insurance subtree, index
/// 3) contain only never-touched accounts, so they are never observed; and
/// accumulating nodes across positions with different evolving roots is
/// incoherent. Instead, a real touched account at index `i` carries — in ONE
/// honest proof against ONE coherent root — every node on `i`'s root-path. We
/// pick the LOWEST level `b` (constrained to a level supplied by the caller via
/// `branch_level`, see [`common_empty_branch_level`]) where `i`'s sibling
/// subtree is EMPTY, then descend INTO that empty subtree to a guaranteed-empty
/// leaf `index = i ^ (1<<b)` with the bits below `b` cleared. Its sibling-path
/// is:
///
/// - levels `< b`: the empty-subtree hash (its whole subtree is empty);
/// - level `b`: `i`'s own subtree root (fold `i`'s leaf up with `proof[0..b]`);
/// - levels `> b`: `i`'s shared-ancestor siblings (`proof[b+1..]`).
///
/// Folding a ZERO leaf at `index` with this path reproduces the state root with
/// NO accumulation — fully coherent.
///
/// `leaf_index` / `leaf_hash` / `proof` are the touched account's honest
/// (index, native leaf hash, leaf-first sibling path) against the current root.
/// `branch_level` is the empty-sibling level to descend at; it MUST satisfy
/// `proof[branch_level] == empty_sibling(branch_level)` (the caller picks a
/// level empty in EVERY tree sharing the index). Returns `None` if that level's
/// sibling is not actually empty in this proof (a coherence failure).
pub fn empty_path_from_proof<const L: usize>(
    leaf_index: u128,
    leaf_hash: HashOut<F>,
    proof: &[HashOut<F>; L],
    branch_level: usize,
) -> Option<EmptyLeafPath<L>> {
    if branch_level >= L {
        return None;
    }
    // The descended-into subtree must genuinely be empty at this level.
    if proof[branch_level] != empty_sibling(branch_level) {
        return None;
    }

    // The guaranteed-empty leaf index: flip bit `branch_level`, clear below.
    let mut index = leaf_index ^ (1u128 << branch_level);
    index &= !((1u128 << branch_level) - 1);

    let mut path = [HashOut::<F>::ZERO; L];
    for (level, slot) in path.iter_mut().enumerate().take(branch_level) {
        *slot = empty_sibling(level);
    }

    // Level `branch_level`: the touched account's own subtree root (fold its
    // leaf up with the in-subtree siblings proof[0..branch_level]).
    let mut node = leaf_hash;
    let mut key = leaf_index;
    for &sibling in proof.iter().take(branch_level) {
        let bit = key & 1;
        node = if bit == 0 {
            Poseidon2Hash::two_to_one(node, sibling)
        } else {
            Poseidon2Hash::two_to_one(sibling, node)
        };
        key >>= 1;
    }
    path[branch_level] = node;

    // Levels above the branch: the shared-ancestor siblings, identical to the
    // touched account's proof (the empty index and `i` share these ancestors).
    path[(branch_level + 1)..L].copy_from_slice(&proof[(branch_level + 1)..L]);

    Some(EmptyLeafPath { index, path })
}

/// All levels (low → high) at which `leaf_index`'s sibling subtree is EMPTY in
/// EVERY supplied proof simultaneously. All proofs MUST be for the SAME
/// `leaf_index` against trees sharing that index (the account /
/// account_pub_data / account_delta trees share `account_index`), so a single
/// empty leaf index derived at any returned level is genuinely empty in all of
/// them.
///
/// Returned low-first so the caller can prefer the smallest descended empty
/// subtree, but try deeper levels when a shallow one yields a reserved/special
/// index (e.g. the `NIL_ACCOUNT_INDEX` sentinel).
pub fn common_empty_branch_levels<const L: usize>(proofs: &[&[HashOut<F>; L]]) -> Vec<usize> {
    (0..L)
        .filter(|&level| {
            let empty = empty_sibling(level);
            proofs.iter().all(|p| p[level] == empty)
        })
        .collect()
}

/// The lowest level at which `leaf_index`'s sibling subtree is EMPTY in EVERY
/// supplied proof simultaneously (see [`common_empty_branch_levels`]). Kept for
/// callers / tests that only need one candidate.
pub fn common_empty_branch_level<const L: usize>(
    leaf_index: u128,
    proofs: &[&[HashOut<F>; L]],
) -> Option<usize> {
    let _ = leaf_index; // index is implicit in the proofs; kept for clarity.
    common_empty_branch_levels(proofs).into_iter().next()
}

#[cfg(test)]
mod tests {
    use super::*;
    use circuit::types::constants::{
        EMPTY_ACCOUNT_DELTA_TREE_ROOT, MARKET_MERKLE_LEVELS,
    };
    use plonky2::field::types::Field;

    fn mk_leaf(seed: u64) -> HashOut<F> {
        HashOut::<F>::from_partial(&[F::from_canonical_u64(seed + 1)])
    }

    #[test]
    fn empty_tree_root_matches_account_delta_empty_root() {
        // The depth-48 empty account-family tree root equals the protocol's
        // pinned EMPTY_ACCOUNT_DELTA_TREE_ROOT (same zero-leaf two_to_one fold).
        let tree = AccountFamilyTree::<ACCOUNT_MERKLE_LEVELS>::new();
        assert_eq!(tree.root(), EMPTY_ACCOUNT_DELTA_TREE_ROOT);
    }

    #[test]
    fn empty_proof_folds_to_empty_root() {
        let tree = AccountFamilyTree::<ACCOUNT_MERKLE_LEVELS>::new();
        let root = tree.root();
        for &idx in &[0u128, 1, 2, 7, 100, (1u128 << 20) + 3] {
            let path = tree.proof(idx);
            let folded = AccountFamilyTree::<ACCOUNT_MERKLE_LEVELS>::fold(idx, HashOut::ZERO, &path);
            assert_eq!(folded, root, "empty proof for index {idx} must fold to root");
        }
    }

    #[test]
    fn inserted_leaf_proof_round_trips() {
        let mut tree = AccountFamilyTree::<ACCOUNT_MERKLE_LEVELS>::new();
        // Insert several leaves, then assert each leaf's proof folds to the
        // (updated) root, and the chosen empty index 2's proof also folds.
        let inserts: &[(u128, u64)] = &[(0, 10), (1, 20), (5, 30), (1234, 40), (1u128 << 30, 50)];
        for &(idx, seed) in inserts {
            tree.insert_leaf(idx, mk_leaf(seed));
        }
        let root = tree.root();
        for &(idx, seed) in inserts {
            let path = tree.proof(idx);
            let folded =
                AccountFamilyTree::<ACCOUNT_MERKLE_LEVELS>::fold(idx, mk_leaf(seed), &path);
            assert_eq!(folded, root, "leaf {idx} proof must fold to current root");
        }
        // The empty (never-inserted) index 2 must still fold from a ZERO leaf.
        let path2 = tree.proof(2);
        let folded2 = AccountFamilyTree::<ACCOUNT_MERKLE_LEVELS>::fold(2, HashOut::ZERO, &path2);
        assert_eq!(folded2, root, "empty index-2 proof must fold to current root");
    }

    #[test]
    fn overwriting_a_leaf_updates_root_and_proof() {
        let mut tree = AccountFamilyTree::<ACCOUNT_MERKLE_LEVELS>::new();
        tree.insert_leaf(2, mk_leaf(1));
        let r1 = tree.root();
        tree.insert_leaf(2, mk_leaf(2));
        let r2 = tree.root();
        assert_ne!(r1, r2, "overwriting a leaf must change the root");
        let path = tree.proof(2);
        let folded = AccountFamilyTree::<ACCOUNT_MERKLE_LEVELS>::fold(2, mk_leaf(2), &path);
        assert_eq!(folded, r2);
    }

    #[test]
    fn harvester_reconstructs_target_path_from_other_leaves_proofs() {
        // Build a ground-truth tree with several resident leaves (NOT at the
        // target index 2). Feed each resident leaf's (leaf, proof) into the
        // harvester. The harvester must then reconstruct index 2's sibling path
        // such that folding index 2's empty leaf yields the SAME root — WITHOUT
        // ever being told index 2's contents (it stays empty/ZERO).
        let mut truth = AccountFamilyTree::<ACCOUNT_MERKLE_LEVELS>::new();
        let residents: &[(u128, u64)] =
            &[(0, 1), (1, 2), (3, 3), (6, 4), (7, 5), (130, 6), (1u128 << 20, 7)];
        for &(idx, seed) in residents {
            truth.insert_leaf(idx, mk_leaf(seed));
        }
        let truth_root = truth.root();

        let mut harvester = PathHarvester::<ACCOUNT_MERKLE_LEVELS>::new();
        for &(idx, seed) in residents {
            let proof = truth.proof(idx);
            // strict: all proofs are against ONE fixed truth state, so no node
            // may conflict.
            harvester
                .record_proof_strict(idx, mk_leaf(seed), &proof)
                .expect("coherent proofs record without conflict");
        }

        // The harvested root must equal the truth root (every path to the root
        // is covered once a leaf on each top-level subtree is observed; here the
        // residents span both halves so heap key 1 is pinned).
        assert_eq!(
            harvester.harvested_root(),
            truth_root,
            "harvested root must match ground-truth root"
        );

        // Reconstruct index 2's path and fold its EMPTY leaf to the root.
        let path2 = harvester.path(2);
        let folded =
            PathHarvester::<ACCOUNT_MERKLE_LEVELS>::fold(2, HashOut::ZERO, &path2);
        assert_eq!(
            folded, truth_root,
            "index-2 empty leaf folded with harvested path must equal the root"
        );

        // And it must equal the tree's own proof for index 2 (the ground truth).
        let truth_path2 = truth.proof(2);
        assert_eq!(path2, truth_path2, "harvested index-2 path must match truth");
    }

    #[test]
    fn harvester_detects_incoherent_proofs() {
        let mut tree = AccountFamilyTree::<ACCOUNT_MERKLE_LEVELS>::new();
        tree.insert_leaf(0, mk_leaf(1));
        tree.insert_leaf(1, mk_leaf(2));
        let proof0 = tree.proof(0);

        let mut harvester = PathHarvester::<ACCOUNT_MERKLE_LEVELS>::new();
        harvester.record_proof_strict(0, mk_leaf(1), &proof0).unwrap();
        // Feeding leaf 0 with a DIFFERENT hash but the same proof conflicts at
        // the parent node — strict mode must reject it.
        let err = harvester
            .record_proof_strict(0, mk_leaf(999), &proof0)
            .unwrap_err();
        assert!(err.contains("conflict"), "incoherence must be a loud error: {err}");

        // Non-strict last-writer-wins: the same overwrite is ACCEPTED (the sweep
        // relies on this to absorb cross-position churn; the fold-back guard, not
        // a conflict error, is what keeps emitted paths correct).
        harvester.record_proof(0, mk_leaf(999), &proof0);
    }

    #[test]
    fn market_depth_tree_round_trips() {
        // The market tree is depth 12; exercise the generic depth parameter.
        let mut tree = AccountFamilyTree::<MARKET_MERKLE_LEVELS>::new();
        tree.insert_leaf(3, mk_leaf(7));
        tree.insert_leaf(255, mk_leaf(8));
        let root = tree.root();
        let path = tree.proof(3);
        let folded = AccountFamilyTree::<MARKET_MERKLE_LEVELS>::fold(3, mk_leaf(7), &path);
        assert_eq!(folded, root);
    }

    // ── #263 fix: empty-path reconstruction from ONE coherent proof ──────────
    //
    // The #243 harvester returned None for a fixed empty index (2) because that
    // index's sibling subtrees (treasury/insurance, index 3) are never touched
    // and accumulation across positions is incoherent. The fix derives an empty
    // leaf index + path from a SINGLE touched account's honest proof by
    // descending into its first empty sibling subtree. These tests assert the
    // reconstructed path folds to the SAME root with NO accumulation — the
    // regression the gated sweep used to catch, now catchable cheaply.

    /// A touched account at a HIGH index (mirroring the real cap block's
    /// ~2^48 residents) reconstructs an empty index whose path folds to root,
    /// from that account's proof ALONE — even though the fixed index 2 (and its
    /// untouched neighbours) are never observed.
    #[test]
    fn empty_path_reconstructs_from_single_proof() {
        let mut tree = AccountFamilyTree::<ACCOUNT_MERKLE_LEVELS>::new();
        // Residents clustered away from index 2 (as in the real block).
        let residents: &[(u128, u64)] = &[
            (281_474_976_639_642, 1),
            (281_474_976_644_047, 2),
            (39_720, 3),
            (103_631, 4),
        ];
        for &(idx, seed) in residents {
            tree.insert_leaf(idx, mk_leaf(seed));
        }
        let root = tree.root();

        // Take ONE touched account and its honest proof.
        let (idx, seed) = residents[0];
        let leaf = mk_leaf(seed);
        let proof = tree.proof(idx);

        // Find a branch level whose sibling is empty in this single tree.
        let b = common_empty_branch_level(idx, &[&proof])
            .expect("a sparse high-index account has an empty sibling subtree");
        let ep = empty_path_from_proof(idx, leaf, &proof, b)
            .expect("branch level is genuinely empty");

        // The reconstructed empty index must be EMPTY in the tree and its path
        // must fold a ZERO leaf to the SAME root.
        let folded =
            AccountFamilyTree::<ACCOUNT_MERKLE_LEVELS>::fold(ep.index, HashOut::ZERO, &ep.path);
        assert_eq!(folded, root, "reconstructed empty-index path must fold to root");
        // And it must match the tree's own proof for that empty index (ground
        // truth), confirming the descended index is genuinely empty.
        let truth_path = tree.proof(ep.index);
        assert_eq!(ep.path, truth_path, "reconstructed path must match ground truth");
    }

    /// The SAME empty index (derived at the lowest common empty branch level)
    /// folds to root in THREE trees that share the account index but have
    /// DIFFERENT populations (account / account_pub_data / account_delta).
    #[test]
    fn common_empty_index_folds_in_three_trees() {
        // Three trees with the SAME touched index `i` but different other leaves
        // (different populations), as account vs delta trees differ in reality.
        let i: u128 = 281_474_976_639_642;
        let mut t_acc = AccountFamilyTree::<ACCOUNT_MERKLE_LEVELS>::new();
        let mut t_pd = AccountFamilyTree::<ACCOUNT_MERKLE_LEVELS>::new();
        let mut t_delta = AccountFamilyTree::<ACCOUNT_MERKLE_LEVELS>::new();
        t_acc.insert_leaf(i, mk_leaf(11));
        t_pd.insert_leaf(i, mk_leaf(22));
        t_delta.insert_leaf(i, mk_leaf(33));
        // Distinct extra residents per tree (different populations).
        t_acc.insert_leaf(39_720, mk_leaf(1));
        t_pd.insert_leaf(660_542, mk_leaf(2));
        t_delta.insert_leaf(7, mk_leaf(3)); // delta populates near the low indices

        let p_acc = t_acc.proof(i);
        let p_pd = t_pd.proof(i);
        let p_delta = t_delta.proof(i);

        let b = common_empty_branch_level(i, &[&p_acc, &p_pd, &p_delta])
            .expect("a common empty branch level exists across the three trees");

        let e_acc = empty_path_from_proof(i, mk_leaf(11), &p_acc, b).unwrap();
        let e_pd = empty_path_from_proof(i, mk_leaf(22), &p_pd, b).unwrap();
        let e_delta = empty_path_from_proof(i, mk_leaf(33), &p_delta, b).unwrap();

        // SAME empty index across all three (derived from the same i and b).
        assert_eq!(e_acc.index, e_pd.index);
        assert_eq!(e_acc.index, e_delta.index);

        assert_eq!(
            AccountFamilyTree::<ACCOUNT_MERKLE_LEVELS>::fold(e_acc.index, HashOut::ZERO, &e_acc.path),
            t_acc.root(),
            "account tree empty-index fold"
        );
        assert_eq!(
            AccountFamilyTree::<ACCOUNT_MERKLE_LEVELS>::fold(e_pd.index, HashOut::ZERO, &e_pd.path),
            t_pd.root(),
            "pub_data tree empty-index fold"
        );
        assert_eq!(
            AccountFamilyTree::<ACCOUNT_MERKLE_LEVELS>::fold(
                e_delta.index,
                HashOut::ZERO,
                &e_delta.path
            ),
            t_delta.root(),
            "delta tree empty-index fold"
        );
    }

    /// The market tree (depth 12) reconstructs an empty index from its single
    /// touched market leaf.
    #[test]
    fn empty_path_reconstructs_market_depth() {
        let mut tree = AccountFamilyTree::<MARKET_MERKLE_LEVELS>::new();
        tree.insert_leaf(0, mk_leaf(7));
        tree.insert_leaf(1, mk_leaf(8));
        let root = tree.root();
        let proof = tree.proof(0);
        let b = common_empty_branch_level(0, &[&proof]).expect("empty branch in market tree");
        let ep = empty_path_from_proof(0u128, mk_leaf(7), &proof, b).unwrap();
        let folded =
            AccountFamilyTree::<MARKET_MERKLE_LEVELS>::fold(ep.index, HashOut::ZERO, &ep.path);
        assert_eq!(folded, root, "market empty-index path must fold to root");
        assert_eq!(ep.path, tree.proof(ep.index), "market path must match truth");
    }

    /// `empty_path_from_proof` HONESTLY rejects a branch level whose sibling is
    /// NOT empty (a populated subtree) — never emits a wrong path.
    #[test]
    fn empty_path_rejects_non_empty_branch() {
        let mut tree = AccountFamilyTree::<ACCOUNT_MERKLE_LEVELS>::new();
        // Make index 0 and 1 BOTH populated so index 0's level-0 sibling (1) is
        // non-empty.
        tree.insert_leaf(0, mk_leaf(1));
        tree.insert_leaf(1, mk_leaf(2));
        let proof = tree.proof(0);
        // Level 0's sibling (leaf 1) is populated -> must be rejected.
        assert!(
            empty_path_from_proof(0u128, mk_leaf(1), &proof, 0).is_none(),
            "a non-empty sibling level must be rejected"
        );
    }
}
