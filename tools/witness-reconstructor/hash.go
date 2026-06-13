// Copyright (c) Elliot Technologies, Inc.
// SPDX-License-Identifier: BUSL-1.1

// Package main implements the Phase-0 witness-reconstructor replay/validation
// harness for issue #122 (epic #121).
//
// This file wraps the verified Poseidon2 primitives from
// github.com/elliottech/poseidon_crypto v0.0.17 (package poseidon2_plonky2,
// imported below from .../hash/poseidon2_goldilocks_plonky2). Feasibility #120
// proved this library is a BIT-FOR-BIT drop-in for the prover's in-circuit
// Poseidon2 across three PoCs. We re-derive each leaf hash + Merkle fold exactly
// as the circuit does and compare against the JSON-stored ground-truth roots.
//
// Source-of-truth circuit references (verified at handoff tip
// c62df3a79d844ccaae43fd551d8fa637e158dd32):
//   - merkle_helpers.rs:84  verify_merkle_proof (fold leaf-upward, swap by bit)
//   - merkle_helpers.rs:134 recalculate_root
//   - hash_utils.rs:88      hash_two_to_one_swap (left->state[0..4], right->state[4..8])
//   - api_key.rs:71-84      api_key leaf hash
//   - order.rs:69-80        order-book Order leaf hash
//   - account_order.rs:134  AccountOrder leaf hash
//   - account_asset.rs:101  AccountAsset leaf hash
//   - market.rs:277         Market leaf hash
//   - order_book_node.rs:47 OrderBookNode internal hash
package main

import (
	g "github.com/elliottech/poseidon_crypto/field/goldilocks"
	p2 "github.com/elliottech/poseidon_crypto/hash/poseidon2_goldilocks_plonky2"
)

// HashOut is the 4-limb Goldilocks Poseidon2 output, identical to the circuit's
// HashOut<F> and the library's poseidon2_plonky2.HashOut ([4]GoldilocksField).
type HashOut = p2.HashOut

// zeroHash is HashOut::ZERO = [0,0,0,0]. Used for the empty-leaf shortcut
// (e.g. all-zero pubkey api_key, is_empty() order/account_order/asset).
func zeroHash() HashOut { return p2.EmptyHashOut() }

// limbsOf returns the 4 canonical uint64 limbs of a HashOut for printing /
// comparison. HashOut limb i = field element i (NO byte reversal), per
// merkle_helpers semantics confirmed in #120.
func limbsOf(h HashOut) [4]uint64 {
	return [4]uint64{
		h[0].ToCanonicalUint64(),
		h[1].ToCanonicalUint64(),
		h[2].ToCanonicalUint64(),
		h[3].ToCanonicalUint64(),
	}
}

// hashFromLimbs builds a HashOut from 4 canonical uint64 limbs (as stored in
// the JSON ground-truth roots and Merkle-proof siblings).
func hashFromLimbs(l [4]uint64) HashOut {
	return p2.HashOutFromUint64Array(l)
}

// equalHash compares two HashOuts limb-for-limb in canonical form.
func equalHash(a, b HashOut) bool {
	la, lb := limbsOf(a), limbsOf(b)
	return la == lb
}

// fcU64 mirrors F::from_canonical_u64 / from_canonical_u8/u16/u32 and
// from_canonical_i64 for NON-NEGATIVE values: just the canonical field element.
func fcU64(v uint64) g.GoldilocksField { return g.GoldilocksField(v) }

// fncI64 mirrors F::from_noncanonical_i64 (the library's NonCannonicalGoldilocks
// Field, sic spelling): used for SIGNED i64 fields that may be negative
// (e.g. order-book ask/bid base/quote sums).
func fncI64(v int64) g.GoldilocksField { return g.NonCannonicalGoldilocksField(v) }

// hashNoPad is the overwrite-mode sponge (no padding) used by the circuit's
// Poseidon2Hash::hash_no_pad / hash_n_to_hash_no_pad.
func hashNoPad(in []g.GoldilocksField) HashOut { return p2.HashNoPad(in) }

// twoToOne compresses two HashOuts: input1 -> state[0..4] (LEFT),
// input2 -> state[4..8] (RIGHT), permute, take first 4. Matches
// hash_utils.rs:88 hash_two_to_one_swap with swap=false.
func twoToOne(a, b HashOut) HashOut { return p2.HashTwoToOne(a, b) }

// merkleFold re-derives a Merkle root from a leaf, its sibling path, and a leaf
// index, exactly as merkle_helpers.rs verify_merkle_proof / recalculate_root:
//
//   - path bits = little-endian decomposition of index, LEAF-LEVEL FIRST:
//     bit[i] = (index >> i) & 1
//   - bit==0 => node is LEFT  child => twoToOne(node, sibling)
//   - bit==1 => node is RIGHT child => twoToOne(sibling, node)
//   - fold runs from the leaf upward; siblings[0] is the leaf-level sibling.
//
// len(siblings) must equal the tree depth.
func merkleFold(leaf HashOut, siblings []HashOut, index uint64) HashOut {
	node := leaf
	for i := 0; i < len(siblings); i++ {
		bit := (index >> uint(i)) & 1
		if bit == 0 {
			node = twoToOne(node, siblings[i])
		} else {
			node = twoToOne(siblings[i], node)
		}
	}
	return node
}

// ----------------------------------------------------------------------------
// Leaf-hash recipes (each transcribed EXACTLY from the cited circuit source).
// ----------------------------------------------------------------------------

// apiKeyLeafHash implements api_key.rs:71-84.
//
//	if is_empty() (public_key all-zero) -> HashOut::ZERO
//	else hash_no_pad([pk0,pk1,pk2,pk3,pk4, from_canonical_i64(nonce)])
//
// public_key is a quintic-extension field element (5 limbs). is_empty() is
// public_key.is_zero() (api_key.rs:67-69).
func apiKeyLeafHash(pubKey [5]uint64, nonce int64) HashOut {
	if pubKey == [5]uint64{} {
		return zeroHash()
	}
	return hashNoPad([]g.GoldilocksField{
		fcU64(pubKey[0]), fcU64(pubKey[1]), fcU64(pubKey[2]),
		fcU64(pubKey[3]), fcU64(pubKey[4]),
		fcU64(uint64(nonce)), // from_canonical_i64; nonce expected non-negative
	})
}

// orderLeafHash implements order.rs:69-80 (the order-book Order leaf).
//
//	if is_empty() (all four sums == 0) -> HashOut::ZERO
//	else hash_no_pad([fnc_i64(ask_base), fnc_i64(ask_quote),
//	                  fnc_i64(bid_base), fnc_i64(bid_quote)])
func orderLeafHash(askBase, askQuote, bidBase, bidQuote int64) HashOut {
	if askBase == 0 && askQuote == 0 && bidBase == 0 && bidQuote == 0 {
		return zeroHash()
	}
	return hashNoPad([]g.GoldilocksField{
		fncI64(askBase), fncI64(askQuote), fncI64(bidBase), fncI64(bidQuote),
	})
}

// orderBookInternalHash implements order_book_node.rs:47-56. Internal nodes of
// the order-book tree carry aggregated sums; the hash is just the 4 sums as the
// 4 HashOut limbs (NOT a Poseidon permutation).
func orderBookInternalHash(askBase, askQuote, bidBase, bidQuote int64) HashOut {
	return HashOut{fncI64(askBase), fncI64(askQuote), fncI64(bidBase), fncI64(bidQuote)}
}

// accountOrderLeafHash implements account_order.rs:134-157 (16 elements).
// is_empty() (account_order.rs:115-132) is true when oi,coi,iba,p,n,rba,a,t,
// tif,ro,tp,e,ts,ttoi0,ttoi1,tcoi0 are all zero. The cited field encodings:
//
//	from_canonical_i64(order_index), from_canonical_i64(client_order_index),
//	from_canonical_i64(initial_base_amount), from_canonical_u32(price),
//	from_canonical_i64(nonce), from_canonical_i64(remaining_base_amount),
//	from_canonical_u8(is_ask), from_canonical_u8(order_type),
//	from_canonical_u8(time_in_force), from_canonical_u8(reduce_only),
//	from_canonical_u32(trigger_price), from_canonical_i64(expiry),
//	from_canonical_u8(trigger_status), from_canonical_i64(to_trigger_order_index0),
//	from_canonical_i64(to_trigger_order_index1), from_canonical_i64(to_cancel_order_index0)
func accountOrderLeafHash(a AccountOrderLeaf) HashOut {
	if a.OrderIndex == 0 && a.ClientOrderIndex == 0 && a.InitialBaseAmount == 0 &&
		a.Price == 0 && a.Nonce == 0 && a.RemainingBaseAmount == 0 && a.IsAsk == 0 &&
		a.OrderType == 0 && a.TimeInForce == 0 && a.ReduceOnly == 0 && a.TriggerPrice == 0 &&
		a.Expiry == 0 && a.TriggerStatus == 0 && a.ToTriggerOrderIndex0 == 0 &&
		a.ToTriggerOrderIndex1 == 0 && a.ToCancelOrderIndex0 == 0 {
		return zeroHash()
	}
	return hashNoPad([]g.GoldilocksField{
		fcU64(uint64(a.OrderIndex)),
		fcU64(uint64(a.ClientOrderIndex)),
		fcU64(uint64(a.InitialBaseAmount)),
		fcU64(uint64(a.Price)),
		fcU64(uint64(a.Nonce)),
		fcU64(uint64(a.RemainingBaseAmount)),
		fcU64(uint64(a.IsAsk)),
		fcU64(uint64(a.OrderType)),
		fcU64(uint64(a.TimeInForce)),
		fcU64(uint64(a.ReduceOnly)),
		fcU64(uint64(a.TriggerPrice)),
		fcU64(uint64(a.Expiry)),
		fcU64(uint64(a.TriggerStatus)),
		fcU64(uint64(a.ToTriggerOrderIndex0)),
		fcU64(uint64(a.ToTriggerOrderIndex1)),
		fcU64(uint64(a.ToCancelOrderIndex0)),
	})
}

// accountAssetLeafHash implements account_asset.rs:101-124.
//
//	if is_empty() (balance==0 && locked_balance==0 && margin_mode==0) -> ZERO
//	else hash_no_pad([balance limbs (BIG_U96_LIMBS u32 limbs, LE),
//	                  locked_balance limbs (BIG_U96_LIMBS), margin_mode])
//
// BIG_U96_LIMBS = 3 (96 bits as three u32 limbs). The balance/locked_balance
// are BigUint; we split each into bigU96Limbs little-endian u32 limbs.
func accountAssetLeafHash(balance, lockedBalance uint64, marginMode uint8) HashOut {
	if balance == 0 && lockedBalance == 0 && marginMode == 0 {
		return zeroHash()
	}
	elems := make([]g.GoldilocksField, 0, 2*bigU96Limbs+1)
	for _, limb := range u96Limbs(balance) {
		elems = append(elems, fcU64(uint64(limb)))
	}
	for _, limb := range u96Limbs(lockedBalance) {
		elems = append(elems, fcU64(uint64(limb)))
	}
	elems = append(elems, fcU64(uint64(marginMode)))
	return hashNoPad(elems)
}

// bigU96Limbs is BIG_U96_LIMBS (account_asset.rs / config.rs): a 96-bit balance
// represented as 3 little-endian u32 limbs.
const bigU96Limbs = 3

// u96Limbs splits a balance into bigU96Limbs little-endian u32 limbs. The
// harness only consumes balances that fit in uint64 (the sample's touched asset
// leaves are all-zero/empty in the validated tx[0]); the 3rd limb is therefore
// zero for these but is included to match the circuit's fixed-width layout.
func u96Limbs(v uint64) [bigU96Limbs]uint32 {
	return [bigU96Limbs]uint32{
		uint32(v),
		uint32(v >> 32),
		0,
	}
}

// marketLeafHash implements market.rs:277-304. is_empty() (market.rs:256-275)
// is true when ask_nonce,bid_nonce,taker_fee,maker_fee,liquidation_fee,
// min_base_amount,min_quote_amount,status,order_quote_limit,total_order_count,
// market_type,base_asset_id,quote_asset_id,size_ext,quote_ext are all zero.
//
// The leaf hash field ORDER (NOTE: differs from struct order):
//
//	market_type, status, base_asset_id, quote_asset_id, ask_nonce, bid_nonce,
//	taker_fee, maker_fee, liquidation_fee, min_base_amount, min_quote_amount,
//	order_quote_limit, total_order_count, size_extension_multiplier,
//	quote_extension_multiplier, order_book_root[0..4].
//
// All scalar fields use from_canonical_* (set_market_target, market.rs:312-349)
// so they are non-negative -> plain GoldilocksField(uint64).
func marketLeafHash(m MarketLeaf) HashOut {
	if m.AskNonce == 0 && m.BidNonce == 0 && m.TakerFee == 0 && m.MakerFee == 0 &&
		m.LiquidationFee == 0 && m.MinBaseAmount == 0 && m.MinQuoteAmount == 0 &&
		m.Status == 0 && m.OrderQuoteLimit == 0 && m.TotalOrderCount == 0 &&
		m.MarketType == 0 && m.BaseAssetID == 0 && m.QuoteAssetID == 0 &&
		m.SizeExtensionMultiplier == 0 && m.QuoteExtensionMultiplier == 0 {
		return zeroHash()
	}
	r := m.OrderBookRoot
	return hashNoPad([]g.GoldilocksField{
		fcU64(uint64(m.MarketType)),
		fcU64(uint64(m.Status)),
		fcU64(uint64(m.BaseAssetID)),
		fcU64(uint64(m.QuoteAssetID)),
		fcU64(uint64(m.AskNonce)),
		fcU64(uint64(m.BidNonce)),
		fcU64(uint64(m.TakerFee)),
		fcU64(uint64(m.MakerFee)),
		fcU64(uint64(m.LiquidationFee)),
		fcU64(m.MinBaseAmount),
		fcU64(m.MinQuoteAmount),
		fcU64(uint64(m.OrderQuoteLimit)),
		fcU64(uint64(m.TotalOrderCount)),
		fcU64(uint64(m.SizeExtensionMultiplier)),
		fcU64(uint64(m.QuoteExtensionMultiplier)),
		fcU64(r[0]), fcU64(r[1]), fcU64(r[2]), fcU64(r[3]),
	})
}
