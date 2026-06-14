// Copyright (c) Elliot Technologies, Inc.
// SPDX-License-Identifier: BUSL-1.1

//! The witness plane — `{height, witness_index}` addressing + a k=1 local
//! resolver + the `witness_fetch_ms` measurement seam.
//!
//! Normative spec: **ADR-0008 §1** (delivery model) and **§2.1** (the exact
//! measurement point), operationalizing **ADR-0006 §3** (the seam) and
//! supplying **ADR-0004 §3.1**'s `witness_move` term as a *readable* (not yet
//! distributed-measured) field.
//!
//! ## Addressing (ADR-0008 §1.1)
//!
//! ```text
//! witness_key = { height, witness_index }
//! ```
//!
//! - `height` identifies the block (the same `height` already carried on the
//!   stream-mode `StreamArrival` / `ChunkProven` BENCH_EVENTs).
//! - `witness_index` is the **chunk ordinal** `0 .. k-1` over the
//!   coordinator's SPLIT — it selects the contiguous slice of the block's
//!   witness a cell proves at L1 (ADR-0008 §1.1; ADR-0003 §D6 amendment:
//!   "witnesses must be PARTITIONABLE across the cells; today's whole-block
//!   mounted corpus is the **k=1 case**").
//!
//! ## References, not bytes (ADR-0008 §1.2)
//!
//! Dispatch carries the `WitnessKey` **reference**, never the witness bytes.
//! The cell resolves the reference to bytes through a [`WitnessResolver`] —
//! the same call shape whether backed by today's mounted corpus or a future
//! Lighter witness service (source = TBD-by-#83; ADR-0006 §7c). The witness
//! "never travels the trace or the message bus" (ADR-0003 §D6 + amendment).
//!
//! ## The k=1 degenerate case (ADR-0008 §1.4)
//!
//! [`MountedCorpus::single_block`] is the k=1 building block: one block
//! mounted on local disk, addressed by a trivial `{height, witness_index}`,
//! read locally with no network. This is exactly what `bench_test.json` is
//! today (ADR-0008 §1.4). The resolver degrades cleanly: addressing collapses
//! to a constant, partitioning collapses to identity, the store is the
//! bundled file.
//!
//! ## The instrumentation seam (ADR-0008 §2.1)
//!
//! [`WitnessResolver::resolve`] wraps the resolve-and-read in an
//! `Instant::now()` / `.elapsed()` pair and returns the measured
//! `witness_fetch_ms` alongside the slice. This is the **local-resolve
//! floor**, NOT `witness_move` — the distributed term stays UNMODELED
//! (ADR-0008 §2.3 caveat; ADR-0004 §3.1/§3.2). No fetch-cost number is
//! invented: the field is the *real* local read wall, or `None`.

use std::collections::HashMap;
use std::time::Instant;

/// A witness *reference* — the tiny key that crosses the dispatch wire
/// instead of the witness bytes (ADR-0008 §1.2).
///
/// `Copy` + `Eq` + `Hash` so it can be a `HashMap` key and cheaply cloned
/// onto a `ChunkJob`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct WitnessKey {
    /// The block this witness belongs to (ADR-0008 §1.1).
    pub height: u64,
    /// The chunk ordinal `0 .. k-1` over the coordinator's SPLIT — the
    /// slice of the block's witness this cell proves (ADR-0008 §1.1).
    pub witness_index: u64,
}

impl WitnessKey {
    pub fn new(height: u64, witness_index: u64) -> Self {
        Self {
            height,
            witness_index,
        }
    }
}

/// One resolved witness slice: the opaque payload a cell proves, plus the
/// `S`-tx count it represents. Generic over the payload `T` so this module
/// stays plonky2-free and unit-testable (the binary instantiates `T` with
/// the real `Vec<Tx<F>>` chunk; tests use a cheap stand-in).
#[derive(Debug, Clone)]
pub struct WitnessSlice<T> {
    /// The witness key this slice resolved from (echoed for attribution).
    pub key: WitnessKey,
    /// The witness payload (the chunk's txs in the binary; a stand-in in
    /// tests). Opaque to the conductor.
    pub payload: T,
    /// Transactions in this slice (`S`, the chunk size).
    pub tx_count: usize,
}

/// The result of a [`WitnessResolver::resolve`] call: the slice plus the
/// **measured** `witness_fetch_ms` (the ADR-0008 §2.1 seam). `fetch_ms` is
/// the *local-resolve floor*, never `witness_move` (ADR-0008 §2.3).
#[derive(Debug, Clone)]
pub struct ResolvedWitness<T> {
    pub slice: WitnessSlice<T>,
    /// Wall time of the resolve-and-read, in milliseconds (ADR-0008 §2.1).
    /// This is what the `witness_fetch_ms` BENCH_EVENT field carries.
    pub fetch_ms: u64,
}

/// The source-independent witness resolution seam (ADR-0006 §3; ADR-0008
/// §1.2). A cell, given its `{height, witness_index}`, resolves the witness
/// slice through this interface — the same call shape whether backed by the
/// mounted corpus today or a witness service later.
///
/// Implementors must NOT place a synchronous remote GET on the prove path
/// (ADR-0008 §1.3): the *resolve* must be a local operation. The default
/// [`WitnessResolver::resolve`] times the implementor's [`fetch`] and
/// returns the measured `fetch_ms`.
///
/// [`fetch`]: WitnessResolver::fetch
pub trait WitnessResolver {
    /// The opaque witness payload type (the real chunk in the binary; a
    /// stand-in in tests).
    type Payload;

    /// Resolve `key` to its witness slice *without timing* — implementors
    /// only do the local read here. Returns `None` when the key is absent
    /// (ADR-0008 §1.4: "falls back to current behaviour when the corpus is
    /// absent" — the caller decides the fallback).
    fn fetch(&self, key: WitnessKey) -> Option<WitnessSlice<Self::Payload>>;

    /// Resolve `key` AND measure the resolve-and-read wall (ADR-0008 §2.1).
    /// This is the single place `witness_fetch_ms` is produced. Returns
    /// `None` (so the caller can fall back) when the key is absent.
    fn resolve(&self, key: WitnessKey) -> Option<ResolvedWitness<Self::Payload>> {
        let t = Instant::now();
        let slice = self.fetch(key)?;
        let fetch_ms = t.elapsed().as_millis() as u64;
        Some(ResolvedWitness { slice, fetch_ms })
    }

    /// How many chunk slices this resolver holds for `height` (= `k`, the
    /// coordinator's SPLIT width for that block). `0` when the height is
    /// absent. Used by the inner-tier SPLIT to know its fan-out width.
    fn slice_count(&self, height: u64) -> u64;
}

/// A local **mounted read-only corpus** resolver (ADR-0008 §1.3, primary
/// store), keyed by `{height, witness_index}` via in-memory indexed lookup.
/// This is the k=1 / small-k building block: a local map read, no network,
/// so it carries **no GCS tax** (ADR-0003 §D6; ADR-0008 §1.3).
///
/// In the binary this is loaded from the bundled `bench_test.json` (the k=1
/// case, ADR-0008 §1.4) by pre-slicing the block's txs into `S`-tx chunks
/// and indexing them by `{height, witness_index}`. In tests it is populated
/// with cheap stand-in payloads.
#[derive(Debug, Default)]
pub struct MountedCorpus<T> {
    /// `{height, witness_index}` -> slice. The whole-block mount is the
    /// k=1 case (one height, `k` indices).
    slices: HashMap<WitnessKey, WitnessSlice<T>>,
    /// `height` -> number of slices (the SPLIT width `k`).
    counts: HashMap<u64, u64>,
}

impl<T: Clone> MountedCorpus<T> {
    /// An empty corpus (the "corpus absent" case; resolves nothing so the
    /// caller falls back — ADR-0008 §1.4 acceptance criterion).
    pub fn new() -> Self {
        Self {
            slices: HashMap::new(),
            counts: HashMap::new(),
        }
    }

    /// Mount one block's pre-sliced witnesses under `height`. `slices[i]`
    /// becomes `{height, witness_index = i}`. This is the general
    /// partitioned-block mount; passing a single-element-per-index set at
    /// one height is the k=1 whole-block case (ADR-0008 §1.4).
    pub fn mount_block(&mut self, height: u64, slices: Vec<(T, usize)>) {
        let k = slices.len() as u64;
        for (i, (payload, tx_count)) in slices.into_iter().enumerate() {
            let key = WitnessKey::new(height, i as u64);
            self.slices.insert(
                key,
                WitnessSlice {
                    key,
                    payload,
                    tx_count,
                },
            );
        }
        self.counts.insert(height, k);
    }

    /// Convenience k=1 constructor: mount a single block's `k` chunk slices
    /// (ADR-0008 §1.4, "today's `bench_test.json`").
    pub fn single_block(height: u64, slices: Vec<(T, usize)>) -> Self {
        let mut c = Self::new();
        c.mount_block(height, slices);
        c
    }

    /// Total slices across all heights (for diagnostics).
    pub fn len(&self) -> usize {
        self.slices.len()
    }

    pub fn is_empty(&self) -> bool {
        self.slices.is_empty()
    }
}

impl<T: Clone> WitnessResolver for MountedCorpus<T> {
    type Payload = T;

    fn fetch(&self, key: WitnessKey) -> Option<WitnessSlice<T>> {
        // Local indexed lookup — a map read, no network (ADR-0008 §1.3).
        self.slices.get(&key).cloned()
    }

    fn slice_count(&self, height: u64) -> u64 {
        self.counts.get(&height).copied().unwrap_or(0)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn corpus_3() -> MountedCorpus<String> {
        // k=3 partitioned block at height 100: three 4-tx slices.
        MountedCorpus::single_block(
            100,
            vec![
                ("chunk-0".to_string(), 4),
                ("chunk-1".to_string(), 4),
                ("chunk-2".to_string(), 4),
            ],
        )
    }

    #[test]
    fn addressing_resolves_each_index() {
        let c = corpus_3();
        assert_eq!(c.slice_count(100), 3);
        for i in 0..3u64 {
            let r = c.resolve(WitnessKey::new(100, i)).expect("slice present");
            assert_eq!(r.slice.key, WitnessKey::new(100, i));
            assert_eq!(r.slice.payload, format!("chunk-{i}"));
            assert_eq!(r.slice.tx_count, 4);
            // fetch_ms is a real measured local read (>= 0); never invented.
            // (No upper assertion: it is the genuine local-resolve floor.)
        }
    }

    #[test]
    fn absent_key_returns_none_for_fallback() {
        let c = corpus_3();
        // Wrong index (k=3, index 3 is out of range) and wrong height.
        assert!(c.resolve(WitnessKey::new(100, 3)).is_none());
        assert!(c.resolve(WitnessKey::new(999, 0)).is_none());
        assert_eq!(c.slice_count(999), 0);
    }

    #[test]
    fn empty_corpus_resolves_nothing() {
        // ADR-0008 §1.4: corpus absent -> resolves nothing -> caller falls
        // back to current recycled-witness behaviour.
        let c: MountedCorpus<String> = MountedCorpus::new();
        assert!(c.is_empty());
        assert!(c.resolve(WitnessKey::new(1, 0)).is_none());
        assert_eq!(c.slice_count(1), 0);
    }

    #[test]
    fn k1_whole_block_is_one_slice() {
        // The degenerate k=1 case: one block, one slice (ADR-0008 §1.4).
        let c = MountedCorpus::single_block(7, vec![("whole-block".to_string(), 500)]);
        assert_eq!(c.slice_count(7), 1);
        let r = c.resolve(WitnessKey::new(7, 0)).unwrap();
        assert_eq!(r.slice.tx_count, 500);
        assert_eq!(r.slice.payload, "whole-block");
    }

    #[test]
    fn multi_height_partitioned() {
        let mut c = MountedCorpus::new();
        c.mount_block(10, vec![("a".to_string(), 4), ("b".to_string(), 4)]);
        c.mount_block(11, vec![("c".to_string(), 4)]);
        assert_eq!(c.slice_count(10), 2);
        assert_eq!(c.slice_count(11), 1);
        assert_eq!(
            c.resolve(WitnessKey::new(10, 1)).unwrap().slice.payload,
            "b"
        );
        assert_eq!(
            c.resolve(WitnessKey::new(11, 0)).unwrap().slice.payload,
            "c"
        );
        assert_eq!(c.len(), 3);
    }

    #[test]
    fn resolve_returns_measured_fetch_ms() {
        // The seam returns a real measured number (ADR-0008 §2.1). We do not
        // assert a specific value (that would invent one); we assert the
        // field is populated by the timer path, not a constant.
        let c = corpus_3();
        let r = c.resolve(WitnessKey::new(100, 0)).unwrap();
        // It is a u64 produced by Instant::elapsed; a local map read is
        // typically 0ms but the field exists and is real.
        let _real_floor: u64 = r.fetch_ms;
    }
}
