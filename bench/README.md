The `bench` executable is produced locally by `make build` (which moves `../target/release/bench` to `bench/bench`) — it is a per-machine build artifact and is **not** shipped in the repository. Once built, it and `bench_test.json` can be copied to another machine *of the same architecture* (placed at the same level) and run independently from the repository.

## Supported `--tx-per-proof` range

This bench is validated for `--tx-per-proof ∈ {1..=32}` (building and
proving). The previous cap of 6 was caused by a chain-recursion circuit
sizing bug — a goal-vs-actual `CommonCircuitData` mismatch from the
upstream `log_gates = 14` bump, fixed in
[issue #63](https://github.com/kunallimaye/lighter-prover/issues/63)
(supersedes the analysis in
[issue #8](https://github.com/kunallimaye/lighter-prover/issues/8)).
Values above 32 are unvalidated and rejected at startup.

For throughput, `--tx-per-proof 20` is the measured optimum for 500-tx
blocks: 12.8 s block wall vs 40.6 s at the old cap of 6 (sweep
measurements on
[issue #60](https://github.com/kunallimaye/lighter-prover/issues/60)).

The default `--tx-per-proof 4` matches upstream's production setting
(`bench/src/bin/bench.rs:33`, `build_circuits.sh:21`).

## Structured output

Alongside the existing `info!()` log lines (`TOTAL ...`, `AVERAGE ...`,
`BENCH_META ...`), the bench emits structured per-layer measurements on
stdout as JSON Lines. Each event is a single line prefixed with
`BENCH_EVENT ` (so it is easy to `grep` out of the normal log stream)
followed by a JSON object. stdout is flushed after every event, so
partial output survives a later crash (e.g. an OOM during proving).

### Event types

| `event`           | When                                              | Notable fields                                                                                                                                  |
|-------------------|---------------------------------------------------|--------------------------------------------------------------------------------------------------------------------------------------------------|
| `circuit_define`  | After each circuit's `define + build` step        | `layer`, `name`, `wall_ms`, `rss_mb_after`, `ts`                                                                                                |
| `layer_prove`     | After each `*::prove(...)` call (L1, L2, L3; in #67 tree mode also L2 merges, and L4 with `--l4-check`) | `layer`, `name`, `chunk_idx`, `chunk_total`, `tx_per_proof`, `wall_ms`, `cpu_ms`, `rss_mb_peak`, `rss_mb_after`, `ts`                            |
| `summary`         | Once at end of `main`                             | `tx_per_proof`, `tx_limit`, `chunks`, `total_wall_ms`, `total_cpu_ms`, `peak_rss_mb`, `ts`                                                      |
| `l5_segment_batch` | Once after the `--l5-segment-check` (#78) run     | `segment_count`, `segment_sizes`, `per_segment_wall_ms`, `block_count`, `effective_ms_per_block`, `cpu_ms`, `rss_mb_peak`, `ts` (see the L5 segment scheduler section) |

For L3 (`BlockPreExecutionCircuit`, one-shot) the `chunk_idx` and
`chunk_total` fields are explicitly `null`. For L1/L2 (per-chunk) they
are non-null `0..chunk_total`.

`cpu_ms`, `rss_mb_peak`, `rss_mb_after`, `peak_rss_mb`, and
`total_cpu_ms` are Linux-only (parsed from `/proc/self/status` /
`getrusage`). On non-Linux platforms they serialize as `null`. `ts`
is UTC in `YYYY-MM-DDTHH:MM:SSZ` form.

### Example events

```jsonl
BENCH_EVENT {"event":"circuit_define","layer":1,"name":"BlockTxCircuit","wall_ms":92340,"rss_mb_after":2400,"ts":"2026-06-10T03:08:22Z"}
BENCH_EVENT {"event":"layer_prove","layer":3,"name":"BlockPreExecutionCircuit","chunk_idx":null,"chunk_total":null,"tx_per_proof":4,"wall_ms":542,"cpu_ms":4120,"rss_mb_peak":1280,"rss_mb_after":1240,"ts":"2026-06-10T03:09:11Z"}
BENCH_EVENT {"event":"layer_prove","layer":1,"name":"BlockTxCircuit","chunk_idx":5,"chunk_total":120,"tx_per_proof":4,"wall_ms":2310,"cpu_ms":18432,"rss_mb_peak":2920,"rss_mb_after":2900,"ts":"2026-06-10T03:14:22Z"}
BENCH_EVENT {"event":"layer_prove","layer":2,"name":"BlockTxChainCircuit","chunk_idx":5,"chunk_total":120,"tx_per_proof":4,"wall_ms":498,"cpu_ms":3960,"rss_mb_peak":2925,"rss_mb_after":2910,"ts":"2026-06-10T03:14:23Z"}
BENCH_EVENT {"event":"summary","tx_per_proof":4,"tx_limit":480,"chunks":120,"total_wall_ms":345200,"total_cpu_ms":10984000,"peak_rss_mb":2925,"ts":"2026-06-10T03:15:00Z"}
```

### Aggregating with `jq`

Strip the prefix, parse as JSON, then aggregate. For example, mean
wall-clock per layer across all chunks:

```bash
grep '^BENCH_EVENT ' bench.log \
  | sed 's/^BENCH_EVENT //' \
  | jq -s '[.[] | select(.event=="layer_prove")]
           | group_by(.layer)
           | map({layer: .[0].layer,
                  n: length,
                  avg_wall_ms: (map(.wall_ms) | add/length)})'
```

Or just the summary line:

```bash
grep '^BENCH_EVENT ' bench.log | sed 's/^BENCH_EVENT //' \
  | jq 'select(.event=="summary")'
```

## Feeder (trace producer)

`bench/feeder/feeder.py` produces JSONL block-event streams for the
streaming bench. The producer/consumer contract — schema, gap markers,
provenance header, monotonicity rules, and policies P1–P4 — is pinned in
[`trace-format.md`](trace-format.md); every emitted stream conforms to it.
Witnesses never flow through the feeder: it carries only block cadence
and tx counts; the consumer (`bench --stream`, issue #49) sources witness
data itself.

| Subcommand | Mode | Summary |
|---|---|---|
| `record` | network | Capture live chain cadence: WS height channel (`wss://mainnet.zklighter.elliot.ai/stream?readonly=true`) merged with explorer tx counts via a ~4 s late-bind watermark; unmatched heights emit `tx_count: null` |
| `replay` | offline | Re-emit a recorded trace retimed (`--speed N` or `--target-rate TXS`; `--loop`, `--duration`, `--dry-run`) |
| `synth-peak` | offline, no inputs | Fabricate an idealized trace from a rate: back-to-back 500-tx blocks at cadence `500/rate` s |
| `peak-hours` | analysis helper | Locate peak windows from the explorer hourly tx stats (top-N hours by tx/s) |

Make targets (from `bench/`): `stream-record OUT=... [DURATION=...]`,
`stream-replay TRACE=... [SPEED=N | RATE=TXS] [DURATION=...]`,
`stream-peak RATE=... DURATION=...`, and `feeder-test` (offline suite,
no network, <1 min; also wired into the repo-root `make local-test`).

Dependencies: `record` and `peak-hours` need `bench/feeder/requirements.txt`
(`websockets`, `requests`); `replay`, `synth-peak`, and the tests are pure
Python 3 stdlib.

Geo-block note: the chain's main REST API returns 403 to US IPs. The
feeder avoids it entirely — the WS stream with `?readonly=true` and the
explorer API (`explorer.elliot.ai`) are not geo-blocked. The explorer
blocks endpoint is rate-limited to 90 req/min per IP; `record` polls at
~85 req/min with an identifying User-Agent.

## Tree-fold mode (bench --l2-fold tree)

`--l2-fold tree` (issue #67, ADR-0003 §D3) replaces the serial L2 fold
with a log-depth tree: each chunk gets a LEAF chain proof (a 1-chunk
chain seeded at the chunk's pre-state), then adjacent proofs are merged
pairwise by the dedicated `BlockTxChainMergeCircuit` (same 2^14 shape
and PI surface as the leaf circuit; odd proofs at any level carry up
unchanged). The default `--l2-fold serial` is byte-for-byte today's
behavior. The run ends with a `TREEFOLD` summary line: depth, merges,
leaf/merge averages, and the critical-path latency (depth × avg merge —
the serial-L2 wall a parallel cell would see).

Execution is sequential by default; with `--l2-workers M` (issue #73,
ADR-0003 §D1) the driver dispatches the leaves and per-level merges
across M worker threads sharing the resident `CircuitData` -- see
[Intra-cell parallel scheduler](#intra-cell-parallel-scheduler-bench---l2-workers-m)
below.

| Flag | Default | Meaning |
|------|---------|---------|
| `--l2-fold serial\|tree` | `serial` | L2 fold strategy (batch mode only) |
| `--l2-workers M` | `1` | Tree mode (#73): M worker threads sharing the resident `CircuitData`. `1` keeps the serial driver byte-for-byte. |
| `--ab-check` | off | Tree mode: also serial-fold the same L1 proofs and assert element-wise equality of the two final proofs' semantic public inputs (#67 acceptance) |
| `--l4-check` | off | After the fold, define+prove+verify L4 (`BlockCircuit`) against the final chain proof — the merge circuit's data in tree mode (#67 acceptance) |

```bash
./bench --tx-per-proof 4 --tx-limit 32 --l2-fold tree --ab-check --l4-check
```

### Intra-cell parallel scheduler (bench --l2-workers M)

`--l2-workers M` (issue #73, ADR-0003 §D1) realizes the
critical-path latency that the sequential tree-fold only *reports*.
A cell is one host, one Rust process, M worker threads sharing the
resident proving keys (multi-GB `CircuitData`, built once and held by
reference across workers — not Arc-cloned). Sibling processes would
multiply the key RSS by M, which the cell topology rules out.

Default `M = 1` keeps the historical serial driver byte-for-byte (zero
regression). For `M > 1` the driver:

1. Builds a dedicated rayon `ThreadPool` of M workers
   (`l2-worker-{0..M-1}`).
2. Phase 2 (leaf chain proofs): all leaves are dispatched as a single
   `par_iter` into the M-worker pool. Leaves are order-free post-#72,
   so the proving order is a free parameter.
3. Phase 3 (merges): each level is dispatched as a `par_iter` over the
   level's pairs into the M-worker pool. Odd proofs carry up unchanged
   (the merge circuit accepts leaf and merge children in any mix).

**Open question (issue #73, ADR-0003 §D1):** plonky2 already saturates
all cores per proof via the *global* rayon pool, so M concurrent proves
contend for cores. The sweep `M ∈ {1,2,4,8,16}` at S=4 (`--tx-limit
32`) and S=20 (`--tx-limit 500`) measures the real M / wall-clock
curve; the `l2_tree_schedule` event in the JSONL stream is the
headline.

`--ab-check` PASSes at every `M`: every chunk's seed is witness-native
(#72) and the merge circuit is associative on the semantic PI surface,
so the proving order has no effect on the root.

Two new BENCH_EVENT lines are emitted (additive — pre-#73 consumers
ignore unknown events):

| `event` | When | Notable fields |
|---------|------|----------------|
| `l2_tree_level` | Once per tree level (level `0` = leaves; `1..depth` = merge levels) | `level`, `nodes`, `level_wall_ms` (level start-to-end), `node_wall_sum_ms`, `node_wall_max_ms`, `node_wall_min_ms`, `workers`, `rss_mb_*` |
| `l2_tree_schedule` | Once at the end of the tree fold | `workers`, `leaves`, `depth`, `merges`, `leaves_wall_ms`, `merges_wall_ms`, `realized_wall_ms` (the headline), `critical_path_ms` (`depth × avg merge` — the pre-#73 metric), `leaf_avg_ms`, `merge_avg_ms`, `rss_mb_*` |

```bash
# M sweep at S=4, --tx-limit 32 (PR #69's bench fixture):
for M in 1 2 4 8 16; do
  ./bench --tx-per-proof 4 --tx-limit 32 --l2-fold tree --ab-check \
          --l2-workers "$M" 2>&1 | grep -E "L2_SCHEDULE|TREEFOLD|AB_CHECK PASS"
done
```

## L5 segment scheduler (bench --l5-segment-check)

`--l5-segment-check` (issue #78) runs the 8-way L5
(`CyclicRecursionCircuit`) segment-parallel scheduler. The wrapper circuit
(`NUM_CHAINS_PER_BATCH = 8`) is designed to accept up to 8 independent
segment chains and merge their roots in one shot; this driver realizes that
parallelism. It synthesizes a `--blocks` block sequence from
`bench_test.json` (the repo ships only a single-block fixture), splits it
into `--segments` chains, computes each chain's starting on-chain-operations
keccak prefix on the host (prove-free), produces one L4 (`BlockCircuit`)
proof per block, then folds each segment's L4 proofs into a running L5 proof
**in parallel across segments** (rayon). Every resulting segment proof is
L5-verified. Batch mode only.

```bash
./bench --l5-segment-check --segments 8 --blocks 64
```

### Flags

| Flag | Default | Meaning |
|------|---------|---------|
| `--l5-segment-check` | off | Enable the L5 segment scheduler (batch mode only) |
| `--segments N` | 8 | Number of parallel L5 segment chains (`1..=8`, the wrapper's `NUM_CHAINS_PER_BATCH`) |
| `--blocks N` | 64 | Synthesized block count (must be `>= --segments`) |

It emits a `l5_segment_batch` event:

| `event` | When | Notable fields |
|---------|------|----------------|
| `l5_segment_batch` | Once after all segments fold + verify | `segment_count`, `segment_sizes`, `per_segment_wall_ms`, `block_count`, `effective_ms_per_block` (= `max(per_segment_wall_ms) / max(segment_size)`, the parallel critical path per block), `cpu_ms`, `rss_mb_peak` |

### Measurement gate and termination boundary (#83)

The acceptance target — **effective ≤ 200 ms/block on the #10 AMD EPYC 7B13
baseline** — is a *hardware measurement gate*. This driver delivers the
instrument (the parallel scheduler + the `effective_ms_per_block` event) and
proves it **functionally** at small scale: every L5 segment proof verifies.
Reproduce the headline number on the EPYC baseline with
`bench --l5-segment-check --segments 8 --blocks 64` and post it on
[issue #78](https://github.com/kunallimaye/lighter-prover/issues/78).

The **verifying L6 termination is gated on
[issue #83](https://github.com/kunallimaye/lighter-prover/issues/83)**:
`WrapperCircuit::prove_inner` additionally needs a `delta_chain_proof`, a
`blob_evaluation_proof`, and a KZG `WrapperInput` that do not yet exist
in-repo. When #83 lands, the L6 call pads the unused `chain_proofs[S..8)`
slots with `chain_proofs[0]` and sets `segment_count = S`; the wrapper
asserts segment 0's on-chain-operations hash is zero (which the host
pre-pass guarantees). This driver documents that call shape but does not
invoke the wrapper.

Within a *single* segment, multi-block folds require a genuinely
state-chained multi-block dataset (the synthesized clones share state
roots): the cross-segment dependency this issue targets — the
on-chain-operations keccak prefix — is what the host pre-pass computes and
what the scheduler parallelizes. Running with `--blocks N --segments N`
(one block per segment) exercises the full define → base-proof → witness →
prove → verify path for every segment in parallel.

## Pre-L5 tree-fold mode (bench --l5-fold tree)

`--l5-fold tree` (issue #82, ADR-0003 §D5) is the L5 analogue of the L2
tree-fold one layer up: it builds the pre-L5 block-proof aggregation
`BatchMergeCircuit` (`circuit/src/recursion/batch_merge_constraints.rs`),
asserts the **self-shape gate** `merge.common == l5.common` (the merge
node builds into the L5 cyclic circuit's exact 2^15 / 1496-PI shape, so
its root is consumable anywhere an L5 proof is), and wires the log-depth
pairwise fold of per-block L5 `Batch` proofs (carrying odd proofs up a
level, mirroring `--l2-fold tree`). Two L5 children are merged by
`BatchTarget::conditionally_merge_consecutive` (contiguity, monotonic
timestamps, state/delta-root and priority-op keccak-chain continuity)
plus the on-chain-ops keccak **start-digest stitch**
(`SegmentInfoTarget::connect_segments`, escape hatch iii) — the same
stitch L6 uses, which makes the keccak chain associative across the tree.

This path is **build-validated and A/B-wired**; it does **not** execute a
live L5 prove in-workspace. The timed ≥4-leaf prove on the AMD EPYC 7B13
baseline (confirming ≈0.94 s/step and the log₂(N)·0.94 s critical path)
is a documented follow-up run requiring dedicated hardware + long
wall-clock. The default `--l5-fold serial` is unchanged.

| Flag | Default | Meaning |
|------|---------|---------|
| `--l5-fold serial\|tree` | `serial` | L5 fold strategy (batch mode only; `tree` build-validates `BatchMergeCircuit` and wires the host-level fold) |
| `--l5-ab-check` | off | Tree mode: also serial-fold the same batches and assert element-wise equality of the two roots' semantic public inputs (`Batch`+`SegmentInfo`, excluding trailing VK PIs) |

```bash
./bench --l5-fold tree --l5-ab-check
```

## Streaming mode (bench --stream)

`bench --stream` (issue #49) turns the one-shot batch bench into a
trace-driven consumer: it reads a JSONL block trace on stdin —
conforming to the pinned contract in
[`trace-format.md`](./trace-format.md) (issue #47) — fans each block
arrival out into `ceil(tx_count / tx_per_proof)` chunk jobs over a
bounded queue, and proves them with the same L1 + L2 pipeline as batch
mode. Without `--stream` the original batch behavior is untouched.

```bash
python3 bench/feeder/feeder.py replay --in trace.jsonl --target-rate 1000 \
  | ./bench --stream --tx-per-proof 4 --duration 15m
```

### Flags

| Flag | Default | Meaning |
|------|---------|---------|
| `--stream` | off | Enable streaming mode (stdin trace consumer) |
| `--max-queue N` | 1024 | Bounded chunk-job queue; overflow jobs are dropped and counted (`dropped_chunks`) |
| `--l3-every N` | off | Additionally prove L3 once every N proven chunks |
| `--duration D` | none | Stop after wall-clock `D` (`900s`, `15m`, `2h`); otherwise run to trace EOF or SIGINT/SIGTERM |

Trace handling per the contract: the provenance header (first line) is
parsed, logged, and skipped; headerless pre-spec traces are accepted;
gap markers are skipped and counted (`gaps_skipped`); malformed lines
are warned about and skipped; monotonicity violations (`ts_ms`
regression, non-increasing `height`) abort the run with exit 1.
`tx_count: null` should not occur in replayed input (the producer
fills it, policy P1) but is leniently treated as 500 with a warning.

### Witness recycling (why proving repeated content is OK)

`bench_test.json` is loaded once, circuits are built once, and the
block's txs are pre-sliced into `tx_per_proof`-sized chunks that are
cycled round-robin — the *content* of each proof repeats; only the
*cadence* is live (driven by the trace). Proving cost is dominated by
circuit size, not witness values, so recycled witnesses measure
throughput faithfully. State rolls forward chunk-to-chunk exactly as
in batch mode within one pass over the pool; **when the pool wraps,
state restarts from the block's initial state** — each pool pass is an
independent replay of the same block's chunks (the L2 chain proof also
restarts from a fresh cyclic base proof).

### Streaming event types

Same `BENCH_EVENT ` prefix, serialization, and per-event flush
discipline as the batch events above:

| `event` | When | Notable fields |
|---------|------|----------------|
| `stream_arrival` | Each accepted trace block event | `height`, `tx_count` (`null` mirrors the trace), `queue_depth`, `ts` |
| `chunk_proven` | Per layer (L1, L2) for every dequeued chunk job | the `layer_prove` fields (with `chunk_idx`/`chunk_total` = pool position/size) plus `height`, `lag_ms` (layer completion − enqueue), `queue_depth` |
| `stream_summary` | Every 60s (`phase:"periodic"`) and once at exit (`phase:"final"`) | `throughput_tx_s`, `lag_p50_ms`, `lag_p95_ms`, `peak_rss_mb`, `dropped_chunks`, `arrivals`, `gaps_skipped`, `chunks_proven`, `elapsed_s` |

### Expected divergence at peak rates

This is a single-instance consumer. At the recorded peak of ~2,213
tx/s with `tx_per_proof=4`, the consumer would need to absorb ~556
chunk-jobs/s while a single L1+L2 prove takes seconds — so divergence
at 1.0× peak (queue saturating, `dropped_chunks` climbing) is the
**expected, cleanly-reported outcome**, not a failure mode. The
interesting measurement is the highest rate at which the queue stays
bounded; that is what `make stream-sweep` ladders toward.

### stream-smoke and stream-sweep

- `make -C bench stream-smoke` — manual real-proving smoke test
  (minutes: circuit define dominates). Pipes a tiny inline trace into
  `bench --stream --tx-per-proof 1`. Deliberately **not** part of any
  automated test target; `cargo test -p bench` covers the stream
  machinery with a stub prover and zero plonky2 calls.
- `make -C bench stream-sweep` / `scripts/stream-sweep.sh` — rate
  ladder driving `feeder.py replay` (issue #48) into `bench --stream`,
  reporting the highest stable rate vs. first diverging rate from the
  final `stream_summary`. **Not runnable until the #48 sibling PR
  merges** (the script fails fast with a clear message if the feeder
  is absent).
