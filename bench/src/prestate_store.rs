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

    /// Deserialize from RAW (uncompressed) JSON bytes — issue #318.
    ///
    /// This is the ZERO-DECOMPRESS load path: it feeds the bytes straight to
    /// `serde_json::from_slice` with NO [`GzDecoder`] in the way. Baking the
    /// corpus into the runtime image as RAW JSON (`/data/captured_corpus.json`)
    /// and loading it through this path removes the per-startup gunzip cost,
    /// which is critical because LATENCY MEASUREMENT is a first-class concern of
    /// this project — a per-pod gunzip would pollute the measured numbers. The
    /// gzip path ([`from_gzip_bytes`]) is retained for the smaller committed
    /// `.gz` source-of-truth artifact.
    pub fn from_json_bytes(bytes: &[u8]) -> Result<Self, CorpusError> {
        Ok(serde_json::from_slice(bytes)?)
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

/// Load a per-tx pre-state corpus from a LOCAL DISK path back into a
/// [`PreStateSnapshots`], AUTO-DETECTING the wire framing by extension
/// (issue #318):
///
///   * `*.json` → [`PreStateCorpus::from_json_bytes`] — RAW, ZERO-DECOMPRESS.
///     This is the path the baked-in `/data/captured_corpus.json` runtime
///     artifact uses, so an in-pod load pays NO gunzip cost (latency
///     measurement is critical to this project — a per-startup gunzip would
///     pollute it).
///   * `*.gz` (or anything not ending in `.json`) → [`PreStateCorpus::from_gzip_bytes`]
///     — the existing gzip path for the smaller committed `.gz` source artifact.
///
/// Honest-failure: a missing / corrupt / incompatible file returns `Err` — the
/// caller must fall back to the replay path, never fabricate snapshots.
pub fn load_prestate_corpus_from_path(
    path: impl AsRef<Path>,
) -> Result<PreStateSnapshots, CorpusError> {
    let path = path.as_ref();
    let bytes = std::fs::read(path)?;
    let is_raw_json = path
        .extension()
        .and_then(|e| e.to_str())
        .map(|e| e.eq_ignore_ascii_case("json"))
        .unwrap_or(false);
    let corpus = if is_raw_json {
        PreStateCorpus::from_json_bytes(&bytes)?
    } else {
        PreStateCorpus::from_gzip_bytes(&bytes)?
    };
    corpus.into_snapshots()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The committed cap-block corpus artifacts (issue #318). Resolved relative
    /// to the crate root (`bench/`) so the test runs from `cargo test -p bench`.
    fn corpus_gz_path() -> std::path::PathBuf {
        std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("corpus/cap-block/captured_corpus.gz")
    }
    fn corpus_json_path() -> std::path::PathBuf {
        std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("corpus/cap-block/captured_corpus.json")
    }

    /// Cheap, fast (no proving) validation that the RAW `.json` load path
    /// (issue #318) yields the SAME snapshots as the existing `.gz` path,
    /// auto-detects framing by extension, and that `at_chunk` resolves. Also
    /// reports the raw-vs-gz LOAD LATENCY — the key datum justifying baking the
    /// RAW artifact into the image (latency measurement is critical here; a
    /// per-startup gunzip would pollute it).
    #[test]
    fn raw_json_load_equals_gzip_load_and_resolves_at_chunk() {
        let gz = corpus_gz_path();
        let json = corpus_json_path();
        if !gz.exists() || !json.exists() {
            eprintln!(
                "skipping: corpus artifact(s) absent (gz={}, json={})",
                gz.display(),
                json.display()
            );
            return;
        }

        // --- gz path (with decompress) ---
        let t0 = std::time::Instant::now();
        let snaps_gz = load_prestate_corpus_from_path(&gz).expect("gz corpus must load");
        let gz_ms = t0.elapsed().as_secs_f64() * 1000.0;

        // --- raw json path (zero decompress) ---
        let t1 = std::time::Instant::now();
        let snaps_json = load_prestate_corpus_from_path(&json).expect("raw json corpus must load");
        let json_ms = t1.elapsed().as_secs_f64() * 1000.0;

        // Same shape (501 snapshots: 500 txs + 1 trailing post-state).
        assert_eq!(snaps_gz.len(), 501, "gz corpus snapshot count");
        assert_eq!(snaps_json.len(), 501, "raw json corpus snapshot count");
        assert_eq!(
            snaps_gz.len(),
            snaps_json.len(),
            "raw json and gz must yield identical snapshot counts"
        );

        // at_chunk(4, 3) -> position 12 must be populated (roots present).
        let c_gz = snaps_gz.at_chunk(4, 3).expect("gz at_chunk(4,3)");
        let c_json = snaps_json.at_chunk(4, 3).expect("raw json at_chunk(4,3)");

        // Identical roots prove raw-load == gz-load (the corpus is the same
        // dataset, just a different on-disk framing). Compare every snapshot's
        // four state roots field-by-field (ChunkPreState has no PartialEq).
        for (i, (a, b)) in snaps_gz
            .snapshots()
            .iter()
            .zip(snaps_json.snapshots().iter())
            .enumerate()
        {
            assert_eq!(a.account_tree_root, b.account_tree_root, "pos {i} account root");
            assert_eq!(
                a.account_pub_data_tree_root, b.account_pub_data_tree_root,
                "pos {i} account_pub_data root"
            );
            assert_eq!(
                a.account_delta_tree_root, b.account_delta_tree_root,
                "pos {i} account_delta root"
            );
            assert_eq!(a.market_tree_root, b.market_tree_root, "pos {i} market root");
        }

        // The at_chunk(4,3) roots must be non-degenerate and identical.
        assert_eq!(c_gz.account_tree_root, c_json.account_tree_root);
        assert_eq!(c_gz.market_tree_root, c_json.market_tree_root);

        eprintln!(
            "[issue#318] corpus-load LATENCY: raw-json={json_ms:.3}ms gz={gz_ms:.3}ms \
             delta(gz-raw)={:.3}ms (raw avoids gunzip; latency measurement is critical)",
            gz_ms - json_ms
        );
    }
}
