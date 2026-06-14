// Copyright (c) Elliot Technologies, Inc.
// SPDX-License-Identifier: BUSL-1.1
//
// Phase-1 witness-reconstructor: Cancel (tx_type 15, JSON key 2co) end-to-end
// state transition + bit-for-bit validation against bench/bench_test.json
// ground truth (issue #123, epic #121; depends on Phase 0 #122).
//
// Cancel is the simplest tx (NO matching engine, l2_cancel_order.rs:173-233).
// Its apply() touches:
//   - order-book tree: remove the order leaf (set empty) + recompute the
//     aggregation nodes up the path (get_order_book_path_delta,
//     matching_engine.rs:42-130) -> new order_book_root.
//   - account_orders sub-tree: remove the account_order leaf (set empty) ->
//     new account_orders_root.
//   - owner account: decrement order count; api_key nonce++ (every L2 tx,
//     tx_constraints.rs:2601); spot+limit decrement locked balance.
//   - market leaf: order_book_root + total_order_count (nonces unchanged).
//
// GROUND-TRUTH STRATEGY (hard honesty, no fabrication): bench_test.json stores
// only SPARSE per-tx before-leaves + proofs (no per-tx after-roots). State
// chains tx-to-tx (block_tx_constraints.rs:426-462): a tx's after-root IS the
// next tx's before-root for the same (sub)tree. So a cancel's reconstructed
// after order_book_root is validated bit-for-bit against the NEXT tx that
// touches the SAME market index (its mmb.r before-root). The order_book_root is
// a block-level-chained root carried inside the market leaf, so reproducing it
// is a genuine end-to-end Cancel validation, not a self-referential check.
package main

import "fmt"

// cancelValidation accumulates Phase-1 Cancel reconstruction results.
type cancelValidation struct {
	totalCancels    int // tx_type == 15
	realCancels     int // non-empty order (actually mutates the order book)
	chainable       int // real cancel with a later same-market tx (after-root ground truth)
	beforeMatched   int // before order_book_root == stored mmb.r (fold sanity)
	afterMatched    int // after order_book_root == next-same-market tx's mmb.r (the goal)
	marketLeafMatch int // reconstructed after market leaf hash == next tx's mmb leaf hash
	aoBeforeMatched int // account_orders before-root matches a stored aor
	divergences     []cancelDivergence
}

type cancelDivergence struct {
	tx       int
	next     int
	field    string
	expected [4]uint64
	got      [4]uint64
}

func (cv *cancelValidation) addDivergence(tx, next int, field string, expected, got HashOut) {
	if len(cv.divergences) < 8 {
		cv.divergences = append(cv.divergences, cancelDivergence{
			tx: tx, next: next, field: field,
			expected: limbsOf(expected), got: limbsOf(got),
		})
	} else if len(cv.divergences) == 8 {
		cv.divergences = append(cv.divergences, cancelDivergence{tx: -1, field: "... (further divergences omitted; see counts)"})
	}
}

// validateCancels runs the Phase-1 Cancel reconstruction over the first n txs and
// validates each real, chainable cancel's reconstructed AFTER order_book_root +
// after market leaf bit-for-bit against the next-same-market tx's before state.
func validateCancels(block *Block, n int) *cancelValidation {
	cv := &cancelValidation{}
	for i := 0; i < n; i++ {
		tx := &block.Txs[i]
		if tx.TxType != txTypeL2CancelOrder {
			continue
		}
		cv.totalCancels++
		if !isRealCancel(tx) {
			continue // empty-order cancel: success==false, no root change
		}
		cv.realCancels++
		if len(tx.OrderBookPath) != orderBookDepth || len(tx.MarketProof) != marketDepth {
			continue
		}

		// (a) before-root sanity: fold the loaded order -> stored mmb.r.
		before := cancelOrderBookBeforeRoot(tx)
		storedBefore := toHashOut(tx.MarketBefore.OrderBookRoot)
		if equalHash(before, storedBefore) {
			cv.beforeMatched++
		} else {
			cv.addDivergence(i, -1, "order_book_root(before)", storedBefore, before)
		}

		// (b) account_orders before-root: should match some stored aor (Phase 0
		//     proved this; re-derived to show the before->after transition).
		if aob, ok := cancelAccountOrdersBeforeRoot(tx); ok {
			for _, a := range tx.AccountsBefore {
				if equalHash(aob, toHashOut(a.AccountOrdersRoot)) {
					cv.aoBeforeMatched++
					break
				}
			}
		}

		// after-root ground truth: the next tx touching the same market.
		nxt := nextSameMarketTx(block.Txs, i, tx.MarketBefore.Index)
		if nxt < 0 || nxt >= n {
			continue
		}
		cv.chainable++

		// (c) after order_book_root == next tx's before order_book_root.
		gotAfter := cancelOrderBookAfterRoot(tx)
		wantAfter := toHashOut(block.Txs[nxt].MarketBefore.OrderBookRoot)
		if equalHash(gotAfter, wantAfter) {
			cv.afterMatched++
		} else {
			cv.addDivergence(i, nxt, "order_book_root(after)", wantAfter, gotAfter)
		}

		// (d) full after market leaf hash == next tx's before market leaf hash.
		//     Cancel mutates only order_book_root + total_order_count (-1); nonces
		//     and all other fields are unchanged.
		afterLeaf := tx.MarketBefore
		afterLeaf.OrderBookRoot = limbsOf(gotAfter)
		afterLeaf.TotalOrderCount = tx.MarketBefore.TotalOrderCount - 1
		gotLeaf := marketLeafHash(afterLeaf)
		wantLeaf := marketLeafHash(block.Txs[nxt].MarketBefore)
		if equalHash(gotLeaf, wantLeaf) {
			cv.marketLeafMatch++
		} else {
			cv.addDivergence(i, nxt, "market_leaf(after)", wantLeaf, gotLeaf)
		}
	}
	return cv
}

// printCancelEvidence prints one fully worked Cancel reconstruction with
// expected-vs-got limbs, citing resolvable tx indices in the bundled fixture.
// It picks the first real, chainable cancel (deterministic) so the evidence is
// reproducible with `go run . -evidence`.
func printCancelEvidence(block *Block) {
	for i := range block.Txs {
		tx := &block.Txs[i]
		if tx.TxType != txTypeL2CancelOrder || !isRealCancel(tx) ||
			len(tx.OrderBookPath) != orderBookDepth || len(tx.MarketProof) != marketDepth {
			continue
		}
		nxt := nextSameMarketTx(block.Txs, i, tx.MarketBefore.Index)
		if nxt < 0 {
			continue
		}
		ob := tx.OrderInfoBefr
		before := cancelOrderBookBeforeRoot(tx)
		after := cancelOrderBookAfterRoot(tx)
		fmt.Printf("=== worked Cancel example: tx[%d] -> tx[%d] (market %d) ===\n", i, nxt, tx.MarketBefore.Index)
		fmt.Printf("cancelled order (obinfob): kp=%d kn=%d ab=%d aq=%d bb=%d bq=%d\n",
			ob.KeyPrice, ob.KeyNonce, ob.AskBaseSum, ob.AskQuoteSum, ob.BidBaseSum, ob.BidQuoteSum)
		fmt.Println()
		fmt.Println("order_book_root BEFORE (reconstructed) vs stored tx[" + fmt.Sprint(i) + "].mmb.r:")
		fmt.Printf("  got      = %v\n", limbsOf(before))
		fmt.Printf("  expected = %v\n", tx.MarketBefore.OrderBookRoot)
		fmt.Printf("  match    = %v\n", equalHash(before, toHashOut(tx.MarketBefore.OrderBookRoot)))
		fmt.Println()
		fmt.Println("order_book_root AFTER (reconstructed: empty leaf + path delta) vs stored tx[" + fmt.Sprint(nxt) + "].mmb.r:")
		fmt.Printf("  got      = %v\n", limbsOf(after))
		fmt.Printf("  expected = %v\n", block.Txs[nxt].MarketBefore.OrderBookRoot)
		fmt.Printf("  match    = %v\n", equalHash(after, toHashOut(block.Txs[nxt].MarketBefore.OrderBookRoot)))
		fmt.Println()
		fmt.Printf("market total_order_count: tx[%d].toc=%d -> reconstructed %d == tx[%d].toc=%d : %v\n",
			i, tx.MarketBefore.TotalOrderCount, tx.MarketBefore.TotalOrderCount-1,
			nxt, block.Txs[nxt].MarketBefore.TotalOrderCount,
			tx.MarketBefore.TotalOrderCount-1 == block.Txs[nxt].MarketBefore.TotalOrderCount)
		return
	}
	fmt.Println("no real, chainable cancel found in fixture")
}

// printCancelResult renders the Phase-1 Cancel validation summary.
func printCancelResult(cv *cancelValidation) {
	status := "PASS"
	if len(cv.divergences) > 0 {
		status = "DIVERGENCE"
	} else if cv.chainable == 0 {
		status = "NO-DATA"
	}
	fmt.Printf("[%-10s] Cancel reconstruction (order leaf -> empty; get_order_book_path_delta;\n", status)
	fmt.Printf("             account_order -> empty; market toc-1; api_key nonce++)\n")
	fmt.Printf("             total cancels=%d  real (mutating) cancels=%d  chainable=%d\n",
		cv.totalCancels, cv.realCancels, cv.chainable)
	fmt.Printf("             order_book_root BEFORE (fold sanity vs mmb.r):      %d/%d bit-for-bit\n",
		cv.beforeMatched, cv.realCancels)
	fmt.Printf("             account_orders BEFORE (vs stored aor):             %d/%d bit-for-bit\n",
		cv.aoBeforeMatched, cv.realCancels)
	fmt.Printf("             order_book_root AFTER  (vs next-same-mkt mmb.r):   %d/%d bit-for-bit  <-- the goal\n",
		cv.afterMatched, cv.chainable)
	fmt.Printf("             market leaf    AFTER  (r:=after, toc-1, vs next):  %d/%d bit-for-bit\n",
		cv.marketLeafMatch, cv.chainable)
	for _, d := range cv.divergences {
		if d.tx == -1 {
			fmt.Printf("             %s\n", d.field)
			continue
		}
		fmt.Printf("             DIVERGENCE tx[%d]->tx[%d] %s: expected=%v got=%v\n",
			d.tx, d.next, d.field, d.expected, d.got)
	}
	fmt.Println()
	fmt.Println("Ground truth: bench_test.json stores only SPARSE per-tx before-leaves+proofs")
	fmt.Println("(no per-tx after-roots). State chains tx-to-tx, so a cancel's reconstructed")
	fmt.Println("after order_book_root is validated against the NEXT tx touching the SAME market")
	fmt.Println("(its mmb.r before-root). The order_book_root is a block-level-chained root")
	fmt.Println("carried in the market leaf, so reproducing it is a genuine Cancel validation.")
}

// cancelOrderBookPathDelta applies get_order_book_path_delta (matching_engine.rs
// :42-130) for a Cancel: order_after is the EMPTY order (all four sums = 0). It
// returns the order-book proof path with each level's aggregated sums updated;
// every sibling_child_hash is carried unchanged from the before path (the cancel
// only mutates the single order leaf + the aggregation along its path).
//
//	level 0:  sibling_sum = before[0].sum - order_before.sum
//	          after[0].sum = order_after.sum(=0) + sibling_sum
//	level i:  sibling_sum = before[i].sum - before[i-1].sum
//	          after[i].sum = after[i-1].sum + sibling_sum
func cancelOrderBookPathDelta(before []OrderBookNode, ob OrderInfo) []OrderBookNode {
	after := make([]OrderBookNode, len(before))

	// level 0: order_after == empty (all sums 0).
	after[0] = OrderBookNode{
		SiblingHash: before[0].SiblingHash,
		AskBaseSum:  before[0].AskBaseSum - ob.AskBaseSum,
		AskQuoteSum: before[0].AskQuoteSum - ob.AskQuoteSum,
		BidBaseSum:  before[0].BidBaseSum - ob.BidBaseSum,
		BidQuoteSum: before[0].BidQuoteSum - ob.BidQuoteSum,
	}
	for i := 1; i < len(before); i++ {
		sibAB := before[i].AskBaseSum - before[i-1].AskBaseSum
		sibAQ := before[i].AskQuoteSum - before[i-1].AskQuoteSum
		sibBB := before[i].BidBaseSum - before[i-1].BidBaseSum
		sibBQ := before[i].BidQuoteSum - before[i-1].BidQuoteSum
		after[i] = OrderBookNode{
			SiblingHash: before[i].SiblingHash,
			AskBaseSum:  after[i-1].AskBaseSum + sibAB,
			AskQuoteSum: after[i-1].AskQuoteSum + sibAQ,
			BidBaseSum:  after[i-1].BidBaseSum + sibBB,
			BidQuoteSum: after[i-1].BidQuoteSum + sibBQ,
		}
	}
	return after
}

// cancelOrderBookAfterRoot reconstructs the order_book_root AFTER a Cancel:
// empty the order leaf and fold the delta-updated path. The fold uses the order
// before's key_price/key_nonce for the path bits (the leaf POSITION is unchanged
// by a cancel; only its value goes to ZERO).
func cancelOrderBookAfterRoot(tx *Tx) HashOut {
	ob := tx.OrderInfoBefr
	after := cancelOrderBookPathDelta(tx.OrderBookPath, ob)
	bits := orderBookPathBits(ob.KeyPrice, ob.KeyNonce)
	return orderBookFold(zeroHash(), after, bits)
}

// cancelOrderBookBeforeRoot reconstructs the order_book_root BEFORE a Cancel:
// fold the (non-empty) order leaf through the before path. Used to prove the
// fold faithfully reproduces the stored mmb.r before any mutation.
func cancelOrderBookBeforeRoot(tx *Tx) HashOut {
	ob := tx.OrderInfoBefr
	leaf := orderLeafHash(ob.AskBaseSum, ob.AskQuoteSum, ob.BidBaseSum, ob.BidQuoteSum)
	bits := orderBookPathBits(ob.KeyPrice, ob.KeyNonce)
	return orderBookFold(leaf, tx.OrderBookPath, bits)
}

// cancelAccountOrdersAfterRoot reconstructs the account_orders sub-tree root
// AFTER a Cancel: the account_order leaf is set EMPTY (account_order.rs empty
// keeps index_0/index_1/owner_account_index, zeros the rest -> is_empty() ->
// HashOut::ZERO) and re-folded through the SAME account_orders proof (mpokb), at
// the SAME path index (index_0). The cancel mutates only this one leaf in the
// account_orders sub-tree, so the siblings are unchanged.
func cancelAccountOrdersAfterRoot(tx *Tx) (HashOut, bool) {
	if len(tx.AccountOrdersProof) <= ownerAccountID ||
		len(tx.AccountOrdersProof[ownerAccountID]) != accountOrdersDepth {
		return HashOut{}, false
	}
	leafAfter := zeroHash() // emptied account_order -> is_empty() -> ZERO
	idx := uint64(tx.AccountOrderBefore.Index0)
	return merkleFold(leafAfter, toSiblings(tx.AccountOrdersProof[ownerAccountID]), idx), true
}

// cancelAccountOrdersBeforeRoot reconstructs the account_orders sub-tree root
// BEFORE the cancel (the non-empty account_order leaf folded through mpokb),
// which Phase 0 already proved equals a stored aor. Re-derived here so Phase 1
// can show before -> after as one transition.
func cancelAccountOrdersBeforeRoot(tx *Tx) (HashOut, bool) {
	if len(tx.AccountOrdersProof) <= ownerAccountID ||
		len(tx.AccountOrdersProof[ownerAccountID]) != accountOrdersDepth {
		return HashOut{}, false
	}
	leaf := accountOrderLeafHash(tx.AccountOrderBefore)
	idx := uint64(tx.AccountOrderBefore.Index0)
	return merkleFold(leaf, toSiblings(tx.AccountOrdersProof[ownerAccountID]), idx), true
}

// isRealCancel reports whether a cancel tx actually mutates the order book: the
// loaded order has at least one non-zero aggregation sum. An empty-order cancel
// has success==false in the circuit (is_account_order_present==false,
// l2_cancel_order.rs:129) and changes no root.
func isRealCancel(tx *Tx) bool {
	if tx.TxType != txTypeL2CancelOrder {
		return false
	}
	ob := tx.OrderInfoBefr
	return ob.AskBaseSum != 0 || ob.AskQuoteSum != 0 || ob.BidBaseSum != 0 || ob.BidQuoteSum != 0
}

// nextSameMarketTx returns the index of the next tx (> i) that touches the same
// market index as tx[i], or -1. That tx's market leaf carries the before-root
// that equals tx[i]'s after order_book_root (state chains tx-to-tx, with no
// intervening tx touching this market).
func nextSameMarketTx(txs []Tx, i int, market uint16) int {
	for j := i + 1; j < len(txs); j++ {
		if len(txs[j].MarketProof) == marketDepth && txs[j].MarketBefore.Index == market {
			return j
		}
	}
	return -1
}
