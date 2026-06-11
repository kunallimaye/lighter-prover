// Copyright (c) Elliot Technologies, Inc.
// SPDX-License-Identifier: BUSL-1.1

use anyhow::Result;
use num::BigInt;
use plonky2::field::types::{Field, PrimeField64};
use plonky2::hash::hash_types::{HashOut, HashOutTarget, RichField};
use plonky2::iop::target::{BoolTarget, Target};
use plonky2::iop::witness::Witness;
use serde::Deserialize;
use serde_with::serde_as;

use crate::bigint::bigint::{BigIntTarget, SignTarget};
use crate::bigint::biguint::BigUintTarget;
use crate::block::BlockWitness;
use crate::bool_utils::CircuitBuilderBoolUtils;
use crate::circuit_logger::CircuitBuilderLogging;
use crate::comparison::CircuitBuilderSubtractiveComparison;
use crate::deserializers;
use crate::hash_utils::CircuitBuilderHashUtils;
use crate::keccak::constants::KECCAK_OUTPUT_LENGHT;
use crate::keccak::helpers::keccak;
use crate::keccak::keccak::{CircuitBuilderKeccak, KeccakOutputTarget};
use crate::types::config::{Builder, F};
use crate::types::constants::{KECCAK_HASH_OUT_BYTE_SIZE, POSITION_LIST_SIZE, TIMESTAMP_BITS};
use crate::types::market_details::{
    PublicMarketDetails, PublicMarketDetailsTarget, PublicMarketDetailsWitness,
};
use crate::uint::u8::U8Target;
use crate::uint::u32::gadgets::arithmetic_u32::U32Target;
use crate::utils::CircuitBuilderUtils;

const NEW_PUBLIC_MARKET_DETAILS_INDEX: usize = 24;
const ON_CHAIN_OPERATIONS_PUB_DATA_HASH_INDEX: usize =
    NEW_PUBLIC_MARKET_DETAILS_INDEX + POSITION_LIST_SIZE * 5;
const PRIORITY_OPERATIONS_COUNT_INDEX: usize =
    ON_CHAIN_OPERATIONS_PUB_DATA_HASH_INDEX + KECCAK_OUTPUT_LENGHT;
const OLD_PREFIX_PRIORITY_OPERATION_HASH_INDEX: usize = PRIORITY_OPERATIONS_COUNT_INDEX + 1;
const NEW_PREFIX_PRIORITY_OPERATION_HASH_INDEX: usize =
    OLD_PREFIX_PRIORITY_OPERATION_HASH_INDEX + KECCAK_OUTPUT_LENGHT;
pub const BATCH_TARGET_INDEX: usize =
    NEW_PREFIX_PRIORITY_OPERATION_HASH_INDEX + KECCAK_OUTPUT_LENGHT;
pub const SEGMENT_INFO_INDEX: usize = BATCH_TARGET_INDEX + KECCAK_OUTPUT_LENGHT;

#[serde_as]
#[derive(Debug, Clone, Deserialize)]
#[serde(bound = "")]
/// Public witness that represents aggregated [`crate::block::Block`]. Note that there is no secret witness here
pub struct Batch<F>
where
    F: Field + RichField,
{
    #[serde(rename = "bn")]
    pub end_block_number: u64,

    #[serde(rename = "bs")]
    pub batch_size: u64,

    #[serde(rename = "fca")]
    pub first_created_at: i64,

    #[serde(rename = "lca")]
    pub last_created_at: i64,

    #[serde(rename = "osr")]
    #[serde(deserialize_with = "deserializers::hash_out")]
    pub old_state_root: HashOut<F>,

    #[serde(rename = "nvr")]
    #[serde(deserialize_with = "deserializers::hash_out")]
    pub new_validium_root: HashOut<F>,

    #[serde(rename = "nsr")]
    #[serde(deserialize_with = "deserializers::hash_out")]
    pub new_state_root: HashOut<F>,

    #[serde(rename = "oapdtr")]
    #[serde(deserialize_with = "deserializers::hash_out")]
    pub old_account_delta_tree_root: HashOut<F>,

    #[serde(rename = "napdtr")]
    #[serde(deserialize_with = "deserializers::hash_out")]
    pub new_account_delta_tree_root: HashOut<F>,

    #[serde(rename = "ocpdh")]
    #[serde(deserialize_with = "deserializers::hex_to_bytes")]
    pub on_chain_operations_pub_data_hash: [u8; KECCAK_HASH_OUT_BYTE_SIZE],

    #[serde(rename = "poc")]
    pub priority_operations_count: u64,

    #[serde(rename = "oppoh")]
    #[serde(deserialize_with = "deserializers::hex_to_bytes")]
    pub old_prefix_priority_operation_hash: [u8; KECCAK_HASH_OUT_BYTE_SIZE],

    #[serde(rename = "nppoh")]
    #[serde(deserialize_with = "deserializers::hex_to_bytes")]
    pub new_prefix_priority_operation_hash: [u8; KECCAK_HASH_OUT_BYTE_SIZE],

    #[serde(rename = "pmda")]
    #[serde_as(as = "[_; POSITION_LIST_SIZE]")]
    pub new_public_market_details: [PublicMarketDetails; POSITION_LIST_SIZE],
}

impl<F> Default for Batch<F>
where
    F: Field + RichField,
{
    fn default() -> Self {
        Self {
            end_block_number: 0,
            batch_size: 0,
            first_created_at: 0,
            last_created_at: 0,
            old_state_root: HashOut::<F>::default(),
            new_validium_root: HashOut::<F>::default(),
            new_state_root: HashOut::<F>::default(),
            old_account_delta_tree_root: HashOut::<F>::default(),
            new_account_delta_tree_root: HashOut::<F>::default(),
            on_chain_operations_pub_data_hash: [0; KECCAK_HASH_OUT_BYTE_SIZE],
            priority_operations_count: 0,
            old_prefix_priority_operation_hash: [0; KECCAK_HASH_OUT_BYTE_SIZE],
            new_prefix_priority_operation_hash: [0; KECCAK_HASH_OUT_BYTE_SIZE],
            new_public_market_details: core::array::from_fn(|_| PublicMarketDetails::default()),
        }
    }
}

impl<F> Batch<F>
where
    F: Field + RichField,
{
    /// Parse public inputs from proof into Batch
    pub fn from_public_inputs(pis: &[F]) -> Self {
        Self {
            end_block_number: pis[0].to_canonical_u64(),
            batch_size: pis[1].to_canonical_u64(),

            first_created_at: pis[2].to_canonical_u64() as i64,
            last_created_at: pis[3].to_canonical_u64() as i64,

            old_state_root: HashOut::<F>::from([pis[4], pis[5], pis[6], pis[7]]),
            new_validium_root: HashOut::<F>::from([pis[8], pis[9], pis[10], pis[11]]),
            new_state_root: HashOut::<F>::from([pis[12], pis[13], pis[14], pis[15]]),

            old_account_delta_tree_root: HashOut::<F>::from([pis[16], pis[17], pis[18], pis[19]]),
            new_account_delta_tree_root: HashOut::<F>::from([pis[20], pis[21], pis[22], pis[23]]),

            new_public_market_details: pis
                [NEW_PUBLIC_MARKET_DETAILS_INDEX..ON_CHAIN_OPERATIONS_PUB_DATA_HASH_INDEX]
                .chunks(5)
                .map(|chunk| {
                    let mut funding_rate_prefix_sum_abs =
                        (chunk[1].to_canonical_u64() + (chunk[2].to_canonical_u64() << 32)) as i64;
                    if !chunk[0].is_one() && !chunk[0].is_zero() {
                        funding_rate_prefix_sum_abs *= -1;
                    }
                    PublicMarketDetails {
                        funding_rate_prefix_sum: BigInt::from(funding_rate_prefix_sum_abs),
                        mark_price: chunk[3].to_canonical_u64() as u32,
                        quote_multiplier: chunk[4].to_canonical_u64() as u32,
                    }
                })
                .collect::<Vec<_>>()
                .try_into()
                .unwrap(),

            on_chain_operations_pub_data_hash: core::array::from_fn(|i| {
                pis[ON_CHAIN_OPERATIONS_PUB_DATA_HASH_INDEX + i].to_canonical_u64() as u8
            }),

            priority_operations_count: pis[PRIORITY_OPERATIONS_COUNT_INDEX].to_canonical_u64(),
            old_prefix_priority_operation_hash: core::array::from_fn(|i| {
                pis[OLD_PREFIX_PRIORITY_OPERATION_HASH_INDEX + i].to_canonical_u64() as u8
            }),
            new_prefix_priority_operation_hash: core::array::from_fn(|i| {
                pis[NEW_PREFIX_PRIORITY_OPERATION_HASH_INDEX + i].to_canonical_u64() as u8
            }),
        }
    }

    pub fn aggregate_block(&mut self, current_block: &BlockWitness<F>) {
        if self.batch_size > 0 {
            assert_eq!(
                self.end_block_number + 1,
                current_block.block_number,
                "current block is not next block"
            );
            assert_eq!(
                self.new_state_root, current_block.old_state_root,
                "current block's old state root is not equal to last new state root"
            );
        }
        assert!(
            current_block.created_at >= self.last_created_at,
            "current block created_at is less than last created_at"
        );

        self.aggregate_on_chain_operations_pub_data(current_block);
        self.aggregate_priority_operations_pub_data(current_block);

        if self.batch_size == 0 {
            // for first block use current blocks timestamp
            self.first_created_at = current_block.created_at;
            self.old_state_root = current_block.old_state_root;
            self.old_account_delta_tree_root = current_block.old_account_delta_tree_root;
        }
        self.end_block_number = current_block.block_number;
        self.last_created_at = current_block.created_at;
        self.new_validium_root = current_block.new_validium_root;
        self.new_state_root = current_block.new_state_root;
        self.new_account_delta_tree_root = current_block.new_account_delta_tree_root;
        self.new_public_market_details = current_block.new_public_market_details.clone();
        self.batch_size += 1;
    }

    fn aggregate_priority_operations_pub_data(&mut self, current_block: &BlockWitness<F>) {
        self.priority_operations_count += current_block.priority_operations_count;
        if self.batch_size == 0 {
            self.old_prefix_priority_operation_hash =
                current_block.old_prefix_priority_operation_hash;
        } else {
            assert_eq!(
                self.new_prefix_priority_operation_hash,
                current_block.old_prefix_priority_operation_hash,
                "current block's old prefix priority operation hash is not equal to last new prefix priority operation hash"
            );
        }
        self.new_prefix_priority_operation_hash = current_block.new_prefix_priority_operation_hash;
    }

    fn aggregate_on_chain_operations_pub_data(&mut self, current_block: &BlockWitness<F>) {
        // Calculate new on chain operations hash. For first iteration, `old_batch.on_chain_operations_pub_data_hash` is
        // zero keccak output(ie. full of zero bits)
        if current_block.on_chain_operations_count == 0 {
            return;
        }

        current_block
            .on_chain_operations_pub_data
            .iter()
            .enumerate()
            .for_each(|(i, current_on_chain_operations_pub_data)| {
                if i > current_block.on_chain_operations_count as usize {
                    return;
                }

                let in_1 = self.on_chain_operations_pub_data_hash;
                let in_2 = current_on_chain_operations_pub_data;

                // println!("self.on_chain_operations_pub_data_hash: {:?} ", in_1);
                // println!("current_on_chain_operations_pub_data: {:?}", in_2);

                let mut on_chain_operations_pub_data_input = vec![];

                on_chain_operations_pub_data_input.extend_from_slice(&in_1);
                on_chain_operations_pub_data_input.extend_from_slice(in_2);

                self.on_chain_operations_pub_data_hash =
                    keccak(&on_chain_operations_pub_data_input);

                // println!("RESULT: {:?}", self.on_chain_operations_pub_data_hash);
            });
    }

    /// Issue #82: host-side pairwise merge mirroring the in-circuit
    /// [`BatchTarget::conditionally_merge_consecutive`] semantics (with the
    /// circuit's `cond = true` path). Merges `self` (the earlier, left range)
    /// with `right` (the later range) into the combined range, asserting the
    /// same continuity invariants the circuit enforces:
    ///
    /// * contiguity: `right`'s start block (`end_block_number - batch_size`)
    ///   equals `self.end_block_number`;
    /// * monotonic timestamps: `self.last_created_at <= right.first_created_at`;
    /// * state-root continuity: `self.new_state_root == right.old_state_root`;
    /// * account-delta-root continuity:
    ///   `self.new_account_delta_tree_root == right.old_account_delta_tree_root`;
    /// * priority-op keccak prefix chain:
    ///   `self.new_prefix_priority_operation_hash ==
    ///   right.old_prefix_priority_operation_hash`.
    ///
    /// The merged output takes `old_*`/start fields from the left child and
    /// `new_*`/end fields from the right child, summing the counts — the
    /// host mirror of the circuit's `select(cond, b, a)` rules.
    ///
    /// The on-chain-ops pub-data hash is taken from the right child (its
    /// running hash already incorporates the left child's chain via the seam
    /// stitch enforced by [`SegmentInfo::stitch`]); this mirrors the
    /// `select_keccak_output(cond, b.., a..)` for that field.
    pub fn merge_consecutive(&self, right: &Self) -> Self {
        let right_start_block = right.end_block_number.saturating_sub(right.batch_size);
        assert_eq!(
            self.end_block_number, right_start_block,
            "Batch::merge_consecutive: right child's start block ({}) is not contiguous with \
             left child's end block ({})",
            right_start_block, self.end_block_number
        );

        assert!(
            self.last_created_at <= right.first_created_at,
            "Batch::merge_consecutive: timestamps not monotonic across the seam \
             (left.last_created_at {} > right.first_created_at {})",
            self.last_created_at,
            right.first_created_at
        );

        assert_eq!(
            self.new_state_root, right.old_state_root,
            "Batch::merge_consecutive: left.new_state_root != right.old_state_root"
        );

        assert_eq!(
            self.new_account_delta_tree_root, right.old_account_delta_tree_root,
            "Batch::merge_consecutive: left.new_account_delta_tree_root != \
             right.old_account_delta_tree_root"
        );

        assert_eq!(
            self.new_prefix_priority_operation_hash, right.old_prefix_priority_operation_hash,
            "Batch::merge_consecutive: priority-op keccak prefix chain broken across the seam \
             (left.new_prefix_priority_operation_hash != \
             right.old_prefix_priority_operation_hash)"
        );

        Self {
            end_block_number: right.end_block_number,
            batch_size: self.batch_size + right.batch_size,

            first_created_at: self.first_created_at,
            last_created_at: right.last_created_at,

            old_state_root: self.old_state_root,
            new_validium_root: right.new_validium_root,
            new_state_root: right.new_state_root,

            old_account_delta_tree_root: self.old_account_delta_tree_root,
            new_account_delta_tree_root: right.new_account_delta_tree_root,

            // The running on-chain-ops hash flows from the right child (its
            // chain already includes the left child's, via the seam stitch).
            on_chain_operations_pub_data_hash: right.on_chain_operations_pub_data_hash,

            priority_operations_count: self.priority_operations_count
                + right.priority_operations_count,
            old_prefix_priority_operation_hash: self.old_prefix_priority_operation_hash,
            new_prefix_priority_operation_hash: right.new_prefix_priority_operation_hash,

            new_public_market_details: right.new_public_market_details.clone(),
        }
    }

    /// Validate-only host mirror of `merge_consecutive` for callers that only
    /// need to check whether two batches form a contiguous, mergeable pair
    /// (e.g. driver-side A/B sanity checks). Returns `Err` instead of
    /// panicking on a broken seam.
    pub fn try_merge_consecutive(&self, right: &Self) -> Result<Self> {
        let right_start_block = right.end_block_number.saturating_sub(right.batch_size);
        if self.end_block_number != right_start_block {
            anyhow::bail!(
                "Batch::try_merge_consecutive: non-contiguous block ranges (left end {}, right start {})",
                self.end_block_number,
                right_start_block
            );
        }
        if self.last_created_at > right.first_created_at {
            anyhow::bail!("Batch::try_merge_consecutive: non-monotonic timestamps across the seam");
        }
        if self.new_state_root != right.old_state_root {
            anyhow::bail!("Batch::try_merge_consecutive: state-root discontinuity across the seam");
        }
        if self.new_account_delta_tree_root != right.old_account_delta_tree_root {
            anyhow::bail!(
                "Batch::try_merge_consecutive: account-delta-root discontinuity across the seam"
            );
        }
        if self.new_prefix_priority_operation_hash != right.old_prefix_priority_operation_hash {
            anyhow::bail!(
                "Batch::try_merge_consecutive: priority-op keccak prefix chain broken across the seam"
            );
        }
        Ok(self.merge_consecutive(right))
    }
}

#[derive(Debug, Clone)]
/// BatchTarget represents result of aggregation in circuit. Each field is public
pub struct BatchTarget {
    pub end_block_number: Target,
    pub batch_size: Target,

    pub start_timestamp: Target,
    pub end_timestamp: Target,

    pub old_state_root: HashOutTarget,
    pub new_validium_root: HashOutTarget,
    pub new_state_root: HashOutTarget,

    pub old_account_delta_tree_root: HashOutTarget,
    pub new_account_delta_tree_root: HashOutTarget,

    pub on_chain_operations_pub_data_hash: KeccakOutputTarget,

    pub priority_operations_count: Target,
    pub old_prefix_priority_operation_hash: KeccakOutputTarget,
    pub new_prefix_priority_operation_hash: KeccakOutputTarget,

    pub new_public_market_details: [PublicMarketDetailsTarget; POSITION_LIST_SIZE],
}

impl BatchTarget {
    /// Initialize BatchTarget with public virtual targets
    pub fn new_public(builder: &mut Builder) -> Self {
        Self {
            end_block_number: builder.add_virtual_public_input(),
            batch_size: builder.add_virtual_public_input(),

            start_timestamp: builder.add_virtual_public_input(),
            end_timestamp: builder.add_virtual_public_input(),

            old_state_root: builder.add_virtual_hash_public_input(),
            new_validium_root: builder.add_virtual_hash_public_input(),
            new_state_root: builder.add_virtual_hash_public_input(),

            old_account_delta_tree_root: builder.add_virtual_hash_public_input(),
            new_account_delta_tree_root: builder.add_virtual_hash_public_input(),

            new_public_market_details: core::array::from_fn(|_| {
                PublicMarketDetailsTarget::new_public(builder)
            }),

            // safe because in first recursion it is read form segment info which is range-checked, and next iterations it is read from previous batch proof which is also safe
            on_chain_operations_pub_data_hash: builder
                .add_virtual_keccak_output_public_input_unsafe(),

            priority_operations_count: builder.add_virtual_public_input(),
            // safe because in first recursion it is read form block circuit proof which is range-checked, and next iterations it is read from previous batch proof which is also safe
            old_prefix_priority_operation_hash: builder
                .add_virtual_keccak_output_public_input_unsafe(),
            // Safe because it is connected to public witness from constrained circuit
            new_prefix_priority_operation_hash: builder
                .add_virtual_keccak_output_public_input_unsafe(),
        }
    }

    /// Parse public inputs from proof into Batch
    pub fn from_public_inputs(pis: &[Target]) -> Self {
        Self {
            end_block_number: pis[0],
            batch_size: pis[1],

            start_timestamp: pis[2],
            end_timestamp: pis[3],

            old_state_root: HashOutTarget {
                elements: [pis[4], pis[5], pis[6], pis[7]],
            },
            new_validium_root: HashOutTarget {
                elements: [pis[8], pis[9], pis[10], pis[11]],
            },
            new_state_root: HashOutTarget {
                elements: [pis[12], pis[13], pis[14], pis[15]],
            },

            old_account_delta_tree_root: HashOutTarget {
                elements: [pis[16], pis[17], pis[18], pis[19]],
            },
            new_account_delta_tree_root: HashOutTarget {
                elements: [pis[20], pis[21], pis[22], pis[23]],
            },

            new_public_market_details: pis
                [NEW_PUBLIC_MARKET_DETAILS_INDEX..ON_CHAIN_OPERATIONS_PUB_DATA_HASH_INDEX]
                .chunks(5)
                .map(|chunk| PublicMarketDetailsTarget {
                    funding_rate_prefix_sum: BigIntTarget {
                        sign: SignTarget::new_unsafe(chunk[0]),
                        abs: BigUintTarget {
                            limbs: vec![U32Target(chunk[1]), U32Target(chunk[2])],
                        },
                    },
                    mark_price: chunk[3],
                    quote_multiplier: chunk[4],
                })
                .collect::<Vec<_>>()
                .try_into()
                .unwrap(),

            on_chain_operations_pub_data_hash: core::array::from_fn(|i| {
                U8Target(pis[ON_CHAIN_OPERATIONS_PUB_DATA_HASH_INDEX + i])
            }),

            priority_operations_count: pis[PRIORITY_OPERATIONS_COUNT_INDEX],
            old_prefix_priority_operation_hash: core::array::from_fn(|i| {
                U8Target(pis[OLD_PREFIX_PRIORITY_OPERATION_HASH_INDEX + i])
            }),
            new_prefix_priority_operation_hash: core::array::from_fn(|i| {
                U8Target(pis[NEW_PREFIX_PRIORITY_OPERATION_HASH_INDEX + i])
            }),
        }
    }

    /// Merges two consecutive BatchTargets
    pub fn conditionally_merge_consecutive(
        builder: &mut Builder,
        cond: BoolTarget,
        a: &Self,
        b: &Self,
    ) -> Self {
        // end_block_number and batch_size
        let b_start_point = builder.sub(b.end_block_number, b.batch_size);
        builder.conditional_assert_eq(cond, a.end_block_number, b_start_point);

        // Timestamp
        builder.conditional_assert_lte(cond, a.end_timestamp, b.start_timestamp, TIMESTAMP_BITS);

        // State roots
        builder.conditional_assert_eq_hash(cond, &a.new_state_root, &b.old_state_root);

        // Account pub data delta tree roots
        builder.conditional_assert_eq_hash(
            cond,
            &a.new_account_delta_tree_root,
            &b.old_account_delta_tree_root,
        );

        // Priority operations pub data hash
        builder.conditional_assert_eq_keccak_output(
            cond,
            a.new_prefix_priority_operation_hash,
            b.old_prefix_priority_operation_hash,
        );

        Self {
            end_block_number: builder.select(cond, b.end_block_number, a.end_block_number),
            batch_size: builder.mul_add(cond.target, b.batch_size, a.batch_size),

            start_timestamp: a.start_timestamp,
            end_timestamp: builder.select(cond, b.end_timestamp, a.end_timestamp),

            old_state_root: a.old_state_root,
            new_validium_root: builder.select_hash(
                cond,
                &b.new_validium_root,
                &a.new_validium_root,
            ),
            new_state_root: builder.select_hash(cond, &b.new_state_root, &a.new_state_root),
            old_account_delta_tree_root: a.old_account_delta_tree_root,
            new_account_delta_tree_root: builder.select_hash(
                cond,
                &b.new_account_delta_tree_root,
                &a.new_account_delta_tree_root,
            ),

            new_public_market_details: core::array::from_fn(|i| {
                PublicMarketDetailsTarget::select(
                    builder,
                    cond,
                    &b.new_public_market_details[i],
                    &a.new_public_market_details[i],
                )
            }),

            on_chain_operations_pub_data_hash: builder.select_keccak_output(
                cond,
                b.on_chain_operations_pub_data_hash,
                a.on_chain_operations_pub_data_hash,
            ),

            priority_operations_count: builder.mul_add(
                cond.target,
                b.priority_operations_count,
                a.priority_operations_count,
            ),
            old_prefix_priority_operation_hash: a.old_prefix_priority_operation_hash,
            new_prefix_priority_operation_hash: builder.select_keccak_output(
                cond,
                b.new_prefix_priority_operation_hash,
                a.new_prefix_priority_operation_hash,
            ),
        }
    }

    /// `new_account_delta_tree_root` will be taken from the aggregated block, so it
    /// won't be empty when this function is called.
    pub fn is_empty_for_recursion(&self, builder: &mut Builder) -> BoolTarget {
        let assertions = [
            builder.is_zero(self.end_block_number),
            builder.is_zero(self.batch_size),
            builder.is_zero(self.start_timestamp),
            builder.is_zero(self.end_timestamp),
            builder.is_zero_hash_out(&self.old_state_root),
            builder.is_zero_hash_out(&self.new_validium_root),
            builder.is_zero_hash_out(&self.new_state_root),
            builder.is_zero_hash_out(&self.old_account_delta_tree_root),
            builder.is_zero_keccak_output(self.on_chain_operations_pub_data_hash),
            builder.is_zero(self.priority_operations_count),
            builder.is_zero_keccak_output(self.old_prefix_priority_operation_hash),
            builder.is_zero_keccak_output(self.new_prefix_priority_operation_hash),
        ];

        builder.multi_and(&assertions)
    }

    pub fn connect_batches(&self, builder: &mut Builder, other: &Self) {
        builder.connect(self.end_block_number, other.end_block_number);
        builder.connect(self.batch_size, other.batch_size);
        builder.connect(self.start_timestamp, other.start_timestamp);
        builder.connect(self.end_timestamp, other.end_timestamp);

        builder.connect_hashes(self.new_validium_root, other.new_validium_root);
        builder.connect_hashes(self.old_state_root, other.old_state_root);
        builder.connect_hashes(self.new_validium_root, other.new_validium_root);
        builder.connect_hashes(self.new_state_root, other.new_state_root);

        builder.connect_hashes(
            self.old_account_delta_tree_root,
            other.old_account_delta_tree_root,
        );
        builder.connect_hashes(
            self.new_account_delta_tree_root,
            other.new_account_delta_tree_root,
        );

        builder.connect_keccak_output(
            self.on_chain_operations_pub_data_hash,
            other.on_chain_operations_pub_data_hash,
        );

        builder.connect(
            self.priority_operations_count,
            other.priority_operations_count,
        );
        builder.connect_keccak_output(
            self.old_prefix_priority_operation_hash,
            other.old_prefix_priority_operation_hash,
        );
        builder.connect_keccak_output(
            self.new_prefix_priority_operation_hash,
            other.new_prefix_priority_operation_hash,
        );
    }

    pub fn print(&self, builder: &mut Builder, log: &str) {
        builder.println(self.end_block_number, &format!("{} end_block_number", log));
        builder.println(self.batch_size, &format!("{} batch_size", log));
        builder.println(self.start_timestamp, &format!("{} first_created_at", log));
        builder.println(self.end_timestamp, &format!("{} last_created_at", log));
        builder.println_hash_out(&self.old_state_root, &format!("{} old_state_root", log));
        builder.println_hash_out(
            &self.new_validium_root,
            &format!("{} new_validium_root", log),
        );
        builder.println_hash_out(&self.new_state_root, &format!("{} new_state_root", log));
        builder.println_hash_out(
            &self.old_account_delta_tree_root,
            &format!("{} old_account_delta_tree_root", log),
        );
        builder.println_hash_out(
            &self.new_account_delta_tree_root,
            &format!("{} new_account_delta_tree_root", log),
        );
        builder.println_keccak_output(
            &self.on_chain_operations_pub_data_hash,
            &format!("{} on_chain_operations_pub_data_hash", log),
        );
        builder.println(
            self.priority_operations_count,
            &format!("{} priority_operations_count", log),
        );
        builder.println_keccak_output(
            &self.old_prefix_priority_operation_hash,
            &format!("{} old_prefix_priority_operation_hash", log),
        );
        builder.println_keccak_output(
            &self.new_prefix_priority_operation_hash,
            &format!("{} new_prefix_priority_operation_hash", log),
        );
    }
}

pub trait BatchTargetWitness<F: PrimeField64 + RichField> {
    fn set_batch_target(&mut self, a: &BatchTarget, b: &Batch<F>) -> Result<()>;
}

impl<T: Witness<F>, F: PrimeField64 + RichField> BatchTargetWitness<F> for T {
    fn set_batch_target(&mut self, a: &BatchTarget, b: &Batch<F>) -> Result<()> {
        self.set_target(
            a.end_block_number,
            F::from_canonical_u64(b.end_block_number),
        )?;

        self.set_target(a.batch_size, F::from_canonical_u64(b.batch_size))?;

        self.set_target(a.start_timestamp, F::from_canonical_i64(b.first_created_at))?;
        self.set_target(a.end_timestamp, F::from_canonical_i64(b.last_created_at))?;

        self.set_hash_target(a.old_state_root, b.old_state_root)?;
        self.set_hash_target(a.new_validium_root, b.new_validium_root)?;
        self.set_hash_target(a.new_state_root, b.new_state_root)?;

        self.set_hash_target(a.old_account_delta_tree_root, b.old_account_delta_tree_root)?;
        self.set_hash_target(a.new_account_delta_tree_root, b.new_account_delta_tree_root)?;

        for i in 0..KECCAK_HASH_OUT_BYTE_SIZE {
            self.set_target(
                a.on_chain_operations_pub_data_hash[i].0,
                F::from_canonical_u8(b.on_chain_operations_pub_data_hash[i]),
            )?;
        }

        self.set_target(
            a.priority_operations_count,
            F::from_canonical_u64(b.priority_operations_count),
        )?;
        for i in 0..KECCAK_HASH_OUT_BYTE_SIZE {
            self.set_target(
                a.old_prefix_priority_operation_hash[i].0,
                F::from_canonical_u8(b.old_prefix_priority_operation_hash[i]),
            )?;
            self.set_target(
                a.new_prefix_priority_operation_hash[i].0,
                F::from_canonical_u8(b.new_prefix_priority_operation_hash[i]),
            )?;
        }

        a.new_public_market_details
            .iter()
            .zip(b.new_public_market_details.iter())
            .try_for_each(|(t, mi)| self.set_public_market_details_target(t, mi))?;

        Ok(())
    }
}

#[serde_as]
#[derive(Debug, Clone, Deserialize, Eq, PartialEq)]
#[serde(bound = "")]
pub struct SegmentInfo {
    #[serde(rename = "oocpdh")]
    #[serde(deserialize_with = "deserializers::hex_to_bytes")]
    pub old_on_chain_operations_pub_data_hash: [u8; KECCAK_HASH_OUT_BYTE_SIZE],
}

impl Default for SegmentInfo {
    fn default() -> Self {
        Self {
            old_on_chain_operations_pub_data_hash: [0; KECCAK_HASH_OUT_BYTE_SIZE],
        }
    }
}

impl SegmentInfo {
    /// Parse public inputs from proof into SegmentInfo
    pub fn from_public_inputs(pis: &[F]) -> Self {
        Self {
            old_on_chain_operations_pub_data_hash: core::array::from_fn(|i| {
                u8::try_from(pis[i].to_canonical_u64())
                    .expect("Failed to convert old_on_chain_operations_pub_data_hash limb to u8")
            }),
        }
    }

    pub fn to_public_inputs(&self) -> Vec<F> {
        let mut public_inputs = vec![];

        public_inputs.extend_from_slice(
            &self
                .old_on_chain_operations_pub_data_hash
                .iter()
                .map(|&b| F::from_canonical_u8(b))
                .collect::<Vec<F>>(),
        );

        public_inputs
    }

    /// Issue #82: host-side mirror of the in-circuit
    /// [`SegmentInfoTarget::connect_segments`] start-digest stitch (escape
    /// hatch iii). Stitches the left segment (`self`) to the right segment
    /// (`right`) given the left child's running on-chain-ops hash at the seam
    /// (`left_running_on_chain_ops_hash`): the right segment's start digest
    /// must equal the left child's running hash, and the merged segment
    /// carries the LEFT child's start digest so the on-chain-ops hash chain
    /// stays anchored at the segment's true start across merges.
    ///
    /// This is what makes the on-chain-ops keccak chain associative across the
    /// tree-fold (ADR-0003 §D5): the same stitch L6 performs at
    /// `wrapper_circuit.rs:196-200`. Panics on a broken seam.
    pub fn stitch(
        &self,
        right: &Self,
        left_running_on_chain_ops_hash: &[u8; KECCAK_HASH_OUT_BYTE_SIZE],
    ) -> Self {
        assert_eq!(
            *left_running_on_chain_ops_hash, right.old_on_chain_operations_pub_data_hash,
            "SegmentInfo::stitch: on-chain-ops seam broken (left running hash != right segment \
             start digest)"
        );
        Self {
            old_on_chain_operations_pub_data_hash: self.old_on_chain_operations_pub_data_hash,
        }
    }

    /// Validate-only host mirror of `stitch` that returns `Err` instead of
    /// panicking on a broken seam.
    pub fn try_stitch(
        &self,
        right: &Self,
        left_running_on_chain_ops_hash: &[u8; KECCAK_HASH_OUT_BYTE_SIZE],
    ) -> Result<Self> {
        if *left_running_on_chain_ops_hash != right.old_on_chain_operations_pub_data_hash {
            anyhow::bail!(
                "SegmentInfo::try_stitch: on-chain-ops seam broken (left running hash != right \
                 segment start digest)"
            );
        }
        Ok(self.stitch(right, left_running_on_chain_ops_hash))
    }
}

#[derive(Debug, Clone)]
/// BatchTarget represents result of aggregation in circuit. Each field is public
pub struct SegmentInfoTarget {
    pub old_on_chain_operations_pub_data_hash: KeccakOutputTarget,
}

impl SegmentInfoTarget {
    /// Initialize SegmentInfoTarget with public virtual targets
    pub fn new_public(builder: &mut Builder) -> Self {
        Self {
            old_on_chain_operations_pub_data_hash: builder
                .add_virtual_keccak_output_public_input_safe(), // needs to be safe even read from public input because initial proof is dummy
        }
    }

    /// Parse public inputs from proof into SegmentTarget
    /// Only pass the section of the public inputs that correspond to a SegmentTarget
    pub fn from_public_inputs(pis: &[Target]) -> Self {
        Self {
            old_on_chain_operations_pub_data_hash: core::array::from_fn(|i| U8Target(pis[i])),
        }
    }

    pub fn is_empty(&self, builder: &mut Builder) -> BoolTarget {
        let assertions =
            [builder.is_zero_keccak_output(self.old_on_chain_operations_pub_data_hash)];

        builder.multi_and(&assertions)
    }

    pub fn connect_segments(&self, builder: &mut Builder, other: &Self) {
        builder.connect_keccak_output(
            self.old_on_chain_operations_pub_data_hash,
            other.old_on_chain_operations_pub_data_hash,
        );
    }

    pub fn print(&self, builder: &mut Builder, log: &str) {
        builder.println_keccak_output(
            &self.old_on_chain_operations_pub_data_hash,
            &format!("{} old_on_chain_operations_pub_data_hash", log),
        );
    }
}

pub trait SegmentInfoTargetWitness<F: PrimeField64 + RichField> {
    fn set_segment_info_target(&mut self, a: &SegmentInfoTarget, b: &SegmentInfo) -> Result<()>;
}

impl<T: Witness<F>, F: PrimeField64 + RichField> SegmentInfoTargetWitness<F> for T {
    fn set_segment_info_target(&mut self, a: &SegmentInfoTarget, b: &SegmentInfo) -> Result<()> {
        for i in 0..KECCAK_HASH_OUT_BYTE_SIZE {
            self.set_target(
                a.old_on_chain_operations_pub_data_hash[i].0,
                F::from_canonical_u8(b.old_on_chain_operations_pub_data_hash[i]),
            )?;
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    //! Issue #82: host-side unit tests for the pre-L5 merge mirror. These
    //! exercise the plain-Rust `Batch::merge_consecutive` / `SegmentInfo::stitch`
    //! mirrors of the in-circuit `BatchTarget::conditionally_merge_consecutive`
    //! / `SegmentInfoTarget::connect_segments` semantics, with NO plonky2
    //! proving — they run under a plain `cargo test -p circuit recursion::batch`.

    use plonky2::field::goldilocks_field::GoldilocksField;
    use plonky2::hash::hash_types::HashOut;

    use super::*;

    type TF = GoldilocksField;

    fn h(seed: u64) -> HashOut<TF> {
        HashOut::<TF>::from([
            TF::from_canonical_u64(seed),
            TF::from_canonical_u64(seed + 1),
            TF::from_canonical_u64(seed + 2),
            TF::from_canonical_u64(seed + 3),
        ])
    }

    fn k(seed: u8) -> [u8; KECCAK_HASH_OUT_BYTE_SIZE] {
        core::array::from_fn(|i| seed.wrapping_add(i as u8))
    }

    /// Build a single-block batch (batch_size = 1) covering `block_number`.
    fn one_block_batch(
        block_number: u64,
        created_at: i64,
        old_state_root: HashOut<TF>,
        new_state_root: HashOut<TF>,
        old_delta: HashOut<TF>,
        new_delta: HashOut<TF>,
        old_priority_prefix: [u8; KECCAK_HASH_OUT_BYTE_SIZE],
        new_priority_prefix: [u8; KECCAK_HASH_OUT_BYTE_SIZE],
        on_chain_hash: [u8; KECCAK_HASH_OUT_BYTE_SIZE],
        priority_count: u64,
    ) -> Batch<TF> {
        Batch {
            end_block_number: block_number,
            batch_size: 1,
            first_created_at: created_at,
            last_created_at: created_at,
            old_state_root,
            new_validium_root: h(900 + block_number),
            new_state_root,
            old_account_delta_tree_root: old_delta,
            new_account_delta_tree_root: new_delta,
            on_chain_operations_pub_data_hash: on_chain_hash,
            priority_operations_count: priority_count,
            old_prefix_priority_operation_hash: old_priority_prefix,
            new_prefix_priority_operation_hash: new_priority_prefix,
            new_public_market_details: core::array::from_fn(|_| PublicMarketDetails::default()),
        }
    }

    #[test]
    fn merge_two_consecutive_batches_field_correctness() {
        // Left covers block 5 (start block 4 -> end 5), right covers block 6.
        let mid_state = h(50);
        let mid_delta = h(60);
        let mid_prio = k(7);

        let left = one_block_batch(
            5,
            100,
            h(40),     // old_state_root
            mid_state, // new_state_root (== right.old_state_root)
            h(45),     // old_delta
            mid_delta, // new_delta (== right.old_delta)
            k(1),      // old_priority_prefix
            mid_prio,  // new_priority_prefix (== right.old_priority_prefix)
            k(20),     // on_chain hash
            2,         // priority count
        );
        let right = one_block_batch(
            6,
            150,
            mid_state, // old_state_root (== left.new_state_root)
            h(70),     // new_state_root
            mid_delta, // old_delta (== left.new_delta)
            h(80),     // new_delta
            mid_prio,  // old_priority_prefix (== left.new_priority_prefix)
            k(9),      // new_priority_prefix
            k(30),     // on_chain hash (right's running chain)
            3,         // priority count
        );

        let merged = left.merge_consecutive(&right);

        // Combined range: old_* from left, new_* from right, counts summed.
        assert_eq!(merged.end_block_number, 6);
        assert_eq!(merged.batch_size, 2);
        assert_eq!(merged.first_created_at, 100);
        assert_eq!(merged.last_created_at, 150);
        assert_eq!(merged.old_state_root, h(40));
        assert_eq!(merged.new_state_root, h(70));
        assert_eq!(merged.old_account_delta_tree_root, h(45));
        assert_eq!(merged.new_account_delta_tree_root, h(80));
        assert_eq!(merged.new_validium_root, right.new_validium_root);
        // Priority counts sum; old prefix from left, new prefix from right.
        assert_eq!(merged.priority_operations_count, 5);
        assert_eq!(merged.old_prefix_priority_operation_hash, k(1));
        assert_eq!(merged.new_prefix_priority_operation_hash, k(9));
        // On-chain-ops running hash flows from the right child.
        assert_eq!(merged.on_chain_operations_pub_data_hash, k(30));

        // Contiguous start block: combined start == left start (4).
        assert_eq!(
            merged.end_block_number - merged.batch_size,
            left.end_block_number - left.batch_size
        );
    }

    #[test]
    #[should_panic(expected = "not contiguous")]
    fn merge_rejects_non_contiguous_block_ranges() {
        let mid_state = h(50);
        let mid_delta = h(60);
        let mid_prio = k(7);
        let left = one_block_batch(
            5,
            100,
            h(40),
            mid_state,
            h(45),
            mid_delta,
            k(1),
            mid_prio,
            k(20),
            0,
        );
        // Right covers block 8 (gap after 5): start block 7 != left end 5.
        let right = one_block_batch(
            8,
            150,
            mid_state,
            h(70),
            mid_delta,
            h(80),
            mid_prio,
            k(9),
            k(30),
            0,
        );
        let _ = left.merge_consecutive(&right);
    }

    #[test]
    #[should_panic(expected = "keccak prefix chain")]
    fn merge_rejects_broken_priority_prefix_chain() {
        let mid_state = h(50);
        let mid_delta = h(60);
        let left = one_block_batch(
            5,
            100,
            h(40),
            mid_state,
            h(45),
            mid_delta,
            k(1),
            k(7),
            k(20),
            1,
        );
        // right.old_priority_prefix (k(8)) != left.new_priority_prefix (k(7)).
        let right = one_block_batch(
            6,
            150,
            mid_state,
            h(70),
            mid_delta,
            h(80),
            k(8),
            k(9),
            k(30),
            1,
        );
        let _ = left.merge_consecutive(&right);
    }

    /// On-chain-ops keccak-chain continuity with a NON-EMPTY on-chain-ops leaf:
    /// the right child's segment-start digest must equal the left child's
    /// running on-chain-ops hash. Seam-match passes, seam-mismatch errors.
    #[test]
    fn on_chain_ops_keccak_chain_continuity_non_empty_leaf() {
        // Construct a realistic non-empty on-chain-ops running hash for the
        // left child by folding one op into the zero digest, exactly as the
        // host `aggregate_on_chain_operations_pub_data` does.
        let zero_digest = [0u8; KECCAK_HASH_OUT_BYTE_SIZE];
        let op_pub_data: Vec<u8> = (0..KECCAK_HASH_OUT_BYTE_SIZE as u8).collect();
        let mut input = vec![];
        input.extend_from_slice(&zero_digest);
        input.extend_from_slice(&op_pub_data);
        let left_running: [u8; KECCAK_HASH_OUT_BYTE_SIZE] = keccak(&input);
        // The running hash must be genuinely non-empty (non-zero) for this to
        // exercise the AC edge case.
        assert_ne!(left_running, zero_digest);

        // Left segment starts at the segment's true start (here: zero);
        // right segment's start digest equals the left child's running hash.
        let left_segment = SegmentInfo {
            old_on_chain_operations_pub_data_hash: zero_digest,
        };
        let right_segment_match = SegmentInfo {
            old_on_chain_operations_pub_data_hash: left_running,
        };

        // Seam match: stitch succeeds, carrying the LEFT segment's start digest.
        let stitched = left_segment.stitch(&right_segment_match, &left_running);
        assert_eq!(
            stitched.old_on_chain_operations_pub_data_hash,
            left_segment.old_on_chain_operations_pub_data_hash,
            "stitched segment must carry the left child's start digest"
        );
        // try_stitch agrees.
        assert!(
            left_segment
                .try_stitch(&right_segment_match, &left_running)
                .is_ok()
        );

        // Seam mismatch: a right segment whose start digest disagrees with the
        // left running hash must error (and panic via the hard `stitch`).
        let right_segment_mismatch = SegmentInfo {
            old_on_chain_operations_pub_data_hash: k(42),
        };
        assert!(
            left_segment
                .try_stitch(&right_segment_mismatch, &left_running)
                .is_err(),
            "mismatched on-chain-ops seam must be rejected"
        );

        // And the full Batch-level merge mirror: when the left child's running
        // on-chain-ops hash is `left_running` and the right child's
        // segment-start matches, the merged batch carries the right child's
        // (further-advanced) running hash.
        let mid_state = h(50);
        let mid_delta = h(60);
        let mid_prio = k(7);
        // Advance the right child's running hash one more op past `left_running`.
        let mut input2 = vec![];
        input2.extend_from_slice(&left_running);
        input2.extend_from_slice(&op_pub_data);
        let right_running: [u8; KECCAK_HASH_OUT_BYTE_SIZE] = keccak(&input2);

        let left = one_block_batch(
            5,
            100,
            h(40),
            mid_state,
            h(45),
            mid_delta,
            k(1),
            mid_prio,
            left_running, // left child's running on-chain-ops hash
            1,
        );
        let right = one_block_batch(
            6,
            150,
            mid_state,
            h(70),
            mid_delta,
            h(80),
            mid_prio,
            k(9),
            right_running, // right child's running on-chain-ops hash
            1,
        );
        let merged = left.merge_consecutive(&right);
        assert_eq!(
            merged.on_chain_operations_pub_data_hash, right_running,
            "merged batch must expose the right child's running on-chain-ops hash"
        );
    }

    #[test]
    #[should_panic(expected = "seam broken")]
    fn segment_stitch_rejects_broken_seam() {
        let left_segment = SegmentInfo {
            old_on_chain_operations_pub_data_hash: k(1),
        };
        let right_segment = SegmentInfo {
            old_on_chain_operations_pub_data_hash: k(50),
        };
        // Left running hash k(99) != right segment start k(50): must panic.
        let _ = left_segment.stitch(&right_segment, &k(99));
    }

    #[test]
    fn segment_stitch_start_digest_threading() {
        // The stitched segment must always carry the LEFT segment's start
        // digest, never the right's, so the chain stays anchored at the
        // segment's true start across a chain of merges.
        let seg_a = SegmentInfo {
            old_on_chain_operations_pub_data_hash: k(10),
        };
        let running_ab = k(20);
        let seg_b = SegmentInfo {
            old_on_chain_operations_pub_data_hash: running_ab,
        };
        let ab = seg_a.stitch(&seg_b, &running_ab);
        assert_eq!(ab.old_on_chain_operations_pub_data_hash, k(10));

        // Stitch the (A+B) node to a third segment C whose start equals the
        // running hash after B; the result still carries A's start digest.
        let running_abc = k(30);
        let seg_c = SegmentInfo {
            old_on_chain_operations_pub_data_hash: running_abc,
        };
        let abc = ab.stitch(&seg_c, &running_abc);
        assert_eq!(
            abc.old_on_chain_operations_pub_data_hash,
            k(10),
            "start digest must remain anchored at the leftmost segment across merges"
        );
    }
}
