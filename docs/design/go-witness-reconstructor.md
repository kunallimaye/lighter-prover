# Design: Go-Native Witness Reconstructor

**Status**: Proposed
**Date**: 2026-06-13
**Issues**: #120 (feasibility — this doc is its design-doc task); #121 (epic) with phases #122–#126

## Goal & scope

- **Goal**: generate **valid** block witnesses with **varied** data so the prover
  can be driven beyond the single bundled `bench/bench_test.json`, primarily for
  **throughput benchmarking**.
- The prover is a **strict verifier**: plonky2 panics during witness generation on
  any unsatisfied constraint (`bench/src/bin/bench.rs:664-666`) and
  `BlockTxCircuit::prove()` calls `circuit.verify()`
  (`circuit/src/block_tx_constraints.rs:176`). A witness must therefore be
  cryptographically self-consistent — synthetic/garbage data crashes. The
  reconstructor must produce correct roots, Merkle proofs, before-leaves, and
  deltas.
- **Non-goals**: re-implementing the closed-source sequencer's business policy
  beyond what the circuit enforces; full coverage of all ~41 tx types in v1.

## Validated foundation (three PoCs)

The public Go library `github.com/elliottech/poseidon_crypto` v0.0.17
(import `.../hash/poseidon2_goldilocks_plonky2`, package name `poseidon2_plonky2`;
field pkg `.../field/goldilocks`) is a **bit-for-bit drop-in** for the prover's
Poseidon2, validated by:

1. **Single leaf-vector hash** (`all_assets_hash`, 434 elements) — exact match.
2. **Full block state root `osr`** end-to-end (sponge + compress + multi-input fold
   across assets / market-details / public-market-details / state-metadata /
   system-config) — exact match.
3. **Thin-slice Merkle loop**: api_key leaf hash → nonce+1 state transition → proof
   verify against `ab[0].akr` → new-root recalc — exact match on first try.

APIs used: `HashNoPad([]GoldilocksField) HashOut`, `HashTwoToOne(a,b)`,
`HashNToOne([]HashOut)`, `EmptyHashOut()`; `HashOut = [4]GoldilocksField`;
construct via `GoldilocksField(uint64(v))`, read via `.ToCanonicalUint64()`; for
negatives `NonCannonicalGoldilocksField(int64)`.

**Conclusion**: the cryptography is solved; remaining work is plumbing (model trees
+ per-tx-type state transitions), not crypto.

## Tree model (the core data structures)

Depths from `circuit/src/types/constants.rs:407-414`: ACCOUNT=48, API_KEY=8,
ACCOUNT_ORDERS=60, POSITION=8, MARKET=12, ASSET=6, ORDER_BOOK=80
(= ORDER_PRICE_BITS 32 + ORDER_NONCE_BITS 48).

```
ACCOUNT TREE (depth 48) — root old_account_tree_root ("oatr")
  leaf = Account (types/account/account.rs:38-113; hash account_hash.rs:39-137)
    ├─ api_key_root             → API_KEY SUB-TREE (depth 8)
    │                              leaf=ApiKey (api_key.rs:29-42; hash :71-84)
    ├─ account_orders_root      → ACCOUNT_ORDERS SUB-TREE (depth 60)
    │                              leaf=AccountOrder (account_order.rs:22-80; hash :134-157)
    ├─ asset_root               → ASSET SUB-TREE (depth 6)
    │                              leaf=AccountAsset (account_asset.rs; hash :101)
    ├─ aggregated_balances_root → (depth-6 asset tree; balance leaves account_hash.rs:22-37)
    └─ positions[255]           → 16 buckets hashed into account leaf
                                   (POSITION_HASH_BUCKET_COUNT=16, constants.rs:314)

ACCOUNT-PUB-DATA TREE (depth 48) — root "oapt" (pub-data subset; account_hash.rs:63-92)

ACCOUNT-DELTA TREE (depth 48) — root "oapdtr"  leaf=AccountDelta
    ├─ asset_delta_root         → asset-delta sub-tree (depth 6)
    └─ position_delta_root      → position-delta sub-tree (depth 8; position_delta.rs:89)

MARKET TREE (depth 12) — root "omtr"  leaf=Market (market.rs:24-79; hash :277)
    └─ order_book_root          → ORDER-BOOK TREE (depth 80)
                                   leaf=Order (order.rs:23-41; hash :69-80)
                                   internal nodes = OrderBookNode (aggregated sums;
                                   order_book_node.rs:18-57)

all_assets[64] and all_market_details[255] are FLAT ARRAYS carried in public
inputs (NOT trees): ASSET_LIST_SIZE=64, POSITION_LIST_SIZE=255.
```

**Validated rules:**
- Merkle path = **little-endian** bit-decomposition of the index, **leaf-level
  first** (`merkle_helpers.rs:11-81`); fold **from leaf upward**.
- Child order: `bit==0` → `HashTwoToOne(node, sibling)`; `bit==1` →
  `HashTwoToOne(sibling, node)`.
- HashOut limb `i` = field element `i` (no byte reversal).
- Sub-tree roots are recomputed **bottom-up**, then embedded into the parent leaf
  (Account / Market), then the parent root is recomputed.
- The **order-book tree is special**: internal nodes carry aggregated
  ask/bid base/quote sums (`order_book_tree_helpers.rs`,
  `matching_engine.rs:42-130`), making it the hardest tree.

## Per-tx pipeline (fixed sequence)

There is **no switch**; every tx runs the full pipeline and selects effects via
boolean `is_<type>` flags (`tx_constraints.rs:471-724`; verify dispatch
`:2313-2411`; apply dispatch `:2528-2602`). The leaf verify/recalc order
(`tx_constraints.rs:668-693`) is:

position-delta → api_key → account_orders → assets (asset / pub-balance /
asset-delta) → account + pub-data + account-delta (taker/maker/fee) →
market + order-book.

The api_key nonce auto-increments for every L2 tx (`tx_constraints.rs:2601`).
Per-tx slot model: `NB_ACCOUNTS_PER_TX=3` (taker/owner=0, maker=1, fee=2),
`NB_ASSETS_PER_TX=2`.

## The four dominant tx types

Sample distribution: 15=118 cancel, 17=168 modify, 21=169 claim, 14=45 create.

| Type | Name | JSON key | Matching engine? | Register stack? | Mutates | Complexity |
|---|---|---|---|---|---|---|
| 15 | L2_CANCEL_ORDER | `2co` | No | pushes child-cancels only | account_orders (remove), order_book (remove leaf + aggregation), owner account (counts, nonce++, locked balance for spot), market (order_book_root only) | **Easiest** |
| 17 | L2_MODIFY_ORDER | `2mo` | Yes | Yes (INSERT_ORDER) | remove old order + re-insert via engine; owner account, market (nonces + order_book_root), account_orders, plus engine counterparty | Medium-hard |
| 21 | INTERNAL_CLAIM_ORDER | `Ic` | Yes (entirely) | Consumes register | trivial file (apply just sets matching_engine_flag, `:83-91`); all work is in the engine + cross-tx register stack | Medium |
| 14 | L2_CREATE_ORDER | `2cr` | Yes | Yes | full order validation + cloid non-membership Merkle proof + engine; account_orders (new), order_book, owner account, market (nonces + order_book_root), engine counterparty | **Hardest** |

Files: `circuit/src/transactions/l2_cancel_order.rs` (verify :90-170, apply :173-233);
`l2_modify_order.rs` (verify :229-489, apply :491-605); `internal_claim_order.rs`;
`l2_create_order.rs` (verify :251-601, apply :604-660).

**Note**: complexity ranking is 15 < {17, 21} < 14. Claim (21) is deceptively
trivial in its own file but needs the full matching engine + the register stack
carried across txs, so it is **not** simpler than modify to reconstruct.

## The matching engine

`circuit/src/matching_engine.rs` (~2200 lines) + `apply_trade.rs`. Runs once per tx
(`tx_constraints.rs:624`). Produces: order-book aggregation node updates
(`get_order_book_path_delta:42-130`), counterparty maker account
(position/collateral/balances/order count/locked balance), fee account
(taker_fee/maker_fee validated against market fees), market open interest, impact
orders. **Required by** create/modify/claim; **not** by cancel/empties. It is the
single biggest piece — comparable in size to all other tx logic combined: position
math, funding, fees, risk/health, spot vs perps paths, register-stack state
machine, order-book aggregation.

## State carry & the sparse-data constraint (CRITICAL)

- State chains tx-to-tx: each tx's after-roots / arrays / register_stack /
  system_config become the next tx's before- (`block_tx_constraints.rs:426-462`);
  same at chunk boundaries (`bench/src/bin/bench.rs:644-697`). Model: process txs
  sequentially over **live in-memory trees**, snapshotting before-leaves + proofs
  per step. The register stack carries across txs (create/modify push INSERT_ORDER;
  claim consumes).
- **CRITICAL DATA FINDING**: `bench_test.json` contains only **sparse** per-tx data
  — the before-leaves + Merkle proofs for the ≤3 accounts / 1 order / 1 market each
  tx touches — plus the block's 4 old roots and the **full** flat arrays
  `all_assets[64]` / `all_market_details[255]`. It does **not** contain full
  account/order/order-book tree contents. Implication:
  - The sample is sufficient to **replay/validate** the existing 500-tx block
    (every needed before-leaf + proof is present and they chain) — this is the
    Phase 0 validation harness.
  - Generating a **novel** block requires the **full initial state** (all touched
    leaves + enough to build proofs) from **outside** the sample — i.e. from the
    sequencer/API. This is the main external dependency and a key risk.

## Inputs vs computed outputs

- **Inputs (novel block)**: raw signed txs (per-type payloads + nonce/expiry/sig/
  fees); full initial state (account / api_key / account_orders / asset /
  order_book / delta tree contents for touched paths); the 4 old roots;
  `all_assets[64]`; `all_market_details[255]`; register_stack; system_config;
  created_at; chain_id; block fee params; impact-order data.
- **Outputs (the witness)**: per-tx before-leaves (accounts / api_key /
  account_order / market / order / account_assets); all Merkle proofs
  (`mpab, mpapd, mpapdd, mpaab, mpaa, mpad, mpppdd, mpakb, mpokb, mpmmb, obpb`);
  asset_indices; impact orders + proofs; taker/maker fees; block-level after-roots
  (chained); updated flat arrays; register_stack; on-chain/priority pub data;
  delta-tree contents.

## Phased build plan

1. **Phase 0 — Replay/validation harness** (#122). Parse `bench_test.json`;
   implement the 7 leaf hashes (`account_hash.rs`, `order.rs:69`,
   `account_order.rs:134`, `api_key.rs:71`, `market.rs:277`, `account_asset.rs:101`,
   `order_book_node.rs:47`) + Merkle verify/recalc (`merkle_helpers.rs`). Re-derive
   each tx's after-roots from supplied before-leaves + proofs and confirm they
   chain. No state-mutation logic.
   *Exit*: all 500 txs' roots reproduce and chain to the sample's final state root.
2. **Phase 1 — Cancel (15)** end-to-end, no matching engine (#123):
   account_orders removal, order_book removal + aggregation recompute
   (`get_order_book_path_delta`), order-count/locked-balance, api_key nonce++.
   *Exit*: reproduce the sample's after-roots for cancel txs.
3. **Phase 2 — Order-book aggregation tree (full) + Modify (17)** non-crossing path
   (#124): post-only / non-crossing, no fills.
   *Exit*: reproduce modify after-roots for non-crossing modifies.
4. **Phase 3 — Matching engine** (`matching_engine.rs` + `apply_trade.rs`) +
   **Claim (21)** + **Create (14)** (#125): fills/trades, register-stack state
   machine, full create validation.
   *Exit*: reproduce after-roots for trading txs incl. counterparty maker/fee
   deltas, covering all four dominant tx types.
5. **Phase 4 — Empties (type 0) padding + remaining tx types + larger-block
   generation** (#126).
   *Exit*: the prover proves a generated block larger than / different from the
   bundled sample without any constraint panic.

**Note**: for the throughput-benchmarking goal, Phases 0–1 plus empties (a slice of
Phase 4) may already unlock varied/larger valid blocks; full matching-engine
fidelity (Phase 3) is only needed for realistic mixed-trade blocks.

## Risks & open questions

- **Full initial state for novel blocks is not in the sample** — depends on
  sequencer/API access. The main REST API is US-geo-blocked (prior findings); the
  SDKs/explorer give raw txs + public state but not tree contents.
- **Matching-engine fidelity** is the largest effort and the highest correctness
  risk.
- **Order-book aggregation tree** (depth 80, summed internal nodes) is the
  trickiest structure.
- **Encoding landmines already found**: the `ia`/`ib` impact-price name swap
  (`market_details.rs:59-64`); signed vs unsigned field encodings
  (`from_noncanonical_i64` vs `from_canonical_i64`); `qm`/`f` belong to the
  public-md hash, not the md hash; empty-leaf shortcut (all-zero pubkey → zero
  hash, not Poseidon of zeros).
- **Validation strategy**: every phase validates by reproducing the sample's
  ground-truth roots before attempting novel data.

## References

- Tracking issue #120; epic #121; phases #122–#126.
- Native reference: `bench/src/seed.rs`.
- Public Poseidon2 lib: <https://github.com/elliottech/poseidon_crypto>
- Pipeline: `circuit/src/tx_constraints.rs:471-724`; chaining
  `circuit/src/block_tx_constraints.rs:426-462`; chunks
  `bench/src/bin/bench.rs:644-697`.
