// Copyright (c) Elliot Technologies, Inc.
// SPDX-License-Identifier: BUSL-1.1

//! Library crate for the `bench` binary. Exposes the structured-event
//! emitter plus the streaming-mode machinery (trace-contract parsing
//! and the bounded-queue scheduler), so the binary and tests share the
//! same plonky2-free code paths.

pub mod blob_encode;
pub mod conductor;
pub mod empty_witness;
pub mod events;
pub mod kzg;
pub mod l5segment;
pub mod l6drive;
pub mod seed;
pub mod stream;
pub mod trace;
