// Copyright (c) Elliot Technologies, Inc.
// SPDX-License-Identifier: BUSL-1.1

#![feature(stmt_expr_attributes)]
#![allow(unused_imports)]

use std::fs;
use std::path::Path;
use std::sync::Mutex;
use std::time::{Duration, Instant};

use clap::Parser;
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
use circuit::types::config::{C, CIRCUIT_CONFIG, D, F};
use circuit::types::constants::*;
use circuit::types::state_metadata::StateMetadata;
use circuit::types::{account_delta, state_metadata};
use env_logger::{Builder, DEFAULT_FILTER_ENV, Env, try_init_from_env};
use log::{Level, LevelFilter, Log, Metadata, Record, debug, info};
use plonky2::field::goldilocks_field::GoldilocksField;
use plonky2::field::types::PrimeField64;
use plonky2::plonk::proof::{CompressedProofWithPublicInputs, ProofWithPublicInputs};
use plonky2::recursion::dummy_circuit::{self, dummy_circuit};
use plonky2::util::timing::TimingTree;
use rayon::vec;

const CHAIN_ID: u32 = 304;

static UNSTRUCTURED_LOGS: Mutex<Vec<String>> = Mutex::new(Vec::new());
static STAGE_LOGS: Mutex<Vec<String>> = Mutex::new(Vec::new());

#[derive(Parser, Debug)]
#[command(author, version, about = "Lighter STARK Proving Observatory")]
struct Cli {
    #[arg(long, default_value = "reports")]
    reports_dir: String,

    #[arg(long, default_value_t = 4)]
    tx_per_proof: usize,
}

fn main() {
    let args = Cli::parse();
    init_logger_no_warn();

    let tx_per_proof = args.tx_per_proof;
    let block = get_test_block_json_file("bench_test.json");
    let tx_chunks: Vec<_> = block.txs.chunks(tx_per_proof).collect();
    let chunks_count = tx_chunks.len();

    info!(
        concat!(
            "Tx and chain circuits are configured to prove {} txs per proof in each iteration. ",
            "There are {} txs in the test block, so there will be {} iterations of proving.\n\n"
        ),
        tx_per_proof,
        block.txs.len(),
        chunks_count
    );

    let circuit = BlockTxCircuit::define(CIRCUIT_CONFIG, tx_per_proof, CHAIN_ID);
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

    let chain_circuit = BlockTxChainCircuit::define(CIRCUIT_CONFIG, &data, tx_per_proof, 1);
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
    let mut account_delta_tree_root = block.old_account_delta_tree_root;
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
    let mut witness_total = Duration::ZERO;
    let mut stark_prove_total = Duration::ZERO;

    struct PipedProofItem {
        index: usize,
        tx_proof: ProofWithPublicInputs<F, C, D>,
        w_dt: Duration,
        p_dt: Duration,
    }

    let (tx_sender, tx_receiver) = std::sync::mpsc::sync_channel::<PipedProofItem>(2);

    let data_ref = &data;
    let bt_ref = &bt;
    let block_ref = &block;
    let tx_chunks_ref = &tx_chunks;

    let scope_start = Instant::now();
    std::thread::scope(|s| {
        s.spawn(move || {
            let mut all_assets = block_ref.all_assets.clone();
            let mut all_market_details = pre_exec_witness.new_market_details.clone();
            let mut system_config = block_ref.old_system_config;
            let mut register_stack = block_ref.register_stack_before;
            let mut account_tree_root = block_ref.old_account_tree_root;
            let mut account_pub_data_tree_root = block_ref.old_account_pub_data_tree_root;
            let mut market_tree_root = block_ref.old_market_tree_root;

            for (index, tx) in tx_chunks_ref.iter().enumerate() {
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

                let w_start = Instant::now();
                let pw = BlockTxCircuit::generate_witness(&block_tx, bt_ref)
                    .unwrap_or_else(|err| panic!("Failed to generate witness for tx chunk #{index}: {err:?}"));
                let w_dt = w_start.elapsed();

                let mut timing = TimingTree::new("BlockTxCircuit::prove", Level::Debug);
                let p_start = Instant::now();
                let tx_proof = plonky2::plonk::prover::prove::<F, C, D>(&data_ref.prover_only, &data_ref.common, pw, &mut timing)
                    .unwrap_or_else(|err| panic!("Failed to STARK prove tx chunk #{index}: {err:?}"));
                let p_dt = p_start.elapsed();

                let tx_witness = BlockTxWitness::from_public_inputs(&tx_proof.public_inputs.clone());
                all_assets = tx_witness.all_assets_after.clone();
                all_market_details = tx_witness.all_market_details_after.clone();
                register_stack = tx_witness.register_stack_after;
                system_config = tx_witness.new_system_config;
                account_tree_root = tx_witness.new_account_tree_root;
                account_pub_data_tree_root = tx_witness.new_account_pub_data_tree_root;
                account_delta_tree_root = tx_witness.new_account_delta_tree_root;
                market_tree_root = tx_witness.new_market_tree_root;

                if tx_sender.send(PipedProofItem { index, tx_proof, w_dt, p_dt }).is_err() {
                    break;
                }
            }
        });

        for _ in 0..chunks_count {
            let item = tx_receiver.recv().expect("Producer pipeline disconnected");
            witness_total += item.w_dt;
            stark_prove_total += item.p_dt;
            let tx_dt = item.w_dt + item.p_dt;
            tx_prove_total += tx_dt;

            info!(
                "tx chunk #{}/{} BlockTxCircuit::prove time: {:?} (witness: {:?}, prove: {:?})",
                item.index, chunks_count, tx_dt, item.w_dt, item.p_dt
            );

            let chain_dt = Instant::now();
            let chain_proof = BlockTxChainCircuit::prove(
                &chain_circuit_t,
                &chain_circuit_data,
                item.index as u64,
                &current_chain_proof,
                &dummy_proof,
                &item.tx_proof,
            ).unwrap_or_else(|err| panic!("Block Chain circuit failed to prove chunk #{}: {err:?}", item.index));
            let c_dt = chain_dt.elapsed();
            chain_prove_total += c_dt;

            info!(
                "tx chunk #{}/{} BlockTxChainCircuit::prove time: {:?}\n",
                item.index, chunks_count, c_dt
            );

            current_chain_proof = chain_proof;
        }
    });
    let scope_total = scope_start.elapsed();

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

    // ─── Telemetry Summary Export ─────────────────────────────────────
    let (rss_kb, peak_kb) = get_memory_stats_kb();
    let cpu_sec = get_cpu_seconds();
    let total_wall_sec = pre_execution_total.as_secs_f64() + scope_total.as_secs_f64();
    let total_txs = block.txs.len();
    let tps = if total_wall_sec > 0.0 {
        total_txs as f64 / total_wall_sec
    } else {
        0.0
    };

    let summary = serde_json::json!({
        "block_number": block.block_number,
        "created_at_timestamp": created_at,
        "batch_size_k": tx_per_proof,
        "total_transactions": total_txs,
        "chunks_count": chunks_count,
        "circuit_metrics": {
            "num_public_inputs_tx": data.common.num_public_inputs,
            "num_gate_constraints_tx": data.common.num_gate_constraints,
            "degree_bits_tx": data.common.degree_bits(),
            "num_public_inputs_chain": chain_circuit_data.common.num_public_inputs,
            "num_gate_constraints_chain": chain_circuit_data.common.num_gate_constraints,
            "degree_bits_chain": chain_circuit_data.common.degree_bits()
        },
        "system_telemetry": {
            "peak_rss_mb": peak_kb as f64 / 1024.0,
            "final_rss_mb": rss_kb as f64 / 1024.0,
            "total_cpu_seconds": cpu_sec,
            "effective_tps": tps,
            "avg_prove_latency_per_tx_ms": (tx_prove_total.as_secs_f64() * 1000.0) / total_txs as f64
        },
        "phase_durations_ms": {
            "pre_execution_prove": pre_execution_total.as_millis(),
            "tx_circuit_prove_total": tx_prove_total.as_millis(),
            "chain_circuit_prove_total": chain_prove_total.as_millis()
        },
        "cryptographic_phase_telemetry": {
            "pre_exec_prove_ms": pre_execution_total.as_secs_f64() * 1000.0,
            "avg_leaf_witness_gen_ms": (witness_total.as_secs_f64() * 1000.0) / chunks_count as f64,
            "avg_leaf_stark_prove_ms": (stark_prove_total.as_secs_f64() * 1000.0) / chunks_count as f64,
            "avg_chain_recursive_prove_ms": (chain_prove_total.as_secs_f64() * 1000.0) / chunks_count as f64,
            "total_witness_gen_sec": witness_total.as_secs_f64(),
            "total_stark_prove_sec": stark_prove_total.as_secs_f64(),
            "total_chain_prove_sec": chain_prove_total.as_secs_f64(),
            "total_pipelined_scope_wall_sec": scope_total.as_secs_f64()
        },
        "scraped_plonky2_stage_tree": *STAGE_LOGS.lock().unwrap()
    });

    if let Err(err) = fs::create_dir_all(&args.reports_dir) {
        eprintln!("Warning: unable to create reports dir: {:?}", err);
    } else {
        let sum_path = Path::new(&args.reports_dir).join("bench_summary.json");
        let _ = fs::write(&sum_path, serde_json::to_string_pretty(&summary).unwrap_or_default());

        let txt_path = Path::new(&args.reports_dir).join("bench_unstructured.txt");
        let txt_content = UNSTRUCTURED_LOGS.lock().unwrap().join("\n");
        let _ = fs::write(&txt_path, txt_content);

        info!("Observability observatory reports exported to {}", args.reports_dir);
    }
}

fn get_memory_stats_kb() -> (u64, u64) {
    let mut rss = 0;
    let mut peak = 0;
    if let Ok(content) = fs::read_to_string("/proc/self/status") {
        for line in content.lines() {
            if line.starts_with("VmRSS:") {
                let parts: Vec<&str> = line.split_whitespace().collect();
                if parts.len() >= 2 { rss = parts[1].parse().unwrap_or(0); }
            } else if line.starts_with("VmPeak:") {
                let parts: Vec<&str> = line.split_whitespace().collect();
                if parts.len() >= 2 { peak = parts[1].parse().unwrap_or(0); }
            }
        }
    }
    (rss, peak)
}

fn get_cpu_seconds() -> f64 {
    if let Ok(content) = fs::read_to_string("/proc/self/stat") {
        let parts: Vec<&str> = content.split_whitespace().collect();
        if parts.len() > 14 {
            let utime: f64 = parts[13].parse().unwrap_or(0.0);
            let stime: f64 = parts[14].parse().unwrap_or(0.0);
            return (utime + stime) / 100.0;
        }
    }
    0.0
}

pub fn get_test_block_json_file(file_name: &str) -> Block<F> {
    let path = Path::new(".").join(file_name);
    let data = fs::read_to_string(path).expect("Unable to read file");

    serde_json::from_str(&data).expect("JSON does not have correct format.")
}

struct NoWarnLogger(env_logger::Logger);

impl Log for NoWarnLogger {
    fn enabled(&self, metadata: &Metadata) -> bool {
        metadata.level() != Level::Warn
    }

    fn log(&self, record: &Record) {
        if record.level() == Level::Warn {
            return;
        }
        let msg = format!("[{}] {}", record.level(), record.args());
        if let Ok(mut logs) = UNSTRUCTURED_LOGS.lock() {
            logs.push(msg.clone());
        }
        if record.level() == Level::Debug || record.target().contains("plonky2") || record.target().contains("timing") || record.target().contains("circuit") {
            if let Ok(mut s) = STAGE_LOGS.lock() {
                s.push(msg);
            }
        }
        if record.level() <= Level::Info {
            self.0.log(record)
        }
    }

    fn flush(&self) {
        self.0.flush()
    }
}

fn init_logger_no_warn() {
    let env = Env::default().filter_or(DEFAULT_FILTER_ENV, "debug");
    let mut b = Builder::from_env(env);
    b.filter_level(LevelFilter::Debug);
    let inner = b.build();

    let _ = log::set_boxed_logger(Box::new(NoWarnLogger(inner)));
    log::set_max_level(LevelFilter::Debug);
}
