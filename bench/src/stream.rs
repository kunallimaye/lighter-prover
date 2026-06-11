// Copyright (c) Elliot Technologies, Inc.
// SPDX-License-Identifier: BUSL-1.1

//! Streaming-mode machinery for `bench --stream` (issue #49).
//!
//! Everything here is plonky2-free and unit-testable: the prover is
//! injected as a closure (`FnMut(&ChunkJob) -> ProverOutput`), so tests
//! use a millisecond-sleep stub while the binary wires in the real
//! L1+L2 proving pipeline.
//!
//! Architecture:
//!
//! ```text
//! stdin --> LineSource (trace.rs) --> reader thread --> bounded queue
//!                                       (Enqueuer)    (std sync_channel,
//!                                                      --max-queue)
//!                                                          |
//!                                                          v
//!                                              prover loop (main thread)
//!                                              dequeue -> prove -> events
//! ```
//!
//! Bounded-queue policy: on overflow the chunk job is dropped and
//! counted (`dropped_chunks`); the stream never blocks the reader.
//! `tx_count: null` should not occur in replayed input (the producer
//! fills it per trace-format.md P1) -- the consumer stays lenient and
//! treats it as 500 (the chain's per-block cap) with a warning.

use std::sync::Mutex;
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
use std::sync::mpsc::{Receiver, RecvTimeoutError, SyncSender, TrySendError};
use std::time::{Duration, Instant};

use log::{info, warn};

use crate::events::{self, BenchEvent, now_iso8601, peak_rss_mb};
use crate::trace::{TraceEvent, TraceSource};

/// Fallback tx_count for lenient handling of `null` (the chain's
/// per-block cap; see trace-format.md section 6.2 rationale).
pub const NULL_TX_COUNT_FALLBACK: u64 = 500;

/// How often the periodic `stream_summary` event is emitted.
pub const SUMMARY_PERIOD: Duration = Duration::from_secs(60);

/// One unit of proving work: a single pool chunk attributed to a trace
/// arrival. `enqueued_at` anchors the lag measurement.
#[derive(Debug, Clone)]
pub struct ChunkJob {
    /// Height of the arrival this job was fanned out from.
    pub height: u64,
    /// Index of this job within its arrival (0..n_chunks).
    pub chunk_in_block: u64,
    /// Wall-clock enqueue time (lag_ms = completion - enqueued_at).
    pub enqueued_at: Instant,
}

/// Per-layer measurement returned by the injected prover.
#[derive(Debug, Clone)]
pub struct LayerStat {
    pub layer: u8,
    pub name: &'static str,
    pub wall_ms: u64,
    pub cpu_ms: Option<u64>,
    /// Wall-clock completion instant of this layer's prove call.
    pub completed_at: Instant,
}

/// What the injected prover reports for one chunk job.
#[derive(Debug, Clone)]
pub struct ProverOutput {
    /// Which pre-sliced pool chunk was proven (round-robin index).
    pub pool_chunk_idx: usize,
    /// Total chunks in the witness pool.
    pub pool_chunk_total: usize,
    /// Per-layer stats (L1 then L2 for the real prover).
    pub layers: Vec<LayerStat>,
}

/// State shared between the reader thread, the trace source, and the
/// prover loop. All lock-free except the fatal-error slot.
pub struct StreamShared {
    /// Jobs currently sitting in the bounded queue.
    pub queue_depth: AtomicUsize,
    /// Chunk jobs dropped because the queue was full.
    pub dropped_chunks: AtomicU64,
    /// Block events accepted from the trace source.
    pub arrivals: AtomicU64,
    /// Gap markers skipped (skip-and-count policy).
    pub gaps_skipped: AtomicU64,
    /// Malformed lines skipped with a warning.
    pub malformed_lines: AtomicU64,
    /// Arrivals with `tx_count: null` (leniently treated as 500).
    pub null_tx_counts: AtomicU64,
    /// Cooperative shutdown flag (SIGINT/SIGTERM or fatal error).
    shutdown: AtomicBool,
    fatal: Mutex<Option<String>>,
}

impl StreamShared {
    pub fn new() -> Self {
        Self {
            queue_depth: AtomicUsize::new(0),
            dropped_chunks: AtomicU64::new(0),
            arrivals: AtomicU64::new(0),
            gaps_skipped: AtomicU64::new(0),
            malformed_lines: AtomicU64::new(0),
            null_tx_counts: AtomicU64::new(0),
            shutdown: AtomicBool::new(false),
            fatal: Mutex::new(None),
        }
    }

    pub fn request_shutdown(&self) {
        self.shutdown.store(true, Ordering::SeqCst);
    }

    pub fn shutdown_requested(&self) -> bool {
        self.shutdown.load(Ordering::SeqCst)
    }

    /// Record a fatal contract violation and request shutdown.
    pub fn set_fatal(&self, msg: String) {
        let mut slot = self.fatal.lock().unwrap();
        if slot.is_none() {
            *slot = Some(msg);
        }
        drop(slot);
        self.request_shutdown();
    }

    pub fn fatal_message(&self) -> Option<String> {
        self.fatal.lock().unwrap().clone()
    }
}

impl Default for StreamShared {
    fn default() -> Self {
        Self::new()
    }
}

/// `ceil(tx_count / tx_per_proof)`: chunk jobs enqueued per arrival.
pub fn chunks_for(tx_count: u64, tx_per_proof: usize) -> u64 {
    debug_assert!(tx_per_proof > 0);
    tx_count.div_ceil(tx_per_proof as u64)
}

/// Parse a human duration: `"900s"`, `"15m"`, `"2h"`, or bare seconds
/// (`"900"`).
pub fn parse_duration(s: &str) -> Result<Duration, String> {
    let s = s.trim();
    if s.is_empty() {
        return Err("empty duration".to_string());
    }
    let (num, mult): (&str, u64) = if let Some(rest) = s.strip_suffix('s') {
        (rest, 1)
    } else if let Some(rest) = s.strip_suffix('m') {
        (rest, 60)
    } else if let Some(rest) = s.strip_suffix('h') {
        (rest, 3600)
    } else {
        (s, 1)
    };
    let n: u64 = num
        .parse()
        .map_err(|_| format!("invalid duration '{s}': expected <int>[s|m|h]"))?;
    if n == 0 {
        return Err(format!("invalid duration '{s}': must be > 0"));
    }
    Ok(Duration::from_secs(n * mult))
}

/// Nearest-rank percentile over a slice (sorts a copy). `p` in 0..=100.
/// Returns 0 for an empty slice.
pub fn percentile_ms(samples: &[u64], p: f64) -> u64 {
    if samples.is_empty() {
        return 0;
    }
    let mut sorted = samples.to_vec();
    sorted.sort_unstable();
    let n = sorted.len();
    let rank = ((p / 100.0) * n as f64).ceil() as usize;
    sorted[rank.clamp(1, n) - 1]
}

/// Fans one trace arrival out into bounded-queue chunk jobs. Owns the
/// sender side of the queue; dropping it disconnects the channel,
/// which the prover loop treats as drain-then-exit.
pub struct Enqueuer {
    tx: SyncSender<ChunkJob>,
    shared: std::sync::Arc<StreamShared>,
    tx_per_proof: usize,
}

impl Enqueuer {
    pub fn new(
        tx: SyncSender<ChunkJob>,
        shared: std::sync::Arc<StreamShared>,
        tx_per_proof: usize,
    ) -> Self {
        Self { tx, shared, tx_per_proof }
    }

    /// Enqueue `ceil(tx_count / tx_per_proof)` jobs for one arrival,
    /// dropping (and counting) on queue overflow. Emits the
    /// `stream_arrival` event. Returns `(enqueued, dropped)`.
    pub fn on_arrival(&self, ev: &TraceEvent) -> (u64, u64) {
        self.shared.arrivals.fetch_add(1, Ordering::Relaxed);
        let tx_count = match ev.tx_count {
            Some(n) => n,
            None => {
                // Replayed input should be null-free (producer fills
                // per P1); stay lenient per the consumer brief.
                self.shared.null_tx_counts.fetch_add(1, Ordering::Relaxed);
                warn!(
                    "stream: height {} has tx_count null (unexpected in replayed input); treating as {}",
                    ev.height, NULL_TX_COUNT_FALLBACK
                );
                NULL_TX_COUNT_FALLBACK
            }
        };
        let n_chunks = chunks_for(tx_count, self.tx_per_proof);

        events::emit(&BenchEvent::StreamArrival {
            height: ev.height,
            tx_count: ev.tx_count,
            queue_depth: self.shared.queue_depth.load(Ordering::Relaxed),
            ts: now_iso8601(),
        });

        let mut enqueued = 0u64;
        let mut dropped = 0u64;
        for i in 0..n_chunks {
            let job = ChunkJob {
                height: ev.height,
                chunk_in_block: i,
                enqueued_at: Instant::now(),
            };
            match self.tx.try_send(job) {
                Ok(()) => {
                    self.shared.queue_depth.fetch_add(1, Ordering::Relaxed);
                    enqueued += 1;
                }
                Err(TrySendError::Full(_)) => {
                    self.shared.dropped_chunks.fetch_add(1, Ordering::Relaxed);
                    dropped += 1;
                }
                Err(TrySendError::Disconnected(_)) => break,
            }
        }
        if dropped > 0 {
            warn!(
                "stream: queue full at height {}: dropped {dropped}/{n_chunks} chunk jobs",
                ev.height
            );
        }
        (enqueued, dropped)
    }
}

/// Reader-thread body: pull events from the source, fan out into the
/// queue, until EOF / fatal / shutdown.
pub fn reader_loop<S: TraceSource>(source: &mut S, enq: &Enqueuer) {
    while let Some(ev) = source.next_event() {
        if enq.shared.shutdown_requested() {
            break;
        }
        enq.on_arrival(&ev);
    }
    info!("stream: trace source ended (EOF/fatal/shutdown); reader exiting");
    // Dropping the Enqueuer (and its SyncSender) disconnects the
    // channel; the prover loop drains whatever is queued, then exits.
}

/// Prover-loop configuration.
pub struct StreamConfig {
    pub tx_per_proof: usize,
    /// Periodic summary cadence (60s in production; long in tests).
    pub summary_every: Duration,
    /// Absolute wall-clock deadline (from `--duration`).
    pub deadline: Option<Instant>,
    /// Prove L3 once every N chunks (off by default).
    pub l3_every: Option<u64>,
}

/// Aggregate results of a stream run (also drives `stream_summary`).
#[derive(Debug)]
pub struct StreamOutcome {
    pub chunks_proven: u64,
    pub lags_ms: Vec<u64>,
    pub elapsed: Duration,
}

/// Pure summary computation, shared by the periodic and final
/// `stream_summary` emissions (and unit-testable).
pub struct SummaryData {
    pub throughput_tx_s: f64,
    pub lag_p50_ms: u64,
    pub lag_p95_ms: u64,
}

pub fn summarize(chunks_proven: u64, tx_per_proof: usize, lags_ms: &[u64], elapsed: Duration) -> SummaryData {
    let secs = elapsed.as_secs_f64();
    let throughput = if secs > 0.0 {
        (chunks_proven * tx_per_proof as u64) as f64 / secs
    } else {
        0.0
    };
    SummaryData {
        // Approximation: jobs are uniform tx_per_proof-sized pool
        // chunks, so a partial final chunk of an arrival counts as a
        // full chunk's worth of txs (consistent with the work done).
        throughput_tx_s: (throughput * 10.0).round() / 10.0,
        lag_p50_ms: percentile_ms(lags_ms, 50.0),
        lag_p95_ms: percentile_ms(lags_ms, 95.0),
    }
}

fn emit_stream_summary(
    phase: &str,
    shared: &StreamShared,
    chunks_proven: u64,
    tx_per_proof: usize,
    lags_ms: &[u64],
    elapsed: Duration,
) {
    let s = summarize(chunks_proven, tx_per_proof, lags_ms, elapsed);
    events::emit(&BenchEvent::StreamSummary {
        phase,
        throughput_tx_s: s.throughput_tx_s,
        lag_p50_ms: s.lag_p50_ms,
        lag_p95_ms: s.lag_p95_ms,
        peak_rss_mb: peak_rss_mb(),
        dropped_chunks: shared.dropped_chunks.load(Ordering::Relaxed),
        arrivals: shared.arrivals.load(Ordering::Relaxed),
        gaps_skipped: shared.gaps_skipped.load(Ordering::Relaxed),
        chunks_proven,
        elapsed_s: (elapsed.as_secs_f64() * 10.0).round() / 10.0,
        ts: now_iso8601(),
    });
}

/// Main-thread prover loop. Dequeues chunk jobs and runs the injected
/// prover; emits `chunk_proven` per layer and `stream_summary`
/// periodically and at the end.
///
/// Exit conditions:
/// - channel disconnected and drained (source EOF): drain-then-exit;
/// - shutdown flag (SIGINT/SIGTERM or fatal trace error): stop
///   immediately without draining;
/// - deadline (`--duration`) reached: stop immediately.
pub fn run_prover_loop(
    rx: Receiver<ChunkJob>,
    shared: &StreamShared,
    cfg: &StreamConfig,
    prove: &mut dyn FnMut(&ChunkJob) -> ProverOutput,
    mut l3: Option<&mut dyn FnMut()>,
) -> StreamOutcome {
    let start = Instant::now();
    let mut last_summary = Instant::now();
    let mut chunks_proven: u64 = 0;
    let mut lags_ms: Vec<u64> = Vec::new();

    loop {
        if shared.shutdown_requested() {
            info!("stream: shutdown requested; stopping (queue not drained)");
            break;
        }
        if let Some(deadline) = cfg.deadline {
            if Instant::now() >= deadline {
                info!("stream: --duration reached; stopping");
                break;
            }
        }

        match rx.recv_timeout(Duration::from_millis(100)) {
            Ok(job) => {
                shared.queue_depth.fetch_sub(1, Ordering::Relaxed);
                let out = prove(&job);
                let depth = shared.queue_depth.load(Ordering::Relaxed);
                let mut chunk_lag_ms = 0u64;
                for layer in &out.layers {
                    let lag_ms = layer
                        .completed_at
                        .saturating_duration_since(job.enqueued_at)
                        .as_millis() as u64;
                    chunk_lag_ms = chunk_lag_ms.max(lag_ms);
                    events::emit(&BenchEvent::ChunkProven {
                        layer: layer.layer,
                        name: layer.name,
                        chunk_idx: Some(out.pool_chunk_idx),
                        chunk_total: Some(out.pool_chunk_total),
                        tx_per_proof: cfg.tx_per_proof,
                        wall_ms: layer.wall_ms,
                        cpu_ms: layer.cpu_ms,
                        rss_mb_peak: peak_rss_mb(),
                        rss_mb_after: events::current_rss_mb(),
                        height: job.height,
                        lag_ms,
                        queue_depth: depth,
                        ts: now_iso8601(),
                    });
                }
                chunks_proven += 1;
                lags_ms.push(chunk_lag_ms);

                if let (Some(every), Some(l3_fn)) = (cfg.l3_every, l3.as_deref_mut()) {
                    if every > 0 && chunks_proven % every == 0 {
                        l3_fn();
                    }
                }
            }
            Err(RecvTimeoutError::Timeout) => { /* re-check exit conditions */ }
            Err(RecvTimeoutError::Disconnected) => {
                info!("stream: queue drained after source EOF; exiting");
                break;
            }
        }

        if last_summary.elapsed() >= cfg.summary_every {
            emit_stream_summary(
                "periodic",
                shared,
                chunks_proven,
                cfg.tx_per_proof,
                &lags_ms,
                start.elapsed(),
            );
            last_summary = Instant::now();
        }
    }

    let elapsed = start.elapsed();
    emit_stream_summary("final", shared, chunks_proven, cfg.tx_per_proof, &lags_ms, elapsed);
    StreamOutcome { chunks_proven, lags_ms, elapsed }
}

/// Install SIGINT/SIGTERM handlers that flip the process-wide shutdown
/// flag. Returns the flag so callers can bridge it into `StreamShared`.
/// Uses `libc::signal` with an async-signal-safe handler (a single
/// atomic store) -- no new dependencies, no async runtime.
pub fn install_signal_handlers() -> &'static AtomicBool {
    static FLAG: AtomicBool = AtomicBool::new(false);
    extern "C" fn on_signal(_sig: libc::c_int) {
        FLAG.store(true, Ordering::SeqCst);
    }
    // SAFETY: on_signal only performs an atomic store, which is
    // async-signal-safe. `libc::signal` with a valid handler pointer
    // is sound on Linux (the bench's target platform).
    unsafe {
        libc::signal(libc::SIGINT, on_signal as *const () as libc::sighandler_t);
        libc::signal(libc::SIGTERM, on_signal as *const () as libc::sighandler_t);
    }
    &FLAG
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;
    use std::sync::mpsc::sync_channel;

    struct VecSource {
        events: std::vec::IntoIter<TraceEvent>,
    }

    impl VecSource {
        fn new(events: Vec<TraceEvent>) -> Self {
            Self { events: events.into_iter() }
        }
    }

    impl TraceSource for VecSource {
        fn next_event(&mut self) -> Option<TraceEvent> {
            self.events.next()
        }
    }

    fn ev(height: u64, tx_count: Option<u64>) -> TraceEvent {
        TraceEvent { ts_ms: height as i64, height, tx_count }
    }

    fn stub_output(idx: usize, total: usize) -> ProverOutput {
        let now = Instant::now();
        ProverOutput {
            pool_chunk_idx: idx,
            pool_chunk_total: total,
            layers: vec![
                LayerStat { layer: 1, name: "BlockTxCircuit", wall_ms: 2, cpu_ms: Some(2), completed_at: now },
                LayerStat { layer: 2, name: "BlockTxChainCircuit", wall_ms: 1, cpu_ms: Some(1), completed_at: now },
            ],
        }
    }

    #[test]
    fn ceil_enqueue_math_tpp_1_through_6() {
        // (tx_count, tpp) -> expected ceil(tx_count / tpp)
        let cases = [
            (500, 1, 500),
            (500, 2, 250),
            (500, 3, 167),
            (500, 4, 125),
            (500, 5, 100),
            (500, 6, 84),
            (1, 1, 1),
            (1, 6, 1),
            (13, 4, 4),
            (0, 3, 0),
        ];
        for (txs, tpp, want) in cases {
            assert_eq!(chunks_for(txs, tpp), want, "tx_count={txs} tpp={tpp}");
        }
    }

    #[test]
    fn max_queue_overflow_drops_counted_exactly() {
        let shared = Arc::new(StreamShared::new());
        let (tx, _rx) = sync_channel::<ChunkJob>(4);
        let enq = Enqueuer::new(tx, shared.clone(), 1);
        // One arrival with 10 txs at tpp=1 -> 10 jobs; capacity 4 and
        // no consumer -> exactly 4 enqueued, 6 dropped.
        let (enqueued, dropped) = enq.on_arrival(&ev(100, Some(10)));
        assert_eq!(enqueued, 4);
        assert_eq!(dropped, 6);
        assert_eq!(shared.dropped_chunks.load(Ordering::Relaxed), 6);
        assert_eq!(shared.queue_depth.load(Ordering::Relaxed), 4);
        assert_eq!(shared.arrivals.load(Ordering::Relaxed), 1);
    }

    #[test]
    fn null_tx_count_lenient_500() {
        let shared = Arc::new(StreamShared::new());
        let (tx, rx) = sync_channel::<ChunkJob>(2048);
        let enq = Enqueuer::new(tx, shared.clone(), 4);
        let (enqueued, dropped) = enq.on_arrival(&ev(7, None));
        assert_eq!(enqueued, chunks_for(500, 4)); // 125
        assert_eq!(dropped, 0);
        assert_eq!(shared.null_tx_counts.load(Ordering::Relaxed), 1);
        drop(rx);
    }

    #[test]
    fn duration_parsing() {
        assert_eq!(parse_duration("900s").unwrap(), Duration::from_secs(900));
        assert_eq!(parse_duration("15m").unwrap(), Duration::from_secs(900));
        assert_eq!(parse_duration("2h").unwrap(), Duration::from_secs(7200));
        assert_eq!(parse_duration("90").unwrap(), Duration::from_secs(90));
        assert_eq!(parse_duration(" 5m ").unwrap(), Duration::from_secs(300));
        assert!(parse_duration("").is_err());
        assert!(parse_duration("0s").is_err());
        assert!(parse_duration("abc").is_err());
        assert!(parse_duration("1.5h").is_err());
        assert!(parse_duration("-3m").is_err());
    }

    #[test]
    fn lag_percentiles() {
        let lags: Vec<u64> = (1..=100).collect();
        assert_eq!(percentile_ms(&lags, 50.0), 50);
        assert_eq!(percentile_ms(&lags, 95.0), 95);
        assert_eq!(percentile_ms(&lags, 100.0), 100);
        assert_eq!(percentile_ms(&[42], 50.0), 42);
        assert_eq!(percentile_ms(&[42], 95.0), 42);
        assert_eq!(percentile_ms(&[], 95.0), 0);
        // Unsorted input is handled (function sorts a copy).
        assert_eq!(percentile_ms(&[30, 10, 20], 50.0), 20);
    }

    #[test]
    fn summarize_throughput() {
        let s = summarize(125, 4, &[10, 20, 30], Duration::from_secs(10));
        assert!((s.throughput_tx_s - 50.0).abs() < 1e-9); // 125*4/10
        assert_eq!(s.lag_p50_ms, 20);
        let s0 = summarize(0, 4, &[], Duration::from_secs(0));
        assert_eq!(s0.throughput_tx_s, 0.0);
    }

    #[test]
    fn end_to_end_stub_clean_shutdown_on_eof() {
        // 3 arrivals at tpp=4: 500 -> 125, 13 -> 4, null -> 125 (lenient).
        let arrivals = vec![ev(100, Some(500)), ev(101, Some(13)), ev(102, None)];
        let expected_chunks = 125 + 4 + 125;

        let shared = Arc::new(StreamShared::new());
        let (tx, rx) = sync_channel::<ChunkJob>(1024);
        let enq = Enqueuer::new(tx, shared.clone(), 4);
        let reader_shared = shared.clone();
        let reader = std::thread::spawn(move || {
            let _ = &reader_shared;
            let mut src = VecSource::new(arrivals);
            reader_loop(&mut src, &enq);
            // enq dropped here -> channel disconnects -> loop drains.
        });

        let pool_total = 120;
        let mut pool_idx = 0usize;
        let mut prove = |_job: &ChunkJob| {
            std::thread::sleep(Duration::from_millis(1));
            let out = stub_output(pool_idx, pool_total);
            pool_idx = (pool_idx + 1) % pool_total;
            out
        };
        let cfg = StreamConfig {
            tx_per_proof: 4,
            summary_every: Duration::from_secs(3600),
            deadline: None,
            l3_every: None,
        };
        let outcome = run_prover_loop(rx, &shared, &cfg, &mut prove, None);
        reader.join().unwrap();

        assert_eq!(outcome.chunks_proven, expected_chunks);
        assert_eq!(outcome.lags_ms.len(), expected_chunks as usize);
        assert_eq!(shared.arrivals.load(Ordering::Relaxed), 3);
        assert_eq!(shared.dropped_chunks.load(Ordering::Relaxed), 0);
        assert_eq!(shared.queue_depth.load(Ordering::Relaxed), 0);
        assert!(outcome.lags_ms.iter().all(|&l| l < 60_000));
    }

    #[test]
    fn l3_every_fires_at_cadence() {
        let shared = Arc::new(StreamShared::new());
        let (tx, rx) = sync_channel::<ChunkJob>(64);
        let enq = Enqueuer::new(tx, shared.clone(), 1);
        enq.on_arrival(&ev(1, Some(10))); // 10 chunk jobs
        drop(enq); // disconnect so the loop drains then exits

        let mut l3_calls = 0u64;
        let mut l3 = |/* prove L3 stub */| l3_calls += 1;
        let mut prove = |_job: &ChunkJob| stub_output(0, 1);
        let cfg = StreamConfig {
            tx_per_proof: 1,
            summary_every: Duration::from_secs(3600),
            deadline: None,
            l3_every: Some(4),
        };
        let outcome = run_prover_loop(rx, &shared, &cfg, &mut prove, Some(&mut l3));
        assert_eq!(outcome.chunks_proven, 10);
        assert_eq!(l3_calls, 2); // after chunks 4 and 8
    }

    #[test]
    fn deadline_stops_loop() {
        let shared = Arc::new(StreamShared::new());
        let (tx, rx) = sync_channel::<ChunkJob>(8);
        // Keep the sender alive: the channel never disconnects, so
        // only the deadline can end the loop.
        let _keep_tx = tx;
        let mut prove = |_job: &ChunkJob| stub_output(0, 1);
        let cfg = StreamConfig {
            tx_per_proof: 1,
            summary_every: Duration::from_secs(3600),
            deadline: Some(Instant::now() + Duration::from_millis(150)),
            l3_every: None,
        };
        let t0 = Instant::now();
        let outcome = run_prover_loop(rx, &shared, &cfg, &mut prove, None);
        assert_eq!(outcome.chunks_proven, 0);
        let elapsed = t0.elapsed();
        assert!(elapsed >= Duration::from_millis(140), "stopped early: {elapsed:?}");
        assert!(elapsed < Duration::from_secs(5), "deadline ignored: {elapsed:?}");
    }

    #[test]
    fn shutdown_flag_stops_loop_without_draining() {
        let shared = Arc::new(StreamShared::new());
        let (tx, rx) = sync_channel::<ChunkJob>(64);
        let enq = Enqueuer::new(tx, shared.clone(), 1);
        enq.on_arrival(&ev(1, Some(50))); // 50 jobs queued
        shared.request_shutdown();
        let mut proven = 0u64;
        let mut prove = |_job: &ChunkJob| {
            proven += 1;
            stub_output(0, 1)
        };
        let cfg = StreamConfig {
            tx_per_proof: 1,
            summary_every: Duration::from_secs(3600),
            deadline: None,
            l3_every: None,
        };
        let outcome = run_prover_loop(rx, &shared, &cfg, &mut prove, None);
        assert_eq!(outcome.chunks_proven, 0, "shutdown skips the backlog");
        assert_eq!(proven, 0);
    }
}
