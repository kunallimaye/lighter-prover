// Copyright (c) Elliot Technologies, Inc.
// SPDX-License-Identifier: BUSL-1.1
//
// Phase-4 empties (tx_type 0) + larger-block generation tests (#126). These lock
// in: (a) the empty-tx no-op invariant — every reconstructed root after an empty
// tx equals the before (stored ground-truth) root, bit-for-bit; and (b) the
// larger-block composition — a varied multi-tx block built from no-engine tx
// types has a bit-for-bit-valid order_book_root state chain.

package main

import "testing"

// TestEmptyTxNoOpAllTreesBitForBit is the headline Phase-4 empties invariant:
// synthesizing an empty tx from EVERY real tx's before-leaves+proofs reproduces
// every (sub)tree root unchanged (after == before), bit-for-bit. A single
// divergence fails with the exact tx + field + limbs.
func TestEmptyTxNoOpAllTreesBitForBit(t *testing.T) {
	b := haveFixture(t)
	ev := validateEmpties(b, len(b.Txs))
	if ev.synthesized == 0 {
		t.Fatal("no empties synthesized (need txs with full before-leaves+proofs)")
	}
	if len(ev.divergences) > 0 {
		for _, d := range ev.divergences {
			t.Errorf("empty-tx divergence tx[%d] %s: expected=%v got=%v",
				d.tx, d.field, d.expected, d.got)
		}
		t.FailNow()
	}
	if ev.apiKeyUnchanged != ev.synthesized {
		t.Fatalf("api_key unchanged %d/%d (expected all)", ev.apiKeyUnchanged, ev.synthesized)
	}
	if ev.accountOrdersUnchg != ev.synthesized {
		t.Fatalf("account_orders unchanged %d/%d (expected all)", ev.accountOrdersUnchg, ev.synthesized)
	}
	if ev.orderBookUnchanged != ev.synthesized {
		t.Fatalf("order_book unchanged %d/%d (expected all)", ev.orderBookUnchanged, ev.synthesized)
	}
	if ev.marketUnchanged != ev.synthesized {
		t.Fatalf("market leaf unchanged %d/%d (expected all)", ev.marketUnchanged, ev.synthesized)
	}
	t.Logf("empty-tx no-op: api_key=%d/%d account_orders=%d/%d order_book=%d/%d market=%d/%d",
		ev.apiKeyUnchanged, ev.synthesized, ev.accountOrdersUnchg, ev.synthesized,
		ev.orderBookUnchanged, ev.synthesized, ev.marketUnchanged, ev.synthesized)
}

// TestEmptyTxOrderBookRootUnchanged is a targeted check on a known real tx: the
// empty-tx order_book_root reconstruction equals the stored mmb.r exactly (the
// no-op leaves the order-book aggregation root untouched).
func TestEmptyTxOrderBookRootUnchanged(t *testing.T) {
	b := haveFixture(t)
	tx := &b.Txs[67] // a known real cancel with a full order-book path
	_, _, ob, _, ok := applyEmptyTx(tx)
	if !ok {
		t.Fatal("applyEmptyTx not ok for tx[67]")
	}
	want := toHashOut(tx.MarketBefore.OrderBookRoot)
	if !equalHash(ob, want) {
		t.Fatalf("empty-tx order_book_root tx[67]: got=%v want(mmb.r)=%v", limbsOf(ob), limbsOf(want))
	}
}

// TestLargerBlockChainValidBitForBit is the headline Phase-4 larger-block
// invariant: the generated varied no-engine block has a bit-for-bit-valid
// order_book_root state chain (every tx's after-root == the next tx's
// before-root), and all real-tx roots are anchored to JSON ground truth.
func TestLargerBlockChainValidBitForBit(t *testing.T) {
	b := haveFixture(t)
	gb := generateVariedBlock(b, 2)
	if len(gb.txs) < 2 {
		t.Fatalf("generated block too small (%d txs); need a no-engine run", len(gb.txs))
	}
	if !gb.chainValid {
		g := gb.txs[gb.firstBreak]
		t.Fatalf("order_book_root chain BROKEN at emitted tx[%d] (%s, src fixture tx[%d]): rootBefore=%v",
			gb.firstBreak, g.kind, g.sourceTx, limbsOf(g.rootBefore))
	}
	// Verify the chain explicitly (defense-in-depth beyond the chainsOK flag).
	for k := 1; k < len(gb.txs); k++ {
		if !equalHash(gb.txs[k].rootBefore, gb.txs[k-1].rootAfter) {
			t.Fatalf("chain break emitted tx[%d]->tx[%d]: before=%v prev.after=%v",
				k-1, k, limbsOf(gb.txs[k].rootBefore), limbsOf(gb.txs[k-1].rootAfter))
		}
	}
	// Empty pads must be true no-ops (after == before).
	for k, g := range gb.txs {
		if g.isNoOp && !equalHash(g.rootAfter, g.rootBefore) {
			t.Fatalf("empty pad emitted tx[%d] is not a no-op: before=%v after=%v",
				k, limbsOf(g.rootBefore), limbsOf(g.rootAfter))
		}
	}
	// All real txs must be anchored to JSON ground truth.
	for k, g := range gb.txs {
		if !g.isNoOp && !g.groundTruth {
			t.Fatalf("real emitted tx[%d] (%s, src fixture tx[%d]) not anchored to ground truth",
				k, g.kind, g.sourceTx)
		}
	}
	t.Logf("generated varied block: market=%d txs=%d (cancels=%d modifies=%d empties=%d) chainValid=%v",
		gb.market, len(gb.txs), gb.nKindsCancel, gb.nKindsModify, gb.nEmpty, gb.chainValid)
}

// TestLargerBlockIsVaried asserts the generated block is genuinely VARIED — it
// composes more than one tx KIND (the #126 "varied data" goal), and is larger
// than a single tx.
func TestLargerBlockIsVaried(t *testing.T) {
	b := haveFixture(t)
	gb := generateVariedBlock(b, 2)
	if len(gb.txs) < 3 {
		t.Fatalf("generated block not larger-than-trivial: %d txs", len(gb.txs))
	}
	kinds := 0
	if gb.nKindsCancel > 0 {
		kinds++
	}
	if gb.nKindsModify > 0 {
		kinds++
	}
	if gb.nEmpty > 0 {
		kinds++
	}
	if kinds < 2 {
		t.Fatalf("generated block not varied: only %d distinct tx kinds (cancels=%d modifies=%d empties=%d)",
			kinds, gb.nKindsCancel, gb.nKindsModify, gb.nEmpty)
	}
}
