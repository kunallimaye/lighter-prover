// Copyright (c) Elliot Technologies, Inc.
// SPDX-License-Identifier: BUSL-1.1

#![feature(stmt_expr_attributes)]
#![allow(unused_imports)]

use std::fs;
use std::path::Path;
use std::time::{Duration, Instant};

use circuit::block::{Block, BlockWitness};
use circuit::block_constraints::{BlockCircuit, Circuit as _};
use circuit::block_pre_execution::{BlockPreExec, BlockPreExecWitness};
use circuit::block_pre_execution_constraints::{BlockPreExecutionCircuit, Circuit as _};
use circuit::block_tx::{BlockTx, BlockTxWitness};
use circuit::block_tx_chain::BlockTxChainWitness;
use circuit::block_tx_chain_constraints::{BlockTxChainCircuit, Circuit as _};
use circuit::block_tx_constraints::{BlockTxCircuit, BlockTxTarget, Circuit as _};
use circuit::builder::custom::cyclic_base_proof;
use circuit::tx;
use circuit::types::config::{C, CIRCUIT_CONFIG, F};
use circuit::types::constants::*;
use circuit::types::state_metadata::StateMetadata;
use circuit::types::{account_delta, state_metadata};
use clap::Parser;
use env_logger::{Builder, DEFAULT_FILTER_ENV, Env, try_init_from_env};
use log::{Level, LevelFilter, Log, Metadata, Record, debug, info};
use plonky2::field::goldilocks_field::GoldilocksField;
use plonky2::field::types::PrimeField64;
use plonky2::plonk::proof::CompressedProofWithPublicInputs;
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
}

fn main() {
    init_logger_no_warn();

    let args = Args::parse();

    const UPSTREAM_TESTED_MAX_TX_PER_PROOF: usize = 6;
    if args.tx_per_proof > UPSTREAM_TESTED_MAX_TX_PER_PROOF {
        eprintln!(
            "error: --tx-per-proof {} exceeds the upstream-tested maximum of {}.\n\
             \n\
             Upstream lighter-prover (commit 5bbb307) sets log_gates = 14 in\n\
             circuit/src/block_tx_chain_constraints.rs:128 when tx_per_proof > 6.\n\
             That setting is insufficient: the chain-recursion verifier circuit\n\
             requires more than 2^14 = 16384 rows for tx_per_proof in {{7, 8, 16, 32}}\n\
             and panics at build time with 'Failed to build circuit'.\n\
             \n\
             Empirically validated chunk sizes on upstream 5bbb307: 1, 2, 3, 4, 5, 6.\n\
             \n\
             See https://github.com/kunallimaye/lighter-prover/issues/8 for the\n\
             log_gates analysis and proposed fix paths (bump constant, tiered\n\
             table, or dynamic computation from CommonCircuitData).",
            args.tx_per_proof, UPSTREAM_TESTED_MAX_TX_PER_PROOF
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

    log_machine_metadata(&args);

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
    let effective_limit = aligned_limit.min(
        (block.txs.len() / args.tx_per_proof) * args.tx_per_proof,
    );
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

    let circuit = BlockTxCircuit::define(CIRCUIT_CONFIG, args.tx_per_proof, CHAIN_ID);
    let bt = circuit.target;
    let data = circuit.builder.build::<C>();
    info!("BlockTxCircuit defined!");
    info!(
        "BlockTxCircuit # public inputs = {:?}",
        data.common.num_public_inputs
    );
    info!(
        "BlockTxCircuit # num_gate_constraints = {:?}",
        data.common.num_gate_constraints
    );

    let pre_exec_circuit = BlockPreExecutionCircuit::define(CIRCUIT_CONFIG);
    let pbt = pre_exec_circuit.target;
    let pre_exec_data = pre_exec_circuit.builder.build::<C>();
    info!("BlockPreExecutionCircuit defined!");

    let chain_circuit = BlockTxChainCircuit::define(CIRCUIT_CONFIG, &data, args.tx_per_proof, 1);
    let chain_circuit_t = chain_circuit.target;
    let chain_circuit_data = chain_circuit.builder.build::<C>();
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
    let pre_proof = BlockPreExecutionCircuit::prove(&pre_exec_data, &block_pre_exec, &pbt);
    if let Err(err) = pre_proof {
        panic!("Block pre-exec failed to prove. err = {:?}", err);
    }
    let pre_proof = pre_proof.unwrap();
    let pre_execution_total = pre_execution_time.elapsed();

    let pre_exec_witness =
        BlockPreExecWitness::from_public_inputs(&pre_proof.clone().public_inputs);

    let state_metadata = pre_exec_witness.new_state_metadata.clone();
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
        let tx_proof = BlockTxCircuit::prove(&data, &block_tx, &bt);
        let tx_dt = tx_dt.elapsed();
        if let Err(err) = tx_proof {
            panic!("Failed to prove tx chunk #{}. err = {:?}", index, err);
        }

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
        let chain_proof = BlockTxChainCircuit::prove(
            &chain_circuit_t,
            &chain_circuit_data,
            index as u64,
            &current_chain_proof,
            &dummy_proof,
            &tx_proof,
        );
        let chain_dt = chain_dt.elapsed();
        if let Err(err) = chain_proof {
            panic!("Block Chain circuit failed to prove. err = {:?}", err);
        }

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
        hostname,
        cpu_model,
        cpu_cores,
        mem_total,
        git_sha,
        args.tx_per_proof,
        args.tx_limit,
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
                return line.split(':').nth(1).map(|s| s.trim().to_string()).unwrap_or_default();
            }
        }
    }
    "unknown".to_string()
}
