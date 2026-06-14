// Copyright (c) Elliot Technologies, Inc.
// SPDX-License-Identifier: BUSL-1.1

//! Integration test: the committed synthetic block corpus (issue #165) LOADS
//! via the real `bench::conductor::MountedCorpus` `{height, witness_index}`
//! resolver — with NO adapter code (ADR-0008 §1.1/§1.4).
//!
//! This mirrors exactly how `bench/src/bin/bench.rs` builds the k=1 mounted
//! corpus from `bench_test.json` (pre-slice into `S`-tx chunks indexed
//! `0..k-1`, then `MountedCorpus::single_block`/`mount_block`). It proves the
//! corpus's on-disk `{height, witness_index}` layout resolves under the real
//! resolver: every index entry `resolve()`s, `slice_count(height) == k`, and
//! the cap-band real-seed block is present at 500-tx.
//!
//! Refs #128 #121 #144.

use std::collections::BTreeMap;
use std::path::PathBuf;

use bench::conductor::{MountedCorpus, WitnessKey, WitnessResolver};
use serde_json::Value;

fn corpus_dir() -> PathBuf {
    // bench/ is CARGO_MANIFEST_DIR for this crate.
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("corpus")
}

fn load_json(name: &str) -> Value {
    let p = corpus_dir().join(name);
    let s = std::fs::read_to_string(&p)
        .unwrap_or_else(|e| panic!("corpus file missing: {} ({e})", p.display()));
    serde_json::from_str(&s)
        .unwrap_or_else(|e| panic!("corpus file not valid JSON: {} ({e})", p.display()))
}

/// Build a `MountedCorpus` from the committed corpus index, exactly as the
/// binary mounts `bench_test.json`: each `{height, witness_index}` becomes one
/// slice. The payload is the index ordinal (a stand-in for the real witness
/// bytes — the binary uses the pool index too; the resolve MODELS the lookup).
fn mount_from_index() -> (MountedCorpus<u64>, BTreeMap<u64, u64>) {
    let index = load_json("index.json");
    let entries = index["entries"].as_array().expect("index.entries array");

    // Group slices by height so we can mount_block per height (the resolver's
    // unit of mount). slices[i] -> {height, witness_index = i}.
    let mut by_height: BTreeMap<u64, Vec<(u64, usize)>> = BTreeMap::new();
    for (ord, e) in entries.iter().enumerate() {
        let height = e["height"].as_u64().expect("height u64");
        let wi = e["witness_index"].as_u64().expect("witness_index u64");
        let tx_count = e["tx_count"].as_u64().expect("tx_count u64") as usize;
        let v = by_height.entry(height).or_default();
        // witness_index must be contiguous 0..k within a height (the SPLIT
        // ordinal). Place by index so mount_block's enumerate matches.
        assert_eq!(
            wi as usize,
            v.len(),
            "witness_index not contiguous for height {height}: expected {}, got {wi}",
            v.len()
        );
        v.push((ord as u64, tx_count));
    }

    let mut corpus = MountedCorpus::new();
    let mut expected_counts = BTreeMap::new();
    for (height, slices) in &by_height {
        expected_counts.insert(*height, slices.len() as u64);
        corpus.mount_block(*height, slices.clone());
    }
    (corpus, expected_counts)
}

#[test]
fn corpus_loads_via_mounted_corpus_resolver() {
    let (corpus, expected_counts) = mount_from_index();
    let index = load_json("index.json");
    let entries = index["entries"].as_array().unwrap();

    assert!(!corpus.is_empty(), "corpus mounted empty");
    assert_eq!(
        corpus.len(),
        entries.len(),
        "every index entry must be one mounted slice"
    );

    // Every {height, witness_index} resolves through the REAL resolver path,
    // returning a slice + a real measured fetch_ms (never fabricated).
    for e in entries {
        let height = e["height"].as_u64().unwrap();
        let wi = e["witness_index"].as_u64().unwrap();
        let key = WitnessKey::new(height, wi);
        let r = corpus
            .resolve(key)
            .unwrap_or_else(|| panic!("key did not resolve: {key:?}"));
        assert_eq!(r.slice.key, key, "resolved slice echoes its key");
        // fetch_ms is the genuine local-resolve floor (u64 from Instant);
        // we only assert the field exists and is the timer path's output.
        let _real_floor: u64 = r.fetch_ms;
    }

    // slice_count(height) == k for every mounted height (the SPLIT width).
    for (height, k) in &expected_counts {
        assert_eq!(
            corpus.slice_count(*height),
            *k,
            "slice_count mismatch at height {height}"
        );
    }

    // Absent keys fall back (ADR-0008 §1.4): a witness_index past k, and an
    // unknown height, both resolve to None.
    let (some_height, some_k) = expected_counts.iter().next().unwrap();
    assert!(
        corpus.resolve(WitnessKey::new(*some_height, *some_k)).is_none(),
        "index past k must not resolve"
    );
    assert_eq!(corpus.slice_count(u64::MAX), 0, "unknown height -> 0 slices");
}

#[test]
fn manifest_matches_real_distribution_and_seed() {
    let manifest = load_json("manifest.json");

    // The distribution must be sourced from the real analyzer (or explicitly
    // the labeled doc fallback) — never invented.
    let source = manifest["distribution"]["source"]
        .as_str()
        .expect("distribution.source");
    assert!(
        source.contains("REAL analyzer on REAL trace") || source.contains("DOCUMENTED"),
        "distribution source not provenance-labeled: {source}"
    );

    // Cap-heavy: the scaled corpus must keep the ~73.6% real cap mass.
    let cap_frac = manifest["distribution"]["cap_fraction_scaled"]
        .as_f64()
        .expect("cap_fraction_scaled");
    assert!(
        cap_frac >= 0.70 && cap_frac <= 0.78,
        "corpus not cap-heavy: cap_fraction = {cap_frac} (expected ~0.736)"
    );

    // Exactly ONE real chain-VALID seed block, the cap block referencing
    // bench_test.json (no fabricated witness bytes).
    let blocks = manifest["blocks"].as_array().expect("blocks array");
    let seeds: Vec<&Value> = blocks
        .iter()
        .filter(|b| b["is_real_seed"].as_bool().unwrap_or(false))
        .collect();
    assert_eq!(seeds.len(), 1, "expected exactly one real seed block");
    let seed = seeds[0];
    assert_eq!(seed["tx_count"].as_u64(), Some(500), "seed must be a cap block");
    assert_eq!(seed["band"].as_str(), Some("eq_500"));
    assert_eq!(
        seed["seed_ref"].as_str(),
        Some("bench/bench_test.json"),
        "seed references the real validated fixture"
    );

    // No block claims to synthesize witness bytes (honest-partial).
    for b in blocks {
        assert_eq!(
            b["synthesized"].as_bool(),
            Some(false),
            "no corpus block may claim fabricated witness bytes"
        );
    }
    assert_eq!(
        manifest["honest_scope"]["synthesized_witness_bytes"].as_bool(),
        Some(false)
    );
}
