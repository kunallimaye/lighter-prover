# Per-transaction positional pre-state corpus — the FINDING D fix

**Issue:** #177 · **Refs:** #75 #172 #174 #165 #61 #72 · **Future path:** #178 (G4)

> **Status: honest-partial.** This delivers the LOCAL correctness fix for
> FINDING D (the distributed cell proving only 1 of `k` chunks per block) plus
> the library machinery for a per-transaction positional pre-state corpus. The
> Layer-0 correctness gate is REAL (it proves every chunk of the real cap block
> at two chunk sizes from positional snapshots and matches them against the
> known-good rolling-state path). The cloud layers (corpus generation at scale,
> distributed smoke, measured ladder) are gated behind this fix and are tracked
> separately; what is and is not executed is stated explicitly below.

## 1. The bug (FINDING D)

`docs/live-benchmark-results.md` recorded that the merged distributed prover
(#173) proves only `witness_index ≡ 0 (mod pool_total)` per block; every other
chunk fails the L1 wire-consistency check
(`Partition containing Wire(...) was set twice with different values`).

Root cause (`bench/src/bin/bench.rs` `run_cell`): the cell built EVERY chunk's
`BlockTx` from the block's INITIAL ledger state (`block.all_assets`,
`block.register_stack_before`, the initial roots, etc.) while selecting a
mid-block tx slice. Only chunk 0's pre-state equals block-initial; chunks
`1..k-1` need the CHAINED intermediate state (the ledger after all PRIOR chunks
applied), so the initial-state seed is inconsistent with the mid-block witness.

## 2. The fix — pre-state is POSITIONAL, snapshotted per-transaction

Pre-state is a property of a POSITION in the tx sequence, not of a chunk. Chunk
boundaries are an overlay the coordinator chooses via `S = tx_per_proof` at
dispatch time (`split_k(tx_count, S) = ceil(tx_count/S)`; ADR-0006 §1.2). So we
snapshot the 8-field ledger pre-state at EVERY tx position `0..=tx_count`:

```
snapshot[i] = ledger pre-state having applied txs 0..i
chunk k at chunk size S  ->  pre-state = snapshot[S * k]
```

The 8 fields (`bench::prestate::ChunkPreState`, promoted from the binary's
`ChunkPreState`, issue #72): `register_stack, all_assets, all_market_details,
system_config, account_tree_root, account_pub_data_tree_root,
account_delta_tree_root, market_tree_root`.

### Why PER-TX and not per-chunk

Per-chunk snapshots would bake `S` into the corpus, forcing regeneration to
benchmark a different `S`. Per-tx snapshots are **S-independent**: the SAME
array serves S=9, S=4, or any S, because chunk `k`'s pre-state is always
`snapshot[S*k]`. The expensive work (the sequential L1 sweep) is IDENTICAL
whether you snapshot per-tx or per-chunk — snapshots are a byproduct of the
sweep; per-tx only costs more STORAGE (highly compressible — consecutive
snapshots differ in only the few accounts one tx touches).

## 3. How snapshots are produced — offline, off the critical path

There is **no** prove-free host-side L1 transition function in the workspace
today (no `apply_block_tx`, no `host_tx_transition`). The ONLY way to advance
ledger state across a tx is to PROVE that step's L1 and read `*_after` from its
public inputs (`BlockTxWitness::from_public_inputs`). So
`bench::prestate::sweep_per_tx_snapshots` runs the sequential L1 sweep at S=1
(one tx per L1 prove), capturing the pre-state before each tx and rolling
forward from each proof's outputs.

This sweep is intrinsically SERIAL but is done OFFLINE — entirely off the
benchmark critical path. The k-way parallel PROVE the distributed prover
measures is fully preserved.

### Production-delivery gap (named loudly — do NOT let it calcify)

- **(a)** Pre-state chaining is intrinsically serial but intrinsically CHEAP;
  doing it offline keeps the parallel prove intact.
- **(b)** The STEADY-STATE production design is the coordinator computing
  pre-states LIVE in microseconds via a host-side prove-free transition
  function (future work, **#178**, mapped G4), OR consuming them from a live
  witness service (#119, Lighter dependency, parked).
- **(c)** Therefore this benchmark measures the PROVING-TIER cost airtight, but
  **pre-state DELIVERY cost is a SEPARATE, currently-unmeasured production
  term**. In the benchmark the corpus is local-disk / cell-materialized, so
  `witness_fetch_ms` is a mounted-resolve / local-materialize floor, NOT a
  production witness-service fetch. "We baked them in" is NOT "that's how
  production works".

## 4. The architectural seam

`bench::prestate::ChunkPreState::block_tx(created_at, txs)` builds the L1
`BlockTx` from a positional pre-state. The distributed cell now calls
`snapshots.at_chunk(S, witness_index).block_tx(...)` instead of using
block-initial state. This seam is identical regardless of WHO fills the
pre-state in: an offline generator for this benchmark; a live coordinator
(#178) or witness service (#119) in production.

For this benchmark the cell materializes the snapshot array once at startup via
the S=1 sweep over its mounted block (a one-time cost off the prove loop).
`LIGHTER_DISABLE_PRESTATE_FIX=1` reverts to pre-#177 block-initial seeding for
A/B confirmation of the bug.

## 5. Layer-0 correctness gate (`bench/tests/prestate_finding_d.rs`)

On the real `bench/bench_test.json` cap block (500 txs), gated behind
`LAYER0_FINDING_D=1`:

1. Generate the per-tx snapshot array via the REAL S=1 sweep.
2. For S=9 AND S=4, prove EVERY full chunk from its positional snapshot
   `snapshot[S*k]`.

Assertions (all REAL, no stubbing):
- All chunks prove; ZERO "set twice" panics (FINDING D fixed).
- S-INDEPENDENCE: the SAME snapshot array serves both S values.
- MATCH-KNOWN-GOOD: each positional-snapshot proof's public inputs MATCH the
  single-process rolling-state path's proof for the same chunk (catches any
  hidden chunk-level vs positional coupling).

Run:
```sh
LAYER0_FINDING_D=1 cargo test -p bench --release --test prestate_finding_d \
  -- --nocapture --test-threads=1
```

### Chunk-count note (honest)

With 500 txs at S=9 there are `floor(500/9) = 55` FULL 9-tx chunks (495 txs);
the 56th chunk in the live `ceil(500/9)=56` SPLIT is a 5-tx partial. The
FINDING D gate proves the 55 full chunks. At S=4 there are `floor(500/4) = 125`
full chunks. The FINDING D fix is chunk-size-agnostic.

The 56th partial chunk is **NOT** "handled by the coordinator's SPLIT exactly as
before" — a short final chunk trips an `itertools::zip_eq` panic in
`circuit/src/block_tx_constraints.rs` because the circuit's tx `Vec` is hard-sized
to exactly `tx_per_proof` at `define` time (`run_cell` aligns the tx count DOWN to
495 to avoid it). **Issue #243** makes the true `ceil(500/9)=56` reachable by
PADDING the 56th chunk to a full 9 txs: `5` real leftover txs + `4`
`TX_TYPE_EMPTY` txs. Empty txs mutate nothing but run every unconditional Merkle
verification, so each padding empty must carry HONEST mid-block sibling-paths for
the chosen empty leaf index (`EMPTY_ACCOUNT_INDEX = 2`, never touched by the 500
txs) against the CURRENT account-family trees — NOT the all-empty/genesis paths
(which fold the empty leaf to the EMPTY root and clash with the chained mid-block
root, "Partition … set twice").

#243 implements this host-side, with no circuit change:

- **Native account-family leaf hashes** (`bench::account_family_native`) port the
  in-circuit `AccountTarget::hash` / `AccountDeltaTarget::hash` / `MarketTarget::hash`
  to plain Rust over Poseidon2, verified **bit-for-bit** against the circuit by
  cheap one-shot extractor proves.
- An **off-circuit sparse Merkle tree** (`bench::account_family_tree`) reconstructs
  an EMPTY leaf's honest sibling-path from the per-tx `(leaf, proof)` data the S=1
  sweep already iterates over. **Issue #263 correction:** the original #243
  approach harvested a FIXED empty index (2) by unioning per-tx proofs across
  positions; this never worked — index 2's neighbouring subtrees ({0,1}
  treasury/insurance, index 3) are never touched so their nodes are never
  observed, and unioning nodes across positions with different evolving roots is
  incoherent, so the harvester returned `None` for all four trees at every
  position (the gate failed). The fix derives an **adaptive** empty index per
  position from a SINGLE real touched account's coherent proof
  (`account_family_tree::empty_path_from_proof`): descend into the account's
  first empty sibling subtree (common to the account / pub_data / delta trees) to
  a guaranteed-empty leaf whose full path folds a ZERO leaf to that position's
  root — no accumulation, fully coherent. The chosen index is constrained to a
  normal account slot (`2 ..= MAX_ACCOUNT_INDEX`, excluding the `NIL_ACCOUNT_INDEX`
  sentinel that the circuit treats specially).
- The sweep variant `sweep_per_tx_snapshots_with_paths` captures those paths +
  their chosen indices at each position and stores them in the corpus's optional
  `sibling_paths` field (**schema 1.1**, backward-compatible with #257's 1.0 — a
  1.0 reader loads 1.1 unchanged). Each captured path is fold-validated to the
  position's PROVEN root, so a wrong path is never emitted (incomplete-data
  positions store `None`).
- `empty_witness::mid_block_empty_tx(paths)` substitutes those honest paths into
  an otherwise-empty tx and points its empty leaves at the chosen adaptive
  indices. `run_cell --pad-final-chunk` (off by default) appends the padded 56th
  chunk so the cell reaches the true `ceil(tx_count/S)`. **The empties go FIRST in
  the chunk** (before the real leftover txs): an empty tx mutates nothing, so it
  must verify its empty leaf against the root it ENTERS with — the chunk's INPUT
  pre-state, which the captured paths fold to. Placing empties after the real txs
  would verify them against the (evolved) post-real-tx root the captured path does
  NOT match (a "Partition … set twice" witness conflict). Empties-first leaves the
  chunk's net mutation and output roots identical.

The padded 56th chunk (4 empties + 5 real) is a benchmark-valid stand-in (per-tx
prove cost is tx-type-flat), NOT a novel mainnet block; pre-state DELIVERY remains
a separate production term. See the env-gated `padded_final_chunk_proves_with_honest_paths`
gate (`LAYER0_PAD_FINAL_CHUNK=1`).

## 5a. Corpus PERSISTENCE — serialize once, mount thereafter (issue #257)

Implemented in `bench::prestate_store`. The §4 sweep is INTRINSIC (the static
`bench_test.json` cannot be stitched without proving — see #243), but it used to
be REGENERATED IN-MEMORY on every `run_cell` startup. #257 removes the REPEATED
cost: the swept `PreStateSnapshots` is serialized to a versioned, path-aware
corpus and mounted, so the sweep runs **at most once per height**.

- **Serde:** `Serialize` was added to the 8-field state types (`Asset`,
  `MarketDetails`, `RegisterStack` + nested `BaseRegisterInfo`, `SystemConfig`),
  including a matching `bigint_to_int` serializer for `MarketDetails`'s
  custom-deserialized `funding_rate_prefix_sum: BigInt` so it round-trips with
  its existing `int_to_bigint` deserializer.
- **Format:** versioned (`schema_version`, currently `1.0`), gzip-framed JSON.
  Each position carries the 8 state fields AND an OPTIONAL, per-position
  `sibling_paths` field — present-but-unpopulated in #257. Issue **#243** fills
  that field (shipping as schema `1.1`, same MAJOR) **without a format
  revision**; a `1.0` reader loads a `1.1` corpus unchanged. A different MAJOR
  is rejected honestly.
- **Measured size (501-snapshot probe,
  `prestate_store::tests::probe_corpus_size_501_snapshots`):** raw JSON
  ≈ 19.5 MiB, gzip ≈ 0.30 MiB — **≈ 65× compression**. (The probe corpus is
  mostly-empty arrays, so this is a representative compressibility floor; a
  denser real corpus is larger raw but gzip still dominates.) A full delta
  scheme is deliberately NOT implemented — gzip is sufficient for this issue.
- **Wiring:** `save_prestate_corpus_to_path` / `load_prestate_corpus_from_path`
  (local disk, tests + mounted corpus) and
  `save_prestate_corpus_to_store` / `load_prestate_corpus_from_store` over the
  existing `GcloudStorage` byte transport, keyed by
  `prestate_object_key(height) = "{height}/p/corpus"` (the `/p/` segment is
  disjoint from the leaf `{height}/{wi}` and merge `{height}/m/...` namespaces).
- **Cache-or-generate:** `run_cell` LOADs the corpus (local path
  `--prestate-corpus-path` / `LIGHTER_PRESTATE_CORPUS`, else the proof store)
  when present and only falls back to the sweep when absent — saving the
  freshly-swept corpus on the miss path. A clear log line states LOADED-from-
  cache vs REGENERATED-via-sweep. The `LIGHTER_DISABLE_PRESTATE_FIX=1` A/B
  toggle is preserved (it skips the corpus entirely).

## 6. Corpus schema (Layer 1 — design; generation gated on Layer 0)

The 100-block synthetic corpus (Decision 3) reuses PR #166's 100-height
`{height, witness_index}` layout and adds a per-tx pre-state snapshot store per
height. Source blocks are SYNTHETIC, shaped to #128's distribution via the G2
generators (`tools/witness-reconstructor/`). KNOWN LIMIT: Cancel(15) +
non-crossing Modify(17) + empties(0) + compositions; NO Claim(21)/Create(14)
(matching engine #125 closed; spike #157: per-tx prove cost is tx-type-flat to
<6%). Cost-projection use supported; correctness-coverage use is not.
