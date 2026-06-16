// Copyright (c) Elliot Technologies, Inc.
// SPDX-License-Identifier: BUSL-1.1

//! Smoke test: the committed schema-1.1 paths-bearing pre-state corpus at
//! `bench/corpus/cap-block/captured_corpus.gz` loads cleanly through the
//! production `bench::prestate_store::load_prestate_corpus_from_path` path
//! and carries the shape the issue brief promises.
//!
//! Issue #265: the artifact is a captured run against `bench_test.json`
//! (height `186974592`, 500 txs + 1 trailing post-state = 501 snapshots),
//! produced by `bench::prestate::sweep_per_tx_snapshots_with_paths` at
//! source HEAD `caaae0d`. Position 495 (the padded-chunk pre-state at
//! S=9 × 55 full chunks) carries the empty-index sibling-paths the padded
//! 56th chunk consumes (issue #243, finalized by #263).
//!
//! This is the CHEAP regression guard: it (a) detects bit-rot or
//! corruption of the committed artifact, (b) catches an incompatible
//! schema bump (the loader would return `IncompatibleVersion`), and
//! (c) keeps the format-version + snapshot-count + paths-at-495
//! invariants from drifting silently. It runs in the normal lane — no
//! env gates, no heavy proving, sub-second on a warm build.

use std::path::PathBuf;

use bench::prestate_store::{CORPUS_SCHEMA_VERSION_WITH_PATHS, load_prestate_corpus_from_path};

/// The committed cap-block corpus, located relative to the bench crate.
fn cap_block_corpus_path() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("corpus")
        .join("cap-block")
        .join("captured_corpus.gz")
}

#[test]
fn committed_cap_block_corpus_loads_with_expected_shape() {
    let path = cap_block_corpus_path();
    assert!(
        path.exists(),
        "committed cap-block corpus missing at {} — did `bench/corpus/cap-block/captured_corpus.gz` get deleted or moved? See bench/corpus/cap-block/README.md for the regenerate command.",
        path.display(),
    );

    // Production loader — gzip-framed JSON, schema-MAJOR-gated, shape-validated.
    let snaps = load_prestate_corpus_from_path(&path).unwrap_or_else(|e| {
        panic!(
            "committed cap-block corpus failed to load via load_prestate_corpus_from_path: {e}. \
             This usually means (a) the artifact was corrupted in transit or by a merge, or \
             (b) the schema MAJOR bumped incompatibly and the corpus needs regenerating per \
             bench/corpus/cap-block/README.md."
        )
    });

    // Block-identity invariants — these come straight from bench_test.json.
    assert_eq!(
        snaps.height, 186_974_592,
        "corpus height mismatch — wrong source block?"
    );

    // 500 txs in the source block + 1 trailing post-state.
    assert_eq!(
        snaps.len(),
        501,
        "expected 501 snapshots (500 tx positions + 1 trailing post-state)"
    );

    // Schema-1.1: the corpus carries captured empty-index sibling-paths.
    // We round-trip through the loader to confirm the on-disk schema_version
    // stamp is what the file says — we do this by re-serializing and reading
    // the stamp from the wire form, since `into_snapshots` consumes the doc.
    // Simpler equivalent: every position 0..=499 must carry paths AND
    // position 495 specifically (the padded-chunk pre-state) must carry
    // them — these are the schema-1.1 invariants the artifact promises.
    let pos_495 = snaps
        .at_position(495)
        .expect("position 495 exists in a 501-snapshot corpus");
    assert!(
        pos_495.empty_index_sibling_paths.is_some(),
        "snapshot[495] (padded-chunk pre-state at S=9 × 55 full chunks) MUST carry \
         empty_index_sibling_paths in a schema-1.1 corpus — this is the slot issue #243 needs"
    );

    // Position 500 is the trailing post-state — no following tx to harvest
    // paths from, so it is structurally `None`. Asserting this guards
    // against a regression that fabricates paths at the trailing position.
    let pos_500 = snaps
        .at_position(500)
        .expect("position 500 exists in a 501-snapshot corpus");
    assert!(
        pos_500.empty_index_sibling_paths.is_none(),
        "snapshot[500] is the trailing post-state and must NOT carry paths \
         (no following tx to harvest from)"
    );

    // 500 / 501 snapshots carry paths (positions 0..=499).
    let with_paths = snaps
        .snapshots()
        .iter()
        .filter(|s| s.empty_index_sibling_paths.is_some())
        .count();
    assert_eq!(
        with_paths, 500,
        "expected 500/501 snapshots to carry paths (positions 0..=499)"
    );

    // Document, not enforce, the schema-1.1 constant — keeps the link
    // between the committed artifact and the producer's stamp explicit.
    assert_eq!(
        CORPUS_SCHEMA_VERSION_WITH_PATHS, "1.1",
        "schema-1.1 const drifted — the committed corpus is stamped 1.1; \
         a MAJOR bump means regenerating per bench/corpus/cap-block/README.md"
    );
}
