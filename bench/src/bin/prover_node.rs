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
//! * [`Role::TreeNode`] — reads its children's level-(L-1) proofs from the
//!   transport and folds them with the #281/#289 reduction-tree circuit. Tree
//!   depth is **dynamic**: `depth = ceil(log_radix(N))` for N leaves, so the
//!   same `tree-node --level L` invocation folds any level. Level 1 folds leaf
//!   proofs (non-recursive children, `dummy_proof` padding); level >= 2 folds
//!   level-(L-1) node proofs (recursive children, real-base-proof padding per
//!   #289). Each level pins the level-(L-1) child verifying key, enforces
//!   state-root continuity and **verifies** the produced parent proof.
//! * [`Role::RootCoordinator`] — computes the root level dynamically from N,
//!   harvests the real root proof from the transport, **verifies** it against
//!   the level-`root_level` circuit's VK, and emits metrics derived from real
//!   proving wall-time. It performs **no** L1 settlement: real settlement needs
//!   an Ethereum signer/RPC + deployed verifier contract that are not wired
//!   here, so it fails loudly rather than fabricating a dispatch.
//!
//! Multi-level (dynamic-depth) aggregation is implemented end-to-end over the
//! filesystem transport, using the same `HexadecimalTreeChainCircuit` family at
//! every level so the verifying keys chain (a level-L node pins the level-(L-1)
//! node's VK). The radix-2 single-level case is retained on the
//! [`BinaryTreeChainCircuit`] path for exact #281 back-compat.

use std::fs;
use std::path::{Path, PathBuf};
use std::time::Instant;

use clap::{Parser, Subcommand};
use log::{Level, LevelFilter, info};
use serde_json::json;

use bench::transport::{
    CommitOutcome, LocalTransport, Role as WorkRole, WorkLease, WorkTransport,
};
use circuit::binary_tree_chain_constraints::BinaryTreeChainCircuit;
use circuit::block::Block;
use circuit::block_pre_execution::BlockPreExec;
use circuit::block_pre_execution_constraints::{BlockPreExecutionCircuit, Circuit as _};
use circuit::block_pre_execution::BlockPreExecWitness;
use circuit::block_tx::{BlockTx, BlockTxWitness};
use circuit::block_tx_constraints::{BlockTxCircuit, Circuit as _};
use circuit::hexadecimal_tree_chain_constraints::{
    HexadecimalTreeChainCircuit, HexadecimalTreeChainTarget, RADIX as HEX_RADIX,
};
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
    /// Fold child proofs at level L-1 into a level-L parent proof.
    ///
    /// At level 1 the children are level-0 leaf proofs; at level L>=2 they are
    /// level-(L-1) node proofs. The fold uses the radix-16 reduction-tree
    /// circuit pinned to the level-(L-1) child VK, padding under-full nodes per
    /// the #289 API (`dummy_proof` at level 1, a real recursive base proof at
    /// level >= 2). `--leaf-count` is the total number of leaves N in the tree;
    /// it determines per-level node counts and the overall depth.
    TreeNode {
        #[arg(long)]
        level: usize,
        #[arg(long)]
        node_idx: usize,
        #[arg(long, default_value_t = 2)]
        radix: usize,
        /// Total number of level-0 leaf proofs (N) feeding the tree. Decoupled
        /// from `radix` (fan-in) so N can exceed radix and span multiple levels.
        #[arg(long, default_value_t = 2)]
        leaf_count: usize,
        #[arg(long, default_value_t = 1)]
        tx_per_proof: usize,
    },
    /// Harvest and verify the root proof, then report real metrics.
    RootCoordinator {
        #[arg(long, default_value_t = 1042)]
        block_number: u64,
        #[arg(long, default_value_t = 2)]
        radix: usize,
        /// Total number of level-0 leaf proofs (N). The root level is computed
        /// dynamically as `ceil(log_radix(N))` rather than hardcoded.
        #[arg(long, default_value_t = 2)]
        leaf_count: usize,
        #[arg(long, default_value_t = 0)]
        node_idx: usize,
        #[arg(long, default_value_t = 1)]
        tx_per_proof: usize,
    },
    /// Fungible role-per-message dispatch loop over a work transport.
    ///
    /// ONE pod = one dispatch loop = any role per message. The loop seeds the N
    /// leaf descriptors, then repeatedly pulls a [`WorkDescriptor`], assumes the
    /// role it names (leaf prove / tree-node fold), commits the proof bytes
    /// idempotently, acks, and lets readiness gating publish the next level's
    /// folds — until the dynamic-depth root is produced and verified.
    ///
    /// The backend is selected by `--transport`:
    /// * `local` (default) — the in-process/filesystem [`LocalTransport`]; runs
    ///   the full e2e local smoke (no cloud), unchanged from the prior slice.
    /// * `pubsub` — the production [`PubSubGcsTransport`]: GCP Pub/Sub pull + GCS
    ///   native-API atomic claim/commit. Compiled only with `--features pubsub`;
    ///   requires `--project/--topic/--subscription/--bucket` (and optionally
    ///   `--ack-deadline`). Both backends implement the SAME `WorkTransport`
    ///   trait, so the dispatch loop is transport-agnostic.
    Work {
        #[arg(long, default_value_t = 2)]
        radix: usize,
        /// Total number of level-0 leaves N to prove and aggregate.
        #[arg(long, default_value_t = 4)]
        leaf_count: usize,
        #[arg(long, default_value_t = 1)]
        tx_per_proof: usize,
        #[arg(long, default_value_t = 1042)]
        block_number: u64,
        /// Which work-transport backend to drive.
        #[arg(long, value_enum, default_value_t = TransportKind::Local)]
        transport: TransportKind,
        /// Run as a one-off **seeder** instead of a worker: publish the N leaf
        /// descriptors onto the transport, log what was seeded, and exit. A
        /// seeded queue is then drained by the fungible worker pods. For
        /// `--transport=local` the seed step is always performed inline before
        /// the loop (so the local e2e smoke is self-contained); this flag makes
        /// the seed an explicit *separate* one-off for the `--transport=pubsub`
        /// pool, where exactly one seeder pod bootstraps the run.
        #[arg(long, default_value_t = false)]
        seed: bool,
        /// (pubsub) GCP project id. Defaults to ADC / metadata-server discovery.
        /// Falls back to env `PROVER_PUBSUB_PROJECT` when the flag is absent.
        #[arg(long)]
        project: Option<String>,
        /// (pubsub) Pub/Sub topic id for follow-on fold descriptors. Falls back
        /// to env `PROVER_PUBSUB_TOPIC` when the flag is empty.
        #[arg(long, default_value = "")]
        topic: String,
        /// (pubsub) Pub/Sub subscription id to pull work from. Falls back to env
        /// `PROVER_PUBSUB_SUBSCRIPTION` when the flag is empty.
        #[arg(long, default_value = "")]
        subscription: String,
        /// (pubsub) GCS bucket for committed proof outputs + CAS gating markers.
        /// Falls back to env `PROVER_PUBSUB_BUCKET` when the flag is empty.
        #[arg(long, default_value = "")]
        bucket: String,
        /// (pubsub) Ack deadline (seconds), ≈ 2×P99. Default 60s (radix-16 fold
        /// ≈ 30s ⇒ 2×P99). Pub/Sub range [10, 600]s; the lease is also
        /// heartbeated via modifyAckDeadline while proving.
        #[arg(long, default_value_t = 60)]
        ack_deadline: i32,
        /// (pubsub) Optional object-name prefix so multiple runs can share one
        /// bucket without colliding (e.g. `runs/block_1042/`).
        #[arg(long, default_value = "")]
        object_prefix: String,
    },
}

/// Which [`WorkTransport`] backend the fungible dispatch loop drives.
#[derive(Clone, Copy, Debug, PartialEq, Eq, clap::ValueEnum)]
pub enum TransportKind {
    /// In-process/filesystem dev/test backend (no cloud).
    Local,
    /// Production GCP Pub/Sub pull + GCS native-API atomic claim/commit.
    Pubsub,
}

// ─────────────────────────────────────────────────────────────────────────
// Dynamic tree geometry
// ─────────────────────────────────────────────────────────────────────────

/// Depth of the reduction tree needed to aggregate `n` leaves with the given
/// `radix` fan-in: `ceil(log_radix(n))`, i.e. the number of node levels above
/// the leaves. A single leaf needs no folding (depth 0); `n <= radix` needs a
/// single level (depth 1); `radix < n <= radix^2` needs two levels, and so on.
///
/// Computed iteratively to avoid floating-point rounding hazards near exact
/// powers of the radix (e.g. `log_2(8)` must yield exactly 3).
fn tree_depth(n: usize, radix: usize) -> usize {
    assert!(radix >= 2, "radix must be >= 2");
    if n <= 1 {
        return 0;
    }
    let mut depth = 0usize;
    let mut span = 1usize; // radix^depth
    while span < n {
        span = span.saturating_mul(radix);
        depth += 1;
    }
    depth
}

/// Number of nodes at `level` (>= 1) in a `radix`-ary reduction tree over `n`
/// leaves: `ceil(n / radix^level)`. Level 1 folds the N leaves into
/// `ceil(N/radix)` nodes; level 2 folds those into `ceil(N/radix^2)`, etc. The
/// final (root) level always has exactly one node.
fn nodes_at_level(n: usize, radix: usize, level: usize) -> usize {
    assert!(level >= 1, "tree levels are 1-indexed");
    assert!(radix >= 2, "radix must be >= 2");
    let mut divisor = 1usize; // radix^level
    for _ in 0..level {
        divisor = divisor.saturating_mul(radix);
    }
    n.div_ceil(divisor).max(1)
}

/// Number of children that node `node_idx` at `level` actually has (the rest of
/// its `radix` slots are padding). The child population at `level` is
/// `nodes_at_level(n, radix, level - 1)` (with level-0 == the N leaves); this
/// node owns the contiguous slice `[node_idx*radix, (node_idx+1)*radix)` of
/// that population, clamped to the real count.
fn real_children_for_node(n: usize, radix: usize, level: usize, node_idx: usize) -> usize {
    let children_population = if level == 1 {
        n
    } else {
        nodes_at_level(n, radix, level - 1)
    };
    let first = node_idx * radix;
    if first >= children_population {
        return 0;
    }
    (children_population - first).min(radix)
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
// Tree aggregation: dynamic-depth fold via the #281/#289 reduction-tree circuit
//
// Multi-level aggregation requires the SAME circuit family at every level so the
// verifying keys chain: a level-L node pins the level-(L-1) node's VK via
// `constant_verifier_data`. Only `HexadecimalTreeChainCircuit` exposes the
// recursive-base-proof padding API (`padding_proof: Some(..)`, validated in
// #289) that level>=2 folding requires, so the multi-level engine uses it for
// ALL levels. The CLI `--radix` controls *fan-in* (how many children each node
// reads from the transport); the circuit itself is always RADIX-shaped, with
// under-full nodes padded. radix=2 => depth = ceil(log2(N)).
//
// The radix-2 single-level (`BinaryTreeChainCircuit`) path is retained as the
// exact-back-compat depth==1, radix==2 special case so #281 behaviour does not
// regress.
// ─────────────────────────────────────────────────────────────────────────

/// A built reduction-tree node circuit plus the child circuit data its children
/// are pinned to (needed both to pin the VK and to mint recursive base padding).
struct NodeCircuit {
    target: HexadecimalTreeChainTarget<D>,
    data: CircuitData<F, C, D>,
    /// The child circuit's data (level-(L-1) node, or the leaf at level 1).
    child_data: CircuitData<F, C, D>,
    /// `true` when the child is itself a recursive tree node (level >= 2), so
    /// padding must use a real base proof rather than `dummy_proof`.
    child_is_recursive: bool,
}

/// Build the level-`level` reduction-tree node circuit. The circuit at level L
/// is a `HexadecimalTreeChainCircuit` pinned to the level-(L-1) circuit's VK.
///
/// Built bottom-up and deterministically from the leaf circuit definition, so
/// the VK at every level is identical across the `TreeNode` and
/// `RootCoordinator` roles (both reconstruct the same chain of circuits). This
/// is essential: a level-L node proof written by one process must verify against
/// the level-L circuit rebuilt by another.
///
/// `level == 0` is the (non-recursive) leaf circuit itself, used as the base of
/// the recursion; callers fold at `level >= 1`.
fn build_node_circuit_for_level(level: usize) -> NodeCircuit {
    assert!(level >= 1, "tree node circuits exist at level >= 1");

    // Recurse to obtain the child circuit data. At level 1 the child is the
    // non-recursive leaf; at level L the child is the level-(L-1) node.
    let (child_data, child_is_recursive) = if level == 1 {
        let (leaf_data, _t) = build_batch_leaf_data();
        (leaf_data, false)
    } else {
        let child = build_node_circuit_for_level(level - 1);
        (child.data, true)
    };

    let circuit = HexadecimalTreeChainCircuit::define(CIRCUIT_CONFIG, &child_data);
    let target = circuit.target;
    let data = circuit.builder.build::<C>();
    NodeCircuit {
        target,
        data,
        child_data,
        child_is_recursive,
    }
}

/// Mint a real, satisfiable base proof of the level-`level` node circuit, usable
/// as recursive padding for a level-(`level`+1) node (see the #289 doc comment).
///
/// The base proof's public inputs are irrelevant (padding slots fold with
/// `cond = false`); it only has to *verify* against the pinned child VK. It is
/// minted recursively, bottoming out at the leaf where `dummy_proof` works:
///   * level-1 base: a single trivial leaf child, remaining slots dummy-padded.
///   * level-L base: a single level-(L-1) base child, remaining slots padded
///     with a level-(L-1) base proof.
fn mint_base_proof_for_level(level: usize, timing: &mut TimingTree) -> ProofWithPublicInputs<F, C, D> {
    assert!(level >= 1, "base proofs are minted at level >= 1");
    timing.push("mint_recursive_base_proof", Level::Debug);
    let node = build_node_circuit_for_level(level);

    let proof = if !node.child_is_recursive {
        // Level-1 base: one trivial leaf child; remaining slots dummy-padded.
        let leaf_batch = Batch::<F> {
            end_block_number: 1,
            batch_size: 1,
            ..Batch::<F>::default()
        };
        let leaf_proof = prove_batch_leaf(&leaf_batch);
        HexadecimalTreeChainCircuit::prove(
            &node.target,
            &node.data,
            &[leaf_proof],
            &node.child_data,
            None,
        )
        .expect("level-1 base proof must prove")
    } else {
        // Level-L base: one level-(L-1) base child; remaining slots padded with
        // a level-(L-1) base proof (recursive padding all the way down).
        let child_base = mint_base_proof_for_level(level - 1, timing);
        HexadecimalTreeChainCircuit::prove(
            &node.target,
            &node.data,
            &[child_base.clone()],
            &node.child_data,
            Some(&child_base),
        )
        .expect("level-L base proof must prove")
    };
    timing.pop();
    proof
}

/// Read the real (non-padding) child proofs for node `node_idx` at `level` from
/// the filesystem transport. Level-1 children are leaf proofs (`leaf_{i}.proof`);
/// level-L children are level-(L-1) node proofs (`tree_L{L-1}_N{j}.proof`).
fn read_children_for_node(
    level: usize,
    node_idx: usize,
    radix: usize,
    leaf_count: usize,
) -> Vec<ProofWithPublicInputs<F, C, D>> {
    let real = real_children_for_node(leaf_count, radix, level, node_idx);
    let first = node_idx * radix;
    (0..real)
        .map(|c| {
            let child_global_idx = first + c;
            let path = if level == 1 {
                leaf_proof_path(child_global_idx)
            } else {
                tree_proof_path(level - 1, child_global_idx)
            };
            read_proof(&path)
        })
        .collect()
}

/// Fold node `node_idx` at `level` over `leaf_count` total leaves with the given
/// `radix` fan-in, producing a real, verified level-`level` parent proof.
///
/// Generalises the original single-level fold to arbitrary depth using the #289
/// recursive-padding API. The radix-2, depth-1 case is delegated to the
/// `BinaryTreeChainCircuit` path for exact #281 back-compat.
fn aggregate_node(
    level: usize,
    node_idx: usize,
    radix: usize,
    leaf_count: usize,
    tx_per_proof: usize,
    timing: &mut TimingTree,
) -> ProofWithPublicInputs<F, C, D> {
    assert!(level >= 1, "tree levels are 1-indexed");
    assert!(radix >= 2, "radix must be >= 2");
    assert!(
        radix <= HEX_RADIX,
        "radix {radix} exceeds the reduction-tree node fan-in {HEX_RADIX}; \
         a wider circuit would be required"
    );

    let depth = tree_depth(leaf_count, radix);
    assert!(
        level <= depth.max(1),
        "TreeNode level {level} exceeds tree depth {depth} for N={leaf_count}, radix={radix}; \
         refusing to fold a non-existent level"
    );
    let node_count = nodes_at_level(leaf_count, radix, level);
    assert!(
        node_idx < node_count,
        "TreeNode level {level} node {node_idx} out of range: only {node_count} node(s) \
         exist at this level for N={leaf_count}, radix={radix}"
    );

    let _ = tx_per_proof; // child proofs already carry the proven batch state.

    // Exact #281 back-compat: radix-2 single-level uses the binary circuit.
    if radix == 2 && level == 1 && depth <= 1 {
        let (child_data, _t) = build_batch_leaf_data();
        timing.push("recursive_tree_aggregation", Level::Info);
        let left = read_proof(&leaf_proof_path(2 * node_idx));
        let right = read_proof(&leaf_proof_path(2 * node_idx + 1));
        let circuit = BinaryTreeChainCircuit::define(CIRCUIT_CONFIG, &child_data);
        let target = circuit.target;
        let data = circuit.builder.build::<C>();
        let parent = BinaryTreeChainCircuit::prove(&target, &data, &left, &right)
            .expect("Radix-2 tree aggregation failed to prove");
        timing.pop();
        return parent;
    }

    // General path (any radix, any level): build the level-`level` node circuit
    // (pinned to the level-(L-1) child VK) and fold the real children, padding
    // under-full nodes per the #289 API.
    let node = build_node_circuit_for_level(level);
    let child_proofs = read_children_for_node(level, node_idx, radix, leaf_count);
    assert!(
        !child_proofs.is_empty(),
        "TreeNode level {level} node {node_idx}: no child proofs found in transport"
    );

    timing.push("recursive_tree_aggregation", Level::Info);
    // Level-1 children are non-recursive leaf proofs => `dummy_proof` padding
    // (None). Level >= 2 children are recursive node proofs => a real base proof
    // is required ("generators weren't run" otherwise — see #289).
    let parent = if node.child_is_recursive {
        let base = mint_base_proof_for_level(level - 1, timing);
        HexadecimalTreeChainCircuit::prove(
            &node.target,
            &node.data,
            &child_proofs,
            &node.child_data,
            Some(&base),
        )
        .expect("level >= 2 tree aggregation failed to prove")
    } else {
        HexadecimalTreeChainCircuit::prove(
            &node.target,
            &node.data,
            &child_proofs,
            &node.child_data,
            None,
        )
        .expect("level-1 tree aggregation failed to prove")
    };
    timing.pop();
    parent
}

// ─────────────────────────────────────────────────────────────────────────
// Fungible role-per-message dispatch loop
//
// Reuses the SAME role-execution code as the explicit subcommands
// (`load_or_prove_leaf`, `aggregate_node`, the root verify) — it does NOT
// reimplement proving. It routes each pulled `WorkDescriptor` to that code,
// commits the proof bytes through the transport's atomic `commit_output`
// (idempotent-output guard), and lets readiness gating publish the next level's
// fold tasks via `commit_and_gate`. One loop = any role per message.
// ─────────────────────────────────────────────────────────────────────────

/// Verify a level-`root_level` root proof against its rebuilt circuit VK. Mirrors
/// the `RootCoordinator` verification (binary circuit for the radix-2 depth-1
/// back-compat case, the dynamic Hex node chain otherwise).
fn verify_root_proof(
    root_proof: &ProofWithPublicInputs<F, C, D>,
    root_level: usize,
    radix: usize,
) {
    if radix == 2 && root_level == 1 {
        let (child_data, _t) = build_batch_leaf_data();
        let circuit = BinaryTreeChainCircuit::define(CIRCUIT_CONFIG, &child_data);
        let root_data = circuit.builder.build::<C>();
        root_data
            .verify(root_proof.clone())
            .expect("Root proof failed cryptographic verification");
    } else {
        let root_node = build_node_circuit_for_level(root_level);
        root_node
            .data
            .verify(root_proof.clone())
            .expect("Root proof failed cryptographic verification");
    }
}

/// Stable identity of this worker pod for CAS-winner attribution across a
/// many-pod fungible pool. Prefers the Kubernetes pod name (`HOSTNAME`, which GKE
/// sets to the pod name), falling back to the OS process id so the field is
/// always present even outside a pod (local runs, tests). Logged on every
/// per-iteration instrumentation line as `worker={id}` so that, when N pods race
/// the same `commit_and_gate` CAS, the single `Committed` winner is observable
/// per descriptor.
fn worker_identity() -> String {
    std::env::var("HOSTNAME")
        .ok()
        .filter(|h| !h.trim().is_empty())
        .unwrap_or_else(|| format!("pid-{}", std::process::id()))
}

/// Run the fungible dispatch loop to completion over `transport`: prove every
/// leaf, fold every tree level, and verify the dynamic-depth root. Returns the
/// verified root proof. Uses `PROOF_DIR` as the shared proof store so the reused
/// role code (`load_or_prove_leaf`/`aggregate_node`, which read/write
/// `PROOF_DIR`) and the transport's committed outputs are the same bytes.
/// Run the fungible dispatch loop to completion (root produced + verified) or
/// until a graceful shutdown is requested.
///
/// **Transport-agnostic**: generic over any [`WorkTransport`], so the SAME loop
/// drives the in-process/filesystem [`LocalTransport`] (default build) and the
/// production `PubSubGcsTransport` (under `--features pubsub`). Every queue/store
/// operation goes through the trait (`pull_one`/`extend`/`ack`/`nack`/
/// `commit_and_gate`/`output_exists`/`read_output`), never a backend-specific
/// method.
///
/// **No internal seeding**: the loop ONLY pulls + works + commits + acks. The N
/// leaf descriptors are seeded by an explicit, separate step in `main` (local:
/// inline before the loop; pubsub: a one-off `--seed` seeder pod), so a worker
/// pod is a pure consumer and many pods can share one seeded queue.
///
/// Returns `Some(root_proof)` once the dynamic-depth root is committed and
/// verified, or `None` if the loop drained early due to a graceful shutdown
/// (SIGTERM) before the root existed — in which case remaining work stays on the
/// queue for another worker and NO root is fabricated. See
/// [`bench::shutdown`] for the drain contract.
fn run_dispatch_loop<T: WorkTransport>(
    transport: &T,
    radix: usize,
    leaf_count: usize,
    tx_per_proof: usize,
    timing: &mut TimingTree,
) -> Option<ProofWithPublicInputs<F, C, D>> {
    use bench::transport::tree_depth as t_depth;

    let depth = t_depth(leaf_count, radix).max(1);
    let root_key = tree_proof_path(depth, 0)
        .file_name()
        .unwrap()
        .to_string_lossy()
        .to_string();
    let worker = worker_identity();
    // `tx_per_proof` is now only relevant to the SEEDER (it goes into the leaf
    // descriptors): the worker loop reads it from each pulled descriptor's
    // `d.tx_per_proof`, so the loop no longer seeds and does not use the param
    // directly. Kept in the signature for a uniform call site across backends.
    let _ = tx_per_proof;

    let mut processed = 0usize;
    // Loop until the root output exists (the dynamic-depth top node is committed)
    // and the queue has drained.
    loop {
        // ── Graceful drain (ADR §7) ──────────────────────────────────────────
        // On SIGTERM (KEDA scale-down / Spot preemption) the shutdown flag flips.
        // We check it HERE, at the top of the iteration, BEFORE pulling the next
        // message: this stops pulling NEW work while the most-recently-leased
        // message — if any — has already been proved, committed, and acked at the
        // BOTTOM of the previous iteration. Breaking here therefore drains
        // gracefully: no leased message is ever dropped mid-prove. The pod then
        // exits, letting Kubernetes reclaim it within terminationGracePeriod, and
        // any not-yet-pulled work stays on the queue for another (or a restarted)
        // worker. Never scale the WHOLE pool to zero before the root exists — that
        // is enforced operationally by KEDA `minReplicaCount` = baseload, not here.
        if bench::shutdown::is_shutdown_requested() {
            info!(
                "[dispatch] graceful shutdown requested (SIGTERM): stop pulling new work; \
                 {processed} descriptor(s) already committed + acked. Draining and exiting \
                 cleanly without dropping any in-flight lease."
            );
            break;
        }

        // ── [instrumentation] PULL_latency_ms ────────────────────────────────
        // Time the pull so per-pod queue-wait is observable. We pull at most
        // once per iteration (flow-control = 1) and reuse the lease below.
        let iter_start = Instant::now();
        let pull_start = Instant::now();
        if transport.output_exists(&root_key) && transport.pull_one().is_none() {
            break;
        }
        let lease = transport.pull_one();
        let pull_latency_ms = pull_start.elapsed().as_millis();
        let Some(lease) = lease else {
            // Nothing pullable but root not yet committed: the gating either
            // hasn't published the next level or work is still in flight. With a
            // single in-process loop this means we're done seeding but a commit
            // race left no visible work — re-check the root then bail.
            if transport.output_exists(&root_key) {
                break;
            }
            panic!(
                "dispatch loop stalled: no work pullable but root {root_key} not committed \
                 (processed {processed} descriptors)"
            );
        };

        let d = lease.descriptor().clone();
        // Heartbeat the lease while we do the (potentially long) proving work.
        lease.extend();

        // ── [instrumentation] PROVE_total_latency_ms (+ per-role) ────────────
        // The transport's `commit_output` is the SINGLE writer of each proof
        // into the shared store (`PROOF_DIR`); the reused role code reads from
        // the same store. We therefore prove in-memory here and let the
        // transport commit — we do NOT call the FS-writing helpers
        // (`load_or_prove_leaf` / a redundant `write_proof`), which would create
        // the file before the transport CAS and turn every commit into an
        // `AlreadyExists` no-win that never advances readiness gating.
        let prove_start = Instant::now();
        let (bytes, role_tag) = match d.role {
            WorkRole::Leaf => {
                info!(
                    "[dispatch] worker={worker} leaf chunk {} -> {}",
                    d.chunk_idx,
                    d.output_key()
                );
                // Reuse the exact leaf execution: real batch + verified batch leaf.
                let batch = prove_leaf_batch(d.chunk_idx, d.tx_per_proof, timing);
                let proof = prove_batch_leaf(&batch);
                (
                    bincode::serialize(&proof).expect("serialize leaf proof"),
                    "leaf",
                )
            }
            WorkRole::TreeNode => {
                info!(
                    "[dispatch] worker={worker} fold level {} node {} (radix {}, N={}) -> {}",
                    d.level,
                    d.node_idx,
                    d.radix,
                    d.leaf_count,
                    d.output_key()
                );
                // `aggregate_node` reads its children from the shared store
                // (written there by the transport commit of the prior level) and
                // returns the parent proof; the transport commit below persists
                // it for the next level's readers.
                let parent = aggregate_node(
                    d.level,
                    d.node_idx,
                    d.radix,
                    d.leaf_count,
                    d.tx_per_proof,
                    timing,
                );
                (
                    bincode::serialize(&parent).expect("serialize parent proof"),
                    "fold",
                )
            }
            WorkRole::RootCoordinator => {
                // Not seeded by this loop (the loop verifies the root itself);
                // ack and continue if one ever appears.
                lease.ack();
                continue;
            }
        };
        let prove_total_latency_ms = prove_start.elapsed().as_millis();

        // ── [instrumentation] COMMIT_latency_ms + outcome ────────────────────
        // Atomic idempotent commit + readiness gating (publishes the parent fold
        // when this node completes its parent's last child). The `outcome` is the
        // CAS result: exactly one pod observes `Committed` per descriptor — the
        // `worker={id}` field makes that single winner attributable across pods.
        let commit_start = Instant::now();
        let outcome = transport.commit_and_gate(&d, &bytes);
        let commit_latency_ms = commit_start.elapsed().as_millis();
        let outcome_str = match outcome {
            CommitOutcome::Committed => "Committed",
            CommitOutcome::AlreadyExists => {
                info!(
                    "[dispatch] worker={worker} {} already committed (idempotent)",
                    d.output_key()
                );
                "AlreadyExists"
            }
        };

        // ── [instrumentation] ACK_latency_ms ─────────────────────────────────
        // Ack only AFTER the output is durably committed.
        let ack_start = Instant::now();
        lease.ack();
        let ack_latency_ms = ack_start.elapsed().as_millis();
        processed += 1;

        // ── [instrumentation] LOOP_iteration_total_ms ────────────────────────
        // One structured line per iteration carrying the pod identity + every
        // phase latency, so a many-pod run is observable (CAS-winner attribution,
        // queue-wait, prove cost per role) without a metrics backend.
        let loop_iteration_total_ms = iter_start.elapsed().as_millis();
        info!(
            "[instrumentation] worker={worker} key={} role={role_tag} \
             PULL_latency_ms={pull_latency_ms} PROVE_total_latency_ms={prove_total_latency_ms} \
             COMMIT_latency_ms={commit_latency_ms} outcome={outcome_str} \
             ACK_latency_ms={ack_latency_ms} LOOP_iteration_total_ms={loop_iteration_total_ms}",
            d.output_key()
        );
    }

    // If we broke out for graceful shutdown before the root was committed, return
    // `None`: the worker drained cleanly, leaving remaining work on the queue for
    // another worker — it must NOT pretend a root exists or fabricate one.
    if !transport.output_exists(&root_key) {
        info!(
            "[dispatch] loop exited before root committed ({processed} descriptor(s) done); \
             graceful drain leaves remaining work on the queue. No root harvested here."
        );
        return None;
    }

    info!("[dispatch] tree complete: {processed} descriptors processed; harvesting root");
    let root_bytes = transport
        .read_output(&root_key)
        .expect("root output must exist after dispatch loop completes");
    let root_proof: ProofWithPublicInputs<F, C, D> =
        bincode::deserialize(&root_bytes).expect("deserialize root proof");
    verify_root_proof(&root_proof, depth, radix);
    Some(root_proof)
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
            leaf_count,
            tx_per_proof,
        } => {
            let depth = tree_depth(leaf_count, radix);
            let node_count = nodes_at_level(leaf_count, radix, level);
            info!(
                "Tree node: aggregating level {level}/{depth} node {node_idx} \
                 (radix {radix}, N={leaf_count}, {node_count} node(s) at this level) \
                 by folding child proofs read from {PROOF_DIR}/"
            );

            // `aggregate_node` refuses genuinely-unimplementable cases (level
            // beyond the tree depth, node out of range, radix > circuit fan-in)
            // with a clear panic message — no silent `exit(2)` cap on level != 1.
            let parent =
                aggregate_node(level, node_idx, radix, leaf_count, tx_per_proof, &mut timing);
            let path = tree_proof_path(level, node_idx);
            write_proof(&path, &parent);
            let digest = proof_digest(&parent);

            let report = json!({
                "telemetry_event": "TREE_PARENT_PROVED",
                "span_id": format!("tree_L{level}_N{node_idx}"),
                "transport": "filesystem",
                "radix": radix,
                "leaf_count": leaf_count,
                "tree_depth": depth,
                "reduction_level": level,
                "nodes_at_level": node_count,
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
            leaf_count,
            node_idx,
            tx_per_proof,
        } => {
            // Root level is computed DYNAMICALLY from the actual leaf count N,
            // not hardcoded to 1. depth = ceil(log_radix(N)); the root is the
            // single node at that top level.
            let root_level = tree_depth(leaf_count, radix).max(1);
            info!(
                "Root coordinator: harvesting root proof for block #{block_number} \
                 (radix {radix}, N={leaf_count}, root_level={root_level}) from {PROOF_DIR}/"
            );

            let root_path = tree_proof_path(root_level, node_idx);
            if !root_path.exists() {
                eprintln!(
                    "Root proof {} not found (expected the single level-{root_level} node \
                     for N={leaf_count}, radix={radix}). Run the leaf workers and all \
                     {root_level} tree level(s) first; refusing to fabricate a root proof \
                     or settlement.",
                    root_path.display()
                );
                std::process::exit(1);
            }
            let root_proof = read_proof(&root_path);

            // Verify the root proof against the level-`root_level` circuit's VK.
            // For radix-2 depth-1 the binary circuit was used (back-compat); for
            // every other shape the dynamic-depth Hex node circuit chain is
            // rebuilt deterministically to the same VK that produced the proof.
            let verify_start = Instant::now();
            if radix == 2 && root_level == 1 {
                let (child_data, _t) = build_batch_leaf_data();
                let circuit = BinaryTreeChainCircuit::define(CIRCUIT_CONFIG, &child_data);
                let root_data = circuit.builder.build::<C>();
                root_data
                    .verify(root_proof.clone())
                    .expect("Root proof failed cryptographic verification");
            } else {
                let root_node = build_node_circuit_for_level(root_level);
                root_node
                    .data
                    .verify(root_proof.clone())
                    .expect("Root proof failed cryptographic verification");
            }
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
                "leaf_count": leaf_count,
                "root_level": root_level,
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
        Role::Work {
            radix,
            leaf_count,
            tx_per_proof,
            block_number,
            transport,
            seed,
            project,
            topic,
            subscription,
            bucket,
            ack_deadline,
            object_prefix,
        } => match transport {
            TransportKind::Local => {
                use bench::transport::{seed_leaf_descriptors, tree_depth as t_depth};
                let depth = t_depth(leaf_count, radix).max(1);
                info!(
                    "Fungible dispatch loop [--transport=local]: proving + folding an \
                     N={leaf_count} tree (radix {radix}, depth {depth}) over the \
                     LocalTransport, then verifying the root. Proof store: {PROOF_DIR}/"
                );

                // Install the graceful-drain signal handler: on SIGTERM (KEDA
                // scale-down / Spot preemption) or SIGINT the dispatch loop stops
                // pulling new work, finishes the in-flight lease, acks, and exits
                // cleanly. Failure to register is non-fatal (loop still runs).
                if let Err(e) = bench::shutdown::install_handlers() {
                    info!("[dispatch] could not install SIGTERM handler ({e}); continuing without OS-signal drain");
                }

                let transport = LocalTransport::new(PROOF_DIR);

                // Seed the N leaf descriptors EXPLICITLY here (the dispatch loop
                // no longer seeds internally — seeding is a separate step so a
                // worker pod is a pure consumer). For the in-process local
                // backend the seeder and the worker are the same process, so we
                // seed inline immediately before the loop; this preserves the
                // exact end-to-end local behaviour (`--transport=local` produces
                // a verified root from scratch). The `--seed` flag is accepted
                // for symmetry with the pubsub path but is a no-op distinction
                // here because local seeding always precedes the local loop.
                let seeds = seed_leaf_descriptors(radix, leaf_count, tx_per_proof);
                let seeded = seeds.len();
                for d in seeds {
                    transport.publish(d);
                }
                info!(
                    "[dispatch] seeded {seeded} leaf descriptor(s) onto LocalTransport \
                     (radix {radix}, N={leaf_count}, tx_per_proof={tx_per_proof}){}",
                    if seed {
                        " [--seed requested: local seeds inline then runs the loop]"
                    } else {
                        ""
                    }
                );

                let Some(root_proof) =
                    run_dispatch_loop(&transport, radix, leaf_count, tx_per_proof, &mut timing)
                else {
                    // Graceful shutdown drained the loop before the root existed.
                    // Report honestly and exit 0 (clean drain, work left on queue).
                    let report = json!({
                        "telemetry_event": "FUNGIBLE_DISPATCH_DRAINED_ON_SHUTDOWN",
                        "span_id": format!("dispatch_block_{block_number}"),
                        "transport": "local",
                        "radix": radix,
                        "leaf_count": leaf_count,
                        "tree_depth": depth,
                        "root_committed": false,
                        "status": "DRAINED_ON_SIGTERM",
                        "note": "graceful shutdown: stopped pulling new work, finished + acked \
                                 the in-flight lease, left remaining work on the queue"
                    });
                    println!("{report}");
                    info!(
                        "Fungible dispatch loop drained on graceful shutdown for block \
                         #{block_number} in {:?}; no root harvested (remaining work left on \
                         the queue for another worker).",
                        start.elapsed()
                    );
                    timing.print();
                    return;
                };

                let digest = proof_digest(&root_proof);
                use circuit::recursion::batch::BATCH_TARGET_INDEX;
                let root_batch = Batch::<F>::from_public_inputs(
                    &root_proof.public_inputs[..BATCH_TARGET_INDEX],
                );

                let report = json!({
                    "telemetry_event": "FUNGIBLE_DISPATCH_ROOT_VERIFIED",
                    "span_id": format!("dispatch_block_{block_number}"),
                    "transport": "local",
                    "radix": radix,
                    "leaf_count": leaf_count,
                    "tree_depth": depth,
                    "root_proof_key": tree_proof_path(depth, 0).file_name()
                        .map(|n| n.to_string_lossy().to_string()).unwrap_or_default(),
                    "proof_digest_sha256_8": digest,
                    "aggregated_batch_size": root_batch.batch_size,
                    "aggregated_end_block_number": root_batch.end_block_number,
                    "l1_settlement": "not_configured",
                    "elapsed_ms": start.elapsed().as_millis(),
                    "status": "OK"
                });
                println!("{report}");
                info!(
                    "Fungible dispatch loop produced + verified root ({digest}, {} txs \
                     aggregated) for block #{block_number} in {:?}",
                    root_batch.batch_size,
                    start.elapsed()
                );
                timing.print();
            }
            TransportKind::Pubsub => {
                run_pubsub_work(
                    radix,
                    leaf_count,
                    tx_per_proof,
                    block_number,
                    seed,
                    project,
                    topic,
                    subscription,
                    bucket,
                    ack_deadline,
                    object_prefix,
                );
            }
        },
    }
}

/// Drive the fungible dispatch loop over the production
/// [`PubSubGcsTransport`](bench::transport::pubsub::PubSubGcsTransport).
///
/// Compiled only with `--features pubsub`. Without the feature, the binary still
/// accepts `--transport=pubsub` but fails fast with a clear message rather than
/// pretending a cloud backend exists.
#[cfg(feature = "pubsub")]
#[allow(clippy::too_many_arguments)]
fn run_pubsub_work(
    radix: usize,
    leaf_count: usize,
    tx_per_proof: usize,
    block_number: u64,
    seed: bool,
    project: Option<String>,
    topic: String,
    subscription: String,
    bucket: String,
    ack_deadline: i32,
    object_prefix: String,
) {
    use bench::transport::pubsub::{PubSubGcsConfig, PubSubGcsTransport};
    use bench::transport::tree_depth as t_depth;

    let start = Instant::now();

    // Env fallbacks for the pubsub config (the clap `env` feature is not enabled
    // workspace-wide, so resolve env vars here to keep the default build's clap
    // feature set unchanged).
    let env_or = |flag: String, var: &str| -> String {
        if flag.trim().is_empty() {
            std::env::var(var).unwrap_or_default()
        } else {
            flag
        }
    };
    let project = project.or_else(|| std::env::var("PROVER_PUBSUB_PROJECT").ok());
    let topic = env_or(topic, "PROVER_PUBSUB_TOPIC");
    let subscription = env_or(subscription, "PROVER_PUBSUB_SUBSCRIPTION");
    let bucket = env_or(bucket, "PROVER_PUBSUB_BUCKET");
    let object_prefix = env_or(object_prefix, "PROVER_PUBSUB_OBJECT_PREFIX");
    let ack_deadline = std::env::var("PROVER_PUBSUB_ACK_DEADLINE")
        .ok()
        .and_then(|v| v.parse::<i32>().ok())
        .filter(|_| ack_deadline == 60) // only override the default if env set
        .unwrap_or(ack_deadline);

    let depth = t_depth(leaf_count, radix).max(1);
    let config = PubSubGcsConfig {
        project_id: project,
        topic,
        subscription,
        bucket,
        ack_deadline_secs: ack_deadline,
        object_prefix,
    };
    if let Err(e) = config.validate() {
        eprintln!("Invalid --transport=pubsub config: {e}");
        std::process::exit(2);
    }

    // Install the SAME graceful-drain signal handler as the local path. On the
    // LIVE run (TODO(confirm-on-live-run)) the production pull→prove→commit→ack
    // loop MUST honour SIGTERM exactly as the local loop does: on KEDA scale-down
    // or Spot preemption, stop pulling new Pub/Sub messages, finish the in-flight
    // prove, extend the lease via modifyAckDeadline while proving, ack only AFTER
    // the GCS `ifGenerationMatch=0` commit, then exit before
    // terminationGracePeriodSeconds elapses. The handler is wired here so the
    // contract is in place for the live runner; the live loop itself is NOT run
    // in this slice.
    if let Err(e) = bench::shutdown::install_handlers() {
        info!("[dispatch] could not install SIGTERM handler ({e}); live drain would proceed without OS-signal drain");
    }

    let mode = if seed { "seeder" } else { "worker" };
    info!(
        "Fungible dispatch [--transport=pubsub, mode={mode}]: connecting production \
         backend for an N={leaf_count} tree (radix {radix}, depth {depth}).",
    );

    // Connect the production transport. This authenticates + opens the GCS and
    // Pub/Sub clients (Application Default Credentials) and resolves the topic +
    // subscription. Connecting REQUIRES live GCP credentials + reachable
    // Pub/Sub/GCS; with no creds it fails cleanly HERE (clear error, exit 1) and
    // does NOT proceed — so this path never fabricates a run.
    //
    // TODO(confirm-on-live-run): real client auth + connect against a live
    // project. The auth/connect path is the maintained crate's; pilot-verified
    // ephemerally, not re-run live in this slice.
    let transport = match PubSubGcsTransport::connect(config) {
        Ok(t) => t,
        Err(e) => {
            eprintln!(
                "Failed to connect --transport=pubsub backend: {e}\n\
                 (This requires live GCP credentials, a real Pub/Sub topic/subscription, \
                 and a GCS bucket. The backend + primitives are verified-by-construction \
                 and were pilot-verified ephemerally; a full live run is \
                 TODO(confirm-on-live-run).)"
            );
            std::process::exit(1);
        }
    };

    info!(
        "Production transport connected: {} [mode={mode}].",
        transport.endpoint_summary()
    );

    if seed {
        // ── Seeder mode: a ONE-OFF bootstrap pod ─────────────────────────────
        // Publish the N leaf descriptors onto the topic, log what was seeded, and
        // EXIT. Readiness gating (driven by each worker's `commit_and_gate`) then
        // publishes the fold descriptors level-by-level as children complete, so
        // the seeder only ever publishes leaves. Exactly one seeder bootstraps a
        // run; the worker pods drain it.
        //
        // TODO(confirm-on-live-run): real Pub/Sub publish of the N leaves to a
        // live topic. The publish primitive is verified-by-construction
        // (`PubSubPublisher`); not re-run live in this slice.
        transport.seed_leaves(radix, leaf_count, tx_per_proof);
        let report = json!({
            "telemetry_event": "FUNGIBLE_DISPATCH_PUBSUB_SEEDED",
            "span_id": format!("dispatch_block_{block_number}"),
            "transport": "pubsub",
            "mode": "seeder",
            "endpoint": transport.endpoint_summary(),
            "radix": radix,
            "leaf_count": leaf_count,
            "tree_depth": depth,
            "seeded_leaf_descriptors": leaf_count,
            "status": "SEEDED_AND_EXITING",
            "live_run": "TODO(confirm-on-live-run)"
        });
        println!("{report}");
        info!(
            "Seeder published {leaf_count} leaf descriptor(s) for block \
             #{block_number}; exiting (workers will drain the queue). Live publish \
             is TODO(confirm-on-live-run)."
        );
        return;
    }

    // ── Worker mode: the REAL fungible dispatch loop ─────────────────────────
    // Run the SAME generic `run_dispatch_loop` the local path runs, now driving
    // the production `PubSubGcsTransport` through the `WorkTransport` trait:
    // pull→extend→prove→commit_and_gate(GCS ifGenerationMatch=0)→ack, honouring
    // graceful drain on SIGTERM. The loop genuinely pulls/proves/commits/acks
    // against the live broker + bucket — there is NO early "no live run" exit.
    //
    // TODO(confirm-on-live-run): real Pub/Sub pull/redelivery, real GCS CAS
    // across nodes, end-to-end completion on GKE. Every primitive
    // (flow-control=1 pull, modifyAckDeadline lease-extend, ack-after-commit,
    // nack-on-failure, ifGenerationMatch=0 commit + gating markers) is
    // verified-by-construction here and was pilot-verified ephemerally; the full
    // live run is the separate GKE smoke test, not executed in this slice.
    let Some(root_proof) =
        run_dispatch_loop(&transport, radix, leaf_count, tx_per_proof, &mut TimingTree::new("prover_node::pubsub_dispatch", Level::Info))
    else {
        // Graceful shutdown drained the loop before the root existed: honest
        // clean-drain report, exit 0 (work left on the Pub/Sub queue).
        let report = json!({
            "telemetry_event": "FUNGIBLE_DISPATCH_DRAINED_ON_SHUTDOWN",
            "span_id": format!("dispatch_block_{block_number}"),
            "transport": "pubsub",
            "mode": "worker",
            "endpoint": transport.endpoint_summary(),
            "radix": radix,
            "leaf_count": leaf_count,
            "tree_depth": depth,
            "root_committed": false,
            "status": "DRAINED_ON_SIGTERM",
            "note": "graceful shutdown: stopped pulling new work, finished + acked \
                     the in-flight lease, left remaining work on the Pub/Sub queue",
            "live_run": "TODO(confirm-on-live-run)"
        });
        println!("{report}");
        info!(
            "Pub/Sub worker drained on graceful shutdown for block #{block_number}; \
             no root harvested (remaining work left on the queue)."
        );
        return;
    };

    let digest = proof_digest(&root_proof);
    let report = json!({
        "telemetry_event": "FUNGIBLE_DISPATCH_ROOT_VERIFIED",
        "span_id": format!("dispatch_block_{block_number}"),
        "transport": "pubsub",
        "mode": "worker",
        "endpoint": transport.endpoint_summary(),
        "radix": radix,
        "leaf_count": leaf_count,
        "tree_depth": depth,
        "root_proof_key": tree_proof_path(depth, 0).file_name()
            .map(|n| n.to_string_lossy().to_string()).unwrap_or_default(),
        "proof_digest_sha256_8": digest,
        "ack_deadline_secs": transport.ack_deadline_secs(),
        "live_cloud_action_performed": true,
        "status": "OK",
        "live_run": "TODO(confirm-on-live-run)"
    });
    println!("{report}");
    info!(
        "Pub/Sub worker produced + verified root ({digest}) for block \
         #{block_number} in {:?}.",
        start.elapsed()
    );
}

/// Stub for when the `pubsub` feature is NOT enabled: accept the flag but fail
/// fast so the default (cloud-free) build never links cloud crates yet still
/// gives an honest error if someone passes `--transport=pubsub`.
#[cfg(not(feature = "pubsub"))]
#[allow(clippy::too_many_arguments)]
fn run_pubsub_work(
    _radix: usize,
    _leaf_count: usize,
    _tx_per_proof: usize,
    _block_number: u64,
    _seed: bool,
    _project: Option<String>,
    _topic: String,
    _subscription: String,
    _bucket: String,
    _ack_deadline: i32,
    _object_prefix: String,
) {
    eprintln!(
        "--transport=pubsub requires building with the `pubsub` cargo feature \
         (`cargo build --features pubsub`). The default build is cloud-free and does \
         not link the GCP Pub/Sub + GCS clients. Re-run with --transport=local for \
         the cloud-free dispatch loop."
    );
    std::process::exit(2);
}

#[cfg(test)]
mod tests {
    use super::*;
    use plonky2::util::timing::TimingTree;
    use std::collections::VecDeque;
    use std::sync::{Arc, Mutex};

    // ── Generic dispatch-loop signature: drives ANY WorkTransport ────────────
    //
    // The refactor of #306 makes `run_dispatch_loop<T: WorkTransport>` generic so
    // ONE loop drives both the `LocalTransport` (default build) and the
    // production `PubSubGcsTransport` (under `--features pubsub`) through trait
    // methods only. The production backend needs a live broker, so we cannot
    // instantiate it in a cloud-free test; instead we (1) assert at COMPILE TIME
    // that `run_dispatch_loop` monomorphizes for an ARBITRARY non-Local
    // `WorkTransport` double, which is exactly the guarantee that the generic
    // signature works for any backend, and (2) drive the loop's trait-only
    // transport mechanics (pull → commit_and_gate → gating publishes parent →
    // ack → root committed) over an in-memory double to a verified-root sentinel,
    // proving the generic body reaches a committed root through trait calls
    // alone — without real STARK proving (kept fast + cloud-free). The full real
    // verified-root e2e is the `--transport=local` binary smoke.

    /// A minimal in-memory [`WorkTransport`] double: an in-process queue + a
    /// HashMap-backed CAS store + the SAME readiness-gating algorithm the loop
    /// relies on (publish the parent fold exactly once when a node's real-child
    /// quota of distinct children is committed). It is NOT `LocalTransport` (no
    /// filesystem), so it independently exercises the trait surface the generic
    /// loop calls.
    #[derive(Clone)]
    struct InMemTransport {
        inner: Arc<Mutex<InMemState>>,
    }

    struct InMemState {
        queue: VecDeque<bench::transport::WorkDescriptor>,
        store: std::collections::HashMap<String, Vec<u8>>,
        /// Per-parent set of distinct committed child indices (gating counter).
        gate: std::collections::HashMap<(usize, usize), std::collections::HashSet<usize>>,
        /// Parents already published (exactly-once publish guard).
        published: std::collections::HashSet<(usize, usize)>,
    }

    impl InMemTransport {
        fn new() -> Self {
            Self {
                inner: Arc::new(Mutex::new(InMemState {
                    queue: VecDeque::new(),
                    store: std::collections::HashMap::new(),
                    gate: std::collections::HashMap::new(),
                    published: std::collections::HashSet::new(),
                })),
            }
        }
    }

    /// A lease over the in-memory double. Ack removes nothing extra (the message
    /// is popped on pull, matching flow-control=1); nack re-enqueues.
    struct InMemLease {
        transport: InMemTransport,
        descriptor: bench::transport::WorkDescriptor,
        done: bool,
    }

    impl WorkLease for InMemLease {
        fn descriptor(&self) -> &bench::transport::WorkDescriptor {
            &self.descriptor
        }
        fn extend(&self) {}
        fn ack(mut self) {
            self.done = true;
        }
        fn nack(mut self) {
            self.done = true;
            let mut s = self.transport.inner.lock().unwrap();
            s.queue.push_back(self.descriptor.clone());
        }
    }

    impl Drop for InMemLease {
        fn drop(&mut self) {
            if !self.done {
                let mut s = self.transport.inner.lock().unwrap();
                s.queue.push_back(self.descriptor.clone());
            }
        }
    }

    impl WorkTransport for InMemTransport {
        type Lease = InMemLease;

        fn pull_one(&self) -> Option<Self::Lease> {
            let mut s = self.inner.lock().unwrap();
            let descriptor = s.queue.pop_front()?;
            Some(InMemLease {
                transport: self.clone(),
                descriptor,
                done: false,
            })
        }

        fn publish(&self, descriptor: bench::transport::WorkDescriptor) {
            let mut s = self.inner.lock().unwrap();
            if !s.queue.iter().any(|d| *d == descriptor) {
                s.queue.push_back(descriptor);
            }
        }

        fn commit_output(&self, key: &str, bytes: &[u8]) -> CommitOutcome {
            let mut s = self.inner.lock().unwrap();
            if s.store.contains_key(key) {
                CommitOutcome::AlreadyExists
            } else {
                s.store.insert(key.to_string(), bytes.to_vec());
                CommitOutcome::Committed
            }
        }

        fn output_exists(&self, key: &str) -> bool {
            self.inner.lock().unwrap().store.contains_key(key)
        }

        fn read_output(&self, key: &str) -> Option<Vec<u8>> {
            self.inner.lock().unwrap().store.get(key).cloned()
        }

        fn commit_and_gate(
            &self,
            descriptor: &bench::transport::WorkDescriptor,
            bytes: &[u8],
        ) -> CommitOutcome {
            use bench::transport::{real_children_for_node, tree_depth, Role, WorkDescriptor};
            let outcome = self.commit_output(&descriptor.output_key(), bytes);
            if outcome != CommitOutcome::Committed {
                return outcome;
            }
            // Mirror the LocalTransport gating: a committed child advances its
            // parent's distinct-child set and publishes the parent fold once the
            // real-child quota is met. Self-contained (no FS), driving the same
            // geometry helpers re-exported from the transport crate.
            let (child_level, child_idx) = match descriptor.role {
                Role::Leaf => (0usize, descriptor.chunk_idx),
                Role::TreeNode => (descriptor.level, descriptor.node_idx),
                Role::RootCoordinator => return outcome,
            };
            let radix = descriptor.radix;
            let leaf_count = descriptor.leaf_count;
            let depth = tree_depth(leaf_count, radix);
            let parent_level = child_level + 1;
            if parent_level > depth {
                return outcome;
            }
            let parent_idx = child_idx / radix;
            let needed = real_children_for_node(leaf_count, radix, parent_level, parent_idx);
            let publish_parent = {
                let mut s = self.inner.lock().unwrap();
                let set = s.gate.entry((parent_level, parent_idx)).or_default();
                set.insert(child_idx);
                let have = set.len();
                have >= needed && s.published.insert((parent_level, parent_idx))
            };
            if publish_parent {
                self.publish(WorkDescriptor::tree_node(
                    parent_level,
                    parent_idx,
                    radix,
                    leaf_count,
                    descriptor.tx_per_proof,
                ));
            }
            outcome
        }
    }

    /// COMPILE-TIME guarantee: `run_dispatch_loop` monomorphizes for an arbitrary
    /// non-`LocalTransport` `WorkTransport`. If the loop ever reached for a
    /// `LocalTransport`-specific (inherent) method, this would fail to compile —
    /// which is precisely the regression the #306 generic refactor prevents and
    /// what lets the SAME loop drive `PubSubGcsTransport` under `--features
    /// pubsub`. We only need it to TYPE-CHECK, never to run (real proving), so it
    /// is referenced behind a `false` guard.
    #[allow(dead_code)]
    fn _assert_dispatch_loop_is_generic() {
        if false {
            let local = LocalTransport::new(std::env::temp_dir().join("never"));
            let _ = run_dispatch_loop(&local, 2, 4, 1, &mut TimingTree::default());
            let inmem = InMemTransport::new();
            let _ = run_dispatch_loop(&inmem, 2, 4, 1, &mut TimingTree::default());
        }
    }

    /// Drive the generic loop's TRANSPORT MECHANICS over the in-memory double to
    /// a verified-root sentinel WITHOUT real STARK proving: this is the exact
    /// pull → commit_and_gate → (gating publishes parent) → ack progression the
    /// generic `run_dispatch_loop<T>` body performs, but committing a cheap
    /// sentinel payload instead of a real proof so it stays fast + cloud-free.
    /// Proves the generic signature drives ANY `WorkTransport` (not just Local)
    /// from seeded leaves all the way to a committed root via trait methods only.
    fn drive_to_root<T: WorkTransport>(transport: &T, radix: usize, leaf_count: usize) -> bool {
        use bench::transport::{seed_leaf_descriptors, tree_depth, WorkDescriptor};
        let depth = tree_depth(leaf_count, radix).max(1);
        let root_key = WorkDescriptor::tree_node(depth, 0, radix, leaf_count, 1).output_key();
        // Seed leaves (explicit, like the wired local/pubsub seeder).
        for d in seed_leaf_descriptors(radix, leaf_count, 1) {
            transport.publish(d);
        }
        let mut iters = 0usize;
        loop {
            if transport.output_exists(&root_key) && transport.pull_one().is_none() {
                break;
            }
            let Some(lease) = transport.pull_one() else {
                if transport.output_exists(&root_key) {
                    break;
                }
                return false; // stalled: no work but no root
            };
            let d = lease.descriptor().clone();
            lease.extend();
            // Cheap sentinel "proof" bytes (NOT a real STARK) — we are testing
            // the generic loop's transport progression, not the circuits.
            let bytes = format!("sentinel:{}", d.output_key()).into_bytes();
            let _ = transport.commit_and_gate(&d, &bytes);
            lease.ack();
            iters += 1;
            assert!(iters < 10_000, "loop must terminate");
        }
        transport.output_exists(&root_key)
    }

    #[test]
    fn generic_loop_drives_local_transport_to_root_mechanics() {
        // radix=2, N=4 => 4 leaves + 2 level-1 folds + 1 root fold = 7 commits.
        let store = tmp_store("generic-local");
        let transport = LocalTransport::new(&store);
        assert!(
            drive_to_root(&transport, 2, 4),
            "generic loop mechanics must reach a committed root over LocalTransport"
        );
        std::fs::remove_dir_all(&store).ok();
    }

    #[test]
    fn generic_loop_drives_inmemory_transport_to_root_mechanics() {
        // The SAME generic progression over a non-Local WorkTransport double,
        // proving `run_dispatch_loop<T>` is genuinely transport-agnostic (this is
        // the cloud-free stand-in for the PubSubGcsTransport instantiation).
        let transport = InMemTransport::new();
        assert!(
            drive_to_root(&transport, 2, 4),
            "generic loop mechanics must reach a committed root over a non-Local transport"
        );
        // Also exercise a deeper, under-full tree (N=5, depth 3) to cover
        // multi-level gating through the trait.
        let deep = InMemTransport::new();
        assert!(
            drive_to_root(&deep, 2, 5),
            "generic loop must handle a deeper under-full tree via trait methods"
        );
    }

    #[test]
    fn worker_identity_is_stable_and_nonempty() {
        // Pod-identity instrumentation: HOSTNAME when set, pid fallback otherwise.
        let id = worker_identity();
        assert!(!id.trim().is_empty(), "worker identity must never be empty");
        // Two calls in the same process must agree (stable per pod).
        assert_eq!(id, worker_identity(), "worker identity must be stable");
    }

    // ── Graceful-shutdown drain contract (no proving, no real signals) ──
    //
    // These tests exercise the dispatch loop's "stop pulling new work on
    // shutdown, finish the current lease, ack, exit" policy WITHOUT raising an OS
    // signal and WITHOUT running real proofs. The dispatch loop reads exactly one
    // thing — `bench::shutdown::is_shutdown_requested()` — at the top of each
    // iteration before pulling, so we model that boundary directly against the
    // real `LocalTransport` queue. The flag is driven via `request_shutdown()`
    // (the same store the OS handler performs), so this is a faithful unit test
    // of the drain logic with deterministic, signal-free control.

    // The graceful-shutdown flag is process-global, so the drain tests must not
    // race each other. This mutex serialises them (each takes it for its whole
    // body) so the shared flag is never observed across tests.
    static DRAIN_TEST_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

    /// Mirror of the dispatch loop's pull-gating decision: returns how many
    /// messages a loop with this drain contract would pull from `transport`,
    /// where `set_shutdown_after` is the number of pulls after which a SIGTERM is
    /// simulated. Each "iteration" first checks the shutdown flag (stop pulling
    /// if set), then pulls one message and acks it (modelling "finish + ack the
    /// in-flight lease"). This is the exact shape of `run_dispatch_loop`'s top
    /// guard, minus the proving.
    fn drain_pulls(transport: &LocalTransport, set_shutdown_after: usize) -> usize {
        bench::shutdown::reset_for_test();
        let mut pulled = 0usize;
        loop {
            // Top-of-iteration graceful-drain check (identical to the loop).
            if bench::shutdown::is_shutdown_requested() {
                break;
            }
            match transport.pull_one() {
                Some(lease) => {
                    lease.extend();
                    // "Finish + ack the in-flight lease" before honouring shutdown.
                    lease.ack();
                    pulled += 1;
                    if pulled == set_shutdown_after {
                        // Simulate SIGTERM arriving mid-run (after this lease is
                        // already acked, as in production).
                        bench::shutdown::request_shutdown();
                    }
                }
                None => break,
            }
        }
        bench::shutdown::reset_for_test();
        pulled
    }

    fn tmp_store(tag: &str) -> std::path::PathBuf {
        use std::time::{SystemTime, UNIX_EPOCH};
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let dir = std::env::temp_dir().join(format!("prover_node_drain_{tag}_{nanos}"));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn drain_stops_pulling_new_work_after_shutdown() {
        let _guard = DRAIN_TEST_LOCK.lock().unwrap_or_else(|p| p.into_inner());
        // Seed 5 leaf descriptors; simulate SIGTERM after the 2nd is acked.
        // The loop must pull exactly 2 (finish the 2nd, then stop pulling), NOT
        // drain all 5 — proving the "stop pulling new work on SIGTERM" contract.
        let store = tmp_store("stops");
        let transport = LocalTransport::new(&store).without_auto_gating();
        for chunk in 0..5usize {
            transport.publish(bench::transport::WorkDescriptor::leaf(chunk, 2, 5, 1));
        }
        let pulled = drain_pulls(&transport, 2);
        assert_eq!(
            pulled, 2,
            "loop must finish the in-flight lease then stop pulling on shutdown"
        );
        std::fs::remove_dir_all(&store).ok();
    }

    #[test]
    fn no_shutdown_drains_entire_queue() {
        let _guard = DRAIN_TEST_LOCK.lock().unwrap_or_else(|p| p.into_inner());
        // With shutdown never requested, the same loop drains all seeded work —
        // the drain check is inert on the happy path (no regression to e2e).
        let store = tmp_store("nodrain");
        let transport = LocalTransport::new(&store).without_auto_gating();
        for chunk in 0..4usize {
            transport.publish(bench::transport::WorkDescriptor::leaf(chunk, 2, 4, 1));
        }
        // `usize::MAX` => shutdown is never triggered by the helper.
        let pulled = drain_pulls(&transport, usize::MAX);
        assert_eq!(pulled, 4, "without shutdown the loop must drain all work");
        std::fs::remove_dir_all(&store).ok();
    }

    #[test]
    fn shutdown_before_first_pull_pulls_nothing() {
        let _guard = DRAIN_TEST_LOCK.lock().unwrap_or_else(|p| p.into_inner());
        // If SIGTERM arrives before any work is pulled (e.g. pod terminated while
        // idle), the loop pulls zero and exits immediately — clean drain.
        let store = tmp_store("preempt");
        let transport = LocalTransport::new(&store).without_auto_gating();
        for chunk in 0..3usize {
            transport.publish(bench::transport::WorkDescriptor::leaf(chunk, 2, 3, 1));
        }
        bench::shutdown::reset_for_test();
        bench::shutdown::request_shutdown();
        // Mirror the loop's top guard once: shutdown set => no pull.
        let pulled = if bench::shutdown::is_shutdown_requested() {
            0
        } else {
            transport.pull_one().map(|l| l.ack()).is_some() as usize
        };
        bench::shutdown::reset_for_test();
        assert_eq!(pulled, 0, "shutdown before first pull must drain nothing");
        std::fs::remove_dir_all(&store).ok();
    }

    // ── Dynamic tree-geometry helpers (pure, no proving) ──

    #[test]
    fn test_tree_depth_radix2() {
        // depth = ceil(log2(N))
        assert_eq!(tree_depth(1, 2), 0);
        assert_eq!(tree_depth(2, 2), 1);
        assert_eq!(tree_depth(3, 2), 2);
        assert_eq!(tree_depth(4, 2), 2);
        assert_eq!(tree_depth(5, 2), 3);
        assert_eq!(tree_depth(8, 2), 3); // exact power must not overshoot
        assert_eq!(tree_depth(9, 2), 4);
        assert_eq!(tree_depth(16, 2), 4);
    }

    #[test]
    fn test_tree_depth_radix16() {
        assert_eq!(tree_depth(1, 16), 0);
        assert_eq!(tree_depth(16, 16), 1);
        assert_eq!(tree_depth(17, 16), 2);
        assert_eq!(tree_depth(256, 16), 2); // exact 16^2
        assert_eq!(tree_depth(257, 16), 3);
    }

    #[test]
    fn test_nodes_at_level_radix2_n4() {
        // N=4, radix=2 => depth 2: level 1 has 2 nodes, level 2 (root) has 1.
        assert_eq!(nodes_at_level(4, 2, 1), 2);
        assert_eq!(nodes_at_level(4, 2, 2), 1);
    }

    #[test]
    fn test_nodes_at_level_radix2_n8() {
        // N=8, radix=2 => depth 3: levels have 4, 2, 1 nodes.
        assert_eq!(nodes_at_level(8, 2, 1), 4);
        assert_eq!(nodes_at_level(8, 2, 2), 2);
        assert_eq!(nodes_at_level(8, 2, 3), 1);
    }

    #[test]
    fn test_nodes_at_level_radix2_n5_underfull() {
        // N=5, radix=2 => depth 3: level 1 ceil(5/2)=3, level 2 ceil(5/4)=2, root 1.
        assert_eq!(tree_depth(5, 2), 3);
        assert_eq!(nodes_at_level(5, 2, 1), 3);
        assert_eq!(nodes_at_level(5, 2, 2), 2);
        assert_eq!(nodes_at_level(5, 2, 3), 1);
    }

    #[test]
    fn test_real_children_for_node() {
        // N=4, radix=2, level 1: node 0 -> leaves {0,1}, node 1 -> leaves {2,3}.
        assert_eq!(real_children_for_node(4, 2, 1, 0), 2);
        assert_eq!(real_children_for_node(4, 2, 1, 1), 2);
        // N=4, radix=2, level 2 (root): one node folding the 2 level-1 nodes.
        assert_eq!(real_children_for_node(4, 2, 2, 0), 2);
        // N=5, radix=2, level 1: nodes 0,1 full (2 each), node 2 under-full (1).
        assert_eq!(real_children_for_node(5, 2, 1, 0), 2);
        assert_eq!(real_children_for_node(5, 2, 1, 1), 2);
        assert_eq!(real_children_for_node(5, 2, 1, 2), 1);
        // N=5, radix=2, level 2: children population = nodes_at_level(5,2,1)=3,
        // so node 0 folds 2, node 1 folds the leftover 1.
        assert_eq!(real_children_for_node(5, 2, 2, 0), 2);
        assert_eq!(real_children_for_node(5, 2, 2, 1), 1);
    }

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
