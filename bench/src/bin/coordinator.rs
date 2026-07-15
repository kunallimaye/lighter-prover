// Copyright (c) Elliot Technologies, Inc.
// SPDX-License-Identifier: BUSL-1.1

//! Lighter Prover Coordinator.
//!
//! A stateless daemon that runs in GKE and coordinates the distributed proving
//! reduction tree. It listens to completion events from workers (via Pub/Sub),
//! updates the tree state in Redis (using `RedisCasStore` + `GatingEngine`),
//! and publishes parent fold tasks to the work queue exactly once.

use std::sync::Arc;
use std::time::Duration;
use google_cloud_pubsub::client::{Client as PubSubClient, ClientConfig as PubSubClientConfig};
use futures_util::StreamExt as _;
use log::{info, error, warn, LevelFilter};

use bench::transport::gating::{CasStore, GatingEngine, RedisCasStore, GatingOutcome};
use bench::transport::pubsub::PubSubPublisher;
use bench::transport::{CommitOutcome, FoldStrategy, ProverEvent};

fn main() {
    env_logger::Builder::new()
        .filter_level(LevelFilter::Info)
        .init();

    info!("Starting Lighter Prover Coordinator...");

    // 1. Resolve configuration from environment
    let project_id = std::env::var("PROVER_PUBSUB_PROJECT").ok();
    let event_sub_name = std::env::var("PROVER_EVENTS_SUBSCRIPTION")
        .unwrap_or_else(|_| "prover-events-sub".to_string());
    let work_topic_name = std::env::var("PROVER_WORK_TOPIC")
        .unwrap_or_else(|_| "stark-proofs-topic".to_string());
    let redis_url = std::env::var("REDIS_URL")
        .unwrap_or_else(|_| "redis://127.0.0.1:6379".to_string());
    let ttl_secs = std::env::var("REDIS_TTL_SECS")
        .ok()
        .and_then(|v| v.parse::<usize>().ok())
        .unwrap_or(86400); // 24 hours

    info!("Config: project={:?}, event_sub={}, work_topic={}, redis={}, ttl={}s",
        project_id, event_sub_name, work_topic_name, redis_url, ttl_secs);

    // 2. Initialize Tokio Runtime for Pub/Sub clients
    let rt = Arc::new(
        tokio::runtime::Builder::new_multi_thread()
            .enable_all()
            .build()
            .expect("failed to build tokio runtime")
    );

    // 3. Initialize Redis Store
    let redis_store = RedisCasStore::new(&redis_url, ttl_secs)
        .expect("failed to connect to Redis");
    info!("Connected to Redis successfully.");

    // 4. Initialize Pub/Sub Clients
    let (_ps_client, event_subscription, work_topic) = rt.block_on(async {
        let mut ps_config = PubSubClientConfig::default()
            .with_auth()
            .await
            .expect("pubsub auth failed");
        if let Some(pid) = project_id {
            ps_config.project_id = Some(pid);
        }
        let ps_client = PubSubClient::new(ps_config)
            .await
            .expect("failed to create pubsub client");
        
        let sub = ps_client.subscription(&event_sub_name);
        let topic = ps_client.topic(&work_topic_name);
        (ps_client, sub, topic)
    });

    let publisher = PubSubPublisher::new(rt.clone(), work_topic);
    let gating_engine = GatingEngine::new(&redis_store, &publisher);

    info!("Coordinator initialized. Entering event loop...");

    // #321 Phase 5: local counter of reduction tasks that arrived flagged as
    // re-driven by a crash-recovery re-drive (`GatingEngine::redrive_stale_merges`).
    // Surfaced in logs so a stuck-seam recovery is observable operationally.
    let mut stale_lease_redrive_count: u64 = 0;

    // 5. Run the synchronous event loop
    loop {
        // Pull one event message (blocking on the async stream)
        let msg = match rt.block_on(async {
            let mut stream = event_subscription.subscribe(None).await
                .map_err(|e| format!("subscribe: {e}"))?;
            match stream.next().await {
                Some(m) => Ok(m),
                None => Err("subscription stream closed".to_string()),
            }
        }) {
            Ok(m) => m,
            Err(e) => {
                error!("Error pulling event: {e}. Retrying in 5s...");
                std::thread::sleep(Duration::from_secs(5));
                continue;
            }
        };

        let payload = msg.message.data.clone();
        let event: ProverEvent = match serde_json::from_slice(&payload) {
            Ok(e) => e,
            Err(e) => {
                error!("Failed to deserialize ProverEvent: {e}. Payload: {:?}", String::from_utf8_lossy(&payload));
                let _ = rt.block_on(async { msg.ack().await });
                continue;
            }
        };

        let desc = event.descriptor.clone();
        // #328 Phase 1: log the per-task telemetry alongside the original timers
        // so a benchmark run yields the resource-sizing data WITHOUT GCP metrics.
        // `status` is the REAL published status (not hardcoded) — telemetry must
        // never inherit a fabricated status.
        info!(
            "Received event: role={}, idx={}, fold_strategy={:?}, level={}, status={}, \
             prove_time_ms={}, gcs_time_ms={}, total_time_ms={}, peak_rss_bytes={}, \
             prestate_source={}, is_first_task_on_pod={}, chunk_size={}, leaf_count={}, \
             pull_ms={}, pre_exec_ms={}, prove_ms={}, gcs_write_ms={}, queue_wait_ms={}, \
             fold_kind={}, merge_interval_span={}, redriven_after_lease_expiry={}",
            desc.role.as_str(),
            desc.chunk_idx,
            desc.fold_strategy,
            desc.level,
            event.status,
            event.prove_time_ms,
            event.gcs_time_ms,
            event.total_time_ms,
            event.peak_rss_bytes,
            event.prestate_source,
            event.is_first_task_on_pod,
            event.chunk_size,
            event.leaf_count,
            event.pull_ms,
            event.pre_exec_ms,
            event.prove_ms,
            event.gcs_write_ms,
            event.queue_wait_ms,
            event.fold_kind,
            event.merge_interval_span,
            event.redriven_after_lease_expiry,
        );

        // #321 Phase 5: count re-driven tasks so a recovery re-drive is
        // operationally observable. A task is re-driven when a crash-recovery
        // pass (`GatingEngine::redrive_stale_merges`) re-published a lost merge.
        if event.redriven_after_lease_expiry {
            stale_lease_redrive_count += 1;
            info!(
                "Task {} was RE-DRIVEN by crash recovery (stale-lease re-drive count now {}).",
                desc.output_key(),
                stale_lease_redrive_count
            );
        }

        if event.status != "success" {
            warn!("Skipping non-success event for role={}, idx={}", desc.role.as_str(), desc.chunk_idx);
            let _ = rt.block_on(async { msg.ack().await });
            continue;
        }

        // Derive the REAL commit outcome from the event status rather than
        // hardcoding `Committed`. This removes the hardcoded-`Committed`
        // telemetry-lie noted in #328/#321: a completion event only reaches the
        // gate on success (non-success `continue`s above), but we still pass the
        // truthfully-mapped outcome instead of inventing one. `"success"` maps to
        // `Committed`; anything else is treated as not-committed (`AlreadyExists`),
        // which the gate treats as `NotWinner` — never advancing readiness.
        let outcome = if event.status == "success" {
            CommitOutcome::Committed
        } else {
            CommitOutcome::AlreadyExists
        };

        // Route by the descriptor's fold strategy (#321 Phase 5). Reduction
        // descriptors drive the order-free interval gate (`on_interval_committed`);
        // hex descriptors keep the existing fixed-node gate (`on_child_committed`)
        // UNCHANGED. A reduction LEAF is the interval [chunk_idx, chunk_idx] at
        // level 0 (the `reduction_leaf` ctor sets lo=hi=chunk_idx, level 0), so
        // routing on `desc.lo`/`desc.hi`/`desc.level` is correct for both leaves
        // and folds.
        let gating_result = match desc.fold_strategy {
            FoldStrategy::Reduction => gating_engine.on_interval_committed(
                desc.lo,
                desc.hi,
                desc.level,
                desc.radix,
                desc.leaf_count,
                desc.tx_per_proof,
                outcome,
            ),
            FoldStrategy::Hex => gating_engine.on_child_committed(&desc, outcome),
        };

        match gating_result {
            Ok(outcome) => {
                match outcome {
                    GatingOutcome::NotWinner => {
                        warn!("Gating returned NotWinner for {}", desc.output_key());
                    }
                    GatingOutcome::Recorded { have, needed } => {
                        info!("Recorded child {}. Progress: {}/{} completed.", desc.output_key(), have, needed);
                    }
                    GatingOutcome::PublishedParent(parent_desc) => {
                        info!(
                            "Gate OPENED for parent {} (fold_strategy={:?}, level={}). Published fold task to queue.",
                            parent_desc.output_key(),
                            parent_desc.fold_strategy,
                            parent_desc.level,
                        );
                    }
                    GatingOutcome::ParentAlreadyPublished => {
                        info!("Gate OPENED for parent {}, but it was already published by another instance.", desc.output_key());
                    }
                    GatingOutcome::RootReached => {
                        match desc.fold_strategy {
                            FoldStrategy::Reduction => {
                                // #321 Phase 5: interval termination. The padded
                                // reduction tree reached its single root spanning
                                // [0, padded-1]. Make termination OBSERVABLE (a
                                // clear log + a completion marker in the store) but
                                // do NOT `std::process::exit` mid-loop — the
                                // coordinator is a long-lived daemon that may serve
                                // other runs/replays.
                                let padded = GatingEngine::<RedisCasStore, PubSubPublisher>::padded_leaf_count(desc.leaf_count);
                                info!(
                                    "REDUCTION ROOT REACHED [0, {}] — reduction complete for leaf_count={} (padded={}).",
                                    padded.saturating_sub(1),
                                    desc.leaf_count,
                                    padded,
                                );
                                // Best-effort completion marker; a failure to write
                                // it is logged but does not crash the daemon.
                                let complete_key = format!("rgate/complete/{}", desc.leaf_count);
                                match redis_store.cas_create(&complete_key, b"1") {
                                    Ok(_) => info!("Marked run complete: {complete_key}"),
                                    Err(e) => warn!("Failed to write completion marker {complete_key}: {e}"),
                                }
                            }
                            FoldStrategy::Hex => {
                                info!("ROOT REACHED! Tree reduction complete for {}", desc.output_key());
                            }
                        }
                    }
                }

                // Successfully processed, ACK the event
                if let Err(e) = rt.block_on(async { msg.ack().await }) {
                    error!("Failed to ACK event for {}: {e}", desc.output_key());
                }
            }
            Err(e) => {
                error!("Gating engine failed for {}: {e}. Nacking message for retry.", desc.output_key());
                if let Err(ne) = rt.block_on(async { msg.nack().await }) {
                    error!("Failed to NACK event: {ne}");
                }
            }
        }
    }
}
