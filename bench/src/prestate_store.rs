// Copyright (c) Elliot Technologies, Inc.
// SPDX-License-Identifier: BUSL-1.1

//! Serialize / persist / mount the per-tx POSITIONAL pre-state corpus
//! (issue #257) so the ~2h S=1 sweep ([`crate::prestate::sweep_per_tx_snapshots`])
//! runs AT MOST ONCE per height instead of being regenerated in-memory on
//! every `run_cell` startup.
//!
//! ## Why this exists
//!
//! `PreStateSnapshots` / `ChunkPreState` carry NO serde derives in the library
//! and the `snapshots()` accessor had zero call sites — the persistence the
//! corpus design doc (`docs/per-tx-prestate-corpus.md` §4, §6) described was
//! never built. This module is that persistence layer: it (de)serializes the
//! corpus through a versioned, path-AWARE format and wires save/load to both a
//! local-disk path (tests / mounted corpus) and the existing
//! [`GcloudStorage`](crate::conductor::storage::GcloudStorage) byte transport.
//!
//! ## Format design — path-aware FROM DAY ONE (the whole point)
//!
//! The serialized [`PreStateCorpus`] carries BOTH the 8 root/state fields of
//! every snapshot AND an OPTIONAL, versioned [`SnapshotEntry::sibling_paths`]
//! field. In THIS issue every snapshot's `sibling_paths` is `None`, but the
//! field exists in the schema NOW, so the k=56 keystone (issue #243) can
//! populate captured empty-index sibling-paths WITHOUT a format revision —
//! consumers that predate #243 simply ignore a populated field, and the schema
//! version bumps only its MINOR component. This forward-compatibility is the
//! entire reason persistence lands before #243.
//!
//! ## Wire form
//!
//! `serde_json` (already a production dependency) framed by gzip
//! ([`flate2`]). JSON keeps the format human-inspectable and trivially
//! forward-compatible (unknown fields are skipped; absent optional fields
//! default), and gzip collapses the heavy redundancy between consecutive
//! snapshots (they differ in only the few accounts one tx touches). The 8
//! roots are plonky2 `HashOut<F>` (serde-capable). The big per-snapshot arrays
//! (`[Asset; 64]`, `[MarketDetails; 255]`) exceed serde's 32-element array
//! derive limit, so they are carried as `Vec` on the wire and validated back to
//! fixed-size arrays on load.

use std::io::{Read, Write};
use std::path::Path;

use circuit::types::asset::Asset;
use circuit::types::config::F;
use circuit::types::constants::{ASSET_LIST_SIZE, POSITION_LIST_SIZE};
use circuit::types::market_details::MarketDetails;
use circuit::types::register::RegisterStack;
use circuit::types::system_config::SystemConfig;
use flate2::Compression;
use flate2::read::GzDecoder;
use flate2::write::GzEncoder;
use plonky2::hash::hash_types::HashOut;
use serde::{Deserialize, Serialize};

use crate::prestate::{ChunkPreState, PreStateSnapshots};

/// Current corpus schema version. MAJOR is bumped on a breaking wire change;
/// MINOR is bumped on an ADDITIVE, backward-compatible change (e.g. issue #243
/// starting to populate the already-present optional `sibling_paths` field).
///
/// `1.0` is the initial persisted schema: roots + state, with an OPTIONAL
/// `sibling_paths` field present-but-unpopulated. #243 ships as `1.1` (same
/// readers, just a populated optional field) — NOT a `2.0`.
///
/// The version a corpus is STAMPED with is chosen per-document by
/// [`PreStateCorpus::from_snapshots`]: `1.1` when any snapshot carries captured
/// `sibling_paths`, else `1.0`. Both load on a `1.x` reader (additive MINOR).
pub const CORPUS_SCHEMA_VERSION: &str = "1.0";

/// The MINOR-bumped schema version stamped when a corpus carries captured
/// empty-index sibling-paths (issue #243). Backward-compatible with `1.0`.
pub const CORPUS_SCHEMA_VERSION_WITH_PATHS: &str = "1.1";

/// One position's sibling-path payload — the FORWARD-COMPATIBILITY seam for
/// issue #243.
///
/// Empty/absent in this issue. #243 will capture the empty-index Merkle
/// sibling-paths for the trees a chunk touches and store them HERE, per
/// position, WITHOUT changing the surrounding [`SnapshotEntry`] /
/// [`PreStateCorpus`] shape. The paths are kept as raw `HashOut<F>` levels in
/// a string-keyed map so #243 can name the trees it captures (e.g.
/// `"account"`, `"market"`) without this struct enumerating them up front.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct SiblingPaths {
    /// Tree-name -> ordered list of sibling hashes (leaf-first, the
    /// `merkle_helpers::recalculate_root` fold order). Empty until issue #243
    /// populates it. `#[serde(default)]` so older corpora that omit the key
    /// (and this whole optional field) still load.
    #[serde(default)]
    pub paths: std::collections::BTreeMap<String, Vec<HashOut<F>>>,
}

/// Tree-name keys used in [`SiblingPaths::paths`]. Single source of truth so
/// the producer (#243 sweep) and consumer (padding-tx builder) never drift.
pub const SIBLING_PATH_KEY_ACCOUNT: &str = "account";
pub const SIBLING_PATH_KEY_ACCOUNT_PUB_DATA: &str = "account_pub_data";
pub const SIBLING_PATH_KEY_ACCOUNT_DELTA: &str = "account_delta";
pub const SIBLING_PATH_KEY_MARKET: &str = "market";

impl SiblingPaths {
    /// Serialize the four typed empty-index sibling-paths into the string-keyed
    /// wire map (issue #243, schema 1.1).
    pub fn from_empty_index_paths(paths: &crate::prestate::EmptyIndexSiblingPaths) -> Self {
        let mut map = std::collections::BTreeMap::new();
        map.insert(SIBLING_PATH_KEY_ACCOUNT.to_string(), paths.account.to_vec());
        map.insert(
            SIBLING_PATH_KEY_ACCOUNT_PUB_DATA.to_string(),
            paths.account_pub_data.to_vec(),
        );
        map.insert(
            SIBLING_PATH_KEY_ACCOUNT_DELTA.to_string(),
            paths.account_delta.to_vec(),
        );
        map.insert(SIBLING_PATH_KEY_MARKET.to_string(), paths.market.to_vec());
        Self { paths: map }
    }

    /// Deserialize the wire map back into the four typed fixed-length paths,
    /// validating each tree is present with the right depth (honest error
    /// otherwise — never a silently truncated path).
    pub fn into_empty_index_paths(
        self,
    ) -> Result<crate::prestate::EmptyIndexSiblingPaths, CorpusError> {
        use circuit::types::constants::{ACCOUNT_MERKLE_LEVELS, MARKET_MERKLE_LEVELS};

        let take = |map: &std::collections::BTreeMap<String, Vec<HashOut<F>>>,
                    key: &'static str,
                    depth: usize|
         -> Result<Vec<HashOut<F>>, CorpusError> {
            let v = map
                .get(key)
                .cloned()
                .ok_or(CorpusError::MissingSiblingPath { tree: key })?;
            if v.len() != depth {
                return Err(CorpusError::ShapeMismatch {
                    field: key,
                    expected: depth,
                    got: v.len(),
                });
            }
            Ok(v)
        };

        let account = take(&self.paths, SIBLING_PATH_KEY_ACCOUNT, ACCOUNT_MERKLE_LEVELS)?;
        let account_pub_data = take(
            &self.paths,
            SIBLING_PATH_KEY_ACCOUNT_PUB_DATA,
            ACCOUNT_MERKLE_LEVELS,
        )?;
        let account_delta = take(
            &self.paths,
            SIBLING_PATH_KEY_ACCOUNT_DELTA,
            ACCOUNT_MERKLE_LEVELS,
        )?;
        let market = take(&self.paths, SIBLING_PATH_KEY_MARKET, MARKET_MERKLE_LEVELS)?;

        Ok(crate::prestate::EmptyIndexSiblingPaths {
            account: account.try_into().unwrap(),
            account_pub_data: account_pub_data.try_into().unwrap(),
            account_delta: account_delta.try_into().unwrap(),
            market: market.try_into().unwrap(),
        })
    }
}

/// The serializable form of ONE positional snapshot (the wire twin of
/// [`ChunkPreState`]). The big arrays are `Vec` on the wire because serde's
/// derive only covers arrays up to length 32 and these are 64 / 255.
#[derive(Debug, Clone, Serialize, Deserialize)]
struct SnapshotEntry {
    register_stack: RegisterStack,
    /// `ASSET_LIST_SIZE` (64) assets; validated on load.
    all_assets: Vec<Asset>,
    /// `POSITION_LIST_SIZE` (255) market details; validated on load.
    all_market_details: Vec<MarketDetails>,
    system_config: SystemConfig,
    account_tree_root: HashOut<F>,
    account_pub_data_tree_root: HashOut<F>,
    account_delta_tree_root: HashOut<F>,
    market_tree_root: HashOut<F>,

    /// Issue #243 forward-compatibility seam: OPTIONAL captured sibling-paths
    /// for this position. `None` in this issue. `#[serde(default,
    /// skip_serializing_if)]` keeps the v1.0 wire form compact (the field is
    /// simply absent when empty) AND lets a v1.0 reader load a v1.1 corpus
    /// that populates it — additive, no format revision.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    sibling_paths: Option<SiblingPaths>,
}

impl SnapshotEntry {
    fn from_chunk_pre_state(snap: &ChunkPreState) -> Self {
        Self {
            register_stack: snap.register_stack,
            all_assets: snap.all_assets.to_vec(),
            all_market_details: snap.all_market_details.to_vec(),
            system_config: snap.system_config,
            account_tree_root: snap.account_tree_root,
            account_pub_data_tree_root: snap.account_pub_data_tree_root,
            account_delta_tree_root: snap.account_delta_tree_root,
            market_tree_root: snap.market_tree_root,
            // Issue #243: serialize the OPTIONAL captured empty-index
            // sibling-paths (schema 1.1). `None` for roots-only snapshots
            // (#257 behavior); `Some` when the path-capturing sweep filled them.
            sibling_paths: snap
                .empty_index_sibling_paths
                .as_ref()
                .map(SiblingPaths::from_empty_index_paths),
        }
    }

    fn into_chunk_pre_state(self) -> Result<ChunkPreState, CorpusError> {
        let all_assets: [Asset; ASSET_LIST_SIZE] =
            self.all_assets
                .try_into()
                .map_err(|v: Vec<Asset>| CorpusError::ShapeMismatch {
                    field: "all_assets",
                    expected: ASSET_LIST_SIZE,
                    got: v.len(),
                })?;
        let all_market_details: [MarketDetails; POSITION_LIST_SIZE] = self
            .all_market_details
            .try_into()
            .map_err(|v: Vec<MarketDetails>| CorpusError::ShapeMismatch {
                field: "all_market_details",
                expected: POSITION_LIST_SIZE,
                got: v.len(),
            })?;
        // Issue #243: rehydrate the OPTIONAL empty-index sibling-paths (schema
        // 1.1). A 1.0 corpus omits the field (`None`); a 1.1 corpus carries the
        // four named tree paths. Malformed paths are an honest error.
        let empty_index_sibling_paths = match self.sibling_paths {
            Some(sp) => Some(sp.into_empty_index_paths()?),
            None => None,
        };

        Ok(ChunkPreState {
            register_stack: self.register_stack,
            all_assets,
            all_market_details,
            system_config: self.system_config,
            account_tree_root: self.account_tree_root,
            account_pub_data_tree_root: self.account_pub_data_tree_root,
            account_delta_tree_root: self.account_delta_tree_root,
            market_tree_root: self.market_tree_root,
            empty_index_sibling_paths,
        })
    }
}

/// The top-level, versioned, path-aware corpus document — the serialized twin
/// of [`PreStateSnapshots`] plus a schema version.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PreStateCorpus {
    /// Schema version (see [`CORPUS_SCHEMA_VERSION`]). Checked on load so an
    /// incompatible MAJOR is rejected honestly rather than mis-parsed.
    pub schema_version: String,
    pub height: u64,
    pub created_at: i64,
    snapshots: Vec<SnapshotEntry>,
}

impl PreStateCorpus {
    /// Build the serializable corpus from an in-memory [`PreStateSnapshots`].
    pub fn from_snapshots(snaps: &PreStateSnapshots) -> Self {
        let snapshots: Vec<SnapshotEntry> = snaps
            .snapshots()
            .iter()
            .map(SnapshotEntry::from_chunk_pre_state)
            .collect();
        // Stamp 1.1 iff any snapshot carries captured sibling-paths (issue
        // #243), else 1.0 (#257). Both are loadable by a 1.x reader.
        let schema_version = if snapshots.iter().any(|s| s.sibling_paths.is_some()) {
            CORPUS_SCHEMA_VERSION_WITH_PATHS
        } else {
            CORPUS_SCHEMA_VERSION
        }
        .to_string();
        Self {
            schema_version,
            height: snaps.height,
            created_at: snaps.created_at,
            snapshots,
        }
    }

    /// Reconstruct the in-memory [`PreStateSnapshots`] from this corpus,
    /// validating array shapes and the schema MAJOR version.
    pub fn into_snapshots(self) -> Result<PreStateSnapshots, CorpusError> {
        Self::check_compatible(&self.schema_version)?;
        let height = self.height;
        let created_at = self.created_at;
        let snapshots = self
            .snapshots
            .into_iter()
            .map(SnapshotEntry::into_chunk_pre_state)
            .collect::<Result<Vec<_>, _>>()?;
        Ok(PreStateSnapshots::new(height, created_at, snapshots))
    }

    /// Whether `version` is loadable by THIS build. Same MAJOR is compatible
    /// (additive MINOR bumps — e.g. #243 populating `sibling_paths` — stay
    /// readable); a different MAJOR is rejected.
    fn check_compatible(version: &str) -> Result<(), CorpusError> {
        let major = |v: &str| v.split('.').next().unwrap_or("").to_string();
        if major(version) != major(CORPUS_SCHEMA_VERSION) {
            return Err(CorpusError::IncompatibleVersion {
                found: version.to_string(),
                supported: CORPUS_SCHEMA_VERSION.to_string(),
            });
        }
        Ok(())
    }

    /// Serialize to gzip-framed JSON bytes — the on-the-wire / on-disk form.
    pub fn to_gzip_bytes(&self) -> Result<Vec<u8>, CorpusError> {
        let json = serde_json::to_vec(self)?;
        let mut enc = GzEncoder::new(Vec::new(), Compression::default());
        enc.write_all(&json)?;
        Ok(enc.finish()?)
    }

    /// Deserialize from gzip-framed JSON bytes.
    pub fn from_gzip_bytes(bytes: &[u8]) -> Result<Self, CorpusError> {
        let mut dec = GzDecoder::new(bytes);
        let mut json = Vec::new();
        dec.read_to_end(&mut json)?;
        Ok(serde_json::from_slice(&json)?)
    }

    /// Raw (uncompressed) JSON bytes — used by the size probe to report the
    /// raw-vs-gzip ratio.
    pub fn to_json_bytes(&self) -> Result<Vec<u8>, CorpusError> {
        Ok(serde_json::to_vec(self)?)
    }
}

/// Errors from corpus (de)serialization, sizing, save and load. Honest-failure:
/// a missing / corrupt / incompatible corpus is an `Err`, NEVER a silently
/// fabricated or partial `PreStateSnapshots`.
#[derive(Debug)]
pub enum CorpusError {
    /// A serialized array did not have the fixed length the type requires.
    ShapeMismatch {
        field: &'static str,
        expected: usize,
        got: usize,
    },
    /// The corpus schema MAJOR version is not loadable by this build.
    IncompatibleVersion { found: String, supported: String },
    /// A schema-1.1 corpus's `sibling_paths` was present but missing a required
    /// tree's path (issue #243).
    MissingSiblingPath { tree: &'static str },
    /// JSON (de)serialization failure.
    Json(serde_json::Error),
    /// Gzip / file I/O failure.
    Io(std::io::Error),
}

impl std::fmt::Display for CorpusError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            CorpusError::ShapeMismatch {
                field,
                expected,
                got,
            } => write!(
                f,
                "pre-state corpus field '{field}' has wrong length: expected {expected}, got {got}"
            ),
            CorpusError::IncompatibleVersion { found, supported } => write!(
                f,
                "pre-state corpus schema version '{found}' is incompatible with supported '{supported}' (MAJOR mismatch)"
            ),
            CorpusError::MissingSiblingPath { tree } => write!(
                f,
                "pre-state corpus sibling_paths present but missing required tree '{tree}' (issue #243 schema 1.1)"
            ),
            CorpusError::Json(e) => write!(f, "pre-state corpus JSON error: {e}"),
            CorpusError::Io(e) => write!(f, "pre-state corpus I/O error: {e}"),
        }
    }
}

impl std::error::Error for CorpusError {}

impl From<serde_json::Error> for CorpusError {
    fn from(e: serde_json::Error) -> Self {
        CorpusError::Json(e)
    }
}

impl From<std::io::Error> for CorpusError {
    fn from(e: std::io::Error) -> Self {
        CorpusError::Io(e)
    }
}

/// The object key for a height's pre-state corpus in the proof store, in the
/// SAME `{height}/...` namespace as
/// [`proof_object_key`](crate::conductor::storage::proof_object_key) and
/// [`merge_object_key`](crate::conductor::storage::merge_object_key). The
/// `/p/` segment makes it disjoint from BOTH the leaf namespace
/// (`{height}/{witness_index}`, second segment a decimal) AND the merge
/// namespace (`{height}/m/...`): a leaf key has exactly one `/`, a merge key's
/// second segment is the literal `m`, and a prestate key's second segment is
/// the literal `p`. Single source of truth so producer and consumer never
/// drift.
pub fn prestate_object_key(height: u64) -> String {
    format!("{height}/p/corpus")
}

// ─── Save / load: serialize -> bytes -> store, and bytes -> snapshots ───────

/// Serialize `snaps` to gzip-framed JSON and write it to a LOCAL DISK path
/// (creating parent dirs as needed). Used by tests and by a locally-mounted
/// corpus. Returns the number of bytes written.
pub fn save_prestate_corpus_to_path(
    snaps: &PreStateSnapshots,
    path: impl AsRef<Path>,
) -> Result<usize, CorpusError> {
    let bytes = PreStateCorpus::from_snapshots(snaps).to_gzip_bytes()?;
    let path = path.as_ref();
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    std::fs::write(path, &bytes)?;
    Ok(bytes.len())
}

/// Load a gzip-framed JSON corpus from a LOCAL DISK path back into a
/// [`PreStateSnapshots`]. Honest-failure: a missing / corrupt / incompatible
/// file returns `Err`.
pub fn load_prestate_corpus_from_path(
    path: impl AsRef<Path>,
) -> Result<PreStateSnapshots, CorpusError> {
    let bytes = std::fs::read(path)?;
    PreStateCorpus::from_gzip_bytes(&bytes)?.into_snapshots()
}

/// Serialize `snaps` and upload the gzip-framed bytes to the proof store under
/// [`prestate_object_key`]`(snaps.height)`. Reuses the existing
/// [`GcloudStorage`](crate::conductor::storage::GcloudStorage) byte transport
/// (mount-mode file I/O or `gcloud storage cp` CLI). Returns the object key.
pub fn save_prestate_corpus_to_store(
    snaps: &PreStateSnapshots,
    store: &crate::conductor::storage::GcloudStorage,
) -> Result<String, CorpusError> {
    let key = prestate_object_key(snaps.height);
    let bytes = PreStateCorpus::from_snapshots(snaps).to_gzip_bytes()?;
    store.upload(&key, &bytes).map_err(CorpusError::Io)
}

/// Download the corpus for `height` from the proof store and deserialize it
/// into a [`PreStateSnapshots`]. Honest-failure: a missing object or a corrupt
/// payload returns `Err` — the caller must fall back to the sweep, never
/// fabricate snapshots.
pub fn load_prestate_corpus_from_store(
    height: u64,
    store: &crate::conductor::storage::GcloudStorage,
) -> Result<PreStateSnapshots, CorpusError> {
    let key = prestate_object_key(height);
    let bytes = store.download(&key).map_err(CorpusError::Io)?;
    PreStateCorpus::from_gzip_bytes(&bytes)?.into_snapshots()
}

#[cfg(test)]
mod tests {
    use plonky2::field::types::Field;

    use super::*;

    /// A tiny synthetic `ChunkPreState` whose every field varies with `seed`
    /// so a round-trip that drops or transposes a field is caught. We do NOT
    /// run the real S=1 sweep here — that is the env-gated ~2h test.
    fn synthetic_snapshot(seed: u64) -> ChunkPreState {
        let mut all_assets: [Asset; ASSET_LIST_SIZE] =
            core::array::from_fn(|i| Asset::empty(i as i16));
        // Make a couple of assets non-trivial and seed-dependent.
        all_assets[1] = Asset {
            asset_index: 1,
            extension_multiplier: 1000 + seed as i64,
            min_transfer_amount: 5 + seed as i64,
            min_withdrawal_amount: 7,
            margin_mode: (seed % 3) as u8,
        };

        let mut all_market_details: [MarketDetails; POSITION_LIST_SIZE] =
            core::array::from_fn(|_| MarketDetails::default());
        all_market_details[2] = MarketDetails {
            market_index: 2,
            mark_price: 42 + seed as u32,
            // Exercise the custom BigInt (de)serializer round-trip, incl. a
            // negative value so the sign survives the i128 wire form.
            funding_rate_prefix_sum: num::BigInt::from(-12345i64 - seed as i64),
            open_interest: 9,
            ..Default::default()
        };

        let mk_root = |base: u64| {
            HashOut::<F>::from_partial(&[
                F::from_canonical_u64(base + seed),
                F::from_canonical_u64(base + 1),
                F::from_canonical_u64(base + 2),
                F::from_canonical_u64(base + 3),
            ])
        };

        let mut register_stack = RegisterStack::default();
        register_stack.stack[0].instruction_type = 3;
        register_stack.stack[0].account_index = 100 + seed as i64;
        register_stack.count = 1;

        ChunkPreState {
            register_stack,
            all_assets,
            all_market_details,
            system_config: SystemConfig {
                liquidity_pool_index: 11 + seed as i64,
                staking_pool_index: 22,
                liquidity_pool_cooldown_period: 33,
                staking_pool_lockup_period: 44,
            },
            account_tree_root: mk_root(1000),
            account_pub_data_tree_root: mk_root(2000),
            account_delta_tree_root: mk_root(3000),
            market_tree_root: mk_root(4000),
            empty_index_sibling_paths: None,
        }
    }

    fn synthetic_corpus(n: usize) -> PreStateSnapshots {
        let snaps = (0..n).map(|i| synthetic_snapshot(i as u64)).collect();
        PreStateSnapshots::new(186_974_616, 1_700_000_000, snaps)
    }

    /// Field-equality across the whole corpus (no `PartialEq` on `ChunkPreState`
    /// itself, so compare via the serializable twin which derives the needed
    /// equality on every field).
    fn assert_corpus_eq(a: &PreStateSnapshots, b: &PreStateSnapshots) {
        assert_eq!(a.height, b.height, "height");
        assert_eq!(a.created_at, b.created_at, "created_at");
        assert_eq!(a.len(), b.len(), "snapshot count");
        for (i, (x, y)) in a.snapshots().iter().zip(b.snapshots().iter()).enumerate() {
            assert_eq!(x.register_stack, y.register_stack, "register_stack @ {i}");
            assert_eq!(x.all_assets, y.all_assets, "all_assets @ {i}");
            assert_eq!(
                x.all_market_details, y.all_market_details,
                "all_market_details @ {i}"
            );
            assert_eq!(x.system_config, y.system_config, "system_config @ {i}");
            assert_eq!(
                x.account_tree_root, y.account_tree_root,
                "account_root @ {i}"
            );
            assert_eq!(
                x.account_pub_data_tree_root, y.account_pub_data_tree_root,
                "pub_data_root @ {i}"
            );
            assert_eq!(
                x.account_delta_tree_root, y.account_delta_tree_root,
                "delta_root @ {i}"
            );
            assert_eq!(x.market_tree_root, y.market_tree_root, "market_root @ {i}");
        }
    }

    #[test]
    fn serialize_deserialize_round_trips_field_equal() {
        let corpus = synthetic_corpus(5);
        let bytes = PreStateCorpus::from_snapshots(&corpus)
            .to_gzip_bytes()
            .unwrap();
        let back = PreStateCorpus::from_gzip_bytes(&bytes)
            .unwrap()
            .into_snapshots()
            .unwrap();
        assert_corpus_eq(&corpus, &back);
    }

    #[test]
    fn save_load_local_path_round_trips_field_equal() {
        let corpus = synthetic_corpus(4);
        let mut path = std::env::temp_dir();
        path.push(format!(
            "lighter-prestate-test-{}-{:?}/corpus.json.gz",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let n = save_prestate_corpus_to_path(&corpus, &path).unwrap();
        assert!(n > 0, "wrote non-empty corpus");
        let back = load_prestate_corpus_from_path(&path).unwrap();
        assert_corpus_eq(&corpus, &back);
        let _ = std::fs::remove_dir_all(path.parent().unwrap());
    }

    #[test]
    fn save_load_via_gcloud_storage_mount_round_trips() {
        // Exercise the GcloudStorage wiring in hermetic MOUNT mode (local temp
        // dir, no bucket, no gcloud, no auth) — the same transport run_cell
        // uses, proving the prestate_object_key path works end to end.
        use crate::conductor::storage::{GcloudStorage, StorageConfig};

        let mut root = std::env::temp_dir();
        root.push(format!(
            "lighter-prestate-store-{}-{:?}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let store = GcloudStorage::new(StorageConfig {
            bucket: String::new(),
            gcloud_bin: "gcloud".into(),
            mount_path: root.to_string_lossy().to_string(),
        });

        let corpus = synthetic_corpus(3);
        let key = save_prestate_corpus_to_store(&corpus, &store).unwrap();
        assert_eq!(key, prestate_object_key(corpus.height));
        let back = load_prestate_corpus_from_store(corpus.height, &store).unwrap();
        assert_corpus_eq(&corpus, &back);
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn format_carries_optional_sibling_paths_field() {
        // The forward-compat seam for #243: the schema MUST carry an OPTIONAL
        // per-position sibling-paths field. In #257 it is absent (None), but a
        // v1.0 reader MUST accept a corpus that POPULATES it (a future #243
        // v1.1 corpus) without a format revision.
        let corpus = synthetic_corpus(1);
        let doc = PreStateCorpus::from_snapshots(&corpus);

        // #257 default: every position's sibling_paths is None.
        let json = doc.to_json_bytes().unwrap();
        let raw = String::from_utf8(json).unwrap();
        assert!(
            !raw.contains("sibling_paths"),
            "v1.0 omits the empty optional field from the wire (additive form)"
        );

        // Simulate #243 populating the field, then prove THIS reader
        // round-trips it without any format change. Build the populated
        // `sibling_paths` JSON from a REAL `SiblingPaths` value so its wire
        // shape (incl. the `HashOut<F>` representation) is exactly what #243
        // would emit — never a hand-guessed shape. The four trees carry
        // CORRECTLY-SIZED paths (depth 48 for the account family, 12 for
        // market), since #243's path-aware load validates each tree's depth.
        use circuit::types::constants::{ACCOUNT_MERKLE_LEVELS, MARKET_MERKLE_LEVELS};
        let acc_path: Vec<HashOut<F>> = (0..ACCOUNT_MERKLE_LEVELS)
            .map(|i| HashOut::<F>::from_partial(&[F::from_canonical_u64(7 + i as u64)]))
            .collect();
        let mkt_path: Vec<HashOut<F>> = (0..MARKET_MERKLE_LEVELS)
            .map(|i| HashOut::<F>::from_partial(&[F::from_canonical_u64(100 + i as u64)]))
            .collect();
        let mut paths = std::collections::BTreeMap::new();
        paths.insert(SIBLING_PATH_KEY_ACCOUNT.to_string(), acc_path.clone());
        paths.insert(SIBLING_PATH_KEY_ACCOUNT_PUB_DATA.to_string(), acc_path.clone());
        paths.insert(SIBLING_PATH_KEY_ACCOUNT_DELTA.to_string(), acc_path);
        paths.insert(SIBLING_PATH_KEY_MARKET.to_string(), mkt_path);
        let sibling_paths = SiblingPaths { paths };
        let sibling_paths_json = serde_json::to_value(&sibling_paths).unwrap();

        // Reach into the (private) per-snapshot field via a JSON value cycle to
        // avoid widening the public API just for the test.
        let mut value: serde_json::Value =
            serde_json::from_slice(&doc.to_json_bytes().unwrap()).unwrap();
        value["snapshots"][0]["sibling_paths"] = sibling_paths_json;
        let v243_bytes = serde_json::to_vec(&value).unwrap();
        let reloaded: PreStateCorpus = serde_json::from_slice(&v243_bytes).unwrap();
        // It still deserializes into snapshots cleanly (the optional field is
        // carried but does not affect the 8 state fields a v1.0 consumer uses).
        let _ = reloaded.into_snapshots().unwrap();

        // And the SiblingPaths type itself round-trips a populated value.
        let sp_back: SiblingPaths =
            serde_json::from_value(serde_json::to_value(&sibling_paths).unwrap()).unwrap();
        assert_eq!(sibling_paths, sp_back);

        // Keep `doc` referenced (avoids an unused-mut lint on some toolchains).
        assert_eq!(doc.schema_version, CORPUS_SCHEMA_VERSION);
    }

    /// Issue #243: a corpus whose snapshots carry captured empty-index
    /// sibling-paths is stamped 1.1, round-trips the four typed paths exactly,
    /// and a malformed (wrong-depth) path is an honest error.
    #[test]
    fn schema_1_1_sibling_paths_round_trip() {
        use circuit::types::constants::{ACCOUNT_MERKLE_LEVELS, MARKET_MERKLE_LEVELS};

        let acc: [HashOut<F>; ACCOUNT_MERKLE_LEVELS] =
            core::array::from_fn(|i| HashOut::<F>::from_partial(&[F::from_canonical_u64(i as u64 + 1)]));
        let pd: [HashOut<F>; ACCOUNT_MERKLE_LEVELS] =
            core::array::from_fn(|i| HashOut::<F>::from_partial(&[F::from_canonical_u64(i as u64 + 1000)]));
        let delta: [HashOut<F>; ACCOUNT_MERKLE_LEVELS] =
            core::array::from_fn(|i| HashOut::<F>::from_partial(&[F::from_canonical_u64(i as u64 + 2000)]));
        let market: [HashOut<F>; MARKET_MERKLE_LEVELS] =
            core::array::from_fn(|i| HashOut::<F>::from_partial(&[F::from_canonical_u64(i as u64 + 3000)]));
        let paths = crate::prestate::EmptyIndexSiblingPaths {
            account: acc,
            account_pub_data: pd,
            account_delta: delta,
            market,
        };

        // A two-snapshot corpus where snapshot[0] carries the captured paths.
        let mut snaps: Vec<ChunkPreState> = (0..2).map(|i| synthetic_snapshot(i as u64)).collect();
        snaps[0].empty_index_sibling_paths = Some(paths.clone());
        let corpus = PreStateSnapshots::new(123, 456, snaps);

        let doc = PreStateCorpus::from_snapshots(&corpus);
        // Carrying captured paths bumps the stamp to 1.1 (still 1.x — loadable).
        assert_eq!(doc.schema_version, CORPUS_SCHEMA_VERSION_WITH_PATHS);

        let bytes = doc.to_gzip_bytes().unwrap();
        let back = PreStateCorpus::from_gzip_bytes(&bytes)
            .unwrap()
            .into_snapshots()
            .unwrap();

        let rt = back
            .at_position(0)
            .unwrap()
            .empty_index_sibling_paths
            .as_ref()
            .expect("snapshot[0] carries paths after round-trip");
        assert_eq!(rt.account, paths.account, "account path");
        assert_eq!(rt.account_pub_data, paths.account_pub_data, "pub_data path");
        assert_eq!(rt.account_delta, paths.account_delta, "delta path");
        assert_eq!(rt.market, paths.market, "market path");
        // snapshot[1] stays None.
        assert!(
            back.at_position(1)
                .unwrap()
                .empty_index_sibling_paths
                .is_none()
        );

        // A wrong-depth account path is an honest ShapeMismatch, not a panic.
        let mut value: serde_json::Value =
            serde_json::from_slice(&doc.to_json_bytes().unwrap()).unwrap();
        value["snapshots"][0]["sibling_paths"]["paths"][SIBLING_PATH_KEY_ACCOUNT] =
            serde_json::json!([]);
        let bad: PreStateCorpus = serde_json::from_value(value).unwrap();
        let err = bad.into_snapshots().unwrap_err();
        assert!(
            matches!(err, CorpusError::ShapeMismatch { field: "account", .. }),
            "wrong-depth sibling path must be a ShapeMismatch: {err}"
        );
    }

    #[test]
    fn incompatible_major_version_is_rejected() {
        let corpus = synthetic_corpus(1);
        let mut value: serde_json::Value = serde_json::from_slice(
            &PreStateCorpus::from_snapshots(&corpus)
                .to_json_bytes()
                .unwrap(),
        )
        .unwrap();
        value["schema_version"] = serde_json::json!("2.0");
        let doc: PreStateCorpus = serde_json::from_value(value).unwrap();
        let err = doc.into_snapshots().unwrap_err();
        assert!(
            matches!(err, CorpusError::IncompatibleVersion { .. }),
            "a different MAJOR must be rejected: {err}"
        );
    }

    #[test]
    fn same_major_minor_bump_is_accepted() {
        // #243 ships as 1.1 — same MAJOR, additive field. A v1.0 build MUST
        // still load it.
        let corpus = synthetic_corpus(2);
        let mut value: serde_json::Value = serde_json::from_slice(
            &PreStateCorpus::from_snapshots(&corpus)
                .to_json_bytes()
                .unwrap(),
        )
        .unwrap();
        value["schema_version"] = serde_json::json!("1.1");
        let doc: PreStateCorpus = serde_json::from_value(value).unwrap();
        let back = doc.into_snapshots().unwrap();
        assert_corpus_eq(&corpus, &back);
    }

    #[test]
    fn shape_mismatch_is_honest_error() {
        // A corpus whose all_assets vec is the wrong length must be a typed
        // error, never a panic or a silently truncated array.
        let corpus = synthetic_corpus(1);
        let mut value: serde_json::Value = serde_json::from_slice(
            &PreStateCorpus::from_snapshots(&corpus)
                .to_json_bytes()
                .unwrap(),
        )
        .unwrap();
        value["snapshots"][0]["all_assets"] = serde_json::json!([]); // empty
        let doc: PreStateCorpus = serde_json::from_value(value).unwrap();
        let err = doc.into_snapshots().unwrap_err();
        assert!(
            matches!(
                err,
                CorpusError::ShapeMismatch {
                    field: "all_assets",
                    ..
                }
            ),
            "wrong array length must be a ShapeMismatch: {err}"
        );
    }

    #[test]
    fn gzip_is_smaller_than_raw_json() {
        // The size-probe sanity check: gzip MUST beat raw JSON on a corpus with
        // the real redundancy (mostly-empty asset/market arrays repeated across
        // positions). Documents the raw-vs-gzip ratio the issue asks for.
        let corpus = synthetic_corpus(20);
        let doc = PreStateCorpus::from_snapshots(&corpus);
        let raw = doc.to_json_bytes().unwrap().len();
        let gz = doc.to_gzip_bytes().unwrap().len();
        assert!(
            gz < raw,
            "gzip ({gz}) must be smaller than raw JSON ({raw})"
        );
    }

    #[test]
    fn prestate_key_is_disjoint_from_leaf_and_merge() {
        use crate::conductor::storage::{merge_object_key, proof_object_key};
        let h = 42u64;
        let pk = prestate_object_key(h);
        assert_eq!(pk, "42/p/corpus");
        // Second segment is the literal 'p' — disjoint from leaf (decimal) and
        // merge ('m').
        assert_eq!(pk.split('/').nth(1), Some("p"));
        assert_ne!(pk, proof_object_key(h, 0));
        for level in 1..=4u64 {
            for idx in 0..8u64 {
                assert_ne!(pk, merge_object_key(h, level, idx));
            }
        }
    }

    /// Issue #257 size probe (run with `--ignored --nocapture` to see numbers):
    /// serialize a 501-snapshot corpus (a 500-tx block + final state) and print
    /// raw-JSON vs gzip sizes + ratio, so the PR documents ACTUAL measured
    /// sizes. Ignored by default so the non-gated suite stays fast and never
    /// runs the real ~2h sweep.
    #[test]
    #[ignore]
    fn probe_corpus_size_501_snapshots() {
        let corpus = synthetic_corpus(501);
        let doc = PreStateCorpus::from_snapshots(&corpus);
        let raw = doc.to_json_bytes().unwrap().len();
        let gz = doc.to_gzip_bytes().unwrap().len();
        println!(
            "PRESTATE-CORPUS-SIZE-PROBE snapshots=501 raw_json_bytes={raw} gzip_bytes={gz} \
             raw_MiB={:.2} gzip_MiB={:.2} ratio={:.1}x",
            raw as f64 / (1024.0 * 1024.0),
            gz as f64 / (1024.0 * 1024.0),
            raw as f64 / gz as f64,
        );
    }
}
