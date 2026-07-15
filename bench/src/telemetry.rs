// Copyright (c) Elliot Technologies, Inc.
// SPDX-License-Identifier: BUSL-1.1

//! Per-task application-level telemetry (issue #328 Phase 1).
//!
//! This module carries the *application-derived* resource/latency facts about a
//! single proving task from the worker (`prover-node`) to the completion-event
//! payload ([`crate::transport::ProverEvent`]), so a benchmark run yields the
//! data needed to derive resource sizing WITHOUT needing GCP node metrics,
//! Prometheus, or a metrics backend.
//!
//! # Anti-fabrication contract
//!
//! Every value in [`TaskTelemetry`] is either a *measured* fact or an honest
//! sentinel. An unavailable metric is reported as `0` (numeric) or `"n/a"`
//! (string), NEVER a made-up number. See `reports/PROVENANCE.md`. The peak-RSS
//! reader ([`read_peak_rss_bytes`]) returns `0` when no source is readable
//! rather than guessing.

use std::sync::atomic::{AtomicBool, Ordering};

/// Per-process flag tracking whether the FIRST task on this pod has been
/// observed yet. The first task pays the cold circuit-build cost (#322); the
/// rest reuse the cached circuits. Reading via [`take_is_first_task_on_pod`]
/// atomically flips this so exactly one task per process reports `true`.
static FIRST_TASK: AtomicBool = AtomicBool::new(true);

/// Atomically report whether the calling task is the FIRST task executed by this
/// process (pod). The first caller gets `true` (cold, circuit-build-paying); all
/// subsequent callers get `false` (warm, cached circuits). This separates cold
/// vs cached folds when sizing resources (#322).
///
/// Uses `swap(false, SeqCst)` so the transition is observed exactly once even
/// under concurrent workers in the same process.
pub fn take_is_first_task_on_pod() -> bool {
    FIRST_TASK.swap(false, Ordering::SeqCst)
}

/// Read this process's peak resident set size (RSS) in bytes, best-effort.
///
/// Source order (first readable wins):
///   1. cgroup v2 `/sys/fs/cgroup/memory.peak` — the container's peak memory
///      high-water mark (the number a GKE pod is actually sized against).
///   2. cgroup v1 `/sys/fs/cgroup/memory/memory.max_usage_in_bytes` — the v1
///      equivalent high-water mark.
///   3. `/proc/self/status` `VmHWM:` — the kernel's per-process peak RSS, in kB
///      (converted to bytes).
///
/// Returns `0` when NONE of the sources are readable — an honest zero, never a
/// fabricated number. A `0` in the telemetry therefore means "peak RSS was not
/// observable on this host", not "the task used no memory".
pub fn read_peak_rss_bytes() -> u64 {
    // 1. cgroup v2 memory.peak (raw bytes, single integer line).
    if let Some(v) = read_u64_first_token("/sys/fs/cgroup/memory.peak") {
        return v;
    }
    // 2. cgroup v1 memory.max_usage_in_bytes (raw bytes).
    if let Some(v) = read_u64_first_token("/sys/fs/cgroup/memory/memory.max_usage_in_bytes") {
        return v;
    }
    // 3. /proc/self/status VmHWM (in kB) -> bytes.
    if let Some(kb) = read_vmhwm_kb("/proc/self/status") {
        return kb.saturating_mul(1024);
    }
    0
}

/// Read the first whitespace-delimited token of `path` and parse it as a `u64`.
/// Returns `None` on any read/parse failure (missing file, empty, non-numeric).
fn read_u64_first_token(path: &str) -> Option<u64> {
    let contents = std::fs::read_to_string(path).ok()?;
    let token = contents.split_whitespace().next()?;
    token.parse::<u64>().ok()
}

/// Parse the `VmHWM:` line from a `/proc/<pid>/status`-formatted string and
/// return the peak-RSS value in kB. Returns `None` if the line is absent or
/// malformed. Accepts the caller passing a file path; reads it internally.
fn read_vmhwm_kb(path: &str) -> Option<u64> {
    let contents = std::fs::read_to_string(path).ok()?;
    parse_vmhwm_kb(&contents)
}

/// Extract the `VmHWM:` kB value from a `/proc/self/status` body. Split out from
/// I/O so it is unit-testable without touching the filesystem. Line format is
/// `VmHWM:\t   12345 kB`.
fn parse_vmhwm_kb(status_body: &str) -> Option<u64> {
    for line in status_body.lines() {
        if let Some(rest) = line.strip_prefix("VmHWM:") {
            // rest is e.g. "\t   12345 kB"
            let kb = rest.split_whitespace().next()?;
            return kb.parse::<u64>().ok();
        }
    }
    None
}

/// The pre-state provenance for a leaf task, mapped to the wire string carried
/// in [`crate::transport::ProverEvent::prestate_source`]. Non-leaf roles
/// (folds/root) have no pre-state and report [`PrestateSource::NotApplicable`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PrestateSource {
    /// Leaf read its pre-state from the committed corpus (#316 fast path).
    Corpus,
    /// Leaf fell back to prefix REPLAY on a corpus miss.
    ReplayFallback,
    /// Fold/root role — no pre-state involved.
    NotApplicable,
}

impl PrestateSource {
    /// The stable wire string emitted in the completion event.
    pub fn as_str(&self) -> &'static str {
        match self {
            PrestateSource::Corpus => "corpus",
            PrestateSource::ReplayFallback => "replay-fallback",
            PrestateSource::NotApplicable => "n/a",
        }
    }
}

/// Application-level telemetry for one completed proving task, threaded as a
/// single extra parameter through [`crate::transport::WorkTransport::commit_and_gate`]
/// (instead of six scalar params) and folded into the published
/// [`crate::transport::ProverEvent`].
///
/// Fields NOT already available as direct `commit_and_gate` args
/// (`prove_time_ms`, `total_time_ms`) or on the descriptor (`chunk_size`,
/// `leaf_count`) live here. The dispatch loop builds one of these from the
/// timings and flags it already has.
#[derive(Clone, Copy, Debug)]
pub struct TaskTelemetry {
    /// Peak resident memory for the task/pod, in bytes (`0` if unreadable).
    pub peak_rss_bytes: u64,
    /// Pre-state provenance for this task (leaf: corpus/replay; else n/a).
    pub prestate_source: PrestateSource,
    /// Time spent pulling the message off the queue, in ms. Best-effort from
    /// the dispatch loop's pull timing.
    pub pull_ms: u64,
    /// Time spent in pre-execution / setup NOT part of the prove itself, in ms.
    /// Emitted `0` when not separately isolatable (see dispatch-loop comment).
    pub pre_exec_ms: u64,
    /// Time the task waited between becoming visible and being pulled, in ms.
    /// Emitted `0` when not separately measurable with the current plumbing.
    pub queue_wait_ms: u64,
    /// Whether this was the FIRST task on the pod (cold, circuit-build-paying).
    pub is_first_task_on_pod: bool,
}

impl TaskTelemetry {
    /// A telemetry record for a task where only the peak RSS and first-task flag
    /// are known (the phase timers default to `0` = "not separately measured").
    /// Callers set the fields they can measure.
    pub fn new(
        peak_rss_bytes: u64,
        prestate_source: PrestateSource,
        is_first_task_on_pod: bool,
    ) -> Self {
        Self {
            peak_rss_bytes,
            prestate_source,
            pull_ms: 0,
            pre_exec_ms: 0,
            queue_wait_ms: 0,
            is_first_task_on_pod,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// `read_peak_rss_bytes()` must not panic and must return a plausible value.
    /// On this Linux host at least `/proc/self/status VmHWM` is readable, so we
    /// expect a positive value; we tolerate a graceful `0` on a host where no
    /// source is readable (the honest-zero contract). Either way it never panics.
    #[test]
    fn read_peak_rss_bytes_is_plausible_or_honest_zero() {
        let v = read_peak_rss_bytes();
        // The type is unsigned so `v >= 0` is trivially true; the meaningful
        // assertion is that on a standard Linux host we can read a real value.
        if cfg!(target_os = "linux") {
            assert!(
                v > 0,
                "expected a positive peak RSS from cgroup/proc on Linux, got {v}"
            );
        }
    }

    /// The VmHWM parser extracts the kB value from a realistic status body.
    #[test]
    fn parse_vmhwm_kb_extracts_value() {
        let body = "Name:\tprover\nVmPeak:\t 200000 kB\nVmHWM:\t   12345 kB\nVmRSS:\t 10000 kB\n";
        assert_eq!(parse_vmhwm_kb(body), Some(12345));
    }

    /// A status body without VmHWM yields None (so the reader falls through / 0).
    #[test]
    fn parse_vmhwm_kb_absent_is_none() {
        let body = "Name:\tprover\nVmRSS:\t 10000 kB\n";
        assert_eq!(parse_vmhwm_kb(body), None);
    }

    /// The first-task flag: the first read is `true`, every subsequent read is
    /// `false` (per-process cold/warm separation). Because `FIRST_TASK` is a
    /// process-global, we exercise the underlying primitive on a local flag with
    /// the same swap semantics to keep the test order-independent.
    #[test]
    fn first_task_flag_swap_semantics() {
        let flag = AtomicBool::new(true);
        assert!(flag.swap(false, Ordering::SeqCst), "first read is true");
        assert!(!flag.swap(false, Ordering::SeqCst), "second read is false");
        assert!(!flag.swap(false, Ordering::SeqCst), "third read is false");
    }

    /// The process-global `take_is_first_task_on_pod` transitions true -> false.
    /// This test consumes the global exactly once; subsequent reads (in any test
    /// or at runtime) are false, matching the cold/warm contract.
    #[test]
    fn take_is_first_task_on_pod_transitions_once() {
        let first = take_is_first_task_on_pod();
        let second = take_is_first_task_on_pod();
        // Whatever the first observed value, a later read must be false, and the
        // two reads cannot both be true.
        assert!(!(first && second), "at most one read may be true");
        assert!(!second, "after the first observation the flag is false");
    }

    #[test]
    fn prestate_source_wire_strings_are_stable() {
        assert_eq!(PrestateSource::Corpus.as_str(), "corpus");
        assert_eq!(PrestateSource::ReplayFallback.as_str(), "replay-fallback");
        assert_eq!(PrestateSource::NotApplicable.as_str(), "n/a");
    }
}
