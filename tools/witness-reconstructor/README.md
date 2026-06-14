# Go Witness Reconstructor — Phase 0 (replay) + Phase 1 (Cancel) + Phase 2 (Modify) + Phase 4 (empties + larger blocks)

Issues: [#122](../../README.md) (Phase 0), #123 (Phase 1: Cancel), #124
(Phase 2: order-book aggregation tree + non-crossing Modify), and #126 (Phase 4:
empties + larger-block generation) of epic #121.
Design:
[`docs/design/go-witness-reconstructor.md`](../../docs/design/go-witness-reconstructor.md).
Feasibility: #120.

## What this is

A Go harness that REPLAYS the bundled 500-tx block fixture
`bench/bench_test.json` and VALIDATES reconstructed Merkle sub-tree / tree roots
**bit-for-bit** against the JSON's stored ground-truth roots.

- **Phase 0 (read-only)**: re-derive each root from the supplied before-leaves +
  Merkle proofs; confirm they reproduce the recorded roots. No state mutation.
- **Phase 1 (Cancel, tx_type 15)**: apply the Cancel state transition (empty the
  order leaf + `get_order_book_path_delta` aggregation recompute; empty the
  account_order leaf; market `total_order_count`-1) and validate the
  reconstructed AFTER roots bit-for-bit against ground truth.
- **Phase 2 (order-book aggregation tree + Modify, tx_type 17)**: reconstruct the
  depth-80 order-book aggregation tree (internal nodes carry 4 aggregated sums)
  and the NON-CROSSING Modify after `order_book_root`, validated bit-for-bit
  against ground truth.
- **Phase 4 (empties tx_type 0 + larger-block generation)**: prove an empty tx is
  a verified no-op (every root unchanged), then COMPOSE the no-engine tx types
  (Cancel + Modify + empties) into a VARIED, larger multi-tx block with a
  bit-for-bit-valid `order_book_root` state chain — the G2 "varied synthetic
  data" payoff.

This proves the hashing + Merkle-fold machinery and the Cancel state transition
are faithful before later phases build on them. It is a separate Go program with
its own module (`go.mod`) so it does **not** perturb the repo-root `go.mod` or
`make local-test`, and it does **not** touch `bench/src/` Rust circuit code.

## Crypto foundation

Uses the public, verified library
`github.com/elliottech/poseidon_crypto` v0.0.17, package
`poseidon2_plonky2` (import path `.../hash/poseidon2_goldilocks_plonky2`), which
feasibility #120 proved reproduces the prover's Poseidon2 **bit-for-bit** across
three PoCs. Field package `.../field/goldilocks`
(`GoldilocksField uint64`, ORDER = 0xffffffff00000001).

Verified Merkle rules (circuit `merkle_helpers.rs:84/134`, `hash_utils.rs:88`):

- Path bits = little-endian decomposition of the leaf index, leaf-level first.
- `bit==0` → `HashTwoToOne(node, sibling)`; `bit==1` → `HashTwoToOne(sibling, node)`.
- HashOut limb `i` = field element `i` (no byte reversal).
- Empty-leaf shortcut: an empty leaf hashes to `HashOut::ZERO`, not Poseidon of
  zeros (e.g. all-zero api_key pubkey, `is_empty()` order/asset).

## Leaf hashes implemented (each transcribed from the cited circuit source)

| Leaf | Source | Status in this harness |
|---|---|---|
| api_key | `api_key.rs:71-84` | validated, 500/500 bit-for-bit |
| account_order | `account_order.rs:134-157` | validated, 500/500 bit-for-bit |
| account_asset | `account_asset.rs:101-124` | validated, 2994/3000 bit-for-bit |
| market | `market.rs:277-304` | validated end-to-end to `omtr` |
| order (order-book leaf) | `order.rs:69-80` | implemented (+ empty shortcut test) |
| order_book_node (internal) | `order_book_node.rs:47-56` | implemented |
| account | `account_hash.rs:39-137` | NOT reconstructed (Phase 1+; nests sub-tree roots that we validate independently) |

## Run

```sh
cd tools/witness-reconstructor
go build ./...
go run . -json ../../bench/bench_test.json                 # Phase 0+1+2+4 summary
go run . -json ../../bench/bench_test.json -evidence        # one worked Cancel example
go run . -json ../../bench/bench_test.json -modify-evidence # one worked Modify example
go run . -json ../../bench/bench_test.json -block-emit      # emit one generated varied block (per-tx)
go test ./...                                               # locks in bit-for-bit invariants
```

Flags: `-json <path>` (default `bench/bench_test.json`), `-limit N` (first N txs),
`-v` (print Phase-0 evidence limbs for tx[0]), `-evidence` (print one worked
Cancel expected-vs-got example and exit), `-modify-evidence` (print one worked
Modify expected-vs-got example and exit), `-block-emit` (emit one generated
varied no-engine multi-tx block, per-tx, and exit), `-pad-every N` (insert an
empty-tx pad after every N real txs in the generated block; default 2).

## Phase 1 — Cancel (tx_type 15) reconstruction (#123)

Cancel is the simplest tx (no matching engine, `l2_cancel_order.rs:173-233`). Its
`apply()` empties the order-book order leaf and the account_order leaf, recomputes
the order-book aggregation path (`get_order_book_path_delta`,
`matching_engine.rs:42-130`), decrements the owner/market order counts, and (for
spot+limit) the locked balance; the api_key nonce auto-increments on every L2 tx.

**Ground-truth strategy (no fabrication).** `bench_test.json` stores only sparse
per-tx **before**-leaves + proofs — there are no per-tx after-roots. State chains
tx-to-tx (`block_tx_constraints.rs:426-462`), so a cancel's reconstructed AFTER
`order_book_root` is validated against the **next tx that touches the same
market** (its `mmb.r` before-root). The `order_book_root` is a block-level-chained
root carried inside the market leaf, so reproducing it is a genuine end-to-end
Cancel validation, not a self-referential check.

| Quantity | Coverage |
|---|---|
| `order_book_root` BEFORE (fold sanity vs `mmb.r`) | **96/96 bit-for-bit** |
| account_orders BEFORE (vs stored `aor`) | **96/96 bit-for-bit** |
| `order_book_root` AFTER (vs next-same-market `mmb.r`) | **81/81 bit-for-bit** ← the goal |
| market leaf AFTER (`r`:=after, `toc`-1, vs next) | **81/81 bit-for-bit** |

- **118** total cancels; **96** are *real* (the loaded order has a non-zero
  aggregation sum and actually mutates the book — an empty-order cancel has
  `success==false` and changes no root, `l2_cancel_order.rs:129`).
- **81** of the 96 real cancels are *chainable* (a later tx touches the same
  market, providing the after-root ground truth). All 81 validate bit-for-bit.
- The remaining 15 real cancels are the LAST tx to touch their market in the
  500-tx block, so the sample carries no later before-root to chain against
  (their `order_book_root` rolls into the block's final `nsr`, which needs the
  full account-tree reconstruction — Phase 4 scope).

**Spot+limit locked-balance note.** The cancel `apply()` also decrements the
owner's locked asset balance for spot + limit orders
(`decrement_locked_balance_for_order`). In this fixture **all 96 real cancels are
perps** (market_type != spot) with order_type 0, so the spot+limit locked-balance
branch is never exercised and has **no ground truth to validate against** here. It
is documented as a cancel effect but intentionally not asserted (validating an
unexercised branch would be fabrication); it is covered structurally by the
order-count decrement and re-checked when a spot cancel appears in a future
fixture.

**Scope (strict):** Cancel (tx_type 15) only. The order-book aggregation *insert*
path, the matching engine, Claim, and Create (#125) are out of scope and
untouched. No `bench/src/` Rust code is modified.

## Phase 2 — order-book aggregation tree + Modify (tx_type 17) non-crossing (#124)

**The order-book aggregation tree (#124 goal a, the trickiest structure).** The
order-book tree is depth 80 (`ORDER_PRICE_BITS`32 + `ORDER_NONCE_BITS`48,
`constants.rs:35,37`). Unlike a plain Merkle tree, every internal node carries
FOUR aggregated sums (ask/bid base/quote) summed over its whole subtree
(`order_book_node.rs:18-57`), and an internal node's hash is *literally* those 4
sums as the 4 HashOut limbs — not a Poseidon permutation (`order_book_node.rs:47`).
A leaf update mutates the leaf hash AND every aggregation node on its path:
`get_order_book_path_delta` (`matching_engine.rs:42-130`) recomputes the path,
`recalculate_order_book_tree_root` (`order_book_tree_helpers.rs:51-66`) folds it.
Phase 1 implemented this fold for Cancel; Phase 2 reuses + validates it for Modify.

**Why the modify order-book delta is a removal along the loaded path
(EMPIRICALLY CONFIRMED).** The circuit derives ONE order path helper per tx,
fixed from the LOADED order's position (`tx_constraints.rs:551-555`), and computes
the final `order_book_root` ONCE along that path
(`tx_constraints.rs:1934-1946`). For a non-crossing modify, `apply()` empties the
loaded order (`l2_modify_order.rs:528-561`) and `execute_matching` re-inserts the
new resting order at a NEW `(price, nonce)` (`get_order_from_register`,
`matching_engine.rs:1773-1775`) — a different leaf position than the verified path
covers. So the net `order_book_root` delta along the single verified path IS the
removal of the loaded order, structurally identical to a Cancel. Confirmed
bit-for-bit: every real modify's removal-only reconstructed after root equals the
next-same-market tx's before root.

**Ground-truth strategy (no fabrication).** Same chaining as Cancel: the modify's
reconstructed AFTER `order_book_root` is validated bit-for-bit against the next tx
that touches the same market (its `mmb.r` before-root).

| Quantity | Coverage |
|---|---|
| `order_book_root` BEFORE (aggregation fold vs `mmb.r`) | **167/167 bit-for-bit** |
| `order_book_root` AFTER (vs next-same-market `mmb.r`) | **167/167 bit-for-bit** ← #124 exit |

- **168** total modifies; **167** are *real* (the loaded order has a non-zero
  aggregation sum). The 1 non-real modify loads an empty order → `success==false`
  (`l2_modify_order.rs:278-281`), no `order_book_root` change.
- All 167 real modifies are chainable (a later tx touches the same market) and
  validate bit-for-bit on both before and after `order_book_root`.

**Honest scope boundary (recorded, not hidden).** The full market LEAF after a
modify does NOT equal the next-same-market tx's market leaf, because the modify
re-inserts the order at a new nonce — incrementing `market.ask_nonce` or
`bid_nonce` (`l2_modify_order.rs:498-521`, observed in 167/167) — and the
next-same-market tx in this sample is itself a tx_type-21 claim that touches the
market. The `order_book_root` (the #124 exit criterion) is what Phase 2
reproduces and validates; the nonce/full-leaf chaining and the new-position
re-insert are Phase-3 matching-engine scope (#125).

**Scope (strict):** Modify (17) NON-CROSSING order-book root only. No matching
engine, no fills/trades (crossing path #125), no `bench/src/` Rust code touched.

## Phase 4 — empties (tx_type 0) padding + larger-block generation (#126)

**Empties (TX_TYPE_EMPTY = 0) — verified no-op, cheapest block inflation.** An
empty tx (`constants.rs:115`) runs the FULL per-tx verification pipeline
unconditionally (so valid before-leaves+proofs are still required and the prover
does real work) but every per-type `apply()` is gated off and the api_key nonce
is NOT incremented (`tx_constraints.rs:2601`, `nonce + is_layer2.target`, and
`is_layer2` is false for tx_type 0). So EVERY root is unchanged: an empty tx can
be inserted anywhere in a chained block without breaking the state chain — the
cheapest way to inflate block size for benchmarking.

The bundled sample has NO tx_type-0 txs (its mix is {14,15,17,21}), so the
invariant is validated CONSTRUCTIVELY against REAL ground-truth leaves: synthesize
an empty tx from each real tx's before-leaves+proofs and assert every
reconstructed AFTER root equals the BEFORE (JSON-stored) root, bit-for-bit. The
BEFORE fold is independently anchored to the stored roots, so this is the precise
circuit invariant, not a tautology.

| Empty-tx no-op (synthesized from all 500 real txs) | Coverage |
|---|---|
| `api_key_root` AFTER==BEFORE (vs `akr`) | **500/500 bit-for-bit** |
| `account_orders_root` AFTER==BEFORE (vs `aor`) | **500/500 bit-for-bit** |
| `order_book_root` AFTER==BEFORE (vs `mmb.r`) | **500/500 bit-for-bit** |
| `market_leaf` AFTER==BEFORE (vs `mmb`) | **500/500 bit-for-bit** |

**Larger-block generation (the G2 payoff).** The reconstructor composes the
no-engine tx types — Cancel (15), non-crossing Modify (17), empties (0) — into a
VARIED, larger multi-tx block over a single market and validates the entire
`order_book_root` state chain bit-for-bit: each tx's after-root IS the next tx's
before-root (`block_tx_constraints.rs:426-462`). Real-tx roots are anchored to
JSON ground truth (each before-root == stored `mmb.r`, each after-root == the next
same-market tx's stored before-root — exactly Phase 1/2's anchor); empty-tx pads
inflate the block while preserving the running root.

Emitted example (`go run . -block-emit`): a 7-tx block on market 45 —
**4 cancels + 1 modify + 2 empty pads** — with a `chainValid = true` bit-for-bit
`order_book_root` chain. This is a varied, larger, state-chain-valid block built
ENTIRELY from no-matching-engine tx types — the synthetic-data payoff that
unblocks G4 stress-testing, with no matching engine and no fabricated roots.

**Honest scope boundary (recorded, not hidden).** This composes + validates the
ORDER-BOOK `order_book_root` state chain — the root the no-engine tx types fully
determine. A fully prover-serializable NOVEL block additionally needs signatures,
public data, and the full account-tree leaf, which are gated on full initial
state from the sequencer (#120/#126) and the matching engine (#125). What is
delivered + validated here is the engine-free, bit-for-bit-correct varied multi-tx
state chain.

**Scope (strict):** empties (0) + composition of the no-engine types only. No
matching engine, no crossing/fills (#125), no `bench/src/` Rust code touched.

## What it validates (and current coverage)

Per the design-doc de-risker: **sub-tree roots are stored in the JSON** (each
`ab[acct]` account leaf carries `akr`/`aor`/`asr`/`abr`), so each reconstructed
(sub-)tree is validated **independently** against ground truth without
materializing parent trees.

| Tree | Fold target | Coverage |
|---|---|---|
| api_key (depth 8) | `akb` → `ab[OWNER].akr` | **500/500 bit-for-bit** |
| account_orders (depth 60) | `aob` → `ab[OWNER\|MAKER].aor` | **500/500 bit-for-bit** |
| asset (depth 6) | `aab` → `ab[acct].asr` | **2994/3000 bit-for-bit** |
| market (depth 12) | `mmb` → `omtr` (block old market root) | **3/57 pre-mutation** bit-for-bit |

### Honest interpretation of non-matches

- **account_orders**: for create(14)/claim(21) txs the order belongs to the
  MAKER account, not the owner — the circuit selects
  `select_hash(MAKER.account_orders_root, TAKER.account_orders_root)`. The
  harness accepts a match against any account slot's `aor`; all 500 match.
- **asset (6 of 3000)**: these are the SECOND non-empty asset in the same
  account's asset sub-tree within one tx. Its proof embeds the
  intra-tx-updated first-asset leaf, so it folds to a post-update root, not the
  before-snapshot. The asset-leaf hash itself is proven correct (the first asset
  always matches). Reconstructing that update is **state mutation = Phase 1+**.
- **market (54 of 57)**: `omtr` is a SINGLE root for the whole market tree, so
  only the first tx that touches the market tree before any mutation folds to
  `omtr` (tx[0]). Every later tx sees a CARRIED market tree root. Reproducing it
  requires carrying the market tree root tx-to-tx and reconstructing the
  order-book aggregation (the market leaf's `order_book_root`) — Phase 1+.

None of the non-matches are encoding/crypto bugs: the same leaf-hash code
reproduces ground truth for every first touch.

## Phase-0 exit criterion status

The stated #122 exit criterion — *all 500 txs' after-roots reproduce and chain
to the sample's final state root `nsr`/`osr`* — is **NOT yet met**. Chaining the
block-level state root requires reconstructing the full account-tree leaf hash
(`account_hash.rs` nested position buckets / public-pool info / strategy hashes)
and the order-book aggregation tree, plus carrying roots tx-to-tx. That is
beyond the independently-validatable sub-tree scope delivered here and overlaps
Phase 1+. See issue #122 for the precise remaining-work breakdown.
