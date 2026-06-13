// Copyright (c) Elliot Technologies, Inc.
// SPDX-License-Identifier: BUSL-1.1

//! Structured bench events emitted as JSON Lines on stdout, prefixed with
//! `BENCH_EVENT ` so log aggregators can pick them out of the normal
//! `info!()` stream produced by `env_logger`.
//!
//! Schema (one JSON object per line, after the `BENCH_EVENT ` prefix):
//!
//! - `event = "layer_prove"`: per-chunk (L1/L2) or one-shot (L3) prove
//!   timings. `chunk_idx`/`chunk_total` are `null` for one-shot layers.
//! - `event = "circuit_define"`: define + build time for each circuit.
//! - `event = "summary"`: aggregate totals emitted at the end of `main`.
//!
//! Streaming mode (`bench --stream`, issue #49) additionally emits:
//!
//! - `event = "stream_arrival"`: one per accepted trace block event.
//! - `event = "chunk_proven"`: the `layer_prove` fields plus `lag_ms`,
//!   `queue_depth`, and `height`; emitted per layer (L1, L2) for every
//!   dequeued chunk job.
//! - `event = "stream_summary"`: rolling aggregates, every 60s
//!   (`phase = "periodic"`) and once at exit (`phase = "final"`).
//!
//! ## Platform note
//!
//! `peak_rss_mb`, `current_rss_mb`, and `cpu_time_ms` are Linux-only:
//! they read `/proc/self/status` and call `libc::getrusage`. On other
//! platforms they return `None` and the corresponding JSON fields will
//! be `null`. The bench is intended to run on Linux (containers /
//! workstation), so this is acceptable.

use std::io::{self, Write};
use std::time::{SystemTime, UNIX_EPOCH};

use serde::Serialize;

/// A single structured bench event. Serialized as JSON with `event` as
/// the discriminator tag, so consumers can pattern-match on it.
#[derive(Debug, Serialize)]
#[serde(tag = "event", rename_all = "snake_case")]
pub enum BenchEvent<'a> {
    /// A `*::prove(...)` call wall+CPU+RSS measurement.
    LayerProve {
        layer: u8,
        name: &'a str,
        /// `Some(i)` for per-chunk layers (L1, L2), `None` for one-shot
        /// layers (L3). Serialized as JSON `null` when `None`.
        chunk_idx: Option<usize>,
        chunk_total: Option<usize>,
        tx_per_proof: usize,
        wall_ms: u64,
        cpu_ms: Option<u64>,
        rss_mb_peak: Option<u64>,
        rss_mb_after: Option<u64>,
        ts: String,
    },
    /// A `*::define(...) + builder.build()` measurement.
    CircuitDefine {
        layer: u8,
        name: &'a str,
        wall_ms: u64,
        rss_mb_after: Option<u64>,
        ts: String,
    },
    /// End-of-run aggregate totals.
    Summary {
        tx_per_proof: usize,
        tx_limit: usize,
        chunks: usize,
        total_wall_ms: u64,
        total_cpu_ms: Option<u64>,
        peak_rss_mb: Option<u64>,
        ts: String,
    },
    /// Stream mode: one accepted trace block event (gap markers and
    /// malformed lines are skipped+counted, never emitted here).
    StreamArrival {
        height: u64,
        /// `null` mirrors a trace-level `tx_count: null` (treated as
        /// 500 for enqueue math, per the lenient-consumer policy).
        tx_count: Option<u64>,
        /// Queue depth observed at arrival time, before fan-out.
        queue_depth: usize,
        ts: String,
    },
    /// Stream mode: a dequeued chunk job's per-layer prove measurement.
    /// Carries the same measurement fields as `layer_prove` plus
    /// `height`, `lag_ms` (layer completion - enqueue), and
    /// `queue_depth` (after dequeue).
    ChunkProven {
        layer: u8,
        name: &'a str,
        /// Witness-pool chunk index (round-robin), not a per-block index.
        chunk_idx: Option<usize>,
        /// Witness-pool size.
        chunk_total: Option<usize>,
        tx_per_proof: usize,
        wall_ms: u64,
        cpu_ms: Option<u64>,
        rss_mb_peak: Option<u64>,
        rss_mb_after: Option<u64>,
        height: u64,
        lag_ms: u64,
        queue_depth: usize,
        ts: String,
    },
    /// Stream mode: rolling aggregates. Emitted every 60s
    /// (`phase = "periodic"`) and once at exit (`phase = "final"`).
    StreamSummary {
        phase: &'a str,
        throughput_tx_s: f64,
        lag_p50_ms: u64,
        lag_p95_ms: u64,
        peak_rss_mb: Option<u64>,
        dropped_chunks: u64,
        arrivals: u64,
        gaps_skipped: u64,
        chunks_proven: u64,
        elapsed_s: f64,
        ts: String,
    },
    /// 8-way L5 segment-scheduler batch summary (issue #78). Carries the
    /// parallel critical-path-per-block (`effective_ms_per_block`) headline.
    L5SegmentBatch {
        layer: u8,     // = 5
        name: &'a str, // = "CyclicRecursionCircuit"
        segment_count: u64,
        segment_sizes: Vec<u64>,       // blocks per segment
        per_segment_wall_ms: Vec<u64>, // wall time per segment chain
        block_count: u64,              // total blocks
        effective_ms_per_block: f64,   // max(per_segment_wall_ms)/max(segment_size)
        cpu_ms: Option<u64>,
        rss_mb_peak: Option<u64>,
        ts: String,
    },
    /// Intra-cell parallel L2 tree scheduler per-level summary (issue #73).
    /// Emitted once per tree level (including the leaf level, `level = 0`)
    /// when running with `--l2-workers M`. Carries the per-level wall-clock
    /// stats that establish how close M workers get to the depth × longest-
    /// merge critical-path floor that PR #69's sequential bench only reports.
    L2TreeLevel {
        /// `0` = leaf chain proofs; `1..` = merge levels (1 is closest to
        /// leaves, depth is the root merge).
        level: u32,
        /// Number of proofs at this level (= number of nodes proven here).
        nodes: u64,
        /// Wall-clock from level start to level end (= longest in-flight
        /// proof at this level when M workers are saturated; bounded above
        /// by the level's per-node max).
        level_wall_ms: u64,
        /// Sum of per-node wall_ms at this level (= total CPU-bound work
        /// the level dispatched, ignoring contention).
        node_wall_sum_ms: u64,
        /// Maximum per-node wall_ms at this level.
        node_wall_max_ms: u64,
        /// Minimum per-node wall_ms at this level.
        node_wall_min_ms: u64,
        /// Number of worker threads installed for this run
        /// (`--l2-workers`).
        workers: u32,
        rss_mb_peak: Option<u64>,
        rss_mb_after: Option<u64>,
        ts: String,
    },
    /// L4 split timings (issue #102): circuit build, witness+prove, and
    /// verify walls emitted as separate fields. ADDITIVE to the combined
    /// L4 `layer_prove` event, whose `wall_ms` remains witness+prove+verify
    /// exactly as before -- existing consumers (fleet parser, s-calibrate)
    /// are unaffected; new consumers can split the block-proof budget into
    /// the one-time resident build cost vs the per-block prove/verify wall.
    L4Check {
        name: &'a str,
        /// `"serial"` | `"tree"` -- which L2 fold produced the chain proof.
        label: &'a str,
        tx_per_proof: usize,
        /// `BlockCircuit::define` + `builder.build()` wall. One-time,
        /// resident cost -- NOT part of the per-block proof wall.
        l4_build_ms: u64,
        /// Witness generation + prove wall.
        l4_prove_ms: u64,
        /// Verify wall.
        l4_verify_ms: u64,
        ts: String,
    },
    /// Intra-cell parallel L2 tree-scheduler run summary (issue #73). Emitted
    /// once at the end of a `--l2-workers M` tree-fold. Reports the realized
    /// wall-clock latency alongside the reported `critical_path = depth × avg
    /// merge` so the M / wall-clock curve can be tabulated directly from the
    /// JSONL stream without re-parsing the per-level lines.
    L2TreeSchedule {
        /// `--l2-workers` value.
        workers: u32,
        /// Leaf count (= number of L1 chunks).
        leaves: u64,
        /// Tree depth (= number of merge levels; 0 for single-leaf tree).
        depth: u32,
        /// Total merge nodes across all levels (= leaves - 1 for a
        /// balanced tree; less when odd carries occur).
        merges: u64,
        /// Wall-clock for Phase 2 (leaf chain proofs) start-to-end.
        leaves_wall_ms: u64,
        /// Wall-clock for Phase 3 (all merge levels) start-to-end.
        merges_wall_ms: u64,
        /// Total realized wall-clock for the parallel tree fold
        /// (leaves + merges). This is the headline the sweep records
        /// against the reported critical_path.
        realized_wall_ms: u64,
        /// `depth × avg_merge` from the existing TREEFOLD line.
        critical_path_ms: u64,
        /// Per-leaf average wall_ms (across all leaves).
        leaf_avg_ms: u64,
        /// Per-merge average wall_ms (across all merge levels).
        merge_avg_ms: u64,
        rss_mb_peak: Option<u64>,
        rss_mb_after: Option<u64>,
        ts: String,
    },
}

/// Serialize an event as `BENCH_EVENT <json>\n` and write+flush it to
/// stdout. Flushing per-event guarantees that partial output survives a
/// later crash (e.g. an OOM during proving): the events that already
/// landed will be intact JSON Lines.
pub fn emit(event: &BenchEvent<'_>) {
    let json = match serde_json::to_string(event) {
        Ok(s) => s,
        Err(e) => {
            // Should be unreachable for our types; fall back to a
            // best-effort diagnostic on stderr so we never panic in
            // measurement code.
            eprintln!("bench: failed to serialize event: {e}");
            return;
        }
    };
    let stdout = io::stdout();
    let mut h = stdout.lock();
    // Best-effort write+flush. If stdout is closed there is nothing
    // useful we can do besides not panic.
    let _ = writeln!(h, "BENCH_EVENT {json}");
    let _ = h.flush();
}

/// Peak resident-set size of this process in MB, as reported by
/// `/proc/self/status`'s `VmHWM` field. Returns `None` on non-Linux or
/// if the field can't be parsed.
pub fn peak_rss_mb() -> Option<u64> {
    read_proc_status_kb("VmHWM:").map(kb_to_mb)
}

/// Current resident-set size of this process in MB (`VmRSS` from
/// `/proc/self/status`).
pub fn current_rss_mb() -> Option<u64> {
    read_proc_status_kb("VmRSS:").map(kb_to_mb)
}

/// Total user+system CPU time consumed by this process so far, in
/// milliseconds. Uses `getrusage(RUSAGE_SELF, ...)`.
pub fn cpu_time_ms() -> Option<u64> {
    // SAFETY: `getrusage` writes a fully-initialized `rusage` struct on
    // success; on failure we discard the value.
    unsafe {
        let mut usage: libc::rusage = std::mem::zeroed();
        if libc::getrusage(libc::RUSAGE_SELF, &mut usage) != 0 {
            return None;
        }
        let user_us = (usage.ru_utime.tv_sec as i64) * 1_000_000 + (usage.ru_utime.tv_usec as i64);
        let sys_us = (usage.ru_stime.tv_sec as i64) * 1_000_000 + (usage.ru_stime.tv_usec as i64);
        let total_us = user_us.saturating_add(sys_us);
        if total_us < 0 {
            return None;
        }
        Some((total_us as u64) / 1_000)
    }
}

/// UTC timestamp in `YYYY-MM-DDTHH:MM:SSZ` form (no fractional seconds,
/// trailing `Z`). Stdlib-only -- no `chrono` dependency.
pub fn now_iso8601() -> String {
    let secs = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    format_iso8601_utc(secs)
}

// ---------- internals ----------

fn read_proc_status_kb(field: &str) -> Option<u64> {
    let content = std::fs::read_to_string("/proc/self/status").ok()?;
    for line in content.lines() {
        if let Some(rest) = line.strip_prefix(field) {
            // Format: "VmRSS:\t   1234 kB"
            let trimmed = rest.trim();
            let kb_str = trimmed.split_whitespace().next()?;
            return kb_str.parse::<u64>().ok();
        }
    }
    None
}

fn kb_to_mb(kb: u64) -> u64 {
    kb / 1024
}

/// Convert a Unix timestamp (seconds since epoch) to
/// `YYYY-MM-DDTHH:MM:SSZ`. Pure arithmetic, no external deps. Handles
/// the proleptic Gregorian calendar; correct for any year >= 1970.
fn format_iso8601_utc(secs: u64) -> String {
    let days = secs / 86_400;
    let rem = secs % 86_400;
    let hour = rem / 3_600;
    let minute = (rem % 3_600) / 60;
    let second = rem % 60;

    // Convert days-since-1970-01-01 to (year, month, day) using the
    // civil-from-days algorithm by Howard Hinnant.
    let z = days as i64 + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let doe = (z - era * 146_097) as u64; // [0, 146096]
    let yoe = (doe - doe / 1_460 + doe / 36_524 - doe / 146_096) / 365; // [0, 399]
    let y = yoe as i64 + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100); // [0, 365]
    let mp = (5 * doy + 2) / 153; // [0, 11]
    let d = doy - (153 * mp + 2) / 5 + 1; // [1, 31]
    let m = if mp < 10 { mp + 3 } else { mp - 9 }; // [1, 12]
    let year = if m <= 2 { y + 1 } else { y };

    format!(
        "{:04}-{:02}-{:02}T{:02}:{:02}:{:02}Z",
        year, m, d, hour, minute, second
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn iso8601_known_epochs() {
        assert_eq!(format_iso8601_utc(0), "1970-01-01T00:00:00Z");
        // 2026-06-10T03:14:22Z -> 1_781_061_262
        assert_eq!(format_iso8601_utc(1_781_061_262), "2026-06-10T03:14:22Z");
        // 2000-02-29T12:00:00Z (leap-year sanity) -> 951_825_600
        assert_eq!(format_iso8601_utc(951_825_600), "2000-02-29T12:00:00Z");
    }

    #[test]
    fn cpu_time_monotonic() {
        let a = cpu_time_ms();
        // Spin briefly so CPU time advances.
        let mut x: u64 = 0;
        for i in 0..1_000_000u64 {
            x = x.wrapping_add(i);
        }
        std::hint::black_box(x);
        let b = cpu_time_ms();
        match (a, b) {
            (Some(a), Some(b)) => assert!(b >= a, "cpu time went backwards: {a} -> {b}"),
            _ => {} // Non-Linux: skip.
        }
    }

    #[test]
    fn stream_event_serialization() {
        let arrival = BenchEvent::StreamArrival {
            height: 260_138_266,
            tx_count: None,
            queue_depth: 3,
            ts: "2026-06-11T00:00:00Z".into(),
        };
        let json = serde_json::to_string(&arrival).unwrap();
        assert!(json.contains("\"event\":\"stream_arrival\""));
        assert!(json.contains("\"tx_count\":null"));

        let proven = BenchEvent::ChunkProven {
            layer: 2,
            name: "BlockTxChainCircuit",
            chunk_idx: Some(5),
            chunk_total: Some(120),
            tx_per_proof: 4,
            wall_ms: 498,
            cpu_ms: Some(3960),
            rss_mb_peak: Some(2925),
            rss_mb_after: Some(2910),
            height: 260_138_266,
            lag_ms: 1234,
            queue_depth: 7,
            ts: "2026-06-11T00:00:01Z".into(),
        };
        let json = serde_json::to_string(&proven).unwrap();
        assert!(json.contains("\"event\":\"chunk_proven\""));
        assert!(json.contains("\"lag_ms\":1234"));
        assert!(json.contains("\"queue_depth\":7"));

        let summary = BenchEvent::StreamSummary {
            phase: "final",
            throughput_tx_s: 49.6,
            lag_p50_ms: 800,
            lag_p95_ms: 2100,
            peak_rss_mb: Some(2925),
            dropped_chunks: 0,
            arrivals: 201,
            gaps_skipped: 0,
            chunks_proven: 254,
            elapsed_s: 20.5,
            ts: "2026-06-11T00:00:21Z".into(),
        };
        let json = serde_json::to_string(&summary).unwrap();
        assert!(json.contains("\"event\":\"stream_summary\""));
        assert!(json.contains("\"phase\":\"final\""));
        assert!(json.contains("\"throughput_tx_s\":49.6"));
    }

    #[test]
    fn l5_segment_batch_serialization() {
        let batch = BenchEvent::L5SegmentBatch {
            layer: 5,
            name: "CyclicRecursionCircuit",
            segment_count: 8,
            segment_sizes: vec![8, 8, 8, 8, 8, 8, 8, 8],
            per_segment_wall_ms: vec![7520, 7480, 7600, 7510, 7490, 7530, 7470, 7550],
            block_count: 64,
            effective_ms_per_block: 950.0,
            cpu_ms: Some(60_160),
            rss_mb_peak: Some(4096),
            ts: "2026-06-11T00:00:00Z".into(),
        };
        let json = serde_json::to_string(&batch).unwrap();
        assert!(json.contains("\"event\":\"l5_segment_batch\""));
        assert!(json.contains("\"segment_count\":8"));
        assert!(json.contains("\"block_count\":64"));
        assert!(json.contains("\"effective_ms_per_block\":950.0"));
        assert!(json.contains("\"name\":\"CyclicRecursionCircuit\""));
    }

    #[test]
    fn l4_check_serialization() {
        let ev = BenchEvent::L4Check {
            name: "BlockCircuit",
            label: "tree",
            tx_per_proof: 4,
            l4_build_ms: 10_900,
            l4_prove_ms: 4_800,
            l4_verify_ms: 355,
            ts: "2026-06-13T00:00:00Z".into(),
        };
        let json = serde_json::to_string(&ev).unwrap();
        assert!(json.contains("\"event\":\"l4_check\""));
        assert!(json.contains("\"label\":\"tree\""));
        assert!(json.contains("\"l4_build_ms\":10900"));
        assert!(json.contains("\"l4_prove_ms\":4800"));
        assert!(json.contains("\"l4_verify_ms\":355"));
    }

    #[test]
    fn rss_helpers_smoke() {
        // On Linux these should return Some; elsewhere None. Either is fine.
        let _ = peak_rss_mb();
        let _ = current_rss_mb();
    }

    #[test]
    fn l2_tree_level_serialization() {
        let level = BenchEvent::L2TreeLevel {
            level: 0,
            nodes: 8,
            level_wall_ms: 512,
            node_wall_sum_ms: 4000,
            node_wall_max_ms: 510,
            node_wall_min_ms: 495,
            workers: 4,
            rss_mb_peak: Some(2048),
            rss_mb_after: Some(2000),
            ts: "2026-06-11T00:00:00Z".into(),
        };
        let json = serde_json::to_string(&level).unwrap();
        assert!(json.contains("\"event\":\"l2_tree_level\""));
        assert!(json.contains("\"level\":0"));
        assert!(json.contains("\"nodes\":8"));
        assert!(json.contains("\"workers\":4"));
        assert!(json.contains("\"level_wall_ms\":512"));
    }

    #[test]
    fn l2_tree_schedule_serialization() {
        let sched = BenchEvent::L2TreeSchedule {
            workers: 4,
            leaves: 8,
            depth: 3,
            merges: 7,
            leaves_wall_ms: 600,
            merges_wall_ms: 900,
            realized_wall_ms: 1500,
            critical_path_ms: 1417,
            leaf_avg_ms: 500,
            merge_avg_ms: 472,
            rss_mb_peak: Some(2048),
            rss_mb_after: Some(2000),
            ts: "2026-06-11T00:00:00Z".into(),
        };
        let json = serde_json::to_string(&sched).unwrap();
        assert!(json.contains("\"event\":\"l2_tree_schedule\""));
        assert!(json.contains("\"workers\":4"));
        assert!(json.contains("\"realized_wall_ms\":1500"));
        assert!(json.contains("\"critical_path_ms\":1417"));
    }
}
