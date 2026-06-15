# Trace Format — Streaming Bench Contract

Status: **pinned** (policies pilot-validated 2026-06-11)

This document specifies the JSONL trace format that binds the streaming
bench producer and consumer:

- **Producer** — the feeder (`record`, `replay`, `synth-peak`): issue #48
- **Consumer** — `bench --stream`: issue #49
- **Origin** — split from issue #46 (producer/consumer decomposition);
  this contract supersedes the format sketch in #46 and lands first.

Both implementations MUST conform to this document. Any change to the
format requires updating this spec in the same PR.

## 1. File format

A trace is a UTF-8 text file (or stream) with **one JSON object per
line** (JSONL). Three line types exist:

| Line type | Discriminator | Cardinality |
|---|---|---|
| Provenance header | top-level `"provenance"` key | exactly 1, always the first line |
| Block event | `"height"` key, no `"gap"` key | 0..n |
| Gap marker | `"gap": true` | 0..n |

Blank lines are forbidden. Unknown top-level keys MUST be ignored by
consumers (forward compatibility) and MUST NOT be emitted by producers
without a spec update.

## 2. Block event schema

```json
{"ts_ms": 1781143874390, "height": 260138266, "tx_count": 500}
```

| Field | Type | Meaning |
|---|---|---|
| `ts_ms` | int | Unix epoch milliseconds at which the block was observed (record) or scheduled (replay/synth) |
| `height` | int | L2 block height |
| `tx_count` | int **or** null | Transactions in the block; `null` when the recorder could not resolve the count before flush |

`tx_count` is **int or null — never a float** (see policy P4).

## 3. Gap markers

The recorder emits a gap marker when the upstream feed is interrupted
(WebSocket disconnect, reconnect backoff, subscription lapse):

```json
{"gap": true, "ts_ms": 1781143880000, "reason": "ws_disconnect"}
```

| Field | Type | Meaning |
|---|---|---|
| `gap` | bool (always `true`) | Discriminator |
| `ts_ms` | int | Epoch ms when the interruption was detected |
| `reason` | string | e.g. `"ws_disconnect"`, `"ws_reconnect_timeout"`, `"subscribe_lapse"` |

**Consumer policy: skip-and-count.** Consumers MUST NOT enqueue work
for gap markers; they MUST count them and report the total in run
output. Height continuity rules (section 5) are evaluated across gap
markers as if the markers were absent — but a height discontinuity that
immediately follows a gap marker is *not* expanded (section 6.2): those
blocks were genuinely unobserved, not coalesced.

## 4. Provenance header

The **first line** of every trace is a comment-style object recording
how the trace was generated. Exactly one header line; always first.

Synthesized / renormalized traces:

```json
{"provenance": {"generator": "synth-peak", "params": {"block_rate": 1.0, "tx_count": 216, "duration_s": 60.0}, "generated_at": "2026-06-11T08:00:00Z"}}
```

`synth-peak` is configured on **two independent load axes** (issue #217):

- `block_rate` — blocks/sec; sets the cadence between blocks
  (`cadence_ms = 1000 / block_rate`), independent of `tx_count`.
- `tx_count` — txns per block; every block carries exactly this many
  transactions.

These axes are orthogonal: the coordinator splits work into
`k = ceil(tx_count / S)` chunks, so a **constant `tx_count`** pins `k`
regardless of how fast blocks arrive — the canonical way to drive a
fixed-k stream (e.g. `tx_count=216` → k=24, `tx_count=288` → k=32 at
S=9). Because `tx_count` is constant and heights advance by exactly 1
per block, constant-`tx_count` synth traces have **no height jumps**
(no P2 expansion) and **no nulls** (no P1 fill).

When `synth-peak --rate` (aggregate tx/s) is used instead of
`--block-rate`, the block rate is derived as `rate / tx_count` and the
legacy `peak_rate` axis is recorded in `params` too (back-compat: with
the default `tx_count=500` this reproduces the old `cadence = 500/rate`).

The earlier single-axis form (only `peak_rate`, `tx_count` fixed at 500)
remains valid provenance:

```json
{"provenance": {"generator": "synth-peak", "params": {"peak_rate": 9000, "duration_s": 300}, "source_trace": "gs://kunal-scratch-bench-fleet-runs/traces/2026-06-11T0204Z-15m-offpeak/trace_15m.jsonl", "generated_at": "2026-06-11T08:00:00Z"}}
```

```json
{"provenance": {"generator": "replay --target-rate", "params": {"target_rate": 8000, "fill": "mean", "fill_value": 401}, "source_trace": "gs://.../trace_15m.jsonl", "generated_at": "2026-06-11T08:00:00Z"}}
```

Live recordings carry a header too:

```json
{"provenance": {"generator": "record", "params": {"endpoint": "wss://...", "duration_s": 900}, "generated_at": "2026-06-11T02:04:00Z"}}
```

| Field | Required | Meaning |
|---|---|---|
| `generator` | yes | `"record"`, `"replay --target-rate"`, or `"synth-peak"` |
| `params` | yes | Generator parameters sufficient to reproduce the trace |
| `source_trace` | for derived traces | Identity (URI) of the source trace |
| `generated_at` | yes | RFC 3339 UTC timestamp of generation |

**Pre-spec exemption:** traces captured before this spec existed (e.g.
the canonical banked trace, section 8) have no header. Consumers MUST
accept a trace whose first line is a block event, treating it as a
pre-spec capture with unknown provenance. Producers MUST always emit a
header.

## 5. Monotonicity rules

Evaluated over block events only (header and gap markers excluded):

1. **`ts_ms` is non-decreasing.** Equal timestamps are legal and
   expected — height-jump expansion (P2) deliberately emits multiple
   events at the same `ts_ms`.
2. **`height` is strictly increasing** across consecutive non-gap
   events. Equal or decreasing heights are a hard error.
3. **Duplicates are forbidden.** No two block events may share a
   `height`.

Consumers MUST reject (fail fast, non-zero exit) any trace violating
these rules.

## 6. Pinned policies

All four policies were validated by prototyping against the canonical
banked trace (section 8) on 2026-06-11.

### 6.1 P1 — `--target-rate` denominator

A trace's **aggregate rate** is:

```
aggregate_rate = Σ(tx_count, post-fill, post-expansion) ÷ time span (s)
```

The **null-fill value is the mean of the trace's non-null
`tx_count`s**. For the banked off-peak trace: mean 400.72 → aggregate
4,438.3 tx/s. `replay --target-rate R` scales by `R ÷ aggregate_rate`.

Rationale:

- Nulls are not uniformly distributed — they skew toward burstier
  blocks, so dropping them biases the denominator.
- A naive null-skipping sum mis-scales the target-rate factor by
  **1.69×**.
- Median-fill (500, since the median block is at the chain cap)
  overshoots the aggregate by ~25%.

### 6.2 P2 — Height-jump expansion

The upstream WS height channel **coalesces pushes during bursts**: a
single push can advance the height by k > 1. In the banked trace,
~0.9% of heights are skipped this way (max 8 skipped in one jump);
spot checks confirmed the intermediates are real blocks, not chain
gaps.

**Policy:** during replay/synthesis, a jump of k expands into **k
per-block events at the same `ts_ms`**. The intermediate heights
receive the fill value (P1); the final height keeps the observed
`tx_count`.

Rationale: the consumer enqueues `ceil(tx_count / tx_per_proof)` proof
jobs **per line**. Leaving a jump as one summed line would (a)
under-count chunk jobs and (b) fabricate >500-tx blocks — 500 is the
chain's per-block cap, so such blocks never exist on-chain.

Exception: discontinuities immediately following a gap marker
(section 3) are *not* expanded — those blocks were unobserved during a
feed outage, not coalesced into a push.

### 6.3 P3 — Loop seam

For `replay --loop`, the gap between the last event of iteration *n*
and the first event of iteration *n+1* is:

```
seam = median inter-block gap of the trace × active speed factor
```

(Banked trace: median gap 87 ms.) This keeps looped streams
statistically indistinguishable from the trace body at the seam
instead of introducing a zero-gap burst or an artificial stall.

### 6.4 P4 — Integer tx_counts

Filled values (P1, P2) are **rounded to the nearest integer** in all
emitted streams. The schema's `tx_count` is `int | null`, never a
float, so the JSONL stays schema-clean for the Rust consumer's strict
deserialization.

## 7. Validation summary (normative checklist)

A conforming trace:

- [ ] Every line parses as a JSON object
- [ ] First line is a provenance header (post-spec traces)
- [ ] `ts_ms` non-decreasing over block events
- [ ] `height` strictly increasing over block events; no duplicates
- [ ] `tx_count` is int or null on every block event
- [ ] Gap markers carry `gap: true`, `ts_ms`, `reason`

## 8. Reference data

### 8.1 Canonical banked trace (GCS, not committed)

`gs://kunal-scratch-bench-fleet-runs/traces/2026-06-11T0204Z-15m-offpeak/trace_15m.jsonl`

| Property | Value |
|---|---|
| Blocks (lines) | 9,876 |
| Height range | 260,133,716 – 260,143,683 |
| Time span | 900.0 s |
| Inter-block gap | median 87 ms / p95 204 ms / max 337 ms |
| `tx_count` null fraction | 40.16% |
| tx/block | median 500 / mean (non-null) 400.72 |
| Height jumps (Δ > 1) | ~50 (≈0.9% of heights skipped; max 8 per jump) |
| Aggregate rate (P1 fill) | 4,438.3 tx/s |

### 8.2 Committed fixture

`bench/feeder/fixtures/trace_excerpt.jsonl` — a **verbatim excerpt**
of the banked trace (lines 4521–4721, 1-indexed), cut around the
trace's largest height jump. Measured properties:

| Property | Value |
|---|---|
| Lines | 201 (all block events) |
| Height range | 260,138,266 – 260,138,493 (228 heights spanned) |
| `ts_ms` range | 1,781,143,874,390 – 1,781,143,894,030 (19.64 s) |
| Jumps (Δ > 1) | 9 — deltas: 2, **9**, 4, 4, 4, 4, 4, 3, 2 (27 heights skipped total) |
| Largest jump | 260,138,395 → 260,138,404 (Δ = 9, i.e. 8 skipped heights — the trace maximum) |
| Null `tx_count` | 40 (19.9%) |
| Non-null tx/block | min 1 / max 500 / mean 367.55 |
| Monotonicity | `ts_ms` non-decreasing ✓, `height` strictly increasing ✓ |

**Pre-spec exemption note:** the banked trace predates this spec and
carries no provenance header. The fixture is a *verbatim* excerpt, so
it is exempt from the header requirement (section 4); fabricating a
header inside a file presented as a verbatim capture would falsify it.
Tests using this fixture exercise the headerless pre-spec path.

## 9. Cross-references

- Issue #46 — original streaming-bench issue (superseded origin; split
  into #47/#48/#49)
- Issue #47 — this contract
- Issue #48 — feeder (producer); MUST cite this doc as its contract
- Issue #49 — `bench --stream` (consumer); MUST cite this doc as its
  contract
