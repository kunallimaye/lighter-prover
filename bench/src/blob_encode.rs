// Copyright (c) Elliot Technologies, Inc.
// SPDX-License-Identifier: BUSL-1.1

//! Synthesized-batch -> blob encoder for the L6 inner-wrapper drive path (#83).
//!
//! Lays out a correctly-shaped blob from synthesized batch data following the
//! field offsets in `circuit/src/blob/constants.rs`:
//!
//! ```text
//! [BLOB_VERSION_INDEX        .. BLOB_RESERVED_INDEX)        version (2 bytes, big-endian)
//! [BLOB_RESERVED_INDEX       .. BLOB_MARK_PRICE_INDEX)      reserved (32 bytes, zero)
//! [BLOB_MARK_PRICE_INDEX     .. BLOB_FUNDING_INDEX)         per-market mark price (4 bytes BE each)
//! [BLOB_FUNDING_INDEX        .. BLOB_QUOTE_MULTIPLIER_INDEX) per-market funding (9 bytes each: sign + 2x4 limbs)
//! [BLOB_QUOTE_MULTIPLIER_INDEX .. BLOB_ACCOUNT_OFFSET)      per-market quote multiplier (2 bytes BE each)
//! [BLOB_ACCOUNT_OFFSET       .. BLOB_DATA_BYTES_COUNT)      compressed account-delta leaves
//! ```
//!
//! The version + reserved region MUST be zero: the inner wrapper enforces this
//! in `verify_version_and_reserved_data`. The market region MUST match the
//! batch's `new_public_market_details` (the inner wrapper checks this in
//! `verify_latest_market_data`). The account-leaf region MUST polynomial-encode
//! the aggregated delta (the inner wrapper checks this in
//! `verify_delta_polynomial_evaluation`).
//!
//! For the correctly-shaped *synthesized* batch targeted by #83 we encode an
//! empty batch: version 0, all market slots empty, and an empty account-leaf
//! region (consistent with `EMPTY_ACCOUNT_DELTA_TREE_ROOT` and a degree-0
//! aggregated delta). The encoder is structured so non-empty market/leaf data
//! can be layered in for later issues (#116+).

use circuit::blob::constants::*;

use crate::kzg::MarketLimbs;

/// A correctly-shaped, empty (zero) synthesized blob: version 0, reserved 0,
/// all market slots empty, empty account-leaf region.
///
/// All `BLOB_DATA_BYTES_COUNT` bytes are zero. This is the blob for a batch with
/// `EMPTY_ACCOUNT_DELTA_TREE_ROOT` and no market updates, and is what `#83`'s
/// `--l6-inner` / `--blob-prove` synthesized smoke flow encodes.
pub fn empty_blob() -> Box<[u8; BLOB_DATA_BYTES_COUNT]> {
    Box::new([0u8; BLOB_DATA_BYTES_COUNT])
}

/// The per-market limbs corresponding to [`empty_blob`]: every slot empty.
///
/// `POSITION_LIST_SIZE` empty market slots, matching the all-zero market region
/// of the empty blob and the empty `public_market_details` of an empty batch.
pub fn empty_market_limbs() -> Vec<MarketLimbs> {
    vec![MarketLimbs::default(); circuit::types::constants::POSITION_LIST_SIZE]
}

/// Write a single market slot's mark price into the blob (4 bytes, big-endian).
///
/// Exposed for non-empty encodings layered in by later issues; the #83
/// synthesized flow uses [`empty_blob`].
pub fn write_mark_price(
    blob: &mut [u8; BLOB_DATA_BYTES_COUNT],
    market_index: usize,
    mark_price: u32,
) {
    let off = BLOB_MARK_PRICE_INDEX + market_index * MARK_PRICE_BYTE_SIZE;
    blob[off..off + MARK_PRICE_BYTE_SIZE].copy_from_slice(&mark_price.to_be_bytes());
}

/// Write a single market slot's quote multiplier into the blob (2 bytes, big-endian).
pub fn write_quote_multiplier(
    blob: &mut [u8; BLOB_DATA_BYTES_COUNT],
    market_index: usize,
    quote_multiplier: u16,
) {
    let off = BLOB_QUOTE_MULTIPLIER_INDEX + market_index * QUOTE_MULTIPLIER_BYTE_SIZE;
    blob[off..off + QUOTE_MULTIPLIER_BYTE_SIZE].copy_from_slice(&quote_multiplier.to_be_bytes());
}
