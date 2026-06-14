// Copyright (c) Elliot Technologies, Inc.
// SPDX-License-Identifier: BUSL-1.1
//
// Phase-2 Modify (tx_type 17) non-crossing reconstruction tests (#124). These
// lock in the bit-for-bit invariants: the reconstructed Modify after
// order_book_root (via the order-book aggregation tree) reproduces the
// next-same-market tx's before order_book_root, validated against
// bench/bench_test.json ground truth.

package main

import "testing"

// firstRealChainableModify returns a known-good modify tx index for targeted
// tests: the first real, chainable modify in the fixture. Skips the test if the
// fixture has none (defensive; the bundled sample has 167).
func firstRealChainableModify(t *testing.T, b *Block) (i, nxt int) {
	t.Helper()
	for k := range b.Txs {
		tx := &b.Txs[k]
		if tx.TxType != txTypeL2ModifyOrder || !isRealModify(tx) ||
			len(tx.OrderBookPath) != orderBookDepth || len(tx.MarketProof) != marketDepth {
			continue
		}
		n := nextSameMarketTx(b.Txs, k, tx.MarketBefore.Index)
		if n >= 0 {
			return k, n
		}
	}
	t.Skip("no real, chainable modify found in fixture")
	return -1, -1
}

// TestModifyOrderBookBeforeRootMatchesStored proves the order-book aggregation
// fold faithfully reproduces the stored mmb.r before any mutation, on a known
// real modify. Isolates the depth-80 aggregation-tree fold + order leaf hash
// from the state transition.
func TestModifyOrderBookBeforeRootMatchesStored(t *testing.T) {
	b := haveFixture(t)
	i, _ := firstRealChainableModify(t, b)
	tx := &b.Txs[i]
	got := modifyOrderBookBeforeRoot(tx)
	want := toHashOut(tx.MarketBefore.OrderBookRoot)
	if !equalHash(got, want) {
		t.Fatalf("modify order_book before-root tx[%d]: got=%v want(mmb.r)=%v",
			i, limbsOf(got), limbsOf(want))
	}
}

// TestModifyOrderBookAfterRootMatchesNext is the headline Phase-2 invariant on a
// single known modify: reconstruct the AFTER order_book_root (empty loaded leaf +
// get_order_book_path_delta along the aggregation tree) and match the
// next-same-market tx's before order_book_root, bit-for-bit (#124 exit criterion).
func TestModifyOrderBookAfterRootMatchesNext(t *testing.T) {
	b := haveFixture(t)
	i, nxt := firstRealChainableModify(t, b)
	got := modifyOrderBookAfterRoot(&b.Txs[i])
	want := toHashOut(b.Txs[nxt].MarketBefore.OrderBookRoot)
	if !equalHash(got, want) {
		t.Fatalf("modify order_book after-root tx[%d]->tx[%d]: got=%v want=%v",
			i, nxt, limbsOf(got), limbsOf(want))
	}
}

// TestModifyOrderBookPathDeltaConserves verifies the aggregation delta exactly
// removes the loaded order's sums from the top (root-level) aggregation node, for
// a real modify — a non-crypto invariant that the order-book aggregation path
// delta is well-formed (the depth-80 tree's sums are conserved).
func TestModifyOrderBookPathDeltaConserves(t *testing.T) {
	b := haveFixture(t)
	i, _ := firstRealChainableModify(t, b)
	tx := &b.Txs[i]
	ob := tx.OrderInfoBefr
	after := cancelOrderBookPathDelta(tx.OrderBookPath, ob)
	top := orderBookDepth - 1
	if after[top].AskBaseSum != tx.OrderBookPath[top].AskBaseSum-ob.AskBaseSum ||
		after[top].AskQuoteSum != tx.OrderBookPath[top].AskQuoteSum-ob.AskQuoteSum ||
		after[top].BidBaseSum != tx.OrderBookPath[top].BidBaseSum-ob.BidBaseSum ||
		after[top].BidQuoteSum != tx.OrderBookPath[top].BidQuoteSum-ob.BidQuoteSum {
		t.Fatalf("aggregation path delta top not conserved: before=%+v order=%+v after=%+v",
			tx.OrderBookPath[top], ob, after[top])
	}
}

// TestModifyAllRealChainableBitForBit is the strongest Phase-2 invariant: EVERY
// real, chainable modify in the block reproduces the next-same-market tx's before
// order_book_root, bit-for-bit. A single divergence fails the test with the exact
// tx pair and limbs.
func TestModifyAllRealChainableBitForBit(t *testing.T) {
	b := haveFixture(t)
	mv := validateModifies(b, len(b.Txs))
	if mv.chainable == 0 {
		t.Fatal("no real, chainable modifies found to validate")
	}
	if len(mv.divergences) > 0 {
		for _, d := range mv.divergences {
			t.Errorf("divergence tx[%d]->tx[%d] %s: expected=%v got=%v",
				d.tx, d.next, d.field, d.expected, d.got)
		}
		t.FailNow()
	}
	if mv.afterMatched != mv.chainable {
		t.Fatalf("after order_book_root matched %d/%d (expected all)", mv.afterMatched, mv.chainable)
	}
	if mv.beforeMatched != mv.realModifies {
		t.Fatalf("before order_book_root matched %d/%d (expected all real modifies)",
			mv.beforeMatched, mv.realModifies)
	}
	t.Logf("Modify reconstruction: before=%d/%d after=%d/%d (real=%d chainable=%d)",
		mv.beforeMatched, mv.realModifies, mv.afterMatched, mv.chainable,
		mv.realModifies, mv.chainable)
}

// TestModifyPayloadParsed locks the 2mo payload decoding (new price/base/trigger)
// so a future schema drift on the modify payload is caught.
func TestModifyPayloadParsed(t *testing.T) {
	b := haveFixture(t)
	i, _ := firstRealChainableModify(t, b)
	tx := &b.Txs[i]
	if tx.Modify == nil {
		t.Fatalf("tx[%d] modify payload (2mo) did not parse", i)
	}
	if tx.Modify.Price == 0 {
		t.Fatalf("tx[%d] modify new price must be non-zero (l2_modify_order.rs:362)", i)
	}
}
