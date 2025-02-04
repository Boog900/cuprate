//! # Block Weights
//!
//! This module contains calculations for block weights, including calculating block weight
//! limits, effective medians and long term block weights.
//!
//! For more information please see the [block weights chapter](https://cuprate.github.io/monero-book/consensus_rules/blocks/weight_limit.html)
//! in the Monero Book.
//!
use std::cmp::{max, min};
use std::collections::VecDeque;
use std::ops::Range;

use monero_serai::{block::Block, transaction::Transaction};
use tower::ServiceExt;
use tracing::instrument;

use crate::{hardforks::HardFork, ConsensusError, Database, DatabaseRequest, DatabaseResponse};

const PENALTY_FREE_ZONE_1: usize = 20000;
const PENALTY_FREE_ZONE_2: usize = 60000;
const PENALTY_FREE_ZONE_5: usize = 300000;

const SHORT_TERM_WINDOW: u64 = 100;
const LONG_TERM_WINDOW: u64 = 100000;

#[derive(Debug)]
pub struct BlockWeightInfo {
    pub block_weight: usize,
    pub long_term_weight: usize,
}

/// Calculates the blocks weight.
///
/// https://cuprate.github.io/monero-book/consensus_rules/blocks/weight_limit.html#blocks-weight
pub fn block_weight(block: &Block, txs: &[Transaction]) -> usize {
    txs.iter()
        .chain([&block.miner_tx])
        .map(|tx| tx.weight())
        .sum()
}

/// Returns the penalty free zone
///
/// https://cuprate.github.io/monero-book/consensus_rules/blocks/weight_limit.html#penalty-free-zone
pub fn penalty_free_zone(hf: &HardFork) -> usize {
    if hf == &HardFork::V1 {
        PENALTY_FREE_ZONE_1
    } else if hf.in_range(&HardFork::V2, &HardFork::V5) {
        PENALTY_FREE_ZONE_2
    } else {
        PENALTY_FREE_ZONE_5
    }
}

/// A cache used to calculate block weight limits, the effective median and
/// long term block weights.
///
/// These calculations require a lot of data from the database so by caching
/// this data it reduces the load on the database.
#[derive(Clone)]
pub struct BlockWeightsCache {
    /// This list is not sorted.
    short_term_block_weights: VecDeque<usize>,
    /// This list is sorted.
    long_term_weights: Vec<usize>,
    /// The height of the top block.
    tip_height: u64,
}

impl BlockWeightsCache {
    /// Initialize the [`BlockWeightsCache`] at the the height of the database.
    pub async fn init<D: Database + Clone>(mut database: D) -> Result<Self, ConsensusError> {
        let DatabaseResponse::ChainHeight(chain_height) = database
            .ready()
            .await?
            .call(DatabaseRequest::ChainHeight)
            .await?
        else {
            panic!("Database sent incorrect response!");
        };

        Self::init_from_chain_height(chain_height, database).await
    }

    /// Initialize the [`BlockWeightsCache`] at the the given chain height.
    #[instrument(name = "init_weight_cache", level = "info", skip(database))]
    pub async fn init_from_chain_height<D: Database + Clone>(
        chain_height: u64,
        database: D,
    ) -> Result<Self, ConsensusError> {
        tracing::info!("Initializing weight cache this may take a while.");

        let mut long_term_weights = get_long_term_weight_in_range(
            chain_height.saturating_sub(LONG_TERM_WINDOW)..chain_height,
            database.clone(),
        )
        .await?;

        long_term_weights.sort_unstable();
        tracing::debug!(
            "Sorted long term weights with length: {}",
            long_term_weights.len()
        );

        let short_term_block_weights: VecDeque<usize> = get_blocks_weight_in_range(
            chain_height.saturating_sub(SHORT_TERM_WINDOW)..chain_height,
            database,
        )
        .await?
        .into();

        tracing::info!("Initialized block weight cache, chain-height: {:?}, long term weights length: {:?}, short term weights length: {:?}", chain_height, long_term_weights.len(), short_term_block_weights.len());

        Ok(BlockWeightsCache {
            short_term_block_weights,
            long_term_weights,
            tip_height: chain_height - 1,
        })
    }

    /// Add a new block to the cache.
    ///
    /// The block_height **MUST** be one more than the last height the cache has
    /// seen.
    pub async fn new_block_added<D: Database>(
        &mut self,
        block_height: u64,
        block_weight: usize,
        long_term_weight: usize,
        database: &mut D,
    ) -> Result<(), ConsensusError> {
        tracing::debug!(
            "Adding new block's {} weights to block cache, weight: {}, long term weight: {}",
            block_weight,
            block_weight,
            long_term_weight
        );
        assert_eq!(self.tip_height + 1, block_height);
        self.tip_height += 1;

        match self.long_term_weights.binary_search(&long_term_weight) {
            Ok(idx) | Err(idx) => self.long_term_weights.insert(idx, long_term_weight),
        };

        if let Some(height_to_remove) = block_height.checked_sub(LONG_TERM_WINDOW) {
            tracing::debug!(
                "Block {} is out of the long term weight window, removing it",
                height_to_remove
            );
            let DatabaseResponse::BlockWeights(weights) = database
                .oneshot(DatabaseRequest::BlockWeights(height_to_remove.into()))
                .await?
            else {
                panic!("Database sent incorrect response!");
            };
            let idx = self
                .long_term_weights
                .binary_search(&weights.long_term_weight)
                .expect("Weight must be in list if in the window");
            self.long_term_weights.remove(idx);
        }

        self.short_term_block_weights.push_back(block_weight);
        if self.short_term_block_weights.len() > SHORT_TERM_WINDOW.try_into().unwrap() {
            self.short_term_block_weights.pop_front();
        }

        Ok(())
    }

    /// Returns the next blocks long term weight.
    ///
    /// See: https://cuprate.github.io/monero-book/consensus_rules/blocks/weight_limit.html#calculating-a-blocks-long-term-weight
    pub fn next_block_long_term_weight(&self, hf: &HardFork, block_weight: usize) -> usize {
        calculate_block_long_term_weight(hf, block_weight, &self.long_term_weights)
    }

    /// Returns the effective median weight, used for block reward calculations and to calculate
    /// the block weight limit.
    ///
    /// See: https://cuprate.github.io/monero-book/consensus_rules/blocks/weight_limit.html#calculating-effective-median-weight
    pub fn effective_median_block_weight(&self, hf: &HardFork) -> usize {
        let mut sorted_short_term_weights: Vec<usize> =
            self.short_term_block_weights.clone().into();
        sorted_short_term_weights.sort_unstable();
        calculate_effective_median_block_weight(
            hf,
            &sorted_short_term_weights,
            &self.long_term_weights,
        )
    }

    /// Returns the block weight limit.
    pub fn next_block_weight_limit(&self, hf: &HardFork) -> usize {
        2 * self.effective_median_block_weight(hf)
    }
}

fn calculate_effective_median_block_weight(
    hf: &HardFork,
    sorted_short_term_window: &[usize],
    sorted_long_term_window: &[usize],
) -> usize {
    if hf.in_range(&HardFork::V1, &HardFork::V10) {
        return median(sorted_short_term_window);
    }

    let long_term_median = median(sorted_long_term_window).max(PENALTY_FREE_ZONE_5);
    let short_term_median = median(sorted_short_term_window);
    let effective_median = if hf.in_range(&HardFork::V10, &HardFork::V15) {
        min(
            max(PENALTY_FREE_ZONE_5, short_term_median),
            50 * long_term_median,
        )
    } else {
        min(
            max(long_term_median, short_term_median),
            50 * long_term_median,
        )
    };

    effective_median.max(penalty_free_zone(hf))
}

fn calculate_block_long_term_weight(
    hf: &HardFork,
    block_weight: usize,
    sorted_long_term_window: &[usize],
) -> usize {
    if hf.in_range(&HardFork::V1, &HardFork::V10) {
        return block_weight;
    }

    let long_term_median = max(penalty_free_zone(hf), median(sorted_long_term_window));

    let (short_term_constraint, adjusted_block_weight) =
        if hf.in_range(&HardFork::V10, &HardFork::V15) {
            let stc = long_term_median + long_term_median * 2 / 5;
            (stc, block_weight)
        } else {
            let stc = long_term_median + long_term_median * 7 / 10;
            (stc, max(block_weight, long_term_median * 10 / 17))
        };

    min(short_term_constraint, adjusted_block_weight)
}

fn get_mid(a: usize, b: usize) -> usize {
    // https://github.com/monero-project/monero/blob/90294f09ae34ef96f3dea5fea544816786df87c8/contrib/epee/include/misc_language.h#L43
    (a / 2) + (b / 2) + ((a - 2 * (a / 2)) + (b - 2 * (b / 2))) / 2
}

fn median(array: &[usize]) -> usize {
    let mid = array.len() / 2;

    if array.len() == 1 {
        return array[0];
    }

    if array.len() % 2 == 0 {
        get_mid(array[mid - 1], array[mid])
    } else {
        array[mid]
    }
}

#[instrument(name = "get_block_weights", skip(database))]
async fn get_blocks_weight_in_range<D: Database + Clone>(
    range: Range<u64>,
    database: D,
) -> Result<Vec<usize>, ConsensusError> {
    tracing::info!("getting block weights.");

    let DatabaseResponse::BlockWeightsInRange(weights) = database
        .oneshot(DatabaseRequest::BlockWeightsInRange(range))
        .await?
    else {
        panic!("Database sent incorrect response!")
    };

    Ok(weights.into_iter().map(|info| info.block_weight).collect())
}

#[instrument(name = "get_long_term_weights", skip(database), level = "info")]
async fn get_long_term_weight_in_range<D: Database + Clone>(
    range: Range<u64>,
    database: D,
) -> Result<Vec<usize>, ConsensusError> {
    tracing::info!("getting block long term weights.");

    let DatabaseResponse::BlockWeightsInRange(weights) = database
        .oneshot(DatabaseRequest::BlockWeightsInRange(range))
        .await?
    else {
        panic!("Database sent incorrect response!")
    };

    Ok(weights
        .into_iter()
        .map(|info| info.long_term_weight)
        .collect())
}
