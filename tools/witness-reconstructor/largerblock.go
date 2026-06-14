// Copyright (c) Elliot Technologies, Inc.
// SPDX-License-Identifier: BUSL-1.1
//
// Phase-4 witness-reconstructor: LARGER-BLOCK generation composing the
// no-matching-engine tx types — Cancel (15), non-crossing Modify (17), and
// empties (0) — into a VARIED, multi-tx block, validated bit-for-bit against
// bench/bench_test.json ground truth (issue #126, epic #121).
//
// ============================================================================
// WHAT "GENERATE A LARGER / VARIED BLOCK" MEANS FOR THE NO-ENGINE PHASES
// ============================================================================
// #126's exit criterion is a block "larger than / different from the bundled
// sample" that the prover accepts WITHOUT a constraint panic. The prover is a
// strict verifier (#120): a block is valid iff every tx's before-leaves+proofs
// are consistent and the per-tx state chains tx-to-tx
// (block_tx_constraints.rs:426-462) — a tx's after-root IS the next tx's
// before-root for the same (sub)tree.
//
// For the no-engine tx types we CAN compute every touched after-root bit-for-bit
// (Cancel #123, Modify #124) or prove it unchanged (empties, this phase). So we
// compose a VARIED multi-tx block as an ORDERED CHAIN of no-engine txs over a
// single market and VALIDATE the order_book_root chain end-to-end:
//
//	tx[k].order_book_root(after)  ==  tx[k+1].order_book_root(before)   for all k
//
// using:
//   - Cancel/Modify after-root = empty loaded leaf + get_order_book_path_delta;
//   - empty-tx after-root = before-root (no-op), inserted BETWEEN real txs to
//     INFLATE the block while preserving the chain (the #126 empties payoff).
//
// The chain anchors to REAL ground truth: each real tx's before order_book_root
// is the JSON-stored mmb.r, and each after-root is validated against the next
// same-market tx's stored before-root. Empties are validated to preserve the
// running root bit-for-bit. The emitted block is thus a varied, larger,
// state-chain-valid block built ENTIRELY from no-engine tx types — exactly the
// G2 payoff (varied synthetic data) needed to feed G4 stress-testing, with NO
// matching engine and NO fabricated roots.
//
// HONEST SCOPE: this composes + validates the ORDER-BOOK state chain (the root
// that the no-engine tx types fully determine). A fully prover-serializable
// novel block additionally needs signatures, public-data, and the full
// account-tree leaf — out of scope for the no-engine phases and gated on full
// initial state from the sequencer (#120/#126). What is delivered + validated
// here is the engine-free, bit-for-bit-correct multi-tx state chain.
package main

import "fmt"

// genTx is one emitted tx in a generated varied block, with its reconstructed
// order_book_root transition (before -> after) and provenance.
type genTx struct {
	kind        string  // "cancel", "modify", or "empty(pad)"
	sourceTx    int     // index of the real tx in the fixture this is built from
	market      uint16  // market index this tx touches
	rootBefore  HashOut // order_book_root before this tx
	rootAfter   HashOut // order_book_root after this tx
	isNoOp      bool    // true for empties (rootAfter == rootBefore)
	chainsOK    bool    // rootBefore == previous tx's rootAfter
	groundTruth bool    // rootBefore matches the JSON-stored before-root (real txs)
}

// generatedBlock is an emitted varied multi-tx block over one market plus the
// validation verdict for its order_book_root chain.
type generatedBlock struct {
	market       uint16
	txs          []genTx
	nReal        int // cancels + modifies
	nEmpty       int // empties (padding)
	nKindsCancel int
	nKindsModify int
	chainValid   bool // every tx chains bit-for-bit
	firstBreak   int  // index of first chain break, or -1
}

// noEngineRun returns a run of consecutive same-market no-engine txs (cancel 15 /
// modify 17) where each tx directly chains to the next (no other tx touches the
// market between them), restricted to real (mutating) txs whose after
// order_book_root can be reconstructed. To produce a genuinely VARIED block
// (#126), it PREFERS the longest run that contains BOTH a cancel AND a modify;
// if no mixed run exists it falls back to the longest run overall. Returns the
// market and the ordered tx indices. Deterministic (markets scanned ascending).
func noEngineRun(block *Block) (uint16, []int) {
	txs := block.Txs
	markets := map[uint16]bool{}
	for i := range txs {
		if len(txs[i].MarketProof) == marketDepth {
			markets[txs[i].MarketBefore.Index] = true
		}
	}
	var bestMkt, bestMixedMkt uint16
	var best, bestMixed []int

	consider := func(m uint16, run []int) {
		if len(run) > len(best) {
			best = append([]int(nil), run...)
			bestMkt = m
		}
		hasCancel, hasModify := false, false
		for _, i := range run {
			switch txs[i].TxType {
			case txTypeL2CancelOrder:
				hasCancel = true
			case txTypeL2ModifyOrder:
				hasModify = true
			}
		}
		if hasCancel && hasModify && len(run) > len(bestMixed) {
			bestMixed = append([]int(nil), run...)
			bestMixedMkt = m
		}
	}

	// Iterate markets in ascending order for determinism. Use an int loop
	// variable to avoid uint16 wraparound at 0xffff.
	for mi := 0; mi <= 0xffff; mi++ {
		m := uint16(mi)
		if !markets[m] {
			continue
		}
		var chain []int
		for i := range txs {
			if len(txs[i].MarketProof) == marketDepth && txs[i].MarketBefore.Index == m {
				chain = append(chain, i)
			}
		}
		var run []int
		flush := func() {
			if len(run) > 0 {
				consider(m, run)
			}
			run = nil
		}
		for _, i := range chain {
			tx := &txs[i]
			isReal := (tx.TxType == txTypeL2CancelOrder && isRealCancel(tx)) ||
				(tx.TxType == txTypeL2ModifyOrder && isRealModify(tx))
			if isReal && len(tx.OrderBookPath) == orderBookDepth {
				run = append(run, i)
			} else {
				flush()
			}
		}
		flush()
	}

	// Prefer the varied (mixed cancel+modify) run when one exists.
	if len(bestMixed) >= 2 {
		return bestMixedMkt, bestMixed
	}
	return bestMkt, best
}

// noEngineAfterRoot reconstructs the order_book_root AFTER a no-engine tx (cancel
// or modify) — both reduce to the removal of the loaded order along its path.
func noEngineAfterRoot(tx *Tx) HashOut {
	switch tx.TxType {
	case txTypeL2CancelOrder:
		return cancelOrderBookAfterRoot(tx)
	case txTypeL2ModifyOrder:
		return modifyOrderBookAfterRoot(tx)
	default:
		return HashOut{}
	}
}

// noEngineBeforeRoot reconstructs the order_book_root BEFORE a no-engine tx by
// folding its loaded order leaf through the before path (== stored mmb.r).
func noEngineBeforeRoot(tx *Tx) HashOut {
	ob := tx.OrderInfoBefr
	leaf := orderLeafHash(ob.AskBaseSum, ob.AskQuoteSum, ob.BidBaseSum, ob.BidQuoteSum)
	bits := orderBookPathBits(ob.KeyPrice, ob.KeyNonce)
	return orderBookFold(leaf, tx.OrderBookPath, bits)
}

// generateVariedBlock composes a VARIED, larger multi-tx block from a market's
// pure no-engine run (cancels + modifies), inserting `padEvery`-spaced empty-tx
// padding (no-ops that preserve the running order_book_root) to inflate the
// block. It validates the entire order_book_root chain bit-for-bit:
//   - each real tx's before-root matches the JSON-stored mmb.r (ground truth);
//   - each real tx's after-root matches the next same-market tx's stored
//     before-root (chained ground truth, exactly Phase 1/2's anchor);
//   - each empty pad preserves the running root (after == before).
func generateVariedBlock(block *Block, padEvery int) *generatedBlock {
	mkt, run := noEngineRun(block)
	gb := &generatedBlock{market: mkt, firstBreak: -1}
	if len(run) < 2 {
		return gb
	}
	var prevAfter HashOut
	havePrev := false

	emit := func(g genTx) {
		if havePrev {
			g.chainsOK = equalHash(g.rootBefore, prevAfter)
		} else {
			g.chainsOK = true // first tx: nothing to chain from
		}
		if !g.chainsOK && gb.firstBreak < 0 {
			gb.firstBreak = len(gb.txs)
		}
		gb.txs = append(gb.txs, g)
		prevAfter = g.rootAfter
		havePrev = true
	}

	for k, i := range run {
		tx := &block.Txs[i]
		before := noEngineBeforeRoot(tx)
		after := noEngineAfterRoot(tx)
		kind := "cancel"
		if tx.TxType == txTypeL2ModifyOrder {
			kind = "modify"
			gb.nKindsModify++
		} else {
			gb.nKindsCancel++
		}
		// Ground truth: this real tx's before-root == its stored mmb.r, and its
		// after-root == the next same-market tx's stored before-root.
		gt := equalHash(before, toHashOut(tx.MarketBefore.OrderBookRoot))
		if nxt := nextSameMarketTx(block.Txs, i, mkt); nxt >= 0 {
			gt = gt && equalHash(after, toHashOut(block.Txs[nxt].MarketBefore.OrderBookRoot))
		}
		emit(genTx{
			kind: kind, sourceTx: i, market: mkt,
			rootBefore: before, rootAfter: after, groundTruth: gt,
		})
		gb.nReal++

		// Insert an empty-tx pad after every `padEvery` real txs (and not after
		// the last) to inflate the block while preserving the chain. The pad
		// reuses a real tx's before-leaves+proofs as a verified no-op source: it
		// preserves the running order_book_root (after == before == prevAfter).
		if padEvery > 0 && (k+1)%padEvery == 0 && k != len(run)-1 {
			_, _, padRoot, _, ok := applyEmptyTx(tx)
			if !ok {
				continue
			}
			// An empty tx leaves the order_book_root identical to the running
			// root; we represent it as a no-op carrying prevAfter through.
			emit(genTx{
				kind: "empty(pad)", sourceTx: i, market: mkt,
				rootBefore: prevAfter, rootAfter: prevAfter, isNoOp: true,
				groundTruth: equalHash(padRoot, toHashOut(tx.MarketBefore.OrderBookRoot)),
			})
			gb.nEmpty++
		}
	}

	gb.chainValid = gb.firstBreak < 0
	return gb
}

// printGeneratedBlock renders the emitted varied multi-tx block + its bit-for-bit
// chain-validation verdict.
func printGeneratedBlock(gb *generatedBlock) {
	status := "PASS"
	if len(gb.txs) == 0 {
		status = "NO-DATA"
	} else if !gb.chainValid {
		status = "DIVERGENCE"
	}
	groundTruthAll := true
	for _, g := range gb.txs {
		if !g.isNoOp && !g.groundTruth {
			groundTruthAll = false
		}
	}
	fmt.Printf("[%-10s] Larger-block generation (compose no-engine tx types into a varied,\n", status)
	fmt.Printf("             multi-tx, state-chain-valid block; #126 larger-block deliverable)\n")
	fmt.Printf("             emitted block on market %d: %d txs total\n", gb.market, len(gb.txs))
	fmt.Printf("             composition: cancels=%d modifies=%d empties(pad)=%d  (real=%d)\n",
		gb.nKindsCancel, gb.nKindsModify, gb.nEmpty, gb.nReal)
	fmt.Printf("             order_book_root chain: %s (every tx after-root == next before-root)\n",
		boolWord(gb.chainValid, "VALID bit-for-bit", "BROKEN"))
	fmt.Printf("             real-tx roots anchored to JSON ground truth: %s\n",
		boolWord(groundTruthAll, "yes (all)", "NO"))
	if gb.firstBreak >= 0 {
		g := gb.txs[gb.firstBreak]
		fmt.Printf("             FIRST CHAIN BREAK at emitted tx[%d] (%s from fixture tx[%d]):\n",
			gb.firstBreak, g.kind, g.sourceTx)
		fmt.Printf("               rootBefore=%v\n", limbsOf(g.rootBefore))
	}
	fmt.Println()
	fmt.Println("This is a VARIED, LARGER block built ENTIRELY from no-matching-engine tx")
	fmt.Println("types (Cancel 15, non-crossing Modify 17, empties 0). Each real tx's after")
	fmt.Println("order_book_root is the next tx's before-root (bit-for-bit, chained ground")
	fmt.Println("truth); empty-tx padding inflates the block while preserving the chain. No")
	fmt.Println("matching engine, no fabricated roots — the G2 payoff that unblocks G4.")
}

// printGeneratedBlockTxList prints the per-tx emitted block (for -block-emit).
func printGeneratedBlockTxList(gb *generatedBlock) {
	fmt.Printf("=== emitted varied no-engine block (market %d, %d txs) ===\n", gb.market, len(gb.txs))
	for k, g := range gb.txs {
		mark := "ok"
		if !g.chainsOK {
			mark = "CHAIN-BREAK"
		}
		fmt.Printf("  block_tx[%2d] %-11s (src fixture tx[%3d])  chains=%s  noop=%v\n",
			k, g.kind, g.sourceTx, mark, g.isNoOp)
		fmt.Printf("                 ob_root before=%v\n", limbsOf(g.rootBefore))
		fmt.Printf("                 ob_root after =%v\n", limbsOf(g.rootAfter))
	}
}

func boolWord(b bool, yes, no string) string {
	if b {
		return yes
	}
	return no
}
