// Copyright (c) Elliot Technologies, Inc.
// SPDX-License-Identifier: BUSL-1.1

//! The NATIVE in-process Pub/Sub client for the merge-task plane (issue #203).
//!
//! ## Why this exists (the #200 measurement)
//!
//! The live #200 fold-wall matrix pinned the distributed fold's per-level
//! barrier on the **gcloud-CLI poll**: the leader and every fold worker pulled
//! merge tasks/results by SHELLING OUT to `gcloud pubsub subscriptions pull`
//! (see [`super::pubsub::GcloudPubSub::pull`]). Each poll:
//!
//!   - spawns a FRESH `gcloud` process (Python interpreter + ADC auth + a TLS
//!     handshake), ~1–2 s of startup before a single round-trip;
//!   - uses `--auto-ack` (the message is acked the instant it is pulled, NOT
//!     after the merge is proven + uploaded);
//!   - is paced by the caller's poll-interval sleep on an empty pull.
//!
//! That CLI poll — not prove (~0.5 s/merge), not stragglers (≤17 ms), not
//! payload (constant ~422 KB) — dominated the ~4–20 s/level barrier and capped
//! realized speedup at ~1.78× vs a ~3.75× depth-bounded ideal at k=16.
//!
//! ## What this module is
//!
//! A native, IN-PROCESS Pub/Sub client (no process-per-poll) that talks the
//! Pub/Sub REST API over a persistent HTTP connection:
//!
//!   - **streaming-style low-latency delivery**: `pull` with
//!     `returnImmediately=false`, so the Pub/Sub server BLOCKS the request and
//!     returns the instant a message is available (no fixed poll interval, no
//!     fresh process). This is the synchronous-pull analogue of the gRPC
//!     StreamingPull lever named in #200/#203 — same effect (sub-second pickup),
//!     without the heavy `tokio`+`tonic`+`openssl-sys` dependency tree the
//!     `google-cloud-pubsub` crate pulls (which is a real cross-compile risk on
//!     this fork's x86→aarch64 image path; see [`super::pubsub`]'s module docs).
//!   - **manual ack**: a separate `acknowledge` call. A merge task is acked
//!     ONLY after the worker has proven the merge AND uploaded the result
//!     (ack-after-upload), so a crashed worker's task redelivers at-least-once.
//!
//! ## Auth (reuses the image's existing path, but not per-poll)
//!
//! The bearer token comes from `gcloud auth print-access-token` — the SAME
//! Application Default Credentials / Workload-Identity path the rest of the
//! image already uses (`cicd/entrypoint.sh`, [`super::storage`]). Crucially it
//! is fetched ONCE and cached in-process (refreshed shortly before its ~1 h
//! expiry, or on a 401), so the per-poll process spawn the #200 barrier was
//! made of is gone entirely. The token fetch is a single short-lived process
//! call that happens a handful of times over a whole fold, not on every poll.
//!
//! ## Cross-compile safety
//!
//! Behind the `native-pubsub` Cargo feature (off by default). The only new
//! dependency is a minimal BLOCKING HTTP client with the rustls/ring TLS
//! backend — NOT `openssl-sys`, whose aarch64 system libraries are absent on
//! the x86 Cloud Build cross host. The CLI poll stays the compiled-in fallback
//! when the feature is off.
//!
//! ## Honest failure + idempotency (preserved)
//!
//! `pull`/`acknowledge` return `Err` on transport failure — never a fabricated
//! message. Because tasks are acked only after a successful upload, a redelivered
//! task is re-proven and re-uploaded under the SAME `{height}/m/{level}/{index}`
//! key (overwrite-safe, same bytes), so the leader's #193 re-sort still yields a
//! bit-identical final proof (at-least-once + idempotent = exactly-once effect).

use std::sync::Mutex;
use std::time::{Duration, Instant};

use crate::conductor::pubsub::{MergeResultMessage, MergeTaskMessage};

/// How long before a cached access token's nominal expiry we proactively
/// refresh it (tokens last ~1 h; refresh with comfortable margin).
const TOKEN_REFRESH_MARGIN: Duration = Duration::from_secs(300);
/// Conservative assumed token lifetime when the fetch doesn't report one.
const TOKEN_ASSUMED_LIFETIME: Duration = Duration::from_secs(3000);

/// A pulled Pub/Sub message: the decoded UTF-8 body plus its `ackId` (the
/// handle the caller passes to [`NativePubSub::acknowledge`] AFTER it has
/// durably handled the message — ack-after-upload).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NativePulledMessage {
    /// The decoded message body (the JSON we published).
    pub data: String,
    /// The ack handle for manual acknowledgement.
    pub ack_id: String,
}

/// A merge task paired with its ack handle (manual ack after upload).
#[derive(Debug, Clone)]
pub struct AckableMergeTask {
    pub task: MergeTaskMessage,
    pub ack_id: String,
}

/// A merge result paired with its ack handle (the leader acks a consumed
/// result after recording it).
#[derive(Debug, Clone)]
pub struct AckableMergeResult {
    pub result: MergeResultMessage,
    pub ack_id: String,
}

/// In-process access-token cache. Fetched via `gcloud auth print-access-token`
/// (the image's existing ADC path) and reused until shortly before expiry, so
/// there is NO per-poll process spawn (the #200 barrier cost).
struct TokenCache {
    gcloud_bin: String,
    token: Option<String>,
    fetched_at: Instant,
    lifetime: Duration,
}

impl TokenCache {
    fn new(gcloud_bin: String) -> Self {
        Self {
            gcloud_bin,
            token: None,
            fetched_at: Instant::now(),
            lifetime: TOKEN_ASSUMED_LIFETIME,
        }
    }

    /// Whether the cached token is still comfortably valid.
    fn fresh(&self) -> bool {
        self.token.is_some() && self.fetched_at.elapsed() + TOKEN_REFRESH_MARGIN < self.lifetime
    }

    /// Return a valid bearer token, fetching/refreshing if needed.
    fn get(&mut self) -> anyhow::Result<String> {
        if self.fresh() {
            return Ok(self.token.clone().unwrap());
        }
        let token = fetch_access_token(&self.gcloud_bin)?;
        self.token = Some(token.clone());
        self.fetched_at = Instant::now();
        self.lifetime = TOKEN_ASSUMED_LIFETIME;
        Ok(token)
    }

    /// Force a refresh on the next [`get`](Self::get) (e.g. after a 401).
    fn invalidate(&mut self) {
        self.token = None;
    }
}

/// Fetch a fresh OAuth access token via `gcloud auth print-access-token`. This
/// is the SAME ADC/Workload-Identity path the image already uses; it is called
/// rarely (cache miss / refresh), NOT per poll. Returns `Err` honestly on
/// failure — never a fabricated token.
fn fetch_access_token(gcloud_bin: &str) -> anyhow::Result<String> {
    let out = std::process::Command::new(gcloud_bin)
        .args(["auth", "print-access-token"])
        .output()
        .map_err(|e| anyhow::anyhow!("spawn `{gcloud_bin} auth print-access-token`: {e}"))?;
    if !out.status.success() {
        anyhow::bail!(
            "`{gcloud_bin} auth print-access-token` failed: {}",
            String::from_utf8_lossy(&out.stderr).trim()
        );
    }
    let token = String::from_utf8_lossy(&out.stdout).trim().to_string();
    if token.is_empty() {
        anyhow::bail!("`{gcloud_bin} auth print-access-token` returned an empty token");
    }
    Ok(token)
}

/// Build the Pub/Sub REST `:pull` URL for a subscription.
pub fn pull_url(project: &str, subscription: &str) -> String {
    format!("https://pubsub.googleapis.com/v1/projects/{project}/subscriptions/{subscription}:pull")
}

/// Build the Pub/Sub REST `:acknowledge` URL for a subscription.
pub fn ack_url(project: &str, subscription: &str) -> String {
    format!(
        "https://pubsub.googleapis.com/v1/projects/{project}/subscriptions/{subscription}:acknowledge"
    )
}

/// Parse a Pub/Sub REST `pull` response body into `(data, ack_id)` pairs.
///
/// The response shape is `{"receivedMessages":[{"ackId":"...","message":{
/// "data":"<base64>",...}}]}`. `data` is standard-base64 of the published JSON.
/// An empty/absent `receivedMessages` yields `[]`. Pure — unit-tested without a
/// network. Reuses the dependency-free base64 decoder from [`super::pubsub`].
pub fn parse_pull_response(body: &str) -> anyhow::Result<Vec<NativePulledMessage>> {
    let trimmed = body.trim();
    if trimmed.is_empty() || trimmed == "{}" {
        return Ok(Vec::new());
    }
    let val: serde_json::Value =
        serde_json::from_str(trimmed).map_err(|e| anyhow::anyhow!("pull response parse: {e}"))?;
    let arr = match val.get("receivedMessages").and_then(|m| m.as_array()) {
        Some(a) => a,
        None => return Ok(Vec::new()),
    };
    let mut out = Vec::with_capacity(arr.len());
    for entry in arr {
        let ack_id = entry
            .get("ackId")
            .and_then(|a| a.as_str())
            .unwrap_or_default()
            .to_string();
        let data_b64 = entry
            .get("message")
            .and_then(|m| m.get("data"))
            .and_then(|d| d.as_str());
        if let Some(b64) = data_b64 {
            // Pub/Sub uses standard base64 for `data`. Reuse the module's
            // dependency-free decoder (tolerate already-decoded on the fallback
            // path, identical to the CLI parser).
            let data = match crate::conductor::pubsub::base64_decode_pub(b64) {
                Some(bytes) => match String::from_utf8(bytes) {
                    Ok(s) => s,
                    Err(_) => continue,
                },
                None => b64.to_string(),
            };
            if ack_id.is_empty() {
                // A message with no ackId cannot be manually acked — refuse it
                // honestly rather than risk an un-ackable redelivery loop.
                anyhow::bail!("pull response: receivedMessage missing ackId");
            }
            out.push(NativePulledMessage { data, ack_id });
        }
    }
    Ok(out)
}

/// The native in-process Pub/Sub client (issue #203). Holds the project, a
/// blocking HTTP agent (persistent connections), and the in-process token
/// cache. All merge-task-plane pulls/acks go through here instead of a fresh
/// `gcloud` CLI process.
pub struct NativePubSub {
    project: String,
    /// Persistent blocking HTTP agent (connection reuse — no per-poll TLS
    /// handshake). The default agent keeps an idle-connection pool.
    agent: ureq::Agent,
    tokens: Mutex<TokenCache>,
    /// Server-side blocking pull deadline. With `returnImmediately=false` the
    /// Pub/Sub server holds the request open up to this long waiting for a
    /// message, returning early the instant one arrives — the low-latency
    /// pickup that replaces the CLI poll interval.
    pull_timeout: Duration,
}

impl NativePubSub {
    /// Build a native client for `project`, fetching tokens via `gcloud_bin`'s
    /// `auth print-access-token` (cached in-process). `pull_timeout` is the
    /// server-side long-pull hold (e.g. 20 s).
    pub fn new(project: String, gcloud_bin: String, pull_timeout: Duration) -> Self {
        // The agent timeout must exceed the server hold so the connection is
        // not torn down mid-long-pull; add a margin for transit.
        let http_timeout = pull_timeout + Duration::from_secs(10);
        let agent = ureq::AgentBuilder::new()
            .timeout(http_timeout)
            .timeout_connect(Duration::from_secs(10))
            .build();
        Self {
            project,
            agent,
            tokens: Mutex::new(TokenCache::new(gcloud_bin)),
            pull_timeout,
        }
    }

    pub fn project(&self) -> &str {
        &self.project
    }

    fn token(&self) -> anyhow::Result<String> {
        self.tokens.lock().unwrap().get()
    }

    fn invalidate_token(&self) {
        self.tokens.lock().unwrap().invalidate();
    }

    /// Low-latency manual-ack pull from `subscription` of up to `max_messages`.
    /// Uses `returnImmediately=false` (server blocks until a message is
    /// available or the deadline hits), so pickup is near-instant — NOT paced by
    /// a poll interval and NOT a fresh process. Returns the decoded bodies with
    /// their ack handles (the caller acks AFTER it has durably handled them).
    ///
    /// An empty return (deadline reached with no message) is `Ok(vec![])`, not
    /// an error. A transport error returns `Err` (honest failure).
    pub fn pull(
        &self,
        subscription: &str,
        max_messages: u32,
    ) -> anyhow::Result<Vec<NativePulledMessage>> {
        let url = pull_url(&self.project, subscription);
        let req_body = serde_json::json!({
            "maxMessages": max_messages,
            "returnImmediately": false,
        });

        // One retry on a 401 (token may have expired between cache refreshes).
        for attempt in 0..2 {
            let token = self.token()?;
            let resp = self
                .agent
                .post(&url)
                .set("Authorization", &format!("Bearer {token}"))
                .set("Content-Type", "application/json")
                .send_json(req_body.clone());
            match resp {
                Ok(r) => {
                    let body = r
                        .into_string()
                        .map_err(|e| anyhow::anyhow!("read pull body: {e}"))?;
                    return parse_pull_response(&body);
                }
                Err(ureq::Error::Status(401, _)) if attempt == 0 => {
                    // Stale token — refresh once and retry.
                    self.invalidate_token();
                    continue;
                }
                Err(ureq::Error::Status(code, r)) => {
                    let body = r.into_string().unwrap_or_default();
                    anyhow::bail!("pubsub pull {subscription} HTTP {code}: {}", body.trim());
                }
                Err(e) => {
                    anyhow::bail!("pubsub pull {subscription} transport error: {e}");
                }
            }
        }
        // Unreachable: the loop returns or bails on both attempts.
        Ok(Vec::new())
    }

    /// Manually acknowledge `ack_ids` on `subscription` (the ack-after-upload
    /// call). An empty `ack_ids` is a no-op. Honest failure: a failed ack
    /// returns `Err` (the message will simply redeliver — at-least-once).
    pub fn acknowledge(&self, subscription: &str, ack_ids: &[String]) -> anyhow::Result<()> {
        if ack_ids.is_empty() {
            return Ok(());
        }
        let url = ack_url(&self.project, subscription);
        let req_body = serde_json::json!({ "ackIds": ack_ids });
        for attempt in 0..2 {
            let token = self.token()?;
            let resp = self
                .agent
                .post(&url)
                .set("Authorization", &format!("Bearer {token}"))
                .set("Content-Type", "application/json")
                .send_json(req_body.clone());
            match resp {
                Ok(_) => return Ok(()),
                Err(ureq::Error::Status(401, _)) if attempt == 0 => {
                    self.invalidate_token();
                    continue;
                }
                Err(ureq::Error::Status(code, r)) => {
                    let body = r.into_string().unwrap_or_default();
                    anyhow::bail!(
                        "pubsub acknowledge {subscription} HTTP {code}: {}",
                        body.trim()
                    );
                }
                Err(e) => {
                    anyhow::bail!("pubsub acknowledge {subscription} transport error: {e}");
                }
            }
        }
        Ok(())
    }

    // ---- typed merge-task-plane wrappers (issue #203) ----

    /// Pull up to `max` MERGE TASKS with their ack handles (fold workers
    /// compete). The worker acks each task ONLY after proving the merge and
    /// uploading the result (ack-after-upload) via [`Self::acknowledge`].
    pub fn pull_merge_tasks(
        &self,
        subscription: &str,
        max: u32,
    ) -> anyhow::Result<Vec<AckableMergeTask>> {
        let msgs = self.pull(subscription, max)?;
        let mut out = Vec::with_capacity(msgs.len());
        for m in msgs {
            match MergeTaskMessage::from_json(&m.data) {
                Ok(task) => out.push(AckableMergeTask {
                    task,
                    ack_id: m.ack_id,
                }),
                // A body we can't decode is NOT acked — it will redeliver. We
                // skip it here (honest: don't fabricate a task) and let the
                // ack-deadline expiry re-surface it.
                Err(e) => log::warn!("native merge-task decode skipped: {e}"),
            }
        }
        Ok(out)
    }

    /// Pull up to `max` MERGE RESULTS with their ack handles (the leader
    /// collects a level's results). The leader acks each result after recording
    /// it (so it is not redelivered into the next level's barrier).
    pub fn pull_merge_results(
        &self,
        subscription: &str,
        max: u32,
    ) -> anyhow::Result<Vec<AckableMergeResult>> {
        let msgs = self.pull(subscription, max)?;
        let mut out = Vec::with_capacity(msgs.len());
        for m in msgs {
            match MergeResultMessage::from_json(&m.data) {
                Ok(result) => out.push(AckableMergeResult {
                    result,
                    ack_id: m.ack_id,
                }),
                Err(e) => log::warn!("native merge-result decode skipped: {e}"),
            }
        }
        Ok(out)
    }

    /// The configured server-side long-pull hold (for callers that want to log
    /// or bound their own loop deadline against it).
    pub fn pull_timeout(&self) -> Duration {
        self.pull_timeout
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::conductor::pubsub::{MergeResultMessage, MergeTaskMessage};

    #[test]
    fn pull_and_ack_urls() {
        assert_eq!(
            pull_url("kunal-scratch", "i203-merge-task-sub"),
            "https://pubsub.googleapis.com/v1/projects/kunal-scratch/subscriptions/i203-merge-task-sub:pull"
        );
        assert_eq!(
            ack_url("kunal-scratch", "i203-merge-result-sub"),
            "https://pubsub.googleapis.com/v1/projects/kunal-scratch/subscriptions/i203-merge-result-sub:acknowledge"
        );
    }

    /// Test-only standard-base64 encoder (mirrors the decoder under test).
    fn b64(input: &[u8]) -> String {
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

    #[test]
    fn parse_pull_response_empty_variants() {
        assert!(parse_pull_response("").unwrap().is_empty());
        assert!(parse_pull_response("{}").unwrap().is_empty());
        assert!(
            parse_pull_response(r#"{"receivedMessages":[]}"#)
                .unwrap()
                .is_empty()
        );
    }

    #[test]
    fn parse_pull_response_decodes_merge_task_with_ack_id() {
        let task = MergeTaskMessage {
            height: 186_974_616,
            level: 1,
            index: 0,
            left_key: "186974616/0".into(),
            right_key: "186974616/1".into(),
            left_is_merge: false,
            right_is_merge: false,
        };
        let data = b64(task.to_json().as_bytes());
        let body = format!(
            r#"{{"receivedMessages":[{{"ackId":"ACK-XYZ","message":{{"data":"{data}","messageId":"7"}}}}]}}"#
        );
        let pulled = parse_pull_response(&body).unwrap();
        assert_eq!(pulled.len(), 1);
        assert_eq!(pulled[0].ack_id, "ACK-XYZ");
        assert_eq!(MergeTaskMessage::from_json(&pulled[0].data).unwrap(), task);
    }

    #[test]
    fn parse_pull_response_multiple_messages_preserve_ack_ids() {
        let r0 = MergeResultMessage {
            height: 100,
            level: 1,
            index: 0,
            ok: true,
            cell: "fold203-worker-1".into(),
            proof_object: Some("100/m/1/0".into()),
            prove_ms: Some(519),
        };
        let r1 = MergeResultMessage {
            height: 100,
            level: 1,
            index: 1,
            ok: true,
            cell: "fold203-worker-2".into(),
            proof_object: Some("100/m/1/1".into()),
            prove_ms: Some(523),
        };
        let body = format!(
            r#"{{"receivedMessages":[
                {{"ackId":"A0","message":{{"data":"{}"}}}},
                {{"ackId":"A1","message":{{"data":"{}"}}}}
            ]}}"#,
            b64(r0.to_json().as_bytes()),
            b64(r1.to_json().as_bytes())
        );
        let pulled = parse_pull_response(&body).unwrap();
        assert_eq!(pulled.len(), 2);
        assert_eq!(pulled[0].ack_id, "A0");
        assert_eq!(pulled[1].ack_id, "A1");
        assert_eq!(MergeResultMessage::from_json(&pulled[0].data).unwrap(), r0);
        assert_eq!(MergeResultMessage::from_json(&pulled[1].data).unwrap(), r1);
    }

    #[test]
    fn parse_pull_response_missing_ack_id_is_error() {
        // A received message with no ackId can't be manually acked — must error
        // rather than risk an un-ackable redelivery loop.
        let task = MergeTaskMessage {
            height: 7,
            level: 1,
            index: 0,
            left_key: "7/0".into(),
            right_key: "7/1".into(),
            left_is_merge: false,
            right_is_merge: false,
        };
        let data = b64(task.to_json().as_bytes());
        let body = format!(r#"{{"receivedMessages":[{{"message":{{"data":"{data}"}}}}]}}"#);
        assert!(parse_pull_response(&body).is_err());
    }

    #[test]
    fn token_cache_freshness_logic() {
        let mut tc = TokenCache::new("gcloud".into());
        // No token yet -> not fresh.
        assert!(!tc.fresh());
        // Simulate a cached token within its lifetime.
        tc.token = Some("tok".into());
        tc.fetched_at = Instant::now();
        tc.lifetime = Duration::from_secs(3000);
        assert!(tc.fresh());
        // Invalidate -> not fresh.
        tc.invalidate();
        assert!(!tc.fresh());
    }
}
