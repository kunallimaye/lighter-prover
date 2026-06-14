// Copyright (c) Elliot Technologies, Inc.
// SPDX-License-Identifier: BUSL-1.1

//! LAYER 0 — the FINDING D correctness gate (issue #177).
//!
//! This is the hard acceptance criterion for the per-tx positional pre-state
//! fix. On the real `bench/bench_test.json` cap block it:
//!
//!   1. Generates the per-TX positional pre-state snapshot array via the REAL
//!      sequential L1 sweep at S=1 (`bench::prestate::sweep_per_tx_snapshots`).
//!   2. For TWO chunk sizes — S=9 (k=56) AND S=4 (k=125) — proves EVERY chunk
//!      seeded from its POSITIONAL snapshot `snapshot[S*k]`.
//!
//! Assertions (all must hold):
//!   - All chunks prove; ZERO "set twice with different values" panics
//!     (the FINDING D fix).
//!   - S-INDEPENDENCE: the SAME per-tx snapshot array serves both S values
//!     correctly (validates the per-TX, not per-chunk, design).
//!   - MATCH-KNOWN-GOOD: a chunk proven from its positional snapshot produces
//!     a proof whose public inputs MATCH what the single-process rolling-state
//!     path (the tree-fold pre-pass) produces for the SAME chunk. Not just "no
//!     panic" — "same proof the known-correct path produces". This catches any
//!     hidden chunk-level (vs positional) coupling.
//!
//! The prove path is REAL throughout — never stubbed. This test is EXPENSIVE
//! (it proves ~862 L1 chunks on the cap block) and is gated behind the
//! `LAYER0_FINDING_D=1` env var so it does not run in the normal `cargo test`
//! lane. Run it explicitly:
//!
//! ```sh
//! LAYER0_FINDING_D=1 cargo test -p bench --release --test prestate_finding_d \
//!   -- --nocapture --test-threads=1
//! ```
//!
//! Refs #75 #172 #174 #165 #61 #72 #177.

use std::time::Instant;

use bench::prestate::{ChunkPreState, sweep_per_tx_snapshots};
use circuit::block::Block;
use circuit::block_pre_execution::{BlockPreExec, BlockPreExecWitness};
use circuit::block_pre_execution_constraints::{BlockPreExecutionCircuit, Circuit as _};
use circuit::block_tx::BlockTxWitness;
use circuit::block_tx_constraints::{BlockTxCircuit, Circuit as _};
use circuit::types::config::{C, CIRCUIT_CONFIG, D, F};
use plonky2::plonk::proof::ProofWithPublicInputs;

const CHAIN_ID: u32 = 304;

fn enabled() -> bool {
    std::env::var("LAYER0_FINDING_D")
        .map(|v| v == "1")
        .unwrap_or(false)
}

fn load_block() -> Block<F> {
    // bench/ is CARGO_MANIFEST_DIR; the cap block fixture lives there.
    let path = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("bench_test.json");
    let data = std::fs::read_to_string(&path)
        .unwrap_or_else(|e| panic!("cannot read {}: {e}", path.display()));
    serde_json::from_str(&data).expect("bench_test.json parses as Block")
}

/// Field-by-field equality of two pre-states. Returns the first mismatching
/// field name, or `None` if equal.
///
/// `RegisterStack`, `Asset`, and `SystemConfig` do not derive `PartialEq` in
/// the circuit crate, so we compare them through the SAME canonical hashes the
/// in-circuit pre-state recomputation uses (`bench::seed`), which is the
/// ground-truth identity for these ledger fields. The four tree roots and the
/// market-details array are compared via their hashes too, for one uniform,
/// collision-resistant identity check across all 8 fields.
fn prestate_diff(a: &ChunkPreState, b: &ChunkPreState) -> Option<&'static str> {
    use bench::seed::{
        all_assets_hash, all_market_details_hash, register_stack_hash, system_config_hash,
    };
    if register_stack_hash(&a.register_stack) != register_stack_hash(&b.register_stack) {
        return Some("register_stack");
    }
    if all_assets_hash(&a.all_assets) != all_assets_hash(&b.all_assets) {
        return Some("all_assets");
    }
    if all_market_details_hash(&a.all_market_details)
        != all_market_details_hash(&b.all_market_details)
    {
        return Some("all_market_details");
    }
    if system_config_hash(&a.system_config) != system_config_hash(&b.system_config) {
        return Some("system_config");
    }
    if a.account_tree_root != b.account_tree_root {
        return Some("account_tree_root");
    }
    if a.account_pub_data_tree_root != b.account_pub_data_tree_root {
        return Some("account_pub_data_tree_root");
    }
    if a.account_delta_tree_root != b.account_delta_tree_root {
        return Some("account_delta_tree_root");
    }
    if a.market_tree_root != b.market_tree_root {
        return Some("market_tree_root");
    }
    None
}

#[test]
fn finding_d_per_tx_positional_prestate_all_chunks_prove() {
    if !enabled() {
        eprintln!("SKIP finding_d gate (set LAYER0_FINDING_D=1 to run; it proves ~862 L1 chunks)");
        return;
    }
    // The pre-state structs hold large fixed-size arrays and the plonky2 prover
    // is stack-hungry; run on a thread with a large stack (the bench binary's
    // main thread has one too).
    std::thread::Builder::new()
        .stack_size(4 * 1024 * 1024 * 1024)
        .spawn(run_gate)
        .expect("spawn large-stack gate thread")
        .join()
        .expect("gate thread panicked");
}

fn run_gate() {
    let t_total = Instant::now();
    let mut block = load_block();

    // Optional fast-subset knob: cap the tx count so the gate logic can be
    // smoke-tested in minutes before the full ~862-prove run. The subset is
    // aligned to lcm(9,4)=36 so both S=9 and S=4 still tile it exactly and the
    // S-independence + match-known-good assertions remain meaningful.
    if let Ok(lim) = std::env::var("LAYER0_TX_LIMIT") {
        let lim: usize = lim.parse().expect("LAYER0_TX_LIMIT must be an integer");
        let lim = lim.min(block.txs.len());
        block.txs.truncate(lim);
        println!(
            "[layer0] LAYER0_TX_LIMIT set: truncated to {} txs (subset smoke)",
            lim
        );
    }
    let tx_count = block.txs.len();
    println!(
        "[layer0] loaded bench_test.json: height={} tx_count={}",
        block.block_number, tx_count
    );

    // ---- Build the L1 + pre-exec circuits ONCE (resident). S=1 for the sweep
    // and for the cell's positional prove; tx_per_proof here is the L1
    // circuit's chunk width. The L1 circuit shape depends on tx_per_proof, so
    // each S needs its own L1 circuit (the cell builds one L1 circuit at its
    // configured S). We build the S=1 circuit for the sweep and a per-S
    // circuit for the chunk proves below.
    let pre_exec_circuit = BlockPreExecutionCircuit::define(CIRCUIT_CONFIG);
    let pbt = pre_exec_circuit.target;
    let pre_exec_data = pre_exec_circuit.builder.build::<C>();
    let block_pre_exec = BlockPreExec::from_block(&block);
    let pre_proof = BlockPreExecutionCircuit::prove(&pre_exec_data, &block_pre_exec, &pbt)
        .expect("pre-exec prove");
    let pre_exec_witness = BlockPreExecWitness::from_public_inputs(&pre_proof.public_inputs);
    let created_at = block.created_at;

    // Block-INITIAL pre-state (snapshot[0]) — exactly as run_tree_fold seeds
    // chunk 0 (bench.rs lines 1973-1980): note all_market_details comes from
    // the pre-exec witness, not block.* directly.
    let initial = ChunkPreState {
        register_stack: block.register_stack_before,
        all_assets: block.all_assets.clone(),
        all_market_details: pre_exec_witness.new_market_details.clone(),
        system_config: block.old_system_config,
        account_tree_root: block.old_account_tree_root,
        account_pub_data_tree_root: block.old_account_pub_data_tree_root,
        account_delta_tree_root: block.old_account_delta_tree_root,
        market_tree_root: block.old_market_tree_root,
    };

    // ---- S=1 SWEEP: build the per-TX positional snapshot array. REAL proves.
    println!("[layer0] building S=1 L1 circuit for the sweep...");
    let l1_s1 = BlockTxCircuit::define(CIRCUIT_CONFIG, 1, CHAIN_ID);
    let bt_s1 = l1_s1.target;
    let l1_s1_data = l1_s1.builder.build::<C>();

    println!(
        "[layer0] starting S=1 sweep over {} txs (REAL L1 proves; this is the long pole)...",
        tx_count
    );
    let t_sweep = Instant::now();
    let mut sweep_proves = 0u64;
    let snapshots = sweep_per_tx_snapshots(
        block.block_number,
        created_at,
        initial.clone(),
        &block.txs,
        &l1_s1_data,
        &bt_s1,
        |pos, wall_ms| {
            sweep_proves += 1;
            if pos % 25 == 0 || pos + 1 == tx_count {
                println!(
                    "[layer0]   sweep pos {}/{} ({} ms)  elapsed={:?}",
                    pos,
                    tx_count,
                    wall_ms,
                    t_sweep.elapsed()
                );
            }
        },
    );
    println!(
        "[layer0] sweep DONE: {} snapshots ({} proves) in {:?}",
        snapshots.len(),
        sweep_proves,
        t_sweep.elapsed()
    );
    assert_eq!(
        snapshots.len(),
        tx_count + 1,
        "sweep must produce one snapshot per tx position plus the final post-state"
    );
    // snapshot[0] must equal the block-initial state by construction.
    assert!(
        prestate_diff(snapshots.at_position(0).unwrap(), &initial).is_none(),
        "snapshot[0] must equal block-initial pre-state"
    );

    // Run the gate for both chunk sizes off the SAME snapshot array (the
    // S-independence proof).
    run_s_gate(&block, &pre_exec_witness, &snapshots, 9);
    run_s_gate(&block, &pre_exec_witness, &snapshots, 4);

    println!(
        "[layer0] FINDING D GATE PASSED for S=9 (k=56) AND S=4 (k=125) in {:?}",
        t_total.elapsed()
    );
}

/// For a given chunk size S, prove every chunk (a) via the rolling-state
/// KNOWN-GOOD path and (b) via the POSITIONAL snapshot lookup, and assert:
///   - both prove (no panic, FINDING D fixed),
///   - the positional snapshot equals the rolling pre-state (positional == no
///     chunk coupling),
///   - the two proofs' public inputs MATCH (match-known-good with REAL proofs).
fn run_s_gate(
    block: &Block<F>,
    pre_exec_witness: &BlockPreExecWitness<F>,
    snapshots: &bench::prestate::PreStateSnapshots,
    s: usize,
) {
    let created_at = block.created_at;
    let effective_limit = (block.txs.len() / s) * s;
    let k = effective_limit / s;
    println!("[layer0] === S={s} gate: k={k} chunks (effective_limit={effective_limit}) ===");

    // Build the L1 circuit at THIS S (the cell builds one L1 circuit at its
    // configured tx_per_proof).
    let l1 = BlockTxCircuit::define(CIRCUIT_CONFIG, s, CHAIN_ID);
    let bt = l1.target;
    let l1_data = l1.builder.build::<C>();

    // KNOWN-GOOD rolling state, threaded forward across chunks exactly like
    // run_tree_fold (bench.rs 1973-2056).
    let mut rolling = ChunkPreState {
        register_stack: block.register_stack_before,
        all_assets: block.all_assets.clone(),
        all_market_details: pre_exec_witness.new_market_details.clone(),
        system_config: block.old_system_config,
        account_tree_root: block.old_account_tree_root,
        account_pub_data_tree_root: block.old_account_pub_data_tree_root,
        account_delta_tree_root: block.old_account_delta_tree_root,
        market_tree_root: block.old_market_tree_root,
    };

    for chunk_idx in 0..k {
        let lo = chunk_idx * s;
        let hi = lo + s;
        let txs: Vec<_> = block.txs[lo..hi].to_vec();

        // --- (1) KNOWN-GOOD: prove from the rolling pre-state ---
        let kg_block_tx = rolling.block_tx(created_at, txs.clone());
        let kg_proof: ProofWithPublicInputs<F, C, D> =
            BlockTxCircuit::prove(&l1_data, &kg_block_tx, &bt).unwrap_or_else(|err| {
                panic!("S={s} chunk {chunk_idx}: KNOWN-GOOD rolling prove FAILED: {err:?}")
            });

        // --- (2) POSITIONAL: prove from snapshot[S*k] (the FINDING D fix) ---
        let pos_pre = snapshots.at_chunk(s, chunk_idx).unwrap_or_else(|| {
            panic!(
                "S={s} chunk {chunk_idx}: snapshot[{}] missing",
                s * chunk_idx
            )
        });

        // The positional snapshot MUST equal the rolling pre-state (no chunk
        // coupling). If this fails, STOP — there is positional/chunk coupling.
        if let Some(field) = prestate_diff(pos_pre, &rolling) {
            panic!(
                "S={s} chunk {chunk_idx}: POSITIONAL snapshot[{}] disagrees with rolling \
                 pre-state on field `{field}` — positional/chunk coupling detected, STOP",
                s * chunk_idx
            );
        }

        let pos_block_tx = pos_pre.block_tx(created_at, txs.clone());
        let pos_proof: ProofWithPublicInputs<F, C, D> = BlockTxCircuit::prove(
            &l1_data,
            &pos_block_tx,
            &bt,
        )
        .unwrap_or_else(|err| {
            panic!(
                "S={s} chunk {chunk_idx}: POSITIONAL prove FAILED (FINDING D NOT fixed): {err:?}"
            )
        });

        // --- MATCH-KNOWN-GOOD: public inputs must be identical ---
        assert_eq!(
            pos_proof.public_inputs, kg_proof.public_inputs,
            "S={s} chunk {chunk_idx}: positional-snapshot proof public inputs do NOT match \
             the known-good rolling-state proof — hidden chunk-level coupling, STOP"
        );

        // Roll the known-good state forward from the proof's outputs.
        let w = BlockTxWitness::from_public_inputs(&kg_proof.public_inputs);
        rolling = ChunkPreState {
            register_stack: w.register_stack_after,
            all_assets: w.all_assets_after.clone(),
            all_market_details: w.all_market_details_after.clone(),
            system_config: w.new_system_config,
            account_tree_root: w.new_account_tree_root,
            account_pub_data_tree_root: w.new_account_pub_data_tree_root,
            account_delta_tree_root: w.new_account_delta_tree_root,
            market_tree_root: w.new_market_tree_root,
        };

        if chunk_idx % 10 == 0 || chunk_idx + 1 == k {
            println!("[layer0]   S={s} chunk {chunk_idx}/{k}: positional == known-good ✓");
        }
    }

    println!("[layer0] === S={s} gate PASSED: all {k} chunks prove + match-known-good ===");
}
