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

The `chunk_proven` (stream mode) and `layer_prove` events additionally
carry an optional `witness_fetch_ms` (`Option<u64>`) — the measured
witness-acquisition wall when the chunk is resolved through the conductor
witness plane (issue #61 / ADR-0008 §2.2; see the conductor section below).
It is omitted/`null` when the chunk was proven without going through the
witness plane.

`cpu_ms`, `rss_mb_peak`, `rss_mb_after`, `peak_rss_mb`, and
`total_cpu_ms` are Linux-only (parsed from `/proc/self/status` /
`getrusage`). On non-Linux platforms they serialize as `null`. `ts`
is UTC in `YYYY-MM-DDTHH:MM:SSZ` form.

### Per-chunk tx-type attribution (issue #157)

Two additive flags annotate L1/L2 `layer_prove` events with per-chunk
tx-type information so the per-type cost shape can be filtered out of
the existing JSONL stream without rewriting the bench:

| Flag | Effect |
|------|--------|
| `--attribute-tx-type` | Adds two optional fields to L1/L2 `layer_prove`: `tx_types` (the `tx_type` of each tx in the chunk, in chunk order) and `chunk_tx_type_homogeneous` (`Some(t)` when every tx in the chunk shares `tx_type == t`, otherwise omitted). Tx order is not changed; chunks are homogeneous opportunistically (always true at `--tx-per-proof 1`). |
| `--group-by-tx-type` | Stable-sort `block.txs` by `tx_type` before chunking, then emit attribution (implies `--attribute-tx-type`). Produces type-homogeneous chunks except at type boundaries. **Caveat:** re-ordering txs breaks chain-validity; the L1 witness for some tx types asserts cross-tx state that the unsorted chain established, so prove can panic on `bench_test.json` (see issue #159 for the root-cause investigation). For that fixture, prefer `--attribute-tx-type --tx-per-proof 1` to isolate per-type cost without sorting. |

Both flags are off by default and the pre-#157 JSON shape is preserved
when neither is set (the new fields use `#[serde(skip_serializing_if =
"Option::is_none")]`). Consumers (fleet parser, calibration) that select
fields by name are transparently unaffected.

Example with attribution on:
```
BENCH_EVENT {"event":"layer_prove","layer":1,"name":"BlockTxCircuit","chunk_idx":0,"chunk_total":500,"tx_per_proof":1,"wall_ms":616,"cpu_ms":10094,"rss_mb_peak":1192,"rss_mb_after":1192,"ts":"2026-06-14T07:22:52Z","tx_types":[15],"chunk_tx_type_homogeneous":15}
```

### Conductor witness plane + `witness_fetch_ms` (issue #61 / ADR-0008)

The MINIMUM distributed-prover conductor (issue #75, local slice) lives in
`bench/src/conductor/` — a plonky2-free, host-testable distribution layer
that reuses the `--stream` closure-injection + bounded-queue pattern:

- **OUTER tier** (`conductor::queue`): a `BlockQueue` trait + in-memory
  `LocalBlockQueue` (the ADR-0006 §1.1 competing-pull block-dispatch layer;
  a real Pub/Sub adapter drops in behind the trait, no GCP today).
- **INNER tier + pool** (`conductor::dispatch`): a `Coordinator` SPLITs a
  block into `k=ceil(tx/S)` chunks and fans the chunk **references** out to a
  HORIZONTAL `CoordinatorPool` of cells (ADR-0006 §1.2/§2; #113 horizontal
  lever only — no per-coordinator concurrency).
- **Witness plane** (`conductor::witness`): `{height, witness_index}`
  addressing (ADR-0008 §1.1) + a k=1 `MountedCorpus` local indexed-lookup
  resolver (ADR-0008 §1.4 — the `bench_test.json` whole-block mount is the
  k=1 case). Dispatch carries witness **references, not bytes** (ADR-0008
  §1.2).

The `--stream` mode wires this in: each chunk resolves its witness reference
through the witness plane, and the resolve-and-read wall is emitted as
`witness_fetch_ms` (`Option<u64>`) on the `chunk_proven` event (ADR-0008 §2.1
/ §2.2). This is the **local-resolve FLOOR** — a real measured number
(`null`/omitted on the legacy recycled-witness path that does not resolve a
reference), **not** the distributed `witness_move` term, which stays
UNMODELED until ADR-0008 §3's gated fleet study (ADR-0008 §2.3; ADR-0004
§3.1/§3.2). The field is additive and consumer-safe (matches the `cpu_ms` /
#157 optional-field convention).

Example `chunk_proven` line carrying a real measured `witness_fetch_ms`
(k=1 mounted-corpus local resolve; sub-millisecond → `0`):
```
BENCH_EVENT {"event":"chunk_proven","layer":1,"name":"BlockTxCircuit","chunk_idx":0,"chunk_total":10,"tx_per_proof":4,"wall_ms":2672,"cpu_ms":44235,"rss_mb_peak":2676,"rss_mb_after":2525,"height":260138266,"lag_ms":3068,"queue_depth":124,"ts":"2026-06-14T08:23:03Z","witness_fetch_ms":0}
```

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
| `synth-peak` | offline, no inputs | Fabricate an idealized trace on **two independent load axes** — blocks/sec (`--block-rate`) and txns/block (`--tx-count`, 1..500); a constant `tx_count` pins `k = ceil(tx_count/S)` for a fixed-k stream. Back-compat: `--rate TXS` alone derives `block_rate = rate/tx_count` with the default `tx_count=500` (legacy cadence `500/rate` s) |
| `peak-hours` | analysis helper | Locate peak windows from the explorer hourly tx stats (top-N hours by tx/s) |
| `tx-mix` | network (geo-sensitive) | Capture the per-block tx-type mix (#128) from the zklighter `blockTxs` API — the only HTTP source carrying per-tx `tx_type`. Geo-blocks some regions (observed: US); **run from Tokyo / ap-northeast** (see recipe below). `--sample-block` reads the in-repo sample-size-1 block offline |

Make targets (from `bench/`): `stream-record OUT=... [DURATION=...]`,
`stream-replay TRACE=... [SPEED=N | RATE=TXS] [DURATION=...]`,
`stream-peak [RATE=TXS | BLOCK_RATE=blk/s] [TX_COUNT=N] DURATION=...`,
`stream-tx-mix [BLOCKS=N | HEIGHTS="LO HI"] | SAMPLE=1`, and `feeder-test`
(offline suite, no network, <1 min; also wired into the repo-root
`make local-test`).

### Two load axes: driving a fixed-k stream (issue #217)

The coordinator splits each block into `k = ceil(tx_count / S)` chunks, so
`tx_count` (not block cadence) is what determines `k`. `synth-peak`
exposes the two axes independently:

- `--block-rate B` — blocks/sec; cadence is `1000/B` ms, independent of
  `tx_count`.
- `--tx-count N` — txns per block (1..500); constant across every block, so
  `k` is pinned. At the canonical **S=9**: `N=216 → k=24`, `N=288 → k=32`,
  `N=500 → k=56`.

Two committed Tier-2 fixtures are deterministic `--dry-run` artifacts of
this code path (base ts=0, `--block-rate 1 --duration 60s`, ~61 blocks each,
header on line 1, constant `tx_count`, no height jumps, no nulls):

- `bench/feeder/fixtures/synth_k24_tx216.jsonl` — every block `tx_count=216` (k=24)
- `bench/feeder/fixtures/synth_k32_tx288.jsonl` — every block `tx_count=288` (k=32)

```bash
# Regenerate a fixture (byte-deterministic except generated_at):
python3 bench/feeder/feeder.py synth-peak --tx-count 216 --block-rate 1 \
  --duration 60s --dry-run > bench/feeder/fixtures/synth_k24_tx216.jsonl

# Drive a k=24 / k=32 dispatch stream by replaying a fixture into the
# dispatch topic (pacing honored via the native publisher bridge, #211):
python3 bench/feeder/feeder.py replay \
  --in bench/feeder/fixtures/synth_k24_tx216.jsonl \
  --target-rate <r> --publish-to <dispatch-topic> --project kunal-scratch
python3 bench/feeder/feeder.py replay \
  --in bench/feeder/fixtures/synth_k32_tx288.jsonl \
  --target-rate <r> --publish-to <dispatch-topic> --project kunal-scratch

# Or generate + publish a live fixed-k stream directly (no fixture):
python3 bench/feeder/feeder.py synth-peak --block-rate 8 --tx-count 216 \
  --duration 5m --publish-to <dispatch-topic> --project kunal-scratch
```

Each fixture carries `BlockMessage {height, tx_count}` so the coordinator
splits k=24 / k=32 at S=9. Use `--block-rate` to control how fast blocks
arrive without changing `k`.

### Bimodal sampled load (issue #220)

A third axis — **sampled per-block size** — drives a mainnet-faithful
mix into the Tier-2 keep-pace phase (consumed by #214 Phase B). The
canonical 7-band weights live in `bench/feeder/size_distributions.py`
(`MAINNET_BIMODAL_COUNTS`) as the single-source-of-truth, with the
same mix documented in `bench/trace-format.md` §4 (provenance header)
for the #212 mainnet shape: ~11% blocks at `tx==1`, ~74% pinned to
the chain's 500-tx cap, the long tail in between. Sampling is seeded
and explicitly RNG-injected (no module-level `random`) so the same
`--seed N` always produces a byte-identical stream.

- `--size-distribution bimodal` — the #212 mix (the default Tier-2
  load).
- `--size-dist-file PATH` — a custom JSON sampler (same band partition,
  custom weights / representatives).
- `--seed N` — required when sampling; pins determinism.
- `--histogram-out PATH` — optional JSON sidecar with the realized
  per-band counts + sampler config + seed (audit artifact). A
  `REALIZED_HISTOGRAM eq_1=N1 ... eq_500=N7 total=N` line is always
  emitted on stderr at end of run.

```bash
# Regenerate the committed Tier-2 fixture (byte-deterministic body):
python3 bench/feeder/feeder.py synth-peak \
  --size-distribution bimodal --seed 220 \
  --block-rate 11.08 --duration 60s --dry-run \
  --histogram-out bench/feeder/fixtures/synth_bimodal_mainnet.histogram.json \
  > bench/feeder/fixtures/synth_bimodal_mainnet.jsonl

# Drive Phase B live with the native publisher bridge (no gcloud
# shell-out on the hot path; pacing drift + realized histogram both
# reported on stderr at end of run):
python3 bench/feeder/feeder.py synth-peak \
  --size-distribution bimodal --seed 220 \
  --block-rate 11.08 --duration 15m \
  --publish-to <dispatch-topic> --project <gcp-project> \
  --histogram-out /tmp/keep-pace.histogram.json
```

Honesty caveats:

- Varies block **size/height** per the real distribution, **not tx
  content** — stays inside the ADR-0009 §2 (decision) / §3 (scope)
  sanctioned size+height boundary. Content variety remains gated on
  #184.
- Because the #212 mix is `k=56`-dominated (~74% of blocks pinned to
  the cap), driving this load is the **first live end-to-end k=56
  distributed fold** — never validated live before (prior runs were
  `k ≤ 16` or accounting-only). The Phase B run using this feeder
  depends on #177 + #209 + the real merge+L4 path holding (audited
  green in #214). This feeder ships the producer; the live Phase B
  run is a separate event tracked in #214.

Dependencies: `record`, `peak-hours`, and `tx-mix` need
`bench/feeder/requirements.txt` (`websockets`, `requests`); `replay`,
`synth-peak`, and the tests are pure Python 3 stdlib.

Geo-block note: the chain's main REST API returns 403 to US IPs. The
cadence feeder (`record`/`replay`/`synth-peak`/`peak-hours`) avoids it
entirely — the WS stream with `?readonly=true` and the explorer API
(`explorer.elliot.ai`) are not geo-blocked. The explorer blocks endpoint
is rate-limited to 90 req/min per IP; `record` polls at ~85 req/min with an
identifying User-Agent.

### tx-mix: region-operable capture (issue #128)

The tx-type **mix** is NOT in the trace format and is NOT served by the
explorer (its block endpoints carry `block_size`/`total_transactions`/
`markets`, no per-tx `tx_type`). The only HTTP source exposing per-tx
`tx_type` is the zklighter mainnet `blockTxs` API — and that endpoint
**geo-blocks some regions (observed: US) with HTTP 403**. Tokyo /
ap-northeast is normally **not** geo-blocked, so the operable answer is to
run the capture from there. The tool cannot change its own egress IP, so on
a 403 it **fails honestly with Tokyo guidance** and exits non-zero rather
than fabricating a mix (a tx-mix result must cite a real successful
capture).

**Tokyo run recipe** (the actual capture is an operator step):

```bash
# 1. Provision a small VM in a non-geo-blocked region (Tokyo / ap-northeast).
gcloud compute instances create txmix-capture \
  --zone=asia-northeast1-b --machine-type=e2-small \
  --image-family=debian-12 --image-project=debian-cloud
gcloud compute ssh txmix-capture --zone=asia-northeast1-b

# 2. On the VM: clone, install deps, run the capture.
git clone https://github.com/kunallimaye/lighter-prover && cd lighter-prover
pip install -r bench/feeder/requirements.txt
make -C bench stream-tx-mix BLOCKS=200      # or HEIGHTS="LO HI"
#   equivalently: python3 bench/feeder/feeder.py tx-mix --blocks 200 \
#                   --region asia-northeast1

# 3. Tear the VM down when done.
gcloud compute instances delete txmix-capture --zone=asia-northeast1-b
```

**Tokyo run recipe — reusable Cloud Run JOB** (issue #128, the repeatable
path). Instead of a hand-provisioned VM, the capture is packaged as a
parametrised Cloud Run **Job** in `asia-northeast1` (Tokyo) that writes its
results **durably to GCS**. A Job (not a Service) runs to completion and
exits; the task timeout is generous (default 24h) because a rate-limited
representative window can take hours. The SAME job runs a tiny smoke window
and a big representative window purely by config — no redefinition.

```bash
# One-time: build the image + deploy the job (public Tokyo egress).
make cloud-txmix-build      # Cloud Build -> asia-northeast1 Artifact Registry
make cloud-txmix-deploy     # gcloud run jobs create/update (asia-northeast1)

# Validate the machinery with a tiny window (small-N — NOT the answer to G1).
make cloud-txmix-smoke
make cloud-txmix-results    # prints the GCS artifact: meta + mix table + DONE

# OPERATOR representative capture — ONE command. Choose a peak/off-peak
# window large enough to be representative (a human judgment):
TXMIX_HEIGHTS="<LO> <HI>" TXMIX_LABEL=peak make cloud-txmix-capture
#   or by recent-block count:
TXMIX_BLOCKS=5000 TXMIX_LABEL=offpeak make cloud-txmix-capture

make cloud-txmix-results    # the captured mix + its provenance
make cloud-txmix-post       # post the cited summary to issue #128
```

Output (durable — a Job's filesystem evaporates on exit) lands in the
**shared fleet results bucket** under a `txmix/` prefix:
`gs://kunal-scratch-bench-fleet-runs/txmix/<ts>-<label>-<id>/`:
`tx-mix.txt` (the rendered mix table), `tx-mix.meta.json` (machine-readable
provenance: egress region/IP, endpoint, window, N, `--max-rpm`),
`tx-mix.stderr.txt` (tool stderr, incl. any 403 guidance), and a `DONE`
sentinel. Every knob (window, `--max-rpm`, GCS bucket/path, `--base-url`/
`--proxy`, task timeout) is an env var — see `scripts/cloud-txmix.sh`.

**Output bucket — config-driven, reuses the fleet bucket.** The bucket is
set in `config.toml` under `[txmix].bucket` (mirroring `[fleet].results_bucket`)
and flows through `scripts/config.py` as the `TXMIX_BUCKET` env var; the
`TXMIX_BUCKET` env var overrides. We **reuse the existing shared fleet
results bucket** (`gs://kunal-scratch-bench-fleet-runs`, region
**us-central1**) rather than provisioning a new txmix-only bucket — it
already exists (created by `make admin-cloud-init`, Owner-tier) and already
hosts other data-collection output. The Job runs in **Tokyo**
(asia-northeast1) for the egress geo-block workaround but writes its tiny
payload to the **us-central1** fleet bucket; a cross-region write of a few
KB is negligible. `cloud-txmix.sh` only **verifies** the bucket exists
(read-only `describe`) — it never creates one, so no `storage.buckets.create`
is needed.

**Identity / run-as model (single SA).** The Cloud Run control-plane calls
(`build`/`deploy`/`smoke`/`capture`/`results`) run **as the project agent
SA** `lighter-prover-agent@kunal-scratch.iam.gserviceaccount.com` via
`--impersonate-service-account` (the active operator/runtime identity holds
`roles/iam.serviceAccountTokenCreator` + `serviceAccountUser` on it). The
**Job itself also runs as** that agent SA (`--service-account`), whose
project-level `storage.objects.*` lets the running Tokyo job write to the
us-central1 fleet bucket. Both are configurable via `TXMIX_IMPERSONATE_SA`
and `TXMIX_RUN_AS_SA` (config.toml `[txmix].impersonate_sa` /
`[txmix].run_as_sa`); set `TXMIX_IMPERSONATE_SA=""` to run as the active
gcloud account directly.

Egress fallback: the job uses **public** Cloud Run egress (a GCP-assigned
Tokyo IP) — no Cloud NAT / static egress by default. If a smoke run 403s
(the Tokyo GCP range is geo-blocked), the tool hard-fails honestly and the
finding lands in GCS. Recover by either attaching a Cloud NAT static egress
IP to the job's VPC connector, or fall back to the VM recipe above.

Config knobs (no code edits needed; all optional, conservative defaults):

| Flag | Env | Purpose |
|---|---|---|
| `--base-url URL` | `LIGHTER_TXMIX_BASE_URL` | Alternate `blockTxs` base URL — point the tool at a Tokyo egress |
| `--proxy URL` | `LIGHTER_EGRESS_PROXY` (else `HTTPS_PROXY`) | Route requests through a Tokyo egress proxy without moving the tool |
| `--region LABEL` | `LIGHTER_REGION` | Region label recorded in the output for citation hygiene (does not affect routing) |
| `--max-rpm N` | — | Cap requests/min (default 80, under the 90/min per-IP limit) |
| `--max-retries N` | — | Retries on 429/transient before failing (default 5) |

Rate-limit behavior (the tool is a well-behaved client that cannot hammer
the endpoint): every request is paced to the `--max-rpm` minimum interval;
HTTP **429** responses respect `Retry-After` (capped) or fall back to
exponential backoff; transient 5xx / connection errors back off
exponentially up to `--max-retries`; a **403 geo-block is treated as a hard,
non-transient failure** (not retried — it just prints the Tokyo guidance and
exits 2). All of this is unit-tested offline (`feeder-test`) with mocked
403/429/Retry-After/backoff paths — no real network, no real sleeps.

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

`--l5-segment-check` (issue #78, extended by #94) runs the 8-way L5
(`CyclicRecursionCircuit`) segment-parallel scheduler. The wrapper circuit
(`NUM_CHAINS_PER_BATCH = 8`) is designed to accept up to 8 independent
segment chains and merge their roots in one shot; this driver realizes that
parallelism. It builds a **genuinely state-chained `--blocks`-block
fixture** by tx-slicing `bench_test.json` (the repo ships only a
single-block 500-tx fixture), splits it into `--segments` chains, computes
each chain's starting on-chain-operations keccak prefix on the host
(prove-free), produces one L4 (`BlockCircuit`) proof per block while
capturing the per-chunk rolling state, and uses that rolling state plus the
previous block's L4 `BlockWitness` to clone the next block in the chain
(issue #94's recipe: `bench/src/l5segment.rs::chain_next_block`). It then
folds each segment's L4 proofs into a running L5 proof **in parallel across
segments** (rayon). Every resulting segment proof is L5-verified. Batch
mode only.

```bash
./bench --l5-segment-check --segments 8 --blocks 64
```

### Flags

| Flag | Default | Meaning |
|------|---------|---------|
| `--l5-segment-check` | off | Enable the L5 segment scheduler (batch mode only) |
| `--segments N` | 8 | Number of parallel L5 segment chains (`1..=8`, the wrapper's `NUM_CHAINS_PER_BATCH`) |
| `--blocks N` | 64 | Chained block count (must be `>= --segments` AND satisfy `--blocks * --tx-per-proof <= DEFAULT_TX_LIMIT = 480`) |

The `blocks × tx_per_proof ≤ 480` bound is enforced at parse time (issue
#94): each chained block consumes one disjoint L1 chunk's worth of txs
from the 500-tx fixture (per-block tx slice = `tx_per_proof` =
`tx_per_block`), so the headline `--blocks 64 --segments 8` invocation is
runnable at the default `--tx-per-proof 4` (`64 × 4 = 256 ≤ 480 ≤ 500`).
Increasing `--tx-per-proof` reduces the maximum runnable `--blocks`
proportionally; the guard rejects over-budget invocations with a precise
error naming all three quantities (`blocks`, `tx_per_block`, the 480
ceiling).

It emits a `l5_segment_batch` event:

| `event` | When | Notable fields |
|---------|------|----------------|
| `l5_segment_batch` | Once after all segments fold + verify | `segment_count`, `segment_sizes`, `per_segment_wall_ms`, `block_count`, `effective_ms_per_block` (= `max(per_segment_wall_ms) / max(segment_size)`, the parallel critical path per block), `cpu_ms`, `rss_mb_peak` |

### Measurement gate and termination boundary (#83)

The acceptance target — **effective ≤ 200 ms/block on the #10 AMD EPYC 7B13
baseline** — is a *hardware measurement gate*. This driver delivers the
instrument (the parallel scheduler + the `effective_ms_per_block` event) and
proves it **functionally**: every L5 segment proof verifies, including
within-segment continuity folds (`not_first_recursion = true` on chained
blocks, issue #94). Reproduce the headline number on the EPYC baseline
with `bench --l5-segment-check --segments 8 --blocks 64` and post it on
[issue #78](https://github.com/kunallimaye/lighter-prover/issues/78); the
full per-machine measurement session is tracked on
[issue #90](https://github.com/kunallimaye/lighter-prover/issues/90) and is
re-runnable on the new chained fixture.

The **L6 inner-wrapper drive path is implemented by
[issue #83](https://github.com/kunallimaye/lighter-prover/issues/83)** — see the
[L6 inner-wrapper drive modes](#l6-inner-wrapper-drive-modes-bench---delta-prove----blob-prove----l6-inner)
section below. `WrapperCircuit::prove_inner` needs a `delta_chain_proof`, a
`blob_evaluation_proof`, and a KZG `WrapperInput`; #83 adds the `--delta-prove`,
`--blob-prove`, and `--l6-inner` modes that produce them. The L6 call pads the
unused `chain_proofs[S..8)` slots with `chain_proofs[0]` and sets
`segment_count = S`; the wrapper asserts segment 0's on-chain-operations hash is
zero (which the host pre-pass guarantees).

Within-segment multi-block folds exercise the L5 fold's
`batch.new_state_root == current_block.old_state_root` continuity assert
(`cyclic_circuit.rs:208-213`, active on `not_first_recursion`). Issue
#94's tx-slicing recipe satisfies this assert by re-anchoring each chained
block against the rolling state and the L4 `BlockWitness` of its
predecessor — verified at small scale by
`bench --l5-segment-check --segments 4 --blocks 4` (4 single-block
segments, no within-segment fold) and
`bench --l5-segment-check --segments 2 --blocks 4` (2 segments × 2
blocks, exercising one within-segment continuity fold per segment).

## Pre-L5 tree-fold mode (bench --l5-fold tree)

`--l5-fold tree` (issue #82, extended by #94 + PR #96, ADR-0003 §D5) is the
L5 analogue of the L2 tree-fold one layer up: it builds the pre-L5
block-proof aggregation `BatchMergeCircuit`
(`circuit/src/recursion/batch_merge_constraints.rs`), asserts the
**self-shape gate** `merge.common == l5.common` (the merge node builds
into the L5 cyclic circuit's exact 2^15 / 1496-PI shape, so its root is
consumable anywhere an L5 proof is), and **live-proves** the log-depth
pairwise fold of per-block L5 `Batch` proofs (carrying odd proofs up a
level, mirroring `--l2-fold tree`). Two L5 children are merged by
`BatchTarget::conditionally_merge_consecutive` (contiguity, monotonic
timestamps, state/delta-root and priority-op keccak-chain continuity)
plus the on-chain-ops keccak **start-digest stitch**
(`SegmentInfoTarget::connect_segments`, escape hatch iii) — the same
stitch L6 uses, which makes the keccak chain associative across the tree.

Each tree leaf is a real per-block L5 fold of the chained fixture built
by `build_chained_blocks_and_l4_proofs` (#94, shared with
`--l5-segment-check`). Each tree-level merge calls
`BatchMergeCircuit::prove`, which uses PR #96's `generate_witness` fix
(commit `351363d` on `main`) to populate the merged `Batch` / `SegmentInfo`
public-input targets. The root proof verifies against `merge_data` (or
`l5_data` for the trivial single-leaf case). The default `--l5-fold
serial` is unchanged.

| Flag | Default | Meaning |
|------|---------|---------|
| `--l5-fold serial\|tree` | `serial` | L5 fold strategy (batch mode only; `tree` live-proves ≥4-leaf pairwise tree fold) |
| `--blocks N` | 64 | Leaf count: real chained blocks consumed from `bench_test.json`. Same `blocks × tx_per_proof ≤ 480` parse-time guard as `--l5-segment-check`. The driver pads up to 4 leaves internally so the tree has at least two levels. |
| `--l5-ab-check` | off | Tree mode: also host-mirror serial-fold the same batches and assert element-wise equality of the two roots' semantic public inputs (`Batch`+`SegmentInfo`, excluding trailing VK PIs) |

```bash
./bench --l5-fold tree --blocks 4
```

## L6 inner-wrapper drive modes (bench --delta-prove / --blob-prove / --l6-inner)

Issue #83 drives `WrapperCircuit::prove_inner` by producing its three previously
missing inputs over a **correctly-shaped synthesized** (empty) batch. Real
mainnet witness generation is closed-source and deferred to #119. See
[ADR-0005](../docs/decisions/ADR-0005-l6-inner-wrapper-kzg-sidecar.md) for the
full design, including the BLS12-381-vs-BN254 distinction and the **custom
Poseidon2 PCE evaluation point** (not the EIP-4844 standard challenge).

| Flag | Produces | Acceptance criterion |
|------|----------|----------------------|
| `--delta-prove` | `delta_chain_proof` — drives `DeltaCircuit` then `CyclicDeltaCircuit` and verifies | #1 |
| `--blob-prove` | `blob_evaluation_proof` + the KZG `WrapperInput` (versioned hash + PCE opening `x`/`y`) | #2, #3 |
| `--l6-inner` | assembles all three inputs + builds the L5 (2^15) and inner-wrapper (2^18) circuits | #4 (partial — see below) |
| `--trusted-setup-path PATH` | KZG ceremony setup for `--blob-prove`/`--l6-inner` (default `bench/assets/trusted_setup.txt`) | — |

```bash
./bench --delta-prove
./bench --blob-prove
./bench --l6-inner
```

All three are **batch mode only** (incompatible with `--stream`). The
blob-evaluation and inner-wrapper circuits are deep enough to overflow the
default 8 MiB main-thread stack, so the modes run them on a dedicated 4 GiB-stack
thread automatically.

### KZG sidecar (bench/src/kzg.rs)

`WrapperInput.kzg_versioned_hash` is the EIP-4844 versioned hash of the
BLS12-381 KZG commitment to the blob, computed via `c-kzg`
(`0x01 || SHA-256(commitment)[1..]`) against the **public Ethereum KZG ceremony**
trusted setup. The opening `(x, y)` is **not** the EIP-4844 challenge: the
sidecar replicates the in-circuit custom Poseidon2 PCE transcript
(`BlobEvaluationCircuit::verify_pce_evaluation`) off-circuit, using the existing
plain-Rust `BLS12381Scalar` arithmetic and `Poseidon2Hash`. Correctness is
enforced by the in-circuit `connect_nonnative` check: if `x` or `y` is wrong,
`--blob-prove` fails. `c-kzg` needs a C toolchain at build time (pure-Rust
`kzg-rs` is the documented swap-in fallback).

### Status of criterion #4 (`--l6-inner`)

`--l6-inner` produces and verifies the `delta_chain_proof` and
`blob_evaluation_proof`, derives the wrapper-consistent delta evaluation point
off-circuit, computes the KZG `WrapperInput`, and builds the inner-wrapper
circuit. The remaining step is producing **8 L5 chain proofs whose merged batch
has `new_account_delta_tree_root == EMPTY_ACCOUNT_DELTA_TREE_ROOT`** (an L5 chain
over no-op blocks), mutually consistent with the empty delta chain and empty blob
across `verify_aggregated_delta` and `verify_delta_polynomial_evaluation`. The
existing `--l5-segment-check` driver synthesizes blocks with real txs (non-empty
delta-tree root), so it cannot directly feed `prove_inner` without a
consistent-empty (or fully mutually-consistent) batch. No KZG values were
fabricated and no constraint was relaxed to force a terminating prove; the
consistent-empty L5 chain is the closing step for #83 (in-repo work, no Lighter
dependency).

### Smoke tests

```bash
# heavy plonky2 proves; #[ignore]d by default, run explicitly with a large stack
RUST_MIN_STACK=4294967296 cargo test -p bench --lib -- --ignored \
  test_delta_chain_prove test_blob_evaluation_prove
# fast (always-on): KZG versioned-hash shape check
cargo test -p bench --lib test_kzg_versioned_hash_is_versioned
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
