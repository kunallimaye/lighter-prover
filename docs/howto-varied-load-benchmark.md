# How-to: run a size + height-varied distributed benchmark

Drive the distributed prover fleet (coordinator + cells over real Pub/Sub) with
**size- and height-varied** block load, with **no new prover code**. This
measures fleet mechanics, throughput, fold, and the L4 serial term under varied
fan-out and fold width.

> **Why this is valid (and its limits): see
> [ADR-0009](decisions/ADR-0009-distributed-benchmark-load-construction.md).**
> This guide is the operational HOW only; the WHY lives in the ADR so the two
> never drift. In short: the bus carries `{height, tx_count}` and `{height,
> witness_index}` **references**, each cell proves slices of its own local
> `bench_test.json`, and a prefix of N txs is a valid provable block — so
> varying `tx_count` and `height` is real, varied load.

---

## 1. What you publish

Each block job is one `BlockMessage { height, tx_count }` JSON message on the
**dispatch topic**. The coordinator pulls it, SPLITs into `k = ceil(tx_count /
S)` chunks (`S = --tx-per-proof`), and fans `k` chunk references to the cells.

Two ways to produce the job list:

### Option A — drive from the committed corpus index (cheapest path)

`bench/corpus/index.json` already encodes a size+height-varied layout: 421
entries across 100 distinct heights, banded `tx_count`, all referencing the one
real seed. Collapse it to one job per height and publish:

```bash
python3 - "$DISPATCH_TOPIC" "$PROJECT_ID" <<'PY'
import json, subprocess, sys
topic, project = sys.argv[1], sys.argv[2]
entries = json.load(open("bench/corpus/index.json"))["entries"]
# one block JOB per height; tx_count = max slice extent seen for that height
jobs = {}
for e in entries:
    jobs[e["height"]] = max(jobs.get(e["height"], 0), e["tx_count"])
for height, tx_count in sorted(jobs.items()):
    msg = json.dumps({"height": height, "tx_count": tx_count})
    subprocess.run(["gcloud", "pubsub", "topics", "publish", topic,
                    "--project", project, "--message", msg], check=True)
PY
```

### Option B — publish a banded job list directly

Publish `{height, tx_count}` jobs with banded sizes and distinct, strictly
increasing heights:

```bash
for height_txcount in 1001:100 1002:250 1003:400 1004:500 1005:500; do
  height="${height_txcount%%:*}"; tx_count="${height_txcount##*:}"
  gcloud pubsub topics publish "$DISPATCH_TOPIC" --project "$PROJECT_ID" \
    --message "{\"height\":$height,\"tx_count\":$tx_count}"
done
```

> The cadence feeder `bench/feeder/feeder.py` (`replay` / `synth-peak`) emits
> `{ts_ms, height, tx_count}` JSONL — the same fields, plus a timestamp — and
> is the way to **pace** a realistic arrival cadence. To use it as the producer,
> publish each emitted line's `{height, tx_count}` to `$DISPATCH_TOPIC` (same
> `gcloud pubsub topics publish` call as above). The feeder never carries
> witnesses; it carries block cadence only.

---

## 2. Recommended `tx_count` bands (and why)

Size the bands to G1's real **bimodal** block-size distribution (North-Star
#144, G1): the mass is heavy at the **500-tx cap (73.57%)**, with a meaningful
spike at **`=1` (11.17%)** and thin intermediates (`2-49`, `50-99`, `100-249`,
`250-399`, `400-499` each ~2-5%). A representative job mix:

| Band | `tx_count` | Why |
|---|---|---|
| cap | 500 | dominant real mass (73.57%); max `k`, widest fold, full L4 cardinality |
| singleton | 1 | second real mass (11.17%); `k=1` floor, fold/L4 minimum |
| intermediates | 100, 250, 400 | exercise the `k` spread (varied fan-out / fold width) |

Weight the cap and singleton bands heavily to match the real bimodal shape;
sprinkle intermediates to sweep `k`. The live fixture `bench/bench_test.json`
holds 500 txs, so any `tx_count ∈ [1, 500]` is a valid provable prefix.

> **Note:** the *committed* `bench/corpus/index.json` is capped at `tx_count =
> 100` (bands `{1, 25, 50, 75, 100}`). For full cap-band (500) coverage against
> the 500-tx fixture, use Option B (or regenerate the index with a higher cap
> via `tools/corpus-gen/gen_corpus.py`).

---

## 3. How the fleet consumes it

- **Coordinator** (`bench --mode coordinator`): pulls `{height, tx_count}`,
  SPLITs into `k = ceil(tx_count / S)`, publishes `k` chunk references, gathers
  results, folds, emits one per-block completion event.
- **Cells** (`bench --mode cell`): competing-pull chunk references, resolve
  `{height, witness_index}` against the local mounted `bench_test.json`, run
  **real L1 + L2 ZK proves**, publish results.

Pub/Sub wiring is via env (`LIGHTER_DISPATCH_SUBSCRIPTION`,
`LIGHTER_CHUNK_TOPIC`, `LIGHTER_RESULTS_SUBSCRIPTION` on the coordinator;
`LIGHTER_CHUNK_SUBSCRIPTION`, `LIGHTER_RESULTS_TOPIC` on the cells). Set `S` with
`--tx-per-proof`. See `cicd/entrypoint.sh` for the role contract. The mechanics
and their validity are in [ADR-0009](decisions/ADR-0009-distributed-benchmark-load-construction.md)
§1-§3 and ADR-0006 (the conductor).

---

## 4. Reading the results

Both roles emit `BENCH_EVENT ` JSONL to stdout; filter with `grep '^BENCH_EVENT
'`.

- **Per-block completion** (coordinator): a `StreamSummary` event with `phase =
  "block_complete"` (or `"block_partial"`), carrying `throughput_tx_s` and the
  block-arrival→all-chunks-proven wall as `lag_p50_ms` / `lag_p95_ms`. Sweep
  these across the `tx_count` bands to read **fold-width / L4 variation** vs
  block size.
- **Per-chunk** (cells): `chunk_proven` events with `wall_ms`, `lag_ms`,
  `queue_depth`, and `witness_fetch_ms` (the local-resolve floor).
- **L4 / fold**: the per-block lag at the cap band (`tx_count = 500`, widest
  `k`) isolates the L4 serial term — the suspected bottleneck (ADR-0004 §6.1).

Extract a quick lag-by-size table:

```bash
grep '^BENCH_EVENT ' run.log | sed 's/^BENCH_EVENT //' \
  | jq -r 'select(.phase=="block_complete")
           | [.height, .throughput_tx_s, .lag_p50_ms] | @tsv'
```

---

## 5. Scope & caveats

**This measures fleet mechanics under size/count-varied load — NOT content
variety.** Every proven slice comes from the **same one real block's 500 txs**:
no varied signatures, account-tree leaves, or tx-type mix. Per-tx-type cost
sensitivity is sample-size-1.

- For the full rationale and the "DOES vs DOES NOT measure" boundary, read
  **[ADR-0009](decisions/ADR-0009-distributed-benchmark-load-construction.md)**
  (do not re-derive it here).
- Content-varied, prover-serializable novel blocks (full witness synthesis) are
  **blocked / unbuilt** — tracked in **#184** (see ADR-0009 §4). Do **not** read
  a throughput / fold / L4 number from this instrument as a content-sensitivity
  result.
- **Do not** fall back to single-identical-block replay (one height, one size):
  it exercises neither varied `k` nor multi-block dispatch/gather and is the
  explicitly rejected approach (ADR-0009 §5). Vary **size AND height**.
