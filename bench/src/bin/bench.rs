// Copyright (c) Elliot Technologies, Inc.
// SPDX-License-Identifier: BUSL-1.1

#![feature(stmt_expr_attributes)]
#![allow(unused_imports)]

use std::fs;
use std::path::Path;
use std::time::{Duration, Instant};

use bench::events::{self, BenchEvent, cpu_time_ms, current_rss_mb, now_iso8601, peak_rss_mb};
use bench::l5segment::{Rolling, chain_next_block, host_prepass, segment_split_points};
use bench::prestate::{ChunkPreState, PreStateSnapshots, sweep_per_tx_snapshots};
use bench::seed::{ChunkSeed, seed_from_state};
use bench::{blob_encode, kzg, l6drive};
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
use circuit::recursion::batch::{Batch, SegmentInfo};
use circuit::recursion::batch_merge_constraints::{BatchMergeCircuit, Circuit as _};
use circuit::recursion::cyclic_circuit::{Circuit as _, CyclicRecursionCircuit};
use circuit::recursion::wrapper_circuit::NUM_CHAINS_PER_BATCH;
use circuit::tx;
use circuit::types::asset::Asset;
use circuit::types::config::{C, CIRCUIT_CONFIG, D, F};
use circuit::types::constants::*;
use circuit::types::market_details::MarketDetails;
use circuit::types::register::RegisterStack;
use circuit::types::state_metadata::{STATE_METADATA_SIZE, StateMetadata};
use circuit::types::system_config::SystemConfig;
use circuit::types::{account_delta, state_metadata};
use clap::{Parser, ValueEnum};
use env_logger::{Builder, DEFAULT_FILTER_ENV, Env, try_init_from_env};
use log::{Level, LevelFilter, Log, Metadata, Record, debug, info};
use plonky2::field::goldilocks_field::GoldilocksField;
use plonky2::field::types::{Field, Field64, PrimeField64};
use plonky2::hash::hash_types::HashOut;
use plonky2::plonk::circuit_data::CircuitData;
use plonky2::plonk::proof::{CompressedProofWithPublicInputs, ProofWithPublicInputs};
use plonky2::recursion::dummy_circuit::{self, dummy_circuit};
use rayon::prelude::*;
use rayon::vec;

const DEFAULT_TX_PER_PROOF: usize = 4;
const DEFAULT_TX_LIMIT: usize = 480;
const CHAIN_ID: u32 = 304;

/// Lighter prover benchmark.
///
/// Runs the full per-chunk tx-proof + chain-recursion pipeline against
/// `bench_test.json`, configurable for chunk-size sweeps.
#[derive(Parser, Debug, Clone)]
#[command(name = "bench", about, long_about = None)]
struct Args {
    /// Run mode (issue #172 — the GENUINE distributed prover entrypoint).
    ///
    /// - `bench` (default): the historical single-process pipeline — every
    ///   existing flag/behavior is unchanged and fully additive.
    /// - `coordinator`: the OUTER + INNER dispatch tier (ADR-0006 §1.1/§1.2).
    ///   Pulls block jobs from a real Pub/Sub dispatch subscription, SPLITs
    ///   each block into `k = ceil(tx/S)` chunks, fans chunk REFERENCES out
    ///   to cells over a chunk-dispatch topic, collects chunk results from a
    ///   results subscription, and emits per-block completion + lag events.
    /// - `cell`: the leaf prover (ADR-0006 §2 / ADR-0008). Pulls chunk
    ///   references from the chunk subscription, resolves each witness slice
    ///   via the mounted corpus (timing the real `witness_fetch_ms`), runs the
    ///   REAL L1+L2 prove, and reports the result back over the results topic.
    ///
    /// `coordinator` / `cell` are SEPARATE PODS coordinating over Pub/Sub —
    /// not a single-machine simulation. See docs/distributed-prover-runtime.md.
    #[arg(long, value_enum, env = "LIGHTER_MODE", default_value_t = RunMode::Bench)]
    mode: RunMode,

    /// GCP project that owns the Pub/Sub topics/subscriptions. Required for
    /// `--mode coordinator|cell`.
    #[arg(long, env = "LIGHTER_PROJECT")]
    project: Option<String>,

    /// Pub/Sub dispatch (block) subscription — coordinators competing-pull.
    #[arg(long, env = "LIGHTER_DISPATCH_SUBSCRIPTION")]
    dispatch_subscription: Option<String>,

    /// Pub/Sub dispatch (block) topic (the feeder publishes blocks here).
    #[arg(long, env = "LIGHTER_DISPATCH_TOPIC")]
    dispatch_topic: Option<String>,

    /// Pub/Sub chunk-dispatch topic — coordinator publishes chunk refs here.
    #[arg(long, env = "LIGHTER_CHUNK_TOPIC")]
    chunk_topic: Option<String>,

    /// Pub/Sub chunk-dispatch subscription — cells competing-pull chunk refs.
    #[arg(long, env = "LIGHTER_CHUNK_SUBSCRIPTION")]
    chunk_subscription: Option<String>,

    /// Pub/Sub results topic — cells publish chunk results here.
    #[arg(long, env = "LIGHTER_RESULTS_TOPIC")]
    results_topic: Option<String>,

    /// Pub/Sub results subscription — coordinator pulls chunk results.
    #[arg(long, env = "LIGHTER_RESULTS_SUBSCRIPTION")]
    results_subscription: Option<String>,

    /// `gcloud` binary path for the Pub/Sub transport (default `gcloud`).
    #[arg(long, env = "LIGHTER_GCLOUD_BIN", default_value = "gcloud")]
    gcloud_bin: String,

    /// Distributed modes: how many blocks the coordinator proves before
    /// exiting, OR how many chunks the cell proves before exiting. `0` =
    /// run forever (until SIGINT/SIGTERM). Bounded values make a single
    /// pod do a finite unit of real work then exit cleanly — useful for
    /// smoke / one-shot benchmark jobs.
    #[arg(long, env = "LIGHTER_MAX_UNITS", default_value_t = 0)]
    max_units: u64,

    /// Distributed modes: seconds to sleep between empty pulls (backoff when
    /// the queue is drained). Keeps a long-lived pod from hot-spinning.
    #[arg(long, env = "LIGHTER_POLL_INTERVAL_S", default_value_t = 2)]
    poll_interval_s: u64,

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

    /// Issue #72 (cell slice A): order in which the tree-fold driver
    /// proves LEAF chain proofs. `forward` (default) preserves today's
    /// 0..N order; `reverse` proves N-1..0 to demonstrate that the
    /// sequential seeding seam has been removed (leaves no longer depend
    /// on the previous leaf's proven outputs). Tree-fold only.
    #[arg(long, value_enum, default_value_t = LeafOrder::Forward)]
    leaf_order: LeafOrder,

    /// Issue #73 (cell slice B): intra-cell parallel tree scheduler.
    /// `M` worker threads share the resident `CircuitData` (by reference --
    /// it is Send + Sync and immutable after build, so no Arc clone is
    /// needed) and prove leaves/merges concurrently. Default `1` keeps the
    /// historical sequential driver byte-for-byte (zero regression). M > 1
    /// builds a dedicated rayon thread pool of M workers; leaves and
    /// per-level merges are dispatched into that pool, realizing the
    /// critical-path latency PR #69's sequential bench only reports.
    /// Tree-fold only.
    ///
    /// Open question (issue #73, ADR-0003 §D1): plonky2 already saturates
    /// all cores per proof via the global rayon pool, so M concurrent
    /// proves contend for cores. The sweep M ∈ {1,2,4,8,16} measures the
    /// real M / wall-clock curve; the `l2_tree_schedule` event in the
    /// JSONL stream is the headline.
    #[arg(long, default_value_t = 1)]
    l2_workers: usize,

    /// Issue #78: run the 8-way L5 (`CyclicRecursionCircuit`) segment
    /// scheduler. Synthesizes a `--blocks` continuation-consistent block
    /// sequence from `bench_test.json`, splits it into `--segments`
    /// independent segment chains, computes each segment's starting
    /// on-chain-operations keccak prefix on the host (prove-free), then
    /// folds the segments' per-block L4 proofs into running L5 proofs IN
    /// PARALLEL across segments (rayon). Every resulting segment proof is
    /// L5-verified. Emits a `l5_segment_batch` event with the parallel
    /// `effective_ms_per_block` headline. Batch mode only. The ≤200 ms/block
    /// acceptance is a hardware measurement gate (#10 EPYC baseline); this
    /// flag delivers the instrument and proves it functionally at small
    /// scale. The verifying L6 termination is gated on issue #83.
    #[arg(long, default_value_t = false)]
    l5_segment_check: bool,

    /// Issue #78: number of parallel L5 segment chains (`1..=8`, the
    /// wrapper's `NUM_CHAINS_PER_BATCH`). Only meaningful with
    /// `--l5-segment-check`.
    #[arg(long, default_value_t = 8)]
    segments: usize,

    /// Issue #78: number of synthesized blocks to schedule across the
    /// segments. Must be `>= --segments`. Only meaningful with
    /// `--l5-segment-check`. Keep small (e.g. 4) for a tractable smoke run;
    /// real proving is ~0.94 s/fold.
    #[arg(long, default_value_t = 64)]
    blocks: usize,

    /// L5 fold strategy (issue #82). `serial`: today's L5 cyclic linked-list
    /// fold (default; zero behavior change). `tree`: build the pre-L5
    /// `BatchMergeCircuit`, assert its self-shape matches the L5 cyclic
    /// circuit (`merge.common == l5.common`), and wire the host-level
    /// pairwise tree-fold of L5 `Batch` proofs (build-validated). Batch mode
    /// only. The LIVE timed >=4-leaf prove on EPYC hardware is a documented
    /// follow-up run (see the PR), so this path does NOT execute a full L5
    /// prove in-workspace.
    #[arg(long, value_enum, default_value_t = L5FoldMode::Serial)]
    l5_fold: L5FoldMode,

    /// L5 tree mode only (issue #82): after wiring the tree fold, also run the
    /// L5 serial fold over the same per-block batches and assert element-wise
    /// equality of the two roots' semantic public inputs (the `Batch` +
    /// `SegmentInfo` PI surface, excluding the trailing verifier-key PIs which
    /// differ by construction: L5 leaf VK vs merge VK).
    #[arg(long, default_value_t = false)]
    l5_ab_check: bool,

    /// Issue #83: drive the standalone cyclic-delta prove path. Builds
    /// `DeltaCircuit`, proves one (empty, correctly-shaped synthesized) delta
    /// leaf, folds it through `CyclicDeltaCircuit`, and verifies the resulting
    /// `delta_chain_proof`. Batch mode only. (Acceptance criterion #1.)
    #[arg(long, default_value_t = false)]
    delta_prove: bool,

    /// Issue #83: drive the standalone blob-evaluation prove path. Encodes a
    /// correctly-shaped synthesized blob, computes the KZG versioned hash +
    /// custom-Poseidon2 PCE opening (x, y) via the off-circuit sidecar, builds
    /// `BlobEvaluationCircuit`, proves, and verifies the `blob_evaluation_proof`.
    /// Batch mode only. (Acceptance criteria #2 and #3.)
    #[arg(long, default_value_t = false)]
    blob_prove: bool,

    /// Issue #83: drive the end-to-end L6 inner-wrapper prove path. Produces 8
    /// L5 chain proofs + `delta_chain_proof` + `blob_evaluation_proof` + the KZG
    /// `WrapperInput`, calls `WrapperCircuit::prove_inner`, and verifies the
    /// resulting inner-wrapper proof over a correctly-shaped synthesized batch.
    /// Batch mode only; the heaviest path (gated as a bench mode, not a unit
    /// test). (Acceptance criterion #4.)
    #[arg(long, default_value_t = false)]
    l6_inner: bool,

    /// Issue #116: drive the full inner -> outer wrapper chain. Runs the same
    /// `--l6-inner` pipeline to produce + verify the inner-wrapper proof, then
    /// CONTINUES into the outer stage: builds the outer-wrapper circuit via
    /// `WrapperCircuit::define_outer` (BN128 config), calls the previously
    /// uncalled `WrapperCircuit::prove_outer`, and verifies the resulting
    /// outer-wrapper proof — the conversion toward the Ethereum-friendly form.
    /// Batch mode only; heaviest path. (Issue #116.)
    #[arg(long, default_value_t = false)]
    l6_outer: bool,

    /// Issue #117: when set with `--l6-outer`, serialize the verified outer
    /// proof + the outer circuit's common/verifier data to JSON in this
    /// directory, in the schema the gnark bridge consumes
    /// (`types.ReadProofWithPublicInputs` / `ReadCommonCircuitData` /
    /// `ReadVerifierOnlyCircuitData`). These are the inputs to the gnark
    /// `plonk.Prove` final-proof path. The real proof is serialized as-is; no
    /// field is fabricated.
    #[arg(long)]
    l6_outer_export: Option<std::path::PathBuf>,

    /// Issue #83: path to the public Ethereum KZG ceremony trusted setup used by
    /// `--blob-prove` / `--l6-inner` / `--l6-outer` to derive the blob's KZG
    /// versioned hash.
    #[arg(long, default_value = bench::kzg::DEFAULT_TRUSTED_SETUP_PATH)]
    trusted_setup_path: String,

    /// Issue #157 (spike): emit per-chunk tx-type attribution
    /// (`tx_types`, `chunk_tx_type_homogeneous`) on the L1/L2
    /// `layer_prove` events. Tx order is NOT changed -- this just
    /// annotates the existing arrival-order chunks. When the sample
    /// happens to produce homogeneous chunks for a type (always true
    /// at `--tx-per-proof 1`; opportunistic at larger chunk sizes),
    /// the per-type cost can be isolated by filtering events on the
    /// homogeneity tag without breaking witness consistency. Default
    /// `false` keeps the JSON shape byte-identical to pre-#157.
    /// Serial-fold batch path only. Implied by `--group-by-tx-type`.
    #[arg(long, default_value_t = false)]
    attribute_tx_type: bool,

    /// Issue #157 (spike): stable-sort `block.txs` by `tx_type` before
    /// chunking. Implies `--attribute-tx-type`. With this flag, chunks
    /// become type-homogeneous (modulo boundary chunks that straddle
    /// two types). WARNING: re-ordering txs breaks chain-validity --
    /// the L1 witness for some tx types asserts cross-tx state
    /// (Merkle-touch / register-stack constraints) that the unsorted
    /// chain established, so prove can panic with a partition-set
    /// conflict on certain types/positions (see issue #159 for the
    /// root-cause investigation). When the fixture does support
    /// sorting, this is the cleanest per-type isolation. Default
    /// `false` keeps arrival order and produces JSON byte-identical
    /// to pre-#157. Serial-fold batch path only.
    #[arg(long, default_value_t = false)]
    group_by_tx_type: bool,
}

/// Issue #172: run mode. `Bench` is the historical single-process pipeline
/// (default, byte-for-byte unchanged). `Coordinator` and `Cell` are the
/// genuine distributed roles that coordinate over real Pub/Sub as separate
/// GKE pods (ADR-0006 §1.1/§1.2/§2; ADR-0008).
#[derive(Clone, Copy, PartialEq, Eq, Debug, ValueEnum)]
enum RunMode {
    Bench,
    Coordinator,
    Cell,
}

/// Issue #67: L2 fold strategy.
#[derive(Clone, Copy, PartialEq, Eq, Debug, ValueEnum)]
enum L2FoldMode {
    Serial,
    Tree,
}

/// Issue #72: tree-fold leaf-proving order. The default `Forward` order
/// keeps the historical 0..N traversal; `Reverse` exists purely as an
/// acceptance check that the witness-native seeding has decoupled leaf k
/// from leaf k-1's proven outputs.
#[derive(Clone, Copy, PartialEq, Eq, Debug, ValueEnum)]
enum LeafOrder {
    Forward,
    Reverse,
}

/// Issue #82: L5 fold strategy (pre-L5 block-proof aggregation).
#[derive(Clone, Copy, PartialEq, Eq, Debug, ValueEnum)]
enum L5FoldMode {
    Serial,
    Tree,
}

/// Issue #73: a tree-fold node = (proof, is_merge). `is_merge` selects
/// the conditional-VK verifier slot in `BlockTxChainMergeCircuit`
/// (leaf VK if false, merge VK if true).
type TreeNode = (ProofWithPublicInputs<F, C, D>, bool);

/// Issue #73: a single merge pair at one tree level. `None` on the
/// right child marks an odd-count carry-up: the left child is promoted
/// to the next level unchanged.
type MergePair = (TreeNode, Option<TreeNode>);

/// Issue #73: result of attempting one merge pair. `Some(wall_ms)`
/// when a merge actually fired; `None` for an odd carry-up (no merge
/// circuit was run, so no per-node timing is recorded).
type PairResult = (TreeNode, Option<u64>);

/// Issue #73: result of proving one leaf chain proof. Returned by
/// `prove_leaf` and collected in deterministic order by Phase 2's
/// parallel + serial paths. Tuple layout:
/// `(index, leaf_proof, base_proof_duration, full_leaf_duration)`.
type LeafResult = (usize, ProofWithPublicInputs<F, C, D>, Duration, Duration);

fn main() {
    init_logger_no_warn();

    let args = Args::parse();

    // Hard cap raised 32 -> 64 for the per-machine calibration suite
    // (issue #85): probing the 2^20 degree bracket needs S around 40,
    // which only fits machines with >=48 GB RAM (projected ~32 GB peak
    // RSS plus headroom -- see issue #60's bracket table). Values in
    // 33..=64 are accepted with a loud warning; 1..=32 remain the
    // validated range from issues #60/#63.
    const VALIDATED_MAX_TX_PER_PROOF: usize = 32;
    const MAX_TX_PER_PROOF: usize = 64;
    if args.tx_per_proof > MAX_TX_PER_PROOF {
        eprintln!(
            "error: --tx-per-proof {} exceeds the maximum of {}.\n\
             \n\
             Chunk sizes 1..=32 are validated (building and proving) following\n\
             the log_gates / ExponentiationGate fix from issue #63, with sweep\n\
             measurements recorded on issue #60. Sizes 33..=64 are accepted\n\
             (with a warning) for the 2^20-bracket calibration probes from\n\
             issue #85. Values above 64 have never been attempted and are\n\
             refused outright.\n\
             \n\
             See https://github.com/kunallimaye/lighter-prover/issues/63 for the\n\
             root-cause analysis and fix details.",
            args.tx_per_proof, MAX_TX_PER_PROOF
        );
        std::process::exit(2);
    }
    if args.tx_per_proof > VALIDATED_MAX_TX_PER_PROOF {
        eprintln!(
            "warning: --tx-per-proof {} is above the validated maximum of {}.\n\
             Chunk sizes 33..=64 land in the 2^20 degree bracket with a projected\n\
             peak RSS of ~32 GB (issue #60 bracket table; unmeasured until the\n\
             issue #85 calibration runs). Expect a long circuit build and make\n\
             sure this machine has >=48 GB of free RAM. See issues #60 and #63.",
            args.tx_per_proof, VALIDATED_MAX_TX_PER_PROOF
        );
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
        if args.l5_segment_check {
            eprintln!("error: --l5-segment-check is batch-mode only (issue #78); drop --stream");
            std::process::exit(2);
        }
        if args.delta_prove || args.blob_prove || args.l6_inner || args.l6_outer {
            eprintln!(
                "error: --delta-prove/--blob-prove/--l6-inner/--l6-outer are batch-mode only \
                 (issues #83/#116); drop --stream"
            );
            std::process::exit(2);
        }
    }
    if args.ab_check && args.l2_fold != L2FoldMode::Tree {
        eprintln!("error: --ab-check requires --l2-fold tree");
        std::process::exit(2);
    }
    if args.leaf_order != LeafOrder::Forward && args.l2_fold != L2FoldMode::Tree {
        eprintln!("error: --leaf-order requires --l2-fold tree");
        std::process::exit(2);
    }
    if args.l2_workers == 0 {
        eprintln!("error: --l2-workers must be > 0");
        std::process::exit(2);
    }
    if args.l2_workers > 1 && args.l2_fold != L2FoldMode::Tree {
        eprintln!(
            "error: --l2-workers > 1 requires --l2-fold tree (issue #73; the parallel scheduler \
             dispatches leaves and merges across M worker threads, which only makes sense in the \
             tree-fold driver)"
        );
        std::process::exit(2);
    }
    if args.l5_fold == L5FoldMode::Tree && args.stream {
        eprintln!("error: --l5-fold tree is batch-mode only (issue #82); drop --stream");
        std::process::exit(2);
    }
    if args.l5_fold == L5FoldMode::Tree && args.l2_fold == L2FoldMode::Tree {
        eprintln!(
            "error: --l5-fold tree and --l2-fold tree are separate driver paths (issue #82); \
             run them one at a time"
        );
        std::process::exit(2);
    }
    if args.l5_ab_check && args.l5_fold != L5FoldMode::Tree {
        eprintln!("error: --l5-ab-check requires --l5-fold tree");
        std::process::exit(2);
    }
    if args.l5_fold == L5FoldMode::Tree && args.l5_segment_check {
        eprintln!(
            "error: --l5-fold tree (issue #82) and --l5-segment-check (issue #78) are separate \
             L5 driver paths; run them one at a time"
        );
        std::process::exit(2);
    }

    // Issue #78 + #94: validate the L5 segment scheduler knobs up-front.
    // The upper bound is the wrapper's NUM_CHAINS_PER_BATCH (8); blocks
    // must fill at least one block per segment; and the total tx budget
    // `blocks * tx_per_block` (where tx_per_block = tx_per_proof, one L1
    // chunk per chained block per #94's recipe) must fit within the
    // fixture's DEFAULT_TX_LIMIT ceiling (480 of the 500-tx fixture).
    if args.l5_segment_check {
        if args.segments < 1 || args.segments > 8 {
            eprintln!(
                "error: --segments ({}) must be in 1..=8 (the wrapper's NUM_CHAINS_PER_BATCH)",
                args.segments
            );
            std::process::exit(2);
        }
        if args.blocks < args.segments {
            eprintln!(
                "error: --blocks ({}) must be >= --segments ({}); each segment needs at least one block",
                args.blocks, args.segments
            );
            std::process::exit(2);
        }
        let tx_per_block = args.tx_per_proof;
        let total_txs = args.blocks.saturating_mul(tx_per_block);
        if total_txs > DEFAULT_TX_LIMIT {
            eprintln!(
                "error: --blocks ({}) * tx_per_block ({}) = {} exceeds the fixture ceiling \
                 DEFAULT_TX_LIMIT ({}); the tx-slicing chained-block recipe (#94) needs a \
                 disjoint tx window per block. Reduce --blocks or --tx-per-proof.",
                args.blocks, tx_per_block, total_txs, DEFAULT_TX_LIMIT
            );
            std::process::exit(2);
        }
    }
    // Issue #94: the same `blocks * tx_per_block <= DEFAULT_TX_LIMIT`
    // bound applies to the `--l5-fold tree` path because it now consumes
    // the real chained fixture rather than the synthetic-batches stub.
    if args.l5_fold == L5FoldMode::Tree {
        let tx_per_block = args.tx_per_proof;
        let total_txs = args.blocks.saturating_mul(tx_per_block);
        if total_txs > DEFAULT_TX_LIMIT {
            eprintln!(
                "error: --blocks ({}) * tx_per_block ({}) = {} exceeds the fixture ceiling \
                 DEFAULT_TX_LIMIT ({}); --l5-fold tree consumes the same chained-block \
                 fixture as --l5-segment-check (#94). Reduce --blocks or --tx-per-proof.",
                args.blocks, tx_per_block, total_txs, DEFAULT_TX_LIMIT
            );
            std::process::exit(2);
        }
    }

    log_machine_metadata(&args);

    // Issue #172: the genuine distributed roles. Each is a SEPARATE POD
    // coordinating over real Pub/Sub (ADR-0006 §1.1/§1.2/§2; ADR-0008). They
    // branch off here before any single-process fixture flow. The tx-per-proof
    // validation above still applies (cells/coordinators prove real chunks).
    match args.mode {
        RunMode::Bench => {} // fall through to the historical pipeline
        RunMode::Coordinator => {
            run_coordinator(&args);
            return;
        }
        RunMode::Cell => {
            run_cell(&args);
            return;
        }
    }

    if args.stream {
        run_stream(&args);
        return;
    }

    // Issue #83: the L6 drive modes synthesize their own correctly-shaped batch
    // (delta chain, blob evaluation, inner wrapper) and do not consume the
    // bench_test.json fixture, so they branch off here before it is loaded.
    if args.delta_prove {
        run_delta_prove(&args);
        return;
    }
    if args.blob_prove {
        run_blob_prove(&args);
        return;
    }
    if args.l6_inner {
        run_l6_inner(&args);
        return;
    }
    if args.l6_outer {
        run_l6_outer(&args);
        return;
    }

    let mut block = get_test_block_json_file("bench_test.json");

    // Issue #157 (spike): re-group the block's existing real txs by
    // `tx_type` so each per-chunk L1/L2 prove operates on a
    // type-homogeneous chunk (modulo boundary chunks where the sort
    // straddles two types). Stable sort preserves arrival order within
    // a type so the per-type wall measurement is comparable to the
    // pre-#157 arrival-order baseline at the limit of a 1-type batch.
    //
    // SAFETY NOTE: the unsorted serial fold relies on `block.txs` being
    // in chain-valid order so the running state (assets, account roots,
    // ...) threaded through `BlockTx::*_before` matches each chunk's
    // pre-state. Sorting BREAKS that chain-validity: the L1 prove is
    // independent per-chunk and may or may not panic depending on which
    // circuit constraints reference cross-tx state; the L2 chain prove
    // will likely fail to verify (state mismatch) but its wall-clock
    // measurement is still informative because the work IS performed
    // before the verifier rejects. The spike accepts this risk -- the
    // verdict needs L1 wall per type, not chain validity. If L1 itself
    // panics on this fixture, the bench will crash and the spike will
    // fall back to the homogeneous-span approach (see issue #157).
    if args.group_by_tx_type {
        block.txs.sort_by_key(|t| t.tx_type);
    }
    let block = block;

    // Issue #78: the L5 segment scheduler builds its own L1..L4 pipeline and
    // multi-block fixture, so it branches off here (like --stream) before the
    // single-block serial/tree batch flow.
    if args.l5_segment_check {
        run_l5_segment_check(&args, &block);
        return;
    }

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
        tx_types: None,
        chunk_tx_type_homogeneous: None,
        witness_fetch_ms: None,
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

    // Issue #82 + #94: pre-L5 block-proof aggregation tree-fold. Lives-proves
    // a >=4-leaf tree on the genuinely state-chained fixture built by
    // `build_chained_blocks_and_l4_proofs` (#94), using the merged PR #96
    // (`BatchMergeCircuit::generate_witness`) fix.
    if args.l5_fold == L5FoldMode::Tree {
        run_l5_tree_fold(
            &args,
            &block,
            &data,
            &bt,
            &pre_exec_data,
            &pbt,
            &chain_circuit_data,
            &chain_circuit_t,
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

        // Issue #157 (spike): per-chunk tx-type attribution -- only when
        // the caller opted in via `--attribute-tx-type` or its implier
        // `--group-by-tx-type`. `None` keeps the pre-#157 JSON shape
        // byte-identical.
        let (tx_types_attr, homogeneous_attr) = if args.attribute_tx_type || args.group_by_tx_type {
            chunk_tx_type_attribution(tx)
        } else {
            (None, None)
        };

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
            tx_types: tx_types_attr.clone(),
            chunk_tx_type_homogeneous: homogeneous_attr,
            witness_fetch_ms: None,
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
            // Issue #157 (spike): same per-chunk tx-type attribution as
            // the L1 emit just above; the L2 chain prove operates on the
            // same chunk, so it gets the same attribution.
            tx_types: tx_types_attr,
            chunk_tx_type_homogeneous: homogeneous_attr,
            witness_fetch_ms: None,
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

    // ---- Witness plane (ADR-0008 §1.4 k=1 mounted corpus; #61) ----
    //
    // Build a LOCAL mounted read-only corpus keyed by `{height,
    // witness_index}` (ADR-0008 §1.1). This is the k=1 degenerate case: a
    // single whole block (`bench_test.json`, height = block.block_number)
    // mounted on local disk, partitioned into `pool_total` `S`-tx slices
    // indexed `0..pool_total` (ADR-0008 §1.4 -- "today's `bench_test.json`").
    // The resolver payload is just the slice's pool index; the prove path
    // still reads the real txs from `pool` (the resolve MODELS the
    // `{height, witness_index}` -> witness-bytes lookup whose wall is the
    // `witness_fetch_ms` seam, ADR-0008 §2.1). Dispatch carries the
    // REFERENCE `{height, witness_index}`, not the bytes (ADR-0008 §1.2).
    let corpus_height: u64 = block.block_number;
    let witness_corpus: bench::conductor::MountedCorpus<usize> =
        bench::conductor::MountedCorpus::single_block(
            corpus_height,
            (0..pool_total).map(|i| (i, args.tx_per_proof)).collect(),
        );
    info!(
        "stream: witness plane = k=1 mounted corpus at height {} with {} \
         {{height, witness_index}} slices (ADR-0008 §1.4); witness_fetch_ms \
         is the LOCAL-RESOLVE FLOOR, not witness_move (ADR-0008 §2.3)",
        corpus_height, pool_total
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
        tx_types: None,
        chunk_tx_type_homogeneous: None,
        witness_fetch_ms: None,
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
        use bench::conductor::{WitnessKey, WitnessResolver};

        // WITNESS RESOLVE (ADR-0008 §1.2/§2.1): the cell resolves its chunk's
        // witness REFERENCE `{height, witness_index}` through the witness
        // plane, measuring the real local-resolve wall (witness_fetch_ms).
        // The reference is what the dispatch carried; the bytes are pulled
        // locally here. This is the k=1 mounted-corpus lookup -- a local
        // indexed read, never a network GET (ADR-0008 §1.3). The measured
        // wall is the LOCAL-RESOLVE FLOOR, never witness_move (ADR-0008 §2.3).
        let witness_key = WitnessKey::new(corpus_height, pool_idx as u64);
        let witness_fetch_ms = witness_corpus
            .resolve(witness_key)
            .map(|resolved| resolved.fetch_ms);

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
            // ADR-0008 §2.1/§2.2: the real measured local-resolve floor for
            // this chunk's witness reference. Emitted on ChunkProven.
            witness_fetch_ms,
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
            tx_types: None,
            chunk_tx_type_homogeneous: None,
            witness_fetch_ms: None,
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

// ════════════════════════════════════════════════════════════════════════
// Issue #172 — the GENUINE distributed prover entrypoint.
//
// `bench --mode coordinator` and `bench --mode cell` run as SEPARATE GKE
// pods coordinating over REAL Pub/Sub (ADR-0006 §1.1/§1.2/§2; ADR-0008).
// The prove path is the REAL one (BlockTxCircuit L1 + BlockTxChainCircuit
// L2) — never stubbed, never fabricated. See docs/distributed-prover-runtime.md.
// ════════════════════════════════════════════════════════════════════════

/// Resolve the Pub/Sub config from args, exiting with a clear message on any
/// missing required field. Shared by both distributed roles.
fn resolve_pubsub_config(args: &Args) -> bench::conductor::PubSubConfig {
    fn require(name: &str, v: &Option<String>) -> String {
        match v {
            Some(s) if !s.is_empty() => s.clone(),
            _ => {
                eprintln!(
                    "error: --mode {} requires --{} (or its env var)",
                    "coordinator|cell", name
                );
                std::process::exit(2);
            }
        }
    }
    bench::conductor::PubSubConfig {
        project: require("project", &args.project),
        // The topic fields are only needed by the publishing side; default to
        // empty (the role that publishes validates its own required topics).
        dispatch_topic: args.dispatch_topic.clone().unwrap_or_default(),
        dispatch_subscription: args.dispatch_subscription.clone().unwrap_or_default(),
        chunk_topic: args.chunk_topic.clone().unwrap_or_default(),
        chunk_subscription: args.chunk_subscription.clone().unwrap_or_default(),
        results_topic: args.results_topic.clone().unwrap_or_default(),
        results_subscription: args.results_subscription.clone().unwrap_or_default(),
        gcloud_bin: args.gcloud_bin.clone(),
    }
}

/// The leaf prover pod (`bench --mode cell`, ADR-0006 §2 / ADR-0008).
///
/// Builds the L1 (BlockTxCircuit) + L2 (BlockTxChainCircuit) circuits ONCE
/// (resident), mounts the k=1 witness corpus from the bundled `bench_test.json`
/// (the same `{height, witness_index}` MountedCorpus the stream path uses),
/// then loops: pull chunk references from the chunk subscription, resolve the
/// witness slice (timing the REAL `witness_fetch_ms` local-resolve floor,
/// ADR-0008 §2.1/§2.3), run the REAL L1+L2 prove, emit a `chunk_proven`
/// BENCH_EVENT, and publish a result message back to the coordinator.
fn run_cell(args: &Args) {
    use std::time::Instant;

    use bench::conductor::{
        ChunkResultMessage, GcloudPubSub, MountedCorpus, WitnessKey, WitnessResolver,
    };

    let mut cfg = resolve_pubsub_config(args);
    if cfg.chunk_subscription.is_empty() {
        eprintln!("error: --mode cell requires --chunk-subscription (or LIGHTER_CHUNK_SUBSCRIPTION)");
        std::process::exit(2);
    }
    if cfg.results_topic.is_empty() {
        eprintln!("error: --mode cell requires --results-topic (or LIGHTER_RESULTS_TOPIC)");
        std::process::exit(2);
    }
    // Cells never need the dispatch sub or chunk topic; leave them blank.
    cfg.dispatch_subscription.clear();
    let bus = GcloudPubSub::new(cfg);
    let cell_id = read_hostname();

    info!(
        "cell: starting (id={}) chunk_sub={} results_topic={} max_units={}",
        cell_id,
        bus.config().chunk_subscription,
        bus.config().results_topic,
        args.max_units,
    );

    // ---- Build the witness corpus (k=1 mounted, from bench_test.json) ----
    //
    // Cells resolve witness slices LOCALLY (ADR-0008 §1.3 — a mounted
    // read-only corpus, no network on the prove path). The bundled
    // `bench_test.json` is baked into the image; here it is sliced into
    // `S`-tx chunks indexed `0..k-1` at `height = block.block_number`, exactly
    // as `run_stream` builds it. A chunk message's `witness_index` selects the
    // slice. The committed `bench/corpus/` index is the documented multi-height
    // generalization (issue #165); a GCS-backed volume is the future upgrade.
    let block = get_test_block_json_file("bench_test.json");
    if block.txs.len() < args.tx_per_proof {
        eprintln!(
            "error: bench_test.json has {} txs but --tx-per-proof is {}; need at least one full chunk",
            block.txs.len(),
            args.tx_per_proof
        );
        std::process::exit(2);
    }
    let aligned_limit = (args.tx_limit / args.tx_per_proof) * args.tx_per_proof;
    let effective_limit =
        aligned_limit.min((block.txs.len() / args.tx_per_proof) * args.tx_per_proof);
    let pool: Vec<Vec<_>> = block.txs[..effective_limit]
        .chunks(args.tx_per_proof)
        .map(|c| c.to_vec())
        .collect();
    let pool_total = pool.len();
    let corpus_height: u64 = block.block_number;
    let witness_corpus: MountedCorpus<usize> = MountedCorpus::single_block(
        corpus_height,
        (0..pool_total).map(|i| (i, args.tx_per_proof)).collect(),
    );
    info!(
        "cell: witness plane = k=1 mounted corpus at height {} with {} slices \
         (ADR-0008 §1.4); witness_fetch_ms is the LOCAL-RESOLVE FLOOR",
        corpus_height, pool_total
    );

    // ---- Build circuits ONCE (resident; identical to the stream path) ----
    let circuit = BlockTxCircuit::define(CIRCUIT_CONFIG, args.tx_per_proof, CHAIN_ID);
    let bt = circuit.target;
    let data = circuit.builder.build::<C>();
    info!("cell: BlockTxCircuit defined");

    let pre_exec_circuit = BlockPreExecutionCircuit::define(CIRCUIT_CONFIG);
    let pbt = pre_exec_circuit.target;
    let pre_exec_data = pre_exec_circuit.builder.build::<C>();

    let chain_circuit = BlockTxChainCircuit::define(CIRCUIT_CONFIG, &data, args.tx_per_proof, 1);
    let chain_circuit_t = chain_circuit.target;
    let chain_circuit_data = chain_circuit.builder.build::<C>();
    let block_tx_witness_size = chain_circuit.block_tx_witness_size;
    info!("cell: BlockTxChainCircuit defined");

    let dummy_tx_chain_circuit = dummy_circuit(&chain_circuit_data.common);
    let dummy_proof = cyclic_base_proof(
        &chain_circuit_data.common,
        &chain_circuit_data.verifier_only,
        &dummy_tx_chain_circuit,
        Vec::<F>::new().iter().copied().enumerate().collect(),
    )
    .unwrap();

    let block_pre_exec = BlockPreExec::from_block(&block);
    let pre_proof = BlockPreExecutionCircuit::prove(&pre_exec_data, &block_pre_exec, &pbt)
        .unwrap_or_else(|err| panic!("Block pre-exec failed to prove. err = {:?}", err));
    let pre_exec_witness = BlockPreExecWitness::from_public_inputs(&pre_proof.public_inputs);
    let state_metadata = pre_exec_witness.new_state_metadata.clone();
    let created_at = block.created_at;

    // ---- FINDING D fix (issue #177): per-TX POSITIONAL pre-state corpus ----
    //
    // The cell must seed each chunk from its POSITIONAL pre-state
    // (`snapshot[S * witness_index]`), NOT from block-initial state. Pre-state
    // is a property of a tx POSITION, not of a chunk, so a single per-tx
    // snapshot array serves ANY chunk size S (the coordinator owns SPLIT;
    // ADR-0006 §1.2). See `bench::prestate`.
    //
    // The snapshot array is generated OFFLINE in the production design (issue
    // #178 host-side transition / #119 witness service). For THIS benchmark the
    // cell materializes it once at startup via the sequential L1 sweep at S=1
    // over its mounted block -- a one-time cost OFF the per-chunk prove loop, so
    // the k-way parallel PROVE the benchmark measures is fully preserved. The
    // pre-state DELIVERY cost is therefore a SEPARATE, currently-unmeasured
    // production term (named loudly: this is a local-disk materialize, not a
    // witness-service fetch).
    //
    // `LIGHTER_DISABLE_PRESTATE_FIX=1` reverts to the pre-#177 block-initial
    // seeding (only chunk 0 proves) for A/B confirmation of the bug.
    let prestate_fix_enabled = std::env::var("LIGHTER_DISABLE_PRESTATE_FIX")
        .map(|v| v != "1")
        .unwrap_or(true);
    let positional_snapshots: Option<PreStateSnapshots> = if prestate_fix_enabled {
        info!(
            "cell: FINDING D fix ON -- materializing per-tx positional pre-state corpus \
             via S=1 L1 sweep over {} txs (one-time, off the prove-loop critical path)...",
            effective_limit
        );
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
        // The sweep proves SINGLE-tx steps, so it needs an S=1 L1 circuit --
        // NOT the cell's serving circuit `data` (built at S=tx_per_proof). A
        // single-tx BlockTx fed to the S=9 circuit trips the in-circuit
        // `zip_eq` (the circuit expects exactly tx_per_proof txs). This is a
        // separate, transient circuit used only for the one-time sweep.
        let sweep_circuit = BlockTxCircuit::define(CIRCUIT_CONFIG, 1, CHAIN_ID);
        let sweep_bt = sweep_circuit.target;
        let sweep_data = sweep_circuit.builder.build::<C>();
        let sweep_t = Instant::now();
        let snaps = sweep_per_tx_snapshots(
            block.block_number,
            created_at,
            initial,
            &block.txs[..effective_limit],
            &sweep_data,
            &sweep_bt,
            |_pos, _wall_ms| {},
        );
        info!(
            "cell: positional pre-state corpus ready: {} snapshots in {:?}",
            snaps.len(),
            sweep_t.elapsed()
        );
        Some(snaps)
    } else {
        log::warn!(
            "cell: FINDING D fix DISABLED (LIGHTER_DISABLE_PRESTATE_FIX=1) -- seeding every \
             chunk from block-initial state; only chunk 0 will prove (A/B mode)"
        );
        None
    };
    info!("cell: circuits resident; entering chunk-prove loop");

    // ---- Pull → resolve → REAL prove → report loop ----
    let mut proven: u64 = 0;
    loop {
        if args.max_units != 0 && proven >= args.max_units {
            info!("cell: reached max_units={}, exiting", args.max_units);
            break;
        }
        let chunks = match bus.pull_chunks(1) {
            Ok(c) => c,
            Err(e) => {
                log::warn!("cell: pull_chunks error: {e}");
                std::thread::sleep(std::time::Duration::from_secs(args.poll_interval_s));
                continue;
            }
        };
        if chunks.is_empty() {
            std::thread::sleep(std::time::Duration::from_secs(args.poll_interval_s));
            continue;
        }

        for chunk in chunks {
            // WITNESS RESOLVE (ADR-0008 §2.1): resolve the {height,
            // witness_index} REFERENCE that crossed the bus to local witness
            // bytes, measuring the real local-resolve floor. The corpus is
            // keyed by this cell's local height; we map the wire witness_index
            // into the local pool (round-robin if the wire height differs from
            // the local fixture height — the k=1 fixture is a single block, so
            // the index space is the pool).
            let pool_idx = (chunk.witness_index as usize) % pool_total.max(1);
            let witness_key = WitnessKey::new(corpus_height, pool_idx as u64);
            let witness_fetch_ms = witness_corpus.resolve(witness_key).map(|r| r.fetch_ms);

            // FINDING D fix (issue #177): seed this chunk from its POSITIONAL
            // pre-state `snapshot[S * witness_index]`, NOT from block-initial
            // state. Chunk `pool_idx` covers txs `[S*pool_idx, S*(pool_idx+1))`,
            // so its pre-state is the ledger having applied txs `0..S*pool_idx`.
            // Only chunk 0's pre-state equals block-initial; the pre-#177 code
            // used block-initial for EVERY chunk, which is why chunks 1..k-1
            // failed the wire-consistency check (only chunk 0 proved).
            let block_tx = match &positional_snapshots {
                Some(snaps) => {
                    let pre = snaps.at_chunk(args.tx_per_proof, pool_idx).unwrap_or_else(|| {
                        panic!(
                            "cell: positional snapshot[{}] (S={}, chunk={}) missing; \
                             corpus has {} snapshots",
                            args.tx_per_proof * pool_idx,
                            args.tx_per_proof,
                            pool_idx,
                            snaps.len()
                        )
                    });
                    pre.block_tx(created_at, pool[pool_idx].clone())
                }
                // A/B fallback (LIGHTER_DISABLE_PRESTATE_FIX=1): pre-#177
                // block-initial seeding -- only chunk 0 proves (reproduces
                // FINDING D for confirmation).
                None => BlockTx {
                    created_at,
                    old_system_config: block.old_system_config,
                    register_stack_before: block.register_stack_before,
                    all_assets_before: block.all_assets.clone(),
                    all_market_details_before: pre_exec_witness.new_market_details.clone(),
                    old_account_tree_root: block.old_account_tree_root,
                    old_account_pub_data_tree_root: block.old_account_pub_data_tree_root,
                    old_account_delta_tree_root: block.old_account_delta_tree_root,
                    old_market_tree_root: block.old_market_tree_root,
                    txs: pool[pool_idx].clone(),
                },
            };

            let prove_start = Instant::now();
            let l1_cpu_start = cpu_time_ms();
            // ── REAL L1 prove ── (never stubbed)
            let tx_proof = match BlockTxCircuit::prove(&data, &block_tx, &bt) {
                Ok(p) => p,
                Err(err) => {
                    log::error!(
                        "cell: L1 prove FAILED for height={} witness_index={}: {:?}",
                        chunk.height, chunk.witness_index, err
                    );
                    // Honest failure report — no fabricated proof.
                    let _ = bus.publish_result(&ChunkResultMessage {
                        height: chunk.height,
                        witness_index: chunk.witness_index,
                        prove_ms: prove_start.elapsed().as_millis() as u64,
                        witness_fetch_ms,
                        ok: false,
                        cell: cell_id.clone(),
                    });
                    continue;
                }
            };

            // The cell's L2 is a single-chunk LEAF chain proof: fold this one
            // L1 chunk onto the cyclic base (witness_index as the chain index).
            let base_chain_proof = BlockTxChainCircuit::cyclic_base_proof(
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
            // ── REAL L2 prove ── (never stubbed)
            let chain_ok = BlockTxChainCircuit::prove(
                &chain_circuit_t,
                &chain_circuit_data,
                chunk.witness_index,
                &base_chain_proof,
                &dummy_proof,
                &tx_proof,
            );
            let prove_ms = prove_start.elapsed().as_millis() as u64;
            let cpu_ms = diff_ms(l1_cpu_start, cpu_time_ms());

            let ok = chain_ok.is_ok();
            if let Err(err) = &chain_ok {
                log::error!(
                    "cell: L2 prove FAILED for height={} witness_index={}: {:?}",
                    chunk.height, chunk.witness_index, err
                );
            }

            // Emit the chunk_proven BENCH_EVENT with REAL timings (ADR-0008
            // §2.2 — witness_fetch_ms on the primary ChunkProven site).
            events::emit(&BenchEvent::ChunkProven {
                layer: 2,
                name: "BlockTxChainCircuit",
                chunk_idx: Some(pool_idx),
                chunk_total: Some(pool_total),
                tx_per_proof: args.tx_per_proof,
                wall_ms: prove_ms,
                cpu_ms,
                rss_mb_peak: peak_rss_mb(),
                rss_mb_after: current_rss_mb(),
                height: chunk.height,
                lag_ms: prove_ms,
                queue_depth: 0,
                ts: now_iso8601(),
                witness_fetch_ms,
            });

            // Report the result back to the coordinator over the results topic.
            if let Err(e) = bus.publish_result(&ChunkResultMessage {
                height: chunk.height,
                witness_index: chunk.witness_index,
                prove_ms,
                witness_fetch_ms,
                ok,
                cell: cell_id.clone(),
            }) {
                log::warn!("cell: publish_result failed: {e}");
            }

            info!(
                "cell: proved height={} witness_index={} prove_ms={} ok={}",
                chunk.height, chunk.witness_index, prove_ms, ok
            );
            proven += 1;
        }
    }
    info!("cell: done, {} chunks proven", proven);
}

/// The coordinator pod (`bench --mode coordinator`, ADR-0006 §1.1/§1.2).
///
/// One coordinator per pod; per-coordinator vertical concurrency stays 1
/// (#113 PROMISING-NOT-PROVEN — NOT built here). Loops: pull a block from the
/// dispatch subscription (competing-pull), SPLIT it into `k = ceil(tx/S)`
/// chunks (reusing `conductor::dispatch::split_k`), publish the `k` chunk
/// REFERENCES (not bytes; ADR-0008 §1.2) to the chunk topic, collect the `k`
/// chunk results from the results subscription, FOLD/merge the accounting,
/// then emit a per-block completion + lag BENCH_EVENT.
fn run_coordinator(args: &Args) {
    use std::time::Instant;

    use bench::conductor::dispatch::split_k;
    use bench::conductor::{ChunkMessage, GcloudPubSub};

    let mut cfg = resolve_pubsub_config(args);
    if cfg.dispatch_subscription.is_empty() {
        eprintln!(
            "error: --mode coordinator requires --dispatch-subscription (or LIGHTER_DISPATCH_SUBSCRIPTION)"
        );
        std::process::exit(2);
    }
    if cfg.chunk_topic.is_empty() {
        eprintln!("error: --mode coordinator requires --chunk-topic (or LIGHTER_CHUNK_TOPIC)");
        std::process::exit(2);
    }
    if cfg.results_subscription.is_empty() {
        eprintln!(
            "error: --mode coordinator requires --results-subscription (or LIGHTER_RESULTS_SUBSCRIPTION)"
        );
        std::process::exit(2);
    }
    cfg.chunk_subscription.clear();
    let bus = GcloudPubSub::new(cfg);

    info!(
        "coordinator: starting dispatch_sub={} chunk_topic={} results_sub={} S={} max_units={}",
        bus.config().dispatch_subscription,
        bus.config().chunk_topic,
        bus.config().results_subscription,
        args.tx_per_proof,
        args.max_units,
    );

    let mut blocks_done: u64 = 0;
    loop {
        if args.max_units != 0 && blocks_done >= args.max_units {
            info!("coordinator: reached max_units={}, exiting", args.max_units);
            break;
        }
        let block = match bus.pull_block() {
            Ok(Some(b)) => b,
            Ok(None) => {
                std::thread::sleep(std::time::Duration::from_secs(args.poll_interval_s));
                continue;
            }
            Err(e) => {
                log::warn!("coordinator: pull_block error: {e}");
                std::thread::sleep(std::time::Duration::from_secs(args.poll_interval_s));
                continue;
            }
        };

        let block_start = Instant::now();
        // SPLIT: k = ceil(tx / S) (ADR-0006 §1.2).
        let k = split_k(block.tx_count, args.tx_per_proof).max(1);
        info!(
            "coordinator: block height={} tx_count={} -> SPLIT into k={} chunks (S={})",
            block.height, block.tx_count, k, args.tx_per_proof
        );

        // DISPATCH: publish the k chunk REFERENCES to the chunk topic.
        let mut dispatched: u64 = 0;
        for i in 0..k {
            let msg = ChunkMessage::new(block.height, i, args.tx_per_proof as u64);
            match bus.publish_chunk(&msg) {
                Ok(_) => dispatched += 1,
                Err(e) => log::warn!(
                    "coordinator: publish_chunk height={} idx={} failed: {e}",
                    block.height, i
                ),
            }
        }

        // GATHER: collect chunk results until all k are in, the deadline hits,
        // or we time out. Bounded so a lost cell can't hang the coordinator
        // forever (the un-acked chunk redelivers to another cell on a native
        // manual-ack client; here we cap the wait honestly).
        let gather_deadline =
            Instant::now() + std::time::Duration::from_secs((args.poll_interval_s * 30).max(60));
        let mut collected: u64 = 0;
        let mut ok_count: u64 = 0;
        let mut total_prove_ms: u64 = 0;
        let mut total_witness_fetch_ms: u64 = 0;
        while collected < dispatched && Instant::now() < gather_deadline {
            match bus.pull_results(dispatched as u32) {
                Ok(results) => {
                    if results.is_empty() {
                        std::thread::sleep(std::time::Duration::from_secs(args.poll_interval_s));
                        continue;
                    }
                    for r in results {
                        if r.height != block.height {
                            // A straggler from a previous block; count it but
                            // do not attribute to this block's fold.
                            continue;
                        }
                        collected += 1;
                        if r.ok {
                            ok_count += 1;
                        }
                        total_prove_ms += r.prove_ms;
                        total_witness_fetch_ms += r.witness_fetch_ms.unwrap_or(0);
                    }
                }
                Err(e) => {
                    log::warn!("coordinator: pull_results error: {e}");
                    std::thread::sleep(std::time::Duration::from_secs(args.poll_interval_s));
                }
            }
        }

        let block_wall_ms = block_start.elapsed().as_millis() as u64;
        let complete = collected >= dispatched && ok_count == collected && dispatched > 0;

        // FOLD/MERGE + emit per-block completion. We reuse the StreamSummary
        // event shape as the per-block completion record: it carries the
        // headline lag/throughput fields the conductor already standardized.
        // The lag here is the block-arrival→all-chunks-proven wall (ADR-0004's
        // lag(c,l) at the L1→L2 chunk granularity for this slice).
        events::emit(&BenchEvent::StreamSummary {
            phase: if complete { "block_complete" } else { "block_partial" },
            throughput_tx_s: if block_wall_ms > 0 {
                (block.tx_count as f64) / (block_wall_ms as f64 / 1000.0)
            } else {
                0.0
            },
            lag_p50_ms: block_wall_ms,
            lag_p95_ms: block_wall_ms,
            peak_rss_mb: peak_rss_mb(),
            dropped_chunks: dispatched.saturating_sub(collected),
            arrivals: 1,
            gaps_skipped: 0,
            chunks_proven: ok_count,
            elapsed_s: block_wall_ms as f64 / 1000.0,
            ts: now_iso8601(),
        });

        info!(
            "coordinator: block height={} COMPLETE={} k={} dispatched={} collected={} ok={} \
             block_wall_ms={} sum_prove_ms={} sum_witness_fetch_ms={}",
            block.height, complete, k, dispatched, collected, ok_count,
            block_wall_ms, total_prove_ms, total_witness_fetch_ms
        );
        blocks_done += 1;
    }
    info!("coordinator: done, {} blocks dispatched", blocks_done);
}

/// Issue #72: per-chunk witness snapshot captured BEFORE the chunk's
/// L1 is proven. Promoted to the library (`bench::prestate::ChunkPreState`,
/// issue #177) so the distributed cell's FINDING D fix and the offline per-tx
/// positional snapshot generator share ONE definition (imported at the top of
/// this file).
///
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
/// Per-leaf base-proof seeding (issue #72, cell slice A): chunk k's base
/// proof needs the state and validium roots BEFORE chunk k. These are now
/// derived natively from witness data (`bench::seed`) for every chunk in
/// one pre-pass, so no leaf depends on any other leaf's proven outputs.
/// L1 chunks are still proven sequentially in this slice (their post-state
/// rolls forward via `BlockTxWitness::from_public_inputs`, which is the
/// same data plane used to build the seed table); L1 parallelism is a
/// separate slice tracked under the parallel-leaf-proving work.
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

    // ---- Phase 1: prove all L1 chunks sequentially, rolling chunk-input
    // state forward via the L1 proofs' public inputs. For each chunk we
    // also snapshot the pre-chunk witness state -- that snapshot is the
    // ONLY data the witness-native seed derivation in Phase 1.5 needs.
    // Issue #72: leaves no longer consume the previous leaf's PROVEN
    // outputs, so L1 is the only thing still threaded sequentially here
    // (a separate slice will parallelise L1; this slice only severs the
    // leaf-to-leaf seam, which is what blocks parallel leaf proving).
    let mut all_assets = block.all_assets.clone();
    let mut all_market_details = pre_exec_witness.new_market_details.clone();
    let mut system_config = block.old_system_config;
    let mut register_stack = block.register_stack_before;
    let mut account_tree_root = block.old_account_tree_root;
    let mut account_pub_data_tree_root = block.old_account_pub_data_tree_root;
    let mut account_delta_tree_root = block.old_account_delta_tree_root;
    let mut market_tree_root = block.old_market_tree_root;
    let created_at = block.created_at;

    let mut tx_prove_total = Duration::ZERO;
    let mut tx_proofs: Vec<ProofWithPublicInputs<F, C, D>> = Vec::with_capacity(chunks_count);
    // Per-chunk pre-state snapshots, captured BEFORE the chunk's L1 is
    // proven. Each snapshot drives one `ChunkSeed` in Phase 1.5; chunk
    // 0's snapshot matches L3's post-state by construction.
    let mut pre_states: Vec<ChunkPreState> = Vec::with_capacity(chunks_count);

    for (index, tx) in block.txs[..effective_limit]
        .chunks(args.tx_per_proof)
        .enumerate()
    {
        pre_states.push(ChunkPreState {
            register_stack,
            all_assets: all_assets.clone(),
            all_market_details: all_market_details.clone(),
            system_config,
            account_tree_root,
            account_pub_data_tree_root,
            account_delta_tree_root,
            market_tree_root,
        });

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
            // Issue #157: tree-fold path does not (yet) participate in
            // the per-tx-type cost spike; pass None to preserve pre-#157
            // JSON shape. Wiring this path requires plumbing the chunk's
            // `Tx` slice into the tree-fold scheduler -- deferred.
            tx_types: None,
            chunk_tx_type_homogeneous: None,
            witness_fetch_ms: None,
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

        tx_proofs.push(tx_proof);
    }

    // ---- Phase 1.5: derive every chunk's base-proof seed natively from
    // the pre-state snapshots. No proven outputs feed this -- seeds are
    // a pure function of witness data, the L3 state-metadata constants,
    // and the chunk's `old_*` ledger slice.
    let seed_t = Instant::now();
    let seeds: Vec<ChunkSeed> = pre_states
        .iter()
        .map(|s| {
            seed_from_state(
                &s.register_stack,
                s.account_tree_root,
                s.account_pub_data_tree_root,
                s.market_tree_root,
                s.account_delta_tree_root,
                &s.all_assets,
                &s.all_market_details,
                state_metadata,
                &s.system_config,
            )
        })
        .collect();
    info!(
        "witness-native seed derivation: {} seeds in {:?}",
        seeds.len(),
        seed_t.elapsed()
    );

    // Transitional assertion (issue #72 plan step 3): chunk 0's seed
    // must match the L3 (pre-exec) outputs. This is the same equality
    // the pre-#72 driver relied on implicitly when chunk 0 took its
    // seed from `pre_exec_witness.{new_state_root, new_validium_root}`,
    // promoted here to an explicit always-on guard so a future drift
    // in `compute_state_and_validium_roots` (or its `*_hash_parameters`
    // mirrors) is caught immediately instead of corrupting every leaf.
    assert_eq!(
        seeds[0].pre_state_root, pre_exec_witness.new_state_root,
        "witness-derived seed for chunk 0 disagrees with L3 new_state_root \
         (bench::seed mirror has drifted from BlockTxChainCircuit::perform_sanity_checks)"
    );
    assert_eq!(
        seeds[0].pre_validium_root, pre_exec_witness.new_validium_root,
        "witness-derived seed for chunk 0 disagrees with L3 new_validium_root \
         (bench::seed mirror has drifted from BlockTxChainCircuit::perform_sanity_checks)"
    );
    assert_eq!(
        seeds[0].pre_delta_root, block.old_account_delta_tree_root,
        "witness-derived seed for chunk 0 disagrees with the block's old_account_delta_tree_root"
    );

    // ---- Phase 2: prove LEAF chain proofs. Each leaf is independent
    // (its seed is pre-derived), so the iteration order is a free
    // parameter. `--leaf-order reverse` exercises that independence by
    // proving N-1..0; both orders must produce identical results.
    //
    // Issue #73 (cell slice B): when `--l2-workers M` > 1, build a
    // dedicated rayon ThreadPool of M workers and dispatch the leaves
    // (and the per-level merges below) into it. CircuitData is shared
    // by reference -- it is Send+Sync and immutable after build, so
    // every worker sees the same resident proving key without copying.
    // M = 1 takes the byte-for-byte serial path below to guarantee
    // zero regression against the pre-#73 driver.
    let workers = args.l2_workers;
    let l2_pool: Option<rayon::ThreadPool> = if workers > 1 {
        let pool = rayon::ThreadPoolBuilder::new()
            .num_threads(workers)
            .thread_name(|i| format!("l2-worker-{i}"))
            .build()
            .unwrap_or_else(|err| {
                panic!("issue #73: failed to build rayon pool of {workers} l2 workers: {err:?}")
            });
        info!(
            "L2_WORKERS: built rayon pool of {} worker threads (CircuitData shared by reference; \
             plonky2's global rayon pool still saturates cores per individual proof)",
            workers
        );
        Some(pool)
    } else {
        info!("L2_WORKERS: workers=1 (serial driver; zero regression vs pre-#73)");
        None
    };

    let leaf_indices: Vec<usize> = match args.leaf_order {
        LeafOrder::Forward => (0..chunks_count).collect(),
        LeafOrder::Reverse => (0..chunks_count).rev().collect(),
    };
    info!(
        "leaf prove order: {:?} ({} leaves, {} workers)",
        args.leaf_order, chunks_count, workers
    );

    let leaves_phase_start = Instant::now();
    let mut leaf_prove_total = Duration::ZERO; // includes per-leaf base-proof generation
    let mut base_proof_total = Duration::ZERO;
    let mut leaf_wall_per_node: Vec<u64> = vec![0; chunks_count];
    let leaves: Vec<ProofWithPublicInputs<F, C, D>> = if let Some(pool) = l2_pool.as_ref() {
        // Parallel leaf prove: dispatch into the dedicated pool. We collect
        // (index, leaf_proof, base_dt, leaf_dt) so totals + assertions
        // remain deterministic regardless of the worker that produced the
        // proof. Per-node `layer_prove` events fire from the worker thread.
        let results: Vec<LeafResult> = pool.install(|| {
            leaf_indices
                .par_iter()
                .map(|&index| {
                    prove_leaf(
                        index,
                        chunks_count,
                        args.tx_per_proof,
                        seeds[index],
                        &tx_proofs[index],
                        chain_target,
                        chain_data,
                        dummy_chain_circuit,
                        dummy_proof,
                        block.block_number,
                        block.created_at,
                        block_tx_witness_size,
                        state_metadata,
                    )
                })
                .collect()
        });
        let mut leaves_by_index: Vec<Option<ProofWithPublicInputs<F, C, D>>> =
            (0..chunks_count).map(|_| None).collect();
        for (index, proof, base_dt, leaf_dt) in results {
            base_proof_total += base_dt;
            leaf_prove_total += leaf_dt;
            leaf_wall_per_node[index] = leaf_dt.as_millis() as u64;
            leaves_by_index[index] = Some(proof);
        }
        leaves_by_index
            .into_iter()
            .enumerate()
            .map(|(i, opt)| opt.unwrap_or_else(|| panic!("missing leaf proof at index {i}")))
            .collect()
    } else {
        // Serial path: byte-for-byte the pre-#73 driver (workers=1).
        let mut leaves_by_index: Vec<Option<ProofWithPublicInputs<F, C, D>>> =
            (0..chunks_count).map(|_| None).collect();
        for &index in &leaf_indices {
            let (idx, proof, base_dt, leaf_dt) = prove_leaf(
                index,
                chunks_count,
                args.tx_per_proof,
                seeds[index],
                &tx_proofs[index],
                chain_target,
                chain_data,
                dummy_chain_circuit,
                dummy_proof,
                block.block_number,
                block.created_at,
                block_tx_witness_size,
                state_metadata,
            );
            base_proof_total += base_dt;
            leaf_prove_total += leaf_dt;
            leaf_wall_per_node[idx] = leaf_dt.as_millis() as u64;
            leaves_by_index[idx] = Some(proof);
        }
        leaves_by_index
            .into_iter()
            .enumerate()
            .map(|(i, opt)| opt.unwrap_or_else(|| panic!("missing leaf proof at index {i}")))
            .collect()
    };
    let leaves_wall = leaves_phase_start.elapsed();

    // Transitional assertion (issue #72 plan step 3): the leaf's proven
    // post-state must equal the NEXT chunk's witness-derived pre-state.
    // The pre-#73 driver interleaved this with the serial prove loop;
    // here we run it after Phase 2 so the parallel and serial code paths
    // share the same invariant check. Equivalent because every leaf is
    // proven before any seam is checked, and the witness derivation is
    // pure (no proven-output dependency).
    for index in 0..chunks_count.saturating_sub(1) {
        let leaf_witness =
            BlockTxChainWitness::from_public_inputs(&leaves[index].public_inputs, 1, 1);
        assert_eq!(
            leaf_witness.new_state_root,
            seeds[index + 1].pre_state_root,
            "leaf {index} proved new_state_root != witness-derived seed for chunk {} \
             (seed-derivation drift; check bench::seed against \
              BlockTxChainCircuit::perform_sanity_checks)",
            index + 1
        );
        assert_eq!(
            leaf_witness.new_validium_root,
            seeds[index + 1].pre_validium_root,
            "leaf {index} proved new_validium_root != witness-derived seed for chunk {} \
             (seed-derivation drift)",
            index + 1
        );
        assert_eq!(
            leaf_witness.new_account_delta_tree_root,
            seeds[index + 1].pre_delta_root,
            "leaf {index} proved new_account_delta_tree_root != witness-derived seed for \
             chunk {} (seed-derivation drift)",
            index + 1
        );
    }

    // Per-level leaf summary (issue #73). The leaf "level" is `0` -- the
    // base of the tree, populated entirely by `BlockTxChainCircuit::prove`
    // (not the merge circuit). `level_wall_ms` is the start-to-end
    // wall-clock of Phase 2, which equals the slowest leaf only when M
    // saturates the leaf set; otherwise it reflects scheduling reality.
    {
        let (sum, mx, mn) = wall_stats(&leaf_wall_per_node);
        events::emit(&BenchEvent::L2TreeLevel {
            level: 0,
            nodes: chunks_count as u64,
            level_wall_ms: leaves_wall.as_millis() as u64,
            node_wall_sum_ms: sum,
            node_wall_max_ms: mx,
            node_wall_min_ms: mn,
            workers: workers as u32,
            rss_mb_peak: peak_rss_mb(),
            rss_mb_after: current_rss_mb(),
            ts: now_iso8601(),
        });
    }

    // ---- Pairwise merge up the tree. Each entry carries (proof, is_merge).
    // Issue #73: parallelize each level across the M-worker pool. Odd
    // proofs at any level still carry up unchanged (the merge circuit
    // accepts leaf and merge children in any mix). Critical path remains
    // depth × longest-per-level merge.
    let merges_phase_start = Instant::now();
    let mut merge_prove_total = Duration::ZERO;
    let mut merges = 0usize;
    let mut depth = 0usize;
    let mut level: Vec<(ProofWithPublicInputs<F, C, D>, bool)> =
        leaves.into_iter().map(|p| (p, false)).collect();

    while level.len() > 1 {
        depth += 1;
        let level_start = Instant::now();
        let mut pairs: Vec<MergePair> = Vec::with_capacity(level.len() / 2 + 1);
        let mut iter = level.into_iter();
        while let Some(left) = iter.next() {
            match iter.next() {
                Some(right) => pairs.push((left, Some(right))),
                None => pairs.push((left, None)),
            }
        }

        // Stable index per pair within the level for event identity.
        let pair_count = pairs.len();
        let prove_pair = |i: usize, pair: MergePair| -> PairResult {
            let (left, right_opt) = pair;
            match right_opt {
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
                        panic!("Merge pair #{i} (level {depth}) failed. err = {err:?}")
                    });
                    let merge_dt = merge_dt.elapsed();
                    let wall_ms = merge_dt.as_millis() as u64;
                    events::emit(&BenchEvent::LayerProve {
                        layer: 2,
                        name: "BlockTxChainMergeCircuit",
                        chunk_idx: Some(i),
                        chunk_total: Some(pair_count),
                        tx_per_proof: args.tx_per_proof,
                        wall_ms,
                        cpu_ms: diff_ms(merge_cpu_start, cpu_time_ms()),
                        rss_mb_peak: peak_rss_mb(),
                        rss_mb_after: current_rss_mb(),
                        ts: now_iso8601(),
                        // Issue #157: merge nodes aggregate multiple
                        // chunks -- no single tx-type attribution applies.
                        tx_types: None,
                        chunk_tx_type_homogeneous: None,
                        witness_fetch_ms: None,
                    });
                    info!(
                        "merge pair #{i}/{pair_count} (level {depth}) \
                         BlockTxChainMergeCircuit::prove time: {:?}",
                        merge_dt
                    );
                    ((proof, true), Some(wall_ms))
                }
                None => {
                    info!(
                        "level {depth}: odd proof at pair #{i}/{pair_count} \
                         carried up to the next level"
                    );
                    (left, None)
                }
            }
        };

        let level_results: Vec<PairResult> = if let Some(pool) = l2_pool.as_ref() {
            pool.install(|| {
                pairs
                    .into_par_iter()
                    .enumerate()
                    .map(|(i, p)| prove_pair(i, p))
                    .collect()
            })
        } else {
            pairs
                .into_iter()
                .enumerate()
                .map(|(i, p)| prove_pair(i, p))
                .collect()
        };

        let mut node_walls: Vec<u64> = Vec::with_capacity(level_results.len());
        let mut next: Vec<(ProofWithPublicInputs<F, C, D>, bool)> =
            Vec::with_capacity(level_results.len());
        for (node, opt_wall) in level_results {
            if let Some(w) = opt_wall {
                merges += 1;
                merge_prove_total += Duration::from_millis(w);
                node_walls.push(w);
            }
            next.push(node);
        }
        let level_wall = level_start.elapsed();
        if !node_walls.is_empty() {
            let (sum, mx, mn) = wall_stats(&node_walls);
            events::emit(&BenchEvent::L2TreeLevel {
                level: depth as u32,
                nodes: node_walls.len() as u64,
                level_wall_ms: level_wall.as_millis() as u64,
                node_wall_sum_ms: sum,
                node_wall_max_ms: mx,
                node_wall_min_ms: mn,
                workers: workers as u32,
                rss_mb_peak: peak_rss_mb(),
                rss_mb_after: current_rss_mb(),
                ts: now_iso8601(),
            });
        }
        level = next;
    }
    let merges_wall = merges_phase_start.elapsed();
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

    // Issue #73: scheduler-level summary. Reports the realized parallel
    // wall-clock (Phase 2 + Phase 3) alongside the reported
    // critical_path so the sweep can tabulate the M / wall-clock curve
    // directly from the JSONL stream.
    let realized_wall = leaves_wall + merges_wall;
    let leaf_avg = if chunks_count > 0 {
        leaf_prove_total / chunks_count as u32
    } else {
        Duration::ZERO
    };
    info!(
        "L2_SCHEDULE workers={} leaves={} depth={} merges={} leaves_wall={:?} \
         merges_wall={:?} realized_wall={:?} critical_path={:?} (depth x avg merge)",
        workers,
        chunks_count,
        depth,
        merges,
        leaves_wall,
        merges_wall,
        realized_wall,
        critical_path,
    );
    events::emit(&BenchEvent::L2TreeSchedule {
        workers: workers as u32,
        leaves: chunks_count as u64,
        depth: depth as u32,
        merges: merges as u64,
        leaves_wall_ms: leaves_wall.as_millis() as u64,
        merges_wall_ms: merges_wall.as_millis() as u64,
        realized_wall_ms: realized_wall.as_millis() as u64,
        critical_path_ms: critical_path.as_millis() as u64,
        leaf_avg_ms: leaf_avg.as_millis() as u64,
        merge_avg_ms: merge_avg.as_millis() as u64,
        rss_mb_peak: peak_rss_mb(),
        rss_mb_after: current_rss_mb(),
        ts: now_iso8601(),
    });

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

/// Issue #73: prove one LEAF chain proof (= 1-chunk chain: base proof seeded
/// at the chunk's pre-state + one chain step at tx_index = 0). Pulled out
/// of `run_tree_fold`'s Phase 2 so the same body serves both the M=1 serial
/// path and the M>1 parallel path (rayon `par_iter`). Per-node `layer_prove`
/// events fire from the calling worker thread.
///
/// Returns `(index, leaf_proof, base_proof_duration, full_leaf_duration)`.
/// `full_leaf_duration` includes the base-proof generation cost (matching the
/// pre-#73 driver's `leaf_dt` accounting).
#[allow(clippy::too_many_arguments)]
fn prove_leaf(
    index: usize,
    chunks_count: usize,
    tx_per_proof: usize,
    seed: ChunkSeed,
    tx_proof: &ProofWithPublicInputs<F, C, D>,
    chain_target: &BlockTxChainTarget,
    chain_data: &CircuitData<F, C, D>,
    dummy_chain_circuit: &CircuitData<F, C, D>,
    dummy_proof: &ProofWithPublicInputs<F, C, D>,
    block_number: u64,
    block_created_at: i64,
    block_tx_witness_size: usize,
    state_metadata: &StateMetadata,
) -> LeafResult {
    let leaf_dt = Instant::now();
    let l2_cpu_start = cpu_time_ms();
    let base_t = Instant::now();
    let base_proof = BlockTxChainCircuit::cyclic_base_proof(
        chain_data,
        dummy_chain_circuit,
        block_number,
        block_created_at,
        seed.pre_state_root,
        seed.pre_state_root,
        seed.pre_validium_root,
        seed.pre_delta_root,
        block_tx_witness_size,
        state_metadata,
    );
    let base_dt = base_t.elapsed();
    let leaf_proof = BlockTxChainCircuit::prove(
        chain_target,
        chain_data,
        0, // every leaf is the first (and only) step of its own chain
        &base_proof,
        dummy_proof,
        tx_proof,
    )
    .unwrap_or_else(|err| panic!("Leaf chain proof #{index} failed. err = {err:?}"));
    let leaf_dt = leaf_dt.elapsed();
    events::emit(&BenchEvent::LayerProve {
        layer: 2,
        name: "BlockTxChainCircuit",
        chunk_idx: Some(index),
        chunk_total: Some(chunks_count),
        tx_per_proof,
        wall_ms: leaf_dt.as_millis() as u64,
        cpu_ms: diff_ms(l2_cpu_start, cpu_time_ms()),
        rss_mb_peak: peak_rss_mb(),
        rss_mb_after: current_rss_mb(),
        ts: now_iso8601(),
        // Issue #157: tree-fold leaf path -- attribution deferred (see
        // L1 site above).
        tx_types: None,
        chunk_tx_type_homogeneous: None,
        witness_fetch_ms: None,
    });
    info!(
        "tx chunk #{index}/{} leaf BlockTxChainCircuit::prove time (incl. base proof): {:?}",
        chunks_count, leaf_dt
    );
    (index, leaf_proof, base_dt, leaf_dt)
}

/// Issue #73: compute `(sum, max, min)` over a slice of per-node wall_ms
/// for the `L2TreeLevel` event. Returns `(0, 0, 0)` for an empty input.
fn wall_stats(walls: &[u64]) -> (u64, u64, u64) {
    if walls.is_empty() {
        return (0, 0, 0);
    }
    let sum: u64 = walls.iter().sum();
    let mx = walls.iter().copied().max().unwrap_or(0);
    let mn = walls.iter().copied().min().unwrap_or(0);
    (sum, mx, mn)
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
    let l4_build_ms = define_t.elapsed().as_millis() as u64;
    events::emit(&BenchEvent::CircuitDefine {
        layer: 4,
        name: "BlockCircuit",
        wall_ms: l4_build_ms,
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
    // Issue #102: split timings. `l4_prove_ms` covers witness+prove,
    // `l4_verify_ms` covers verify; the combined `layer_prove` event below
    // keeps its historical wall_ms = witness+prove+verify span unchanged.
    let l4_prove_ms = prove_t.elapsed().as_millis() as u64;
    let verify_t = Instant::now();
    l4_data
        .verify(l4_proof.clone())
        .unwrap_or_else(|err| panic!("L4_CHECK [{label}] verify failed: {err:?}"));
    let l4_verify_ms = verify_t.elapsed().as_millis() as u64;
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
        // Issue #157: L4 is a block-level aggregate, not per-tx-chunk.
        tx_types: None,
        chunk_tx_type_homogeneous: None,
        witness_fetch_ms: None,
    });
    // Issue #102 (additive): build / prove / verify split for the
    // calibration suite's objective-4 constants (per-machine L4_WALL).
    events::emit(&BenchEvent::L4Check {
        name: "BlockCircuit",
        label,
        tx_per_proof,
        l4_build_ms,
        l4_prove_ms,
        l4_verify_ms,
        ts: now_iso8601(),
    });
    info!(
        "L4_CHECK [{label}] PASS: BlockCircuit proved+verified the final chain proof in {:?} \
         (build {l4_build_ms} ms, prove {l4_prove_ms} ms, verify {l4_verify_ms} ms)",
        prove_dt
    );
}

/// Issue #83 (acceptance criterion #1): drive the standalone cyclic-delta prove
/// path over a correctly-shaped synthesized (empty) batch and verify the
/// resulting `delta_chain_proof`.
fn run_delta_prove(_args: &Args) {
    let bench_start = Instant::now();
    info!("DELTA_PROVE: driving DeltaCircuit + CyclicDeltaCircuit over an empty synthesized batch");

    // Arbitrary quintic evaluation point for the empty synthesized batch. The
    // standalone delta chain does not constrain the evaluation point against the
    // blob (that cross-check happens only in the inner wrapper), so any value
    // proves; --l6-inner derives this from the blob's pub-data hash instead.
    let x = HashOut::from_vec(vec![
        F::from_canonical_u64(1),
        F::from_canonical_u64(2),
        F::from_canonical_u64(3),
        F::from_canonical_u64(4),
    ]);

    let (proof, _cyclic_data) =
        l6drive::prove_delta_chain(1, x).expect("DELTA_PROVE: delta chain prove");

    info!(
        "DELTA_PROVE PASS: delta_chain_proof produced and verified ({} public inputs) in {:?}",
        proof.public_inputs.len(),
        bench_start.elapsed()
    );
}

/// Run `f` on a dedicated thread with a large (4 GiB) stack. The
/// blob-evaluation and inner-wrapper circuits allocate very deep recursive
/// builders (4096 BLS12-381 nonnative elements) that overflow the default 8 MiB
/// main-thread stack; the L5 drivers sidestep this via rayon worker threads.
fn run_on_big_stack<F: FnOnce() + Send + 'static>(name: &str, f: F) {
    std::thread::Builder::new()
        .name(name.to_string())
        .stack_size(4 * 1024 * 1024 * 1024)
        .spawn(f)
        .expect("spawn big-stack thread")
        .join()
        .expect("big-stack thread panicked");
}

/// Issue #83 (acceptance criteria #2 and #3): drive the standalone
/// blob-evaluation prove path. The off-circuit KZG sidecar computes the KZG
/// versioned hash and the custom-Poseidon2 PCE opening `(x, y)`; the in-circuit
/// PCE check is the correctness gate.
fn run_blob_prove(args: &Args) {
    let args = args.clone();
    run_on_big_stack("blob-prove", move || run_blob_prove_inner(&args));
}

fn run_blob_prove_inner(args: &Args) {
    use circuit::blob::blob_constraints::{BlobEvaluationCircuit, Circuit as _};
    use circuit::types::market_details::PublicMarketDetails;

    let bench_start = Instant::now();
    info!("BLOB_PROVE: encoding a correctly-shaped synthesized blob + computing KZG sidecar");

    let blob = blob_encode::empty_blob();
    let market = blob_encode::empty_market_limbs();
    let public_market_details: [PublicMarketDetails; POSITION_LIST_SIZE] =
        core::array::from_fn(|_| PublicMarketDetails::default());

    let blob_eval = kzg::build_blob_evaluation(
        blob,
        &market,
        EMPTY_ACCOUNT_DELTA_TREE_ROOT,
        public_market_details,
        &args.trusted_setup_path,
    )
    .expect("BLOB_PROVE: build blob evaluation (trusted setup + KZG sidecar)");
    info!(
        "BLOB_PROVE: KZG versioned hash = 0x{}",
        hex::encode(blob_eval.kzg_versioned_hash)
    );

    let circuit = BlobEvaluationCircuit::define(CIRCUIT_CONFIG);
    let data = circuit.builder.build::<C>();
    let proof = BlobEvaluationCircuit::prove(&data, &blob_eval, &circuit.target)
        .expect("BLOB_PROVE: blob evaluation prove (in-circuit PCE check on sidecar (x, y))");
    data.verify(proof.clone())
        .expect("BLOB_PROVE: blob_evaluation_proof verifies");

    info!(
        "BLOB_PROVE PASS: blob_evaluation_proof produced and verified ({} public inputs) in {:?}",
        proof.public_inputs.len(),
        bench_start.elapsed()
    );
}

/// Issue #83 (acceptance criterion #4): drive the end-to-end L6 inner-wrapper
/// prove path. Assembles the three missing inputs — 8 L5 chain proofs, the
/// `delta_chain_proof`, the `blob_evaluation_proof` — plus the KZG `WrapperInput`,
/// then calls `WrapperCircuit::prove_inner` and verifies.
///
/// ## Cross-circuit consistency (the L5 empty-batch requirement)
///
/// `prove_inner`'s `define_inner` couples all three inputs over a SINGLE batch:
///   * `handle_segment_proofs` asserts the first chain proof is an empty segment
///     with `old_account_delta_tree_root == EMPTY_ACCOUNT_DELTA_TREE_ROOT`, then
///     merges the 8 chain proofs into one `batch`;
///   * `verify_aggregated_delta` + `handle_blob_evaluation_proof` connect
///     `batch.new_account_delta_tree_root` to BOTH the delta chain's root AND
///     the blob's `account_delta_tree_root`;
///   * `verify_aggregated_delta` binds the delta evaluation point to
///     `hash_two_to_one(blob_pub_data_hash, account_delta_tree_root)`;
///   * `verify_delta_polynomial_evaluation` ties the blob's compressed-leaf
///     bytes to the delta chain's polynomial evaluation.
///
/// For the correctly-shaped synthesized EMPTY batch this issue targets, the
/// delta chain (empty), the blob (empty), and the derived evaluation point are
/// all produced consistently below. The remaining input is 8 L5 chain proofs
/// whose merged batch has `new_account_delta_tree_root == EMPTY_ACCOUNT_DELTA_TREE_ROOT`
/// (i.e. an L5 chain over genuinely no-op blocks). Producing that
/// consistent-empty L5 chain via the existing L1..L5 pipeline is the documented
/// open step (see docs/decisions/ADR-0005 and the PR); the delta + blob inputs
/// and the KZG `WrapperInput` are fully driven and self-consistent here.
fn run_l6_inner(args: &Args) {
    let args = args.clone();
    run_on_big_stack("l6-inner", move || run_l6_inner_inner(&args));
}

fn run_l6_inner_inner(args: &Args) {
    let bench_start = Instant::now();
    // Produce + verify the inner-wrapper proof (the terminating #83/#129 step).
    let (_inner_data, inner_proof) = produce_inner_wrapper_proof(args);
    info!(
        "L6_INNER: SUCCESS — produced + VERIFIED an inner-wrapper proof over the empty-genesis \
         L5 chain ({} public inputs) in {:?}. Issue #83 acceptance criterion #4 met: a verifying \
         inner-wrapper proof. No values were fabricated and no constraint was relaxed.",
        inner_proof.public_inputs.len(),
        bench_start.elapsed()
    );
}

/// Issue #116: drive the full inner -> outer wrapper chain. Produces + verifies
/// the inner-wrapper proof (the `--l6-inner` pipeline), then CONTINUES into the
/// outer stage via [`l6drive::prove_outer_wrapper`], which calls the previously
/// uncalled `WrapperCircuit::prove_outer` and verifies the result.
fn run_l6_outer(args: &Args) {
    let args = args.clone();
    run_on_big_stack("l6-outer", move || run_l6_outer_inner(&args));
}

fn run_l6_outer_inner(args: &Args) {
    use plonky2::plonk::config::GenericHashOut;

    let bench_start = Instant::now();
    info!("L6_OUTER: driving the full inner -> outer wrapper chain (issue #116)");

    // ---- Stage 1: the verified inner-wrapper proof (input to prove_outer). ----
    let inner_t = Instant::now();
    let (inner_data, inner_proof) = produce_inner_wrapper_proof(args);
    info!(
        "L6_OUTER: inner-wrapper proof produced + verified in {:?} ({} public inputs); driving \
         the outer stage (WrapperCircuit::prove_outer over OUTER_WRAPPER_CONFIG / \
         PoseidonBN128GoldilocksConfig).",
        inner_t.elapsed(),
        inner_proof.public_inputs.len()
    );

    // ---- Stage 2: the outer-wrapper drive (the #116 gap). ----
    // prove_outer_wrapper builds define_outer over the inner shape, builds with
    // the BN128 config, calls WrapperCircuit::prove_outer (proves AND verifies
    // internally), and adds a belt-and-suspenders explicit verify.
    let outer_t = Instant::now();
    let (outer_data, outer_proof) = l6drive::prove_outer_wrapper(&inner_data, &inner_proof)
        .expect("L6_OUTER: outer-wrapper prove_outer + verify");
    let outer_elapsed = outer_t.elapsed();

    // Belt-and-suspenders explicit verify at the drive boundary too (the helper
    // already verified; this mirrors the inner pattern and is the acceptance gate).
    outer_data
        .verify(outer_proof.clone())
        .expect("L6_OUTER: outer-wrapper proof verifies");

    let outer_digest = hex::encode(outer_data.verifier_only.circuit_digest.to_bytes().clone());

    // Issue #117: export the verified outer proof + circuit data to the JSON
    // schema the gnark bridge consumes (the inputs to gnark plonk.Prove). The
    // proof is serialized exactly as produced — no field is fabricated.
    if let Some(dir) = args.l6_outer_export.clone() {
        export_outer_wrapper_json(&dir, &outer_digest, &outer_data, &outer_proof)
            .expect("L6_OUTER: export outer-wrapper JSON for the gnark bridge");
    }

    info!(
        "L6_OUTER: SUCCESS — WrapperCircuit::prove_outer produced + VERIFIED an outer-wrapper \
         proof (BN128 config) over the verified inner-wrapper proof in {:?} (outer prove+verify \
         {:?}; total {:?}). Outer circuit digest: {} ({} public inputs). Issue #116 met: the \
         outer-wrapper drive path now actually calls the previously-uncalled prove_outer and \
         verifies the conversion toward the Ethereum-friendly form. No values were fabricated and \
         no constraint was relaxed.",
        bench_start.elapsed(),
        outer_elapsed,
        bench_start.elapsed(),
        outer_digest,
        outer_proof.public_inputs.len(),
    );
}

/// Issue #117: serialize the verified outer-wrapper proof + its circuit's common
/// and verifier-only data to JSON, in the exact schema the gnark bridge reads
/// (`types.ReadProofWithPublicInputs`, `types.ReadCommonCircuitData`,
/// `types.ReadVerifierOnlyCircuitData`). These three files are the inputs to the
/// gnark `plonk.Prove` final-proof path (issue #117) and to `snark/main.go`'s
/// key-generation step.
///
/// Uses `serde_json::to_string` on the REAL `ProofWithPublicInputs` /
/// `CommonCircuitData` / `VerifierOnlyCircuitData` — identical to the
/// circuit-data export already done in `circuit/src/bin/build_wrapper_circuit.rs`.
/// No field is fabricated; the proof is written exactly as produced + verified.
fn export_outer_wrapper_json(
    dir: &std::path::Path,
    outer_digest: &str,
    outer_data: &CircuitData<
        F,
        circuit::poseidon_bn128::plonky2_config::PoseidonBN128GoldilocksConfig,
        D,
    >,
    outer_proof: &ProofWithPublicInputs<
        F,
        circuit::poseidon_bn128::plonky2_config::PoseidonBN128GoldilocksConfig,
        D,
    >,
) -> anyhow::Result<()> {
    fs::create_dir_all(dir)?;

    let proof_json = serde_json::to_string(outer_proof)?;
    let common_json = serde_json::to_string(&outer_data.common)?;
    let verifier_json = serde_json::to_string(&outer_data.verifier_only)?;

    let proof_path = dir.join(format!("outer-wrapper-proof::{outer_digest}.json"));
    let common_path = dir.join(format!(
        "outer-wrapper-circuit::common_circuit_data::{outer_digest}.json"
    ));
    let verifier_path = dir.join(format!(
        "outer-wrapper-circuit::verifier_circuit_data::{outer_digest}.json"
    ));

    fs::write(&proof_path, proof_json)?;
    fs::write(&common_path, common_json)?;
    fs::write(&verifier_path, verifier_json)?;

    info!(
        "L6_OUTER: exported outer-wrapper JSON for the gnark bridge:\n  proof:    {}\n  common:   {}\n  verifier: {}",
        proof_path.display(),
        common_path.display(),
        verifier_path.display(),
    );
    Ok(())
}

/// Issue #83/#129 (criterion #4) + #116: assemble the three inner-wrapper inputs
/// over an empty synthesized batch, call `WrapperCircuit::prove_inner` (which
/// proves AND verifies), verify again belt-and-suspenders, and return the inner
/// `CircuitData` + verified inner proof. The inner `CircuitData`'s
/// `.common`/`.verifier_only` feed `define_outer`, and the inner proof is the
/// witness `prove_outer` consumes (issue #116).
fn produce_inner_wrapper_proof(
    args: &Args,
) -> (CircuitData<F, C, D>, ProofWithPublicInputs<F, C, D>) {
    use circuit::blob::blob_constraints::{BlobEvaluationCircuit, Circuit as _};
    use circuit::types::market_details::PublicMarketDetails;

    info!("L6_INNER: assembling the three inner-wrapper inputs over an empty synthesized batch");

    // ---- Blob + KZG sidecar (blob_evaluation_proof input) ----
    let blob = blob_encode::empty_blob();
    let market = blob_encode::empty_market_limbs();
    let public_market_details: [PublicMarketDetails; POSITION_LIST_SIZE] =
        core::array::from_fn(|_| PublicMarketDetails::default());

    let blob_eval = kzg::build_blob_evaluation(
        blob.clone(),
        &market,
        EMPTY_ACCOUNT_DELTA_TREE_ROOT,
        public_market_details,
        &args.trusted_setup_path,
    )
    .expect("L6_INNER: build blob evaluation");
    info!(
        "L6_INNER: KZG versioned hash = 0x{}",
        hex::encode(blob_eval.kzg_versioned_hash)
    );

    let blob_circuit = BlobEvaluationCircuit::define(CIRCUIT_CONFIG);
    let blob_data = blob_circuit.builder.build::<C>();
    let blob_proof = BlobEvaluationCircuit::prove(&blob_data, &blob_eval, &blob_circuit.target)
        .expect("L6_INNER: blob evaluation prove");
    blob_data
        .verify(blob_proof.clone())
        .expect("L6_INNER: blob_evaluation_proof verifies");
    info!("L6_INNER: blob_evaluation_proof produced + verified");

    // ---- Delta chain (delta_chain_proof input) ----
    // The wrapper binds the aggregated delta's evaluation point to
    // hash_two_to_one(blob_pub_data_hash, account_delta_tree_root); derive it
    // off-circuit so the delta chain matches what prove_inner recomputes.
    let delta_eval_point =
        kzg::wrapper_delta_evaluation_point(blob.as_ref(), EMPTY_ACCOUNT_DELTA_TREE_ROOT);
    let (delta_chain_proof, delta_cyclic_data) =
        l6drive::prove_delta_chain(1, delta_eval_point).expect("L6_INNER: delta chain prove");
    info!("L6_INNER: delta_chain_proof produced + verified");

    // ---- Inner wrapper circuit ----
    // Build the full L1→L5 pipeline ONCE (issue #129) and reuse its exact
    // `l5_data` both for `define_inner` and for proving the empty L5 chain, so
    // the chain proofs verify against the wrapper's pinned chain verifier.
    let pipeline =
        l6drive::EmptyL5Pipeline::build(CHAIN_ID).expect("L6_INNER: build L1..L5 pipeline");
    info!(
        "L6_INNER: L5 recursion circuit built (degree 2^{})",
        pipeline.l5_data.common.degree_bits()
    );

    let inner = circuit::recursion::wrapper_circuit::WrapperCircuit::define_inner(
        CIRCUIT_CONFIG,
        &pipeline.l5_data.common,
        &pipeline.l5_data.verifier_only,
        &delta_cyclic_data.common,
        &delta_cyclic_data.verifier_only,
        &blob_data.common,
        &blob_data.verifier_only,
    );
    let inner_target = Box::new(inner.target.clone());
    let inner_data = inner.builder.build::<C>();
    info!(
        "L6_INNER: inner-wrapper circuit built (degree 2^{}); blob_evaluation_proof, \
         delta_chain_proof, and KZG WrapperInput are fully driven and mutually consistent. \
         Driving the terminating prove_inner over the empty-genesis L5 chain.",
        inner_data.common.degree_bits()
    );

    // Issue #129 (criterion #4): the terminating, VERIFYING step. Prove one L5
    // chain proof over the empty-genesis empty-tx block (merged batch has
    // new_account_delta_tree_root == EMPTY_ACCOUNT_DELTA_TREE_ROOT), pad the
    // unused chain_proofs[1..8) with chain_proofs[0] (segment_count = 1), derive
    // the WrapperInput batch_commitment from that merged batch, then call
    // WrapperCircuit::prove_inner (which internally proves AND verifies,
    // wrapper_circuit.rs:725-726). No fabricated values; no constraint relaxed.
    let l5_t = Instant::now();
    let chain_proof =
        l6drive::prove_empty_l5_chain(&pipeline).expect("L6_INNER: empty-genesis L5 chain prove");
    info!(
        "L6_INNER: empty-genesis L5 chain proof produced + verified in {:?}",
        l5_t.elapsed()
    );

    // Pad chain_proofs[1..8) with chain_proofs[0]; segment_count = 1 selects only
    // the first (real, empty) segment in `handle_segment_proofs`.
    let segment_count: u64 = 1;
    let chain_proofs_8: Vec<ProofWithPublicInputs<F, C, D>> = (0..NUM_CHAINS_PER_BATCH)
        .map(|_| chain_proof.clone())
        .collect();

    // Merged batch == the single segment's batch; derive batch_commitment from it
    // (matches the in-circuit verify_batch_commitment recomputation).
    let merged_batch = l6drive::batch_from_chain_proof(&chain_proof);
    let bc = l6drive::batch_commitment(
        &merged_batch,
        &blob_eval.blob_polynomial_opening_x,
        &blob_eval.blob_polynomial_opening_y,
        &blob_eval.kzg_versioned_hash,
    );
    let wrapper_input = kzg::build_wrapper_input(
        blob.clone(),
        &market,
        EMPTY_ACCOUNT_DELTA_TREE_ROOT,
        bc,
        &args.trusted_setup_path,
    )
    .expect("L6_INNER: build WrapperInput");

    let prove_t = Instant::now();
    let inner_proof = circuit::recursion::wrapper_circuit::WrapperCircuit::prove_inner(
        &inner_data,
        inner_target,
        wrapper_input,
        &chain_proofs_8,
        segment_count,
        delta_chain_proof,
        blob_proof,
    )
    .expect("L6_INNER: terminating prove_inner");
    let prove_elapsed = prove_t.elapsed();

    // Belt-and-suspenders explicit verify (prove_inner already verified).
    inner_data
        .verify(inner_proof.clone())
        .expect("L6_INNER: inner-wrapper proof verifies");

    info!(
        "L6_INNER: terminating WrapperCircuit::prove_inner produced + VERIFIED an inner-wrapper \
         proof over the empty-genesis L5 chain (prove_inner {:?}). A verifying inner-wrapper \
         proof; no values fabricated and no constraint relaxed.",
        prove_elapsed,
    );

    (inner_data, inner_proof)
}

fn run_l5_segment_check(args: &Args, base_block: &Block<F>) {
    let bench_start = Instant::now();
    let bench_cpu_start = cpu_time_ms();

    info!(
        "L5_SEGMENT_CHECK: synthesizing {} blocks across {} parallel segment chains",
        args.blocks, args.segments
    );

    // ---- 1. Build the pipeline circuits ONCE. ----
    // L1 (BlockTxCircuit) -> L2 (BlockTxChainCircuit) -> L3
    // (BlockPreExecutionCircuit) -> L4 (BlockCircuit) -> L5
    // (CyclicRecursionCircuit). All shapes are block-independent.
    let l1 = BlockTxCircuit::define(CIRCUIT_CONFIG, args.tx_per_proof, CHAIN_ID);
    let l1_target = l1.target;
    let l1_data = l1.builder.build::<C>();

    let l3 = BlockPreExecutionCircuit::define(CIRCUIT_CONFIG);
    let l3_target = l3.target;
    let l3_data = l3.builder.build::<C>();

    let l2 = BlockTxChainCircuit::define(CIRCUIT_CONFIG, &l1_data, args.tx_per_proof, 1);
    let l2_target = l2.target;
    let l2_data = l2.builder.build::<C>();
    let block_tx_witness_size = l2.block_tx_witness_size;
    let dummy_l2_circuit = dummy_circuit(&l2_data.common);
    let dummy_l2_proof = cyclic_base_proof(
        &l2_data.common,
        &l2_data.verifier_only,
        &dummy_l2_circuit,
        Vec::<F>::new().iter().copied().enumerate().collect(),
    )
    .expect("L5_SEGMENT_CHECK: L2 dummy proof");

    let l4_define_t = Instant::now();
    let l4 = BlockCircuit::define(CIRCUIT_CONFIG, &l3_data, &l2_data, 1);
    let l4_target = l4.target;
    let l4_data = l4.builder.build::<C>();
    events::emit(&BenchEvent::CircuitDefine {
        layer: 4,
        name: "BlockCircuit",
        wall_ms: l4_define_t.elapsed().as_millis() as u64,
        rss_mb_after: current_rss_mb(),
        ts: now_iso8601(),
    });

    let l5_define_t = Instant::now();
    let l5 = CyclicRecursionCircuit::define(CIRCUIT_CONFIG, &l4_data, 1);
    let l5_target = l5.target;
    let l5_data = l5.builder.build::<C>();
    events::emit(&BenchEvent::CircuitDefine {
        layer: 5,
        name: "CyclicRecursionCircuit",
        wall_ms: l5_define_t.elapsed().as_millis() as u64,
        rss_mb_after: current_rss_mb(),
        ts: now_iso8601(),
    });
    info!(
        "L5_SEGMENT_CHECK: circuits built (L4 degree 2^{}, L5 degree 2^{})",
        l4_data.common.degree_bits(),
        l5_data.common.degree_bits()
    );

    // L5 dummy proof (over the L5 common data) for the conditional cyclic
    // verifier slot -- built via the in-repo custom helper.
    let l5_dummy_circuit = dummy_circuit(&l5_data.common);
    let l5_dummy_proof = cyclic_base_proof(
        &l5_data.common,
        &l5_data.verifier_only,
        &l5_dummy_circuit,
        Vec::<F>::new().iter().copied().enumerate().collect(),
    )
    .expect("L5_SEGMENT_CHECK: L5 dummy proof");

    // ---- 2 + 3. Build the state-chained block sequence and one L4 proof
    // per block (serial up-front). Issue #94: blocks are constructed
    // **inline with proving** so each block i+1 can re-anchor against the
    // rolling state captured during block i's L4 prove and against block
    // i's L4 BlockWitness. The shared helper is also used by
    // `run_l5_tree_fold` so the two drivers consume the exact same
    // chained-fixture construction.
    let (blocks, l4_proofs) = build_chained_blocks_and_l4_proofs(
        args,
        base_block,
        args.blocks,
        &l1_data,
        &l1_target,
        &l3_data,
        &l3_target,
        &l2_data,
        &l2_target,
        block_tx_witness_size,
        &dummy_l2_circuit,
        &dummy_l2_proof,
        &l4_data,
        &l4_target,
        "L5_SEGMENT_CHECK",
    );

    // ---- 4. Split + host pre-pass (prove-free). ----
    let split_points = segment_split_points(blocks.len(), args.segments);
    let seeds = host_prepass(&blocks, &split_points);

    // ---- 5. Fold each segment IN PARALLEL across segments. ----
    info!(
        "L5_SEGMENT_CHECK: folding {} segments in parallel...",
        args.segments
    );
    let l5_data_ref = &l5_data;
    let l5_target_ref = &l5_target;
    let l5_dummy_ref = &l5_dummy_proof;
    let l4_proofs_ref = &l4_proofs;
    let split_ref = &split_points;
    let seeds_ref = &seeds;

    // Returns (final_segment_proof, wall_ms, segment_size) per segment.
    let mut results: Vec<(ProofWithPublicInputs<F, C, D>, u64, u64)> = (0..args.segments)
        .into_par_iter()
        .map(|k| {
            let start = split_ref[k];
            let end = split_ref[k + 1];
            let segment_size = (end - start) as u64;

            let segment_info = SegmentInfo {
                old_on_chain_operations_pub_data_hash: seeds_ref[k]
                    .old_on_chain_operations_pub_data_hash,
            };

            let seg_t = Instant::now();

            // Base cyclic proof seeded with this segment's SegmentInfo.
            let mut cyclic_proof =
                CyclicRecursionCircuit::cyclic_base_proof(l5_data_ref, &segment_info);

            // Running host batch mirror for this segment (default + per-block
            // aggregate_block), feeding the in-circuit fold's new_batch arg.
            let mut batch = Batch::<F>::default();
            let mut not_first_recursion = false;

            for (offset, l4_proof) in l4_proofs_ref[start..end].iter().enumerate() {
                let idx = start + offset;
                // The L5 circuit reads `current_block` from the L4 proof's
                // public inputs (the partial-block-patched values), and
                // connects the public `new_batch` target to the batch it
                // recomputes from THAT witness. So the host `new_batch` must be
                // aggregated from the L4 proof's BlockWitness -- not from the
                // raw synthesized Block, whose `new_*` roots predate the
                // (possibly partial) tx run.
                let block_witness = BlockWitness::from_public_inputs(&l4_proof.public_inputs, 1, 1);
                batch.aggregate_block(&block_witness);

                cyclic_proof = CyclicRecursionCircuit::prove(
                    l5_target_ref,
                    l5_data_ref,
                    &batch,
                    &segment_info,
                    not_first_recursion,
                    &cyclic_proof,
                    l5_dummy_ref,
                    l4_proof,
                )
                .unwrap_or_else(|err| {
                    panic!("L5_SEGMENT_CHECK: segment {k} fold of block {idx} failed: {err:?}")
                });

                // After the first fold of a segment, every subsequent fold is
                // a recursion over the previous cyclic proof.
                not_first_recursion = true;
            }

            (
                cyclic_proof,
                seg_t.elapsed().as_millis() as u64,
                segment_size,
            )
        })
        .collect();

    // ---- 6. Verify EVERY segment proof (functional acceptance). ----
    let per_segment_wall_ms: Vec<u64> = results.iter().map(|(_, ms, _)| *ms).collect();
    let segment_sizes: Vec<u64> = results.iter().map(|(_, _, sz)| *sz).collect();
    for (k, (proof, _, _)) in results.iter().enumerate() {
        l5_data.verify(proof.clone()).unwrap_or_else(|err| {
            panic!("L5_SEGMENT_CHECK: segment {k} proof failed verify: {err:?}")
        });
    }
    info!(
        "L5_SEGMENT_CHECK: all {} segment proofs verified",
        results.len()
    );
    // `results` consumed below only for length; drop the proofs explicitly to
    // free memory before reporting.
    results.clear();

    // ---- 7. Effective parallel critical path per block + event. ----
    // The parallel wall is the slowest segment; dividing by the largest
    // segment size yields the effective per-block latency a parallel cell
    // sees. Guard against an empty/degenerate denominator.
    let max_wall = per_segment_wall_ms.iter().copied().max().unwrap_or(0) as f64;
    let max_size = segment_sizes.iter().copied().max().unwrap_or(1).max(1) as f64;
    let effective_ms_per_block = max_wall / max_size;

    events::emit(&BenchEvent::L5SegmentBatch {
        layer: 5,
        name: "CyclicRecursionCircuit",
        segment_count: args.segments as u64,
        segment_sizes,
        per_segment_wall_ms,
        block_count: blocks.len() as u64,
        effective_ms_per_block,
        cpu_ms: diff_ms(bench_cpu_start, cpu_time_ms()),
        rss_mb_peak: peak_rss_mb(),
        ts: now_iso8601(),
    });

    info!(
        "L5_SEGMENT_CHECK: effective_ms_per_block={:.1} (parallel critical path; \
         total wall {:?}). The <=200 ms/block acceptance is a hardware measurement \
         gate on the #10 AMD EPYC 7B13 baseline -- run: \
         `bench --l5-segment-check --segments 8 --blocks 64` there and post the \
         number on #78.",
        effective_ms_per_block,
        bench_start.elapsed()
    );
    info!(
        "L5_SEGMENT_CHECK: L6 termination is now driven by `--l6-inner` (issue #83). \
         It pads the unused chain_proofs[S..8) slots with chain_proofs[0] and sets \
         segment_count=S for WrapperCircuit::prove_inner.",
    );
}

/// Produce a single block's L4 (`BlockCircuit`) proof by running the full
/// L3 (pre-exec) + L1/L2 (tx + chain fold) + L4 pipeline for that block.
/// Used by the #78 L5 segment scheduler, which needs one L4 proof per
/// block before the parallel L5 fold. Mirrors the structures in `main`'s
/// serial batch flow; everything here is per-block and shape-stable.
///
/// Thin delegate over `prove_block_l4_with_state` that drops the rolling
/// state captured during the final chunk update. Use
/// `prove_block_l4_with_state` directly when you need the rolling state to
/// feed `chain_next_block` and build the next chained block (#94). Kept
/// for callers that don't need the chain extension (the current in-tree
/// driver uses the with-state variant via `build_chained_blocks_and_l4_proofs`).
#[allow(clippy::too_many_arguments, dead_code)]
fn prove_block_l4(
    args: &Args,
    block: &Block<F>,
    l1_data: &CircuitData<F, C, D>,
    l1_target: &BlockTxTarget,
    l3_data: &CircuitData<F, C, D>,
    l3_target: &circuit::block_pre_execution_constraints::BlockPreExecutionTarget,
    l2_data: &CircuitData<F, C, D>,
    l2_target: &BlockTxChainTarget,
    block_tx_witness_size: usize,
    dummy_l2_circuit: &CircuitData<F, C, D>,
    dummy_l2_proof: &ProofWithPublicInputs<F, C, D>,
    l4_data: &CircuitData<F, C, D>,
    l4_target: &circuit::block_constraints::BlockTarget,
) -> ProofWithPublicInputs<F, C, D> {
    prove_block_l4_with_state(
        args,
        block,
        l1_data,
        l1_target,
        l3_data,
        l3_target,
        l2_data,
        l2_target,
        block_tx_witness_size,
        dummy_l2_circuit,
        dummy_l2_proof,
        l4_data,
        l4_target,
    )
    .0
}

/// Same signature as `prove_block_l4` (and produces the same L4 proof), but
/// additionally returns the 8 rolling-state fields captured after the final
/// L1 chunk update. Issue #94: this rolling state is exactly what
/// `chain_next_block` needs to clone the next block in a state-chained
/// sequence (the rolling state and the L4 proof's `BlockWitness` together
/// fully determine the next block's `old_*` and 8 carried fields).
#[allow(clippy::too_many_arguments)]
fn prove_block_l4_with_state(
    args: &Args,
    block: &Block<F>,
    l1_data: &CircuitData<F, C, D>,
    l1_target: &BlockTxTarget,
    l3_data: &CircuitData<F, C, D>,
    l3_target: &circuit::block_pre_execution_constraints::BlockPreExecutionTarget,
    l2_data: &CircuitData<F, C, D>,
    l2_target: &BlockTxChainTarget,
    block_tx_witness_size: usize,
    dummy_l2_circuit: &CircuitData<F, C, D>,
    dummy_l2_proof: &ProofWithPublicInputs<F, C, D>,
    l4_data: &CircuitData<F, C, D>,
    l4_target: &circuit::block_constraints::BlockTarget,
) -> (ProofWithPublicInputs<F, C, D>, Rolling) {
    // L3: pre-execution.
    let block_pre_exec = BlockPreExec::from_block(block);
    let pre_proof = BlockPreExecutionCircuit::prove(l3_data, &block_pre_exec, l3_target)
        .unwrap_or_else(|err| panic!("L5_SEGMENT_CHECK: L3 prove failed: {err:?}"));
    let pre_exec_witness = BlockPreExecWitness::from_public_inputs(&pre_proof.public_inputs);
    let state_metadata = pre_exec_witness.new_state_metadata.clone();

    // Align tx limit down to a whole number of chunks (same rule as main).
    let aligned_limit = (args.tx_limit / args.tx_per_proof) * args.tx_per_proof;
    let effective_limit =
        aligned_limit.min((block.txs.len() / args.tx_per_proof) * args.tx_per_proof);
    assert!(
        effective_limit >= args.tx_per_proof,
        "L5_SEGMENT_CHECK: block {} has too few txs for one chunk",
        block.block_number
    );
    let txs: &[_] = &block.txs[..effective_limit];

    // Mutable rolling state across L1 chunks.
    let mut all_assets = block.all_assets.clone();
    let mut all_market_details = pre_exec_witness.new_market_details.clone();
    let mut system_config = block.old_system_config;
    let mut register_stack = block.register_stack_before;
    let mut account_tree_root = block.old_account_tree_root;
    let mut account_pub_data_tree_root = block.old_account_pub_data_tree_root;
    let mut account_delta_tree_root = block.old_account_delta_tree_root;
    let mut market_tree_root = block.old_market_tree_root;
    let created_at = block.created_at;

    // L2 seed: cyclic base proof for this block's chain.
    let mut current_chain_proof = BlockTxChainCircuit::cyclic_base_proof(
        l2_data,
        dummy_l2_circuit,
        block.block_number,
        block.created_at,
        pre_exec_witness.new_state_root,
        pre_exec_witness.new_state_root,
        pre_exec_witness.new_validium_root,
        block.old_account_delta_tree_root,
        block_tx_witness_size,
        &state_metadata,
    );

    for (index, tx) in txs.chunks(args.tx_per_proof).enumerate() {
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

        let tx_proof = BlockTxCircuit::prove(l1_data, &block_tx, l1_target)
            .unwrap_or_else(|err| panic!("L5_SEGMENT_CHECK: L1 chunk {index} failed: {err:?}"));

        let tx_witness = BlockTxWitness::from_public_inputs(&tx_proof.public_inputs);
        all_assets = tx_witness.all_assets_after.clone();
        all_market_details = tx_witness.all_market_details_after.clone();
        register_stack = tx_witness.register_stack_after;
        system_config = tx_witness.new_system_config;
        account_tree_root = tx_witness.new_account_tree_root;
        account_pub_data_tree_root = tx_witness.new_account_pub_data_tree_root;
        account_delta_tree_root = tx_witness.new_account_delta_tree_root;
        market_tree_root = tx_witness.new_market_tree_root;

        current_chain_proof = BlockTxChainCircuit::prove(
            l2_target,
            l2_data,
            index as u64,
            &current_chain_proof,
            dummy_l2_proof,
            &tx_proof,
        )
        .unwrap_or_else(|err| panic!("L5_SEGMENT_CHECK: L2 fold {index} failed: {err:?}"));
    }

    // L4: connect the (possibly partial) chain run to the block witness, then
    // prove. Same partial-block patch trick as `run_l4_check`.
    let cw = BlockTxChainWitness::from_public_inputs(&current_chain_proof.public_inputs, 1, 1);
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

    let pw = BlockCircuit::generate_witness(l4_target, &pblock, &pre_proof, &current_chain_proof)
        .unwrap_or_else(|err| panic!("L5_SEGMENT_CHECK: L4 witness gen failed: {err:?}"));
    let l4_proof = l4_data
        .prove(pw)
        .unwrap_or_else(|err| panic!("L5_SEGMENT_CHECK: L4 prove failed: {err:?}"));

    let rolling = Rolling {
        all_assets,
        all_market_details,
        register_stack,
        system_config,
        account_tree_root,
        account_pub_data_tree_root,
        account_delta_tree_root,
        market_tree_root,
        // L3-output metadata: this is what block 0's L2 hashed into its
        // `new_state_root`. The L3 timestamp gates (line ~621-642 in
        // `block_pre_execution_constraints.rs`) are closed for the
        // chained fixture (created_at advances by 1 s; gate is
        // `current_block_time >= next_gate_time` for funding/oracle/
        // premium), so `new_state_metadata == state_metadata` in
        // practice -- but we carry the L3 output explicitly so the next
        // block's L3 input is provably the same value.
        state_metadata,
    };
    (l4_proof, rolling)
}

/// Issue #94: shared helper that builds the `n_blocks`-long state-chained
/// block sequence and proves one L4 per block, returning both vectors in
/// lock-step. The L4 loop is the only place that captures the rolling
/// state and the L4 `BlockWitness` -- both feed `chain_next_block` to
/// produce block `i+1` immediately after block `i`'s L4 prove finishes.
///
/// Per-block tx slice width is `args.tx_per_proof` (one L1 chunk per
/// block), which keeps `--blocks 64 --segments 8` runnable inside the
/// 500-tx fixture (64 * 4 = 256 <= 500) at the default `tx_per_proof=4`.
/// The total-tx budget is enforced by the parse-time guard
/// (`blocks * tx_per_block <= DEFAULT_TX_LIMIT = 480`), so callers can
/// trust the slice arithmetic stays within both the fixture and the
/// repo's tx-limit ceiling.
///
/// Used by both `run_l5_segment_check` (#78) and `run_l5_tree_fold` (#82)
/// so the two L5 driver paths build identical chained fixtures.
#[allow(clippy::too_many_arguments)]
fn build_chained_blocks_and_l4_proofs(
    args: &Args,
    base_block: &Block<F>,
    n_blocks: usize,
    l1_data: &CircuitData<F, C, D>,
    l1_target: &BlockTxTarget,
    l3_data: &CircuitData<F, C, D>,
    l3_target: &circuit::block_pre_execution_constraints::BlockPreExecutionTarget,
    l2_data: &CircuitData<F, C, D>,
    l2_target: &BlockTxChainTarget,
    block_tx_witness_size: usize,
    dummy_l2_circuit: &CircuitData<F, C, D>,
    dummy_l2_proof: &ProofWithPublicInputs<F, C, D>,
    l4_data: &CircuitData<F, C, D>,
    l4_target: &circuit::block_constraints::BlockTarget,
    label: &str,
) -> (Vec<Block<F>>, Vec<ProofWithPublicInputs<F, C, D>>) {
    assert!(n_blocks >= 1, "{label}: n_blocks must be >= 1");
    // One L1 chunk per block; the parse-time guard already enforced
    // `n_blocks * tx_per_block <= DEFAULT_TX_LIMIT <= base.txs.len()`.
    let tx_per_block = args.tx_per_proof;

    let mut blocks: Vec<Block<F>> = Vec::with_capacity(n_blocks);
    let mut l4_proofs: Vec<ProofWithPublicInputs<F, C, D>> = Vec::with_capacity(n_blocks);

    // Block 0 is the base block sliced down to its first `tx_per_block` txs
    // so the L1/L2 driver sees the same per-block workload as every other
    // chained block. (`prove_block_l4_with_state` aligns down to the
    // largest multiple of `tx_per_proof`, so unsliced this would happily
    // run all 500 txs and the chain math would over-budget the fixture.)
    let mut block0 = base_block.clone();
    let block0_slice_end = tx_per_block.min(block0.txs.len());
    block0.txs = block0.txs[..block0_slice_end].to_vec();
    blocks.push(block0);

    info!(
        "{label}: proving {} per-block L4 proofs over a tx-sliced chained fixture \
         (tx_per_block={}, total_txs_used={})",
        n_blocks,
        tx_per_block,
        n_blocks * tx_per_block,
    );

    for i in 0..n_blocks {
        let l4_t = Instant::now();
        let (l4_proof, rolling) = prove_block_l4_with_state(
            args,
            &blocks[i],
            l1_data,
            l1_target,
            l3_data,
            l3_target,
            l2_data,
            l2_target,
            block_tx_witness_size,
            dummy_l2_circuit,
            dummy_l2_proof,
            l4_data,
            l4_target,
        );
        info!(
            "{label}: L4 proof {}/{} (block {}) in {:?}",
            i + 1,
            n_blocks,
            blocks[i].block_number,
            l4_t.elapsed()
        );

        // Issue #94: patch the `new_*` fields of `blocks[i]` to match what
        // the L4 proof actually produced. `prove_block_l4_with_state`
        // applies the partial-block patch only to its internal `pblock`,
        // not the caller's block; without this re-patch, the `host_prepass`
        // call below in `run_l5_segment_check` would feed `aggregate_block`
        // the STALE base-fixture `new_*` values via `BlockWitness::from_block`,
        // tripping `Batch::aggregate_block`'s state-root continuity assert
        // at the second segment seam (segments k >= 2 fold >=2 prefix
        // blocks). Re-read the partial-block patch from the L4 BlockWitness
        // (parsed from the proof's PIs) so the host view of `blocks[i]`
        // matches the in-circuit view exactly.
        let bw = BlockWitness::from_public_inputs(&l4_proof.public_inputs, 1, 1);
        {
            let b = &mut blocks[i];
            b.new_validium_root = bw.new_validium_root;
            b.new_state_root = bw.new_state_root;
            b.new_account_delta_tree_root = bw.new_account_delta_tree_root;
            b.on_chain_operations_count = bw.on_chain_operations_count;
            b.on_chain_operations_pub_data = bw.on_chain_operations_pub_data.clone();
            b.priority_operations_count = bw.priority_operations_count;
            b.new_prefix_priority_operation_hash = bw.new_prefix_priority_operation_hash;
            b.new_public_market_details = bw.new_public_market_details.clone();
        }

        // Chain the next block immediately, while the rolling state and
        // the L4 BlockWitness are in scope. Skip the extension on the
        // last block.
        if i + 1 < n_blocks {
            let next = chain_next_block(base_block, i, tx_per_block, &rolling, &bw);
            blocks.push(next);
        }
        l4_proofs.push(l4_proof);
    }

    (blocks, l4_proofs)
}

/// Issue #82 + #94: pre-L5 block-proof aggregation tree-fold driver.
///
/// Builds L4 -> L5 (`CyclicRecursionCircuit`) -> `BatchMergeCircuit`, asserts
/// the self-shape gate `merge.common == l5.common`, lives-proves one L5 fold
/// per block of the chained fixture built by
/// `build_chained_blocks_and_l4_proofs` (#94), then wires the pairwise log-depth
/// tree fold over those real per-block L5 proofs using the merged PR #96
/// `BatchMergeCircuit::generate_witness` path. Carries odd proofs up a level,
/// mirroring `--l2-fold tree`. With `--l5-ab-check` it also compares the tree
/// root vs the L5 serial fold element-wise on the semantic PI surface.
#[allow(clippy::too_many_arguments)]
fn run_l5_tree_fold(
    args: &Args,
    block: &Block<F>,
    l1_data: &CircuitData<F, C, D>,
    l1_target: &BlockTxTarget,
    l3_data: &CircuitData<F, C, D>,
    l3_target: &circuit::block_pre_execution_constraints::BlockPreExecutionTarget,
    l2_data: &CircuitData<F, C, D>,
    l2_target: &BlockTxChainTarget,
    block_tx_witness_size: usize,
    dummy_l2_circuit: &CircuitData<F, C, D>,
    dummy_l2_proof: &ProofWithPublicInputs<F, C, D>,
    bench_start: Instant,
    bench_cpu_start: Option<u64>,
) {
    // ---- Build the L4 -> L5 -> merge circuit chain.
    let l4_define_t = Instant::now();
    let l4 = BlockCircuit::define(CIRCUIT_CONFIG, l3_data, l2_data, 1);
    let l4_target = l4.target;
    let l4_data = l4.builder.build::<C>();
    info!(
        "L5_TREEFOLD: L4 BlockCircuit defined+built in {:?} (degree 2^{})",
        l4_define_t.elapsed(),
        l4_data.common.degree_bits()
    );

    let l5_define_t = Instant::now();
    let l5_circuit = CyclicRecursionCircuit::define(CIRCUIT_CONFIG, &l4_data, 1);
    let l5_data = l5_circuit.builder.build::<C>();
    events::emit(&BenchEvent::CircuitDefine {
        layer: 5,
        name: "CyclicRecursionCircuit",
        wall_ms: l5_define_t.elapsed().as_millis() as u64,
        rss_mb_after: current_rss_mb(),
        ts: now_iso8601(),
    });
    info!(
        "L5_TREEFOLD: L5 CyclicRecursionCircuit defined+built in {:?} (degree 2^{}, {} public inputs)",
        l5_define_t.elapsed(),
        l5_data.common.degree_bits(),
        l5_data.common.num_public_inputs
    );

    let merge_define_t = Instant::now();
    let merge_circuit = BatchMergeCircuit::define(CIRCUIT_CONFIG, &l5_data);
    let merge_data = merge_circuit.builder.build::<C>();
    events::emit(&BenchEvent::CircuitDefine {
        layer: 5,
        name: "BatchMergeCircuit",
        wall_ms: merge_define_t.elapsed().as_millis() as u64,
        rss_mb_after: current_rss_mb(),
        ts: now_iso8601(),
    });
    info!(
        "L5_TREEFOLD: BatchMergeCircuit defined+built in {:?} (degree 2^{}, {} public inputs)",
        merge_define_t.elapsed(),
        merge_data.common.degree_bits(),
        merge_data.common.num_public_inputs
    );

    // ---- Self-shape gate (issue #82 acceptance). The merge circuit must build
    // into the L5 cyclic circuit's EXACT shape (itself the goal-asserted 2^15
    // fixed point). This build-time equality is what guarantees the merge
    // node's PI surface is consumable anywhere an L5 proof is, AND that the
    // node fits inside L5's 2^15 budget.
    assert!(
        merge_data.common == l5_data.common,
        "BatchMergeCircuit must build into the L5 cyclic circuit's exact self-shape \
         (issue #82); see Builder::verify_leaf_or_cyclic_proof docs. \
         merge: degree 2^{} / {} PIs, l5: degree 2^{} / {} PIs",
        merge_data.common.degree_bits(),
        merge_data.common.num_public_inputs,
        l5_data.common.degree_bits(),
        l5_data.common.num_public_inputs,
    );
    info!(
        "L5_TREEFOLD: self-shape gate PASS -- BatchMergeCircuit.common == L5.common \
         (fixed point closed; merge node fits L5's 2^{} budget)",
        l5_data.common.degree_bits()
    );

    // ---- L5 dummy proof (over the L5 common data) for the conditional cyclic
    // verifier slot, used by `CyclicRecursionCircuit::prove`.
    let l5_dummy_circuit = dummy_circuit(&l5_data.common);
    let l5_dummy_proof = cyclic_base_proof(
        &l5_data.common,
        &l5_data.verifier_only,
        &l5_dummy_circuit,
        Vec::<F>::new().iter().copied().enumerate().collect(),
    )
    .expect("L5_TREEFOLD: L5 dummy proof");

    // ---- Issue #94: build the genuinely state-chained fixture and prove
    // one L4 per block, then one single-block L5 fold per block to produce
    // the per-block L5 leaf proofs that feed the tree.
    let num_leaves = args.blocks.max(4); // >=4 leaves exercises multiple tree levels (#82 acceptance)
    let (blocks, l4_proofs) = build_chained_blocks_and_l4_proofs(
        args,
        block,
        num_leaves,
        l1_data,
        l1_target,
        l3_data,
        l3_target,
        l2_data,
        l2_target,
        block_tx_witness_size,
        dummy_l2_circuit,
        dummy_l2_proof,
        &l4_data,
        &l4_target,
        "L5_TREEFOLD",
    );

    info!(
        "L5_TREEFOLD: proving {} per-block L5 leaf proofs (one single-block fold each)...",
        num_leaves
    );

    // Per-block single-block L5 `Batch` (host mirror) and `SegmentInfo`
    // start-digest. Leaf k's segment starts where the running on-chain-ops
    // keccak hash sits after folding all preceding blocks. Leaf 0 starts at
    // the zero digest (first segment is empty, per wrapper_circuit.rs:160-165).
    let mut leaf_proofs: Vec<ProofWithPublicInputs<F, C, D>> = Vec::with_capacity(num_leaves);
    let mut leaf_batches: Vec<Batch<F>> = Vec::with_capacity(num_leaves);
    let mut leaf_segments: Vec<SegmentInfo> = Vec::with_capacity(num_leaves);
    let mut running_on_chain = [0u8; KECCAK_HASH_OUT_BYTE_SIZE];

    for (i, l4_proof) in l4_proofs.iter().enumerate() {
        // SegmentInfo for this single-block L5 chain: anchored at the
        // running on-chain-ops digest accumulated by all preceding blocks.
        let segment_info = SegmentInfo {
            old_on_chain_operations_pub_data_hash: running_on_chain,
        };

        // Host mirror of the single-block L5 Batch: a fresh Batch absorbing
        // exactly one block's L4 BlockWitness (same aggregation the
        // in-circuit fold computes).
        let block_witness = BlockWitness::from_public_inputs(&l4_proof.public_inputs, 1, 1);
        let mut batch = Batch::<F>::default();
        batch.aggregate_block(&block_witness);

        // Live L5 prove: fresh cyclic base proof seeded with the segment
        // info + one fold (`not_first_recursion = false`).
        let cyclic_base = CyclicRecursionCircuit::cyclic_base_proof(&l5_data, &segment_info);
        let leaf_t = Instant::now();
        let l5_proof = CyclicRecursionCircuit::prove(
            &l5_circuit.target,
            &l5_data,
            &batch,
            &segment_info,
            false,
            &cyclic_base,
            &l5_dummy_proof,
            l4_proof,
        )
        .unwrap_or_else(|err| panic!("L5_TREEFOLD: leaf L5 prove for block {i} failed: {err:?}"));
        info!(
            "L5_TREEFOLD: L5 leaf {}/{} (block {}) in {:?}",
            i + 1,
            num_leaves,
            blocks[i].block_number,
            leaf_t.elapsed()
        );

        // Advance the running on-chain-ops digest for the next leaf.
        running_on_chain = batch.on_chain_operations_pub_data_hash;

        leaf_proofs.push(l5_proof);
        leaf_batches.push(batch);
        leaf_segments.push(segment_info);
    }

    info!(
        "L5_TREEFOLD: {} live L5 leaf proofs built (contiguous block range {}..={})",
        num_leaves,
        leaf_batches
            .first()
            .map(|b| b.end_block_number)
            .unwrap_or(0),
        leaf_batches.last().map(|b| b.end_block_number).unwrap_or(0),
    );

    // ---- Pairwise live merge up the tree. Each entry carries
    // (proof, batch, segment, is_merge); the BatchMergeCircuit live-proves
    // each pairwise merge using PR #96 (`generate_witness`) to populate the
    // merged Batch/SegmentInfo PIs.
    let mut level: Vec<(ProofWithPublicInputs<F, C, D>, Batch<F>, SegmentInfo, bool)> = leaf_proofs
        .into_iter()
        .zip(leaf_batches.iter().cloned())
        .zip(leaf_segments.iter().cloned())
        .map(|((p, b), s)| (p, b, s, false))
        .collect();

    let mut depth = 0usize;
    let mut merges = 0usize;
    while level.len() > 1 {
        depth += 1;
        let mut next = Vec::with_capacity(level.len() / 2 + 1);
        let mut iter = level.into_iter();
        while let Some((left_proof, left_batch, left_segment, left_is_merge)) = iter.next() {
            match iter.next() {
                Some((right_proof, right_batch, right_segment, right_is_merge)) => {
                    // Host mirror: stitched segment + merged batch must
                    // match exactly what the in-circuit merge computes.
                    let stitched_segment = left_segment
                        .try_stitch(&right_segment, &left_batch.on_chain_operations_pub_data_hash)
                        .unwrap_or_else(|err| {
                            panic!("L5_TREEFOLD merge #{merges} (level {depth}) seam stitch failed: {err:?}")
                        });
                    let merged_batch = left_batch
                        .try_merge_consecutive(&right_batch)
                        .unwrap_or_else(|err| {
                            panic!("L5_TREEFOLD merge #{merges} (level {depth}) batch merge failed: {err:?}")
                        });

                    // Live merge prove.
                    let merge_t = Instant::now();
                    let merge_proof = BatchMergeCircuit::prove(
                        &merge_circuit.target,
                        &merge_data,
                        &left_proof,
                        left_is_merge,
                        &right_proof,
                        right_is_merge,
                    )
                    .unwrap_or_else(|err| {
                        panic!(
                            "L5_TREEFOLD merge #{merges} (level {depth}) live prove failed: {err:?}"
                        )
                    });
                    info!(
                        "L5_TREEFOLD: merge #{} (level {}) live-proved in {:?}",
                        merges,
                        depth,
                        merge_t.elapsed()
                    );

                    merges += 1;
                    next.push((merge_proof, merged_batch, stitched_segment, true));
                }
                None => {
                    info!("L5_TREEFOLD level {depth}: odd proof carried up to the next level");
                    next.push((left_proof, left_batch, left_segment, left_is_merge));
                }
            }
        }
        level = next;
    }
    let (root_proof, root_batch, root_segment, root_is_merge) =
        level.pop().expect("L5 tree fold produced no root");

    // Functional acceptance: the root proof verifies against the data
    // whose VK is embedded in the root. `merge.common == l5.common` (the
    // #82 self-shape gate), but `merge.verifier_only != l5.verifier_only`,
    // so we must select the correct verifier_data based on whether the
    // root came out of a merge or a leaf L5 prove. (BatchMergeCircuit::prove
    // also runs an internal verify, so the merge path is double-verified
    // here; the leaf-only path needs this explicit verify.)
    let (root_verify_data, root_verify_label) = if root_is_merge {
        (&merge_data, "merge VK")
    } else {
        (&l5_data, "L5 leaf VK")
    };
    root_verify_data
        .verify(root_proof.clone())
        .unwrap_or_else(|err| {
            panic!("L5_TREEFOLD: root proof verify failed ({root_verify_label}): {err:?}")
        });

    info!(
        "L5_TREEFOLD wired+proved: leaves={} depth={} merges={} root_block_range={}..={} \
         root_batch_size={} root_verify=PASS",
        num_leaves,
        depth,
        merges,
        root_batch
            .end_block_number
            .saturating_sub(root_batch.batch_size),
        root_batch.end_block_number,
        root_batch.batch_size,
    );

    // ---- A/B: serial L5 fold over the SAME leaves (host-mirror only --
    // the live serial L5 fold is exercised by `--l5-segment-check
    // --segments 1`). Root semantic PIs of the host-mirror serial fold
    // must equal the live tree root's semantic PIs.
    if args.l5_ab_check {
        info!(
            "L5_AB_CHECK: running host-mirror serial L5 fold over the same {} leaves...",
            num_leaves
        );
        let mut serial_batch = leaf_batches[0].clone();
        for right in &leaf_batches[1..] {
            serial_batch = serial_batch
                .try_merge_consecutive(right)
                .unwrap_or_else(|err| panic!("L5_AB_CHECK serial step failed: {err:?}"));
        }
        let serial_segment = leaf_segments[0].clone();

        let tree_pis = batch_segment_public_inputs(&root_batch, &root_segment);
        let serial_pis = batch_segment_public_inputs(&serial_batch, &serial_segment);
        assert_eq!(
            tree_pis.len(),
            serial_pis.len(),
            "L5_AB_CHECK: semantic PI lengths differ"
        );
        let mismatches: Vec<usize> = (0..tree_pis.len())
            .filter(|&i| tree_pis[i] != serial_pis[i])
            .collect();
        if mismatches.is_empty() {
            info!(
                "L5_AB_CHECK PASS: all {} semantic public inputs element-wise equal \
                 (trailing verifier-key PIs differ by design: L5 leaf VK vs merge VK)",
                tree_pis.len()
            );
        } else {
            eprintln!(
                "L5_AB_CHECK FAIL: {} of {} semantic public inputs differ; first mismatching indices: {:?}",
                mismatches.len(),
                tree_pis.len(),
                &mismatches[..mismatches.len().min(16)]
            );
            std::process::exit(1);
        }
    }

    info!(
        "L5_TREEFOLD DONE: live ≥4-leaf tree-fold proved on the genuinely state-chained fixture \
         (#94 + PR #96, commit 351363d). Root proof verified against the L5 self-shape."
    );

    let total_wall_ms = bench_start.elapsed().as_millis() as u64;
    let total_cpu_ms = diff_ms(bench_cpu_start, cpu_time_ms());
    events::emit(&BenchEvent::Summary {
        tx_per_proof: args.tx_per_proof,
        tx_limit: args.tx_limit,
        chunks: num_leaves,
        total_wall_ms,
        total_cpu_ms,
        peak_rss_mb: peak_rss_mb(),
        ts: now_iso8601(),
    });
}

/// Concatenate a `Batch`'s and a `SegmentInfo`'s public inputs in L5 layout
/// order (Batch first, then SegmentInfo), for the A/B semantic comparison.
fn batch_segment_public_inputs(batch: &Batch<F>, segment: &SegmentInfo) -> Vec<F> {
    let mut pis = batch_public_inputs(batch);
    pis.extend(segment.to_public_inputs());
    pis
}

/// Reconstruct a `Batch`'s public-input vector in the L5 layout used by
/// `Batch::from_public_inputs` (the inverse of that parser, for the
/// semantic A/B comparison). Only the fields that participate in the merge
/// semantics are compared; the layout matches the in-circuit registration.
fn batch_public_inputs(batch: &Batch<F>) -> Vec<F> {
    use circuit::types::constants::POSITION_LIST_SIZE;

    let mut pis = vec![
        F::from_canonical_u64(batch.end_block_number),
        F::from_canonical_u64(batch.batch_size),
        F::from_canonical_i64(batch.first_created_at),
        F::from_canonical_i64(batch.last_created_at),
    ];
    pis.extend_from_slice(&batch.old_state_root.elements);
    pis.extend_from_slice(&batch.new_validium_root.elements);
    pis.extend_from_slice(&batch.new_state_root.elements);
    pis.extend_from_slice(&batch.old_account_delta_tree_root.elements);
    pis.extend_from_slice(&batch.new_account_delta_tree_root.elements);
    // Market details: 5 field elements per entry (sign, abs lo, abs hi,
    // mark_price, quote_multiplier), matching `Batch::from_public_inputs`.
    for md in batch
        .new_public_market_details
        .iter()
        .take(POSITION_LIST_SIZE)
    {
        let (sign, abs) = {
            use num::Signed;
            let abs = md.funding_rate_prefix_sum.abs();
            let abs_u64 = abs.to_u64_digits().1.first().copied().unwrap_or(0);
            let sign = if md.funding_rate_prefix_sum.is_negative() {
                2u64
            } else {
                1u64
            };
            (sign, abs_u64)
        };
        pis.push(F::from_canonical_u64(sign));
        pis.push(F::from_canonical_u64(abs & 0xFFFF_FFFF));
        pis.push(F::from_canonical_u64(abs >> 32));
        pis.push(F::from_canonical_u32(md.mark_price));
        pis.push(F::from_canonical_u32(md.quote_multiplier));
    }
    pis.extend(
        batch
            .on_chain_operations_pub_data_hash
            .iter()
            .map(|&b| F::from_canonical_u8(b)),
    );
    pis.push(F::from_canonical_u64(batch.priority_operations_count));
    pis.extend(
        batch
            .old_prefix_priority_operation_hash
            .iter()
            .map(|&b| F::from_canonical_u8(b)),
    );
    pis.extend(
        batch
            .new_prefix_priority_operation_hash
            .iter()
            .map(|&b| F::from_canonical_u8(b)),
    );
    pis
}

/// Issue #157 (spike): per-chunk tx-type attribution. Returns
/// `(Some(tx_types), homogeneous)` where `tx_types` lists each tx's
/// `tx_type` in chunk order, and `homogeneous = Some(t)` iff every tx
/// in the chunk shares `tx_type == t`. Only invoked when the caller
/// opts in via `--group-by-tx-type`; the default path returns
/// `(None, None)` upstream and keeps the JSON shape pre-#157 identical.
fn chunk_tx_type_attribution(chunk: &[tx::Tx<F>]) -> (Option<Vec<u8>>, Option<u8>) {
    if chunk.is_empty() {
        return (Some(Vec::new()), None);
    }
    let types: Vec<u8> = chunk.iter().map(|t| t.tx_type).collect();
    let first = types[0];
    let homogeneous = if types.iter().all(|&t| t == first) {
        Some(first)
    } else {
        None
    };
    (Some(types), homogeneous)
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
