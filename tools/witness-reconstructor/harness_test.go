// Copyright (c) Elliot Technologies, Inc.
// SPDX-License-Identifier: BUSL-1.1

package main

import (
	"os"
	"testing"
)

// benchJSON locates the bundled fixture relative to this module directory.
const benchJSON = "../../bench/bench_test.json"

func haveFixture(t *testing.T) *Block {
	t.Helper()
	if _, err := os.Stat(benchJSON); err != nil {
		t.Skipf("fixture %s not present: %v", benchJSON, err)
	}
	b, err := loadBlock(benchJSON)
	if err != nil {
		t.Fatalf("loadBlock: %v", err)
	}
	if len(b.Txs) == 0 {
		t.Fatalf("no txs parsed")
	}
	return b
}

// TestApiKeyLeafEmptyShortcut: all-zero pubkey -> HashOut::ZERO (api_key.rs:72-74).
func TestApiKeyLeafEmptyShortcut(t *testing.T) {
	got := apiKeyLeafHash([5]uint64{}, 0)
	if !equalHash(got, zeroHash()) {
		t.Fatalf("empty api_key leaf = %v, want ZERO", limbsOf(got))
	}
}

// TestOrderLeafEmptyShortcut: all-zero sums -> HashOut::ZERO (order.rs:70-72).
func TestOrderLeafEmptyShortcut(t *testing.T) {
	if !equalHash(orderLeafHash(0, 0, 0, 0), zeroHash()) {
		t.Fatal("empty order leaf must be ZERO")
	}
}

// TestApiKeySubTreeTx0 is the #120 thin-slice PoC, locked in against ground
// truth: apiKeyLeafHash(akb) folded via mpakb over LE bits of akb.aki must
// equal ab[OWNER].akr, all 4 limbs identical.
func TestApiKeySubTreeTx0(t *testing.T) {
	b := haveFixture(t)
	tx := b.Txs[0]
	leaf := apiKeyLeafHash(tx.ApiKeyBefore.PublicKey, tx.ApiKeyBefore.Nonce)
	got := merkleFold(leaf, toSiblings(tx.ApiKeyProof), tx.ApiKeyBefore.Index)
	want := toHashOut(tx.AccountsBefore[ownerAccountID].ApiKeyRoot)
	if !equalHash(got, want) {
		t.Fatalf("api_key sub-tree tx0: got=%v want(akr)=%v", limbsOf(got), limbsOf(want))
	}
}

// TestAccountOrdersSubTreeTx0: aob -> ab[OWNER].aor (empty order leaf path).
func TestAccountOrdersSubTreeTx0(t *testing.T) {
	b := haveFixture(t)
	tx := b.Txs[0]
	leaf := accountOrderLeafHash(tx.AccountOrderBefore)
	got := merkleFold(leaf, toSiblings(tx.AccountOrdersProof[ownerAccountID]), uint64(tx.AccountOrderBefore.Index0))
	want := toHashOut(tx.AccountsBefore[ownerAccountID].AccountOrdersRoot)
	if !equalHash(got, want) {
		t.Fatalf("account_orders sub-tree tx0: got=%v want(aor)=%v", limbsOf(got), limbsOf(want))
	}
}

// TestMarketLeafToOmtrTx0: marketLeafHash(mmb) folded via mpmmb -> omtr.
// Exercises the full market leaf hash recipe (market.rs:277) end-to-end against
// the block-level old market tree root.
func TestMarketLeafToOmtrTx0(t *testing.T) {
	b := haveFixture(t)
	tx := b.Txs[0]
	leaf := marketLeafHash(tx.MarketBefore)
	got := merkleFold(leaf, toSiblings(tx.MarketProof), uint64(tx.MarketBefore.Index))
	want := toHashOut(b.OldMarketTreeRoot)
	if !equalHash(got, want) {
		t.Fatalf("market->omtr tx0: got=%v want(omtr)=%v", limbsOf(got), limbsOf(want))
	}
}

// TestAssetSubTreeTx0: empty asset leaves (all balances zero) fold to asr.
func TestAssetSubTreeTx0(t *testing.T) {
	b := haveFixture(t)
	tx := b.Txs[0]
	leaf := accountAssetLeafHash(0, 0, 0) // tx0 owner assets are empty
	got := merkleFold(leaf, toSiblings(tx.AssetProof[ownerAccountID][0]), uint64(tx.AssetIndices[0]))
	want := toHashOut(tx.AccountsBefore[ownerAccountID].AssetRoot)
	if !equalHash(got, want) {
		t.Fatalf("asset sub-tree tx0: got=%v want(asr)=%v", limbsOf(got), limbsOf(want))
	}
}

// TestApiKeyAllTxsBitForBit: the api_key sub-tree reproduces ground truth for
// EVERY tx in the block, bit-for-bit. This is the strongest Phase-0 invariant.
func TestApiKeyAllTxsBitForBit(t *testing.T) {
	b := haveFixture(t)
	for i, tx := range b.Txs {
		if len(tx.ApiKeyProof) != apiKeyDepth || len(tx.AccountsBefore) <= ownerAccountID {
			continue
		}
		leaf := apiKeyLeafHash(tx.ApiKeyBefore.PublicKey, tx.ApiKeyBefore.Nonce)
		got := merkleFold(leaf, toSiblings(tx.ApiKeyProof), tx.ApiKeyBefore.Index)
		want := toHashOut(tx.AccountsBefore[ownerAccountID].ApiKeyRoot)
		if !equalHash(got, want) {
			t.Fatalf("api_key divergence tx[%d]: got=%v want=%v", i, limbsOf(got), limbsOf(want))
		}
	}
}

// TestAccountOrdersAllTxsMatchSomeSlot: every tx's account_order leaf+proof
// reproduces SOME account slot's aor (owner or maker, per the circuit's
// select_hash), bit-for-bit.
func TestAccountOrdersAllTxsMatchSomeSlot(t *testing.T) {
	b := haveFixture(t)
	for i, tx := range b.Txs {
		if len(tx.AccountOrdersProof) <= ownerAccountID ||
			len(tx.AccountOrdersProof[ownerAccountID]) != accountOrdersDepth {
			continue
		}
		leaf := accountOrderLeafHash(tx.AccountOrderBefore)
		got := merkleFold(leaf, toSiblings(tx.AccountOrdersProof[ownerAccountID]), uint64(tx.AccountOrderBefore.Index0))
		matched := false
		for _, a := range tx.AccountsBefore {
			if equalHash(got, toHashOut(a.AccountOrdersRoot)) {
				matched = true
				break
			}
		}
		if !matched {
			t.Fatalf("account_orders tx[%d]: fold %v matched no slot's aor", i, limbsOf(got))
		}
	}
}
