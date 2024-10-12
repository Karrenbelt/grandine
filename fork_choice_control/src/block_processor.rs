use std::sync::Arc;




use std::collections::{VecDeque, HashMap};
use std::fmt;
use std::sync::Mutex;
use std::time::{Duration, Instant};
use anyhow::Result;
use derive_more::Constructor;
use execution_engine::ExecutionEngine;
use fork_choice_store::{
    validate_merge_block, BlockAction, PartialBlockAction, StateCacheProcessor, Store,
};
use helper_functions::{
    predicates,
    slot_report::{NullSlotReport, RealSlotReport, SlotReport, SyncAggregateRewards},
    verifier::Verifier,
};
use ssz::SszHash;
use state_cache::StateWithRewards;
use std_ext::ArcExt as _;
use tracing::{info, warn}; 
use tracing::trace;
use transition_functions::{
    combined,
    unphased::{ProcessSlots, StateRootPolicy},
};
use types::{
    combined::{BeaconBlock, BeaconState, BlindedBeaconBlock, SignedBeaconBlock},
    config::Config as ChainConfig,
    nonstandard::{BlockRewards, Phase, SlashingKind},
    phase0::primitives::H256,
    preset::Preset,
    traits::{BeaconBlock as _, BeaconState as _, SignedBeaconBlock as _},
};


pub struct TimingMetrics {
    pub times: VecDeque<Duration>,
    pub total: Duration,
    pub max_size: usize,
}

impl Default for TimingMetrics {
    fn default() -> Self {
        Self::new(100) // Default max_size of 100
    }
}

impl TimingMetrics {
    pub fn new(max_size: usize) -> Self {
        Self {
            times: VecDeque::with_capacity(max_size),
            total: Duration::default(),
            max_size,
        }
    }

    pub fn update(&mut self, duration: Duration) {
        if self.times.len() >= self.max_size {
            if let Some(old) = self.times.pop_front() {
                self.total -= old;
            }
        }
        self.times.push_back(duration);
        self.total += duration;
    }

    pub fn min(&self) -> Option<Duration> {
        self.times.iter().min().copied()
    }

    pub fn max(&self) -> Option<Duration> {
        self.times.iter().max().copied()
    }

    pub fn average(&self) -> Option<Duration> {
        (!self.times.is_empty()).then(|| self.total / self.times.len() as u32)
    }

    pub fn median(&self) -> Option<Duration> {
        let len = self.times.len();
        if len == 0 {
            return None;
        }
        let mut sorted: Vec<_> = self.times.iter().collect();
        sorted.sort();
        let mid_idx = len / 2;
        Some((*sorted[mid_idx] + *sorted[len - 1 - mid_idx]) / 2)
    }

    pub fn count(&self) -> usize {
        self.times.len()
    }

    pub fn last(&self) -> Option<Duration> {
        self.times.back().copied()
    }

    pub fn total(&self) -> Duration {
        self.total
    }

    pub fn times(&self) -> &VecDeque<Duration> {
        &self.times
    }

    fn to_milliseconds(duration: Duration) -> f64 {
        duration.as_secs_f64() * 1000.0
    }
}

impl fmt::Display for TimingMetrics {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        if self.times.is_empty() {
            write!(f, "Timing metrics are empty.")
        } else {
            let min_ms = self.min().map(Self::to_milliseconds).unwrap_or(0.0);
            let max_ms = self.max().map(Self::to_milliseconds).unwrap_or(0.0);
            let avg_ms = self.average().map(Self::to_milliseconds).unwrap_or(0.0);
            let median_ms = self.median().map(Self::to_milliseconds).unwrap_or(0.0);
            
            write!(
                f,
                "Min: {:.1}ms, Max: {:.1}ms, Avg: {:.1}ms, Median: {:.1}ms",
                min_ms, max_ms, avg_ms, median_ms
            )
        }
    }
}

#[derive(Constructor)]
pub struct BlockProcessor<P: Preset> {
    chain_config: Arc<ChainConfig>,
    state_cache: Arc<StateCacheProcessor<P>>,
    metrics: Mutex<HashMap<String, TimingMetrics>>,
}

impl<P: Preset> BlockProcessor<P> {
    fn update_metrics(&self, name: &str, duration: Duration) {
        let mut metrics = self.metrics.lock().unwrap();
        let entry = metrics.entry(name.to_string()).or_insert_with(|| TimingMetrics::new(100));
        entry.update(duration);
        trace!("{} timing: {}", name, entry);
    }
    pub fn process_untrusted_block_with_report(
        &self,
        mut state: Arc<BeaconState<P>>,
        block: &BeaconBlock<P>,
        skip_randao_verification: bool,
    ) -> Result<StateWithRewards<P>> {
        let start = Instant::now();
        info!("Processing untrusted block with slot: {}", block.slot());
        let result = self.state_cache
            .get_or_insert_with(block.hash_tree_root(), block.slot(), false, || {
                let mut slot_report = RealSlotReport::default();

                combined::process_untrusted_block(
                    &self.chain_config,
                    state.make_mut(),
                    block,
                    &mut slot_report,
                    skip_randao_verification,
                )?;

                let block_rewards = calculate_block_rewards(&slot_report);
                info!("Block processed. Slot: {} | Rewards: {:?}", block.slot(), block_rewards);


                Ok((state, Some(block_rewards)))
            });
        self.update_metrics("process_untrusted_block", start.elapsed());
        result
    
    }

    pub fn process_trusted_block_with_report(
        &self,
        mut state: Arc<BeaconState<P>>,
        block: &BeaconBlock<P>,
    ) -> Result<StateWithRewards<P>> {
        let start = Instant::now();
        info!("Processing trusted block with slot: {}", block.slot());
        let result = self.state_cache
            .get_or_insert_with(block.hash_tree_root(), block.slot(), false, || {
                let mut slot_report = RealSlotReport::default();

                combined::process_trusted_block(
                    &self.chain_config,
                    state.make_mut(),
                    block,
                    &mut slot_report,
                )?;

                let block_rewards = calculate_block_rewards(&slot_report);
                info!("Trusted block processed. Slot: {} | Rewards: {:?}", block.slot(), block_rewards);

                Ok((state, Some(block_rewards)))
            });
        self.update_metrics("process_trusted_block", start.elapsed());
        result

    }

    pub fn process_untrusted_blinded_block_with_report(
        &self,
        mut state: Arc<BeaconState<P>>,
        block: &BlindedBeaconBlock<P>,
        skip_randao_verification: bool,
    ) -> Result<StateWithRewards<P>> {
        let start = Instant::now();
        info!("Processing untrusted blinded block with slot: {}", block.slot());
        let result = self.state_cache
            .get_or_insert_with(block.hash_tree_root(), block.slot(), false, || {
                let mut slot_report = RealSlotReport::default();

                combined::process_untrusted_blinded_block(
                    &self.chain_config,
                    state.make_mut(),
                    block,
                    &mut slot_report,
                    skip_randao_verification,
                )?;

                let block_rewards = calculate_block_rewards(&slot_report);
                info!("Untrusted blinded block processed. Slot: {} | Rewards: {:?}", block.slot(), block_rewards);


                Ok((state, Some(block_rewards)))
            });
        self.update_metrics("process_untrusted_blinded_block", start.elapsed());
        result
    }

    pub fn process_trusted_blinded_block_with_report(
        &self,
        mut state: Arc<BeaconState<P>>,
        block: &BlindedBeaconBlock<P>,
    ) -> Result<StateWithRewards<P>> {
        let start = Instant::now();
        info!("Processing trusted blinded block with slot: {}", block.slot());
        let result = self.state_cache
            .get_or_insert_with(block.hash_tree_root(), block.slot(), false, || {
                let mut slot_report = RealSlotReport::default();

                combined::process_trusted_blinded_block(
                    &self.chain_config,
                    state.make_mut(),
                    block,
                    &mut slot_report,
                )?;

                let block_rewards = calculate_block_rewards(&slot_report);
                info!("Trusted blinded block processed. Slot: {} | Rewards: {:?}", block.slot(), block_rewards);


                Ok((state, Some(block_rewards)))
            });
        self.update_metrics("process_trusted_blinded_block", start.elapsed());
        result
    }

    #[allow(clippy::too_many_arguments)]
    pub fn perform_state_transition(
        &self,
        mut state: Arc<BeaconState<P>>,
        block: &SignedBeaconBlock<P>,
        block_root: H256,
        process_slots: ProcessSlots,
        state_root_policy: StateRootPolicy,
        execution_engine: impl ExecutionEngine<P> + Send,
        verifier: impl Verifier + Send,
        slot_report: impl SlotReport + Send,
    ) -> Result<Arc<BeaconState<P>>> {
        let start = Instant::now();
        info!("Performing state transition for block with root: {:?}, slot: {}", block_root, block.message().slot());
        let result = self.state_cache
            .get_or_insert_with(block_root, block.message().slot(), true, || {
                combined::custom_state_transition(
                    &self.chain_config,
                    state.make_mut(),
                    block,
                    process_slots,
                    state_root_policy,
                    execution_engine,
                    verifier,
                    slot_report,
                )?;

                Ok((state, None))
            })
            .map(|(state, _)| {
                info!("State transition completed for block with slot: {}", block.message().slot());
                state
            });
            self.update_metrics("perform_state_transition", start.elapsed());
            result
        }

    pub fn validate_block_for_gossip(
        &self,
        store: &Store<P>,
        block: &Arc<SignedBeaconBlock<P>>,
    ) -> Result<Option<BlockAction<P>>> {
        let start = Instant::now();
        info!("Validating block for gossip with slot: {}", block.message().slot());
        let result = store.validate_block_for_gossip(block, |parent| {
            let block_slot = block.message().slot();

            // > Make a copy of the state to avoid mutability issues
            let mut state = self
                .state_cache
                .before_or_at_slot(store, parent.block_root, block_slot)
                .unwrap_or_else(|| parent.state(store));

            // > Process slots (including those with no blocks) since block
            if state.slot() < block_slot {
                combined::process_slots(&self.chain_config, state.make_mut(), block_slot)?;
            }

            combined::process_block_for_gossip(&self.chain_config, &state, block)?;
            info!("Block validation for gossip complete for slot: {}", block.message().slot());

            Ok(None)
        });
        self.update_metrics("validate_block_for_gossip", start.elapsed());
        result
    }

    pub fn validate_block<E: ExecutionEngine<P> + Send>(
        &self,
        store: &Store<P>,
        block: &Arc<SignedBeaconBlock<P>>,
        state_root_policy: StateRootPolicy,
        execution_engine: E,
        verifier: impl Verifier + Send,
    ) -> Result<BlockAction<P>> {
        let start = Instant::now();
        info!("Validating block with slot: {}", block.message().slot());
        let result = store.validate_block_with_custom_state_transition(block, |block_root, parent| {
            // > Make a copy of the state to avoid mutability issues
            let state = self
                .state_cache
                .before_or_at_slot(store, parent.block_root, block.message().slot())
                .unwrap_or_else(|| parent.state(store));

            // This validation was removed from Capella in `consensus-specs` v1.4.0-alpha.0.
            // See <https://github.com/ethereum/consensus-specs/pull/3232>.
            // It is unclear when modifications to fork choice logic should come into effect.
            // We check the phase of the block rather than the current slot.
            if block.phase() < Phase::Capella {
                // > [New in Bellatrix]
                //
                // The Fork Choice specification does this after the state transition.
                // We don't because that would require keeping around a clone of the pre-state.
                if let Some(body) = block
                    .message()
                    .body()
                    .post_bellatrix()
                    .filter(|body| predicates::is_merge_transition_block(&state, *body))
                {
                    match validate_merge_block(&self.chain_config, block, body, &execution_engine)?
                    {
                        PartialBlockAction::Accept => {}
                        PartialBlockAction::Ignore => {
                            warn!("Block ignored at slot: {}", block.message().slot());
                            return Ok((state, Some(BlockAction::Ignore(false))))
                        }
                    }
                }
            }

            let state = self.perform_state_transition(
                state,
                block,
                block_root,
                ProcessSlots::IfNeeded,
                state_root_policy,
                execution_engine,
                verifier,
                NullSlotReport,
            )?;
            info!("Block validation completed for slot: {}", block.message().slot());

            Ok((state, None))
        });
        self.update_metrics("validate_block", start.elapsed());
        result
    }
}

fn calculate_block_rewards(slot_report: &RealSlotReport) -> BlockRewards {
    let attestations = slot_report.attestation_rewards.iter().sum();

    let sync_aggregate = slot_report
        .sync_aggregate_rewards
        .map(SyncAggregateRewards::total)
        .unwrap_or_default();

    let proposer_slashings = slot_report.slashing_rewards[SlashingKind::Proposer]
        .iter()
        .sum();

    let attester_slashings = slot_report.slashing_rewards[SlashingKind::Attester]
        .iter()
        .sum();

    BlockRewards {
        total: attestations + sync_aggregate + proposer_slashings + attester_slashings,
        attestations,
        sync_aggregate,
        proposer_slashings,
        attester_slashings,
    }
}
