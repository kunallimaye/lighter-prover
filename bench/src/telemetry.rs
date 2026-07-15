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
    /// (#321 Phase 5) The reduction fold kind for this task, so real folds size
    /// separately from nearly-free padding no-op folds. See
    /// [`FoldKind`]. Defaults to [`FoldKind::NotApplicable`] (leaf/hex).
    pub fold_kind: FoldKind,
    /// (#321 Phase 5) The merged interval span `hi - lo + 1` for a reduction
    /// event; `0` for non-reduction (honest zero — no interval).
    pub merge_interval_span: usize,
    /// (#321 Phase 6) Epoch-milliseconds at which the dispatch loop PULLED this
    /// task off the queue. Surfaced into [`crate::transport::ProverEvent::pull_ts_ms`]
    /// so a later extractor can compute the leaf wave width. `0` when not
    /// recorded (honest sentinel).
    pub pull_ts_ms: u64,
    /// (#321 Phase 6) The seed-ordering strategy this run used
    /// (`"sequential"` | `"critical-path-first"`), echoed into
    /// [`crate::transport::ProverEvent::scheduling_class`] so a run self-describes.
    /// Defaults to `"sequential"`.
    pub scheduling_class: SchedulingClass,
}

/// (#321 Phase 6) A `Copy` tag for the seed-ordering strategy a run used, so
/// [`TaskTelemetry`] stays `Copy` (no owned `String`) while still carrying the
/// class. Mirrors `bench::transport::SeedOrder` at the telemetry boundary; kept
/// here to avoid a telemetry→transport dependency cycle. Its wire string is
/// emitted into [`crate::transport::ProverEvent::scheduling_class`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SchedulingClass {
    /// The historical default `0..N` seed order.
    Sequential,
    /// The straggler-aware critical-path-first front-loading seed order.
    CriticalPathFirst,
}

impl SchedulingClass {
    /// The stable wire string emitted into the completion event.
    pub fn as_str(&self) -> &'static str {
        match self {
            SchedulingClass::Sequential => "sequential",
            SchedulingClass::CriticalPathFirst => "critical-path-first",
        }
    }
}

/// (#321 Phase 5) The kind of reduction fold a task performed, so REAL folds are
/// sized separately from the nearly-free PADDING no-op passthrough folds.
/// Surfaced from the prover's `Role::ReductionFold` dispatch (which already
/// decides `prove_padding` when the right child is entirely padding).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum FoldKind {
    /// A real same-height fold of two real children.
    Real,
    /// The `right_is_real = false` right-padding passthrough (nearly free).
    PaddingNoop,
    /// Leaf / hex / non-reduction task — no fold-kind concept applies.
    NotApplicable,
}

impl FoldKind {
    /// The stable wire string emitted in the completion event
    /// ([`crate::transport::ProverEvent::fold_kind`]).
    pub fn as_str(&self) -> &'static str {
        match self {
            FoldKind::Real => "real",
            FoldKind::PaddingNoop => "padding-noop",
            FoldKind::NotApplicable => "n/a",
        }
    }
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
            // Non-reduction default; the reduction dispatch arm sets these.
            fold_kind: FoldKind::NotApplicable,
            merge_interval_span: 0,
            // #321 Phase 6: pull timestamp / scheduling class default to the
            // honest "not recorded" / historical values; the dispatch loop sets
            // pull_ts_ms and the run's scheduling class when it has them.
            pull_ts_ms: 0,
            scheduling_class: SchedulingClass::Sequential,
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

    /// (#321 Phase 6) `SchedulingClass` wire strings match the `SeedOrder` /
    /// `ProverEvent::scheduling_class` strings, and the default TaskTelemetry
    /// carries the honest sequential / not-recorded values.
    #[test]
    fn scheduling_class_wire_strings_and_defaults_are_stable() {
        assert_eq!(SchedulingClass::Sequential.as_str(), "sequential");
        assert_eq!(
            SchedulingClass::CriticalPathFirst.as_str(),
            "critical-path-first"
        );
        let t = TaskTelemetry::new(0, PrestateSource::NotApplicable, false);
        assert_eq!(t.pull_ts_ms, 0, "default pull_ts_ms is the honest 0");
        assert_eq!(t.scheduling_class, SchedulingClass::Sequential);
    }
}
