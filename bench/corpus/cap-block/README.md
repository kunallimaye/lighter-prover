# Cap-block pre-state corpus

A pre-captured, schema-1.1, paths-bearing pre-state corpus for the bundled
cap block at `bench/bench_test.json` (height `186974592`). Committed so
`run_cell` startup loads it in milliseconds instead of re-running the
S=1 pre-state sweep (~6 minutes on x86, hours on arm/Axion) on every run.

Established by #257 (persistence + format), populated by #243 (path
capture), corrected by #263 (sibling-path harvester), tracked by #265.

## What this is

The serialized form of `bench::prestate::PreStateSnapshots` produced by
`bench::prestate::sweep_per_tx_snapshots_with_paths` and persisted via
`bench::prestate_store::save_prestate_corpus_to_path`. Wire form is
gzip-framed JSON; schema is version `1.1` — same readers as `1.0`, plus
a populated optional `sibling_paths` field per snapshot.

## Provenance

| field | value |
|---|---|
| File | `captured_corpus.gz` |
| Size | 5,146,673 bytes (4.91 MiB) |
| SHA-256 | `86a5e9e5309d35d451a6240a2c51db414872e7f566773ea51eaad005eefb75a4` |
| Format | gzip-framed JSON (`bench::prestate_store::PreStateCorpus`) |
| Schema version | `1.1` (paths-bearing) |
| Snapshot count | 501 (per-tx positional pre-state for 500 txs + 1 trailing post-state) |
| Snapshots with `empty_index_sibling_paths.is_some()` | 500 / 501 |
| Position 500 paths | `None` (trailing post-state — no following tx to harvest from; structurally correct) |
| Position 495 paths | **present** (the padded-chunk pre-state at S=9 × 55 full chunks — the slot #243 needs) |
| Source block | bundled `bench/bench_test.json` cap block |
| Block height | `186974592` |
| Block created_at | `1772470922509` |
| Source-repo HEAD | `caaae0da25477ce67bdea9339ffd63f50d45a60c` (merge of #264 on top of #260/#259) |
| Capture machine | x86_64, AMD EPYC 7B13 (32 cores), ~126 GiB RAM |
| Capture timings | build 49.6s, sweep 315.4s (~0.629s / L1-prove × 501 proves), save <1s; ~6m5s wall total |

Adaptive empty-leaf indices recorded at position 495 (account family /
market): `account_index = 281474976579584`, `market_index = 256` —
matching the `EmptyIndexSiblingPaths` shape that #263 finalized.

## How to consume

Point `--prestate-corpus-path` / `LIGHTER_PRESTATE_CORPUS` at this file
and `run_cell` will load it via
`bench::prestate_store::load_prestate_corpus_from_path` (a few hundred
ms) and skip the S=1 sweep entirely. If the file is missing or its
schema MAJOR is incompatible with the running build, the loader returns
an honest error — it never fabricates snapshots — so a stale or wrong
corpus fails loudly, not silently.

## Reproduce / regenerate

Run from `lighter-prover/bench/`, with a release `bench` binary built
at `../target/release/bench`:

```sh
LIGHTER_PROJECT=dummy \
LIGHTER_PAD_FINAL_CHUNK=true \
LIGHTER_PRESTATE_CORPUS=<out_dir>/captured_corpus.gz \
LIGHTER_CHUNK_SUBSCRIPTION=dummy-sub \
LIGHTER_RESULTS_TOPIC=dummy-topic \
RUST_LOG=info \
  ../target/release/bench --mode cell \
    --tx-per-proof 9 \
    --tx-limit 500
```

Notes:

- The three dummy env-vars (`LIGHTER_PROJECT`, `LIGHTER_CHUNK_SUBSCRIPTION`,
  `LIGHTER_RESULTS_TOPIC`) satisfy `run_cell`'s upfront validation. After
  the save log line — `cell: saved pre-state corpus to '…' (… gzip bytes);
  future runs LOAD instead of sweeping (issue #257)` — the cell loops
  trying to pull from the dummy Pub/Sub subscription. Kill it
  (`SIGTERM`/`SIGKILL`) once the file is on disk.
- **`--tx-limit 500` is required.** Without it the cell defaults to
  `tx_limit=480`, which produces only 53 chunks rather than the 56
  (55 full + 1 padded) that the cap block actually contains.
- `LIGHTER_PAD_FINAL_CHUNK=true` is what causes position 495 to carry
  the honest empty-index sibling-paths the padded 56th chunk consumes.

**Regenerate this corpus if and only if:**

1. `bench/bench_test.json` changes (it is the source block), OR
2. The corpus schema bumps incompatibly — i.e. a **MAJOR** version
   change (`2.x`). Minor `1.x` bumps stay backward-compatible by
   design (see "Compatibility" below), so a `1.1` reader ingesting
   a future `1.2` corpus is fine.

## Compatibility

Schema `1.1` is backward-compatible with `1.0`:

- A `1.0` reader can ingest a `1.1` corpus — the `sibling_paths` field
  is `#[serde(default, skip_serializing_if = "Option::is_none")]`, so
  a `1.0` reader simply ignores the populated field and gets the same
  roots/state it always did.
- A `1.1` reader can ingest a `1.0` corpus — the optional field is
  absent and rehydrates as `None`, matching `#257`'s roots-only
  snapshots exactly.

The schema MAJOR is what the loader gates on
(`PreStateCorpus::check_compatible`). A `2.x` corpus is rejected as
`CorpusError::IncompatibleVersion` rather than silently mis-parsed —
that is the trigger to regenerate.
