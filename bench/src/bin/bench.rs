// Copyright (c) Elliot Technologies, Inc.
// SPDX-License-Identifier: BUSL-1.1

#![feature(stmt_expr_attributes)]
#![allow(unused_imports)]

use std::fs;
use std::path::Path;
use std::time::{Duration, Instant};

use bench::events::{self, BenchEvent, cpu_time_ms, current_rss_mb, now_iso8601, peak_rss_mb};
use circuit::block::{Block, BlockWitness};
use circuit::block_constraints::{BlockCircuit, Circuit as _};
use circuit::block_pre_execution::{BlockPreExec, BlockPreExecWitness};
use circuit::block_pre_execution_constraints::{BlockPreExecutionCircuit, Circuit as _};
use circuit::block_tx::{BlockTx, BlockTxWitness};
use circuit::block_tx_chain::BlockTxChainWitness;
use circuit::block_tx_chain_constraints::{BlockTxChainCircuit, BlockTxChainTarget, Circuit as _};
use circuit::block_tx_chain_merge_constraints::{BlockTxChainMergeCircuit, Circuit as _};
use circuit::block_tx_constraints::{BlockTxCircuit, BlockTxTarget, Circuit as _};
use circuit::builder::custom::cyclic_base_proof;
use circuit::keccak::helpers::keccak;
use circuit::tx;
use circuit::types::config::{C, CIRCUIT_CONFIG, D, F};
use circuit::types::constants::*;
use circuit::types::state_metadata::{STATE_METADATA_SIZE, StateMetadata};
use circuit::types::{account_delta, state_metadata};
use clap::{Parser, ValueEnum};
use env_logger::{Builder, DEFAULT_FILTER_ENV, Env, try_init_from_env};
use log::{Level, LevelFilter, Log, Metadata, Record, debug, info};
use plonky2::field::goldilocks_field::GoldilocksField;
use plonky2::field::types::PrimeField64;
use plonky2::plonk::circuit_data::CircuitData;
use plonky2::plonk::proof::{CompressedProofWithPublicInputs, ProofWithPublicInputs};
use plonky2::recursion::dummy_circuit::{self, dummy_circuit};
use rayon::vec;

const DEFAULT_TX_PER_PROOF: usize = 4;
const DEFAULT_TX_LIMIT: usize = 480;
const CHAIN_ID: u32 = 304;

/// Lighter prover benchmark.
///
/// Runs the full per-chunk tx-proof + chain-recursion pipeline against
/// `bench_test.json`, configurable for chunk-size sweeps.
#[derive(Parser, Debug)]
#[command(name = "bench", about, long_about = None)]
struct Args {
    /// Number of transactions proven per `BlockTxCircuit` chunk. Each
    /// value produces a different proving key.
    #[arg(long, env = "LIGHTER_TX_PER_PROOF", default_value_t = DEFAULT_TX_PER_PROOF)]
    tx_per_proof: usize,

    /// Upper bound on transactions consumed from the test block. The
    /// effective limit is aligned down to the nearest multiple of
    /// `tx_per_proof` so no short final chunk is produced (which would
    /// trip the `zip_eq` panic in `block_tx_constraints`).
    #[arg(long, env = "LIGHTER_TX_LIMIT", default_value_t = DEFAULT_TX_LIMIT)]
    tx_limit: usize,

    /// Streaming mode (issue #49): read a JSONL block trace conforming
    /// to bench/trace-format.md on stdin, fan each arrival out into
    /// ceil(tx_count / tx_per_proof) chunk jobs over a bounded queue,
    /// and prove them from a recycled witness pool. Without this flag
    /// the bench runs the original one-shot batch pipeline, unchanged.
    #[arg(long, default_value_t = false)]
    stream: bool,

    /// Stream mode: bounded chunk-job queue capacity. Jobs arriving
    /// while the queue is full are dropped and counted
    /// (`dropped_chunks` in stream_summary).
    #[arg(long, default_value_t = 1024)]
    max_queue: usize,

    /// Stream mode: additionally prove L3 (BlockPreExecutionCircuit)
    /// once every N proven chunks. Off when omitted.
    #[arg(long)]
    l3_every: Option<u64>,

    /// Stream mode: stop after this wall-clock duration (e.g. "900s",
    /// "15m", "2h"). Without it the run ends at trace EOF or SIGINT.
    #[arg(long)]
    duration: Option<String>,

    /// L2 fold strategy (issue #67). `serial`: today's linked-list fold
    /// (default; zero behavior change). `tree`: per-chunk LEAF chain proofs
    /// (1-chunk chains) merged pairwise up a log-depth tree by the
    /// chain-merge circuit. Batch mode only. Execution is sequential either
    /// way (plonky2 already uses all cores per proof); parallel leaf/merge
    /// scheduling belongs to the cell implementation (#3).
    #[arg(long, value_enum, default_value_t = L2FoldMode::Serial)]
    l2_fold: L2FoldMode,

    /// Tree mode only (issue #67 acceptance): after the tree fold, ALSO run
    /// the serial fold over the same L1 chunk proofs and assert element-wise
    /// equality of the two final proofs' semantic public inputs (everything
    /// before the trailing verifier-key PIs, which differ by construction).
    #[arg(long, default_value_t = false)]
    ab_check: bool,

    /// Issue #67 acceptance: after the L2 fold completes, define+build L4
    /// (BlockCircuit) against the circuit that produced the final chain
    /// proof (the merge circuit in tree mode -- L4 is shape-blind and takes
    /// the chain CircuitData at define time), then prove and verify it.
    /// Batch mode only.
    #[arg(long, default_value_t = false)]
    l4_check: bool,
}

/// Issue #67: L2 fold strategy.
#[derive(Clone, Copy, PartialEq, Eq, Debug, ValueEnum)]
enum L2FoldMode {
    Serial,
    Tree,
}

fn main() {
    init_logger_no_warn();

    let args = Args::parse();

    const VALIDATED_MAX_TX_PER_PROOF: usize = 32;
    if args.tx_per_proof > VALIDATED_MAX_TX_PER_PROOF {
        eprintln!(
            "error: --tx-per-proof {} exceeds the validated maximum of {}.\n\
             \n\
             Chunk sizes 1..=32 are validated (building and proving) following\n\
             the log_gates / ExponentiationGate fix from issue #63, with sweep\n\
             measurements recorded on issue #60. Values above 32 have not been\n\
             validated and may panic at circuit build time.\n\
             \n\
             See https://github.com/kunallimaye/lighter-prover/issues/63 for the\n\
             root-cause analysis and fix details.",
            args.tx_per_proof, VALIDATED_MAX_TX_PER_PROOF
        );
        std::process::exit(2);
    }

    if args.tx_per_proof == 0 {
        eprintln!("error: --tx-per-proof must be > 0");
        std::process::exit(2);
    }
    if args.tx_limit == 0 {
        eprintln!("error: --tx-limit must be > 0");
        std::process::exit(2);
    }
    if args.tx_per_proof > args.tx_limit {
        eprintln!(
            "error: --tx-per-proof ({}) must be <= --tx-limit ({}); a single chunk would not fit",
            args.tx_per_proof, args.tx_limit
        );
        std::process::exit(2);
    }

    if !args.stream {
        if args.duration.is_some() {
            eprintln!("error: --duration requires --stream");
            std::process::exit(2);
        }
        if args.l3_every.is_some() {
            eprintln!("error: --l3-every requires --stream");
            std::process::exit(2);
        }
    } else {
        if args.max_queue == 0 {
            eprintln!("error: --max-queue must be > 0");
            std::process::exit(2);
        }
        if args.l2_fold != L2FoldMode::Serial {
            eprintln!("error: --l2-fold tree is batch-mode only (issue #67); drop --stream");
            std::process::exit(2);
        }
        if args.ab_check || args.l4_check {
            eprintln!("error: --ab-check/--l4-check are batch-mode only (issue #67)");
            std::process::exit(2);
        }
    }
    if args.ab_check && args.l2_fold != L2FoldMode::Tree {
        eprintln!("error: --ab-check requires --l2-fold tree");
        std::process::exit(2);
    }

    log_machine_metadata(&args);

    if args.stream {
        run_stream(&args);
        return;
    }

    let block = get_test_block_json_file("bench_test.json");

    if block.txs.len() < args.tx_per_proof {
        eprintln!(
            "error: bench_test.json has {} txs but --tx-per-proof is {}; need at least one full chunk",
            block.txs.len(),
            args.tx_per_proof
        );
        std::process::exit(2);
    }

    // Align down to the largest multiple of tx_per_proof that fits within
    // both tx_limit and the available txs. This guarantees every chunk has
    // exactly tx_per_proof txs and BlockTxCircuit::prove never sees a
    // short final chunk (which would panic via zip_eq).
    let aligned_limit = (args.tx_limit / args.tx_per_proof) * args.tx_per_proof;
    let effective_limit =
        aligned_limit.min((block.txs.len() / args.tx_per_proof) * args.tx_per_proof);
    let txs: &[_] = &block.txs[..effective_limit];
    let tx_chunks = txs.chunks(args.tx_per_proof);
    let chunks_count = tx_chunks.len();

    if chunks_count == 0 {
        eprintln!(
            "error: aligned tx limit is 0 (tx_per_proof={}, tx_limit={}, txs_available={})",
            args.tx_per_proof,
            args.tx_limit,
            block.txs.len()
        );
        std::process::exit(2);
    }

    info!(
        concat!(
            "Tx and chain circuits are configured to prove {} txs per proof in each iteration. ",
            "There are {} txs in the test block, using {} (aligned to chunk size), so there will be {} iterations of proving.\n\n"
        ),
        args.tx_per_proof,
        block.txs.len(),
        effective_limit,
        chunks_count
    );

    let bench_start = Instant::now();
    let bench_cpu_start = cpu_time_ms();

    let l1_define_t = Instant::now();
    let circuit = BlockTxCircuit::define(CIRCUIT_CONFIG, args.tx_per_proof, CHAIN_ID);
    let bt = circuit.target;
    let data = circuit.builder.build::<C>();
    let l1_define_ms = l1_define_t.elapsed().as_millis() as u64;
    events::emit(&BenchEvent::CircuitDefine {
        layer: 1,
        name: "BlockTxCircuit",
        wall_ms: l1_define_ms,
        rss_mb_after: current_rss_mb(),
        ts: now_iso8601(),
    });
    info!("BlockTxCircuit defined!");
    info!(
        "BlockTxCircuit # public inputs = {:?}",
        data.common.num_public_inputs
    );
    info!(
        "BlockTxCircuit # num_gate_constraints = {:?}",
        data.common.num_gate_constraints
    );

    let l3_define_t = Instant::now();
    let pre_exec_circuit = BlockPreExecutionCircuit::define(CIRCUIT_CONFIG);
    let pbt = pre_exec_circuit.target;
    let pre_exec_data = pre_exec_circuit.builder.build::<C>();
    let l3_define_ms = l3_define_t.elapsed().as_millis() as u64;
    events::emit(&BenchEvent::CircuitDefine {
        layer: 3,
        name: "BlockPreExecutionCircuit",
        wall_ms: l3_define_ms,
        rss_mb_after: current_rss_mb(),
        ts: now_iso8601(),
    });
    info!("BlockPreExecutionCircuit defined!");

    let l2_define_t = Instant::now();
    let chain_circuit = BlockTxChainCircuit::define(CIRCUIT_CONFIG, &data, args.tx_per_proof, 1);
    let chain_circuit_t = chain_circuit.target;
    let chain_circuit_data = chain_circuit.builder.build::<C>();
    let l2_define_ms = l2_define_t.elapsed().as_millis() as u64;
    events::emit(&BenchEvent::CircuitDefine {
        layer: 2,
        name: "BlockTxChainCircuit",
        wall_ms: l2_define_ms,
        rss_mb_after: current_rss_mb(),
        ts: now_iso8601(),
    });
    info!("BlockTxChainCircuit defined!");
    info!(
        "BlockTxChainCircuit # public inputs = {:?}",
        chain_circuit_data.common.num_public_inputs
    );

    let dummy_tx_chain_circuit = dummy_circuit(&chain_circuit_data.common);
    info!("Dummy Tx Chain Circuit defined!");

    let dummy_proof = cyclic_base_proof(
        &chain_circuit_data.common,
        &chain_circuit_data.verifier_only,
        &dummy_tx_chain_circuit,
        Vec::<F>::new().iter().copied().enumerate().collect(),
    )
    .unwrap();

    let block_pre_exec = BlockPreExec::from_block(&block);

    let pre_execution_time = Instant::now();
    let l3_cpu_start = cpu_time_ms();
    let pre_proof = BlockPreExecutionCircuit::prove(&pre_exec_data, &block_pre_exec, &pbt);
    if let Err(err) = pre_proof {
        panic!("Block pre-exec failed to prove. err = {:?}", err);
    }
    let pre_proof = pre_proof.unwrap();
    let pre_execution_total = pre_execution_time.elapsed();
    let l3_cpu_end = cpu_time_ms();
    events::emit(&BenchEvent::LayerProve {
        layer: 3,
        name: "BlockPreExecutionCircuit",
        chunk_idx: None,
        chunk_total: None,
        tx_per_proof: args.tx_per_proof,
        wall_ms: pre_execution_total.as_millis() as u64,
        cpu_ms: diff_ms(l3_cpu_start, l3_cpu_end),
        rss_mb_peak: peak_rss_mb(),
        rss_mb_after: current_rss_mb(),
        ts: now_iso8601(),
    });

    let pre_exec_witness =
        BlockPreExecWitness::from_public_inputs(&pre_proof.clone().public_inputs);

    let state_metadata = pre_exec_witness.new_state_metadata.clone();

    // Issue #67: tree-fold mode branches off here -- everything above
    // (circuit builds, dummy proof, L3 prove) is shared with serial mode.
    if args.l2_fold == L2FoldMode::Tree {
        run_tree_fold(
            &args,
            &block,
            effective_limit,
            chunks_count,
            &data,
            &bt,
            &pre_exec_data,
            &pre_proof,
            &pre_exec_witness,
            &state_metadata,
            &chain_circuit_t,
            &chain_circuit_data,
            chain_circuit.block_tx_witness_size,
            &dummy_tx_chain_circuit,
            &dummy_proof,
            bench_start,
            bench_cpu_start,
        );
        return;
    }

    let mut all_assets = block.all_assets.clone();
    let mut all_market_details = pre_exec_witness.new_market_details.clone();
    let mut system_config = block.old_system_config;
    let mut register_stack = block.register_stack_before;
    let mut account_tree_root = block.old_account_tree_root;
    let mut account_pub_data_tree_root = block.old_account_pub_data_tree_root;
    let mut account_delta_tree_root = block.old_account_delta_tree_root;
    let mut market_tree_root = block.old_market_tree_root;
    let created_at = block.created_at;

    let mut current_chain_proof = BlockTxChainCircuit::cyclic_base_proof(
        &chain_circuit_data,
        &dummy_tx_chain_circuit,
        block.block_number,
        block.created_at,
        pre_exec_witness.new_state_root,
        pre_exec_witness.new_state_root,
        pre_exec_witness.new_validium_root,
        block.old_account_delta_tree_root,
        chain_circuit.block_tx_witness_size,
        &state_metadata,
    );

    let mut tx_prove_total = Duration::ZERO;
    let mut chain_prove_total = Duration::ZERO;

    for (index, tx) in tx_chunks.enumerate() {
        let block_tx = BlockTx {
            created_at,
            old_system_config: system_config,
            register_stack_before: register_stack,
            all_assets_before: all_assets.clone(),
            all_market_details_before: all_market_details.clone(),
            old_account_tree_root: account_tree_root,
            old_account_pub_data_tree_root: account_pub_data_tree_root,
            old_account_delta_tree_root: account_delta_tree_root,
            old_market_tree_root: market_tree_root,
            txs: tx.to_vec(),
        };

        let tx_dt = Instant::now();
        let l1_cpu_start = cpu_time_ms();
        let tx_proof = BlockTxCircuit::prove(&data, &block_tx, &bt);
        let tx_dt = tx_dt.elapsed();
        let l1_cpu_end = cpu_time_ms();
        if let Err(err) = tx_proof {
            panic!("Failed to prove tx chunk #{}. err = {:?}", index, err);
        }

        events::emit(&BenchEvent::LayerProve {
            layer: 1,
            name: "BlockTxCircuit",
            chunk_idx: Some(index),
            chunk_total: Some(chunks_count),
            tx_per_proof: args.tx_per_proof,
            wall_ms: tx_dt.as_millis() as u64,
            cpu_ms: diff_ms(l1_cpu_start, l1_cpu_end),
            rss_mb_peak: peak_rss_mb(),
            rss_mb_after: current_rss_mb(),
            ts: now_iso8601(),
        });

        info!(
            "tx chunk #{index}/{} BlockTxCircuit::prove time: {:?}",
            chunks_count, tx_dt
        );
        tx_prove_total += tx_dt;

        let tx_proof = tx_proof.unwrap();

        let tx_witness = BlockTxWitness::from_public_inputs(&tx_proof.public_inputs.clone());
        all_assets = tx_witness.all_assets_after.clone();
        all_market_details = tx_witness.all_market_details_after.clone();
        register_stack = tx_witness.register_stack_after;
        system_config = tx_witness.new_system_config;
        account_tree_root = tx_witness.new_account_tree_root;
        account_pub_data_tree_root = tx_witness.new_account_pub_data_tree_root;
        account_delta_tree_root = tx_witness.new_account_delta_tree_root;
        market_tree_root = tx_witness.new_market_tree_root;

        let chain_dt = Instant::now();
        let l2_cpu_start = cpu_time_ms();
        let chain_proof = BlockTxChainCircuit::prove(
            &chain_circuit_t,
            &chain_circuit_data,
            index as u64,
            &current_chain_proof,
            &dummy_proof,
            &tx_proof,
        );
        let chain_dt = chain_dt.elapsed();
        let l2_cpu_end = cpu_time_ms();
        if let Err(err) = chain_proof {
            panic!("Block Chain circuit failed to prove. err = {:?}", err);
        }

        events::emit(&BenchEvent::LayerProve {
            layer: 2,
            name: "BlockTxChainCircuit",
            chunk_idx: Some(index),
            chunk_total: Some(chunks_count),
            tx_per_proof: args.tx_per_proof,
            wall_ms: chain_dt.as_millis() as u64,
            cpu_ms: diff_ms(l2_cpu_start, l2_cpu_end),
            rss_mb_peak: peak_rss_mb(),
            rss_mb_after: current_rss_mb(),
            ts: now_iso8601(),
        });

        chain_prove_total += chain_dt;
        info!(
            "tx chunk #{index}/{} BlockTxChainCircuit::prove time: {:?}\n",
            chunks_count, chain_dt
        );

        current_chain_proof = chain_proof.unwrap();
    }

    info!(
        "TOTAL BlockPreExecutionCircuit::prove time: {:?}\n",
        pre_execution_total
    );

    info!("TOTAL BlockTxCircuit::prove time:   {:?}", tx_prove_total);
    info!(
        "AVERAGE BlockTxCircuit::prove time: {:?}\n",
        tx_prove_total / chunks_count as u32
    );

    info!(
        "TOTAL BlockTxChainCircuit::prove time: {:?}",
        chain_prove_total
    );
    info!(
        "AVERAGE BlockTxChainCircuit::prove time: {:?}",
        chain_prove_total / chunks_count as u32
    );

    // Issue #67 acceptance: L4 over the serial fold's final chain proof.
    if args.l4_check {
        run_l4_check(
            args.tx_per_proof,
            &pre_exec_data,
            &chain_circuit_data,
            &block,
            &pre_proof,
            &current_chain_proof,
            "serial",
        );
    }

    let total_wall_ms = bench_start.elapsed().as_millis() as u64;
    let total_cpu_ms = diff_ms(bench_cpu_start, cpu_time_ms());
    events::emit(&BenchEvent::Summary {
        tx_per_proof: args.tx_per_proof,
        tx_limit: args.tx_limit,
        chunks: chunks_count,
        total_wall_ms,
        total_cpu_ms,
        peak_rss_mb: peak_rss_mb(),
        ts: now_iso8601(),
    });
}

/// Streaming-mode entrypoint (issue #49). Reads a trace-format.md
/// JSONL stream on stdin and proves chunk jobs from a recycled witness
/// pool until EOF, SIGINT/SIGTERM, or `--duration`.
///
/// Witness recycling: `bench_test.json` is loaded once, circuits are
/// built once, and the block's txs are pre-sliced into
/// `tx_per_proof`-sized chunks cycled round-robin. State rolls forward
/// chunk-to-chunk exactly as in batch mode within one pass over the
/// pool; when the pool wraps, state restarts from the block's initial
/// state -- each pool pass is an independent replay of the same
/// block's chunks. Only the *cadence* of proving is live; the content
/// repeats by design (proving cost is content-insensitive enough for
/// throughput benchmarking).
fn run_stream(args: &Args) {
    use std::sync::Arc;
    use std::sync::atomic::Ordering;
    use std::sync::mpsc::sync_channel;

    use bench::stream::{
        self, ChunkJob, Enqueuer, LayerStat, ProverOutput, StreamConfig, StreamShared,
    };
    use bench::trace;

    // Validate stream-only knobs before any expensive work.
    let deadline = match args.duration.as_deref() {
        Some(s) => match stream::parse_duration(s) {
            Ok(d) => Some(Instant::now() + d),
            Err(e) => {
                eprintln!("error: --duration: {e}");
                std::process::exit(2);
            }
        },
        None => None,
    };

    let block = get_test_block_json_file("bench_test.json");
    if block.txs.len() < args.tx_per_proof {
        eprintln!(
            "error: bench_test.json has {} txs but --tx-per-proof is {}; need at least one full chunk",
            block.txs.len(),
            args.tx_per_proof
        );
        std::process::exit(2);
    }

    // Same alignment rule as batch mode: every pool chunk has exactly
    // tx_per_proof txs so BlockTxCircuit::prove never sees a short
    // chunk (zip_eq panic).
    let aligned_limit = (args.tx_limit / args.tx_per_proof) * args.tx_per_proof;
    let effective_limit =
        aligned_limit.min((block.txs.len() / args.tx_per_proof) * args.tx_per_proof);
    let pool: Vec<Vec<_>> = block.txs[..effective_limit]
        .chunks(args.tx_per_proof)
        .map(|c| c.to_vec())
        .collect();
    let pool_total = pool.len();
    if pool_total == 0 {
        eprintln!(
            "error: witness pool is empty (tx_per_proof={}, tx_limit={}, txs_available={})",
            args.tx_per_proof,
            args.tx_limit,
            block.txs.len()
        );
        std::process::exit(2);
    }

    info!(
        "stream: witness pool = {} chunks x {} txs (recycled round-robin; each pool pass independently replays the block from its initial state)",
        pool_total, args.tx_per_proof
    );

    // ---- Circuit build: identical sequence and events to batch mode ----

    let l1_define_t = Instant::now();
    let circuit = BlockTxCircuit::define(CIRCUIT_CONFIG, args.tx_per_proof, CHAIN_ID);
    let bt = circuit.target;
    let data = circuit.builder.build::<C>();
    events::emit(&BenchEvent::CircuitDefine {
        layer: 1,
        name: "BlockTxCircuit",
        wall_ms: l1_define_t.elapsed().as_millis() as u64,
        rss_mb_after: current_rss_mb(),
        ts: now_iso8601(),
    });
    info!("BlockTxCircuit defined!");

    let l3_define_t = Instant::now();
    let pre_exec_circuit = BlockPreExecutionCircuit::define(CIRCUIT_CONFIG);
    let pbt = pre_exec_circuit.target;
    let pre_exec_data = pre_exec_circuit.builder.build::<C>();
    events::emit(&BenchEvent::CircuitDefine {
        layer: 3,
        name: "BlockPreExecutionCircuit",
        wall_ms: l3_define_t.elapsed().as_millis() as u64,
        rss_mb_after: current_rss_mb(),
        ts: now_iso8601(),
    });
    info!("BlockPreExecutionCircuit defined!");

    let l2_define_t = Instant::now();
    let chain_circuit = BlockTxChainCircuit::define(CIRCUIT_CONFIG, &data, args.tx_per_proof, 1);
    let chain_circuit_t = chain_circuit.target;
    let chain_circuit_data = chain_circuit.builder.build::<C>();
    let block_tx_witness_size = chain_circuit.block_tx_witness_size;
    events::emit(&BenchEvent::CircuitDefine {
        layer: 2,
        name: "BlockTxChainCircuit",
        wall_ms: l2_define_t.elapsed().as_millis() as u64,
        rss_mb_after: current_rss_mb(),
        ts: now_iso8601(),
    });
    info!("BlockTxChainCircuit defined!");

    let dummy_tx_chain_circuit = dummy_circuit(&chain_circuit_data.common);
    let dummy_proof = cyclic_base_proof(
        &chain_circuit_data.common,
        &chain_circuit_data.verifier_only,
        &dummy_tx_chain_circuit,
        Vec::<F>::new().iter().copied().enumerate().collect(),
    )
    .unwrap();

    let block_pre_exec = BlockPreExec::from_block(&block);

    // L3 once at startup: it anchors the cyclic base proof's state,
    // exactly as in batch mode.
    let l3_t = Instant::now();
    let l3_cpu_start = cpu_time_ms();
    let pre_proof = BlockPreExecutionCircuit::prove(&pre_exec_data, &block_pre_exec, &pbt)
        .unwrap_or_else(|err| panic!("Block pre-exec failed to prove. err = {:?}", err));
    events::emit(&BenchEvent::LayerProve {
        layer: 3,
        name: "BlockPreExecutionCircuit",
        chunk_idx: None,
        chunk_total: None,
        tx_per_proof: args.tx_per_proof,
        wall_ms: l3_t.elapsed().as_millis() as u64,
        cpu_ms: diff_ms(l3_cpu_start, cpu_time_ms()),
        rss_mb_peak: peak_rss_mb(),
        rss_mb_after: current_rss_mb(),
        ts: now_iso8601(),
    });

    let pre_exec_witness = BlockPreExecWitness::from_public_inputs(&pre_proof.public_inputs);
    let state_metadata = pre_exec_witness.new_state_metadata.clone();
    let created_at = block.created_at;

    // ---- Shutdown plumbing: SIGINT/SIGTERM -> shared flag ----

    let shared = Arc::new(StreamShared::new());
    let sig = stream::install_signal_handlers();
    {
        let shared = shared.clone();
        std::thread::spawn(move || {
            loop {
                if sig.load(Ordering::SeqCst) {
                    shared.request_shutdown();
                    return;
                }
                std::thread::sleep(Duration::from_millis(50));
            }
        });
    }

    // ---- Reader thread: stdin -> bounded queue ----

    let (job_tx, job_rx) = sync_channel::<ChunkJob>(args.max_queue);
    let enqueuer = Enqueuer::new(job_tx, shared.clone(), args.tx_per_proof);
    let reader_shared = shared.clone();
    std::thread::Builder::new()
        .name("trace-reader".into())
        .spawn(move || {
            let mut source = trace::stdin_source(reader_shared);
            stream::reader_loop(&mut source, &enqueuer);
        })
        .expect("failed to spawn trace-reader thread");

    // ---- Real prover closure over the recycled pool ----

    let mut pool_idx: usize = 0;
    let mut all_assets = block.all_assets.clone();
    let mut all_market_details = pre_exec_witness.new_market_details.clone();
    let mut system_config = block.old_system_config;
    let mut register_stack = block.register_stack_before;
    let mut account_tree_root = block.old_account_tree_root;
    let mut account_pub_data_tree_root = block.old_account_pub_data_tree_root;
    let mut account_delta_tree_root = block.old_account_delta_tree_root;
    let mut market_tree_root = block.old_market_tree_root;
    let mut current_chain_proof = BlockTxChainCircuit::cyclic_base_proof(
        &chain_circuit_data,
        &dummy_tx_chain_circuit,
        block.block_number,
        block.created_at,
        pre_exec_witness.new_state_root,
        pre_exec_witness.new_state_root,
        pre_exec_witness.new_validium_root,
        block.old_account_delta_tree_root,
        block_tx_witness_size,
        &state_metadata,
    );

    let mut prove = |_job: &ChunkJob| -> ProverOutput {
        let block_tx = BlockTx {
            created_at,
            old_system_config: system_config,
            register_stack_before: register_stack,
            all_assets_before: all_assets.clone(),
            all_market_details_before: all_market_details.clone(),
            old_account_tree_root: account_tree_root,
            old_account_pub_data_tree_root: account_pub_data_tree_root,
            old_account_delta_tree_root: account_delta_tree_root,
            old_market_tree_root: market_tree_root,
            txs: pool[pool_idx].clone(),
        };

        let l1_t = Instant::now();
        let l1_cpu_start = cpu_time_ms();
        let tx_proof = BlockTxCircuit::prove(&data, &block_tx, &bt).unwrap_or_else(|err| {
            panic!("Failed to prove pool chunk #{}. err = {:?}", pool_idx, err)
        });
        let l1_stat = LayerStat {
            layer: 1,
            name: "BlockTxCircuit",
            wall_ms: l1_t.elapsed().as_millis() as u64,
            cpu_ms: diff_ms(l1_cpu_start, cpu_time_ms()),
            completed_at: Instant::now(),
        };

        let tx_witness = BlockTxWitness::from_public_inputs(&tx_proof.public_inputs.clone());
        all_assets = tx_witness.all_assets_after.clone();
        all_market_details = tx_witness.all_market_details_after.clone();
        register_stack = tx_witness.register_stack_after;
        system_config = tx_witness.new_system_config;
        account_tree_root = tx_witness.new_account_tree_root;
        account_pub_data_tree_root = tx_witness.new_account_pub_data_tree_root;
        account_delta_tree_root = tx_witness.new_account_delta_tree_root;
        market_tree_root = tx_witness.new_market_tree_root;

        let l2_t = Instant::now();
        let l2_cpu_start = cpu_time_ms();
        let chain_proof = BlockTxChainCircuit::prove(
            &chain_circuit_t,
            &chain_circuit_data,
            pool_idx as u64,
            &current_chain_proof,
            &dummy_proof,
            &tx_proof,
        )
        .unwrap_or_else(|err| panic!("Block Chain circuit failed to prove. err = {:?}", err));
        let l2_stat = LayerStat {
            layer: 2,
            name: "BlockTxChainCircuit",
            wall_ms: l2_t.elapsed().as_millis() as u64,
            cpu_ms: diff_ms(l2_cpu_start, cpu_time_ms()),
            completed_at: Instant::now(),
        };
        current_chain_proof = chain_proof;

        let out = ProverOutput {
            pool_chunk_idx: pool_idx,
            pool_chunk_total: pool_total,
            layers: vec![l1_stat, l2_stat],
        };

        pool_idx = (pool_idx + 1) % pool_total;
        if pool_idx == 0 {
            // Pool wrap: restart from the block's initial state. Each
            // pass over the pool is an independent replay of the same
            // block's chunks (see module docs + bench/README.md).
            all_assets = block.all_assets.clone();
            all_market_details = pre_exec_witness.new_market_details.clone();
            system_config = block.old_system_config;
            register_stack = block.register_stack_before;
            account_tree_root = block.old_account_tree_root;
            account_pub_data_tree_root = block.old_account_pub_data_tree_root;
            account_delta_tree_root = block.old_account_delta_tree_root;
            market_tree_root = block.old_market_tree_root;
            current_chain_proof = BlockTxChainCircuit::cyclic_base_proof(
                &chain_circuit_data,
                &dummy_tx_chain_circuit,
                block.block_number,
                block.created_at,
                pre_exec_witness.new_state_root,
                pre_exec_witness.new_state_root,
                pre_exec_witness.new_validium_root,
                block.old_account_delta_tree_root,
                block_tx_witness_size,
                &state_metadata,
            );
            info!("stream: witness pool wrapped; state reset to block initial state");
        }

        out
    };

    // Optional L3 cadence (--l3-every N).
    let mut l3_fn = || {
        let t = Instant::now();
        let cpu_start = cpu_time_ms();
        if let Err(err) = BlockPreExecutionCircuit::prove(&pre_exec_data, &block_pre_exec, &pbt) {
            panic!("Block pre-exec failed to prove. err = {:?}", err);
        }
        events::emit(&BenchEvent::LayerProve {
            layer: 3,
            name: "BlockPreExecutionCircuit",
            chunk_idx: None,
            chunk_total: None,
            tx_per_proof: args.tx_per_proof,
            wall_ms: t.elapsed().as_millis() as u64,
            cpu_ms: diff_ms(cpu_start, cpu_time_ms()),
            rss_mb_peak: peak_rss_mb(),
            rss_mb_after: current_rss_mb(),
            ts: now_iso8601(),
        });
    };
    let mut l3_opt: Option<&mut dyn FnMut()> = if args.l3_every.is_some() {
        Some(&mut l3_fn)
    } else {
        None
    };

    // ---- Prover loop (main thread) ----

    let cfg = StreamConfig {
        tx_per_proof: args.tx_per_proof,
        summary_every: stream::SUMMARY_PERIOD,
        deadline,
        l3_every: args.l3_every,
    };
    let outcome = stream::run_prover_loop(job_rx, &shared, &cfg, &mut prove, l3_opt.take());

    info!(
        "stream: done -- {} chunks proven in {:?} ({} arrivals, {} gaps skipped, {} dropped chunks)",
        outcome.chunks_proven,
        outcome.elapsed,
        shared.arrivals.load(Ordering::Relaxed),
        shared.gaps_skipped.load(Ordering::Relaxed),
        shared.dropped_chunks.load(Ordering::Relaxed),
    );

    if let Some(msg) = shared.fatal_message() {
        eprintln!("error: trace contract violation: {msg}");
        std::process::exit(1);
    }
    // Note: the reader thread may still be blocked on a stdin read;
    // returning from main terminates the process regardless. Exit 0.
}

/// Issue #67: tree-fold L2 driver (batch mode).
///
/// Per chunk: prove the L1 chunk proof, then a LEAF chain proof (a 1-chunk
/// chain: a fresh cyclic base proof seeded at the chunk's pre-state + one
/// chain step at tx_index = 0). Then merge adjacent proofs pairwise up the
/// tree with `BlockTxChainMergeCircuit`; odd proofs at any level are carried
/// up unchanged (the merge circuit accepts leaf and merge children in any
/// mix). Sequential execution throughout -- parallel scheduling is the cell
/// implementation's job (#3).
///
/// Per-leaf base-proof seeding: chunk k's base proof needs the state and
/// validium roots BEFORE chunk k. Chunk 0 takes them from L3 (pre-exec);
/// chunk k > 0 takes them from leaf k-1's proven outputs (the driver is
/// sequential, so they are always available). A parallel driver would
/// compute them natively from witness data instead.
#[allow(clippy::too_many_arguments)]
fn run_tree_fold(
    args: &Args,
    block: &Block<F>,
    effective_limit: usize,
    chunks_count: usize,
    l1_data: &CircuitData<F, C, D>,
    bt: &BlockTxTarget,
    pre_exec_data: &CircuitData<F, C, D>,
    pre_proof: &ProofWithPublicInputs<F, C, D>,
    pre_exec_witness: &BlockPreExecWitness<F>,
    state_metadata: &StateMetadata,
    chain_target: &BlockTxChainTarget,
    chain_data: &CircuitData<F, C, D>,
    block_tx_witness_size: usize,
    dummy_chain_circuit: &CircuitData<F, C, D>,
    dummy_proof: &ProofWithPublicInputs<F, C, D>,
    bench_start: Instant,
    bench_cpu_start: Option<u64>,
) {
    // ---- Merge circuit: define + build + self-shape assertion.
    let merge_define_t = Instant::now();
    let merge_circuit = BlockTxChainMergeCircuit::define(CIRCUIT_CONFIG, chain_data, 1);
    let merge_target = merge_circuit.target;
    let merge_data = merge_circuit.builder.build::<C>();
    events::emit(&BenchEvent::CircuitDefine {
        layer: 2,
        name: "BlockTxChainMergeCircuit",
        wall_ms: merge_define_t.elapsed().as_millis() as u64,
        rss_mb_after: current_rss_mb(),
        ts: now_iso8601(),
    });
    info!(
        "BlockTxChainMergeCircuit defined! (degree 2^{}, {} public inputs)",
        merge_data.common.degree_bits(),
        merge_data.common.num_public_inputs
    );
    // The custom conditional-VK verify helper cannot set the fork's
    // goal_common_data, so the cyclic fixed point is enforced here instead:
    // the merge circuit must build into the leaf chain circuit's EXACT
    // shape (which is itself the goal-asserted 2^14 fixed point).
    assert!(
        merge_data.common == chain_data.common,
        "BlockTxChainMergeCircuit must build into the leaf chain circuit's exact self-shape \
         (issue #67); see Builder::verify_leaf_or_cyclic_proof docs. \
         merge: degree 2^{} / {} PIs, leaf: degree 2^{} / {} PIs",
        merge_data.common.degree_bits(),
        merge_data.common.num_public_inputs,
        chain_data.common.degree_bits(),
        chain_data.common.num_public_inputs,
    );
    info!(
        "BlockTxChainMergeCircuit common data matches the leaf chain circuit's (fixed point closed)"
    );

    // ---- Per-chunk: L1 proof + LEAF chain proof (rolling state, as batch).
    let mut all_assets = block.all_assets.clone();
    let mut all_market_details = pre_exec_witness.new_market_details.clone();
    let mut system_config = block.old_system_config;
    let mut register_stack = block.register_stack_before;
    let mut account_tree_root = block.old_account_tree_root;
    let mut account_pub_data_tree_root = block.old_account_pub_data_tree_root;
    let mut account_delta_tree_root = block.old_account_delta_tree_root;
    let mut market_tree_root = block.old_market_tree_root;
    let created_at = block.created_at;

    let mut pre_state_root = pre_exec_witness.new_state_root;
    let mut pre_validium_root = pre_exec_witness.new_validium_root;

    let mut tx_prove_total = Duration::ZERO;
    let mut leaf_prove_total = Duration::ZERO; // includes per-leaf base-proof generation
    let mut base_proof_total = Duration::ZERO;
    let mut tx_proofs: Vec<ProofWithPublicInputs<F, C, D>> = Vec::with_capacity(chunks_count);
    let mut leaves: Vec<ProofWithPublicInputs<F, C, D>> = Vec::with_capacity(chunks_count);

    for (index, tx) in block.txs[..effective_limit]
        .chunks(args.tx_per_proof)
        .enumerate()
    {
        let block_tx = BlockTx {
            created_at,
            old_system_config: system_config,
            register_stack_before: register_stack,
            all_assets_before: all_assets.clone(),
            all_market_details_before: all_market_details.clone(),
            old_account_tree_root: account_tree_root,
            old_account_pub_data_tree_root: account_pub_data_tree_root,
            old_account_delta_tree_root: account_delta_tree_root,
            old_market_tree_root: market_tree_root,
            txs: tx.to_vec(),
        };
        let pre_delta_root = account_delta_tree_root;

        let tx_dt = Instant::now();
        let l1_cpu_start = cpu_time_ms();
        let tx_proof = BlockTxCircuit::prove(l1_data, &block_tx, bt)
            .unwrap_or_else(|err| panic!("Failed to prove tx chunk #{index}. err = {err:?}"));
        let tx_dt = tx_dt.elapsed();
        events::emit(&BenchEvent::LayerProve {
            layer: 1,
            name: "BlockTxCircuit",
            chunk_idx: Some(index),
            chunk_total: Some(chunks_count),
            tx_per_proof: args.tx_per_proof,
            wall_ms: tx_dt.as_millis() as u64,
            cpu_ms: diff_ms(l1_cpu_start, cpu_time_ms()),
            rss_mb_peak: peak_rss_mb(),
            rss_mb_after: current_rss_mb(),
            ts: now_iso8601(),
        });
        info!(
            "tx chunk #{index}/{} BlockTxCircuit::prove time: {:?}",
            chunks_count, tx_dt
        );
        tx_prove_total += tx_dt;

        let tx_witness = BlockTxWitness::from_public_inputs(&tx_proof.public_inputs.clone());
        all_assets = tx_witness.all_assets_after.clone();
        all_market_details = tx_witness.all_market_details_after.clone();
        register_stack = tx_witness.register_stack_after;
        system_config = tx_witness.new_system_config;
        account_tree_root = tx_witness.new_account_tree_root;
        account_pub_data_tree_root = tx_witness.new_account_pub_data_tree_root;
        account_delta_tree_root = tx_witness.new_account_delta_tree_root;
        market_tree_root = tx_witness.new_market_tree_root;

        // LEAF chain proof: 1-chunk chain seeded at this chunk's pre-state.
        let leaf_dt = Instant::now();
        let l2_cpu_start = cpu_time_ms();
        let base_t = Instant::now();
        let base_proof = BlockTxChainCircuit::cyclic_base_proof(
            chain_data,
            dummy_chain_circuit,
            block.block_number,
            block.created_at,
            pre_state_root,
            pre_state_root,
            pre_validium_root,
            pre_delta_root,
            block_tx_witness_size,
            state_metadata,
        );
        base_proof_total += base_t.elapsed();
        let leaf_proof = BlockTxChainCircuit::prove(
            chain_target,
            chain_data,
            0, // every leaf is the first (and only) step of its own chain
            &base_proof,
            dummy_proof,
            &tx_proof,
        )
        .unwrap_or_else(|err| panic!("Leaf chain proof #{index} failed. err = {err:?}"));
        let leaf_dt = leaf_dt.elapsed();
        events::emit(&BenchEvent::LayerProve {
            layer: 2,
            name: "BlockTxChainCircuit",
            chunk_idx: Some(index),
            chunk_total: Some(chunks_count),
            tx_per_proof: args.tx_per_proof,
            wall_ms: leaf_dt.as_millis() as u64,
            cpu_ms: diff_ms(l2_cpu_start, cpu_time_ms()),
            rss_mb_peak: peak_rss_mb(),
            rss_mb_after: current_rss_mb(),
            ts: now_iso8601(),
        });
        info!(
            "tx chunk #{index}/{} leaf BlockTxChainCircuit::prove time (incl. base proof): {:?}\n",
            chunks_count, leaf_dt
        );
        leaf_prove_total += leaf_dt;

        // The next chunk's base proof is seeded from this leaf's proven
        // outputs (state + validium roots after this chunk).
        let leaf_witness = BlockTxChainWitness::from_public_inputs(&leaf_proof.public_inputs, 1, 1);
        pre_state_root = leaf_witness.new_state_root;
        pre_validium_root = leaf_witness.new_validium_root;

        tx_proofs.push(tx_proof);
        leaves.push(leaf_proof);
    }

    // ---- Pairwise merge up the tree. Each entry carries (proof, is_merge).
    let mut merge_prove_total = Duration::ZERO;
    let mut merges = 0usize;
    let mut depth = 0usize;
    let mut level: Vec<(ProofWithPublicInputs<F, C, D>, bool)> =
        leaves.into_iter().map(|p| (p, false)).collect();

    while level.len() > 1 {
        depth += 1;
        let mut next = Vec::with_capacity(level.len() / 2 + 1);
        let mut iter = level.into_iter();
        while let Some(left) = iter.next() {
            match iter.next() {
                Some(right) => {
                    let merge_dt = Instant::now();
                    let merge_cpu_start = cpu_time_ms();
                    let proof = BlockTxChainMergeCircuit::prove(
                        &merge_target,
                        &merge_data,
                        &left.0,
                        left.1,
                        &right.0,
                        right.1,
                    )
                    .unwrap_or_else(|err| {
                        panic!("Merge #{merges} (level {depth}) failed. err = {err:?}")
                    });
                    let merge_dt = merge_dt.elapsed();
                    events::emit(&BenchEvent::LayerProve {
                        layer: 2,
                        name: "BlockTxChainMergeCircuit",
                        chunk_idx: Some(merges),
                        chunk_total: Some(chunks_count.saturating_sub(1)),
                        tx_per_proof: args.tx_per_proof,
                        wall_ms: merge_dt.as_millis() as u64,
                        cpu_ms: diff_ms(merge_cpu_start, cpu_time_ms()),
                        rss_mb_peak: peak_rss_mb(),
                        rss_mb_after: current_rss_mb(),
                        ts: now_iso8601(),
                    });
                    info!(
                        "merge #{merges} (level {depth}) BlockTxChainMergeCircuit::prove time: {:?}",
                        merge_dt
                    );
                    merge_prove_total += merge_dt;
                    merges += 1;
                    next.push((proof, true));
                }
                None => {
                    info!("level {depth}: odd proof carried up to the next level");
                    next.push(left);
                }
            }
        }
        level = next;
    }
    let (final_proof, final_is_merge) = level.pop().expect("tree fold produced no final proof");

    // ---- Reporting (existing TOTAL/AVERAGE stdout idiom + TREEFOLD line).
    info!("TOTAL BlockTxCircuit::prove time:   {:?}", tx_prove_total);
    info!(
        "AVERAGE BlockTxCircuit::prove time: {:?}\n",
        tx_prove_total / chunks_count as u32
    );
    info!(
        "TOTAL leaf BlockTxChainCircuit::prove time (incl. base proofs): {:?}",
        leaf_prove_total
    );
    info!(
        "AVERAGE leaf BlockTxChainCircuit::prove time: {:?} (of which base-proof avg {:?})",
        leaf_prove_total / chunks_count as u32,
        base_proof_total / chunks_count as u32
    );
    let merge_avg = if merges > 0 {
        merge_prove_total / merges as u32
    } else {
        Duration::ZERO
    };
    if merges > 0 {
        info!(
            "TOTAL BlockTxChainMergeCircuit::prove time: {:?}",
            merge_prove_total
        );
        info!(
            "AVERAGE BlockTxChainMergeCircuit::prove time: {:?}",
            merge_avg
        );
    }
    // Critical path = depth x avg merge step: with parallel leaf workers and
    // parallel merges across disjoint pairs, only one merge per level is
    // serial (the metric ADR-0003 S D3 cares about).
    let critical_path = merge_avg * depth as u32;
    info!(
        "TREEFOLD chunks={} depth={} merges={} leaf_avg={:?} merge_avg={:?} critical_path={:?} (depth x avg merge) total_tree_work={:?}",
        chunks_count,
        depth,
        merges,
        leaf_prove_total / chunks_count as u32,
        merge_avg,
        critical_path,
        leaf_prove_total + merge_prove_total,
    );

    // ---- A/B: serial fold over the SAME L1 proofs; final PIs must match.
    if args.ab_check {
        info!(
            "AB_CHECK: running serial fold over the same {} L1 chunk proofs...",
            chunks_count
        );
        let mut serial_total = Duration::ZERO;
        let mut current_chain_proof = BlockTxChainCircuit::cyclic_base_proof(
            chain_data,
            dummy_chain_circuit,
            block.block_number,
            block.created_at,
            pre_exec_witness.new_state_root,
            pre_exec_witness.new_state_root,
            pre_exec_witness.new_validium_root,
            block.old_account_delta_tree_root,
            block_tx_witness_size,
            state_metadata,
        );
        for (index, tx_proof) in tx_proofs.iter().enumerate() {
            let dt = Instant::now();
            current_chain_proof = BlockTxChainCircuit::prove(
                chain_target,
                chain_data,
                index as u64,
                &current_chain_proof,
                dummy_proof,
                tx_proof,
            )
            .unwrap_or_else(|err| panic!("AB_CHECK serial step #{index} failed. err = {err:?}"));
            serial_total += dt.elapsed();
        }
        info!(
            "AB_CHECK serial fold latency: {:?} ({} steps, avg {:?}) vs tree critical path {:?}",
            serial_total,
            chunks_count,
            serial_total / chunks_count as u32,
            critical_path
        );

        // Semantic PI surface: chain witness + state metadata + the #67
        // range-start delta root. The trailing verifier-key PIs differ by
        // construction (leaf VK in the serial proof, merge VK in the tree
        // root) and are intentionally excluded.
        let semantic_len = block_tx_witness_size + STATE_METADATA_SIZE + 4;
        let serial_pis = &current_chain_proof.public_inputs[..semantic_len];
        let tree_pis = &final_proof.public_inputs[..semantic_len];
        let mismatches: Vec<usize> = (0..semantic_len)
            .filter(|&i| serial_pis[i] != tree_pis[i])
            .collect();
        if mismatches.is_empty() {
            info!(
                "AB_CHECK PASS: all {} semantic public inputs element-wise equal \
                 (trailing verifier-key PIs differ by design: leaf VK vs merge VK)",
                semantic_len
            );
        } else {
            eprintln!(
                "AB_CHECK FAIL: {} of {} semantic public inputs differ; first mismatching indices: {:?}",
                mismatches.len(),
                semantic_len,
                &mismatches[..mismatches.len().min(16)]
            );
            std::process::exit(1);
        }
    }

    // ---- L4 over the tree-folded final proof.
    if args.l4_check {
        let (l4_chain_data, label) = if final_is_merge {
            (&merge_data, "tree (merge VK)")
        } else {
            // Single-chunk block: no merge happened; the final proof is the
            // (sole) leaf proof, so L4 verifies against the leaf chain VK.
            (chain_data, "tree (single leaf, leaf VK)")
        };
        run_l4_check(
            args.tx_per_proof,
            pre_exec_data,
            l4_chain_data,
            block,
            pre_proof,
            &final_proof,
            label,
        );
    }

    let total_wall_ms = bench_start.elapsed().as_millis() as u64;
    let total_cpu_ms = diff_ms(bench_cpu_start, cpu_time_ms());
    events::emit(&BenchEvent::Summary {
        tx_per_proof: args.tx_per_proof,
        tx_limit: args.tx_limit,
        chunks: chunks_count,
        total_wall_ms,
        total_cpu_ms,
        peak_rss_mb: peak_rss_mb(),
        ts: now_iso8601(),
    });
}

/// Issue #67 acceptance: define+build L4 (`BlockCircuit`) against the
/// circuit that produced the final chain proof, patch the block's `new_*`
/// fields to match the (possibly partial) chain run -- the `l45probe` trick
/// archived on issue #10 -- then prove and verify L4 with the chain proof as
/// `tx_chain_proof`.
#[allow(clippy::too_many_arguments)]
fn run_l4_check(
    tx_per_proof: usize,
    l3_data: &CircuitData<F, C, D>,
    chain_like_data: &CircuitData<F, C, D>,
    block: &Block<F>,
    pre_proof: &ProofWithPublicInputs<F, C, D>,
    chain_proof: &ProofWithPublicInputs<F, C, D>,
    label: &str,
) {
    // Patch the block to match the PARTIAL chain run: L4 connects the
    // witness Block's final values to the chain proof's outputs, and our
    // chain proof may cover only --tx-limit txs.
    let cw = BlockTxChainWitness::from_public_inputs(&chain_proof.public_inputs, 1, 1);
    let mut pblock = block.clone();
    pblock.new_validium_root = cw.new_validium_root;
    pblock.new_state_root = cw.new_state_root;
    pblock.new_account_delta_tree_root = cw.new_account_delta_tree_root;
    pblock.on_chain_operations_count = cw.on_chain_operations_count;
    pblock.on_chain_operations_pub_data = cw.on_chain_operations_pub_data.clone();
    pblock.priority_operations_count = cw.priority_operations_count;
    pblock.new_public_market_details = cw.new_public_market_details.clone();
    pblock.new_prefix_priority_operation_hash = if cw.priority_operations_count != 0 {
        // Mirror the in-circuit calc: keccak(old_prefix_hash || priority_pub_data)
        let mut input = Vec::with_capacity(32 + cw.priority_operations_pub_data.len());
        input.extend_from_slice(&block.old_prefix_priority_operation_hash);
        input.extend_from_slice(&cw.priority_operations_pub_data);
        keccak(&input)
    } else {
        block.old_prefix_priority_operation_hash
    };

    let define_t = Instant::now();
    let l4 = BlockCircuit::define(CIRCUIT_CONFIG, l3_data, chain_like_data, 1);
    let l4_target = l4.target;
    let l4_data = l4.builder.build::<C>();
    events::emit(&BenchEvent::CircuitDefine {
        layer: 4,
        name: "BlockCircuit",
        wall_ms: define_t.elapsed().as_millis() as u64,
        rss_mb_after: current_rss_mb(),
        ts: now_iso8601(),
    });
    info!(
        "L4_CHECK [{label}] BlockCircuit defined+built in {:?} (degree 2^{})",
        define_t.elapsed(),
        l4_data.common.degree_bits()
    );

    let prove_t = Instant::now();
    let l4_cpu_start = cpu_time_ms();
    let pw = BlockCircuit::generate_witness(&l4_target, &pblock, pre_proof, chain_proof)
        .unwrap_or_else(|err| panic!("L4_CHECK [{label}] witness generation failed: {err:?}"));
    let l4_proof = l4_data
        .prove(pw)
        .unwrap_or_else(|err| panic!("L4_CHECK [{label}] prove failed: {err:?}"));
    l4_data
        .verify(l4_proof.clone())
        .unwrap_or_else(|err| panic!("L4_CHECK [{label}] verify failed: {err:?}"));
    let prove_dt = prove_t.elapsed();
    events::emit(&BenchEvent::LayerProve {
        layer: 4,
        name: "BlockCircuit",
        chunk_idx: None,
        chunk_total: None,
        tx_per_proof,
        wall_ms: prove_dt.as_millis() as u64,
        cpu_ms: diff_ms(l4_cpu_start, cpu_time_ms()),
        rss_mb_peak: peak_rss_mb(),
        rss_mb_after: current_rss_mb(),
        ts: now_iso8601(),
    });
    info!(
        "L4_CHECK [{label}] PASS: BlockCircuit proved+verified the final chain proof in {:?}",
        prove_dt
    );
}

/// Compute the delta between two CPU-time samples. Returns `None` if
/// either sample is unavailable (e.g. non-Linux) or if the end sample
/// is somehow earlier than the start.
fn diff_ms(start: Option<u64>, end: Option<u64>) -> Option<u64> {
    match (start, end) {
        (Some(s), Some(e)) if e >= s => Some(e - s),
        _ => None,
    }
}

pub fn get_test_block_json_file(file_name: &str) -> Block<F> {
    let path = Path::new(".").join(file_name);
    let data = fs::read_to_string(path).expect("Unable to read file");

    serde_json::from_str(&data).expect("JSON does not have correct format.")
}

struct NoWarnLogger(env_logger::Logger);

impl Log for NoWarnLogger {
    fn enabled(&self, metadata: &Metadata) -> bool {
        metadata.level() != Level::Warn && self.0.enabled(metadata)
    }

    fn log(&self, record: &Record) {
        if record.level() == Level::Warn {
            return;
        }
        self.0.log(record)
    }

    fn flush(&self) {
        self.0.flush()
    }
}

fn init_logger_no_warn() {
    let env = Env::default().filter_or(DEFAULT_FILTER_ENV, "info");
    let mut b = Builder::from_env(env);
    b.filter_level(LevelFilter::Info);
    let inner = b.build();

    let _ = log::set_boxed_logger(Box::new(NoWarnLogger(inner)));
    log::set_max_level(LevelFilter::Info);
}

/// Emit a single info!() line that fully describes the host and the run
/// configuration. Pure stdlib + /proc parsing -- no heavy crates.
fn log_machine_metadata(args: &Args) {
    let hostname = read_hostname();
    let (cpu_model, cpu_cores) = read_cpu_info();
    let mem_total = read_mem_total();
    let git_sha = option_env!("GIT_SHA").unwrap_or("unknown");

    info!(
        "BENCH_META host={} cpu=\"{}\" cores={} ram={} git_sha={} tx_per_proof={} tx_limit={}",
        hostname, cpu_model, cpu_cores, mem_total, git_sha, args.tx_per_proof, args.tx_limit,
    );
}

fn read_hostname() -> String {
    if let Ok(h) = std::env::var("HOSTNAME") {
        if !h.is_empty() {
            return h;
        }
    }
    if let Ok(h) = fs::read_to_string("/etc/hostname") {
        let h = h.trim();
        if !h.is_empty() {
            return h.to_string();
        }
    }
    match std::process::Command::new("uname").arg("-n").output() {
        Ok(out) if out.status.success() => String::from_utf8_lossy(&out.stdout).trim().to_string(),
        _ => "unknown".to_string(),
    }
}

fn read_cpu_info() -> (String, usize) {
    let mut model = String::from("unknown");
    let mut cores = 0usize;
    if let Ok(content) = fs::read_to_string("/proc/cpuinfo") {
        for line in content.lines() {
            if model == "unknown" && line.starts_with("model name") {
                if let Some(idx) = line.find(':') {
                    model = line[idx + 1..].trim().to_string();
                }
            }
            if line.starts_with("processor") {
                cores += 1;
            }
        }
    }
    if cores == 0 {
        if let Ok(out) = std::process::Command::new("nproc").output() {
            if let Ok(s) = String::from_utf8(out.stdout) {
                cores = s.trim().parse().unwrap_or(0);
            }
        }
    }
    (model, cores)
}

fn read_mem_total() -> String {
    if let Ok(content) = fs::read_to_string("/proc/meminfo") {
        for line in content.lines() {
            if line.starts_with("MemTotal:") {
                return line
                    .split(':')
                    .nth(1)
                    .map(|s| s.trim().to_string())
                    .unwrap_or_default();
            }
        }
    }
    "unknown".to_string()
}
