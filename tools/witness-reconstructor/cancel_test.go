// Copyright (c) Elliot Technologies, Inc.
// SPDX-License-Identifier: BUSL-1.1
//
// Phase-1 Cancel (tx_type 15) reconstruction tests (#123). These lock in the
// bit-for-bit invariants: the reconstructed Cancel after order_book_root (and
// full after market leaf) reproduce the next-same-market tx's before state,
// validated against bench/bench_test.json ground truth.

package main

import "testing"

// TestCancelOrderBookBeforeRootTx67 proves the order-book fold faithfully
// reproduces the stored mmb.r before any mutation, on a known real cancel
// (tx[67], market 0). This isolates the fold/leaf-hash from the state transition.
func TestCancelOrderBookBeforeRootTx67(t *testing.T) {
	b := haveFixture(t)
	tx := &b.Txs[67]
	if !isRealCancel(tx) {
		t.Fatalf("tx[67] expected to be a real cancel; obinfob=%+v", tx.OrderInfoBefr)
	}
	got := cancelOrderBookBeforeRoot(tx)
	want := toHashOut(tx.MarketBefore.OrderBookRoot)
	if !equalHash(got, want) {
		t.Fatalf("order_book before-root tx67: got=%v want(mmb.r)=%v", limbsOf(got), limbsOf(want))
	}
}

// TestCancelOrderBookAfterRootTx67 is the headline Phase-1 invariant on a single
// known cancel: reconstruct the AFTER order_book_root (empty leaf +
// get_order_book_path_delta) and match the next same-market tx's (tx[68])
// before order_book_root, bit-for-bit.
func TestCancelOrderBookAfterRootTx67(t *testing.T) {
	b := haveFixture(t)
	tx := &b.Txs[67]
	got := cancelOrderBookAfterRoot(tx)
	want := toHashOut(b.Txs[68].MarketBefore.OrderBookRoot)
	if !equalHash(got, want) {
		t.Fatalf("order_book after-root tx67->tx68: got=%v want=%v", limbsOf(got), limbsOf(want))
	}
}

// TestCancelEmptyLeafShortcuts locks the empty-leaf semantics the cancel relies
// on: an emptied order leaf and an emptied account_order leaf both hash to ZERO.
func TestCancelEmptyLeafShortcuts(t *testing.T) {
	if !equalHash(orderLeafHash(0, 0, 0, 0), zeroHash()) {
		t.Fatal("emptied order leaf must be HashOut::ZERO")
	}
	if !equalHash(accountOrderLeafHash(AccountOrderLeaf{Index0: 1, Index1: 2, OwnerAccountIndex: 3}), zeroHash()) {
		t.Fatal("emptied account_order leaf must be HashOut::ZERO")
	}
}

// TestCancelOrderBookPathDeltaConserves verifies the aggregation delta exactly
// removes the order's sums from the top (root-level) aggregation node, for a
// real cancel — a non-crypto invariant that the path delta is well-formed.
func TestCancelOrderBookPathDeltaConserves(t *testing.T) {
	b := haveFixture(t)
	tx := &b.Txs[67]
	ob := tx.OrderInfoBefr
	after := cancelOrderBookPathDelta(tx.OrderBookPath, ob)
	top := orderBookDepth - 1
	if after[top].AskBaseSum != tx.OrderBookPath[top].AskBaseSum-ob.AskBaseSum ||
		after[top].AskQuoteSum != tx.OrderBookPath[top].AskQuoteSum-ob.AskQuoteSum ||
		after[top].BidBaseSum != tx.OrderBookPath[top].BidBaseSum-ob.BidBaseSum ||
		after[top].BidQuoteSum != tx.OrderBookPath[top].BidQuoteSum-ob.BidQuoteSum {
		t.Fatalf("path delta top aggregation not conserved: before=%+v order=%+v after=%+v",
			tx.OrderBookPath[top], ob, after[top])
	}
}

// TestCancelAllRealChainableBitForBit is the strongest Phase-1 invariant: EVERY
// real, chainable cancel in the block reproduces the next-same-market tx's
// before order_book_root AND full market leaf, bit-for-bit. A single divergence
// fails the test with the exact tx pair and limbs.
func TestCancelAllRealChainableBitForBit(t *testing.T) {
	b := haveFixture(t)
	cv := validateCancels(b, len(b.Txs))
	if cv.chainable == 0 {
		t.Fatal("no real, chainable cancels found to validate")
	}
	if len(cv.divergences) > 0 {
		for _, d := range cv.divergences {
			t.Errorf("divergence tx[%d]->tx[%d] %s: expected=%v got=%v",
				d.tx, d.next, d.field, d.expected, d.got)
		}
		t.FailNow()
	}
	if cv.afterMatched != cv.chainable {
		t.Fatalf("after order_book_root matched %d/%d (expected all)", cv.afterMatched, cv.chainable)
	}
	if cv.marketLeafMatch != cv.chainable {
		t.Fatalf("after market leaf matched %d/%d (expected all)", cv.marketLeafMatch, cv.chainable)
	}
	if cv.beforeMatched != cv.realCancels {
		t.Fatalf("before order_book_root matched %d/%d (expected all real cancels)", cv.beforeMatched, cv.realCancels)
	}
	t.Logf("Cancel reconstruction: before=%d/%d after=%d/%d marketLeaf=%d/%d (real=%d chainable=%d)",
		cv.beforeMatched, cv.realCancels, cv.afterMatched, cv.chainable,
		cv.marketLeafMatch, cv.chainable, cv.realCancels, cv.chainable)
}
