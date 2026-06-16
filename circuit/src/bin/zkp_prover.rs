// Copyright (c) Elliot Technologies, Inc.
// SPDX-License-Identifier: BUSL-1.1

#![allow(unused_imports)]

use std::fs;
use std::path::Path;
use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use circuit::block::{Block, BlockWitness};
use circuit::block_constraints::{BlockCircuit, Circuit as _};
use circuit::block_pre_execution::{BlockPreExec, BlockPreExecWitness};
use circuit::block_pre_execution_constraints::{BlockPreExecutionCircuit, Circuit as _};
use circuit::block_tx::{BlockTx, BlockTxWitness};
use circuit::block_tx_chain::BlockTxChainWitness;
use circuit::block_tx_chain_constraints::{BlockTxChainCircuit, Circuit as _};
use circuit::block_tx_constraints::{BlockTxCircuit, BlockTxTarget, Circuit as _};
use circuit::builder::custom::cyclic_base_proof;
use circuit::types::config::{C, CIRCUIT_CONFIG, F};
use circuit::types::constants::*;
use circuit::types::state_metadata::StateMetadata;
use env_logger::{Builder, DEFAULT_FILTER_ENV, Env, try_init_from_env};
use log::{Level, LevelFilter, Log, Metadata, Record, debug, info};
use plonky2::plonk::proof::CompressedProofWithPublicInputs;
use plonky2::recursion::dummy_circuit::dummy_circuit;

const TX_PER_PROOF: usize = 4;
const CHAIN_ID: u32 = 304;

fn main() -> Result<()> {
    let _ = try_init_from_env(Env::default().filter_or(DEFAULT_FILTER_ENV, "info"));

    let input_path = std::env::args().nth(1)
        .or_else(|| std::env::var("BLOCK_JSON_PATH").ok())
        .unwrap_or_else(|| "bench_test.json".to_string());

    let output_path = std::env::args().nth(2)
        .or_else(|| std::env::var("PROOF_OUTPUT_PATH").ok())
        .unwrap_or_else(|| "proof_out.json".to_string());

    info!("Starting Lighter ZKP Prover (Layer 1 STARK Proving Engine)...");
    info!("  Input Block Path:  {}", input_path);
    info!("  Output Proof Path: {}", output_path);

    let data_str = fs::read_to_string(&input_path)
        .with_context(|| format!("Failed to read input block JSON fixture at {}", input_path))?;
    let block: Block<F> = serde_json::from_str(&data_str)
        .with_context(|| format!("Failed to deserialize JSON into Block<F> from {}", input_path))?;

    let tx_chunks = block.txs.chunks(TX_PER_PROOF);
    let chunks_count = tx_chunks.len();

    info!(
        "Ingested block #{} ({} txs across {} batch segments)...",
        block.block_number,
        block.txs.len(),
        chunks_count
    );

    let circuit = BlockTxCircuit::define(CIRCUIT_CONFIG, TX_PER_PROOF, CHAIN_ID);
    let bt = circuit.target;
    let data = circuit.builder.build::<C>();

    let pre_exec_circuit = BlockPreExecutionCircuit::define(CIRCUIT_CONFIG);
    let pbt = pre_exec_circuit.target;
    let pre_exec_data = pre_exec_circuit.builder.build::<C>();

    let chain_circuit = BlockTxChainCircuit::define(CIRCUIT_CONFIG, &data, TX_PER_PROOF, 1);
    let chain_circuit_t = chain_circuit.target;
    let chain_circuit_data = chain_circuit.builder.build::<C>();

    let dummy_tx_chain_circuit = dummy_circuit(&chain_circuit_data.common);

    let dummy_proof = cyclic_base_proof(
        &chain_circuit_data.common,
        &chain_circuit_data.verifier_only,
        &dummy_tx_chain_circuit,
        Vec::<F>::new().iter().copied().enumerate().collect(),
    ).expect("Dummy cyclic base proof setup failed");

    let block_pre_exec = BlockPreExec::from_block(&block);

    let pre_execution_time = Instant::now();
    let pre_proof = BlockPreExecutionCircuit::prove(&pre_exec_data, &block_pre_exec, &pbt)
        .map_err(|e| anyhow::anyhow!("Block pre-execution STARK proof failed: {:?}", e))?;
    let pre_execution_total = pre_execution_time.elapsed();
    info!("BlockPreExecutionCircuit proved successfully in {:?}", pre_execution_total);

    let pre_exec_witness = BlockPreExecWitness::from_public_inputs(&pre_proof.clone().public_inputs);

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
        let tx_proof = BlockTxCircuit::prove(&data, &block_tx, &bt)
            .map_err(|e| anyhow::anyhow!("Failed to prove tx segment #{}: {:?}", index + 1, e))?;
        let tx_dt = tx_dt.elapsed();
        tx_prove_total += tx_dt;
        info!("  Tx batch segment #{}/{} proved in {:?}", index + 1, chunks_count, tx_dt);

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
        ).map_err(|e| anyhow::anyhow!("Recursive STARK folding failed on segment #{}: {:?}", index + 1, e))?;
        let chain_dt = chain_dt.elapsed();
        chain_prove_total += chain_dt;
        info!("  Recursive STARK folding #{}/{} completed in {:?}", index + 1, chunks_count, chain_dt);

        current_chain_proof = chain_proof;
    }

    info!("End-to-end block STARK proving completed successfully!");
    info!("  PreExecution Proving: {:?}", pre_execution_total);
    info!("  Total Tx Proving:     {:?}", tx_prove_total);
    info!("  Total Chain Folding:  {:?}", chain_prove_total);

    let serialized_proof = serde_json::to_string_pretty(&current_chain_proof)
        .context("Failed to serialize final recursive STARK proof to JSON")?;
    fs::write(&output_path, serialized_proof)
        .with_context(|| format!("Failed to persist proof JSON to {}", output_path))?;

    info!("Successfully persisted verified end-to-end STARK proof to {}", output_path);
    Ok(())
}
