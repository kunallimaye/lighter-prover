# Trace load-distribution analysis (issue #128)

Pure-Python, stdlib-only analysis that extracts the real mainnet load
distribution from banked feeder traces. **Analysis only** — no proving, no
cloud spend, no infrastructure.

It answers the three load questions the trace format *can* answer:

1. **Block-size distribution** — count, non-null mean/median,
   p50/p75/p90/p95/p99/p99.9, min/max, null fraction, plus **size bands**
   (=1 / 2-49 / 50-99 / 100-249 / 250-399 / 400-499 / =500-cap) — the bands
   surface the bimodal mass that percentiles collapse against the cap.
2. **Outlier (large-block) frequency** — fraction of blocks at the 500-tx chain
   cap (`BLOCK_TX_CAP`), and any `> cap` anomalies (spec violations).
3. **Arrival-rate / burst distribution** — inter-block gap
   median/p95/p99/p99.9/max, height-jump count + Δ histogram (bursts where
   Δ > 1), aggregate tx/s under the feeder's P1 mean-fill policy, **blocks/s
   distribution in 1-second wall buckets (mean / p50 / p90 / p95 / p99 / p99.9
   / max, height-expansion-aware)**, and **peak rolling-window block counts**
   for 1/3/5/10 s windows — the burst signal the conductor (#75) is sized
   against (ADR-0004: `>=5 blocks/s`, `lag_p50<=20s`, `lag_p99<=40s`).

> ## ⚠️ Data-limitation caveat — the trace format has NO tx types
>
> The JSONL trace format (`bench/trace-format.md` §2) carries **only three
> fields per block event**: `ts_ms`, `height`, `tx_count`. There is **no
> tx-type field anywhere in a trace.**
>
> Consequently the **tx-type MIX** — the input needed to scope epic #121's
> Phase 3 (#125, the matching engine) — is **NOT extractable from any trace**,
> not from the committed fixture and not from the canonical GCS trace. This
> script does not, and cannot, produce a tx-type distribution.
>
> The only tx-type data that exists in-repo is a **single 500-tx sample block**
> (`bench/bench_test.json`), whose distribution is
> `{15: 118 cancel, 17: 168 modify, 21: 169 claim, 14: 45 create}`. That is a
> **sample of size 1** (one block), not a real mix. Do not treat it as a
> population distribution.
>
> **What would resolve the tx-mix question:** per-tx data from a source that
> carries `tx_type`. The feeder now ships a `tx-mix` subcommand for exactly
> this (see *“Capturing the tx-type mix”* below). The catch confirmed by
> probing the live endpoints (2026-06-14): the **explorer does NOT serve
> tx_type** (its block endpoints carry only `block_size` /
> `total_transactions` / `markets`), and the **only HTTP source that exposes
> per-tx `tx_type` — the zklighter mainnet `blockTxs` API — geo-blocks this
> environment with HTTP 403** (the same block the main REST API applies). So
> the tooling exists and runs, but a representative window is **not collectable
> from a geo-blocked network**; the mix — and thus the Phase-3 scoping
> decision — stays **UNRESOLVED on real data** until `tx-mix` is run from a
> non-geo-blocked network.

## Capturing the tx-type mix (issue #128 tx-mix gap)

The tx-type mix is **not in the trace format** and **not served by the
explorer**, so it is captured separately by the feeder's `tx-mix` subcommand,
which aggregates per-block `tx_type` counts from the zklighter `blockTxs` API
over a height window. Operator interface (always go through `make`):

```bash
# Capture the mix over the N most-recent blocks (network; needs a
# non-geo-blocked source — see the caveat above).
make -C bench stream-tx-mix BLOCKS=200

# Capture an explicit inclusive height window.
make -C bench stream-tx-mix HEIGHTS="262870000 262870199"

# Offline: tabulate the in-repo single sample block. Honestly labeled
# sample-size-1 — a SAMPLE, never "the distribution".
make -C bench stream-tx-mix SAMPLE=1
```

Honesty guarantees baked into the tool:

- A capture of fewer than 30 blocks is labeled `sample-size-N`, never “the
  distribution.”
- If `blockTxs` returns HTTP 403 (geo-block), the tool prints the precise
  blocker and exits non-zero — it never fabricates a mix.
- Unknown `tx_type` values are carried through as `type_<n>` rather than
  silently dropped, so a schema drift surfaces.

The four dominant Lighter trading types are `14 create`, `15 cancel`,
`17 modify`, `21 claim` (`bench/feeder/feeder.py:TX_TYPE_NAMES`, from
`circuit/src` naming).

## Usage

```bash
# Default: the committed 201-line fixture (always available, no network).
python3 scripts/trace-distribution/analyze.py

# Any JSONL trace, e.g. the canonical 9,876-block trace fetched from GCS:
gcloud storage cat \
  gs://kunal-scratch-bench-fleet-runs/traces/2026-06-11T0204Z-15m-offpeak/trace_15m.jsonl \
  > /tmp/trace_15m.jsonl
python3 scripts/trace-distribution/analyze.py /tmp/trace_15m.jsonl

# Raw report as JSON (for piping into other tools):
python3 scripts/trace-distribution/analyze.py --json

# Self-check: assert the committed fixture reproduces trace-format.md §8.2.
python3 scripts/trace-distribution/analyze.py --self-check
```

## How it works (no reimplementation)

The script imports the **pure helpers from `bench/feeder/feeder.py`** (the
trace-contract owner) rather than reimplementing them, so the analysis stays
bit-for-bit consistent with how the feeder replays traces:

- `load_trace` — parse JSONL into block events + gap markers.
- `validate_events` — spec §5 monotonicity (`ts_ms` non-decreasing, `height`
  strictly increasing).
- `expand_and_fill` — policies P2 (expand height jumps) → P1 (fill nulls with
  mean-of-non-null) → P4 (round), used for the aggregate-rate figure.
- `aggregate_rate` — Σ(post-fill, post-expansion tx) ÷ span seconds.
- `BLOCK_TX_CAP` — the 500-tx chain cap, for the outlier metric.

`feeder.py` is **not modified**. Its network dependencies
(`websockets`/`requests`) are imported lazily inside the network subcommands,
so importing the module for its pure helpers needs **stdlib only**.

### Metric definitions

| Metric | Meaning |
|---|---|
| `null_fraction` | Share of block events with `tx_count == null` (late-bind window expired before a count was observed — design P4). |
| `mean_non_null` / `median_non_null` | Central tendency of the **observed** tx counts (nulls excluded, no fill). |
| `p50/p90/p95/p99` | Nearest-rank percentiles of observed non-null tx counts (actual observed block sizes, not interpolated). |
| `at_cap_blocks` | Blocks at exactly 500 tx (the chain cap) — the "common-peak" share. |
| `over_cap_blocks` | Blocks with `tx_count > 500` — a spec violation; expected to be 0. |
| `gap_*_ms` | Inter-block arrival gaps (ms) on the **observed** events (real cadence, pre-expansion). |
| `jumps` / `delta_histogram` | Height discontinuities (Δ > 1) — coalesced pushes / brief feed gaps; the burst signal. |
| `skipped_heights` | Total heights skipped across all jumps (Σ(Δ−1)). |
| `aggregate_tx_per_s_p1` | Throughput after the full P2→P1→P4 pipeline — the number the fleet (#75) is sized against. |
| `blocks_per_s_*` | Blocks-per-second distribution in 1-s wall buckets, height-expansion-aware (jumps attributed to their announcing second) — what the conductor's per-second admission/lag sizing reads. |
| `bursts.peak_blocks_in_Ws` | Peak block count in any rolling W-second window — the burst-window characterization for lag-SLO sizing (ADR-0004). |

## Self-check / test

`analyze.py --self-check` asserts the committed fixture reproduces every
documented property in `bench/trace-format.md` §8.2 (201 lines, mean non-null
367.55, max 500, 9 jumps with deltas `[9,4,4,4,4,4,3,2,2]`, 27 skipped heights,
19.64 s span, etc.). This both exercises the script and pins it to ground
truth. A pytest wrapper lives in `tests/test_analyze.py`.

## Data sources

| Source | Blocks | Committed? | Notes |
|---|---|---|---|
| `bench/feeder/fixtures/trace_excerpt.jsonl` | 201 | yes | Verbatim excerpt (trace-format.md §8.2), cut around the largest height jump. The default input. |
| `gs://kunal-scratch-bench-fleet-runs/.../trace_15m.jsonl` | 9,876 | no (GCS) | The canonical 15-min off-peak banked trace (§8.1). Pass the local path after `gcloud storage cat`. |
