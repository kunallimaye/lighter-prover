# ADR-0009: Distributed benchmark load construction — reference-based dispatch, size/height variation, and the content-variety boundary

**Status**: Accepted
**Date**: 2026-06-15
**Verified-at-tip**: `394f7a2bae246a9a61acb65304f2fe240d599323`
**Issues**: refs #144 (G2 status), refs #184 (content-varied prover-serializable
synthetic blocks — full witness synthesis); consumes ADR-0004 §6.1 (the L4 serial term),
ADR-0006 (the conductor), ADR-0008 §1.4 (the k=1 `bench_test.json` witness
addressing)

> **Numbering note.** This ADR takes **0009** — the next free number, verified
> by listing `docs/decisions/` at the tip above: `ADR-0001`
> (container-topology), `ADR-0003` (prover-cell streaming), `ADR-0004` (unified
> recursive distribution), `ADR-0005` (L6 inner-wrapper KZG sidecar),
> `ADR-0006` (distributed-prover conductor), `ADR-0007` (GCP fleet bench),
> `ADR-0008` (witness delivery plane). `ADR-0002` is **reserved** for #10's
> `ADR-0002-l4-l8-driver.md` and is deliberately left free.

> **Why this ADR exists (read this first).** This is the canonical answer to
> *"how is the distributed benchmark's load constructed, and why is that
> valid?"* It exists to STOP a recurring failure mode: the project has twice
> treated "G2 generation complete" as done when in fact only *generators* plus
> a *one-block load instrument* exist. The distinction between "generators
> exist" and "provable VARIED blocks exist" had no durable record, so it was
> re-explained and re-litigated more than once. A future session should read
> THIS ADR instead of re-investigating the benchmark's load path. If you came
> here to ask "can the bench vary load without new prover code?" — yes, by size
> and block-count (§2); "can it vary tx CONTENT?" — no (§3), that is genuinely
> blocked (§4).

---

## 1. Context — what the distributed fleet actually passes on the bus

The distributed prover benchmark passes **references, not block content.**

- The coordinator pulls a `BlockMessage { height, tx_count }` — **two
  integers** — from the dispatch subscription
  (`bench/src/conductor/pubsub.rs:79-81`, `BlockMessage`).
- It SPLITs the block into `k = ceil(tx_count / S)` chunks
  (`bench/src/bin/bench.rs:1971-1972`, `split_k` from
  `bench/src/conductor/dispatch.rs:49`).
- It publishes `k` `ChunkMessage { height, witness_index, tx_count }`
  **references** to the chunk topic (`bench/src/bin/bench.rs:1978-1989`;
  `ChunkMessage` at `bench/src/conductor/pubsub.rs:103-107`).

**Block bytes and witness bytes never cross the bus.** Each cell loads its OWN
local `bench/bench_test.json` (`bench/src/bin/bench.rs:1547`, inside `run_cell`
which begins at line 1486; the load + pool partition is lines 1538-1568) and
proves slices of it. The wire `witness_index` selects the local slice by
`witness_index % pool_total` (`bench/src/bin/bench.rs:1706`, inside the
chunk-consume loop 1698-1729). The `{height, witness_index}` reference resolves
to local witness bytes through the cell-side resolver
(`bench/src/conductor/witness.rs:129`, `resolve`) — this is the k=1
`bench_test.json` special case designed in **ADR-0008 §1.4**.

Per-chunk seeding is positionally correct: each chunk is seeded from its own
positional pre-state `snapshot[S * witness_index]`, not block-initial state
(`bench/src/bin/bench.rs:1710-1729`; the FINDING-D fix, `bench/src/prestate.rs`
— see the per-tx positional snapshot corpus there). This is why a *prefix* of N
txs of the real block is itself a valid, provable block: the cell seeds each
chunk independently from the per-tx pre-state, so a smaller `tx_count` simply
dispatches fewer chunks over a correctly-seeded prefix.

> **Citation provenance.** Verified at tip `394f7a2`. The facts here describe
> behavior introduced by the per-tx positional-pre-state fix
> (commit `5baf104`, the per-tx pre-state change), which is present in `main`
> at `394f7a2` (4 commits ahead of `5baf104`). The in-code comments attribute
> that fix to issue #177; the commit subject references #180. See §7 (citation
> drift) for the line-number corrections made while writing this ADR.

---

## 2. Decision — vary load by SIZE and BLOCK-COUNT today, with no new prover code

Because the bus carries references and each cell seeds chunks independently,
the benchmark's load can be varied **today**, with **no new prover code**, along
two axes:

1. **Size variation.** Publish block jobs with different `tx_count` values
   (e.g. 100 / 250 / 400 / 500). The coordinator splits each into a different
   `k` → varied chunk fan-out, varied fold width, varied L4 input cardinality.
   A prefix of N txs of the real block is a valid provable block (§1).

2. **Block-count variation.** Publish the same content under many distinct
   `height`s → distinct block JOBS (distinct dispatch, distinct
   competing-pull, distinct fan-out, distinct gather/fold) even though the
   proven bytes repeat.

The committed `bench/corpus/index.json` is **exactly this layout**: 421 entries
across **100 distinct heights**, with banded `tx_count` sizes, every entry
referencing the one real validated seed (`bench_test.json`). Driving the feeder
from the corpus index is therefore the cheapest path to size+height-varied
load. (The generator `tools/corpus-gen/gen_corpus.py` writes this flat
`{height, witness_index, tx_count}` index plus a provenance manifest in which
every block carries `synthesized: false`,
`tools/corpus-gen/gen_corpus.py:158` — i.e. it produces *references to the real
seed*, never fabricated content.)

> **Note on bands.** The size bands "100/250/400/500" above are an *example* of
> what one CAN publish. The *committed* `index.json` is capped at `tx_count
> = 100` and uses bands `{1, 25, 50, 75, 100}` (counts `{11, 6, 2, 7, 395}`).
> The HOW-TO (`docs/howto-varied-load-benchmark.md`) records the recommended
> bands to publish against the live fixture (whose `bench_test.json` holds 500
> txs) and ties them to G1's real bimodal block-size distribution.

---

## 3. What size+height-varied load DOES and does NOT measure

### What it DOES validly measure

These mechanics depend on **block count and block size, NOT on tx content**, so
size+height variation exercises them faithfully — and with **real L1
(`BlockTxCircuit`) + L2 (`BlockTxChainCircuit`) proofs** on the cells (never
stubbed; the cell's L1/L2 prove path is in `run_cell`,
`bench/src/bin/bench.rs`):

- multiple coordinators chunking multiple blocks;
- per-block SPLIT into varied `k` (`split_k`, §1);
- chunk fan-out + bus contention;
- results GATHER + FOLD (the coordinator gather/fold loop,
  `bench/src/bin/bench.rs:1991-2045`);
- **L4 fold-width variation** — the suspected serial bottleneck and "the next
  structural lever" (**ADR-0004 §6.1**, L4 dominance; L4 is the largest single
  term, serial per block, untouched by either distribution grain);
- the witness-resolve floor (the local-resolve `witness_fetch_ms`, ADR-0008).

### What it does NOT measure — CONTENT VARIETY

Every proven slice is drawn from the **same one real block's 500 txs**. There
are:

- no varied signatures,
- no varied account-tree leaves,
- no tx-type-mix diversity.

Per-tx-type cost sensitivity is therefore **sample-size-1**. (This is consistent
with spike #157, which measured per-tx prove cost as tx-type-flat to <6% on that
single sample — a useful result, but still one block of ground truth.)
Size+height-varied load **cannot** turn sample-size-1 into a content
distribution; that requires §4.

---

## 4. The wall — content-varied, prover-serializable NOVEL blocks (GATED)

Fully content-varied, prover-serializable **novel** blocks require **full
witness synthesis**: signatures + account-tree leaves + all Merkle proofs +
public-data, chained tx-to-tx. **The existing generators do NOT produce this:**

- The Go `tools/witness-reconstructor/ -block-emit` path prints an
  `order_book_root` chain as **DIAGNOSTIC TEXT** — it is not a `Block<F>` JSON
  and is not loadable by a cell. Its own header says so:
  `tools/witness-reconstructor/largerblock.go:39-48` ("HONEST SCOPE: this
  composes + validates the ORDER-BOOK state chain … A fully prover-serializable
  novel block additionally needs signatures, public-data, and the full
  account-tree leaf — out of scope for the no-engine phases").
- `tools/corpus-gen/gen_corpus.py` emits **only** the reference index +
  manifest (every entry `synthesized: false`,
  `tools/corpus-gen/gen_corpus.py:158`) — references to the one real seed, no
  synthesized content.

So content-varied synthesis is genuinely **BLOCKED / unbuilt**, not merely
unwired. It is tracked in its own issue, **#184** (content-varied
prover-serializable synthetic blocks — full witness synthesis).

> **Gated dependency.** The prior gating reference for this work — #125
> (matching engine + Claim/Create) — was **closed as not-planned** (it is not
> required for benchmarking; spike #157). Closing #125 did NOT deliver
> content-varied synthesis; it removed one rationale for the matching engine.
> Full witness synthesis is a *separate* scope and needs its own issue (**#184**),
> independent of #125.

---

## 5. Consequences

1. **Size+height-varied load is the SANCTIONED interim load instrument** for
   measuring fleet mechanics, throughput, fold, and L4 fold-width — exactly the
   measurements ADR-0004 §6.1 names as the next structural questions. It runs on
   real L1+L2 proofs and the real conductor (ADR-0006), using the committed
   `bench/corpus/index.json` layout (§2).

2. **Single-identical-block replay is explicitly NOT sufficient** and is the
   **prior rejected approach**: replaying one block under one height exercises
   neither varied `k` (fan-out / fold width / L4 cardinality) nor multi-block
   dispatch/gather. The cheap, correct thing is to vary **size AND height**, not
   to repeat one fixed job. The HOW-TO is written to make the right thing the
   easy thing so nobody reverts to single-block replay.

3. **Content variety is OUT OF SCOPE for this instrument** and is gated on full
   witness synthesis (§4). Benchmarks driven by this instrument must carry the
   scope caveat (the HOW-TO's "Scope & caveats" section links here) so a
   throughput/fold/L4 result is never mis-read as a content-sensitivity result.

4. **The honest G2 status follows from this ADR:** generators + a
   size/height-varied load instrument (one real block) exist; content-varied
   prover-serializable blocks are NOT materialized. #144's G2 line is corrected
   to say exactly that and points here.

---

## 6. Cross-references

- **ADR-0004 §6.1** — L4 dominance / the serial bottleneck this instrument
  measures.
- **ADR-0006** — the distributed-prover conductor (the dispatch/gather/fold
  roles this load drives).
- **ADR-0008 §1.4** — the k=1 `bench_test.json` witness addressing (`{height,
  witness_index}`) the cells resolve against.
- **`docs/howto-varied-load-benchmark.md`** — the operational HOW (run it).
- **#144** — North-Star status (G2).
- **#184** — the blocked content-varied / full-witness-synthesis work (the WALL,
  §4).

---

## 7. Citation drift corrected while writing this ADR

Verified against `main` at `394f7a2` (the task brief cited tip `5baf104`, which
is 4 commits behind `main`; the cited behavior is present in both). Line numbers
in the brief had drifted because `main` advanced past `5baf104`; the corrected
citations are:

| Fact | Brief said | Corrected (at `394f7a2`) |
|---|---|---|
| coordinator SPLIT/dispatch | `bench.rs ~1767-1799` | `bench.rs:1957-1989` (SPLIT 1971-72, DISPATCH 1978-89) |
| cell load of local fixture | `bench.rs ~1509-1530` | `bench.rs:1538-1568` (`run_cell` from 1486; load at 1547) |
| slice select | `bench.rs ~1598` | `bench.rs:1706` (`% pool_total`), loop 1698-1729 |
| witness resolver | `bench/src/witness.rs` | `bench/src/conductor/witness.rs:129` |
| `synthesized:false` | "corpus index … all `synthesized:false`" | the flag lives in the **manifest** (`gen_corpus.py:158`); `index.json` carries only `{height, witness_index, tx_count}` |
| per-tx pre-state issue | #180 | code comments attribute the fix to #177; commit `5baf104` subject references #180 |
| committed corpus bands | implied 100/250/400/500 | committed `index.json` caps at 100; bands `{1,25,50,75,100}` |
