# Synthetic block-corpus generator (issue #165)

`Refs #128 #121 #144`. **Generation only** — no proving fleet, no cloud spend,
no matching engine.

Produces a block corpus whose **block-size distribution matches the real
mainnet load** (derived from the real banked trace via the real analyzer),
laid out in the `{height, witness_index}` **MountedCorpus** addressing the
conductor consumes (`bench/src/conductor/witness.rs`, ADR-0008 §1.1/§1.4).

## What it is

The conductor's witness plane (PR #164, ADR-0008) addresses witnesses by
`WitnessKey{height, witness_index}` and resolves them through an **in-memory**
`MountedCorpus<T>` (a `HashMap` populated by `mount_block(height, slices)`).
There is **no prescribed on-disk corpus format** in the resolver — the binary
(`bench/src/bin/bench.rs`) builds the k=1 corpus *in memory* by pre-slicing
`bench_test.json` into `S`-tx chunks indexed `0..k-1`.

This generator emits the **on-disk artifact a loader reads to populate that
resolver**: a flat `{height, witness_index}` index + a provenance manifest,
committed under `bench/corpus/`. The corpus is a **generalization of the k=1
`bench_test.json` mount to many heights**, with the per-height block sizes
drawn from the real distribution.

## The distribution is REAL, never invented

Band weights come from running the real analyzer on the real trace:

```sh
gcloud storage cat gs://kunal-scratch-bench-fleet-runs/traces/2026-06-11T0204Z-15m-offpeak/trace_15m.jsonl > /tmp/trace_15m.jsonl
python3 tools/corpus-gen/gen_corpus.py --trace /tmp/trace_15m.jsonl --n-blocks 100 --chunk-size 100
# or via make:
make -C bench corpus-gen TRACE=/tmp/trace_15m.jsonl N=100 S=100
```

The generator shells out to `scripts/trace-distribution/analyze.py --json`,
reads the `block_size.bands` field, asserts the bands partition the non-null
set exactly, then scales the per-band proportions down to `--n-blocks`
(largest-remainder rounding so the **~73.6%-at-cap** shape is preserved).

**Fallback (clearly labeled):** `--from-doc` (or `make ... DOC=1`) uses the
documented #163 band counts instead of recomputing. Only use this when GCS is
unreachable; the manifest records the source as `DOCUMENTED ... (NOT freshly
recomputed)`.

The real distribution (9,876 blocks / 5,910 non-null, off-peak 15m):

| Band | Count | % of non-null |
|---|---|---|
| =1 | 660 | 11.17% |
| 2-49 | 122 | 2.06% |
| 50-99 | 124 | 2.10% |
| 100-249 | 301 | 5.09% |
| 250-399 | 219 | 3.71% |
| 400-499 | 136 | 2.30% |
| **=500 (cap)** | **4348** | **73.57%** |

## HONEST SCOPE — what it does and does NOT synthesize

**Does** synthesize: the `{height, witness_index}` addressing layout, the
real block-size distribution, and a provenance manifest.

**Does NOT** synthesize: witness **bytes** / Merkle roots for the lower bands.
The repo ships exactly **one** real chain-VALID, fully prover-serializable
500-tx (cap) block — `bench/bench_test.json` (tx dist
`{15:118, 17:168, 21:169, 14:45}`, **sample-size-1**), validated bit-for-bit by
`tools/witness-reconstructor`. The corpus uses it as the **cap-band
representative seed** (`is_real_seed: true`, `synthesized: false`); every other
block is a `{height, witness_index}` **layout placeholder** that references the
seed and does not materialize witness bytes.

Synthesizing arbitrary fully-serializable sub-cap blocks needs signatures +
public-data + the full account-tree leaf, gated on #120/#126/#125 (see
`tools/witness-reconstructor/largerblock.go` HONEST SCOPE — the no-engine
generator only composes + validates a single-market `order_book_root` chain,
not a fully serializable block). **Honest-partial > fake-complete.**

## Output (`bench/corpus/`)

- `manifest.json` — provenance (trace source, analyzer command, real + scaled
  per-band counts, scale factor, seed sha256, honest-scope flags) + per-block
  summary.
- `index.json` — the flat `{height, witness_index, tx_count}` index a loader
  feeds into `MountedCorpus::mount_block`.

## Validation

```sh
# (a) the representative cap block is chain-VALID (bit-for-bit Merkle roots):
cd tools/witness-reconstructor && go run . -json ../../bench/bench_test.json && go test ./...

# (b) the corpus LOADS via the real MountedCorpus {height, witness_index} resolver:
make -C bench corpus-test     # cargo test --test corpus_mount
```
