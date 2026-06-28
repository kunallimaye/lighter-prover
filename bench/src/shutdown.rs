// Copyright (c) Elliot Technologies, Inc.
// SPDX-License-Identifier: BUSL-1.1

//! Cooperative graceful-shutdown signalling for the fungible `prover-node work`
//! dispatch loop.
//!
//! # Why this exists (graceful drain contract — ADR §7)
//!
//! The fungible prover pool runs on **Spot** capacity for its burst tier and is
//! scaled down by **KEDA** when the Pub/Sub backlog drains. Both scale-down and
//! Spot preemption deliver a **SIGTERM** to the pod and then wait
//! `terminationGracePeriodSeconds` before SIGKILL. A prover that is mid-prove
//! when it receives SIGTERM must **finish the in-flight prove, durably commit it,
//! and ack** — never drop a leased message mid-flight — otherwise the work is
//! redelivered and re-executed (the idempotent GCS `ifGenerationMatch=0` guard
//! makes that *correct*, but it wastes compute and, near the narrow aggregation
//! tail, risks stalling root completion). The ADR therefore mandates:
//!
//! > graceful drain: `terminationGracePeriodSeconds ≥ max prove time`, and on
//! > SIGTERM a pod finishes its in-flight prove and then acks, so scale-down or
//! > preemption never kills a mid-prove pod.
//!
//! # Design: policy / mechanism split (so the loop logic is unit-testable)
//!
//! This module separates the *policy* the dispatch loop consults — a process-wide
//! [`AtomicBool`] queried via [`is_shutdown_requested`] — from the *mechanism*
//! that flips it ([`install_handlers`], which registers an OS signal handler).
//! The dispatch loop only ever reads the flag, so its "stop pulling new work on
//! shutdown, finish the current lease, ack, exit" behaviour can be unit-tested by
//! setting the flag directly with [`request_shutdown`] — **no real signal is
//! raised in tests**. `install_handlers` is the only part that touches the OS and
//! is intentionally thin.
//!
//! The shutdown is **cooperative and graceful**: the flag tells the loop to stop
//! pulling the *next* message. A message already leased when the flag flips is
//! still proved + committed + acked before the loop exits. This is exactly the
//! "finish current task then stop pulling" contract.

use std::sync::atomic::{AtomicBool, Ordering};

/// Process-wide graceful-shutdown flag. Set by [`install_handlers`] on SIGTERM /
/// SIGINT, or directly by [`request_shutdown`] (used by tests and by any
/// in-process caller that wants to drain the loop deterministically).
///
/// `SeqCst` ordering is used throughout: shutdown is a rare, coarse-grained
/// control signal, so the strongest ordering is both correct and free of any
/// meaningful cost on the prove-loop hot path (the loop reads it once per
/// message, between multi-second proves).
static SHUTDOWN_REQUESTED: AtomicBool = AtomicBool::new(false);

/// Returns `true` once a graceful shutdown has been requested (SIGTERM/SIGINT
/// received, or [`request_shutdown`] called). The dispatch loop checks this at
/// the top of each iteration and, when set, stops pulling **new** work.
#[inline]
pub fn is_shutdown_requested() -> bool {
    SHUTDOWN_REQUESTED.load(Ordering::SeqCst)
}

/// Request a graceful shutdown explicitly (without an OS signal).
///
/// This is what the installed signal handler calls, and what tests call to
/// exercise the dispatch loop's drain behaviour deterministically. Idempotent.
#[inline]
pub fn request_shutdown() {
    SHUTDOWN_REQUESTED.store(true, Ordering::SeqCst);
}

/// Reset the flag to its initial (not-requested) state.
///
/// Exposed for tests so each test starts from a clean slate regardless of order;
/// production code never needs to un-request a shutdown.
#[inline]
pub fn reset_for_test() {
    SHUTDOWN_REQUESTED.store(false, Ordering::SeqCst);
}

/// Install OS signal handlers that flip the graceful-shutdown flag on SIGTERM
/// (sent by Kubernetes on scale-down / Spot preemption) and SIGINT (Ctrl-C in
/// interactive runs). Safe to call once at process start.
///
/// The handler does the minimum async-signal-safe thing — set an atomic flag —
/// and returns; all real draining happens cooperatively in the dispatch loop on
/// its next iteration boundary. Errors registering the handler are returned so
/// the caller can decide whether to proceed (the loop still works without it,
/// just without OS-signal-driven drain).
///
/// `signal-hook` is a tiny, dependency-light, cloud-free crate; registering via
/// it keeps the default (non-`pubsub`) build cloud-free and green.
pub fn install_handlers() -> std::io::Result<()> {
    use signal_hook::consts::{SIGINT, SIGTERM};
    // Register a handler that flips OUR process-wide static flag for both
    // signals, so an OS signal and an in-process `request_shutdown` are
    // indistinguishable to the dispatch loop (which only ever reads the flag).
    //
    // SAFETY: the registered action (`request_shutdown`) only performs a single
    // atomic store, which is async-signal-safe; it allocates nothing and takes
    // no locks. This satisfies `signal_hook::low_level::register`'s contract.
    unsafe {
        signal_hook::low_level::register(SIGTERM, request_shutdown)?;
        signal_hook::low_level::register(SIGINT, request_shutdown)?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn flag_defaults_then_sets_then_resets() {
        reset_for_test();
        assert!(
            !is_shutdown_requested(),
            "flag must start clear after reset"
        );
        request_shutdown();
        assert!(
            is_shutdown_requested(),
            "request_shutdown must set the flag"
        );
        // Idempotent: requesting twice stays set.
        request_shutdown();
        assert!(is_shutdown_requested());
        reset_for_test();
        assert!(!is_shutdown_requested(), "reset_for_test must clear it");
    }
}
