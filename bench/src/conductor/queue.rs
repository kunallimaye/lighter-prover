// Copyright (c) Elliot Technologies, Inc.
// SPDX-License-Identifier: BUSL-1.1

//! The OUTER tier — block-dispatch queue (ADR-0006 §1.1).
//!
//! The conductor's outer tier is the **competing-pull block-dispatch layer**
//! (ADR-0003 §D2, reused unchanged): the feeder publishes one block event per
//! arriving block, and a **pool** of coordinators competes to pull blocks
//! (ADR-0006 §1.1 DISPATCH row: "**pull** — competing-pull Pub/Sub, one
//! subscription, `maxOutstandingMessages=1`, **ack after the block proof is
//! emitted**"). One block redelivers to another coordinator on death
//! (ADR-0006 §1.1 failure row).
//!
//! ## The trait is the seam
//!
//! ADR-0006 §1.1 names Pub/Sub as the real backing, but this LOCAL slice
//! ships an **in-memory adapter** ([`LocalBlockQueue`]) so the whole path
//! runs WITHOUT GCP. The [`BlockQueue`] trait is the seam a real Pub/Sub
//! adapter (`PubSubBlockQueue`, future #75 cloud milestone) drops into
//! unchanged. No cloud is provisioned by this slice.
//!
//! Competing-pull semantics, modeled locally: [`BlockQueue::pull`] hands each
//! block to **exactly one** caller (the coordinators race; whoever pulls owns
//! the block). [`BlockQueue::ack`] is the post-proof ack. Redelivery on
//! coordinator death is modeled by [`BlockQueue::nack`] (return the block for
//! another puller) — the seam exists; the cloud milestone wires it to
//! Pub/Sub's redelivery.

use std::collections::VecDeque;
use std::sync::Mutex;

/// One outer-tier work item: a block to prove. Carries `height` (the block
/// id) and `tx_count` (for the inner-tier SPLIT math). This is the block
/// event the feeder publishes (ADR-0006 §1.1 SPLIT row).
///
/// Note it carries NO witness bytes — witness delivery is by reference
/// (`{height, witness_index}`) resolved cell-side (ADR-0008 §1.2). The outer
/// item is just the block id + size.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct BlockJob {
    /// The block height (the block id; ADR-0008 §1.1).
    pub height: u64,
    /// Transactions in the block (drives the inner SPLIT `k = ceil(tx/S)`).
    pub tx_count: u64,
}

impl BlockJob {
    pub fn new(height: u64, tx_count: u64) -> Self {
        Self { height, tx_count }
    }
}

/// The OUTER block-dispatch seam (ADR-0006 §1.1). A real Pub/Sub adapter
/// implements this for the cloud milestone; [`LocalBlockQueue`] implements
/// it in-memory for the local slice.
///
/// Semantics (competing-pull, ADR-0006 §1.1):
/// - `publish` — the feeder enqueues a block (the SPLIT step).
/// - `pull` — a coordinator competes to take the next block; exactly one
///   coordinator gets any given block (the DISPATCH step). `None` = empty.
/// - `ack` — the coordinator acks after the block proof is emitted.
/// - `nack` — the coordinator failed; the block is redelivered to the pool
///   (the failure/redelivery unit is a WHOLE BLOCK, ADR-0006 §1.1).
pub trait BlockQueue: Send + Sync {
    fn publish(&self, job: BlockJob);
    fn pull(&self) -> Option<BlockJob>;
    fn ack(&self, job: BlockJob);
    fn nack(&self, job: BlockJob);
    /// Blocks currently waiting to be pulled (diagnostics / backlog metric;
    /// the cloud milestone's MIG autoscales on this — ADR-0006 §5).
    fn backlog(&self) -> usize;
}

/// In-memory competing-pull adapter (LOCAL slice; no GCP). Thread-safe via a
/// single `Mutex<VecDeque>` — sufficient for the host test where a coordinator
/// pool pulls concurrently. A `pull` removes the head and hands it to exactly
/// one caller; a `nack` returns it to the front for redelivery.
#[derive(Debug, Default)]
pub struct LocalBlockQueue {
    inner: Mutex<VecDeque<BlockJob>>,
}

impl LocalBlockQueue {
    pub fn new() -> Self {
        Self {
            inner: Mutex::new(VecDeque::new()),
        }
    }
}

impl BlockQueue for LocalBlockQueue {
    fn publish(&self, job: BlockJob) {
        self.inner.lock().unwrap().push_back(job);
    }

    fn pull(&self) -> Option<BlockJob> {
        self.inner.lock().unwrap().pop_front()
    }

    fn ack(&self, _job: BlockJob) {
        // Local adapter: pull already removed it; ack is a no-op. The real
        // Pub/Sub adapter acks the message here so it is not redelivered.
    }

    fn nack(&self, job: BlockJob) {
        // Redeliver to the FRONT so the failed block is retried promptly by
        // another puller (whole-block redelivery; ADR-0006 §1.1).
        self.inner.lock().unwrap().push_front(job);
    }

    fn backlog(&self) -> usize {
        self.inner.lock().unwrap().len()
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use super::*;

    #[test]
    fn publish_pull_fifo() {
        let q = LocalBlockQueue::new();
        q.publish(BlockJob::new(100, 500));
        q.publish(BlockJob::new(101, 13));
        assert_eq!(q.backlog(), 2);
        assert_eq!(q.pull(), Some(BlockJob::new(100, 500)));
        assert_eq!(q.pull(), Some(BlockJob::new(101, 13)));
        assert_eq!(q.pull(), None);
        assert_eq!(q.backlog(), 0);
    }

    #[test]
    fn nack_redelivers_to_front() {
        let q = LocalBlockQueue::new();
        q.publish(BlockJob::new(1, 4));
        q.publish(BlockJob::new(2, 4));
        let j = q.pull().unwrap();
        assert_eq!(j.height, 1);
        // Coordinator "died": redeliver. It must come back BEFORE block 2.
        q.nack(j);
        assert_eq!(q.pull(), Some(BlockJob::new(1, 4)));
        assert_eq!(q.pull(), Some(BlockJob::new(2, 4)));
    }

    #[test]
    fn competing_pull_hands_each_block_once() {
        // Two coordinators racing to pull from the same queue must each get
        // distinct blocks (exactly-one-owner; ADR-0006 §1.1).
        let q = Arc::new(LocalBlockQueue::new());
        for h in 0..100u64 {
            q.publish(BlockJob::new(h, 4));
        }
        let mut handles = Vec::new();
        for _ in 0..4 {
            let q = q.clone();
            handles.push(std::thread::spawn(move || {
                let mut pulled = Vec::new();
                while let Some(j) = q.pull() {
                    pulled.push(j.height);
                }
                pulled
            }));
        }
        let mut all: Vec<u64> = handles
            .into_iter()
            .flat_map(|h| h.join().unwrap())
            .collect();
        all.sort_unstable();
        // Every block pulled exactly once across the pool, none duplicated.
        assert_eq!(all, (0..100u64).collect::<Vec<_>>());
        assert_eq!(q.backlog(), 0);
    }

    #[test]
    fn ack_is_noop_locally() {
        let q = LocalBlockQueue::new();
        q.publish(BlockJob::new(5, 4));
        let j = q.pull().unwrap();
        q.ack(j); // does not re-add
        assert_eq!(q.backlog(), 0);
        assert_eq!(q.pull(), None);
    }
}
