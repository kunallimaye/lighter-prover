// Copyright (c) Elliot Technologies, Inc.
// SPDX-License-Identifier: BUSL-1.1

//! LOAD-only mount of the per-tx POSITIONAL pre-state corpus (issue #316).
//!
//! ## What this is (READ path only)
//!
//! This is the trimmed, LOAD-ONLY port of the corpus persistence layer from
//! `parallel-v0.0.1-alpha`. It deserializes a gzip-framed JSON corpus from disk
//! back into a [`PreStateSnapshots`] so a consumer can replay the SAME committed
//! block from the committed dataset
//! (`bench/corpus/cap-block/captured_corpus.gz`) instead of re-running the O(N²)
//! prefix replay.
//!
//! The SAVE / STORE half — serializing snapshots to bytes, writing to a local
//! path, and uploading to / downloading from the `GcloudStorage` byte transport
//! — is NOT ported here. It is only needed to MINT a NEW corpus and lives on
//! `parallel-v0.0.1-alpha`. Accordingly the three wire structs derive
//! [`Deserialize`] but NOT `Serialize`.
//!
//! ## Wire form
//!
//! `serde_json` framed by gzip ([`flate2`]). The 8 roots are plonky2
//! `HashOut<F>` (serde-capable). The big per-snapshot arrays (`[Asset; 64]`,
//! `[MarketDetails; 255]`) exceed serde's 32-element array derive limit, so they
//! are carried as `Vec` on the wire and validated back to fixed-size arrays on
//! load. The OPTIONAL per-position `sibling_paths` field is `#[serde(default)]`
//! so a v1.0 reader loads a v1.1 corpus and vice-versa (additive MINOR bump).

use std::io::Read;
use std::path::Path;

use circuit::types::asset::Asset;
use circuit::types::config::F;
use circuit::types::constants::{ASSET_LIST_SIZE, POSITION_LIST_SIZE};
use circuit::types::market_details::MarketDetails;
use circuit::types::register::RegisterStack;
use circuit::types::system_config::SystemConfig;
use flate2::read::GzDecoder;
use plonky2::hash::hash_types::HashOut;
use serde::Deserialize;

use crate::prestate::{ChunkPreState, PreStateSnapshots};

/// Current corpus schema MAJOR.MINOR the loader gates on. MAJOR is bumped on a
/// breaking wire change; MINOR on an ADDITIVE, backward-compatible change (e.g.
/// issue #243 populating the already-present optional `sibling_paths` field). A
/// `1.x` reader loads any `1.y` corpus.
pub const CORPUS_SCHEMA_VERSION: &str = "1.0";

/// The MINOR-bumped schema version stamped when a corpus carries captured
/// empty-index sibling-paths (issue #243). Backward-compatible with `1.0`. The
/// committed cap-block corpus is stamped `1.1`.
pub const CORPUS_SCHEMA_VERSION_WITH_PATHS: &str = "1.1";

/// One position's sibling-path payload (issue #243).
///
/// The paths are kept as raw `HashOut<F>` levels in a string-keyed map so the
/// generator can name the trees it captured (e.g. `"account"`, `"market"`)
/// without this struct enumerating them up front.
#[derive(Debug, Clone, Default, PartialEq, Deserialize)]
pub struct SiblingPaths {
    /// Tree-name -> ordered list of sibling hashes (leaf-first, the
    /// `merkle_helpers::recalculate_root` fold order). `#[serde(default)]` so a
    /// corpus that omits the key still loads.
    #[serde(default)]
    pub paths: std::collections::BTreeMap<String, Vec<HashOut<F>>>,
    /// Issue #263: the ADAPTIVE empty leaf index shared by the account /
    /// account_pub_data / account_delta trees. `#[serde(default)]` for forward
    /// compatibility.
    #[serde(default)]
    pub account_index: u128,
    /// Issue #263: the adaptive empty leaf index for the market tree.
    #[serde(default)]
    pub market_index: u128,
}

/// Tree-name keys used in [`SiblingPaths::paths`]. Single source of truth so
/// the producer (generator sweep) and consumer (padding-tx builder) never drift.
pub const SIBLING_PATH_KEY_ACCOUNT: &str = "account";
pub const SIBLING_PATH_KEY_ACCOUNT_PUB_DATA: &str = "account_pub_data";
pub const SIBLING_PATH_KEY_ACCOUNT_DELTA: &str = "account_delta";
pub const SIBLING_PATH_KEY_MARKET: &str = "market";

impl SiblingPaths {
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
            account_index: self.account_index,
            market_index: self.market_index,
            account: account.try_into().unwrap(),
            account_pub_data: account_pub_data.try_into().unwrap(),
            account_delta: account_delta.try_into().unwrap(),
            market: market.try_into().unwrap(),
        })
    }
}

/// The deserializable form of ONE positional snapshot (the wire twin of
/// [`ChunkPreState`]). The big arrays are `Vec` on the wire because serde's
/// derive only covers arrays up to length 32 and these are 64 / 255.
#[derive(Debug, Clone, Deserialize)]
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

    /// Issue #243 OPTIONAL captured sibling-paths for this position.
    /// `#[serde(default)]` so a v1.0 corpus (field absent) and a v1.1 corpus
    /// (field populated) both load on this reader.
    #[serde(default)]
    sibling_paths: Option<SiblingPaths>,
}

impl SnapshotEntry {
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

/// The top-level, versioned, path-aware corpus document — the deserializable
/// twin of [`PreStateSnapshots`] plus a schema version.
#[derive(Debug, Clone, Deserialize)]
pub struct PreStateCorpus {
    /// Schema version (see [`CORPUS_SCHEMA_VERSION`]). Checked on load so an
    /// incompatible MAJOR is rejected honestly rather than mis-parsed.
    pub schema_version: String,
    pub height: u64,
    pub created_at: i64,
    snapshots: Vec<SnapshotEntry>,
}

impl PreStateCorpus {
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

    /// Deserialize from gzip-framed JSON bytes.
    pub fn from_gzip_bytes(bytes: &[u8]) -> Result<Self, CorpusError> {
        let mut dec = GzDecoder::new(bytes);
        let mut json = Vec::new();
        dec.read_to_end(&mut json)?;
        Ok(serde_json::from_slice(&json)?)
    }
}

/// Errors from corpus deserialization and load. Honest-failure: a missing /
/// corrupt / incompatible corpus is an `Err`, NEVER a silently fabricated or
/// partial [`PreStateSnapshots`].
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
    /// JSON deserialization failure.
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

/// Load a gzip-framed JSON corpus from a LOCAL DISK path back into a
/// [`PreStateSnapshots`]. Honest-failure: a missing / corrupt / incompatible
/// file returns `Err` — the caller must fall back to the replay path, never
/// fabricate snapshots.
pub fn load_prestate_corpus_from_path(
    path: impl AsRef<Path>,
) -> Result<PreStateSnapshots, CorpusError> {
    let bytes = std::fs::read(path)?;
    PreStateCorpus::from_gzip_bytes(&bytes)?.into_snapshots()
}
