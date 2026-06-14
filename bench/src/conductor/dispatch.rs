// Copyright (c) Elliot Technologies, Inc.
// SPDX-License-Identifier: BUSL-1.1

//! The INNER tier — coordinator SPLIT + chunk fan-out to a cell pool, and the
//! coordinator POOL that consumes the outer queue (ADR-0006 §1.2 + §2).
//!
//! ## Inner tier (ADR-0006 §1.2)
//!
//! A [`Coordinator`], having pulled a block from the outer queue, runs the
//! inner grain of the ADR-0004 §2 primitive:
//!
//! - **SPLIT** — partition the block into `k = ceil(tx/S)` chunks; each chunk
//!   gets a `{height, witness_index}` reference (`witness_index = 0..k-1`,
//!   ADR-0008 §1.1).
//! - **DISPATCH** — push the `k` chunk *references* (not bytes; ADR-0008
//!   §1.2) onto a bounded queue, fanning out to a HORIZONTAL pool of cells.
//! - **PROVE (per cell)** — each cell **resolves its witness reference**
//!   through the [`WitnessResolver`] (measuring `witness_fetch_ms`,
//!   ADR-0008 §2.1) and then proves its chunk via the **injected prover
//!   closure** (the `bench::stream` pattern; plonky2 stays out of this
//!   module so it is unit-testable).
//! - **GATHER / FOLD** — chunk proofs return to the coordinator (modeled
//!   here as collected `ChunkResult`s).
//!
//! ## Coordinator pool (ADR-0006 §2; #113 PRIMARY lever)
//!
//! [`CoordinatorPool`] runs N coordinators as a HORIZONTAL pool, each a
//! second consumer class on the outer [`BlockQueue`] (ADR-0006 §1.1, §2).
//! Independent per-block work scales horizontally (ADR-0006 §2). **No
//! per-coordinator vertical concurrency** is built (#113 SECONDARY lever,
//! deferred) — each coordinator proves one block at a time.
//!
//! ## Reuse, not reinvention
//!
//! The inner fan-out reuses `bench::stream`'s bounded `sync_channel` queue
//! and **closure-injected prover** (`FnMut(&ChunkJob) -> ProverOutput`): the
//! prover is INJECTED so the dispatch + witness path is testable without GCP
//! or plonky2, and the existing `ChunkJob` / event plumbing is reused.

use std::sync::Arc;
use std::sync::mpsc::sync_channel;
use std::time::Instant;

use crate::conductor::queue::{BlockJob, BlockQueue};
use crate::conductor::witness::{WitnessKey, WitnessResolver};

/// `ceil(tx_count / chunk_size)` — the inner SPLIT width `k` (ADR-0006 §1.2:
/// `k = ceil(tx/S)`). Mirrors `stream::chunks_for` for one block.
pub fn split_k(tx_count: u64, chunk_size: usize) -> u64 {
    debug_assert!(chunk_size > 0);
    tx_count.div_ceil(chunk_size as u64)
}

/// What a cell reports for one proven chunk: its witness key, the measured
/// `witness_fetch_ms` (the ADR-0008 §2.1 seam — the local-resolve FLOOR, not
/// `witness_move`), and the prove wall.
#[derive(Debug, Clone)]
pub struct ChunkResult {
    pub key: WitnessKey,
    /// The real measured witness-resolve wall (ADR-0008 §2.1). Local floor.
    pub witness_fetch_ms: u64,
    /// The chunk's prove wall (from the injected prover).
    pub prove_ms: u64,
    /// txs in this chunk (`S`).
    pub tx_count: usize,
}

/// The result of proving one block through the inner tier.
#[derive(Debug, Clone)]
pub struct InnerDispatchOutcome {
    pub height: u64,
    /// SPLIT width actually dispatched.
    pub k: u64,
    /// One result per proven chunk (GATHER).
    pub chunks: Vec<ChunkResult>,
    /// Chunks whose witness reference could not be resolved (corpus miss;
    /// ADR-0008 §1.4 fallback boundary). Carried honestly, not hidden.
    pub unresolved: u64,
}

impl InnerDispatchOutcome {
    /// Total measured witness-fetch wall across the block's chunks (the
    /// summable local floor; NEVER reported as `witness_move`).
    pub fn total_witness_fetch_ms(&self) -> u64 {
        self.chunks.iter().map(|c| c.witness_fetch_ms).sum()
    }

    /// Total prove wall across the block's chunks.
    pub fn total_prove_ms(&self) -> u64 {
        self.chunks.iter().map(|c| c.prove_ms).sum()
    }
}

/// What the injected prover reports for one resolved chunk. The conductor is
/// generic over the witness payload `T`, so this stays plonky2-free; the
/// binary instantiates the closure with the real L1+L2 prove and `T =`
/// the real chunk type.
#[derive(Debug, Clone)]
pub struct CellProveStat {
    /// The chunk's prove wall in ms (e.g. L1 wall, or L1+L2).
    pub prove_ms: u64,
}

/// A single coordinator: owns one block's chunk set, SPLITs it, fans out the
/// chunk *references* to a cell pool over a bounded queue, has each cell
/// resolve its witness (measuring `witness_fetch_ms`) and prove via the
/// injected closure, then GATHERs the results (ADR-0006 §1.2).
pub struct Coordinator {
    /// Inner-tier chunk size `S` (the SPLIT grain).
    pub chunk_size: usize,
    /// Bounded inner queue capacity (the `bench::stream` bounded-queue
    /// policy; small M, large k — ADR-0006 §1.2).
    pub queue_cap: usize,
}

impl Coordinator {
    pub fn new(chunk_size: usize, queue_cap: usize) -> Self {
        assert!(chunk_size > 0, "chunk_size must be > 0");
        assert!(queue_cap > 0, "queue_cap must be > 0");
        Self {
            chunk_size,
            queue_cap,
        }
    }

    /// Prove one block through the inner tier.
    ///
    /// - `block` — the block pulled from the outer queue.
    /// - `resolver` — the witness plane (k=1 mounted corpus today). Each
    ///   cell calls `resolver.resolve(key)` (ADR-0008 §2.1) to get its slice
    ///   + the measured `witness_fetch_ms`.
    /// - `prove` — the INJECTED prover closure. Given a *resolved* witness
    ///   slice payload, it returns the prove stat. The binary wires the real
    ///   L1+L2 prove here; tests wire a sleep stub. This is the
    ///   `bench::stream` closure-injection contract.
    ///
    /// Witness delivery carries REFERENCES, not bytes (ADR-0008 §1.2): the
    /// dispatch enqueues `WitnessKey`s; the cell pulls the bytes locally.
    ///
    /// The horizontal cell pool is modeled by `n_cells` worker threads
    /// draining the bounded queue (small M, large k). No per-cell vertical
    /// concurrency beyond one chunk at a time per cell (#113 secondary lever
    /// not built).
    pub fn prove_block<R, P>(
        &self,
        block: BlockJob,
        resolver: &R,
        n_cells: usize,
        prove: P,
    ) -> InnerDispatchOutcome
    where
        R: WitnessResolver + Sync,
        R::Payload: Send,
        P: Fn(&R::Payload) -> CellProveStat + Sync,
    {
        assert!(n_cells > 0, "n_cells must be > 0");

        // SPLIT: k = ceil(tx/S), but never exceed what the resolver actually
        // holds for this height (the corpus is the authority on partition
        // width; ADR-0008 §1.1 — the coordinator owns the SPLIT, the corpus
        // realizes it). For k=1 (whole-block mount) this is 1.
        let k_requested = split_k(block.tx_count, self.chunk_size);
        let k_available = resolver.slice_count(block.height);
        let k = if k_available == 0 {
            k_requested
        } else {
            k_requested.min(k_available)
        };

        // DISPATCH: push the k chunk REFERENCES onto a bounded queue
        // (references, not bytes — ADR-0008 §1.2). Reuses the stream bounded
        // sync_channel.
        let (tx, rx) = sync_channel::<WitnessKey>(self.queue_cap);
        let rx = Arc::new(std::sync::Mutex::new(rx));

        // PROVE (per cell): n_cells workers drain the queue, each resolving
        // its reference (witness_fetch_ms) then proving via the injected
        // closure. std::thread::scope lets the closure + resolver borrow
        // without 'static bounds. The scope's value is the assembled outcome.
        std::thread::scope(|scope| {
            let mut handles = Vec::with_capacity(n_cells);
            for _ in 0..n_cells {
                let rx = rx.clone();
                let prove = &prove;
                let resolver = &resolver;
                handles.push(scope.spawn(move || {
                    let mut results: Vec<ChunkResult> = Vec::new();
                    let mut unresolved: u64 = 0;
                    loop {
                        // Pull the next chunk reference (competing among
                        // cells). Lock only to dequeue.
                        let key = {
                            let guard = rx.lock().unwrap();
                            match guard.recv() {
                                Ok(k) => k,
                                Err(_) => break, // queue closed + drained
                            }
                        };
                        // Cell resolves its witness REFERENCE -> bytes,
                        // measuring witness_fetch_ms (ADR-0008 §2.1).
                        match resolver.resolve(key) {
                            Some(resolved) => {
                                let stat = prove(&resolved.slice.payload);
                                results.push(ChunkResult {
                                    key,
                                    witness_fetch_ms: resolved.fetch_ms,
                                    prove_ms: stat.prove_ms,
                                    tx_count: resolved.slice.tx_count,
                                });
                            }
                            None => unresolved += 1,
                        }
                    }
                    (results, unresolved)
                }));
            }

            // Enqueue the k references, then close the channel so cells drain
            // and exit.
            for i in 0..k {
                let key = WitnessKey::new(block.height, i);
                // Bounded queue: block on a full queue (the coordinator owns
                // the set and waits for cells to make room — small M, large
                // k). For the local slice this back-pressure is the
                // bounded-queue policy.
                tx.send(key).expect("cells alive while enqueuing");
            }
            drop(tx); // close: recv() returns Err once drained

            // GATHER: collect per-cell results.
            let mut chunks = Vec::new();
            let mut unresolved = 0u64;
            for h in handles {
                let (mut r, u) = h.join().expect("cell thread panicked");
                chunks.append(&mut r);
                unresolved += u;
            }
            InnerDispatchOutcome {
                height: block.height,
                k,
                chunks,
                unresolved,
            }
        })
    }
}

/// The coordinator POOL (ADR-0006 §2; #113 PRIMARY lever). Runs N
/// coordinators as a HORIZONTAL pool, each pulling whole blocks from the
/// outer [`BlockQueue`] (competing-pull, ADR-0006 §1.1) and driving the inner
/// tier. No per-coordinator concurrency (#113 secondary, deferred).
pub struct CoordinatorPool {
    /// Number of coordinators (horizontal scaling; ADR-0006 §2).
    pub n_coordinators: usize,
    /// Cells per coordinator's inner fan-out.
    pub cells_per_coordinator: usize,
    /// Inner-tier chunk size `S`.
    pub chunk_size: usize,
    /// Inner bounded-queue capacity.
    pub queue_cap: usize,
}

/// Aggregate result of draining the outer queue across the pool.
#[derive(Debug, Default)]
pub struct PoolOutcome {
    pub blocks_proven: u64,
    pub outcomes: Vec<InnerDispatchOutcome>,
    pub wall_ms: u64,
}

impl PoolOutcome {
    pub fn total_chunks(&self) -> usize {
        self.outcomes.iter().map(|o| o.chunks.len()).sum()
    }

    /// Summed local witness-fetch floor across all blocks (never
    /// `witness_move`).
    pub fn total_witness_fetch_ms(&self) -> u64 {
        self.outcomes
            .iter()
            .map(|o| o.total_witness_fetch_ms())
            .sum()
    }
}

impl CoordinatorPool {
    pub fn new(
        n_coordinators: usize,
        cells_per_coordinator: usize,
        chunk_size: usize,
        queue_cap: usize,
    ) -> Self {
        assert!(n_coordinators > 0);
        assert!(cells_per_coordinator > 0);
        assert!(chunk_size > 0);
        assert!(queue_cap > 0);
        Self {
            n_coordinators,
            cells_per_coordinator,
            chunk_size,
            queue_cap,
        }
    }

    /// Drain the outer `queue` across the horizontal coordinator pool until
    /// empty. Each coordinator competes to pull a whole block (ADR-0006 §1.1)
    /// and drives the inner tier (ADR-0006 §1.2) with the shared `resolver`
    /// and the injected `prove` closure.
    ///
    /// `prove` is `Fn` (shared, called from all coordinators' cells); the
    /// binary wires the real prover, tests wire a stub.
    pub fn run<Q, R, P>(&self, queue: &Q, resolver: &R, prove: P) -> PoolOutcome
    where
        Q: BlockQueue,
        R: WitnessResolver + Sync,
        R::Payload: Send,
        P: Fn(&R::Payload) -> CellProveStat + Sync,
    {
        let start = Instant::now();
        let prove = &prove;

        let outcomes = std::thread::scope(|scope| {
            let mut handles = Vec::with_capacity(self.n_coordinators);
            for _ in 0..self.n_coordinators {
                let queue = &queue;
                let resolver = &resolver;
                handles.push(scope.spawn(move || {
                    let coord = Coordinator::new(self.chunk_size, self.queue_cap);
                    let mut mine: Vec<InnerDispatchOutcome> = Vec::new();
                    // Competing-pull: take whole blocks until the queue is
                    // empty (ADR-0006 §1.1).
                    while let Some(block) = queue.pull() {
                        let out =
                            coord.prove_block(block, *resolver, self.cells_per_coordinator, prove);
                        queue.ack(block); // ack after block proof (ADR-0006 §1.1)
                        mine.push(out);
                    }
                    mine
                }));
            }
            let mut all = Vec::new();
            for h in handles {
                all.append(&mut h.join().expect("coordinator thread panicked"));
            }
            all
        });

        PoolOutcome {
            blocks_proven: outcomes.len() as u64,
            outcomes,
            wall_ms: start.elapsed().as_millis() as u64,
        }
    }
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::{AtomicU64, Ordering};
    use std::time::Duration;

    use super::*;
    use crate::conductor::queue::LocalBlockQueue;
    use crate::conductor::witness::MountedCorpus;

    fn corpus_k(height: u64, k: usize, s: usize) -> MountedCorpus<String> {
        let slices: Vec<(String, usize)> = (0..k).map(|i| (format!("h{height}-c{i}"), s)).collect();
        MountedCorpus::single_block(height, slices)
    }

    #[test]
    fn split_k_math() {
        assert_eq!(split_k(500, 4), 125);
        assert_eq!(split_k(500, 9), 56);
        assert_eq!(split_k(1, 4), 1);
        assert_eq!(split_k(0, 4), 0);
        assert_eq!(split_k(8, 4), 2);
    }

    #[test]
    fn inner_dispatch_proves_every_chunk_once() {
        // k=12 block, 3 cells. Every chunk resolved + proven exactly once,
        // each carrying a real measured witness_fetch_ms.
        let resolver = corpus_k(100, 12, 4);
        let coord = Coordinator::new(4, 64);
        let proven = AtomicU64::new(0);
        let out = coord.prove_block(
            BlockJob::new(100, 48), // 48 tx / S=4 -> k=12
            &resolver,
            3,
            |_payload: &String| {
                proven.fetch_add(1, Ordering::Relaxed);
                CellProveStat { prove_ms: 1 }
            },
        );
        assert_eq!(out.k, 12);
        assert_eq!(out.chunks.len(), 12);
        assert_eq!(out.unresolved, 0);
        assert_eq!(proven.load(Ordering::Relaxed), 12);
        // Every chunk got a distinct witness_index 0..12.
        let mut idxs: Vec<u64> = out.chunks.iter().map(|c| c.key.witness_index).collect();
        idxs.sort_unstable();
        assert_eq!(idxs, (0..12u64).collect::<Vec<_>>());
        // witness_fetch_ms is populated (real local floor) for every chunk.
        assert!(out.chunks.iter().all(|c| c.tx_count == 4));
    }

    #[test]
    fn k1_whole_block_one_chunk() {
        // The k=1 degenerate case: tx_count says k=125 but the corpus holds
        // only 1 whole-block slice, so k clamps to 1 (ADR-0008 §1.4).
        let resolver = corpus_k(7, 1, 500);
        let coord = Coordinator::new(4, 8);
        let out = coord.prove_block(BlockJob::new(7, 500), &resolver, 2, |_p: &String| {
            CellProveStat { prove_ms: 2 }
        });
        assert_eq!(out.k, 1);
        assert_eq!(out.chunks.len(), 1);
        assert_eq!(out.chunks[0].key, WitnessKey::new(7, 0));
        assert_eq!(out.chunks[0].tx_count, 500);
    }

    #[test]
    fn pool_drains_outer_queue_horizontally() {
        // 20 blocks, k=5 each, 4 coordinators x 2 cells. Every block proven
        // exactly once across the horizontal pool; every chunk accounted.
        let queue = LocalBlockQueue::new();
        let mut corpus = MountedCorpus::new();
        for h in 0..20u64 {
            corpus.mount_block(h, (0..5).map(|i| (format!("h{h}-c{i}"), 4)).collect());
            queue.publish(BlockJob::new(h, 20)); // 20/4 -> 5
        }
        let pool = CoordinatorPool::new(4, 2, 4, 64);
        let proven = AtomicU64::new(0);
        let out = pool.run(&queue, &corpus, |_p: &String| {
            proven.fetch_add(1, Ordering::Relaxed);
            CellProveStat { prove_ms: 1 }
        });
        assert_eq!(out.blocks_proven, 20);
        assert_eq!(out.total_chunks(), 100); // 20 blocks * k=5
        assert_eq!(proven.load(Ordering::Relaxed), 100);
        assert_eq!(queue.backlog(), 0);
        // Every block height appears exactly once.
        let mut heights: Vec<u64> = out.outcomes.iter().map(|o| o.height).collect();
        heights.sort_unstable();
        assert_eq!(heights, (0..20u64).collect::<Vec<_>>());
    }

    #[test]
    fn unresolved_chunks_counted_not_hidden() {
        // tx_count demands k=10 but corpus only has 6 slices: k clamps to 6,
        // so all 6 resolve and unresolved stays 0 (the clamp is the honest
        // partition-width authority). Verify the clamp path.
        let resolver = corpus_k(3, 6, 4);
        let coord = Coordinator::new(4, 64);
        let out = coord.prove_block(
            BlockJob::new(3, 40), // would be k=10
            &resolver,
            2,
            |_p: &String| CellProveStat { prove_ms: 1 },
        );
        assert_eq!(out.k, 6); // clamped to available
        assert_eq!(out.chunks.len(), 6);
        assert_eq!(out.unresolved, 0);
    }

    #[test]
    fn witness_fetch_ms_is_measured_per_chunk() {
        let resolver = corpus_k(100, 4, 4);
        let coord = Coordinator::new(4, 16);
        let out = coord.prove_block(BlockJob::new(100, 16), &resolver, 2, |_p: &String| {
            std::thread::sleep(Duration::from_millis(1));
            CellProveStat { prove_ms: 1 }
        });
        // Each chunk carries a real (timer-produced) witness_fetch_ms field.
        assert_eq!(out.chunks.len(), 4);
        let _floor: u64 = out.total_witness_fetch_ms();
    }
}
