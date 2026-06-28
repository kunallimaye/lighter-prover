// Copyright (c) Elliot Technologies, Inc.
// SPDX-License-Identifier: BUSL-1.1

//! Library surface of the `bench` crate.
//!
//! The benchmarking entry points live in the `bench` and `prover-node` binaries
//! (`src/bin/`). This library exposes the reusable, unit-testable pieces — most
//! importantly the [`transport`] work-transport abstraction that the fungible
//! `prover-node` worker pool consumes — so they can be tested with
//! `cargo test -p bench` without invoking a binary.

pub mod shutdown;
pub mod transport;
