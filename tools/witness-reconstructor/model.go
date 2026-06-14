// Copyright (c) Elliot Technologies, Inc.
// SPDX-License-Identifier: BUSL-1.1

package main

import (
	"encoding/json"
	"fmt"
	"os"
)

// This file models the SPARSE per-tx witness data stored in bench/bench_test.json
// that Phase 0 validates against. We only model the keys this harness consumes;
// the full tx struct is in circuit/src/tx.rs. JSON key -> struct mapping is taken
// directly from the #[serde(rename = ...)] attributes in the circuit source.
//
// Tree depths (constants.rs:408-414):
//   ACCOUNT=48, API_KEY=8, ACCOUNT_ORDERS=60, MARKET=12, ASSET=6, ORDER_BOOK=80.
// Per-tx slot model (constants.rs:234-236, 466-474):
//   NB_ACCOUNTS_PER_TX=3 (OWNER/TAKER=0, MAKER=1, FEE=2), NB_ASSETS_PER_TX=2.

const (
	apiKeyDepth        = 8
	accountOrdersDepth = 60
	marketDepth        = 12
	assetDepth         = 6
	accountDepth       = 48
	orderBookDepth     = 80 // ORDER_PRICE_BITS(32) + ORDER_NONCE_BITS(48), constants.rs:35,37

	orderNonceBits = 48 // ORDER_NONCE_BITS (constants.rs:35)
	orderPriceBits = 32 // ORDER_PRICE_BITS (constants.rs:37)

	ownerAccountID      = 0  // OWNER_ACCOUNT_ID == TAKER_ACCOUNT_ID (constants.rs:466)
	txTypeL2CancelOrder = 15 // TX_TYPE_L2_CANCEL_ORDER
	txTypeL2ModifyOrder = 17 // TX_TYPE_L2_MODIFY_ORDER
	txTypeEmpty         = 0  // TX_TYPE_EMPTY
)

// Block is the top-level bench_test.json document (only fields we use).
type Block struct {
	OldMarketTreeRoot  [4]uint64 `json:"omtr"` // old market tree root
	OldAccountTreeRoot [4]uint64 `json:"oatr"` // old account tree root
	OldStateRoot       [4]uint64 `json:"osr"`  // sample's old/initial state root
	NewStateRoot       [4]uint64 `json:"nsr"`  // sample's final state root (exit criterion)
	Txs                []Tx      `json:"txs"`  // 500 transactions
}

// Tx models the sparse per-tx leaves + Merkle proofs we validate. All proof
// siblings are [4]uint64 limbs (HashOut limb i = field element i, no reversal).
type Tx struct {
	TxType int `json:"tx_type"`

	// --- api_key (single leaf + proof for the owner account) ---
	ApiKeyBefore ApiKeyLeaf  `json:"akb"`   // api_key.rs leaf (aki, pk, n)
	ApiKeyProof  [][4]uint64 `json:"mpakb"` // 8 siblings (depth 8)

	// --- account_order (single leaf; proof is per-account-slot) ---
	AccountOrderBefore AccountOrderLeaf `json:"aob"`   // account_order.rs leaf
	AccountOrdersProof [][][4]uint64    `json:"mpokb"` // [NB_ACCOUNT_ORDERS_PATHS_PER_TX][60][4]

	// --- accounts (3 slots) carry the sub-tree ground-truth roots ---
	AccountsBefore []AccountLeaf `json:"ab"`   // [3] account leaves
	AccountProof   [][][4]uint64 `json:"mpab"` // [3][48][4] account-tree proofs

	// --- asset (3 accounts x 2 assets) ---
	AccountAssetsBefore [][]AccountAssetLeaf `json:"aab"`   // [3][2]
	AssetProof          [][][][4]uint64      `json:"mpaab"` // [3][2][6][4] (private/asset_root)
	PublicAssetProof    [][][][4]uint64      `json:"mpaa"`  // [3][2][6][4] (agg_balances_root)
	AssetIndices        []int                `json:"ai"`    // [2] asset indices

	// --- market (single leaf + proof) ---
	MarketBefore MarketLeaf  `json:"mmb"`   // market.rs leaf
	MarketProof  [][4]uint64 `json:"mpmmb"` // 12 siblings (depth 12)

	// --- cancel-specific (tx_type 15, key 2co) ---
	Cancel        *CancelPayload  `json:"2co"`     // L2CancelOrderTx (l2_cancel_order.rs:26-40)
	OrderInfoBefr OrderInfo       `json:"obinfob"` // order-book Order leaf BEFORE (order.rs:22-40)
	OrderBookPath []OrderBookNode `json:"obpb"`    // [80] order-book proof path (order_book_node.rs:18-33)

	// --- modify-specific (tx_type 17, key 2mo) ---
	// The order-book Order leaf before + path (obinfob/obpb) are SHARED with
	// cancel above. Only the 2mo payload is modify-specific.
	Modify *ModifyPayload `json:"2mo"` // L2ModifyOrderTx (l2_modify_order.rs:34-57)
}

// ModifyPayload models the 2mo tx payload (l2_modify_order.rs:34-57).
type ModifyPayload struct {
	AccountIndex int64  `json:"ai"` // owner account index (48 bits)
	ApiKeyIndex  uint8  `json:"ki"` // api key index (8 bits)
	MarketIndex  uint16 `json:"m"`  // market index (defaults 0 via serde(default))
	Index        int64  `json:"i"`  // cloindex or oindex (56 bits)
	BaseAmount   int64  `json:"b"`  // new base amount (64 bits, may be 0)
	Price        uint32 `json:"p"`  // new price (32 bits)
	TriggerPrice uint32 `json:"tp"` // new trigger price (32 bits)
}

// CancelPayload models the 2co tx payload (l2_cancel_order.rs:26-40).
type CancelPayload struct {
	AccountIndex int64  `json:"ai"` // owner account index
	ApiKeyIndex  uint8  `json:"ki"` // api key index
	MarketIndex  uint16 `json:"m"`  // market index (defaults 0 via serde(default))
	Index        int64  `json:"i"`  // cloindex or oindex
}

// OrderInfo models the order-book Order leaf BEFORE the cancel (order.rs:22-40).
// key_price/key_nonce locate the leaf in the depth-80 order-book tree; the four
// sums are the leaf's aggregation contribution removed on cancel.
type OrderInfo struct {
	KeyPrice    int64 `json:"kp"` // 32 bits (price_index)
	KeyNonce    int64 `json:"kn"` // 48 bits (nonce_index)
	AskBaseSum  int64 `json:"ab"`
	AskQuoteSum int64 `json:"aq"`
	BidBaseSum  int64 `json:"bb"`
	BidQuoteSum int64 `json:"bq"`
}

// OrderBookNode models one level of the obpb proof (order_book_node.rs:18-33):
// the sibling child hash plus the PARENT node's aggregated sums. internal_hash()
// uses the four sums as the 4 HashOut limbs (NOT a Poseidon permutation).
type OrderBookNode struct {
	SiblingHash [4]uint64 `json:"h"`  // sibling_child_hash
	AskBaseSum  int64     `json:"ab"` // parent ask_base_sum
	AskQuoteSum int64     `json:"aq"` // parent ask_quote_sum
	BidBaseSum  int64     `json:"bb"` // parent bid_base_sum
	BidQuoteSum int64     `json:"bq"` // parent bid_quote_sum
}

// ApiKeyLeaf models akb (api_key.rs). pk is a quintic-ext field elem (5 limbs).
type ApiKeyLeaf struct {
	Index     uint64    `json:"aki"`
	PublicKey [5]uint64 `json:"pk"`
	Nonce     int64     `json:"n"`
}

// AccountOrderLeaf models aob (account_order.rs:22-80). Only index_0/index_1/
// owner_account_index are present for empty orders in the sample; the rest
// default to 0 (Go zero value), which matches the circuit's is_empty() path.
type AccountOrderLeaf struct {
	Index0               int64  `json:"i0"` // order index used for the Merkle path
	Index1               int64  `json:"i1"`
	OwnerAccountIndex    int64  `json:"oai"`
	OrderIndex           int64  `json:"oi"`
	ClientOrderIndex     int64  `json:"coi"`
	InitialBaseAmount    int64  `json:"iba"`
	Price                uint32 `json:"p"`
	Nonce                int64  `json:"n"`
	RemainingBaseAmount  int64  `json:"rba"`
	IsAsk                uint8  `json:"a"`
	OrderType            uint8  `json:"t"`
	TimeInForce          uint8  `json:"tif"`
	ReduceOnly           uint8  `json:"ro"`
	TriggerPrice         uint32 `json:"tp"`
	Expiry               int64  `json:"e"`
	TriggerStatus        uint8  `json:"ts"`
	ToTriggerOrderIndex0 int64  `json:"ttoi0"`
	ToTriggerOrderIndex1 int64  `json:"ttoi1"`
	ToCancelOrderIndex0  int64  `json:"tcoi0"`
}

// AccountLeaf models ab[i] (account.rs). We only consume the embedded sub-tree
// ground-truth roots; the full account leaf hash (account_hash.rs) is not
// reconstructed in Phase 0 (it nests these sub-tree roots which we validate
// independently — see the de-risking strategy in #122/the design doc).
type AccountLeaf struct {
	AccountIndex           int64     `json:"ai"`
	ApiKeyRoot             [4]uint64 `json:"akr"` // api_key sub-tree root
	AccountOrdersRoot      [4]uint64 `json:"aor"` // account_orders sub-tree root
	AssetRoot              [4]uint64 `json:"asr"` // asset sub-tree root
	AggregatedBalancesRoot [4]uint64 `json:"abr"` // aggregated-balances sub-tree root
}

// AccountAssetLeaf models aab[acct][asset] (account_asset.rs).
type AccountAssetLeaf struct {
	Index0        int64  `json:"i"`
	Balance       uint64 `json:"b"`
	LockedBalance uint64 `json:"lb"`
	MarginMode    uint8  `json:"mm"`
}

// MarketLeaf models mmb (market.rs). Missing keys default to 0 (Go zero value),
// matching serde(default) in the circuit.
type MarketLeaf struct {
	Index                    uint16    `json:"i"` // path hint only, not in leaf hash
	Status                   uint8     `json:"s"`
	MarketType               uint8     `json:"mt"`
	BaseAssetID              uint16    `json:"ba"`
	QuoteAssetID             uint16    `json:"qa"`
	AskNonce                 int64     `json:"a"`
	BidNonce                 int64     `json:"b"`
	TakerFee                 uint32    `json:"t"`
	MakerFee                 uint32    `json:"m"`
	LiquidationFee           uint32    `json:"l"`
	SizeExtensionMultiplier  int64     `json:"sem"`
	QuoteExtensionMultiplier int64     `json:"qem"`
	TotalOrderCount          int64     `json:"toc"`
	MinBaseAmount            uint64    `json:"mba"`
	MinQuoteAmount           uint64    `json:"ma"`
	OrderQuoteLimit          int64     `json:"oql"`
	OrderBookRoot            [4]uint64 `json:"r"`
}

// loadBlock parses bench_test.json into a Block.
func loadBlock(path string) (*Block, error) {
	f, err := os.Open(path)
	if err != nil {
		return nil, fmt.Errorf("open %s: %w", path, err)
	}
	defer f.Close()

	var b Block
	dec := json.NewDecoder(f)
	if err := dec.Decode(&b); err != nil {
		return nil, fmt.Errorf("decode %s: %w", path, err)
	}
	return &b, nil
}

// toHashOut converts [4]uint64 JSON limbs into a library HashOut.
func toHashOut(l [4]uint64) HashOut { return hashFromLimbs(l) }

// toSiblings converts a JSON [][4]uint64 sibling list into []HashOut.
func toSiblings(raw [][4]uint64) []HashOut {
	out := make([]HashOut, len(raw))
	for i, s := range raw {
		out[i] = hashFromLimbs(s)
	}
	return out
}
