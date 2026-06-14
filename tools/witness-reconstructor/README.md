# Go Witness Reconstructor — Phase 0 (replay harness) + Phase 1 (Cancel)

Issues: [#122](../../README.md) (Phase 0) and #123 (Phase 1: Cancel) of epic
#121. Design:
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
go run . -json ../../bench/bench_test.json          # Phase 0 + Phase 1 summary
go run . -json ../../bench/bench_test.json -evidence # one worked Cancel example
go test ./...                                        # locks in bit-for-bit invariants
```

Flags: `-json <path>` (default `bench/bench_test.json`), `-limit N` (first N txs),
`-v` (print Phase-0 evidence limbs for tx[0]), `-evidence` (print one worked
Cancel expected-vs-got example and exit).

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

**Scope (strict):** Cancel (tx_type 15) only. Modify (#124), the order-book
aggregation *insert* path, the matching engine, Claim, and Create (#125) are out
of scope and untouched. No `bench/src/` Rust code is modified.

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
