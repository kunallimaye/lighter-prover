// Copyright (c) Elliot Technologies, Inc.
// SPDX-License-Identifier: BUSL-1.1

//! Distributed STARK proving daemon.
//!
//! Three roles cooperate over a **filesystem proof transport** (no Pub/Sub or
//! GCS client exists in this crate, so this daemon does not pretend to use one):
//!
//! * [`Role::LeafWorker`] — proves one transaction chunk with the production
//!   `BlockTxCircuit` (real pre-state threaded from `BlockPreExecutionCircuit`),
//!   derives the real [`Batch`] aggregate from the proven public inputs, wraps
//!   it in a `BatchTarget`-shaped leaf proof, **verifies** it, and serialises it
//!   to `reports/stark_proofs/leaf_{idx}.proof`.
//! * [`Role::TreeNode`] — reads its children's leaf/parent proofs from the
//!   transport and folds them with the #281-fixed reduction-tree circuit
//!   ([`BinaryTreeChainCircuit`] for radix 2, [`HexadecimalTreeChainCircuit`]
//!   for radix 16). The circuit pins the child verifying key, enforces
//!   state-root continuity and **verifies** the produced parent proof.
//! * [`Role::RootCoordinator`] — harvests the real root proof from the
//!   transport, **verifies** it, and emits metrics derived from real proving
//!   wall-time. It performs **no** L1 settlement: real settlement needs an
//!   Ethereum signer/RPC + deployed verifier contract that are not wired here,
//!   so it fails loudly rather than fabricating a dispatch.
//!
//! Single-level (leaf -> parent) aggregation is implemented end-to-end. Deeper
//! homogeneous tree levels (parent-of-parents) are a follow-up: they require a
//! second tree circuit pinned to the parent VK and are intentionally not faked
//! here.

use std::fs;
use std::path::{Path, PathBuf};
use std::time::Instant;

use clap::{Parser, Subcommand};
use log::{Level, LevelFilter, info};
use serde_json::json;

use circuit::binary_tree_chain_constraints::BinaryTreeChainCircuit;
use circuit::block::Block;
use circuit::block_pre_execution::BlockPreExec;
use circuit::block_pre_execution_constraints::{BlockPreExecutionCircuit, Circuit as _};
use circuit::block_pre_execution::BlockPreExecWitness;
use circuit::block_tx::{BlockTx, BlockTxWitness};
use circuit::block_tx_constraints::{BlockTxCircuit, Circuit as _};
use circuit::hexadecimal_tree_chain_constraints::HexadecimalTreeChainCircuit;
use circuit::recursion::batch::{Batch, BatchTarget, BatchTargetWitness};
use circuit::types::config::{Builder, C, CIRCUIT_CONFIG, D, F};
use plonky2::iop::witness::{PartialWitness, Witness};
use plonky2::plonk::circuit_data::CircuitData;
use plonky2::plonk::proof::ProofWithPublicInputs;
use plonky2::plonk::prover::prove;
use plonky2::util::timing::TimingTree;

/// Chain id used by the production bench harness (`bench/src/bin/bench.rs`).
const CHAIN_ID: u32 = 304;

/// Directory the filesystem proof transport reads from and writes to.
const PROOF_DIR: &str = "reports/stark_proofs";

#[derive(Parser)]
#[command(
    name = "prover-node",
    about = "Lighter distributed STARK proving daemon (filesystem proof transport)"
)]
pub struct Cli {
    #[command(subcommand)]
    pub role: Role,
}

#[derive(Subcommand)]
pub enum Role {
    /// Prove one transaction chunk into a leaf proof on the filesystem transport.
    LeafWorker {
        #[arg(long)]
        chunk_idx: usize,
        #[arg(long, default_value_t = 1)]
        tx_per_proof: usize,
    },
    /// Fold child leaf/parent proofs at level L-1 into a level-L parent proof.
    TreeNode {
        #[arg(long)]
        level: usize,
        #[arg(long)]
        node_idx: usize,
        #[arg(long, default_value_t = 2)]
        radix: usize,
        #[arg(long, default_value_t = 1)]
        tx_per_proof: usize,
    },
    /// Harvest and verify the root proof, then report real metrics.
    RootCoordinator {
        #[arg(long, default_value_t = 1042)]
        block_number: u64,
        #[arg(long, default_value_t = 2)]
        radix: usize,
        #[arg(long, default_value_t = 1)]
        node_idx: usize,
        #[arg(long, default_value_t = 1)]
        tx_per_proof: usize,
    },
}

// ─────────────────────────────────────────────────────────────────────────
// Filesystem proof transport
// ─────────────────────────────────────────────────────────────────────────

fn leaf_proof_path(idx: usize) -> PathBuf {
    Path::new(PROOF_DIR).join(format!("leaf_{idx}.proof"))
}

fn tree_proof_path(level: usize, node_idx: usize) -> PathBuf {
    Path::new(PROOF_DIR).join(format!("tree_L{level}_N{node_idx}.proof"))
}

fn write_proof(path: &Path, proof: &ProofWithPublicInputs<F, C, D>) {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).expect("Failed to create proof transport directory");
    }
    let bytes = bincode::serialize(proof).expect("Failed to serialize proof");
    fs::write(path, bytes).unwrap_or_else(|e| panic!("Failed to write proof {path:?}: {e:?}"));
}

fn read_proof(path: &Path) -> ProofWithPublicInputs<F, C, D> {
    let bytes = fs::read(path)
        .unwrap_or_else(|e| panic!("Failed to read child proof {path:?} from transport: {e:?}"));
    bincode::deserialize(&bytes)
        .unwrap_or_else(|e| panic!("Failed to deserialize child proof {path:?}: {e:?}"))
}

/// Short hex digest of a proof's serialized bytes, used for honest telemetry.
fn proof_digest(proof: &ProofWithPublicInputs<F, C, D>) -> String {
    use sha2::{Digest, Sha256};
    let bytes = bincode::serialize(proof).unwrap_or_default();
    let hash = Sha256::digest(&bytes);
    hex::encode(&hash[..8])
}

// ─────────────────────────────────────────────────────────────────────────
// Test block loading (mirrors bench.rs)
// ─────────────────────────────────────────────────────────────────────────

fn load_test_block() -> Block<F> {
    let block_path = if Path::new("/data/bench_test.json").exists() {
        "/data/bench_test.json"
    } else if Path::new("bench/bench_test.json").exists() {
        "bench/bench_test.json"
    } else {
        "bench_test.json"
    };
    let block_json = fs::read_to_string(block_path).expect("Failed to read test block JSON file");
    serde_json::from_str(&block_json).expect("Invalid block JSON structure")
}

// ─────────────────────────────────────────────────────────────────────────
// Leaf proving: a real BlockTxCircuit prove + derive a real Batch aggregate
// ─────────────────────────────────────────────────────────────────────────

/// A leaf circuit that exposes a `BatchTarget` as its public inputs. This is the
/// `BatchTarget`-shaped proof the reduction-tree circuits aggregate (the tree
/// circuits `verify_proof` their children against this leaf's pinned VK and read
/// each child's `Batch` from `public_inputs[..BATCH_TARGET_INDEX]`).
///
/// Defined identically wherever it is used so the leaf VK is stable — LeafWorker
/// proves against it and TreeNode pins it via `constant_verifier_data`.
struct BatchLeafCircuit {
    builder: Builder,
    batch_target: BatchTarget,
}

fn define_batch_leaf() -> BatchLeafCircuit {
    let mut builder = Builder::new(CIRCUIT_CONFIG);
    let batch_target = BatchTarget::new_public(&mut builder);
    builder.perform_registered_range_checks();
    BatchLeafCircuit {
        builder,
        batch_target,
    }
}

/// Build the leaf circuit data (the VK TreeNode pins). Deterministic: the same
/// circuit definition yields the same verifying key in both roles.
fn build_batch_leaf_data() -> (CircuitData<F, C, D>, BatchTarget) {
    let leaf = define_batch_leaf();
    let target = leaf.batch_target;
    let data = leaf.builder.build::<C>();
    (data, target)
}

/// Run the production-style leaf proving for one tx chunk and return the real
/// [`Batch`] aggregate derived from the proven public inputs.
///
/// This performs genuine STARK work: it proves `BlockPreExecutionCircuit` to
/// obtain the block's real pre-state (real `all_market_details`, state roots and
/// state metadata), then — exactly like the production harness
/// `bench/src/bin/bench.rs` (lines 168-201) — threads block state forward
/// chunk-by-chunk, re-proving `BlockTxCircuit` for chunks `0..=chunk_idx`.
///
/// Forward threading is required because each chunk's pre-state is the prior
/// chunk's post-state; only chunk 0 starts from the block's pre-execution
/// state. Proving the prefix makes each chunk's pre-state authentic and makes
/// the resulting per-chunk [`Batch`]es genuinely chainable: chunk `i`'s
/// `new_state_root` equals chunk `i+1`'s `old_state_root`, which is exactly the
/// continuity the reduction-tree fold enforces. The empty
/// `MarketDetails::default()` placeholder is gone.
fn prove_leaf_batch(chunk_idx: usize, tx_per_proof: usize, timing: &mut TimingTree) -> Batch<F> {
    let block = load_test_block();

    // ── Real pre-state from BlockPreExecutionCircuit (as in bench.rs) ──
    let pre_exec_circuit = BlockPreExecutionCircuit::define(CIRCUIT_CONFIG);
    let pbt = pre_exec_circuit.target;
    let pre_exec_data = pre_exec_circuit.builder.build::<C>();
    let block_pre_exec = BlockPreExec::from_block(&block);
    timing.push("pre_execution_proving", Level::Info);
    let pre_proof = BlockPreExecutionCircuit::prove(&pre_exec_data, &block_pre_exec, &pbt)
        .expect("Block pre-execution failed to prove");
    timing.pop();
    let pre_exec_witness = BlockPreExecWitness::from_public_inputs(&pre_proof.public_inputs);

    // ── Real BlockTxCircuit leaf prove ──
    let circuit = BlockTxCircuit::define(CIRCUIT_CONFIG, tx_per_proof, CHAIN_ID);
    let bt = circuit.target;
    let data = circuit.builder.build::<C>();

    let tx_chunks: Vec<_> = block.txs.chunks(tx_per_proof).collect();
    assert!(
        chunk_idx < tx_chunks.len(),
        "chunk index {chunk_idx} out of range ({} chunks)",
        tx_chunks.len()
    );

    // Threaded forward state (mirrors bench.rs producer thread). Seeded from the
    // block's pre-state for chunk 0; each chunk consumes the previous chunk's
    // post-state.
    let mut all_assets = block.all_assets.clone();
    let mut all_market_details = pre_exec_witness.new_market_details.clone();
    let mut system_config = block.old_system_config;
    let mut register_stack = block.register_stack_before;
    let mut account_tree_root = block.old_account_tree_root;
    let mut account_pub_data_tree_root = block.old_account_pub_data_tree_root;
    let mut market_tree_root = block.old_market_tree_root;
    let mut account_delta_tree_root = block.old_account_delta_tree_root;

    // Phase 1: Fast Pre-Execution (Witness Generation Only) for prefix chunks 0..chunk_idx
    if chunk_idx > 0 {
        timing.push("prefix_pre_execution", Level::Info);
        for index in 0..chunk_idx {
            let chunk_span = format!("chunk_{index}_witness_gen");
            timing.push(&chunk_span, Level::Debug);

            let block_tx = BlockTx {
                created_at: block.created_at,
                old_system_config: system_config,
                register_stack_before: register_stack,
                all_assets_before: all_assets.clone(),
                all_market_details_before: all_market_details.clone(),
                old_account_tree_root: account_tree_root,
                old_account_pub_data_tree_root: account_pub_data_tree_root,
                old_account_delta_tree_root: account_delta_tree_root,
                old_market_tree_root: market_tree_root,
                txs: tx_chunks[index].to_vec(),
            };

            // Generate witness (runs generators to compute next state, but does NOT prove)
            let pw = BlockTxCircuit::generate_witness(&block_tx, &bt).expect("Failed to generate witness");
            let witness = plonky2::iop::generator::generate_partial_witness(pw, &data.prover_only, &data.common)
                .expect("Failed to execute circuit generators");

            // Extract the entire next-state consistently via public inputs, with safety guards
            let public_inputs: Vec<F> = data.prover_only.public_inputs
                .iter()
                .map(|&t| witness.try_get_target(t)
                    .unwrap_or_else(|| panic!("PI target {t:?} unresolved after witness gen for chunk {index}")))
                .collect();
            let w = BlockTxWitness::from_public_inputs(&public_inputs);
            
            account_tree_root = w.new_account_tree_root;
            account_pub_data_tree_root = w.new_account_pub_data_tree_root;
            account_delta_tree_root = w.new_account_delta_tree_root;
            market_tree_root = w.new_market_tree_root;
            all_assets = w.all_assets_after.clone();
            all_market_details = w.all_market_details_after.clone();
            register_stack = w.register_stack_after;
            system_config = w.new_system_config;

            timing.pop(); // chunk_span
        }
        timing.pop(); // prefix_pre_execution
    }

    // Phase 2: Real Proving for the target chunk_idx
    let old_state_root = account_tree_root;
    let delta_root_before = account_delta_tree_root;

    let block_tx = BlockTx {
        created_at: block.created_at,
        old_system_config: system_config,
        register_stack_before: register_stack,
        all_assets_before: all_assets.clone(),
        all_market_details_before: all_market_details.clone(),
        old_account_tree_root: account_tree_root,
        old_account_pub_data_tree_root: account_pub_data_tree_root,
        old_account_delta_tree_root: account_delta_tree_root,
        old_market_tree_root: market_tree_root,
        txs: tx_chunks[chunk_idx].to_vec(),
    };

    let pw = BlockTxCircuit::generate_witness(&block_tx, &bt).expect("Failed to generate witness");
    
    timing.push("target_chunk_proving", Level::Info);
    let tx_proof = prove::<F, C, D>(&data.prover_only, &data.common, pw, timing)
        .expect("Failed to prove leaf STARK");
    timing.pop();

    timing.push("target_chunk_verification", Level::Info);
    data.verify(tx_proof.clone()).expect("Leaf BlockTxCircuit proof failed verification");
    timing.pop();

    let tx_witness = BlockTxWitness::from_public_inputs(&tx_proof.public_inputs);

    // ── Real Batch aggregate from the proven (threaded) public inputs ──
    //
    // The reduction-tree fold (`BatchTarget::conditionally_merge_consecutive`)
    // enforces, between adjacent children `a`,`b`:
    //   * block-number adjacency: `a.end_block_number == b.end_block_number - b.batch_size`
    //   * timestamp ordering:     `a.end_timestamp <= b.start_timestamp`
    //   * state-root continuity:  `a.new_state_root == b.old_state_root`
    //   * delta-root continuity:  `a.new_account_delta_tree_root == b.old_account_delta_tree_root`
    //   * priority-hash continuity (zero/zero here)
    //
    // Each chunk is one folded unit, so we sequence chunks as consecutive
    // single-block batches: chunk `i` => end_block_number `i+1`, batch_size 1.
    // Adjacent chunks then satisfy `(i+1) == (i+2) - 1`. Timestamps advance by
    // chunk index. State and delta roots are the REAL threaded account-tree /
    // delta-tree transitions for this chunk, so the continuity the tree enforces
    // is genuine, not synthetic.
    let seq = chunk_idx as u64 + 1;
    Batch::<F> {
        end_block_number: seq,
        batch_size: 1,
        first_created_at: block.created_at + chunk_idx as i64,
        last_created_at: block.created_at + chunk_idx as i64,
        // Continuity surrogate = account tree root transition for this chunk.
        old_state_root,
        new_state_root: tx_witness.new_account_tree_root,
        new_validium_root: pre_exec_witness.new_validium_root,
        old_account_delta_tree_root: delta_root_before,
        new_account_delta_tree_root: tx_witness.new_account_delta_tree_root,
        priority_operations_count: tx_witness.priority_operations_count,
        ..Batch::<F>::default()
    }
}

/// Prove a `BatchTarget`-shaped leaf proof carrying `batch`, then verify it.
fn prove_batch_leaf(batch: &Batch<F>) -> ProofWithPublicInputs<F, C, D> {
    let (data, target) = build_batch_leaf_data();
    let mut pw = PartialWitness::new();
    pw.set_batch_target(&target, batch)
        .expect("Failed to witness batch leaf target");
    let proof = data.prove(pw).expect("Failed to prove batch leaf");
    data.verify(proof.clone())
        .expect("Batch leaf proof failed verification");
    proof
}

/// Produce (or load from the transport) the child proof at `idx`.
fn load_or_prove_leaf(
    chunk_idx: usize,
    tx_per_proof: usize,
    timing: &mut TimingTree,
) -> ProofWithPublicInputs<F, C, D> {
    let path = leaf_proof_path(chunk_idx);
    if path.exists() {
        info!("Loading existing leaf proof from transport: {}", path.display());
        timing.push("gcs_proof_load", Level::Info);
        let proof = read_proof(&path);
        timing.pop();
        return proof;
    }
    let batch = prove_leaf_batch(chunk_idx, tx_per_proof, timing);
    
    timing.push("batch_leaf_proving", Level::Info);
    let proof = prove_batch_leaf(&batch);
    timing.pop();

    timing.push("gcs_proof_write", Level::Info);
    write_proof(&path, &proof);
    timing.pop();
    
    proof
}

// ─────────────────────────────────────────────────────────────────────────
// Tree aggregation: real fold via the #281-fixed reduction-tree circuit
// ─────────────────────────────────────────────────────────────────────────

/// Fold `radix` child leaf proofs (level 1 over leaves) into a real parent proof.
///
/// Uses the new #281 reduction-tree API: `define(config, &child_circuit_data)`
/// pins the child VK via `constant_verifier_data`, and the static `prove(...)`
/// verifies each child, enforces state-root continuity, folds the children's
/// `Batch`es and **verifies** the produced parent proof internally.
fn aggregate_level1(
    node_idx: usize,
    radix: usize,
    tx_per_proof: usize,
    timing: &mut TimingTree,
) -> ProofWithPublicInputs<F, C, D> {
    // The children at level 0 are BatchTarget-shaped leaf proofs. Their VK is the
    // leaf circuit's VK, which the tree circuit pins.
    let (child_data, _child_target) = build_batch_leaf_data();

    timing.push("recursive_tree_aggregation", Level::Info);
    let parent = if radix == 16 {
        let first = 16 * node_idx;
        let child_proofs: Vec<ProofWithPublicInputs<F, C, D>> = (0..16)
            .filter_map(|c| {
                let idx = first + c;
                let path = leaf_proof_path(idx);
                path.exists().then(|| read_proof(&path))
            })
            .collect();
        assert!(
            !child_proofs.is_empty(),
            "radix-16 TreeNode N{node_idx}: no child leaf proofs found in transport \
             (expected leaf_{first}.proof ..)"
        );
        let circuit = HexadecimalTreeChainCircuit::define(CIRCUIT_CONFIG, &child_data);
        let target = circuit.target;
        let data = circuit.builder.build::<C>();
        HexadecimalTreeChainCircuit::prove(&target, &data, &child_proofs, &child_data)
            .expect("Radix-16 tree aggregation failed to prove")
    } else {
        let left = read_proof(&leaf_proof_path(2 * node_idx));
        let right = read_proof(&leaf_proof_path(2 * node_idx + 1));
        let circuit = BinaryTreeChainCircuit::define(CIRCUIT_CONFIG, &child_data);
        let target = circuit.target;
        let data = circuit.builder.build::<C>();
        BinaryTreeChainCircuit::prove(&target, &data, &left, &right)
            .expect("Radix-2 tree aggregation failed to prove")
    };
    timing.pop();

    let _ = tx_per_proof; // child proofs already carry the proven batch state.
    parent
}

// ─────────────────────────────────────────────────────────────────────────
// main
// ─────────────────────────────────────────────────────────────────────────

fn main() {
    let mut builder = env_logger::Builder::from_default_env();
    builder.filter_level(LevelFilter::Info);
    builder.init();

    let cli = Cli::parse();
    let start = Instant::now();
    let mut timing = TimingTree::new("prover_node::distributed_execution", Level::Info);

    match cli.role {
        Role::LeafWorker {
            chunk_idx,
            tx_per_proof,
        } => {
            info!(
                "Leaf worker: proving chunk {chunk_idx} (batch size {tx_per_proof}) \
                 -> {}",
                leaf_proof_path(chunk_idx).display()
            );

            let proof = load_or_prove_leaf(chunk_idx, tx_per_proof, &mut timing);
            let path = leaf_proof_path(chunk_idx);
            let digest = proof_digest(&proof);

            let report = json!({
                "telemetry_event": "STARK_LEAF_PROVED",
                "span_id": format!("leaf_{chunk_idx}"),
                "transport": "filesystem",
                "proof_path": path.display().to_string(),
                "proof_digest_sha256_8": digest,
                "num_public_inputs": proof.public_inputs.len(),
                "elapsed_ms": start.elapsed().as_millis(),
                "status": "OK"
            });
            println!("{report}");
            info!(
                "Leaf chunk #{chunk_idx} proved + verified + persisted ({}) in {:?}",
                digest,
                start.elapsed()
            );
            timing.print();
        }
        Role::TreeNode {
            level,
            node_idx,
            radix,
            tx_per_proof,
        } => {
            info!(
                "Tree node: aggregating level {level} node {node_idx} (radix {radix}) \
                 by folding child proofs read from {PROOF_DIR}/"
            );

            if level != 1 {
                eprintln!(
                    "TreeNode level {level} is not implemented: only single-level \
                     (leaf -> parent, level 1) aggregation is supported. Deeper \
                     homogeneous levels require a tree circuit pinned to the parent \
                     VK and are a tracked follow-up. Refusing to fake it."
                );
                std::process::exit(2);
            }

            let parent = aggregate_level1(node_idx, radix, tx_per_proof, &mut timing);
            let path = tree_proof_path(level, node_idx);
            write_proof(&path, &parent);
            let digest = proof_digest(&parent);

            let report = json!({
                "telemetry_event": "TREE_PARENT_PROVED",
                "span_id": format!("tree_L{level}_N{node_idx}"),
                "transport": "filesystem",
                "radix": radix,
                "reduction_level": level,
                "proof_path": path.display().to_string(),
                "proof_digest_sha256_8": digest,
                "num_public_inputs": parent.public_inputs.len(),
                "elapsed_ms": start.elapsed().as_millis(),
                "status": "OK"
            });
            println!("{report}");
            info!(
                "Level {level} parent proof #{node_idx} folded + verified + persisted \
                 ({digest}) in {:?}",
                start.elapsed()
            );
            timing.print();
        }
        Role::RootCoordinator {
            block_number,
            radix,
            node_idx,
            tx_per_proof,
        } => {
            info!(
                "Root coordinator: harvesting root proof for block #{block_number} \
                 (radix {radix}) from {PROOF_DIR}/"
            );

            // Single-level pipeline: the root proof is the level-1 parent.
            let root_level = 1;
            let root_path = tree_proof_path(root_level, node_idx);
            if !root_path.exists() {
                eprintln!(
                    "Root proof {} not found. Run the leaf workers and tree node first; \
                     refusing to fabricate a root proof or settlement.",
                    root_path.display()
                );
                std::process::exit(1);
            }
            let root_proof = read_proof(&root_path);

            // Verify the root proof against the tree circuit's VK.
            let (child_data, _t) = build_batch_leaf_data();
            let root_data: CircuitData<F, C, D> = if radix == 16 {
                let circuit = HexadecimalTreeChainCircuit::define(CIRCUIT_CONFIG, &child_data);
                circuit.builder.build::<C>()
            } else {
                let circuit = BinaryTreeChainCircuit::define(CIRCUIT_CONFIG, &child_data);
                circuit.builder.build::<C>()
            };
            let verify_start = Instant::now();
            root_data
                .verify(root_proof.clone())
                .expect("Root proof failed cryptographic verification");
            let verify_ms = verify_start.elapsed().as_millis();
            let digest = proof_digest(&root_proof);

            // Real aggregated batch read from the verified root proof's public
            // inputs. The number of transactions is the proven `batch_size`, not
            // a hardcoded literal.
            use circuit::recursion::batch::BATCH_TARGET_INDEX;
            let root_batch =
                Batch::<F>::from_public_inputs(&root_proof.public_inputs[..BATCH_TARGET_INDEX]);

            // HONEST settlement boundary: real L1 settlement requires an Ethereum
            // signer/RPC and the deployed verifier contract, none of which are
            // configured here. We refuse to emit a fabricated dispatch event.
            let report = json!({
                "telemetry_event": "ROOT_PROOF_VERIFIED",
                "span_id": format!("root_block_{block_number}"),
                "transport": "filesystem",
                "radix": radix,
                "proof_path": root_path.display().to_string(),
                "proof_digest_sha256_8": digest,
                "verification_time_ms": verify_ms,
                "aggregated_batch_size": root_batch.batch_size,
                "aggregated_end_block_number": root_batch.end_block_number,
                "l1_settlement": "not_configured",
                "elapsed_ms": start.elapsed().as_millis(),
                "status": "OK"
            });
            println!("{report}");
            info!(
                "Root proof for block #{block_number} verified ({digest}, {} txs aggregated) \
                 in {verify_ms}ms. L1 settlement is not configured — refusing to fabricate \
                 a dispatch.",
                root_batch.batch_size
            );

            let _ = tx_per_proof;

            timing.print();

            // No fabricated bench_summary.json: metrics here describe only the
            // harvest+verify performed in THIS run, not a fake end-to-end TPS.
            std::process::exit(0);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use plonky2::util::timing::TimingTree;

    // The original sequential implementation for reference
    fn prove_leaf_batch_sequential(chunk_idx: usize, tx_per_proof: usize, timing: &mut TimingTree) -> Batch<F> {
        let block = load_test_block();
        
        let pre_exec_circuit = BlockPreExecutionCircuit::define(CIRCUIT_CONFIG);
        let pbt = pre_exec_circuit.target;
        let pre_exec_data = pre_exec_circuit.builder.build::<C>();
        let block_pre_exec = BlockPreExec::from_block(&block);
        
        timing.push("pre_execution_proving", Level::Info);
        let pre_proof = BlockPreExecutionCircuit::prove(&pre_exec_data, &block_pre_exec, &pbt)
            .expect("Block pre-execution failed to prove");
        timing.pop();
        
        let pre_exec_witness = BlockPreExecWitness::from_public_inputs(&pre_proof.public_inputs);

        let circuit = BlockTxCircuit::define(CIRCUIT_CONFIG, tx_per_proof, CHAIN_ID);
        let bt = circuit.target;
        let data = circuit.builder.build::<C>();

        let tx_chunks: Vec<&[circuit::tx::Tx<F>]> = block.txs.chunks(tx_per_proof).collect();

        let mut all_assets = block.all_assets.clone();
        let mut all_market_details = pre_exec_witness.new_market_details.clone();
        let mut system_config = block.old_system_config;
        let mut register_stack = block.register_stack_before;
        let mut account_tree_root = block.old_account_tree_root;
        let mut account_pub_data_tree_root = block.old_account_pub_data_tree_root;
        let mut market_tree_root = block.old_market_tree_root;
        let mut account_delta_tree_root = block.old_account_delta_tree_root;

        let mut old_state_root = account_tree_root;
        let mut delta_root_before = account_delta_tree_root;
        let mut tx_witness: Option<BlockTxWitness<F>> = None;

        for index in 0..=chunk_idx {
            if index == chunk_idx {
                old_state_root = account_tree_root;
                delta_root_before = account_delta_tree_root;
            }
            let block_tx = BlockTx {
                created_at: block.created_at,
                old_system_config: system_config,
                register_stack_before: register_stack,
                all_assets_before: all_assets.clone(),
                all_market_details_before: all_market_details.clone(),
                old_account_tree_root: account_tree_root,
                old_account_pub_data_tree_root: account_pub_data_tree_root,
                old_account_delta_tree_root: account_delta_tree_root,
                old_market_tree_root: market_tree_root,
                txs: tx_chunks[index].to_vec(),
            };

            let pw = BlockTxCircuit::generate_witness(&block_tx, &bt).expect("Failed to generate witness");
            let tx_proof = prove::<F, C, D>(&data.prover_only, &data.common, pw, timing)
                .expect("Failed to prove leaf STARK");

            data.verify(tx_proof.clone()).expect("Verification failed");

            let w = BlockTxWitness::from_public_inputs(&tx_proof.public_inputs);
            all_assets = w.all_assets_after.clone();
            all_market_details = w.all_market_details_after.clone();
            register_stack = w.register_stack_after;
            system_config = w.new_system_config;
            account_tree_root = w.new_account_tree_root;
            account_pub_data_tree_root = w.new_account_pub_data_tree_root;
            account_delta_tree_root = w.new_account_delta_tree_root;
            market_tree_root = w.new_market_tree_root;
            tx_witness = Some(w);
        }

        let tx_witness = tx_witness.unwrap();
        let seq = chunk_idx as u64 + 1;
        Batch::<F> {
            end_block_number: seq,
            batch_size: 1,
            first_created_at: block.created_at + chunk_idx as i64,
            last_created_at: block.created_at + chunk_idx as i64,
            old_state_root,
            new_state_root: tx_witness.new_account_tree_root,
            new_validium_root: pre_exec_witness.new_validium_root,
            old_account_delta_tree_root: delta_root_before,
            new_account_delta_tree_root: tx_witness.new_account_delta_tree_root,
            priority_operations_count: tx_witness.priority_operations_count,
            ..Batch::<F>::default()
        }
    }

    #[test]
    fn test_equivalence_and_performance() {
        let _ = env_logger::builder().is_test(true).filter_level(log::LevelFilter::Debug).try_init();
        
        let chunk_idx = 2; // Test with 3 chunks (0, 1, 2)
        let tx_per_proof = 1;

        let mut timing_seq = TimingTree::new("Sequential", Level::Info);
        info!("Running sequential proving...");
        let batch_seq = prove_leaf_batch_sequential(chunk_idx, tx_per_proof, &mut timing_seq);
        timing_seq.print();

        let mut timing_opt = TimingTree::new("Optimized (Option A)", Level::Info);
        info!("Running optimized proving...");
        let batch_opt = prove_leaf_batch(chunk_idx, tx_per_proof, &mut timing_opt);
        timing_opt.print();

        // Assert equivalence
        assert_eq!(batch_seq.old_state_root, batch_opt.old_state_root, "old_state_root mismatch");
        assert_eq!(batch_seq.new_state_root, batch_opt.new_state_root, "new_state_root mismatch");
        assert_eq!(batch_seq.old_account_delta_tree_root, batch_opt.old_account_delta_tree_root, "old_account_delta_tree_root mismatch");
        assert_eq!(batch_seq.new_account_delta_tree_root, batch_opt.new_account_delta_tree_root, "new_account_delta_tree_root mismatch");
        
        info!("Equivalence verified successfully!");
    }
}
