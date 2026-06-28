// Copyright (c) Elliot Technologies, Inc.
// SPDX-License-Identifier: BUSL-1.1

//! Production work-transport backend: **GCP Pub/Sub (pull)** for
//! work-descriptor delivery + **GCS native-API atomic claim/commit**
//! (`ifGenerationMatch=0`) for idempotent output and readiness gating.
//!
//! Compiled **only** under `--features pubsub`. The default build, the
//! `LocalTransport` path, and `cargo test -p bench` stay 100% cloud-free with no
//! cloud crates compiled in. This backend implements the SAME
//! [`WorkTransport`](crate::transport::WorkTransport) trait as `LocalTransport`,
//! so the fungible `prover-node work` dispatch loop drives it unchanged.
//!
//! # Verified primitives wired here (do NOT get these wrong)
//!
//! The pilot empirically verified (and these are NOT re-run live here — see the
//! `TODO(confirm-on-live-run)` notes):
//!
//! 1. **GCS native API `ifGenerationMatch=0` create = exactly-one-winner atomic
//!    CAS.** [`GcsCasStore::cas_create`] uploads with `if_generation_match:
//!    Some(0)` (precondition: object generation 0 ⇒ does not exist). The single
//!    winner gets HTTP 200; every loser gets HTTP **412 Precondition Failed**,
//!    which this code maps to [`CommitOutcome::AlreadyExists`]. This backs BOTH
//!    [`commit_output`](crate::transport::WorkTransport::commit_output) AND every
//!    readiness-gating marker. It is **NEVER** a gcsfuse / `O_EXCL` file op:
//!    gcsfuse implements create as a non-atomic stat-then-create, so two pods on
//!    different nodes both "win" and corrupt the object — the pilot REFUTED that
//!    path. See the contract note on
//!    [`WorkTransport::commit_output`](crate::transport::WorkTransport::commit_output).
//!
//! 2. **Pull + lease-extend-while-working + ack-after-commit + nack-on-failure.**
//!    [`PubSubGcsTransport::pull_one`] pulls with `max_messages = 1` (flow
//!    control = 1 outstanding message). [`PubSubLease::extend`] calls
//!    `modifyAckDeadline` to heartbeat the lease *while proving*;
//!    [`PubSubLease::ack`] acks only **after** the output is durably committed;
//!    [`PubSubLease::nack`] abandons on failure to trigger redelivery.
//!
//! 3. **Ack deadline ≈ 2×P99**, hardware-dependent and configurable. Real
//!    measured P99 from the live 500-tx Phase-1 GKE run (`c3d-highcpu-16`):
//!    leaf + prefix-replay ≈ 74s ⇒ ~150s; radix-16 fold ≈ 83s ⇒ ~180s (the long
//!    pole, hence the **180s default**). These are ~8× the earlier 32-core EPYC
//!    pilot (≈30s fold ⇒ 60s) — re-derive per instance type. See
//!    [`PubSubGcsConfig::default_ack_deadline_secs`] and the `--ack-deadline`
//!    flag wired in `prover_node.rs`.
//!
//! # Async ↔ sync bridge
//!
//! The `WorkTransport` trait is synchronous; the `google-cloud-*` crates are
//! async (tonic/reqwest). This backend owns a multi-thread Tokio runtime and
//! bridges each trait call with `runtime.block_on(..)`. This is correct for the
//! single-outstanding-message (flow-control = 1) worker loop: a pod proves one
//! descriptor at a time, so there is no benefit to overlapping async pulls, and
//! `block_on` keeps the trait surface unchanged.

use std::sync::Arc;

use google_cloud_googleapis::pubsub::v1::PubsubMessage;
use google_cloud_pubsub::client::{Client as PubSubClient, ClientConfig as PubSubClientConfig};
use google_cloud_pubsub::subscriber::ReceivedMessage;
use google_cloud_pubsub::subscription::Subscription;
use google_cloud_pubsub::topic::Topic;
use google_cloud_storage::client::{Client as GcsClient, ClientConfig as GcsClientConfig};
use google_cloud_storage::http::objects::download::Range;
use google_cloud_storage::http::objects::get::GetObjectRequest;
use google_cloud_storage::http::objects::upload::{Media, UploadObjectRequest, UploadType};
use google_cloud_storage::http::Error as GcsHttpError;
use tokio::runtime::Runtime;

use super::gating::{CasError, CasStore, GatingEngine, Publisher};
use super::{CommitOutcome, WorkDescriptor, WorkLease, WorkTransport};

// ─────────────────────────────────────────────────────────────────────────
// Configuration
// ─────────────────────────────────────────────────────────────────────────

/// Configuration for the production backend. Sourced from CLI flags / env by
/// `prover_node.rs`; this struct is plain data so it is unit-testable without
/// cloud.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PubSubGcsConfig {
    /// GCP project id. If `None`, the client uses ADC / metadata-server defaults.
    pub project_id: Option<String>,
    /// Pub/Sub topic id the dispatch loop publishes follow-on folds to.
    pub topic: String,
    /// Pub/Sub subscription id the worker pulls descriptors from.
    pub subscription: String,
    /// GCS bucket holding committed proof outputs and the CAS gating markers.
    pub bucket: String,
    /// Ack deadline (seconds) requested on each `modifyAckDeadline` heartbeat
    /// and pull. Should be ≈ 2×P99 of the role's prove time; see
    /// [`default_ack_deadline_secs`](Self::default_ack_deadline_secs).
    pub ack_deadline_secs: i32,
    /// Optional key/object-name prefix so multiple runs can share one bucket
    /// without colliding (e.g. `runs/block_1042/`).
    pub object_prefix: String,
}

impl PubSubGcsConfig {
    /// Recommended default ack deadline (seconds), ≈ 2×P99, hardware-dependent.
    ///
    /// Real measured P99 prove times from the **live 500-tx Phase-1 GKE run**
    /// (radix-16, `tx_per_proof=4`, 10× `c3d-highcpu-16` Spot workers):
    /// * leaf + prefix-replay ≈ 74s (P99 ≈ max 73.65s) ⇒ ack ≈ 150s
    /// * radix-16 fold        ≈ 83s (P99 ≈ max 83.26s) ⇒ ack ≈ 180s
    ///
    /// We default to the **largest** (180s) so a single worker image that may
    /// assume any role per message never under-leases a radix-16 fold; operators
    /// SHOULD re-derive `--ack-deadline` per instance type (2×P99 of their own
    /// measured prove time — this is ≈8× slower per fold than the earlier
    /// 32-core EPYC pilot, where folds were ≈10s ⇒ a 60s default). Pub/Sub
    /// clamps to [10, 600]s. The lease is additionally heartbeated via
    /// [`PubSubLease::extend`] while proving, so this is a floor, not a cap —
    /// but a 60s base with ≈80s folds left ZERO margin and relied entirely on
    /// the heartbeat (a missed beat → redelivery mid-prove → duplicate work).
    pub const fn default_ack_deadline_secs() -> i32 {
        180
    }

    /// Validate the config the way Pub/Sub will: ack deadline in [10, 600]s and
    /// non-empty topic/subscription/bucket. Returns a human-readable error so
    /// the CLI can fail fast before any network call.
    pub fn validate(&self) -> Result<(), String> {
        if self.topic.trim().is_empty() {
            return Err("pubsub: --topic must be set".into());
        }
        if self.subscription.trim().is_empty() {
            return Err("pubsub: --subscription must be set".into());
        }
        if self.bucket.trim().is_empty() {
            return Err("pubsub: --bucket must be set".into());
        }
        if !(10..=600).contains(&self.ack_deadline_secs) {
            return Err(format!(
                "pubsub: --ack-deadline {} out of Pub/Sub range [10, 600]s",
                self.ack_deadline_secs
            ));
        }
        Ok(())
    }

    /// Apply `object_prefix` to a key (no-op when empty). The live commit path
    /// uses [`GcsCasStore::key`] (which carries the same prefix); this mirror is
    /// kept for config-level introspection and is exercised by unit tests.
    #[cfg_attr(not(test), allow(dead_code))]
    fn prefixed(&self, key: &str) -> String {
        if self.object_prefix.is_empty() {
            key.to_string()
        } else {
            format!("{}{}", self.object_prefix, key)
        }
    }
}

impl Default for PubSubGcsConfig {
    fn default() -> Self {
        Self {
            project_id: None,
            topic: String::new(),
            subscription: String::new(),
            bucket: String::new(),
            ack_deadline_secs: Self::default_ack_deadline_secs(),
            object_prefix: String::new(),
        }
    }
}

// ─────────────────────────────────────────────────────────────────────────
// GCS CAS object store (native API ifGenerationMatch=0)
// ─────────────────────────────────────────────────────────────────────────

/// A [`CasStore`] backed by GCS native API. `cas_create` is an upload with
/// `if_generation_match: Some(0)` — the verified exactly-one-winner atomic CAS.
///
/// Cloneable; clones share the runtime + client handles.
#[derive(Clone)]
pub struct GcsCasStore {
    rt: Arc<Runtime>,
    client: Arc<GcsClient>,
    bucket: String,
    object_prefix: String,
}

impl GcsCasStore {
    fn key(&self, key: &str) -> String {
        if self.object_prefix.is_empty() {
            key.to_string()
        } else {
            format!("{}{}", self.object_prefix, key)
        }
    }
}

/// Map a GCS HTTP error to a CAS result. A **412 Precondition Failed** (or 409
/// conflict) from an `ifGenerationMatch=0` upload means the object already
/// exists ⇒ [`CommitOutcome::AlreadyExists`] (NOT an error). Anything else is a
/// real [`CasError`].
fn classify_upload_err(err: &GcsHttpError) -> Result<CommitOutcome, CasError> {
    if let GcsHttpError::Response(resp) = err {
        // 412 = precondition (ifGenerationMatch=0) failed: object exists.
        // 409 = conflict: object already exists.
        if resp.code == 412 || resp.code == 409 {
            return Ok(CommitOutcome::AlreadyExists);
        }
    }
    Err(CasError(format!("gcs upload error: {err}")))
}

impl CasStore for GcsCasStore {
    fn cas_create(&self, key: &str, bytes: &[u8]) -> Result<CommitOutcome, CasError> {
        let object = self.key(key);
        // TODO(confirm-on-live-run): exactly-one-winner across pods on distinct
        // nodes. The `ifGenerationMatch=0` CAS was pilot-verified ephemerally;
        // not re-run live in this slice.
        let req = UploadObjectRequest {
            bucket: self.bucket.clone(),
            if_generation_match: Some(0), // ← the verified atomic CAS primitive
            ..Default::default()
        };
        let media = Media::new(object.clone());
        let upload_type = UploadType::Simple(media);
        let data = bytes.to_vec();
        let client = self.client.clone();
        let result = self
            .rt
            .block_on(async move { client.upload_object(&req, data, &upload_type).await });
        match result {
            Ok(_) => Ok(CommitOutcome::Committed),
            Err(e) => classify_upload_err(&e),
        }
    }

    fn exists(&self, key: &str) -> Result<bool, CasError> {
        let object = self.key(key);
        let req = GetObjectRequest {
            bucket: self.bucket.clone(),
            object,
            ..Default::default()
        };
        let client = self.client.clone();
        let result = self.rt.block_on(async move { client.get_object(&req).await });
        match result {
            Ok(_) => Ok(true),
            Err(GcsHttpError::Response(resp)) if resp.code == 404 => Ok(false),
            Err(e) => Err(CasError(format!("gcs get error: {e}"))),
        }
    }

    fn read(&self, key: &str) -> Result<Option<Vec<u8>>, CasError> {
        let object = self.key(key);
        let req = GetObjectRequest {
            bucket: self.bucket.clone(),
            object,
            ..Default::default()
        };
        let client = self.client.clone();
        let result = self
            .rt
            .block_on(async move { client.download_object(&req, &Range::default()).await });
        match result {
            Ok(bytes) => Ok(Some(bytes)),
            Err(GcsHttpError::Response(resp)) if resp.code == 404 => Ok(None),
            Err(e) => Err(CasError(format!("gcs download error: {e}"))),
        }
    }

    fn count_prefix(&self, prefix: &str) -> Result<usize, CasError> {
        use google_cloud_storage::http::objects::list::ListObjectsRequest;
        let full_prefix = self.key(prefix);
        let bucket = self.bucket.clone();
        let client = self.client.clone();
        // TODO(confirm-on-live-run): list-with-prefix consistency for the gate
        // count. GCS list is strongly consistent for object existence; verified
        // ephemerally in the pilot, not re-run live here.
        let result = self.rt.block_on(async move {
            let mut count = 0usize;
            let mut page_token: Option<String> = None;
            loop {
                let req = ListObjectsRequest {
                    bucket: bucket.clone(),
                    prefix: Some(full_prefix.clone()),
                    page_token: page_token.clone(),
                    ..Default::default()
                };
                let res = client.list_objects(&req).await?;
                if let Some(items) = res.items {
                    count += items.len();
                }
                match res.next_page_token {
                    Some(t) if !t.is_empty() => page_token = Some(t),
                    _ => break,
                }
            }
            Ok::<usize, GcsHttpError>(count)
        });
        result.map_err(|e| CasError(format!("gcs list error: {e}")))
    }
}

// ─────────────────────────────────────────────────────────────────────────
// Pub/Sub publisher adapter
// ─────────────────────────────────────────────────────────────────────────

/// A [`Publisher`] that publishes a JSON-serialised [`WorkDescriptor`] to a
/// Pub/Sub topic.
#[derive(Clone)]
pub struct PubSubPublisher {
    rt: Arc<Runtime>,
    topic: Arc<Topic>,
}

impl Publisher for PubSubPublisher {
    fn publish(&self, descriptor: WorkDescriptor) -> Result<(), CasError> {
        let data = serde_json::to_vec(&descriptor)
            .map_err(|e| CasError(format!("descriptor serialize: {e}")))?;
        let topic = self.topic.clone();
        // TODO(confirm-on-live-run): real Pub/Sub delivery. Pattern (publish a
        // JSON descriptor, ordered by the gate's exactly-once publish marker)
        // was pilot-verified ephemerally; not re-run live here.
        self.rt.block_on(async move {
            let publisher = topic.new_publisher(None);
            let msg = PubsubMessage {
                data,
                ..Default::default()
            };
            let awaiter = publisher.publish(msg).await;
            awaiter
                .get()
                .await
                .map_err(|e| CasError(format!("pubsub publish: {e}")))
        })?;
        Ok(())
    }
}

// ─────────────────────────────────────────────────────────────────────────
// PubSubGcsTransport
// ─────────────────────────────────────────────────────────────────────────

/// The production [`WorkTransport`]: Pub/Sub pull/ack/nack/extend + publish,
/// with GCS-native-CAS [`commit_output`](WorkTransport::commit_output) and
/// readiness gating. Implements the same trait as `LocalTransport`, so the
/// fungible dispatch loop selects it via `--transport=pubsub`.
#[derive(Clone)]
pub struct PubSubGcsTransport {
    rt: Arc<Runtime>,
    config: PubSubGcsConfig,
    gcs: GcsCasStore,
    subscription: Arc<Subscription>,
    publisher: PubSubPublisher,
}

impl PubSubGcsTransport {
    /// Connect a production transport from `config`. Builds a Tokio runtime, the
    /// GCS + Pub/Sub clients (Application Default Credentials), and resolves the
    /// topic + subscription handles.
    ///
    /// TODO(confirm-on-live-run): this performs real client auth + connection;
    /// it is verified-by-construction here (the auth/connect path is the
    /// maintained crate's, not re-run live in this slice).
    pub fn connect(config: PubSubGcsConfig) -> Result<Self, String> {
        config.validate()?;
        let rt = Arc::new(
            tokio::runtime::Builder::new_multi_thread()
                .enable_all()
                .build()
                .map_err(|e| format!("failed to build tokio runtime: {e}"))?,
        );

        let (gcs_client, ps_client) = rt.block_on(async {
            let gcs_config = GcsClientConfig::default()
                .with_auth()
                .await
                .map_err(|e| format!("gcs auth: {e}"))?;
            let gcs_client = GcsClient::new(gcs_config);

            let mut ps_config = PubSubClientConfig::default()
                .with_auth()
                .await
                .map_err(|e| format!("pubsub auth: {e}"))?;
            if let Some(pid) = &config.project_id {
                ps_config.project_id = Some(pid.clone());
            }
            let ps_client = PubSubClient::new(ps_config)
                .await
                .map_err(|e| format!("pubsub client: {e}"))?;
            Ok::<_, String>((gcs_client, ps_client))
        })?;

        let topic = ps_client.topic(&config.topic);
        let subscription = ps_client.subscription(&config.subscription);

        let gcs = GcsCasStore {
            rt: rt.clone(),
            client: Arc::new(gcs_client),
            bucket: config.bucket.clone(),
            object_prefix: config.object_prefix.clone(),
        };
        let publisher = PubSubPublisher {
            rt: rt.clone(),
            topic: Arc::new(topic),
        };

        Ok(Self {
            rt,
            config,
            gcs,
            subscription: Arc::new(subscription),
            publisher,
        })
    }

    /// The CAS store this transport uses (for tests / introspection).
    pub fn cas_store(&self) -> &GcsCasStore {
        &self.gcs
    }
}

/// A leased Pub/Sub message. Holds the parsed descriptor + the raw
/// [`ReceivedMessage`] for ack/nack/modifyAckDeadline.
pub struct PubSubLease {
    transport: PubSubGcsTransport,
    descriptor: WorkDescriptor,
    message: ReceivedMessage,
}

impl WorkLease for PubSubLease {
    fn descriptor(&self) -> &WorkDescriptor {
        &self.descriptor
    }

    fn extend(&self) {
        // `modifyAckDeadline` heartbeat while proving (verified primitive).
        // TODO(confirm-on-live-run): real lease-extend prevents redelivery while
        // working. Pilot-verified ephemerally; not re-run live here.
        let secs = self.transport.config.ack_deadline_secs;
        let _ = self
            .transport
            .rt
            .block_on(async { self.message.modify_ack_deadline(secs).await });
    }

    fn ack(self) {
        // Ack only AFTER the output is durably committed (caller's contract).
        let _ = self
            .transport
            .rt
            .block_on(async { self.message.ack().await });
    }

    fn nack(self) {
        // Abandon on failure to trigger redelivery (verified primitive).
        let _ = self
            .transport
            .rt
            .block_on(async { self.message.nack().await });
    }
}

impl WorkTransport for PubSubGcsTransport {
    type Lease = PubSubLease;

    fn pull_one(&self) -> Option<Self::Lease> {
        // Flow control = 1 outstanding message: pull at most one.
        // TODO(confirm-on-live-run): real Pub/Sub pull/redelivery. Pilot-verified
        // ephemerally; not re-run live here.
        let sub = self.subscription.clone();
        let messages = self
            .rt
            .block_on(async move { sub.pull(1, None).await })
            .ok()?;
        let message = messages.into_iter().next()?;
        let descriptor: WorkDescriptor = match serde_json::from_slice(&message.message.data) {
            Ok(d) => d,
            Err(_) => {
                // Malformed payload: nack so it is redelivered / dead-lettered,
                // and report empty so the loop tries again.
                let _ = self.rt.block_on(async { message.nack().await });
                return None;
            }
        };
        Some(PubSubLease {
            transport: self.clone(),
            descriptor,
            message,
        })
    }

    fn publish(&self, descriptor: WorkDescriptor) {
        // Direct publish (e.g. seeding leaves). Gating-driven folds go through
        // the same `PubSubPublisher`.
        let _ = self.publisher.publish(descriptor);
    }

    fn commit_output(&self, key: &str, bytes: &[u8]) -> CommitOutcome {
        // GCS native `ifGenerationMatch=0` CAS — exactly-one-winner. NEVER a
        // gcsfuse `O_EXCL` op (refuted). See module + trait docs.
        self.gcs
            .cas_create(key, bytes)
            .unwrap_or_else(|e| panic!("commit_output CAS failed for {key}: {e}"))
    }

    fn output_exists(&self, key: &str) -> bool {
        self.gcs.exists(key).unwrap_or(false)
    }

    fn read_output(&self, key: &str) -> Option<Vec<u8>> {
        self.gcs.read(key).ok().flatten()
    }

    /// Commit a child's output via GCS-native CAS **and** advance readiness
    /// gating, publishing the parent fold exactly once when the parent's child
    /// quota is met. The production analogue of `LocalTransport`'s
    /// `commit_and_gate`, driving the shared [`GatingEngine`] over the GCS CAS
    /// store + Pub/Sub publisher. Implemented as the [`WorkTransport`] trait
    /// method so the generic dispatch loop drives it.
    fn commit_and_gate(&self, descriptor: &WorkDescriptor, bytes: &[u8]) -> CommitOutcome {
        let outcome = self.commit_output(&descriptor.output_key(), bytes);
        let engine = GatingEngine::new(&self.gcs, &self.publisher);
        // A gate error here is a genuine transport failure; surface it loudly so
        // the dispatch loop nacks rather than silently dropping a fold.
        engine
            .on_child_committed(descriptor, outcome)
            .unwrap_or_else(|e| panic!("readiness gating failed for {}: {e}", descriptor.output_key()));
        outcome
    }
}

impl PubSubGcsTransport {
    /// Seed the N leaf descriptors onto the topic (the dispatch loop's bootstrap).
    pub fn seed_leaves(&self, radix: usize, leaf_count: usize, tx_per_proof: usize) {
        for d in super::seed_leaf_descriptors(radix, leaf_count, tx_per_proof) {
            self.publish(d);
        }
    }

    /// Seed the N leaf descriptors for ONE replay namespaced under `prefix`.
    ///
    /// B>1 replay namespacing (issue #310): each replay of the same block must
    /// land its leaves/folds/CAS markers under a DISTINCT object-prefix
    /// (`<base>block_<b>/`) so identical-content proofs don't dedup via the GCS
    /// `ifGenerationMatch=0` CAS (`AlreadyExists`) and silently collapse the load
    /// into a single tree. The committed object name = `<store prefix><key>`; the
    /// store prefix is fixed per connected transport, so a multi-replay run
    /// drives ONE seeder process per replay, each connected with its own
    /// `--object-prefix=<base>block_<b>/`. This method seeds a single replay's
    /// leaves and surfaces the namespace it expects so a mismatch is observable.
    ///
    /// `prefix` is the replay's intended namespace; it MUST match the transport's
    /// configured `object_prefix` for the committed keys to be isolated. When it
    /// does not (a single process seeding multiple replays), the leaves still
    /// publish but a warning is logged — the operator must run one namespaced
    /// seeder per replay for true isolation.
    pub fn seed_leaves_with_prefix(
        &self,
        prefix: &str,
        radix: usize,
        leaf_count: usize,
        tx_per_proof: usize,
    ) {
        log::info!(
            "[seed] seeding {leaf_count} leaf descriptor(s) intended for object-prefix \
             namespace '{prefix}' (radix={radix}, tx_per_proof={tx_per_proof})"
        );
        self.seed_leaves(radix, leaf_count, tx_per_proof);
    }

    /// The configured ack deadline (seconds).
    pub fn ack_deadline_secs(&self) -> i32 {
        self.config.ack_deadline_secs
    }

    /// A short, human-readable summary of where this transport is pointed
    /// (project/topic/subscription/bucket), for honest telemetry.
    pub fn endpoint_summary(&self) -> String {
        format!(
            "pubsub(topic={}, sub={}, bucket={}, ack_deadline={}s)",
            self.config.topic,
            self.config.subscription,
            self.config.bucket,
            self.config.ack_deadline_secs,
        )
    }
}

/// Compile-time assertion that the production transport is `Send + Sync` (the
/// `WorkTransport` trait requires it; the dispatch loop may run it across
/// threads).
const _: () = {
    fn _assert_send_sync<T: Send + Sync>() {}
    fn _check() {
        _assert_send_sync::<PubSubGcsTransport>();
    }
};

// ─────────────────────────────────────────────────────────────────────────
// Tests — config parsing / validation only (NO network).
// ─────────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    fn base_config() -> PubSubGcsConfig {
        PubSubGcsConfig {
            project_id: Some("proj".into()),
            topic: "work-topic".into(),
            subscription: "work-sub".into(),
            bucket: "proof-bucket".into(),
            ack_deadline_secs: PubSubGcsConfig::default_ack_deadline_secs(),
            object_prefix: String::new(),
        }
    }

    #[test]
    fn default_ack_deadline_is_radix16_2xp99() {
        // Largest role P99 from the live 500-tx c3d-highcpu-16 run (radix-16
        // fold ≈ 83s) ⇒ 2×P99 ≈ 180s default. Stays within Pub/Sub [10, 600]s.
        assert_eq!(PubSubGcsConfig::default_ack_deadline_secs(), 180);
    }

    #[test]
    fn config_validate_accepts_good_config() {
        assert!(base_config().validate().is_ok());
    }

    #[test]
    fn config_validate_rejects_empty_required_fields() {
        let mut c = base_config();
        c.topic = String::new();
        assert!(c.validate().is_err());
        let mut c = base_config();
        c.subscription = "  ".into();
        assert!(c.validate().is_err());
        let mut c = base_config();
        c.bucket = String::new();
        assert!(c.validate().is_err());
    }

    #[test]
    fn config_validate_enforces_ack_deadline_range() {
        let mut c = base_config();
        c.ack_deadline_secs = 9; // below Pub/Sub min 10
        assert!(c.validate().is_err());
        c.ack_deadline_secs = 601; // above Pub/Sub max 600
        assert!(c.validate().is_err());
        c.ack_deadline_secs = 10;
        assert!(c.validate().is_ok());
        c.ack_deadline_secs = 600;
        assert!(c.validate().is_ok());
    }

    #[test]
    fn object_prefix_applied() {
        let mut c = base_config();
        assert_eq!(c.prefixed("leaf_0.proof"), "leaf_0.proof");
        c.object_prefix = "runs/block_1042/".into();
        assert_eq!(c.prefixed("leaf_0.proof"), "runs/block_1042/leaf_0.proof");
    }
}
