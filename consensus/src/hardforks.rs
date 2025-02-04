use std::fmt::{Display, Formatter};
use std::ops::Range;

use monero_serai::block::BlockHeader;
use tower::ServiceExt;
use tracing::instrument;

use cuprate_common::Network;

use crate::{ConsensusError, Database, DatabaseRequest, DatabaseResponse};

// https://cuprate.github.io/monero-docs/consensus_rules/hardforks.html#accepting-a-fork
const DEFAULT_WINDOW_SIZE: u64 = 10080; // supermajority window check length - a week

#[derive(Debug, Clone, Copy)]
pub struct BlockHFInfo {
    version: HardFork,
    vote: HardFork,
}

impl BlockHFInfo {
    pub fn from_block_header(block_header: &BlockHeader) -> Result<BlockHFInfo, ConsensusError> {
        BlockHFInfo::from_major_minor(block_header.major_version, block_header.minor_version)
    }

    pub fn from_major_minor(
        major_version: u8,
        minor_version: u8,
    ) -> Result<BlockHFInfo, ConsensusError> {
        Ok(BlockHFInfo {
            version: HardFork::from_version(&major_version)?,
            vote: HardFork::from_vote(&minor_version),
        })
    }
}

/// An identifier for every hard-fork Monero has had.
#[derive(Debug, PartialEq, Eq, PartialOrd, Ord, Copy, Clone)]
#[repr(u8)]
pub enum HardFork {
    V1 = 1,
    V2,
    V3,
    V4,
    V5,
    V6,
    V7,
    V8,
    V9,
    V10,
    V11,
    V12,
    V13,
    V14,
    V15,
    // remember to update from_vote!
    V16,
}

impl HardFork {
    /// Returns the hard-fork for a blocks `major_version` field.
    ///
    /// https://cuprate.github.io/monero-docs/consensus_rules/hardforks.html#blocks-version-and-vote
    pub fn from_version(version: &u8) -> Result<HardFork, ConsensusError> {
        Ok(match version {
            1 => HardFork::V1,
            2 => HardFork::V2,
            3 => HardFork::V3,
            4 => HardFork::V4,
            5 => HardFork::V5,
            6 => HardFork::V6,
            7 => HardFork::V7,
            8 => HardFork::V8,
            9 => HardFork::V9,
            10 => HardFork::V10,
            11 => HardFork::V11,
            12 => HardFork::V12,
            13 => HardFork::V13,
            14 => HardFork::V14,
            15 => HardFork::V15,
            16 => HardFork::V16,
            _ => {
                return Err(ConsensusError::InvalidHardForkVersion(
                    "Version is not a known hard fork",
                ))
            }
        })
    }

    /// Returns the hard-fork for a blocks `minor_version` (vote) field.
    ///
    /// https://cuprate.github.io/monero-docs/consensus_rules/hardforks.html#blocks-version-and-vote
    pub fn from_vote(vote: &u8) -> HardFork {
        if *vote == 0 {
            // A vote of 0 is interpreted as 1 as that's what Monero used to default to.
            return HardFork::V1;
        }
        // This must default to the latest hard-fork!
        Self::from_version(vote).unwrap_or(HardFork::V16)
    }

    /// Returns the next hard-fork.
    pub fn next_fork(&self) -> Option<HardFork> {
        HardFork::from_version(&(*self as u8 + 1)).ok()
    }

    /// Returns the threshold of this fork.
    pub fn fork_threshold(&self, _: &Network) -> u64 {
        // No Monero hard forks actually use voting
        0
    }

    /// Returns the votes needed for this fork.
    ///
    /// https://cuprate.github.io/monero-docs/consensus_rules/hardforks.html#accepting-a-fork
    pub fn votes_needed(&self, network: &Network, window: u64) -> u64 {
        (self.fork_threshold(network) * window + 99) / 100
    }

    /// Returns the minimum height this fork will activate at
    pub fn fork_height(&self, network: &Network) -> u64 {
        match network {
            Network::Mainnet => self.mainnet_fork_height(),
            Network::Stagenet => self.stagenet_fork_height(),
            Network::Testnet => self.testnet_fork_height(),
        }
    }

    /// https://cuprate.github.io/monero-docs/consensus_rules/hardforks.html#Stagenet-Hard-Forks
    fn stagenet_fork_height(&self) -> u64 {
        todo!()
    }

    /// https://cuprate.github.io/monero-docs/consensus_rules/hardforks.html#Testnet-Hard-Forks
    fn testnet_fork_height(&self) -> u64 {
        todo!()
    }

    /// https://cuprate.github.io/monero-docs/consensus_rules/hardforks.html#Mainnet-Hard-Forks
    fn mainnet_fork_height(&self) -> u64 {
        match self {
            HardFork::V1 => 0, // Monero core has this as 1, which is strange
            HardFork::V2 => 1009827,
            HardFork::V3 => 1141317,
            HardFork::V4 => 1220516,
            HardFork::V5 => 1288616,
            HardFork::V6 => 1400000,
            HardFork::V7 => 1546000,
            HardFork::V8 => 1685555,
            HardFork::V9 => 1686275,
            HardFork::V10 => 1788000,
            HardFork::V11 => 1788720,
            HardFork::V12 => 1978433,
            HardFork::V13 => 2210000,
            HardFork::V14 => 2210720,
            HardFork::V15 => 2688888,
            HardFork::V16 => 2689608,
        }
    }

    /// Returns if the hard-fork is in range:
    ///
    /// start <= hf < end
    pub fn in_range(&self, start: &HardFork, end: &HardFork) -> bool {
        start <= self && self < end
    }
}

/// A struct holding the current voting state of the blockchain.
#[derive(Debug, Default, Clone)]
struct HFVotes {
    votes: [u64; 16],
}

impl Display for HFVotes {
    fn fmt(&self, f: &mut Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("HFVotes")
            .field("total", &self.total_votes())
            .field("V1", &self.votes_for_hf(&HardFork::V1))
            .field("V2", &self.votes_for_hf(&HardFork::V2))
            .field("V3", &self.votes_for_hf(&HardFork::V3))
            .field("V4", &self.votes_for_hf(&HardFork::V4))
            .field("V5", &self.votes_for_hf(&HardFork::V5))
            .field("V6", &self.votes_for_hf(&HardFork::V6))
            .field("V7", &self.votes_for_hf(&HardFork::V7))
            .field("V8", &self.votes_for_hf(&HardFork::V8))
            .field("V9", &self.votes_for_hf(&HardFork::V9))
            .field("V10", &self.votes_for_hf(&HardFork::V10))
            .field("V11", &self.votes_for_hf(&HardFork::V11))
            .field("V12", &self.votes_for_hf(&HardFork::V12))
            .field("V13", &self.votes_for_hf(&HardFork::V13))
            .field("V14", &self.votes_for_hf(&HardFork::V14))
            .field("V15", &self.votes_for_hf(&HardFork::V15))
            .field("V16", &self.votes_for_hf(&HardFork::V16))
            .finish()
    }
}

impl HFVotes {
    /// Add votes for a hard-fork
    pub fn add_votes_for_hf(&mut self, hf: &HardFork, votes: u64) {
        self.votes[*hf as usize - 1] += votes;
    }

    /// Add a vote for a hard-fork.
    pub fn add_vote_for_hf(&mut self, hf: &HardFork) {
        self.add_votes_for_hf(hf, 1)
    }

    /// Remove a vote for a hard-fork.
    pub fn remove_vote_for_hf(&mut self, hf: &HardFork) {
        self.votes[*hf as usize - 1] -= 1;
    }

    /// Returns the total votes for a hard-fork.
    ///
    /// https://cuprate.github.io/monero-docs/consensus_rules/hardforks.html#accepting-a-fork
    pub fn votes_for_hf(&self, hf: &HardFork) -> u64 {
        self.votes[*hf as usize - 1..].iter().sum()
    }

    /// Returns the total amount of votes being tracked
    pub fn total_votes(&self) -> u64 {
        self.votes.iter().sum()
    }
}

/// Configuration for hard-forks.
///
#[derive(Debug, Clone)]
pub struct HardForkConfig {
    /// The network we are on.
    network: Network,
    /// The amount of votes we are taking into account to decide on a fork activation.
    window: u64,
}

impl HardForkConfig {
    pub fn main_net() -> HardForkConfig {
        Self {
            network: Network::Mainnet,
            window: DEFAULT_WINDOW_SIZE,
        }
    }
}

/// A struct that keeps track of the current hard-fork and current votes.
#[derive(Debug, Clone)]
pub struct HardForkState {
    current_hardfork: HardFork,
    next_hardfork: Option<HardFork>,

    config: HardForkConfig,
    votes: HFVotes,

    last_height: u64,
}

impl HardForkState {
    pub async fn init<D: Database + Clone>(
        config: HardForkConfig,
        mut database: D,
    ) -> Result<Self, ConsensusError> {
        let DatabaseResponse::ChainHeight(chain_height) = database
            .ready()
            .await?
            .call(DatabaseRequest::ChainHeight)
            .await?
        else {
            panic!("Database sent incorrect response")
        };

        let hfs = HardForkState::init_from_chain_height(config, chain_height, database).await?;

        Ok(hfs)
    }

    #[instrument(name = "init_hardfork_state", skip(config, database), level = "info")]
    pub async fn init_from_chain_height<D: Database + Clone>(
        config: HardForkConfig,
        chain_height: u64,
        mut database: D,
    ) -> Result<Self, ConsensusError> {
        tracing::info!("Initializing hard-fork state this may take a while.");

        let block_start = chain_height.saturating_sub(config.window);

        let votes = get_votes_in_range(database.clone(), block_start..chain_height).await?;

        if chain_height > config.window {
            debug_assert_eq!(votes.total_votes(), config.window)
        }

        let DatabaseResponse::BlockHFInfo(hf_info) = database
            .ready()
            .await?
            .call(DatabaseRequest::BlockHFInfo((chain_height - 1).into()))
            .await?
        else {
            panic!("Database sent incorrect response!");
        };

        let current_hardfork = hf_info.version;

        let next_hardfork = current_hardfork.next_fork();

        let mut hfs = HardForkState {
            config,
            current_hardfork,
            next_hardfork,
            votes,
            last_height: chain_height - 1,
        };

        hfs.check_set_new_hf();

        tracing::info!(
            "Initialized Hfs, current fork: {:?}, {}",
            hfs.current_hardfork,
            hfs.votes
        );

        Ok(hfs)
    }

    pub fn check_block_version_vote(&self, block_hf_info: &BlockHFInfo) -> bool {
        self.current_hardfork == block_hf_info.version
            && block_hf_info.vote >= self.current_hardfork
    }

    pub async fn new_block<D: Database>(
        &mut self,
        vote: HardFork,
        height: u64,
        mut database: D,
    ) -> Result<(), ConsensusError> {
        assert_eq!(self.last_height + 1, height);
        self.last_height += 1;

        tracing::debug!(
            "Accounting for new blocks vote, height: {}, vote: {:?}",
            self.last_height,
            vote
        );

        self.votes.add_vote_for_hf(&vote);

        for height_to_remove in
            (self.config.window..self.votes.total_votes()).map(|offset| height - offset)
        {
            let DatabaseResponse::BlockHFInfo(hf_info) = database
                .ready()
                .await?
                .call(DatabaseRequest::BlockHFInfo(height_to_remove.into()))
                .await?
            else {
                panic!("Database sent incorrect response!");
            };

            tracing::debug!(
                "Removing block {} vote ({:?}) as they have left the window",
                height_to_remove,
                hf_info.vote
            );

            self.votes.remove_vote_for_hf(&hf_info.vote);
        }

        if height > self.config.window {
            debug_assert_eq!(self.votes.total_votes(), self.config.window);
        }

        self.check_set_new_hf();
        Ok(())
    }

    /// Checks if the next hard-fork should be activated and activates it if it should.
    ///
    /// https://cuprate.github.io/monero-docs/consensus_rules/hardforks.html#accepting-a-fork
    fn check_set_new_hf(&mut self) {
        while let Some(new_hf) = self.next_hardfork {
            if self.last_height + 1 >= new_hf.fork_height(&self.config.network)
                && self.votes.votes_for_hf(&new_hf)
                    >= new_hf.votes_needed(&self.config.network, self.config.window)
            {
                self.set_hf(new_hf);
            } else {
                return;
            }
        }
    }

    /// Sets a new hard-fork.
    fn set_hf(&mut self, new_hf: HardFork) {
        self.next_hardfork = new_hf.next_fork();
        self.current_hardfork = new_hf;
    }
}

#[instrument(name = "get_votes", skip(database))]
async fn get_votes_in_range<D: Database>(
    database: D,
    block_heights: Range<u64>,
) -> Result<HFVotes, ConsensusError> {
    let mut votes = HFVotes::default();

    let DatabaseResponse::BlockHfInfoInRange(vote_list) = database
        .oneshot(DatabaseRequest::BlockHfInfoInRange(block_heights))
        .await?
    else {
        panic!("Database sent incorrect response!");
    };

    for hf_info in vote_list.into_iter() {
        votes.add_vote_for_hf(&hf_info.vote);
    }

    Ok(votes)
}
