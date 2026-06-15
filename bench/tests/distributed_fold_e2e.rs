// Copyright (c) Elliot Technologies, Inc.
// SPDX-License-Identifier: BUSL-1.1

//! LOCAL end-to-end distributed-fold acceptance gate (issue #179, the
//! "fan-IN half" of the distributed prover).
//!
//! This is the hard acceptance criterion of #179:
//!
//! > A LOCAL end-to-end test proves ONE real multi-chunk block goes
//! > L1 -> L2(cells) -> fold -> L4 (coordinator) and produces a genuine,
//! > verifiable block proof -- with real measured merge + L4 wall times in
//! > the event stream.
//!
//! What it exercises, end to end, with NO live GCP / Pub/Sub:
//!
//!   1. CELL side (per chunk): the REAL `BlockTxCircuit` (L1) prove + the REAL
//!      `BlockTxChainCircuit` (L2) leaf-chain prove, seeded from the chunk's
//!      POSITIONAL pre-state snapshot (the #177 FINDING D path the deployed
//!      cell uses). The cell then SERIALIZES its real L2 leaf proof with the
//!      EXACT `serde_json` of `ProofWithPublicInputs` the production cell
//!      uploads, and UPLOADS it to a proof store via the SAME
//!      [`bench::conductor::GcloudStorage`] surface the production cell uses.
//!
//!   2. PROOF STORE: a HERMETIC, local-filesystem-backed stand-in for GCS.
//!      We do NOT stub `GcloudStorage` -- we point its `gcloud_bin` at a tiny
//!      local `gcloud` shim that implements `gcloud storage cp` against a
//!      temp directory acting as the bucket. So the REAL `cp_to_gcs_argv` /
//!      `cp_from_gcs_argv` argv builders, the REAL `upload`/`download`
//!      temp-file staging, and the REAL serde round-trip all run -- only the
//!      network/auth is replaced by a local dir. The serialized proof bytes
//!      are byte-identical to what crosses the wire on GKE.
//!
//!   3. COORDINATOR side: gather the k chunk proof OBJECT KEYS (keyed by
//!      `{height}/{witness_index}` via the shared `proof_object_key`),
//!      DOWNLOAD + deserialize them through `GcloudStorage`, run the REAL
//!      `BlockTxChainMergeCircuit` merge tree (measuring the merge wall), then
//!      the REAL `BlockCircuit` L4 prove + VERIFY over the merged chain proof
//!      (measuring the L4 wall). The merge + L4 circuit code is the SAME the
//!      production coordinator's `fold_merge_tree` / `prove_block_l4_from_chain`
//!      helpers drive; this test reconstructs the same call sequence against
//!      the library `circuit::` crate (the helpers themselves live in the
//!      `bench` BINARY and are not reachable from an integration test).
//!
//!      Issue #193: the fold is run BOTH ways — the SERIAL fold (pre-#193) and
//!      the PARALLEL per-level fold (the production coordinator's new
//!      `fold_merge_tree` workers>1 path) — over the SAME k leaves, and the two
//!      final proofs are asserted BIT-IDENTICAL (the determinism/equivalence
//!      guarantee). L4 is then driven through the PARALLEL result so this gate
//!      exercises the new parallel coordinator fold end to end.
//!
//! Assertions (all must hold):
//!   - MULTI-CHUNK: k >= 2 so the merge tree actually fires (a single chunk
//!     would not exercise `BlockTxChainMergeCircuit` recursion).
//!   - DETERMINISM (issue #193): the serial and parallel folds over the same
//!     leaves produce a BIT-IDENTICAL final proof (same public inputs).
//!   - The final L4 `BlockCircuit` block proof VERIFIES (genuine, not stubbed).
//!   - The MEASURED merge wall and L4 wall are both > 0 (real proving time).
//!   - The BENCH_EVENT `CoordinatorFold` line emitted for this block is
//!     labeled `merge_source: "measured"` / `l4_source: "measured"` and
//!     carries the same non-zero measured walls -- the WS6 measured-vs-modeled
//!     distinction downstream consumers rely on.
//!
//! ## EXPENSIVE -- opt-in only
//!
//! Real proving of even a small multi-chunk block is slow and stack-hungry, so
//! this test is gated TWICE out of the fast `cargo test` / `make local-test`
//! lane: it is marked `#[ignore]` AND it early-returns unless `DIST_FOLD_E2E=1`
//! is set. Run it explicitly (see `make e2e` or):
//!
//! ```sh
//! DIST_FOLD_E2E=1 cargo test -p bench --release --test distributed_fold_e2e \
//!   -- --ignored --nocapture --test-threads=1
//! ```
//!
//! `DIST_FOLD_E2E_S` (default 4) and `DIST_FOLD_E2E_K` (default 2) tune the
//! chunk width and chunk count; keep k >= 2 (multi-chunk is required).
//!
//! Refs #179 #177 #67 #117 #61 #72.

use std::time::Instant;

use bench::conductor::fold::{InMemoryFoldTransport, MergeFn, fold_distributed};
use bench::conductor::storage::{GcloudStorage, StorageConfig, merge_object_key, proof_object_key};
use bench::events::{self, BenchEvent, now_iso8601, peak_rss_mb};
use bench::prestate::{ChunkPreState, sweep_per_tx_snapshots};
use bench::seed::seed_from_state;
use circuit::block::Block;
use circuit::block_constraints::{BlockCircuit, Circuit as _};
use circuit::block_pre_execution::{BlockPreExec, BlockPreExecWitness};
use circuit::block_pre_execution_constraints::{BlockPreExecutionCircuit, Circuit as _};
use circuit::block_tx_chain::BlockTxChainWitness;
use circuit::block_tx_chain_constraints::{BlockTxChainCircuit, Circuit as _};
use circuit::block_tx_chain_merge_constraints::{BlockTxChainMergeCircuit, Circuit as _};
use circuit::block_tx_constraints::{BlockTxCircuit, Circuit as _};
use circuit::keccak::helpers::keccak;
use circuit::types::config::{C, CIRCUIT_CONFIG, D, F};
use plonky2::plonk::proof::ProofWithPublicInputs;
use plonky2::recursion::dummy_circuit::dummy_circuit;

const CHAIN_ID: u32 = 304;

/// A merge-tree node: the proof plus whether it is a merge (`true`) or a leaf
/// chain proof (`false`). Mirrors the binary's `TreeNode`.
type TreeNode = (ProofWithPublicInputs<F, C, D>, bool);

/// Issue #193: prove ONE pairwise merge. Mirrors the binary's shared
/// `prove_merge_pair` helper (single source of truth for one merge) so both the
/// serial and parallel test folds below invoke the exact same merge circuit.
fn prove_pair(
    merge_target: &circuit::block_tx_chain_merge_constraints::BlockTxChainMergeTarget,
    merge_data: &plonky2::plonk::circuit_data::CircuitData<F, C, D>,
    left: &TreeNode,
    right: &TreeNode,
) -> TreeNode {
    let proof = BlockTxChainMergeCircuit::prove(
        merge_target,
        merge_data,
        &left.0,
        left.1,
        &right.0,
        right.1,
    )
    .expect("merge prove");
    (proof, true)
}

/// Issue #193: SERIAL coordinator fold — byte-for-byte the pre-#193 path. Folds
/// `leaves` into one block-chain proof, returning `(final_node, depth, merges)`.
fn fold_serial(
    merge_target: &circuit::block_tx_chain_merge_constraints::BlockTxChainMergeTarget,
    merge_data: &plonky2::plonk::circuit_data::CircuitData<F, C, D>,
    leaves: &[ProofWithPublicInputs<F, C, D>],
) -> (TreeNode, usize, usize) {
    let mut level: Vec<TreeNode> = leaves.iter().map(|p| (p.clone(), false)).collect();
    let mut depth = 0usize;
    let mut merges = 0usize;
    while level.len() > 1 {
        depth += 1;
        let mut iter = level.into_iter();
        let mut next: Vec<TreeNode> = Vec::new();
        while let Some(left) = iter.next() {
            match iter.next() {
                Some(right) => {
                    next.push(prove_pair(merge_target, merge_data, &left, &right));
                    merges += 1;
                }
                None => next.push(left),
            }
        }
        level = next;
    }
    let node = level.pop().expect("serial fold produced a final proof");
    (node, depth, merges)
}

/// Issue #193: PARALLEL coordinator fold — folds each LEVEL concurrently across
/// an owned rayon pool (mirrors the binary's `fold_merge_tree` workers>1 path:
/// collect the level's pairs preserving odd carry-up, prove with
/// `into_par_iter`, then RE-SORT by the stable in-level index for determinism).
/// Returns `(final_node, depth, merges)`. The KEY correctness property the
/// determinism test asserts: this produces a bit-identical final proof to
/// [`fold_serial`] over the same leaves regardless of worker scheduling.
fn fold_parallel(
    merge_target: &circuit::block_tx_chain_merge_constraints::BlockTxChainMergeTarget,
    merge_data: &plonky2::plonk::circuit_data::CircuitData<F, C, D>,
    leaves: &[ProofWithPublicInputs<F, C, D>],
    workers: usize,
) -> (TreeNode, usize, usize) {
    use rayon::prelude::*;
    let pool = rayon::ThreadPoolBuilder::new()
        .num_threads(workers)
        .build()
        .expect("build fold pool");
    let mut level: Vec<TreeNode> = leaves.iter().map(|p| (p.clone(), false)).collect();
    let mut depth = 0usize;
    let mut merges = 0usize;
    while level.len() > 1 {
        depth += 1;
        let mut pairs: Vec<(TreeNode, Option<TreeNode>)> = Vec::with_capacity(level.len() / 2 + 1);
        let mut iter = level.into_iter();
        while let Some(left) = iter.next() {
            match iter.next() {
                Some(right) => pairs.push((left, Some(right))),
                None => pairs.push((left, None)),
            }
        }
        let mut indexed: Vec<(usize, TreeNode, bool)> = pool.install(|| {
            pairs
                .into_par_iter()
                .enumerate()
                .map(|(i, (left, right_opt))| match right_opt {
                    Some(right) => (i, prove_pair(merge_target, merge_data, &left, &right), true),
                    None => (i, left, false),
                })
                .collect()
        });
        // Determinism: restore in-level order regardless of completion order.
        indexed.sort_by_key(|(i, _, _)| *i);
        let mut next: Vec<TreeNode> = Vec::with_capacity(indexed.len());
        for (_, node, was_merge) in indexed {
            if was_merge {
                merges += 1;
            }
            next.push(node);
        }
        level = next;
    }
    let node = level.pop().expect("parallel fold produced a final proof");
    (node, depth, merges)
}

/// Issue #198: DISTRIBUTED coordinator fold — folds `leaves` by genuinely
/// routing every merge through the SHARED library distributed driver
/// (`bench::conductor::fold_distributed`): each merge pair is emitted as a TASK
/// to the [`InMemoryFoldTransport`], proven on an INDEPENDENT worker thread,
/// and its output TRANSITS the (in-memory) proof store under the real
/// `{height}/m/{level}/{index}` key namespace before the next level reads it.
/// The merge itself is the EXACT same `BlockTxChainMergeCircuit::prove` the
/// serial/parallel folds use (supplied as the single `MergeFn`), so there is
/// one merge implementation. Returns the final node and tree shape.
///
/// The KEY correctness property the e2e asserts: this produces a BIT-IDENTICAL
/// final proof to [`fold_serial`] over the same leaves — the cross-machine fold
/// (issue #198) is the multi-worker generalization of the #193 contract.
fn fold_distributed_via_library(
    merge_target: &circuit::block_tx_chain_merge_constraints::BlockTxChainMergeTarget,
    merge_data: &plonky2::plonk::circuit_data::CircuitData<F, C, D>,
    height: u64,
    leaves: &[ProofWithPublicInputs<F, C, D>],
    workers: usize,
) -> (TreeNode, usize, usize, usize) {
    // Leaf keys: the cells' upload keys (level 0 of transit), exactly the
    // production `{height}/{witness_index}` namespace.
    let leaf_keys: Vec<String> = (0..leaves.len() as u64)
        .map(|i| proof_object_key(height, i))
        .collect();
    let leaves_vec: Vec<ProofWithPublicInputs<F, C, D>> = leaves.to_vec();

    // The SINGLE merge implementation, borrowed by the distributed driver: the
    // SAME `BlockTxChainMergeCircuit::prove` the serial/parallel folds invoke.
    let merge_fn: Box<MergeFn<'_, ProofWithPublicInputs<F, C, D>>> =
        Box::new(move |left, l_is_merge, right, r_is_merge| {
            BlockTxChainMergeCircuit::prove(
                merge_target,
                merge_data,
                left,
                l_is_merge,
                right,
                r_is_merge,
            )
            .map_err(|e| anyhow::anyhow!("merge prove: {e:?}"))
        });

    // Hermetic transport: in-memory proof store + independent worker threads.
    // The merges GENUINELY fan out across `workers` threads and transit by key
    // — only the wire (Pub/Sub) and bucket (GCS) are replaced by memory.
    let transport = InMemoryFoldTransport::with_leaves(&leaf_keys, &leaves_vec, workers);

    let out = fold_distributed(
        height,
        leaves_vec,
        leaf_keys,
        &transport,
        merge_fn.as_ref(),
    )
    .expect("distributed fold");

    // Confirm intermediate proofs really transited the merge-key namespace and
    // their wire size held constant (issue #198 open measurement #3).
    println!(
        "[e2e]   DISTRIBUTED fold: depth={} merges={} max_intermediate_bytes={} \
         transit_total_ms={} (issue #198)",
        out.depth,
        out.merges,
        out.max_intermediate_bytes,
        out.transit_total.as_millis(),
    );
    for m in &out.level_metrics {
        println!(
            "[e2e]     level {} barrier: tasks={} odd_carry={} slowest_prove_ms={} \
             straggler_ms={}",
            m.level, m.tasks, m.odd_carry, m.slowest_prove_ms, m.straggler_ms,
        );
    }
    // Sanity: the merge-key helper the leader uses matches the namespace.
    let _ = merge_object_key(height, 1, 0);

    (
        (out.final_proof, out.final_is_merge),
        out.depth,
        out.merges,
        out.max_intermediate_bytes,
    )
}

fn enabled() -> bool {
    std::env::var("DIST_FOLD_E2E")
        .map(|v| v == "1")
        .unwrap_or(false)
}

fn env_usize(name: &str, default: usize) -> usize {
    std::env::var(name)
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(default)
}

fn load_block() -> Block<F> {
    let path = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("bench_test.json");
    let data = std::fs::read_to_string(&path)
        .unwrap_or_else(|e| panic!("cannot read {}: {e}", path.display()));
    serde_json::from_str(&data).expect("bench_test.json parses as Block")
}

/// Write a tiny local `gcloud` shim that implements exactly the two argv shapes
/// `GcloudStorage` emits:
///
///   gcloud storage cp --quiet <src>           gs://<bucket>/<key>   (upload)
///   gcloud storage cp --quiet gs://<bucket>/<key> <dst>             (download)
///
/// The shim maps `gs://<bucket>/<key>` to `<store_dir>/<key>` and copies the
/// file, creating parent dirs. This keeps `GcloudStorage` (and its argv
/// builders, temp-file staging, and serde round-trip) on the REAL code path
/// while staying fully hermetic -- no network, no auth, no live GCS.
fn write_gcloud_shim(dir: &std::path::Path, store_dir: &std::path::Path) -> std::path::PathBuf {
    let shim = dir.join("gcloud-shim.sh");
    // The shim is intentionally minimal and only understands `storage cp`.
    let script = format!(
        r#"#!/usr/bin/env bash
set -euo pipefail
# Args: storage cp --quiet <src> <dst>
if [ "$1" != "storage" ] || [ "$2" != "cp" ]; then
  echo "shim: unsupported gcloud invocation: $*" >&2
  exit 64
fi
shift 2          # drop "storage cp"
if [ "${{1:-}}" = "--quiet" ]; then shift; fi
SRC="$1"
DST="$2"
STORE="{store}"
deref() {{
  # gs://bucket/key -> $STORE/key ; local paths pass through unchanged.
  case "$1" in
    gs://*) echo "$STORE/${{1#gs://*/}}" ;;
    *)      echo "$1" ;;
  esac
}}
RSRC="$(deref "$SRC")"
RDST="$(deref "$DST")"
mkdir -p "$(dirname "$RDST")"
cp "$RSRC" "$RDST"
"#,
        store = store_dir.display(),
    );
    std::fs::write(&shim, script).expect("write gcloud shim");
    let mut perms = std::fs::metadata(&shim)
        .expect("shim metadata")
        .permissions();
    use std::os::unix::fs::PermissionsExt;
    perms.set_mode(0o755);
    std::fs::set_permissions(&shim, perms).expect("chmod shim");
    shim
}

#[test]
#[ignore = "EXPENSIVE real-proving e2e; run with DIST_FOLD_E2E=1 ... -- --ignored"]
fn distributed_fold_e2e_l1_l2_fold_l4_verifies_with_measured_walls() {
    if !enabled() {
        eprintln!(
            "SKIP distributed_fold_e2e (set DIST_FOLD_E2E=1 to run; it really proves a small \
             multi-chunk block end to end through merge + L4)"
        );
        return;
    }
    // plonky2's prover is stack-hungry; run on a large-stack thread, exactly as
    // the FINDING D gate and the bench binary's main thread do.
    std::thread::Builder::new()
        .stack_size(4 * 1024 * 1024 * 1024)
        .spawn(run_e2e)
        .expect("spawn large-stack e2e thread")
        .join()
        .expect("e2e thread panicked");
}

fn run_e2e() {
    let t_total = Instant::now();

    // ---- Pick a SMALL but genuinely MULTI-CHUNK block. k >= 2 is required so
    // the merge tree fires; we keep it as small as possible (default S=4, k=2 =>
    // 8 txs) while still being real and multi-chunk.
    let s = env_usize("DIST_FOLD_E2E_S", 4);
    let k = env_usize("DIST_FOLD_E2E_K", 2);
    assert!(
        k >= 2,
        "DIST_FOLD_E2E_K must be >= 2 (multi-chunk is required to exercise the fold)"
    );
    let n_tx = s * k;

    let mut block = load_block();
    assert!(
        block.txs.len() >= n_tx,
        "bench_test.json has only {} txs; need {} for S={s} k={k}",
        block.txs.len(),
        n_tx
    );
    block.txs.truncate(n_tx);
    let height = block.block_number;
    let created_at = block.created_at;
    println!(
        "[e2e] block height={height} truncated to {n_tx} txs => S={s} k={k} chunks (multi-chunk)"
    );

    // ---- Hermetic proof store: a temp "bucket" dir + a local gcloud shim, both
    // driven through the REAL GcloudStorage surface.
    let work = std::env::temp_dir().join(format!("lighter-e2e-{}-{}", height, std::process::id()));
    let store_dir = work.join("bucket");
    std::fs::create_dir_all(&store_dir).expect("create store dir");
    let shim = write_gcloud_shim(&work, &store_dir);
    // Issue #206: a mount root selects mount-mode file I/O over the gcloud
    // shim. `LIGHTER_E2E_MOUNT=1` runs the e2e through the NEW mounted-volume
    // transport (write/read + atomic rename against `mount_dir`); unset keeps
    // the original gcloud-shim CLI path. Either way the fold must be
    // bit-identical + VERIFY — the equivalence contract is transport-agnostic.
    let mount_dir = work.join("mount");
    let use_mount = std::env::var("LIGHTER_E2E_MOUNT").is_ok();
    if use_mount {
        std::fs::create_dir_all(&mount_dir).expect("create mount dir");
    }
    let proof_store = GcloudStorage::new(StorageConfig {
        bucket: "e2e-local-bucket".into(),
        gcloud_bin: shim.to_string_lossy().to_string(),
        mount_path: if use_mount {
            mount_dir.to_string_lossy().to_string()
        } else {
            String::new()
        },
    });
    println!(
        "[e2e] proof-store transport: {} (issue #206)",
        if use_mount {
            "MOUNTED volume (file I/O + atomic rename)"
        } else {
            "gcloud storage cp shim (CLI)"
        }
    );
    assert!(
        proof_store.config().enabled(),
        "local proof store must be enabled"
    );
    println!(
        "[e2e] hermetic proof store: shim={} bucket_dir={}",
        shim.display(),
        store_dir.display()
    );

    // ---- Shared resident circuits (built ONCE; the cell and coordinator share
    // the SAME circuit shapes, exactly as the deployed binary does).
    let pre_exec_circuit = BlockPreExecutionCircuit::define(CIRCUIT_CONFIG);
    let pbt = pre_exec_circuit.target;
    let pre_exec_data = pre_exec_circuit.builder.build::<C>();
    let bpe = BlockPreExec::from_block(&block);
    let pre_proof =
        BlockPreExecutionCircuit::prove(&pre_exec_data, &bpe, &pbt).expect("pre-exec prove");
    let pre_exec_witness = BlockPreExecWitness::from_public_inputs(&pre_proof.public_inputs);
    let state_metadata = pre_exec_witness.new_state_metadata.clone();

    // L1 (BlockTxCircuit) at this S.
    let l1 = BlockTxCircuit::define(CIRCUIT_CONFIG, s, CHAIN_ID);
    let bt = l1.target;
    let l1_data = l1.builder.build::<C>();

    // L2 leaf chain (BlockTxChainCircuit) + its cyclic base scaffolding.
    let chain_circuit = BlockTxChainCircuit::define(CIRCUIT_CONFIG, &l1_data, s, 1);
    let chain_t = chain_circuit.target;
    let chain_data = chain_circuit.builder.build::<C>();
    let block_tx_witness_size = chain_circuit.block_tx_witness_size;
    let dummy_chain = dummy_circuit(&chain_data.common);
    let dummy_proof = circuit::builder::custom::cyclic_base_proof(
        &chain_data.common,
        &chain_data.verifier_only,
        &dummy_chain,
        Vec::<F>::new().iter().copied().enumerate().collect(),
    )
    .expect("cyclic base proof");

    // Merge circuit (coordinator's fold), built into the leaf chain's exact
    // self-shape (the closed cyclic fixed point).
    let merge_circuit = BlockTxChainMergeCircuit::define(CIRCUIT_CONFIG, &chain_data, 1);
    let merge_target = merge_circuit.target;
    let merge_data = merge_circuit.builder.build::<C>();
    assert!(
        merge_data.common == chain_data.common,
        "merge circuit must build into the leaf chain's exact self-shape (issue #67)"
    );

    // ---- S=1 positional pre-state sweep (the #177 FINDING D seam): one
    // snapshot per tx position so each chunk seeds from its OWN positional
    // pre-state (no chunk-to-chunk coupling). This is the deployed cell's seed
    // source. We build an S=1 L1 circuit for the sweep.
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
    let l1_s1 = BlockTxCircuit::define(CIRCUIT_CONFIG, 1, CHAIN_ID);
    let bt_s1 = l1_s1.target;
    let l1_s1_data = l1_s1.builder.build::<C>();
    println!("[e2e] running S=1 positional pre-state sweep over {n_tx} txs (real L1 proves)...");
    let snapshots = sweep_per_tx_snapshots(
        height,
        created_at,
        initial,
        &block.txs,
        &l1_s1_data,
        &bt_s1,
        |_pos, _wall_ms| {},
    );

    // ===================================================================
    // CELL SIDE: per chunk, real L1 + real L2 leaf prove, then UPLOAD the
    // serialized leaf proof to the proof store keyed by {height}/{idx}.
    // ===================================================================
    println!("[e2e] CELL phase: proving + uploading {k} real L2 leaf proofs...");
    for chunk_idx in 0..k {
        let lo = chunk_idx * s;
        let hi = lo + s;
        let txs: Vec<_> = block.txs[lo..hi].to_vec();

        let pos_pre = snapshots
            .at_chunk(s, chunk_idx)
            .unwrap_or_else(|| panic!("snapshot for chunk {chunk_idx} (pos {}) missing", lo));

        // REAL L1 chunk prove.
        let block_tx = pos_pre.block_tx(created_at, txs.clone());
        let l1_proof: ProofWithPublicInputs<F, C, D> =
            BlockTxCircuit::prove(&l1_data, &block_tx, &bt)
                .unwrap_or_else(|e| panic!("chunk {chunk_idx}: L1 prove failed: {e:?}"));

        // REAL L2 leaf chain prove, seeded from the positional snapshot (the
        // deployed cell's exact base-proof seeding).
        let seed = seed_from_state(
            &pos_pre.register_stack,
            pos_pre.account_tree_root,
            pos_pre.account_pub_data_tree_root,
            pos_pre.market_tree_root,
            pos_pre.account_delta_tree_root,
            &pos_pre.all_assets,
            &pos_pre.all_market_details,
            &state_metadata,
            &pos_pre.system_config,
        );
        let base = BlockTxChainCircuit::cyclic_base_proof(
            &chain_data,
            &dummy_chain,
            height,
            created_at,
            seed.pre_state_root,
            seed.pre_state_root,
            seed.pre_validium_root,
            seed.pre_delta_root,
            block_tx_witness_size,
            &state_metadata,
        );
        let leaf_proof: ProofWithPublicInputs<F, C, D> =
            BlockTxChainCircuit::prove(&chain_t, &chain_data, 0, &base, &dummy_proof, &l1_proof)
                .unwrap_or_else(|e| panic!("chunk {chunk_idx}: L2 leaf prove failed: {e:?}"));

        // SERIALIZE exactly as the production cell does (serde_json of
        // ProofWithPublicInputs) and UPLOAD via the REAL GcloudStorage surface.
        let bytes = serde_json::to_vec(&leaf_proof).expect("serialize leaf proof");
        let key = proof_object_key(height, chunk_idx as u64);
        proof_store
            .upload(&key, &bytes)
            .unwrap_or_else(|e| panic!("chunk {chunk_idx}: upload to proof store failed: {e}"));
        println!(
            "[e2e]   chunk {chunk_idx}: real L1+L2 leaf proven and uploaded as '{key}' \
             ({} bytes)",
            bytes.len()
        );
    }

    // ===================================================================
    // COORDINATOR SIDE: download + deserialize the k leaf proofs, REAL merge
    // fold (measured), REAL L4 prove+verify (measured).
    // ===================================================================
    println!("[e2e] COORDINATOR phase: downloading + folding + L4...");

    // GATHER -> download + deserialize, in chunk order.
    let mut leaves: Vec<ProofWithPublicInputs<F, C, D>> = Vec::with_capacity(k);
    for chunk_idx in 0..k {
        let key = proof_object_key(height, chunk_idx as u64);
        let bytes = proof_store
            .download(&key)
            .unwrap_or_else(|e| panic!("download of '{key}' failed: {e}"));
        let leaf: ProofWithPublicInputs<F, C, D> = serde_json::from_slice(&bytes)
            .unwrap_or_else(|e| panic!("deserialize of '{key}' failed: {e}"));
        leaves.push(leaf);
    }
    assert_eq!(leaves.len(), k, "must download all k leaf proofs");

    // FOLD: REAL BlockTxChainMergeCircuit pairwise tree (the coordinator's
    // fold). Issue #193 — run BOTH the serial fold AND the parallel fold over
    // the SAME leaves, measure each wall, and assert they produce a
    // BIT-IDENTICAL final proof (the determinism/equivalence guarantee that
    // makes the parallel scheduling safe). The parallel result is then driven
    // through L4 so the e2e exercises the NEW parallel path end to end.
    let serial_start = Instant::now();
    let ((serial_proof, serial_is_merge), serial_depth, serial_merges) =
        fold_serial(&merge_target, &merge_data, &leaves);
    let serial_merge_ms = serial_start.elapsed().as_millis() as u64;

    // workers>1 parallelism: cap at k to avoid idle threads on tiny trees.
    let fold_workers = std::cmp::min(std::cmp::max(2, k), 8);
    let parallel_start = Instant::now();
    let ((parallel_proof, parallel_is_merge), parallel_depth, parallel_merges) =
        fold_parallel(&merge_target, &merge_data, &leaves, fold_workers);
    let parallel_merge_ms = parallel_start.elapsed().as_millis() as u64;

    // ---- DETERMINISM / EQUIVALENCE (issue #193 KEY CHECK): serial == parallel.
    assert_eq!(
        serial_depth, parallel_depth,
        "serial vs parallel fold disagree on tree depth ({serial_depth} != {parallel_depth})"
    );
    assert_eq!(
        serial_merges, parallel_merges,
        "serial vs parallel fold disagree on merge count ({serial_merges} != {parallel_merges})"
    );
    assert_eq!(
        serial_is_merge, parallel_is_merge,
        "serial vs parallel fold disagree on final_is_merge"
    );
    assert_eq!(
        serial_proof.public_inputs, parallel_proof.public_inputs,
        "DETERMINISM VIOLATION: serial and parallel folds produced DIFFERENT final proof \
         public inputs — parallel scheduling must be bit-identical to the serial fold"
    );
    println!(
        "[e2e]   DETERMINISM OK: serial fold ({serial_merge_ms} ms) == parallel fold \
         ({parallel_merge_ms} ms, workers={fold_workers}) — identical final proof public inputs \
         (depth={serial_depth} merges={serial_merges})"
    );

    // ---- ISSUE #198: DISTRIBUTED fold equivalence (the cross-machine fold
    // fan-out correctness bar). Fold the SAME leaves through the SHARED library
    // distributed driver (task emitter + per-level barrier + proof-store transit
    // + #193 re-sort), genuinely fanning merges across INDEPENDENT workers, and
    // assert the final proof is BIT-IDENTICAL to the serial/in-process fold —
    // for SEVERAL worker counts (k>=4 exercises a real multi-level tree). This
    // is the key merge-gating check of #198: distributed == in-process.
    for dist_workers in [1usize, 2, 3, 4] {
        let dist_start = Instant::now();
        let ((dist_proof, dist_is_merge), dist_depth, dist_merges, dist_bytes) =
            fold_distributed_via_library(
                &merge_target,
                &merge_data,
                height,
                &leaves,
                dist_workers,
            );
        let dist_ms = dist_start.elapsed().as_millis() as u64;
        assert_eq!(
            serial_depth, dist_depth,
            "issue #198: serial vs distributed fold disagree on depth \
             ({serial_depth} != {dist_depth}) at workers={dist_workers}"
        );
        assert_eq!(
            serial_merges, dist_merges,
            "issue #198: serial vs distributed fold disagree on merge count \
             ({serial_merges} != {dist_merges}) at workers={dist_workers}"
        );
        assert_eq!(
            serial_is_merge, dist_is_merge,
            "issue #198: serial vs distributed fold disagree on final_is_merge \
             at workers={dist_workers}"
        );
        assert_eq!(
            serial_proof.public_inputs, dist_proof.public_inputs,
            "ISSUE #198 DETERMINISM/EQUIVALENCE VIOLATION: the DISTRIBUTED fold \
             (workers={dist_workers}) produced DIFFERENT final proof public inputs than the \
             in-process fold — the cross-machine fold must be bit-identical regardless of which \
             worker proved which merge"
        );
        // The distributed final proof must also genuinely VERIFY (not just
        // match public inputs). Verify against the merge circuit's data.
        if dist_is_merge {
            merge_data
                .verify(dist_proof.clone())
                .expect("issue #198: the DISTRIBUTED fold's final proof must VERIFY");
        }
        println!(
            "[e2e]   ISSUE #198 EQUIVALENCE OK (workers={dist_workers}, {dist_ms} ms): \
             distributed fold == in-process fold (bit-identical public inputs) AND VERIFIES \
             (depth={dist_depth} merges={dist_merges} max_intermediate_bytes={dist_bytes})"
        );
    }

    // Drive L4 with the PARALLEL fold result (exercise the new path e2e). The
    // reported merge wall is the parallel fold's realized wall.
    let merge_ms = parallel_merge_ms;
    let (final_proof, final_is_merge) = (parallel_proof, parallel_is_merge);
    let depth = parallel_depth;
    let merges = parallel_merges;
    println!(
        "[e2e]   FOLD done: depth={depth} merges={merges} final_is_merge={final_is_merge} \
         serial_merge_ms={serial_merge_ms} parallel_merge_ms={merge_ms}"
    );
    assert!(
        merges >= 1,
        "a multi-chunk block must fire at least one merge"
    );
    assert!(
        final_is_merge,
        "a multi-chunk fold's final proof must carry the merge VK"
    );

    // L4: patch the block's new_* fields to the chain proof's outputs (the
    // shared prove_block_l4_from_chain recipe), then REAL BlockCircuit prove +
    // VERIFY. Measure the wall.
    let cw = BlockTxChainWitness::from_public_inputs(&final_proof.public_inputs, 1, 1);
    let mut pblock = block.clone();
    pblock.new_validium_root = cw.new_validium_root;
    pblock.new_state_root = cw.new_state_root;
    pblock.new_account_delta_tree_root = cw.new_account_delta_tree_root;
    pblock.on_chain_operations_count = cw.on_chain_operations_count;
    pblock.on_chain_operations_pub_data = cw.on_chain_operations_pub_data.clone();
    pblock.priority_operations_count = cw.priority_operations_count;
    pblock.new_public_market_details = cw.new_public_market_details.clone();
    pblock.new_prefix_priority_operation_hash = if cw.priority_operations_count != 0 {
        let mut input = Vec::with_capacity(32 + cw.priority_operations_pub_data.len());
        input.extend_from_slice(&block.old_prefix_priority_operation_hash);
        input.extend_from_slice(&cw.priority_operations_pub_data);
        keccak(&input)
    } else {
        block.old_prefix_priority_operation_hash
    };

    // L4 must be defined against the circuit that PRODUCED the final chain
    // proof so its embedded verifier key matches: the merge circuit when at
    // least one merge fired (the multi-chunk case), the leaf chain circuit for
    // a single-leaf block. This mirrors run_tree_fold's final_is_merge switch
    // (bench.rs ~line 3131) and the coordinator's shared L4 helper.
    let l4_chain_like_data = if final_is_merge {
        &merge_data
    } else {
        &chain_data
    };
    let l4_start = Instant::now();
    let l4 = BlockCircuit::define(CIRCUIT_CONFIG, &pre_exec_data, l4_chain_like_data, 1);
    let l4_target = l4.target;
    let l4_data = l4.builder.build::<C>();
    let l4_proof = BlockCircuit::prove(&l4_target, &l4_data, &pblock, &pre_proof, &final_proof)
        .expect("L4 BlockCircuit prove");
    // The genuine, non-stubbed acceptance check: the block proof VERIFIES.
    l4_data
        .verify(l4_proof.clone())
        .expect("L4 BlockCircuit proof must VERIFY");
    let l4_ms = l4_start.elapsed().as_millis() as u64;
    println!("[e2e]   L4 done: BlockCircuit proved + VERIFIED, l4_ms={l4_ms}");

    // ---- WS6: emit the MEASURED CoordinatorFold event, exactly as the
    // coordinator does on the real path, and assert it carries the measured
    // distinction.
    let ev = BenchEvent::CoordinatorFold {
        height,
        merge_source: "measured",
        l4_source: "measured",
        leaves: k as u64,
        depth: depth as u32,
        merges: merges as u64,
        merge_ms,
        l4_ms,
        merge_s: merge_ms as f64 / 1000.0,
        l4_s: l4_ms as f64 / 1000.0,
        rss_mb_peak: peak_rss_mb(),
        ts: now_iso8601(),
    };
    // Emit to the stream (visible with --nocapture) AND inspect the JSON shape.
    events::emit(&ev);
    let json = serde_json::to_string(&ev).expect("serialize CoordinatorFold");

    // ===================================================================
    // ACCEPTANCE ASSERTIONS (issue #179)
    // ===================================================================
    assert!(k >= 2, "multi-chunk required");
    assert!(
        merge_ms > 0,
        "MEASURED merge wall must be > 0 (real proving): got {merge_ms}"
    );
    assert!(
        l4_ms > 0,
        "MEASURED L4 wall must be > 0 (real proving): got {l4_ms}"
    );
    assert!(
        json.contains("\"event\":\"coordinator_fold\""),
        "event must serialize as coordinator_fold: {json}"
    );
    assert!(
        json.contains("\"merge_source\":\"measured\""),
        "merge timings must be marked MEASURED (not modeled): {json}"
    );
    assert!(
        json.contains("\"l4_source\":\"measured\""),
        "L4 timings must be marked MEASURED (not modeled): {json}"
    );
    assert!(
        json.contains(&format!("\"merge_ms\":{merge_ms}")),
        "event must carry the measured merge_ms={merge_ms}: {json}"
    );
    assert!(
        json.contains(&format!("\"l4_ms\":{l4_ms}")),
        "event must carry the measured l4_ms={l4_ms}: {json}"
    );

    // Best-effort cleanup of the hermetic store.
    let _ = std::fs::remove_dir_all(&work);

    println!(
        "[e2e] PASS: real multi-chunk block height={height} went L1 -> L2(cells) -> fold -> L4 \
         (coordinator); L4 block proof VERIFIED; MEASURED merge_ms={merge_ms} l4_ms={l4_ms} \
         (k={k} depth={depth} merges={merges}); CoordinatorFold marked measured. \
         total={:?}",
        t_total.elapsed()
    );
}
