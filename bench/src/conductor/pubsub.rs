// Copyright (c) Elliot Technologies, Inc.
// SPDX-License-Identifier: BUSL-1.1

//! The REAL Pub/Sub transport — the cloud drop-in behind the conductor's
//! dispatch seams (issue #172; `Refs #75 #61 #144`).
//!
//! ## What this is
//!
//! ADR-0006 §1.1 names Pub/Sub as the real backing of the OUTER block-dispatch
//! tier and §1.2 describes the INNER chunk fan-out. The merged conductor lib
//! (#164) shipped the in-process [`crate::conductor::queue::LocalBlockQueue`]
//! simulation behind the [`BlockQueue`] trait, explicitly leaving "a real
//! Pub/Sub adapter as a future drop-in behind `BlockQueue`". **This module is
//! that drop-in**, and it adds the two extra planes the live distributed run
//! needs: the **chunk-dispatch** topic (coordinator → cells) and the
//! **results** topic (cells → coordinator).
//!
//! Cells are SEPARATE PODS that receive work over the network — not threads.
//!
//! ## Transport choice: shell out to the `gcloud pubsub` CLI (DOCUMENTED)
//!
//! This adapter drives Pub/Sub by invoking the **`gcloud pubsub` CLI**, which
//! is already installed in the runtime image (`cicd/Containerfile` installs
//! `google-cloud-cli`; `cicd/entrypoint.sh` already uses `gcloud storage`).
//!
//! Why not the `google-cloud-pubsub` crate or a native gRPC/REST client:
//!
//! - There is **zero** async runtime, HTTP, or TLS dependency anywhere in this
//!   workspace today. The native crate pulls a heavy `tokio` + `tonic` (gRPC)
//!   + TLS tree.
//! - The image is **cross-compiled for `aarch64` (neoverse-v2 / Google Axion)**
//!   on an x86 Cloud Build worker (`cicd/cloudbuild.yaml`). Introducing an
//!   async-TLS dependency tree is a real cross-compile risk on that path.
//! - Shelling out to `gcloud` adds **no new Rust dependency** → **no
//!   cross-compile risk**, and reuses the exact Application Default
//!   Credentials / Workload-Identity auth path the image already supports.
//! - The shell-exec latency (tens of ms) is irrelevant next to multi-second
//!   ZK proofs — this is a benchmark of the prove path, not of the bus.
//!
//! A native client is a clean future drop-in: it implements the same
//! [`BlockQueue`] trait and the same `publish_chunk` / `pull_chunk` /
//! `publish_result` / `pull_result` shapes. This module isolates **all** of
//! `gcloud` behind a small surface so that swap touches one file.
//!
//! ## Message schemas (DOCUMENTED — also in docs/distributed-prover-runtime.md)
//!
//! All three message bodies are JSON, carry **references not bytes** (ADR-0008
//! §1.2 — the witness never travels the bus), and are `serde`-(de)serialized:
//!
//! - **Dispatch (block) message** → [`BlockMessage`]:
//!   `{ "height": u64, "tx_count": u64 }`
//! - **Chunk message** → [`ChunkMessage`]:
//!   `{ "height": u64, "witness_index": u64, "tx_count": u64 }`
//! - **Chunk result message** → [`ChunkResultMessage`]:
//!   `{ "height": u64, "witness_index": u64, "prove_ms": u64,
//!      "witness_fetch_ms": u64 | null, "ok": bool, "cell": String }`
//!
//! ## What is unit-testable WITHOUT gcloud
//!
//! The message (de)serialization round-trips and the
//! `gcloud`-argument-vector construction are pure and fully unit-tested here.
//! The live pull/publish/ack obviously require a real subscription (or the
//! Pub/Sub emulator) and only run on GKE / against the emulator — see the
//! module tests and `docs/distributed-prover-runtime.md`.

use std::process::Command;

use serde::{Deserialize, Serialize};

use crate::conductor::queue::{BlockJob, BlockQueue};

/// One OUTER-tier block-dispatch message body (ADR-0006 §1.1). Mirrors
/// [`BlockJob`] on the wire. References only — no witness bytes.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct BlockMessage {
    pub height: u64,
    pub tx_count: u64,
}

impl From<BlockJob> for BlockMessage {
    fn from(j: BlockJob) -> Self {
        Self {
            height: j.height,
            tx_count: j.tx_count,
        }
    }
}

impl From<BlockMessage> for BlockJob {
    fn from(m: BlockMessage) -> Self {
        BlockJob::new(m.height, m.tx_count)
    }
}

/// One INNER-tier chunk-dispatch message body (ADR-0006 §1.2; ADR-0008 §1.2).
/// Carries the `{height, witness_index}` **reference** the cell resolves
/// locally — never the witness bytes.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct ChunkMessage {
    pub height: u64,
    pub witness_index: u64,
    /// txs in this chunk (`S`) — diagnostics / split bookkeeping.
    pub tx_count: u64,
}

/// One chunk RESULT message body (cells → coordinator). Carries the measured
/// `prove_ms` and the real local-resolve floor `witness_fetch_ms` (ADR-0008
/// §2.1/§2.3 — never `witness_move`). `ok=false` reports an honest failure;
/// no proof output is ever fabricated.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ChunkResultMessage {
    pub height: u64,
    pub witness_index: u64,
    pub prove_ms: u64,
    /// `None` when the chunk was not routed through the witness plane.
    pub witness_fetch_ms: Option<u64>,
    pub ok: bool,
    /// Reporting cell identity (hostname) — for attribution only.
    pub cell: String,
}

impl BlockMessage {
    pub fn to_json(&self) -> String {
        serde_json::to_string(self).expect("BlockMessage serializes")
    }
    pub fn from_json(s: &str) -> Result<Self, serde_json::Error> {
        serde_json::from_str(s)
    }
}

impl ChunkMessage {
    pub fn new(height: u64, witness_index: u64, tx_count: u64) -> Self {
        Self {
            height,
            witness_index,
            tx_count,
        }
    }
    pub fn to_json(&self) -> String {
        serde_json::to_string(self).expect("ChunkMessage serializes")
    }
    pub fn from_json(s: &str) -> Result<Self, serde_json::Error> {
        serde_json::from_str(s)
    }
}

impl ChunkResultMessage {
    pub fn to_json(&self) -> String {
        serde_json::to_string(self).expect("ChunkResultMessage serializes")
    }
    pub fn from_json(s: &str) -> Result<Self, serde_json::Error> {
        serde_json::from_str(s)
    }
}

/// Configuration for the gcloud Pub/Sub transport. Every field is sourced
/// from a flag or env var by the binary; this struct carries the resolved
/// values so the transport itself reads no globals.
#[derive(Debug, Clone)]
pub struct PubSubConfig {
    /// GCP project that owns the topics/subscriptions.
    pub project: String,
    /// OUTER block-dispatch topic (coordinator publishes blocks here when
    /// acting as feeder; usually a separate feeder publishes).
    pub dispatch_topic: String,
    /// OUTER block-dispatch subscription (coordinators competing-pull).
    pub dispatch_subscription: String,
    /// INNER chunk-dispatch topic (coordinator → cells).
    pub chunk_topic: String,
    /// INNER chunk-dispatch subscription (cells competing-pull).
    pub chunk_subscription: String,
    /// Results topic (cells → coordinator).
    pub results_topic: String,
    /// Results subscription (coordinator pulls chunk results).
    pub results_subscription: String,
    /// `gcloud` binary path (default `gcloud`; overridable for the emulator
    /// or a vendored CLI).
    pub gcloud_bin: String,
}

impl PubSubConfig {
    /// Build the `gcloud pubsub subscriptions pull` argv for `subscription`,
    /// pulling at most `limit` messages and auto-acking. Returns the argument
    /// vector (pure — no process spawned), so it is unit-testable.
    pub fn pull_argv(&self, subscription: &str, limit: u32) -> Vec<String> {
        vec![
            "pubsub".into(),
            "subscriptions".into(),
            "pull".into(),
            subscription.into(),
            format!("--project={}", self.project),
            format!("--limit={limit}"),
            // Auto-ack: the local adapter pulled-and-owns. ADR-0006 §1.1's
            // "ack after the block proof is emitted" is the stricter contract
            // a native client should honor (manual ack); the gcloud CLI's
            // ergonomic path is auto-ack, documented as a known relaxation in
            // docs/distributed-prover-runtime.md.
            "--auto-ack".into(),
            "--format=json".into(),
        ]
    }

    /// Build the `gcloud pubsub topics publish` argv for `topic` with `body`.
    pub fn publish_argv(&self, topic: &str, body: &str) -> Vec<String> {
        vec![
            "pubsub".into(),
            "topics".into(),
            "publish".into(),
            topic.into(),
            format!("--project={}", self.project),
            format!("--message={body}"),
        ]
    }
}

/// One pulled-and-decoded message body (the inner `message.data` already
/// base64-decoded by gcloud's `--format=json` ... actually gcloud returns
/// the data base64-encoded; we decode it). Public so the binary can map it.
#[derive(Debug, Clone)]
pub struct PulledMessage {
    /// The decoded UTF-8 message body (the JSON we published).
    pub data: String,
}

/// The gcloud-backed Pub/Sub transport. Holds the resolved [`PubSubConfig`]
/// and drives the CLI. All network/auth is delegated to `gcloud`.
#[derive(Debug, Clone)]
pub struct GcloudPubSub {
    cfg: PubSubConfig,
}

impl GcloudPubSub {
    pub fn new(cfg: PubSubConfig) -> Self {
        Self { cfg }
    }

    pub fn config(&self) -> &PubSubConfig {
        &self.cfg
    }

    /// Publish a raw JSON `body` to `topic`. Returns the published message id
    /// string on success.
    pub fn publish(&self, topic: &str, body: &str) -> std::io::Result<String> {
        let argv = self.cfg.publish_argv(topic, body);
        let out = Command::new(&self.cfg.gcloud_bin).args(&argv).output()?;
        if !out.status.success() {
            return Err(std::io::Error::other(format!(
                "gcloud publish to {topic} failed: {}",
                String::from_utf8_lossy(&out.stderr).trim()
            )));
        }
        Ok(String::from_utf8_lossy(&out.stdout).trim().to_string())
    }

    /// Pull up to `limit` messages from `subscription`, returning their
    /// decoded JSON bodies. An empty pull returns an empty vec (not an error).
    pub fn pull(&self, subscription: &str, limit: u32) -> std::io::Result<Vec<PulledMessage>> {
        let argv = self.cfg.pull_argv(subscription, limit);
        let out = Command::new(&self.cfg.gcloud_bin).args(&argv).output()?;
        if !out.status.success() {
            return Err(std::io::Error::other(format!(
                "gcloud pull from {subscription} failed: {}",
                String::from_utf8_lossy(&out.stderr).trim()
            )));
        }
        let stdout = String::from_utf8_lossy(&out.stdout);
        parse_pull_json(&stdout)
    }

    // ---- typed convenience wrappers ----

    pub fn publish_block(&self, msg: &BlockMessage) -> std::io::Result<String> {
        self.publish(&self.cfg.dispatch_topic, &msg.to_json())
    }

    pub fn publish_chunk(&self, msg: &ChunkMessage) -> std::io::Result<String> {
        self.publish(&self.cfg.chunk_topic, &msg.to_json())
    }

    pub fn publish_result(&self, msg: &ChunkResultMessage) -> std::io::Result<String> {
        self.publish(&self.cfg.results_topic, &msg.to_json())
    }

    /// Pull one block from the dispatch subscription (competing-pull,
    /// ADR-0006 §1.1). `Ok(None)` = empty queue.
    pub fn pull_block(&self) -> std::io::Result<Option<BlockMessage>> {
        let msgs = self.pull(&self.cfg.dispatch_subscription, 1)?;
        Ok(msgs
            .into_iter()
            .next()
            .and_then(|m| BlockMessage::from_json(&m.data).ok()))
    }

    /// Pull up to `limit` chunks from the chunk subscription (cells compete).
    pub fn pull_chunks(&self, limit: u32) -> std::io::Result<Vec<ChunkMessage>> {
        let msgs = self.pull(&self.cfg.chunk_subscription, limit)?;
        Ok(msgs
            .into_iter()
            .filter_map(|m| ChunkMessage::from_json(&m.data).ok())
            .collect())
    }

    /// Pull up to `limit` chunk results from the results subscription.
    pub fn pull_results(&self, limit: u32) -> std::io::Result<Vec<ChunkResultMessage>> {
        let msgs = self.pull(&self.cfg.results_subscription, limit)?;
        Ok(msgs
            .into_iter()
            .filter_map(|m| ChunkResultMessage::from_json(&m.data).ok())
            .collect())
    }
}

/// The OUTER block-dispatch seam, backed by real Pub/Sub. This is the cloud
/// drop-in the merged conductor lib (#164) named: it implements the SAME
/// [`BlockQueue`] trait as [`crate::conductor::queue::LocalBlockQueue`], so
/// the coordinator code path is transport-agnostic.
///
/// `pull` maps to a competing-pull from the dispatch subscription; `publish`
/// to a topic publish; `ack`/`nack` are no-ops under the CLI's `--auto-ack`
/// (documented relaxation — a native client would do manual ack-after-proof).
/// `backlog` is not cheaply available via the CLI without a monitoring read,
/// so it reports `0` (the HPA reads the real backlog from Cloud Monitoring,
/// `pubsub.googleapis.com|subscription|num_undelivered_messages`, per
/// `cicd/terraform/gke/main.tf` — not from this process).
impl BlockQueue for GcloudPubSub {
    fn publish(&self, job: BlockJob) {
        let msg = BlockMessage::from(job);
        if let Err(e) = self.publish_block(&msg) {
            log::warn!("pubsub publish_block failed: {e}");
        }
    }

    fn pull(&self) -> Option<BlockJob> {
        match self.pull_block() {
            Ok(opt) => opt.map(BlockJob::from),
            Err(e) => {
                log::warn!("pubsub pull_block failed: {e}");
                None
            }
        }
    }

    fn ack(&self, _job: BlockJob) {
        // Auto-acked at pull time by the CLI (documented relaxation).
    }

    fn nack(&self, _job: BlockJob) {
        // Auto-ack means no redelivery via this adapter. Pub/Sub's own
        // ack-deadline expiry redelivers un-acked messages when a native
        // manual-ack client is used (the documented future upgrade).
    }

    fn backlog(&self) -> usize {
        0
    }
}

/// Parse the JSON array `gcloud pubsub subscriptions pull --format=json`
/// emits, returning the decoded message bodies. The CLI returns each entry as
/// `{"ackId": "...", "message": {"data": "<base64>", ...}}` (or, on some
/// versions, the message fields flattened). We tolerate both shapes and
/// base64-decode `data`. An empty pull yields `[]` (or empty stdout).
pub fn parse_pull_json(stdout: &str) -> std::io::Result<Vec<PulledMessage>> {
    let trimmed = stdout.trim();
    if trimmed.is_empty() || trimmed == "[]" || trimmed == "null" {
        return Ok(Vec::new());
    }
    let val: serde_json::Value = serde_json::from_str(trimmed)
        .map_err(|e| std::io::Error::other(format!("pull json parse: {e}")))?;
    let arr = match val.as_array() {
        Some(a) => a,
        None => return Ok(Vec::new()),
    };
    let mut out = Vec::with_capacity(arr.len());
    for entry in arr {
        // Two tolerated shapes: {"message":{"data":...}} or {"data":...}.
        let data_b64 = entry
            .get("message")
            .and_then(|m| m.get("data"))
            .or_else(|| entry.get("data"))
            .and_then(|d| d.as_str());
        if let Some(b64) = data_b64 {
            match base64_decode(b64) {
                Some(bytes) => {
                    if let Ok(s) = String::from_utf8(bytes) {
                        out.push(PulledMessage { data: s });
                    }
                }
                None => {
                    // gcloud sometimes returns already-decoded data on certain
                    // configurations; fall back to the raw string.
                    out.push(PulledMessage {
                        data: b64.to_string(),
                    });
                }
            }
        }
    }
    Ok(out)
}

/// Minimal standard-base64 decoder (no external dep — same dependency-free
/// discipline as the rest of this module). Returns `None` on invalid input.
fn base64_decode(input: &str) -> Option<Vec<u8>> {
    const PAD: u8 = b'=';
    fn val(c: u8) -> Option<u8> {
        match c {
            b'A'..=b'Z' => Some(c - b'A'),
            b'a'..=b'z' => Some(c - b'a' + 26),
            b'0'..=b'9' => Some(c - b'0' + 52),
            b'+' => Some(62),
            b'/' => Some(63),
            _ => None,
        }
    }
    let bytes: Vec<u8> = input
        .bytes()
        .filter(|b| !b.is_ascii_whitespace())
        .collect();
    if bytes.is_empty() {
        return Some(Vec::new());
    }
    if bytes.len() % 4 != 0 {
        return None;
    }
    let mut out = Vec::with_capacity(bytes.len() / 4 * 3);
    for chunk in bytes.chunks(4) {
        let pad = chunk.iter().filter(|&&c| c == PAD).count();
        let mut acc: u32 = 0;
        for &c in chunk {
            let v = if c == PAD { 0 } else { val(c)? };
            acc = (acc << 6) | v as u32;
        }
        out.push((acc >> 16) as u8);
        if pad < 2 {
            out.push((acc >> 8) as u8);
        }
        if pad < 1 {
            out.push(acc as u8);
        }
    }
    Some(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cfg() -> PubSubConfig {
        PubSubConfig {
            project: "kunal-scratch".into(),
            dispatch_topic: "disp-t".into(),
            dispatch_subscription: "disp-s".into(),
            chunk_topic: "chunk-t".into(),
            chunk_subscription: "chunk-s".into(),
            results_topic: "res-t".into(),
            results_subscription: "res-s".into(),
            gcloud_bin: "gcloud".into(),
        }
    }

    #[test]
    fn block_message_roundtrip() {
        let m = BlockMessage {
            height: 186_974_616,
            tx_count: 450,
        };
        let json = m.to_json();
        assert!(json.contains("\"height\":186974616"));
        assert!(json.contains("\"tx_count\":450"));
        assert_eq!(BlockMessage::from_json(&json).unwrap(), m);
        // BlockJob bridge is lossless.
        let job: BlockJob = m.into();
        assert_eq!(job, BlockJob::new(186_974_616, 450));
        assert_eq!(BlockMessage::from(job), m);
    }

    #[test]
    fn chunk_message_roundtrip() {
        let m = ChunkMessage::new(186_974_616, 3, 100);
        let json = m.to_json();
        assert!(json.contains("\"witness_index\":3"));
        assert_eq!(ChunkMessage::from_json(&json).unwrap(), m);
    }

    #[test]
    fn chunk_result_roundtrip_with_and_without_fetch() {
        let m = ChunkResultMessage {
            height: 100,
            witness_index: 2,
            prove_ms: 2310,
            witness_fetch_ms: Some(4),
            ok: true,
            cell: "cell-abc".into(),
        };
        let json = m.to_json();
        assert!(json.contains("\"witness_fetch_ms\":4"));
        assert!(json.contains("\"ok\":true"));
        assert_eq!(ChunkResultMessage::from_json(&json).unwrap(), m);

        let m2 = ChunkResultMessage {
            height: 100,
            witness_index: 5,
            prove_ms: 0,
            witness_fetch_ms: None,
            ok: false,
            cell: "cell-xyz".into(),
        };
        let json2 = m2.to_json();
        assert!(json2.contains("\"witness_fetch_ms\":null"));
        assert!(json2.contains("\"ok\":false"));
        assert_eq!(ChunkResultMessage::from_json(&json2).unwrap(), m2);
    }

    #[test]
    fn pull_argv_is_correct() {
        let c = cfg();
        let argv = c.pull_argv("chunk-s", 8);
        assert_eq!(argv[0], "pubsub");
        assert_eq!(argv[1], "subscriptions");
        assert_eq!(argv[2], "pull");
        assert_eq!(argv[3], "chunk-s");
        assert!(argv.contains(&"--project=kunal-scratch".to_string()));
        assert!(argv.contains(&"--limit=8".to_string()));
        assert!(argv.contains(&"--auto-ack".to_string()));
        assert!(argv.contains(&"--format=json".to_string()));
    }

    #[test]
    fn publish_argv_is_correct() {
        let c = cfg();
        let body = ChunkMessage::new(7, 0, 500).to_json();
        let argv = c.publish_argv("chunk-t", &body);
        assert_eq!(argv[0], "pubsub");
        assert_eq!(argv[1], "topics");
        assert_eq!(argv[2], "publish");
        assert_eq!(argv[3], "chunk-t");
        assert!(argv.contains(&format!("--message={body}")));
    }

    #[test]
    fn base64_roundtrip_known_vectors() {
        // Standard RFC 4648 vectors.
        assert_eq!(base64_decode("").unwrap(), b"");
        assert_eq!(base64_decode("Zg==").unwrap(), b"f");
        assert_eq!(base64_decode("Zm8=").unwrap(), b"fo");
        assert_eq!(base64_decode("Zm9v").unwrap(), b"foo");
        assert_eq!(base64_decode("Zm9vYg==").unwrap(), b"foob");
        assert_eq!(base64_decode("Zm9vYmE=").unwrap(), b"fooba");
        assert_eq!(base64_decode("Zm9vYmFy").unwrap(), b"foobar");
    }

    #[test]
    fn parse_pull_json_decodes_block_message() {
        // Emulate gcloud's --format=json output: data is base64 of our JSON.
        let body = BlockMessage {
            height: 42,
            tx_count: 500,
        }
        .to_json();
        let b64 = base64_encode(body.as_bytes());
        let stdout = format!(r#"[{{"ackId":"A1","message":{{"data":"{b64}"}}}}]"#);
        let pulled = parse_pull_json(&stdout).unwrap();
        assert_eq!(pulled.len(), 1);
        let decoded = BlockMessage::from_json(&pulled[0].data).unwrap();
        assert_eq!(decoded, BlockMessage { height: 42, tx_count: 500 });
    }

    #[test]
    fn parse_pull_json_empty_is_empty() {
        assert!(parse_pull_json("").unwrap().is_empty());
        assert!(parse_pull_json("[]").unwrap().is_empty());
        assert!(parse_pull_json("null").unwrap().is_empty());
    }

    #[test]
    fn parse_pull_json_flattened_shape() {
        // Some gcloud versions flatten the message fields.
        let body = ChunkMessage::new(9, 1, 100).to_json();
        let b64 = base64_encode(body.as_bytes());
        let stdout = format!(r#"[{{"data":"{b64}","ackId":"A2"}}]"#);
        let pulled = parse_pull_json(&stdout).unwrap();
        assert_eq!(pulled.len(), 1);
        assert_eq!(
            ChunkMessage::from_json(&pulled[0].data).unwrap(),
            ChunkMessage::new(9, 1, 100)
        );
    }

    /// Test-only base64 encoder (mirrors the decoder; not used in prod).
    fn base64_encode(input: &[u8]) -> String {
        const TABLE: &[u8; 64] =
            b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
        let mut out = String::new();
        for chunk in input.chunks(3) {
            let b0 = chunk[0] as u32;
            let b1 = chunk.get(1).copied().unwrap_or(0) as u32;
            let b2 = chunk.get(2).copied().unwrap_or(0) as u32;
            let n = (b0 << 16) | (b1 << 8) | b2;
            out.push(TABLE[((n >> 18) & 63) as usize] as char);
            out.push(TABLE[((n >> 12) & 63) as usize] as char);
            if chunk.len() > 1 {
                out.push(TABLE[((n >> 6) & 63) as usize] as char);
            } else {
                out.push('=');
            }
            if chunk.len() > 2 {
                out.push(TABLE[(n & 63) as usize] as char);
            } else {
                out.push('=');
            }
        }
        out
    }
}
