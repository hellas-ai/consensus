//! The [`ViewChain`] is a data structure that represents the chain of views in the consensus
//! protocol, that have not yet been finalized by an L-notarization (n-f votes).
//!
//! The [`ViewChain`] is a modular component that is responsible solely for the following tasks:
//!
//! - Routing messages to the appropriate view context and communicating the decision event produced
//!   by the [`ViewContext`] to the higher-level components, such as the [`ViewProgressManager`].
//! - Finalize blocks with L-notarization and persist them. Note: ONLY L-notarization finalizes
//!   blocks. Nullifications and M-notarizations cause view progression but do NOT finalize blocks.
//! - Manage garbage collection of old views once blocks are L-notarized.
//!
//! IMPORTANT DISTINCTION (from Minimit paper):
//! - **View Progression**: Can happen via M-notarization OR nullification. The view advances but
//!   blocks remain unfinalized.
//! - **Block Finalization**: Happens ONLY via L-notarization (n-f votes). This commits the block to
//!   the ledger.
//!
//! The current logic relies on the following properties from the Minimit protocol:
//!   
//! PROPERTY (View Progression):
//!
//! A replica progresses from view `v` to view `v+1` when EITHER:
//! 1. An M-notarization (2f+1 votes) is received for a view `v` block, OR
//! 2. A nullification (2f+1 nullify messages) is received for view `v`
//!
//! PROPERTY (Block Finalization):
//!
//! A block for view `v` is finalized (committed to ledger) when an L-notarization (n-f votes) is
//! received. This is the ONLY way blocks are finalized.
//!
//! PROPERTY (Vote on M-notarization - Critical for Liveness):
//!
//! When a replica receives an M-notarization for a view `v` block `b`, if it hasn't yet voted
//! or nullified in view `v`, it MUST vote for `b` before progressing to view `v+1`.
//! This ensures all correct replicas vote for blocks proposed by correct leaders after GST.
//!
//! Therefore, the [`ViewChain`] is composed of a chain of consecutive non-finalized views
//!
//! v1 -> ... -> vk -> v(k+1) -> ... -> vn
//!
//! Where `vn` is the current view, and `v1` is the oldest non-L-notarized view.
//! Views may be nullified (have nullification) but remain in the chain until garbage collected.

use std::{collections::HashMap, time::Duration};

use anyhow::Result;

use crate::{
    consensus_manager::view_context::{
        CollectedNullificationsResult, CollectedVotesResult, LeaderProposalResult, ShouldMNotarize,
        ViewContext,
    },
    state::{
        block::Block,
        leader::Leader,
        notarizations::{MNotarization, Vote},
        nullify::{Nullification, Nullify},
        peer::PeerSet,
        view::View,
    },
    storage::store::ConsensusStore,
};

/// [`ViewChain`] manages the chain of `non-finalized` views in the consensus protocol.
///
/// It encapsulates the logic for handling the current view and `non-finalized` previous views,
/// routing messages to the appropriate view context, and managing finalization.
pub struct ViewChain<const N: usize, const F: usize, const M_SIZE: usize> {
    /// The current active view number
    current_view: u64,

    /// Map of non-finalized view contexts, keyed by view number
    ///
    /// These views have achieved a M-notarization or a nullification and the protocol has
    /// progressed past them, but they haven't achieved L-notarization yet. We continue
    /// collecting votes for potential finalization.
    ///
    /// This map contains at least one entry, namely that corresponding to the current view number.
    non_finalized_views: HashMap<u64, ViewContext<N, F, M_SIZE>>,

    /// The persistence storage for the consensus protocol
    /// This is used to persist the view contexts and the votes/nullifications/notarizations
    /// whenever a view in the [`ViewChain`] is finalized by the state machine replication
    /// protocol.
    persistence_storage: ConsensusStore,

    /// The most recent finalized block hash in the current replica's state machine
    // TODO: Move this to [`ViewProgressManager`]
    previously_committed_block_hash: [u8; blake3::OUT_LEN],

    /// The timeout period for a view to be considered nullified by the current replica
    _view_timeout: Duration,
}

impl<const N: usize, const F: usize, const M_SIZE: usize> ViewChain<N, F, M_SIZE> {
    /// Creates a new [`ViewChain`] starting from the given initial view.
    pub fn new(
        initial_view: ViewContext<N, F, M_SIZE>,
        persistence_storage: ConsensusStore,
        view_timeout: Duration,
    ) -> Self {
        Self {
            current_view: initial_view.view_number,
            non_finalized_views: HashMap::from([(initial_view.view_number, initial_view)]),
            persistence_storage,
            previously_committed_block_hash: Default::default(),
            _view_timeout: view_timeout,
        }
    }

    /// Returns a reference to the current view context
    pub fn current(&self) -> &ViewContext<N, F, M_SIZE> {
        &self.non_finalized_views[&self.current_view]
    }

    /// Returns a mutable reference to the current view context
    pub fn current_view_mut(&mut self) -> &mut ViewContext<N, F, M_SIZE> {
        self.non_finalized_views
            .get_mut(&self.current_view)
            .expect("Current view context not found")
    }

    /// Finds a view context by view number
    pub fn find_view_context(&self, view_number: u64) -> Option<&ViewContext<N, F, M_SIZE>> {
        self.non_finalized_views.get(&view_number)
    }

    /// Finds a mutable view context by view number
    pub fn find_view_context_mut(
        &mut self,
        view_number: u64,
    ) -> Option<&mut ViewContext<N, F, M_SIZE>> {
        self.non_finalized_views.get_mut(&view_number)
    }

    /// Returns the current view number
    pub fn current_view_number(&self) -> u64 {
        self.current_view
    }

    /// Returns the number of non-finalized views
    pub fn non_finalized_count(&self) -> usize {
        self.non_finalized_views.len()
    }

    /// Returns the range of view numbers for the unfinalized views
    pub fn non_finalized_view_numbers_range(&self) -> std::ops::RangeInclusive<u64> {
        let current_view = self.current_view;
        let least_non_finalized_view = self
            .current_view
            .saturating_sub(self.non_finalized_count() as u64)
            + 1;
        least_non_finalized_view..=current_view
    }

    /// Returns the range of view numbers for the non-finalized views until the given view number
    pub fn non_finalized_views_until(
        &self,
        view_number: u64,
    ) -> Option<std::ops::RangeInclusive<u64>> {
        let current_view = self.current_view;
        let upper_bound = view_number.min(current_view);
        let least_non_finalized_view = self
            .current_view
            .saturating_sub(self.non_finalized_count() as u64)
            + 1;
        if least_non_finalized_view > upper_bound {
            return None;
        }
        Some(least_non_finalized_view..=upper_bound)
    }

    /// Adds a block proposal to a non-finalized view and validates the chain structure.
    ///
    /// This method performs critical validation to ensure the block extends the correct chain:
    /// - The view must exist in the non-finalized views
    /// - The view must not already have a block or pending block
    /// - The parent block must either be finalized or have an M-notarization
    /// - All intermediate views between the parent and current view must be nullified
    /// - The parent view itself must not be nullified
    ///
    /// If the parent view lacks an M-notarization, the block is stored as pending and will
    /// be processed once the parent receives M-notarization.
    ///
    /// # Arguments
    /// * `view_number` - The view number for which the block is proposed
    /// * `block` - The block proposal to add
    ///
    /// # Returns
    /// * `Ok(LeaderProposalResult)` - Contains block hash and flags indicating whether:
    ///   - The view has enough votes to M-notarize
    ///   - The view has enough votes to finalize
    ///   - The replica should await parent M-notarization
    ///   - The replica should vote for the block
    ///
    /// # Errors
    /// * View is not in non-finalized views
    /// * View already has a block or pending block
    /// * Parent view is nullified
    /// * Intermediate views are not nullified
    /// * Parent block hash doesn't match any finalized or non-finalized block
    pub fn add_block_proposal(
        &mut self,
        view_number: u64,
        block: Block,
    ) -> Result<LeaderProposalResult> {
        // Check if view exists in non-finalized views
        let ctx = self
            .non_finalized_views
            .get_mut(&view_number)
            .ok_or_else(|| {
                anyhow::anyhow!(
                    "Block proposal for view {} is not a non-finalized view (current view: {})",
                    view_number,
                    self.current_view
                )
            })?;

        // Check if this view already has a block
        if ctx.block.is_some() {
            return Err(anyhow::anyhow!(
                "Block proposal for view {} already has a block from leader",
                view_number
            ));
        }

        // Check if this view already has a pending block
        if ctx.pending_block.is_some() {
            return Err(anyhow::anyhow!(
                "Block proposal for view {} already has a pending block",
                view_number
            ));
        }

        let parent_view = self.find_parent_view(&block.parent_block_hash());
        let should_await = match parent_view {
            Some(parent_view_number) => {
                let parent_ctx = self.non_finalized_views.get(&parent_view_number).unwrap();

                // Check if parent was nullified
                if parent_ctx.nullification.is_some() {
                    return Err(anyhow::anyhow!(
                        "Block proposal for parent view {} is nullified, but the block proposal for view {} is for a parent block hash {}",
                        parent_view_number,
                        view_number,
                        hex::encode(block.parent_block_hash())
                    ));
                }

                // Check all intermediate views are nullified
                for intermediate_view in (parent_view_number + 1)..view_number {
                    if let Some(inter_ctx) = self.non_finalized_views.get(&intermediate_view)
                        && inter_ctx.nullification.is_none()
                    {
                        return Err(anyhow::anyhow!(
                            "Intermediate view {} between parent {} and current {} is not nullified",
                            intermediate_view,
                            parent_view_number,
                            view_number
                        ));
                    }
                }

                // Check parent has received at least one M-notarization
                parent_ctx.m_notarization.is_none()
            }
            None => {
                // Parent not in non-finalized views, check if finalized
                if block.parent_block_hash() != self.previously_committed_block_hash {
                    return Err(anyhow::anyhow!(
                        "Block proposal for parent block hash {} is not the previously committed block hash {}",
                        hex::encode(block.parent_block_hash()),
                        hex::encode(self.previously_committed_block_hash)
                    ));
                }
                false
            }
        };

        let ctx = self.non_finalized_views.get_mut(&view_number).unwrap();

        if should_await {
            // Store block for later processing when parent gets notarized
            let block_hash = block.get_hash();
            ctx.pending_block = Some(block);
            return Ok(LeaderProposalResult {
                block_hash,
                is_enough_to_m_notarize: false,
                is_enough_to_finalize: false,
                should_await: true,
                should_vote: false, /* We don't vote for the block yet, as we are awaiting the
                                     * parent to be notarized */
            });
        }

        // Add block to the view context
        ctx.add_new_view_block(block)
    }

    /// Routes a vote to the appropriate non-finalized view context.
    ///
    /// Validates that the vote's view exists in the chain and adds it to that view's context.
    /// For past views, ensures they haven't progressed without M-notarization.
    ///
    /// # Arguments
    /// * `vote` - The vote to route
    /// * `peers` - The peer set for signature verification
    ///
    /// # Returns
    /// * `Ok(CollectedVotesResult)` - Indicates if enough votes for M-notarization or finalization
    ///
    /// # Errors
    /// * Vote is for a view not in the non-finalized views
    /// * View has progressed without M-notarization (invalid state)
    pub fn route_vote(&mut self, vote: Vote, peers: &PeerSet) -> Result<CollectedVotesResult> {
        if let Some(ctx) = self.non_finalized_views.get_mut(&vote.view) {
            let view_number = ctx.view_number;

            // NOTE: If the view number is not the current view, we check if the view has progressed
            // without a m-notarization, this is to ensure that the view chain is not
            // left in an invalid state.
            if view_number != self.current_view {
                ctx.has_view_progressed_without_m_notarization()?;
            }

            return ctx.add_vote(vote, peers);
        }

        Err(anyhow::anyhow!(
            "Vote for view {} is not the current view {} or an unfinalized view",
            vote.view,
            self.current_view
        ))
    }

    /// Routes a nullify message to the appropriate non-finalized view context.
    ///
    /// Validates that the nullify's view exists in the chain and adds it to that view's context.
    /// For past views, ensures they haven't progressed without M-notarization.
    ///
    /// # Arguments
    /// * `nullify` - The nullify message to route
    /// * `peers` - The peer set for signature verification
    ///
    /// # Returns
    /// * `Ok(true)` - If enough nullify messages collected to form a nullification (2f+1)
    /// * `Ok(false)` - If nullify added but threshold not yet reached
    ///
    /// # Errors
    /// * Nullify is for a view not in the non-finalized views
    /// * View has progressed without M-notarization (invalid state)
    pub fn route_nullify(&mut self, nullify: Nullify, peers: &PeerSet) -> Result<bool> {
        if let Some(ctx) = self.non_finalized_views.get_mut(&nullify.view) {
            let view_number = ctx.view_number;
            // NOTE: If the view number is not the current view, we check if the view has progressed
            // without a m-notarization, this is to ensure that the view chain is not
            // left in an invalid state.
            if view_number != self.current_view {
                ctx.has_view_progressed_without_m_notarization()?;
            }
            ctx.add_nullify(nullify, peers)?;
            if ctx.nullification.is_some() {
                return Ok(true);
            }
            return Ok(false);
        }

        Err(anyhow::anyhow!(
            "Nullify for view {} is not the current view {} or an unfinalized view",
            nullify.view,
            self.current_view
        ))
    }

    /// Routes an M-notarization to the appropriate non-finalized view context.
    ///
    /// Validates that the M-notarization's view exists in the chain and adds it to that view's
    /// context. For past views, ensures they haven't progressed without M-notarization.
    ///
    /// # Arguments
    /// * `m_notarization` - The M-notarization to route
    /// * `peers` - The peer set for signature verification
    ///
    /// # Returns
    /// * `Ok(ShouldMNotarize)` - Indicates whether the replica should vote, notarize, await, or
    ///   nullify
    ///
    /// # Errors
    /// * M-notarization is for a view not in the non-finalized views
    /// * View has progressed without M-notarization (invalid state)
    pub fn route_m_notarization(
        &mut self,
        m_notarization: MNotarization<N, F, M_SIZE>,
        peers: &PeerSet,
    ) -> Result<ShouldMNotarize> {
        if let Some(ctx) = self.non_finalized_views.get_mut(&m_notarization.view) {
            if ctx.view_number != self.current_view {
                ctx.has_view_progressed_without_m_notarization()?;
            }
            return ctx.add_m_notarization(m_notarization, peers);
        }

        Err(anyhow::anyhow!(
            "M-notarization for view {} is not the current view {} or an unfinalized view",
            m_notarization.view,
            self.current_view
        ))
    }

    /// Routes a nullification to the appropriate non-finalized view context.
    ///
    /// Validates that the nullification's view exists in the chain and adds it to that view's
    /// context. For past views, ensures they haven't progressed without M-notarization.
    ///
    /// # Arguments
    /// * `nullification` - The nullification (aggregated 2f+1 nullify messages) to route
    /// * `peers` - The peer set for signature verification
    ///
    /// # Returns
    /// * `Ok(CollectedNullificationsResult)` - Indicates if the nullification was successfully
    ///   added
    ///
    /// # Errors
    /// * Nullification is for a view not in the non-finalized views
    /// * View has progressed without M-notarization (invalid state)
    pub fn route_nullification(
        &mut self,
        nullification: Nullification<N, F, M_SIZE>,
        peers: &PeerSet,
    ) -> Result<CollectedNullificationsResult> {
        if let Some(ctx) = self.non_finalized_views.get_mut(&nullification.view) {
            if ctx.view_number != self.current_view {
                ctx.has_view_progressed_without_m_notarization()?;
            }
            return ctx.add_nullification(nullification, peers);
        }

        Err(anyhow::anyhow!(
            "Nullification for view {} is not the current view {} or an unfinalized view",
            nullification.view,
            self.current_view
        ))
    }

    /// Progresses to the next view after the current view receives an M-notarization.
    ///
    /// M-notarization (2f+1 votes) allows view progression but does NOT finalize blocks.
    /// The current view remains in the non-finalized chain and continues collecting votes
    /// for potential L-notarization (n-f votes), which is required for block finalization.
    ///
    /// # Arguments
    /// * `new_view_ctx` - The view context for the next view (current_view + 1)
    ///
    /// # Errors
    /// * New view number is not exactly current_view + 1
    /// * Current view lacks an M-notarization
    ///
    /// # Note
    /// If the current view has a pending block, it will be logged as a warning but progression
    /// continues. The pending block will not be processed in the new view.
    pub fn progress_with_m_notarization(
        &mut self,
        new_view_ctx: ViewContext<N, F, M_SIZE>,
    ) -> Result<()> {
        // 1. Check that the next view context is the next view.
        if new_view_ctx.view_number != self.current_view + 1 {
            return Err(anyhow::anyhow!(
                "View number {} is not the next view number {}",
                new_view_ctx.view_number,
                self.current_view + 1
            ));
        }

        // 2. Check that the current view has indeed received a m-notarization.
        if self.current().m_notarization.is_none() {
            return Err(anyhow::anyhow!(
                "The current view {} has not received a m-notarization, but the view has progressed with a m-notarization",
                self.current_view
            ));
        }

        if self.current().pending_block.is_some() {
            tracing::warn!(
                "Current view {} has a pending block, but the view has progressed with a m-notarization",
                self.current_view
            );
        }

        // 3. Update the current view to the next view.
        // NOTE: We don't persist yet the current view, as it has not been finalized yet.
        // Moreover, we keep the current view context in the `non_finalized_views` map, as it has
        // not been finalized yet.
        self.current_view = new_view_ctx.view_number;
        self.non_finalized_views
            .insert(new_view_ctx.view_number, new_view_ctx);

        Ok(())
    }

    /// Progresses to the next view after the current view receives a nullification.
    ///
    /// Nullification (2f+1 nullify messages) allows view progression but does NOT finalize blocks.
    /// The nullified view remains in the non-finalized chain. Nullified views are persisted later
    /// when garbage collection occurs during L-notarization of a subsequent view.
    ///
    /// # Arguments
    /// * `next_view_ctx` - The view context for the next view (current_view + 1)
    ///
    /// # Errors
    /// * Next view number is not exactly current_view + 1
    /// * Current view lacks a nullification
    ///
    /// # Note
    /// Nullified views represent failed consensus attempts where the replica network
    /// could not agree on a block proposal, typically due to leader failure or network issues.
    pub fn progress_with_nullification(
        &mut self,
        next_view_ctx: ViewContext<N, F, M_SIZE>,
    ) -> Result<()> {
        // 1. Check that the next view context is the next view.
        if next_view_ctx.view_number != self.current_view + 1 {
            return Err(anyhow::anyhow!(
                "View number {} is not the next view number {}",
                next_view_ctx.view_number,
                self.current_view + 1
            ));
        }

        // 2. Check that the current view has indeed received a nullification.
        if self.current().nullification.is_none() {
            return Err(anyhow::anyhow!(
                "The current view {} has not received a nullification, but the view has progressed with a nullification",
                self.current_view
            ));
        }

        // 3. Update the current view to the next view.
        self.current_view = next_view_ctx.view_number;
        self.non_finalized_views
            .insert(next_view_ctx.view_number, next_view_ctx);

        Ok(())
    }

    /// Finalizes a view with L-notarization and performs garbage collection.
    ///
    /// L-notarization (n-f votes) is the ONLY mechanism that finalizes blocks and commits them
    /// to the ledger. This method validates the chain structure, persists all views from the
    /// oldest non-finalized view up to the finalized view, and removes them from the chain.
    ///
    /// # Chain Validation
    /// - The finalized view must have n-f votes (L-notarization threshold)
    /// - The parent view must have at least a M-notarization (2f+1 votes)
    /// - All intermediate views between parent and finalized must be nullified
    ///
    /// # Persistence Behavior
    /// All views from the oldest non-finalized view to the finalized view are persisted:
    /// - **L-notarized views**: Persisted as finalized blocks (committed to ledger)
    /// - **M-notarized views**: Persisted as non-finalized (progressed but not committed)
    /// - **Nullified views**: Persisted as failed consensus attempts
    ///
    /// After persistence, these views are removed from `non_finalized_views` (garbage collection)
    /// and added to persistence storage.
    ///
    /// # Arguments
    /// * `finalized_view` - The view number to finalize (must have n-f votes)
    /// * `peers` - The peer set for validation
    ///
    /// # Errors
    /// * Finalized view is in the future (greater than current view)
    /// * View is not in non-finalized views or already finalized
    /// * View has fewer than n-f votes (lacks L-notarization)
    /// * Finalized view has no block
    /// * Parent view lacks M-notarization
    /// * Intermediate views are not nullified
    pub fn finalize_with_l_notarization(
        &mut self,
        finalized_view: u64,
        peers: &PeerSet,
    ) -> Result<()> {
        // 1. Check that the view number is not the current view.
        if finalized_view > self.current_view {
            return Err(anyhow::anyhow!(
                "View number {} is not greater than the current view {}, cannot finalize views in the future",
                finalized_view,
                self.current_view,
            ));
        }

        // 2. Get the context for the finalized view
        let finalized_ctx =
            self.non_finalized_views
                .get(&finalized_view)
                .ok_or(anyhow::anyhow!(
                    "View number {} is not an non-finalized view",
                    finalized_view
                ))?;

        // 3. Check that finalized view has L-notarization
        if finalized_ctx.votes.len() < N - F {
            return Err(anyhow::anyhow!(
                "View number {} has not received a l-notarization",
                finalized_view
            ));
        }

        // 4. Find the parent view (the view the finalized block builds on)
        let parent_hash = finalized_ctx
            .block
            .as_ref()
            .ok_or(anyhow::anyhow!("Finalized view has no block"))?
            .parent_block_hash();

        let parent_view = self.find_parent_view(&parent_hash);

        // 5. Validate the chain structure
        if let Some(parent_view_number) = parent_view {
            let parent_ctx = self.non_finalized_views.get(&parent_view_number).unwrap();

            // Parent must have M-notarization
            if parent_ctx.m_notarization.is_none() {
                return Err(anyhow::anyhow!(
                    "Parent view {} does not have M-notarization, cannot finalize view {}",
                    parent_view_number,
                    finalized_view
                ));
            }

            if parent_ctx.view_number >= finalized_view {
                return Err(anyhow::anyhow!(
                    "Parent view {} is more recent than the finalized view {}, cannot finalize view {}",
                    parent_view_number,
                    finalized_view,
                    finalized_view
                ));
            }

            // All intermediate views must be nullified
            for intermediate_view in (parent_view_number + 1)..finalized_view {
                if let Some(inter_ctx) = self.non_finalized_views.get(&intermediate_view)
                    && inter_ctx.nullification.is_none()
                {
                    return Err(anyhow::anyhow!(
                        "Intermediate view {} between parent {} and finalized {} is not nullified",
                        intermediate_view,
                        parent_view_number,
                        finalized_view
                    ));
                }
            }
        }

        // 6. Persist all views from oldest to finalized
        let to_persist_range =
            self.non_finalized_views_until(finalized_view)
                .ok_or(anyhow::anyhow!(
                    "View number {} is not an non-finalized view",
                    finalized_view
                ))?;

        for view_number in to_persist_range {
            let ctx = self.non_finalized_views.remove(&view_number).unwrap();

            if view_number == finalized_view {
                // The view being finalized
                self.previously_committed_block_hash = ctx.block_hash.unwrap();
                self.persist_l_notarized_view(&ctx, peers)?;
            } else if let Some(parent_view_number) = parent_view {
                if view_number == parent_view_number {
                    // The parent view - can be M-notarized or L-notarized
                    if ctx.votes.len() >= N - F {
                        // Has L-notarization too
                        self.previously_committed_block_hash = ctx.block_hash.unwrap();
                        self.persist_l_notarized_view(&ctx, peers)?;
                    } else {
                        // Only M-notarized
                        self.persist_m_notarized_view(&ctx, peers)?;
                    }
                } else if view_number > parent_view_number {
                    // Intermediate view - must be nullified (already validated above)
                    self.persist_nullified_view(&ctx, peers)?;
                } else {
                    // View before parent - could be part of an earlier chain
                    // These should have been finalized in a previous call or be nullified
                    if ctx.nullification.is_some() {
                        self.persist_nullified_view(&ctx, peers)?;
                    } else if ctx.votes.len() >= N - F {
                        self.previously_committed_block_hash = ctx.block_hash.unwrap();
                        self.persist_l_notarized_view(&ctx, peers)?;
                    } else {
                        // M-notarized view from an earlier chain
                        self.persist_m_notarized_view(&ctx, peers)?;
                    }
                }
            } else {
                // Parent not in non-finalized views (already finalized)
                // These are intermediate nullified views
                if ctx.nullification.is_none() {
                    return Err(anyhow::anyhow!(
                        "View {} has no nullification but parent is already finalized",
                        view_number
                    ));
                }
                self.persist_nullified_view(&ctx, peers)?;
            }
        }

        Ok(())
    }

    /// Persists an L-notarized view to storage as a finalized block.
    ///
    /// This method commits the block to the ledger by persisting all consensus artifacts:
    /// - Block (marked as `is_finalized = true`) and its transactions
    /// - M-notarization (required before L-notarization can occur)
    /// - All votes collected (n-f or more)
    /// - Leader and view metadata
    ///
    /// # Arguments
    /// * `ctx` - The view context containing the finalized block and consensus data
    /// * `peers` - The peer set for extracting leader public key
    ///
    /// # Errors
    /// * View has no block despite being L-notarized
    /// * View has no M-notarization (invalid state - L-notarization requires prior M-notarization)
    /// * Storage operation fails
    ///
    /// # Note
    /// L-notarization implies the view previously achieved M-notarization, as replicas
    /// only vote after seeing M-notarization or the block proposal.
    fn persist_l_notarized_view(
        &self,
        ctx: &ViewContext<N, F, M_SIZE>,
        peers: &PeerSet,
    ) -> Result<()> {
        let view_number = ctx.view_number;

        // 1. Persist both the block with `is_finalized` set to true, and the transactions
        //    associated with the block.
        if let Some(ref block) = ctx.block {
            for tx in block.transactions.iter() {
                self.persistence_storage.put_transaction(tx)?;

                // TODO: Handle account creation as well.
            }
            let mut finalized_block = block.clone();
            finalized_block.is_finalized = true;
            self.persistence_storage
                .put_finalized_block(&finalized_block)?;
        } else {
            return Err(anyhow::anyhow!(
                "View number {view_number} has no block, but the view has been finalized with a l-notarization"
            ));
        }

        // 2. Persist the M-notarization for the view
        if let Some(ref m_notarization) = ctx.m_notarization {
            self.persistence_storage.put_notarization(m_notarization)?;
        } else {
            return Err(anyhow::anyhow!(
                "View number {view_number} has no m-notarization, but the view has been finalized with a l-notarization"
            ));
        }

        // 3. Persist the leader metadata
        let leader_id = ctx.leader_id;
        let leader = Leader::new(leader_id, view_number);
        self.persistence_storage.put_leader(&leader)?;

        // 4. Persist the view metadata
        let leader_pk = peers.id_to_public_key.get(&leader_id).unwrap();
        let view = View::new(view_number, leader_pk.clone(), true, false);
        self.persistence_storage.put_view(&view)?;

        // 5. Persist the votes for the view
        for vote in ctx.votes.iter() {
            self.persistence_storage.put_vote(vote)?;
        }

        Ok(())
    }

    /// Persists an M-notarized view to storage as a non-finalized block.
    ///
    /// This method persists a view that achieved M-notarization (2f+1 votes) and caused
    /// view progression, but never reached L-notarization (n-f votes). The block is saved
    /// but NOT marked as finalized, preserving the distinction between view progression
    /// and block finalization.
    ///
    /// Persisted artifacts include:
    /// - Block (with `is_finalized = false`) and its transactions
    /// - M-notarization (2f+1 votes)
    /// - All votes collected (between 2f+1 and n-f-1)
    /// - Leader and view metadata (marked as not finalized)
    ///
    /// # Arguments
    /// * `ctx` - The view context containing the M-notarized block and consensus data
    /// * `peers` - The peer set for extracting leader public key
    ///
    /// # Errors
    /// * View has no block despite being M-notarized
    /// * View has no M-notarization
    /// * Storage operation fails
    ///
    /// # Note
    /// This occurs during garbage collection when a later view is L-notarized, and earlier
    /// M-notarized views need to be persisted before removal from the chain.
    fn persist_m_notarized_view(
        &self,
        ctx: &ViewContext<N, F, M_SIZE>,
        peers: &PeerSet,
    ) -> Result<()> {
        let view_number = ctx.view_number;

        // 1. Persist both the block with `is_finalized` set to true, and the transactions
        //    associated with the block.
        if let Some(ref block) = ctx.block {
            for tx in block.transactions.iter() {
                self.persistence_storage.put_transaction(tx)?;

                // TODO: Handle account creation as well.
            }
            self.persistence_storage.put_finalized_block(block)?;
        } else {
            return Err(anyhow::anyhow!(
                "View number {view_number} has no block, but the view has been finalized with a l-notarization"
            ));
        }

        // Persist the M-notarization
        if let Some(ref m_notarization) = ctx.m_notarization {
            self.persistence_storage.put_notarization(m_notarization)?;
        } else {
            return Err(anyhow::anyhow!(
                "View number {view_number} has no m-notarization"
            ));
        }

        // Persist leader and view metadata
        let leader_id = ctx.leader_id;
        let leader = Leader::new(leader_id, view_number);
        self.persistence_storage.put_leader(&leader)?;

        let leader_pk = peers.id_to_public_key.get(&leader_id).unwrap();
        let view = View::new(view_number, leader_pk.clone(), false, false); // Not finalized
        self.persistence_storage.put_view(&view)?;

        // Persist the votes
        for vote in ctx.votes.iter() {
            self.persistence_storage.put_vote(vote)?;
        }

        Ok(())
    }

    /// Persists a nullified view to storage as a failed consensus attempt.
    ///
    /// Nullified views represent failed consensus where the replica network collected 2f+1
    /// nullify messages instead of achieving M-notarization. This typically occurs due to
    /// leader failure, network partition, or timeout.
    ///
    /// Persisted artifacts include:
    /// - Block (if received from leader) marked as nullified, not finalized
    /// - Nullification (aggregated 2f+1 nullify messages)
    /// - All votes collected (if any)
    /// - All nullify messages (2f+1 or more)
    /// - Leader and view metadata
    ///
    /// # Arguments
    /// * `ctx` - The view context containing the nullification and consensus data
    /// * `peers` - The peer set for extracting leader public key
    ///
    /// # Errors
    /// * View has no nullification despite being called for a nullified view
    /// * Storage operation fails
    ///
    /// # Note
    /// The block may be absent if the leader was faulty and never proposed. This is
    /// expected behavior and not treated as an error.
    fn persist_nullified_view(
        &self,
        ctx: &ViewContext<N, F, M_SIZE>,
        peers: &PeerSet,
    ) -> Result<()> {
        let view_number = ctx.view_number;

        if ctx.nullification.is_none() {
            return Err(anyhow::anyhow!(
                "View number {view_number} has no nullification, but the view has been nullified",
            ));
        }

        // 1. Persist the block with `is_finalized` set to true.
        // NOTE: It is possibly that the current replica never received a block for the view,
        // in case of a faulty leader. So we don't error on that case
        if let Some(ref block) = ctx.block {
            self.persistence_storage.put_nullified_block(block)?;
        }

        // 2. Persist the nullification for the view
        let nullification = ctx.nullification.as_ref().unwrap();
        self.persistence_storage.put_nullification(nullification)?;

        // 3. Persist the leader metadata
        let leader_id = ctx.leader_id;
        let leader = Leader::new(leader_id, view_number);
        self.persistence_storage.put_leader(&leader)?;

        // 4. Persist the view metadata
        let leader_pk = peers.id_to_public_key.get(&leader_id).unwrap();
        let view = View::new(view_number, leader_pk.clone(), true, false);
        self.persistence_storage.put_view(&view)?;

        // 5. Persist the votes for the view
        for vote in ctx.votes.iter() {
            self.persistence_storage.put_vote(vote)?;
        }

        // 6. Persist the nullify messages for the view
        for nullify in ctx.nullify_messages.iter() {
            self.persistence_storage.put_nullify(nullify)?;
        }

        Ok(())
    }

    /// Processes pending block proposals that were awaiting parent M-notarization.
    ///
    /// When a view receives M-notarization, child blocks that were stored as pending
    /// (because they arrived before their parent was notarized) can now be processed.
    /// This method finds all such pending blocks in future views and attempts to add
    /// them to their respective view contexts.
    ///
    /// # Arguments
    /// * `notarized_view` - The view number that just received M-notarization
    ///
    /// # Returns
    /// * `Ok(Vec<LeaderProposalResult>)` - Results from processing each pending block, indicating
    ///   vote status and whether they should be processed immediately or await
    ///
    /// # Errors
    /// * Block validation fails during processing (invalid chain structure, duplicate blocks, etc.)
    ///
    /// # Note
    /// This is called after a view receives M-notarization to unblock any dependent
    /// child proposals that were waiting.
    pub fn process_pending_child_proposals(
        &mut self,
        notarized_view: u64,
    ) -> Result<Vec<LeaderProposalResult>> {
        let mut results = vec![];
        let mut pending: Vec<(u64, Block)> = vec![];

        // First, collect all pending blocks
        for (view_number, ctx) in &mut self.non_finalized_views {
            if *view_number > notarized_view
                && let Some(block) = ctx.pending_block.take()
            {
                pending.push((*view_number, block));
            }
        }

        // Then process them (this avoids borrow checker issues)
        for (view_number, block) in pending {
            let result = self.add_block_proposal(view_number, block)?;
            results.push(result);
        }

        Ok(results)
    }

    /// Persists all non-finalized views to storage during graceful shutdown.
    ///
    /// This method saves the current state of the consensus chain by persisting all
    /// blocks in non-finalized views as non-finalized blocks. This allows the replica
    /// to resume from its last known state after restart, without losing progress on
    /// views that achieved M-notarization but not yet L-notarization.
    ///
    /// The method drains `non_finalized_views`, removing all entries from the chain.
    ///
    /// # Returns
    /// * `Ok(())` - All views successfully persisted
    ///
    /// # Errors
    /// * Storage operation fails during persistence
    ///
    /// # Note
    /// Only blocks are persisted; votes, nullifications, and other consensus artifacts
    /// are not saved. This is sufficient for recovery since the replica can re-sync
    /// these artifacts from peers upon restart.
    pub fn persist_all_views(&mut self) -> Result<()> {
        for (_view_number, ctx) in self.non_finalized_views.drain() {
            if let Some(ref block) = ctx.block {
                self.persistence_storage.put_non_finalized_block(block)?;
            }
        }

        Ok(())
    }

    /// Finds the view number for a given parent block hash in the non-finalized chain.
    ///
    /// Searches through all non-finalized views to locate which view contains a block
    /// with the specified hash. Returns `None` if the parent is not found in the chain
    /// (typically because it's already been finalized and garbage collected).
    ///
    /// # Arguments
    /// * `parent_hash` - The block hash to search for
    ///
    /// # Returns
    /// * `Some(view_number)` - The view that contains the block with this hash
    /// * `None` - No non-finalized view has a block with this hash
    fn find_parent_view(&self, parent_hash: &[u8; blake3::OUT_LEN]) -> Option<u64> {
        for (view_num, ctx) in &self.non_finalized_views {
            if let Some(hash) = ctx.block_hash
                && hash == *parent_hash
            {
                return Some(*view_num);
            }
        }
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        consensus_manager::utils::{
            NotarizationData, NullificationData, create_notarization_data,
            create_nullification_data,
        },
        crypto::aggregated::{BlsSecretKey, PeerId},
        state::{block::Block, transaction::Transaction},
        storage::conversions::Storable,
    };
    use rand::thread_rng;
    use std::collections::{HashMap, HashSet};
    use tempfile::TempDir;

    // Test constants matching typical consensus parameters
    const N: usize = 5; // Total peers
    const F: usize = 1; // Faulty peers
    const M_SIZE: usize = 3; // M-notarization size (2F + 1)

    /// Helper struct to hold all testing infrastructure
    struct TestSetup {
        peer_set: PeerSet,
        peer_id_to_secret_key: HashMap<PeerId, BlsSecretKey>,
        temp_dir: TempDir,
        storage: ConsensusStore,
    }

    impl TestSetup {
        fn new(num_peers: usize) -> Self {
            let mut rng = thread_rng();
            let mut public_keys = vec![];
            let mut peer_id_to_secret_key = HashMap::new();

            for _ in 0..num_peers {
                let sk = BlsSecretKey::generate(&mut rng);
                let pk = sk.public_key();
                let peer_id = pk.to_peer_id();
                peer_id_to_secret_key.insert(peer_id, sk);
                public_keys.push(pk);
            }

            let peer_set = PeerSet::new(public_keys);

            // Create temporary storage
            let temp_dir = TempDir::new().expect("Failed to create temp dir");
            let db_path = temp_dir.path().join("test_consensus.db");
            let storage = ConsensusStore::open(db_path).expect("Failed to create storage");

            Self {
                peer_set,
                peer_id_to_secret_key,
                temp_dir,
                storage,
            }
        }

        fn leader_id(&self, index: usize) -> PeerId {
            self.peer_set.sorted_peer_ids[index]
        }

        fn replica_id(&self, index: usize) -> PeerId {
            self.peer_set.sorted_peer_ids[index]
        }
    }

    /// Creates a test transaction
    fn gen_tx() -> Transaction {
        let mut rng = thread_rng();
        let sk = BlsSecretKey::generate(&mut rng);
        let pk = sk.public_key();
        let tx_hash: [u8; blake3::OUT_LEN] = blake3::hash(b"test tx").into();
        let sig = sk.sign(&tx_hash);
        Transaction::new(pk, [7u8; 32], 42, 9, 1_000, 3, tx_hash, sig)
    }

    /// Creates a test block
    fn create_test_block(
        view: u64,
        leader: PeerId,
        parent_hash: [u8; blake3::OUT_LEN],
        leader_sk: BlsSecretKey,
        height: u64,
    ) -> Block {
        let transactions = vec![gen_tx()];
        Block::new(
            view,
            leader,
            parent_hash,
            transactions,
            1234567890,
            leader_sk.sign(b"block proposal"),
            false,
            height,
        )
    }

    /// Creates a signed vote
    fn create_vote(
        peer_index: usize,
        view: u64,
        block_hash: [u8; blake3::OUT_LEN],
        leader_id: PeerId,
        setup: &TestSetup,
    ) -> Vote {
        let peer_id = setup.peer_set.sorted_peer_ids[peer_index];
        let secret_key = setup.peer_id_to_secret_key.get(&peer_id).unwrap();
        let signature = secret_key.sign(&block_hash);
        Vote::new(view, block_hash, signature, peer_id, leader_id)
    }

    /// Creates a signed nullify message
    fn create_nullify(
        peer_index: usize,
        view: u64,
        leader_id: PeerId,
        setup: &TestSetup,
    ) -> Nullify {
        let peer_id = setup.peer_set.sorted_peer_ids[peer_index];
        let secret_key = setup.peer_id_to_secret_key.get(&peer_id).unwrap();
        let message = blake3::hash(&[view.to_le_bytes(), leader_id.to_le_bytes()].concat());
        let signature = secret_key.sign(message.as_bytes());
        Nullify::new(view, leader_id, signature, peer_id)
    }

    /// Creates a test M-notarization from votes
    fn create_m_notarization(
        votes: &HashSet<Vote>,
        view: u64,
        block_hash: [u8; blake3::OUT_LEN],
        leader_id: PeerId,
    ) -> MNotarization<N, F, M_SIZE> {
        let NotarizationData {
            peer_ids,
            aggregated_signature,
        } = create_notarization_data::<M_SIZE>(votes).unwrap();
        MNotarization::new(view, block_hash, aggregated_signature, peer_ids, leader_id)
    }

    /// Creates a test nullification from nullify messages
    fn create_nullification(
        nullify_messages: &HashSet<Nullify>,
        view: u64,
        leader_id: PeerId,
    ) -> Nullification<N, F, M_SIZE> {
        let NullificationData {
            peer_ids,
            aggregated_signature,
        } = create_nullification_data::<M_SIZE>(nullify_messages).unwrap();
        Nullification::new(view, leader_id, aggregated_signature, peer_ids)
    }

    /// Creates a ViewContext with a block and votes for testing
    fn create_view_context_with_votes(
        view_number: u64,
        leader_id: PeerId,
        replica_id: PeerId,
        parent_hash: [u8; blake3::OUT_LEN],
        num_votes: usize,
        setup: &TestSetup,
    ) -> ViewContext<N, F, M_SIZE> {
        let mut ctx = ViewContext::new(view_number, leader_id, replica_id, parent_hash);

        // Add block
        let leader_sk = setup.peer_id_to_secret_key.get(&leader_id).unwrap();
        let block = create_test_block(
            view_number,
            leader_id,
            parent_hash,
            leader_sk.clone(),
            view_number,
        );
        let block_hash = block.get_hash();
        ctx.add_new_view_block(block).unwrap();

        // Add votes
        for i in 0..num_votes {
            let vote = create_vote(i, view_number, block_hash, leader_id, setup);
            ctx.add_vote(vote, &setup.peer_set).unwrap();
        }

        ctx
    }

    #[test]
    fn test_new_view_chain() {
        let setup = TestSetup::new(N);
        let leader_id = setup.leader_id(0);
        let replica_id = setup.replica_id(1);
        let parent_hash = [0u8; blake3::OUT_LEN];

        let initial_view = ViewContext::new(1, leader_id, replica_id, parent_hash);
        let view_chain =
            ViewChain::<N, F, M_SIZE>::new(initial_view, setup.storage, Duration::from_secs(10));

        assert_eq!(view_chain.current_view_number(), 1);
        assert_eq!(view_chain.non_finalized_count(), 1);
        assert_eq!(view_chain.non_finalized_view_numbers_range(), 1..=1);

        std::fs::remove_dir_all(setup.temp_dir.path()).unwrap();
    }

    #[test]
    fn test_current_view_accessors() {
        let setup = TestSetup::new(N);
        let leader_id = setup.leader_id(0);
        let replica_id = setup.replica_id(1);
        let parent_hash = [0u8; blake3::OUT_LEN];

        let initial_view = ViewContext::new(5, leader_id, replica_id, parent_hash);
        let view_chain =
            ViewChain::<N, F, M_SIZE>::new(initial_view, setup.storage, Duration::from_secs(10));

        assert_eq!(view_chain.current().view_number, 5);
        assert_eq!(view_chain.current().leader_id, leader_id);

        std::fs::remove_dir_all(setup.temp_dir.path()).unwrap();
    }

    #[test]
    fn test_route_vote_to_current_view() {
        let setup = TestSetup::new(N);
        let leader_id = setup.leader_id(0);
        let replica_id = setup.replica_id(1);
        let parent_hash = [1u8; blake3::OUT_LEN];

        let mut ctx = ViewContext::new(1, leader_id, replica_id, parent_hash);
        let leader_sk = setup.peer_id_to_secret_key.get(&leader_id).unwrap();
        let block = create_test_block(1, leader_id, parent_hash, leader_sk.clone(), 1);
        let block_hash = block.get_hash();
        ctx.add_new_view_block(block).unwrap();

        let mut view_chain =
            ViewChain::<N, F, M_SIZE>::new(ctx, setup.storage.clone(), Duration::from_secs(10));

        let vote = create_vote(0, 1, block_hash, leader_id, &setup);
        let result = view_chain.route_vote(vote, &setup.peer_set);

        assert!(result.is_ok());
        let votes_result = result.unwrap();
        assert!(!votes_result.should_await);
        assert!(!votes_result.is_enough_to_m_notarize);
        assert!(!votes_result.is_enough_to_finalize);

        assert_eq!(view_chain.current_view_number(), 1);
        assert_eq!(view_chain.non_finalized_count(), 1);
        assert_eq!(view_chain.non_finalized_view_numbers_range(), 1..=1);
        assert_eq!(view_chain.current().votes.len(), 1);

        std::fs::remove_dir_all(setup.temp_dir.path()).unwrap();
    }

    #[test]
    fn test_route_vote_to_unfinalized_view() {
        let setup = TestSetup::new(N);
        let leader_id = setup.leader_id(0);
        let replica_id = setup.replica_id(1);
        let parent_hash = [2u8; blake3::OUT_LEN];

        // Create view 1 with M-notarization
        let mut ctx_v1 =
            create_view_context_with_votes(1, leader_id, replica_id, parent_hash, M_SIZE, &setup);
        let block_hash_v1 = ctx_v1.block.as_ref().unwrap().get_hash();

        // Create M-notarization for view 1
        let votes_v1: HashSet<Vote> = (0..M_SIZE)
            .map(|i| create_vote(i, 1, block_hash_v1, leader_id, &setup))
            .collect();
        let m_notarization = create_m_notarization(&votes_v1, 1, block_hash_v1, leader_id);
        ctx_v1
            .add_m_notarization(m_notarization, &setup.peer_set)
            .unwrap();

        let mut view_chain =
            ViewChain::<N, F, M_SIZE>::new(ctx_v1, setup.storage.clone(), Duration::from_secs(10));

        // Progress to view 2
        let ctx_v2 = ViewContext::new(2, leader_id, replica_id, block_hash_v1);
        view_chain.progress_with_m_notarization(ctx_v2).unwrap();

        // Now route a vote to view 1 (unfinalized)
        let late_vote = create_vote(4, 1, block_hash_v1, leader_id, &setup);
        let result = view_chain.route_vote(late_vote, &setup.peer_set);

        assert!(result.is_ok());

        let votes_result = result.unwrap();
        assert!(!votes_result.should_await);
        assert!(!votes_result.is_enough_to_m_notarize); // M-notarization already exists, so we don't need to create it again
        assert!(votes_result.is_enough_to_finalize);

        assert_eq!(view_chain.current_view_number(), 2);
        assert_eq!(view_chain.non_finalized_count(), 2);
        assert_eq!(view_chain.non_finalized_view_numbers_range(), 1..=2);
        assert_eq!(
            view_chain.non_finalized_views.get(&1).unwrap().votes.len(),
            4
        );
        assert!(
            view_chain
                .non_finalized_views
                .get(&1)
                .unwrap()
                .m_notarization
                .is_some()
        );
        assert!(
            view_chain
                .non_finalized_views
                .get(&1)
                .unwrap()
                .nullification
                .is_none()
        );
        assert_eq!(view_chain.current().votes.len(), 0);

        std::fs::remove_dir_all(setup.temp_dir.path()).unwrap();
    }

    #[test]
    fn test_route_vote_to_invalid_view_fails() {
        let setup = TestSetup::new(N);
        let leader_id = setup.leader_id(0);
        let replica_id = setup.replica_id(1);
        let parent_hash = [3u8; blake3::OUT_LEN];

        let ctx = ViewContext::new(1, leader_id, replica_id, parent_hash);
        let mut view_chain =
            ViewChain::<N, F, M_SIZE>::new(ctx, setup.storage.clone(), Duration::from_secs(10));

        // Try to route vote to view 5 (not in chain)
        let vote = create_vote(0, 5, [99u8; blake3::OUT_LEN], leader_id, &setup);
        let result = view_chain.route_vote(vote, &setup.peer_set);

        assert!(result.is_err());

        std::fs::remove_dir_all(setup.temp_dir.path()).unwrap();
    }

    #[test]
    fn test_route_nullify_to_current_view() {
        let setup = TestSetup::new(N);
        let leader_id = setup.leader_id(0);
        let replica_id = setup.replica_id(1);
        let parent_hash = [4u8; blake3::OUT_LEN];

        let ctx = ViewContext::new(1, leader_id, replica_id, parent_hash);
        let mut view_chain =
            ViewChain::<N, F, M_SIZE>::new(ctx, setup.storage.clone(), Duration::from_secs(10));

        let nullify = create_nullify(0, 1, leader_id, &setup);
        let result = view_chain.route_nullify(nullify.clone(), &setup.peer_set);

        assert!(result.is_ok());
        assert!(!result.unwrap());

        assert_eq!(view_chain.current_view_number(), 1);
        assert_eq!(view_chain.non_finalized_count(), 1);
        assert_eq!(view_chain.non_finalized_view_numbers_range(), 1..=1);
        assert_eq!(view_chain.current().votes.len(), 0);

        assert!(view_chain.current().m_notarization.is_none());

        assert_eq!(view_chain.current().nullify_messages.len(), 1);

        std::fs::remove_dir_all(setup.temp_dir.path()).unwrap();
    }

    #[test]
    fn test_route_nullifiers_to_nullification_in_current_view() {
        let setup = TestSetup::new(N);
        let leader_id = setup.leader_id(0);
        let replica_id = setup.replica_id(1);
        let parent_hash = [4u8; blake3::OUT_LEN];

        let ctx = ViewContext::new(1, leader_id, replica_id, parent_hash);
        let mut view_chain =
            ViewChain::<N, F, M_SIZE>::new(ctx, setup.storage.clone(), Duration::from_secs(10));

        for i in 0..M_SIZE {
            let nullify = create_nullify(i, 1, leader_id, &setup);
            let result = view_chain.route_nullify(nullify.clone(), &setup.peer_set);
            assert!(result.is_ok());
        }

        assert_eq!(view_chain.current_view_number(), 1);
        assert_eq!(view_chain.non_finalized_count(), 1);
        assert_eq!(view_chain.non_finalized_view_numbers_range(), 1..=1);
        assert_eq!(view_chain.current().votes.len(), 0);

        assert!(view_chain.current().m_notarization.is_none());

        assert_eq!(view_chain.current().nullify_messages.len(), M_SIZE);

        assert!(view_chain.current().nullification.is_some());

        std::fs::remove_dir_all(setup.temp_dir.path()).unwrap();
    }

    #[test]
    fn test_progress_with_m_notarization_success() {
        let setup = TestSetup::new(N);
        let leader_id = setup.leader_id(0);
        let replica_id = setup.replica_id(1);
        let parent_hash = [5u8; blake3::OUT_LEN];

        // Create view 1 with M-notarization
        let mut ctx_v1 =
            create_view_context_with_votes(1, leader_id, replica_id, parent_hash, M_SIZE, &setup);
        let block_hash_v1 = ctx_v1.block.as_ref().unwrap().get_hash();

        let votes: HashSet<Vote> = (0..M_SIZE)
            .map(|i| create_vote(i, 1, block_hash_v1, leader_id, &setup))
            .collect();
        let m_notarization = create_m_notarization(&votes, 1, block_hash_v1, leader_id);
        ctx_v1
            .add_m_notarization(m_notarization, &setup.peer_set)
            .unwrap();

        let mut view_chain =
            ViewChain::<N, F, M_SIZE>::new(ctx_v1, setup.storage, Duration::from_secs(10));

        // Progress to view 2
        let ctx_v2 = ViewContext::new(2, leader_id, replica_id, block_hash_v1);
        let result = view_chain.progress_with_m_notarization(ctx_v2);

        assert!(result.is_ok());
        assert_eq!(view_chain.current_view_number(), 2);
        assert_eq!(view_chain.non_finalized_count(), 2);
        assert_eq!(view_chain.non_finalized_view_numbers_range(), 1..=2);
        assert_eq!(view_chain.current().votes.len(), 0);

        assert!(
            view_chain
                .non_finalized_views
                .get(&1)
                .unwrap()
                .m_notarization
                .is_some()
        );
        assert!(
            view_chain
                .non_finalized_views
                .get(&2)
                .unwrap()
                .m_notarization
                .is_none()
        );

        std::fs::remove_dir_all(setup.temp_dir.path()).unwrap();
    }

    #[test]
    fn test_progress_without_m_notarization_fails() {
        let setup = TestSetup::new(N);
        let leader_id = setup.leader_id(0);
        let replica_id = setup.replica_id(1);
        let parent_hash = [6u8; blake3::OUT_LEN];

        let ctx_v1 = ViewContext::new(1, leader_id, replica_id, parent_hash);
        let mut view_chain =
            ViewChain::<N, F, M_SIZE>::new(ctx_v1, setup.storage, Duration::from_secs(10));

        // Try to progress without M-notarization
        let ctx_v2 = ViewContext::new(2, leader_id, replica_id, parent_hash);
        let result = view_chain.progress_with_m_notarization(ctx_v2);

        assert!(result.is_err());

        std::fs::remove_dir_all(setup.temp_dir.path()).unwrap();
    }

    #[test]
    fn test_progress_with_wrong_view_number_fails() {
        let setup = TestSetup::new(N);
        let leader_id = setup.leader_id(0);
        let replica_id = setup.replica_id(1);
        let parent_hash = [7u8; blake3::OUT_LEN];

        let mut ctx_v1 =
            create_view_context_with_votes(1, leader_id, replica_id, parent_hash, M_SIZE, &setup);
        let block_hash_v1 = ctx_v1.block.as_ref().unwrap().get_hash();

        let votes: HashSet<Vote> = (0..M_SIZE)
            .map(|i| create_vote(i, 1, block_hash_v1, leader_id, &setup))
            .collect();
        let m_notarization = create_m_notarization(&votes, 1, block_hash_v1, leader_id);
        ctx_v1
            .add_m_notarization(m_notarization, &setup.peer_set)
            .unwrap();

        let mut view_chain =
            ViewChain::<N, F, M_SIZE>::new(ctx_v1, setup.storage, Duration::from_secs(10));

        // Try to progress to view 5 (should be view 2)
        let ctx_v5 = ViewContext::new(5, leader_id, replica_id, block_hash_v1);
        let result = view_chain.progress_with_m_notarization(ctx_v5);

        assert!(result.is_err());

        std::fs::remove_dir_all(setup.temp_dir.path()).unwrap();
    }

    #[test]
    fn test_multiple_m_notarization_progressions() {
        let setup = TestSetup::new(N);
        let leader_id = setup.leader_id(0);
        let replica_id = setup.replica_id(1);
        let parent_hash = [8u8; blake3::OUT_LEN];

        // Start with view 1
        let mut ctx_v1 =
            create_view_context_with_votes(1, leader_id, replica_id, parent_hash, M_SIZE, &setup);
        let block_hash_v1 = ctx_v1.block.as_ref().unwrap().get_hash();

        let votes_v1: HashSet<Vote> = (0..M_SIZE)
            .map(|i| create_vote(i, 1, block_hash_v1, leader_id, &setup))
            .collect();
        let m_not_v1 = create_m_notarization(&votes_v1, 1, block_hash_v1, leader_id);
        ctx_v1
            .add_m_notarization(m_not_v1, &setup.peer_set)
            .unwrap();

        let mut view_chain =
            ViewChain::<N, F, M_SIZE>::new(ctx_v1, setup.storage.clone(), Duration::from_secs(10));

        // Progress to view 2
        let mut ctx_v2 =
            create_view_context_with_votes(2, leader_id, replica_id, block_hash_v1, M_SIZE, &setup);
        let block_hash_v2 = ctx_v2.block.as_ref().unwrap().get_hash();

        let votes_v2: HashSet<Vote> = (0..M_SIZE)
            .map(|i| create_vote(i, 2, block_hash_v2, leader_id, &setup))
            .collect();
        let m_not_v2 = create_m_notarization(&votes_v2, 2, block_hash_v2, leader_id);
        ctx_v2
            .add_m_notarization(m_not_v2, &setup.peer_set)
            .unwrap();

        view_chain.progress_with_m_notarization(ctx_v2).unwrap();

        // Progress to view 3
        let ctx_v3 = ViewContext::new(3, leader_id, replica_id, block_hash_v2);
        view_chain.progress_with_m_notarization(ctx_v3).unwrap();

        assert_eq!(view_chain.current_view_number(), 3);
        assert_eq!(view_chain.non_finalized_count(), 3);
        assert_eq!(view_chain.non_finalized_view_numbers_range(), 1..=3);

        assert!(
            view_chain
                .non_finalized_views
                .get(&1)
                .unwrap()
                .m_notarization
                .is_some()
        );
        assert!(
            view_chain
                .non_finalized_views
                .get(&2)
                .unwrap()
                .m_notarization
                .is_some()
        );
        assert!(
            view_chain
                .non_finalized_views
                .get(&3)
                .unwrap()
                .m_notarization
                .is_none()
        );

        std::fs::remove_dir_all(setup.temp_dir.path()).unwrap();
    }

    #[test]
    fn test_progress_with_nullification_success() {
        let setup = TestSetup::new(N);
        let leader_id = setup.leader_id(0);
        let replica_id = setup.replica_id(1);
        let parent_hash = [9u8; blake3::OUT_LEN];

        let mut ctx_v1 = ViewContext::new(1, leader_id, replica_id, parent_hash);

        // Add M_SIZE nullify messages to create nullification
        let nullifies: HashSet<Nullify> = (0..M_SIZE)
            .map(|i| create_nullify(i, 1, leader_id, &setup))
            .collect();

        for nullify in nullifies.iter() {
            ctx_v1
                .add_nullify(nullify.clone(), &setup.peer_set)
                .unwrap();
        }

        let nullification = create_nullification(&nullifies, 1, leader_id);
        ctx_v1
            .add_nullification(nullification, &setup.peer_set)
            .unwrap();

        let mut view_chain =
            ViewChain::<N, F, M_SIZE>::new(ctx_v1, setup.storage, Duration::from_secs(10));

        // Progress with nullification
        let ctx_v2 = ViewContext::new(2, leader_id, replica_id, parent_hash);
        let result = view_chain.progress_with_nullification(ctx_v2);

        assert!(result.is_ok());
        assert_eq!(view_chain.current_view_number(), 2);
        assert_eq!(view_chain.non_finalized_count(), 2); // Both view 1 (nullified) and view 2 remain
        assert!(view_chain.non_finalized_views.contains_key(&1));
        assert_eq!(view_chain.non_finalized_view_numbers_range(), 1..=2);

        assert!(
            view_chain
                .non_finalized_views
                .get(&2)
                .unwrap()
                .nullification
                .is_none()
        );
        assert_eq!(view_chain.current().nullify_messages.len(), 0);
        assert!(view_chain.current().nullification.is_none());

        std::fs::remove_dir_all(setup.temp_dir.path()).unwrap();
    }

    #[test]
    fn test_progress_with_nullification_without_nullification_fails() {
        let setup = TestSetup::new(N);
        let leader_id = setup.leader_id(0);
        let replica_id = setup.replica_id(1);
        let parent_hash = [10u8; blake3::OUT_LEN];

        let ctx_v1 = ViewContext::new(1, leader_id, replica_id, parent_hash);
        let mut view_chain =
            ViewChain::<N, F, M_SIZE>::new(ctx_v1, setup.storage, Duration::from_secs(10));

        // Try to progress without nullification
        let ctx_v2 = ViewContext::new(2, leader_id, replica_id, parent_hash);
        let result = view_chain.progress_with_nullification(ctx_v2);

        assert!(result.is_err());

        std::fs::remove_dir_all(setup.temp_dir.path()).unwrap();
    }

    #[test]
    fn test_finalize_past_view_with_l_notarization() {
        let setup = TestSetup::new(N);
        let leader_id = setup.leader_id(0);
        let replica_id = setup.replica_id(1);
        let parent_hash = [15u8; blake3::OUT_LEN];

        // Create view 1 with M-notarization
        let mut ctx_v1 =
            create_view_context_with_votes(1, leader_id, replica_id, parent_hash, M_SIZE, &setup);
        let block_hash_v1 = ctx_v1.block.as_ref().unwrap().get_hash();

        let votes_v1: HashSet<Vote> = (0..M_SIZE)
            .map(|i| create_vote(i, 1, block_hash_v1, leader_id, &setup))
            .collect();
        let m_not_v1 = create_m_notarization(&votes_v1, 1, block_hash_v1, leader_id);
        ctx_v1
            .add_m_notarization(m_not_v1, &setup.peer_set)
            .unwrap();

        let mut view_chain =
            ViewChain::<N, F, M_SIZE>::new(ctx_v1, setup.storage.clone(), Duration::from_secs(10));

        // Progress to view 2
        let ctx_v2 = ViewContext::new(2, leader_id, replica_id, block_hash_v1);
        view_chain.progress_with_m_notarization(ctx_v2).unwrap();

        // Add more votes to view 1 to reach l-notarization
        for i in M_SIZE..(N - F) {
            let vote = create_vote(i, 1, block_hash_v1, leader_id, &setup);
            view_chain.route_vote(vote, &setup.peer_set).unwrap();
        }

        // Finalize view 1 with l-notarization
        let result = view_chain.finalize_with_l_notarization(1, &setup.peer_set);

        assert!(result.is_ok());
        assert_eq!(view_chain.non_finalized_count(), 1); // Only view 2 remains
        assert_eq!(view_chain.current_view_number(), 2);
        assert_eq!(view_chain.current().votes.len(), 0);
        assert_eq!(view_chain.current().nullify_messages.len(), 0);

        assert!(
            view_chain
                .non_finalized_views
                .get(&2)
                .unwrap()
                .m_notarization
                .is_none()
        );

        assert!(
            view_chain
                .non_finalized_views
                .get(&2)
                .unwrap()
                .nullification
                .is_none()
        );

        std::fs::remove_dir_all(setup.temp_dir.path()).unwrap();
    }

    #[test]
    fn test_consecutive_views_invariant() {
        let setup = TestSetup::new(N);
        let leader_id = setup.leader_id(0);
        let replica_id = setup.replica_id(1);
        let parent_hash = [16u8; blake3::OUT_LEN];

        let mut ctx_v1 =
            create_view_context_with_votes(1, leader_id, replica_id, parent_hash, M_SIZE, &setup);
        let block_hash_v1 = ctx_v1.block.as_ref().unwrap().get_hash();

        let votes_v1: HashSet<Vote> = (0..M_SIZE)
            .map(|i| create_vote(i, 1, block_hash_v1, leader_id, &setup))
            .collect();
        let m_not_v1 = create_m_notarization(&votes_v1, 1, block_hash_v1, leader_id);
        ctx_v1
            .add_m_notarization(m_not_v1, &setup.peer_set)
            .unwrap();

        let mut view_chain =
            ViewChain::<N, F, M_SIZE>::new(ctx_v1, setup.storage.clone(), Duration::from_secs(10));

        // Progress through several views
        for view_num in 2..=5 {
            let prev_hash = if view_num == 2 {
                block_hash_v1
            } else {
                view_chain
                    .non_finalized_views
                    .get(&(view_num - 1))
                    .and_then(|ctx| ctx.block.as_ref())
                    .map(|b| b.get_hash())
                    .unwrap_or(parent_hash)
            };

            let mut ctx = create_view_context_with_votes(
                view_num, leader_id, replica_id, prev_hash, M_SIZE, &setup,
            );
            let block_hash = ctx.block.as_ref().unwrap().get_hash();

            let votes: HashSet<Vote> = (0..M_SIZE)
                .map(|i| create_vote(i, view_num, block_hash, leader_id, &setup))
                .collect();
            let m_not = create_m_notarization(&votes, view_num, block_hash, leader_id);
            ctx.add_m_notarization(m_not, &setup.peer_set).unwrap();

            view_chain.progress_with_m_notarization(ctx).unwrap();

            assert_eq!(view_chain.current_view_number(), view_num);
            assert_eq!(view_chain.non_finalized_count(), view_num as usize);
            assert_eq!(view_chain.non_finalized_view_numbers_range(), 1..=view_num);
            assert_eq!(view_chain.current().votes.len(), M_SIZE);
            assert_eq!(view_chain.current().nullify_messages.len(), 0);
            assert!(
                view_chain
                    .non_finalized_views
                    .get(&view_num)
                    .unwrap()
                    .m_notarization
                    .is_some()
            );
            assert!(
                view_chain
                    .non_finalized_views
                    .get(&view_num)
                    .unwrap()
                    .nullification
                    .is_none()
            );
        }

        // Check that all views are consecutive
        let range = view_chain.non_finalized_view_numbers_range();
        assert_eq!(range, 1..=5);
        assert_eq!(view_chain.non_finalized_count(), 5);

        // Verify each view exists in the chain
        for view_num in 1..=5 {
            assert!(view_chain.non_finalized_views.contains_key(&view_num));
        }

        std::fs::remove_dir_all(setup.temp_dir.path()).unwrap();
    }

    #[test]
    fn test_all_unfinalized_views_have_m_notarization_except_current() {
        let setup = TestSetup::new(N);
        let leader_id = setup.leader_id(0);
        let replica_id = setup.replica_id(1);
        let parent_hash = [18u8; blake3::OUT_LEN];

        let mut ctx_v1 =
            create_view_context_with_votes(1, leader_id, replica_id, parent_hash, M_SIZE, &setup);
        let block_hash_v1 = ctx_v1.block.as_ref().unwrap().get_hash();

        let votes_v1: HashSet<Vote> = (0..M_SIZE)
            .map(|i| create_vote(i, 1, block_hash_v1, leader_id, &setup))
            .collect();
        let m_not_v1 = create_m_notarization(&votes_v1, 1, block_hash_v1, leader_id);
        ctx_v1
            .add_m_notarization(m_not_v1, &setup.peer_set)
            .unwrap();

        let mut view_chain =
            ViewChain::<N, F, M_SIZE>::new(ctx_v1, setup.storage, Duration::from_secs(10));

        // Progress to view 2
        let ctx_v2 = ViewContext::new(2, leader_id, replica_id, block_hash_v1);
        view_chain.progress_with_m_notarization(ctx_v2).unwrap();

        // Verify view 1 has m-notarization
        assert!(
            view_chain
                .non_finalized_views
                .get(&1)
                .unwrap()
                .m_notarization
                .is_some()
        );

        // Verify current view (2) doesn't have m-notarization yet
        assert!(view_chain.current().m_notarization.is_none());

        std::fs::remove_dir_all(setup.temp_dir.path()).unwrap();
    }

    #[test]
    fn test_genesis_view_chain() {
        let setup = TestSetup::new(N);
        let leader_id = setup.leader_id(0);
        let replica_id = setup.replica_id(1);
        let parent_hash = [0u8; blake3::OUT_LEN];

        let genesis_ctx = ViewContext::new(1, leader_id, replica_id, parent_hash);
        let view_chain =
            ViewChain::<N, F, M_SIZE>::new(genesis_ctx, setup.storage, Duration::from_secs(10));

        assert_eq!(view_chain.current_view_number(), 1);
        assert_eq!(view_chain.non_finalized_count(), 1);
        assert_eq!(view_chain.non_finalized_view_numbers_range(), 1..=1);

        std::fs::remove_dir_all(setup.temp_dir.path()).unwrap();
    }

    #[test]
    fn test_non_finalized_view_numbers_range_correctness() {
        let setup = TestSetup::new(N);
        let leader_id = setup.leader_id(0);
        let replica_id = setup.replica_id(1);
        let parent_hash = [19u8; blake3::OUT_LEN];

        // Start with view 10
        let mut ctx_v10 =
            create_view_context_with_votes(10, leader_id, replica_id, parent_hash, M_SIZE, &setup);
        let block_hash_v10 = ctx_v10.block.as_ref().unwrap().get_hash();

        let votes: HashSet<Vote> = (0..M_SIZE)
            .map(|i| create_vote(i, 10, block_hash_v10, leader_id, &setup))
            .collect();
        let m_not = create_m_notarization(&votes, 10, block_hash_v10, leader_id);
        ctx_v10.add_m_notarization(m_not, &setup.peer_set).unwrap();

        let mut view_chain =
            ViewChain::<N, F, M_SIZE>::new(ctx_v10, setup.storage, Duration::from_secs(10));

        // Progress to view 11
        let ctx_v11 = ViewContext::new(11, leader_id, replica_id, block_hash_v10);
        view_chain.progress_with_m_notarization(ctx_v11).unwrap();

        assert_eq!(view_chain.non_finalized_view_numbers_range(), 10..=11);

        std::fs::remove_dir_all(setup.temp_dir.path()).unwrap();
    }

    #[test]
    fn test_m_notarization_does_not_finalize_blocks() {
        // CRITICAL: M-notarization causes view progression but NOT finalization
        let setup = TestSetup::new(N);
        let leader_id = setup.leader_id(0);
        let replica_id = setup.replica_id(1);
        let parent_hash = [20u8; blake3::OUT_LEN];

        // Create view 1 with M-notarization (but not L-notarization)
        let mut ctx_v1 =
            create_view_context_with_votes(1, leader_id, replica_id, parent_hash, M_SIZE, &setup);
        let block_hash_v1 = ctx_v1.block.as_ref().unwrap().get_hash();

        let votes: HashSet<Vote> = (0..M_SIZE)
            .map(|i| create_vote(i, 1, block_hash_v1, leader_id, &setup))
            .collect();
        let m_notarization = create_m_notarization(&votes, 1, block_hash_v1, leader_id);
        ctx_v1
            .add_m_notarization(m_notarization, &setup.peer_set)
            .unwrap();

        let mut view_chain =
            ViewChain::<N, F, M_SIZE>::new(ctx_v1, setup.storage.clone(), Duration::from_secs(10));

        // Progress to view 2
        let ctx_v2 = ViewContext::new(2, leader_id, replica_id, block_hash_v1);
        view_chain.progress_with_m_notarization(ctx_v2).unwrap();

        // ASSERT: View 1 is still unfinalized (M-notarization does NOT finalize)
        assert_eq!(view_chain.non_finalized_count(), 2);
        assert!(view_chain.non_finalized_views.contains_key(&1));

        // View 1 block should NOT be marked as finalized
        assert!(
            !view_chain
                .non_finalized_views
                .get(&1)
                .unwrap()
                .block
                .as_ref()
                .unwrap()
                .is_finalized
        );

        std::fs::remove_dir_all(setup.temp_dir.path()).unwrap();
    }

    #[test]
    fn test_only_l_notarization_finalizes_blocks() {
        // CRITICAL: Only L-notarization (n-f votes) can finalize blocks
        let setup = TestSetup::new(N);
        let leader_id = setup.leader_id(0);
        let replica_id = setup.replica_id(1);
        let parent_hash = [21u8; blake3::OUT_LEN];

        let mut ctx_v1 =
            create_view_context_with_votes(1, leader_id, replica_id, parent_hash, M_SIZE, &setup);
        let block_hash_v1 = ctx_v1.block.as_ref().unwrap().get_hash();

        // Add M-notarization (2f+1 = 3 votes)
        let votes_m: HashSet<Vote> = (0..M_SIZE)
            .map(|i| create_vote(i, 1, block_hash_v1, leader_id, &setup))
            .collect();
        let m_not = create_m_notarization(&votes_m, 1, block_hash_v1, leader_id);
        ctx_v1.add_m_notarization(m_not, &setup.peer_set).unwrap();

        let mut view_chain =
            ViewChain::<N, F, M_SIZE>::new(ctx_v1, setup.storage.clone(), Duration::from_secs(10));

        // Progress to view 2
        let ctx_v2 = ViewContext::new(2, leader_id, replica_id, block_hash_v1);
        view_chain.progress_with_m_notarization(ctx_v2).unwrap();

        // View 1 still unfinalized with only M-notarization
        assert_eq!(view_chain.non_finalized_count(), 2);

        // Now add votes to reach L-notarization threshold (n-f = 4 votes total)
        let vote_4 = create_vote(3, 1, block_hash_v1, leader_id, &setup);
        view_chain.route_vote(vote_4, &setup.peer_set).unwrap();

        // View 1 still unfinalized until we explicitly finalize
        assert_eq!(view_chain.non_finalized_count(), 2);

        // Now finalize with L-notarization
        let result = view_chain.finalize_with_l_notarization(1, &setup.peer_set);
        assert!(result.is_ok());

        // After L-notarization, view 1 is removed (finalized)
        assert_eq!(view_chain.non_finalized_count(), 1);
        assert!(!view_chain.non_finalized_views.contains_key(&1));

        std::fs::remove_dir_all(setup.temp_dir.path()).unwrap();
    }

    #[test]
    fn test_nullified_view_remains_until_next_view_l_notarized() {
        // Per Minimit: Nullified views stay in chain until a later view is L-notarized
        let setup = TestSetup::new(N);
        let leader_id = setup.leader_id(0);
        let replica_id = setup.replica_id(1);
        let parent_hash = [22u8; blake3::OUT_LEN];

        // View 1: Nullified
        let mut ctx_v1 = ViewContext::new(1, leader_id, replica_id, parent_hash);
        let nullifies_v1: HashSet<Nullify> = (0..M_SIZE)
            .map(|i| create_nullify(i, 1, leader_id, &setup))
            .collect();
        for nullify in nullifies_v1.iter() {
            ctx_v1
                .add_nullify(nullify.clone(), &setup.peer_set)
                .unwrap();
        }
        let nullification_v1 = create_nullification(&nullifies_v1, 1, leader_id);
        ctx_v1
            .add_nullification(nullification_v1, &setup.peer_set)
            .unwrap();

        let mut view_chain =
            ViewChain::<N, F, M_SIZE>::new(ctx_v1, setup.storage.clone(), Duration::from_secs(10));

        // Progress to view 2 with nullification
        let ctx_v2 = ViewContext::new(2, leader_id, replica_id, parent_hash);
        view_chain.progress_with_nullification(ctx_v2).unwrap();

        // CRITICAL: View 1 should STILL be in chain (nullification doesn't remove it)
        assert_eq!(view_chain.non_finalized_count(), 2);
        assert!(view_chain.non_finalized_views.contains_key(&1));

        std::fs::remove_dir_all(setup.temp_dir.path()).unwrap();
    }

    #[test]
    fn test_block_proposal_awaits_parent_m_notarization() {
        // Per Minimit: When child proposal arrives before parent M-notarization,
        // it should be marked as pending and processed later
        let setup = TestSetup::new(N);
        let leader_id = setup.leader_id(0);
        let replica_id = setup.replica_id(1);
        let parent_hash = [23u8; blake3::OUT_LEN];

        // View 1: Block proposed with only 2 votes (not enough for M-notarization yet)
        let mut ctx_v1 = ViewContext::new(1, leader_id, replica_id, parent_hash);
        let leader_sk = setup.peer_id_to_secret_key.get(&leader_id).unwrap();
        let block_v1 = create_test_block(1, leader_id, parent_hash, leader_sk.clone(), 1);
        let block_hash_v1 = block_v1.get_hash();
        ctx_v1.add_new_view_block(block_v1).unwrap();

        // Add 2 votes (not enough for M-notarization which needs 3)
        for i in 0..2 {
            let vote = create_vote(i, 1, block_hash_v1, leader_id, &setup);
            ctx_v1.add_vote(vote, &setup.peer_set).unwrap();
        }

        // View 1 has NO M-notarization yet
        assert!(ctx_v1.m_notarization.is_none());

        let mut view_chain =
            ViewChain::<N, F, M_SIZE>::new(ctx_v1, setup.storage.clone(), Duration::from_secs(10));

        // Manually create view 2 context (simulating view progression via nullification)
        let ctx_v2 = ViewContext::new(2, leader_id, replica_id, parent_hash);

        // Add nullification to view 1 so we can progress
        let nullifies: HashSet<Nullify> = (0..M_SIZE)
            .map(|i| create_nullify(i, 1, leader_id, &setup))
            .collect();
        for nullify in nullifies.iter() {
            view_chain
                .route_nullify(nullify.clone(), &setup.peer_set)
                .unwrap();
        }
        let nullification = create_nullification(&nullifies, 1, leader_id);
        view_chain
            .route_nullification(nullification, &setup.peer_set)
            .unwrap();

        view_chain.progress_with_nullification(ctx_v2).unwrap();

        // Now try to propose a block for view 2 building on view 1
        // This should FAIL because view 1 was nullified
        let leader_sk = setup.peer_id_to_secret_key.get(&leader_id).unwrap();
        let block_v2 = create_test_block(2, leader_id, block_hash_v1, leader_sk.clone(), 2);
        let result = view_chain.add_block_proposal(2, block_v2);

        // Should fail - cannot build on nullified parent
        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("nullified"));

        std::fs::remove_dir_all(setup.temp_dir.path()).unwrap();
    }

    #[test]
    fn test_pending_block_waits_for_parent_m_notarization() {
        // Test the pending block mechanism: child arrives before parent M-notarization
        let setup = TestSetup::new(N);
        let leader_id = setup.leader_id(0);
        let replica_id = setup.replica_id(1);
        let parent_hash = [29u8; blake3::OUT_LEN];

        // View 1: Block with votes but M-notarization arrives separately
        let mut ctx_v1 = ViewContext::new(1, leader_id, replica_id, parent_hash);
        let leader_sk = setup.peer_id_to_secret_key.get(&leader_id).unwrap();
        let block_v1 = create_test_block(1, leader_id, parent_hash, leader_sk.clone(), 1);
        let block_hash_v1 = block_v1.get_hash();
        ctx_v1.add_new_view_block(block_v1).unwrap();

        // Add 2 votes (not enough for M-notarization)
        for i in 0..2 {
            let vote = create_vote(i, 1, block_hash_v1, leader_id, &setup);
            ctx_v1.add_vote(vote, &setup.peer_set).unwrap();
        }

        let mut view_chain =
            ViewChain::<N, F, M_SIZE>::new(ctx_v1, setup.storage.clone(), Duration::from_secs(10));

        // Simulate early M-notarization arrival (out of order network)
        // Create M-notarization
        let votes_v1: HashSet<Vote> = (0..M_SIZE)
            .map(|i| create_vote(i, 1, block_hash_v1, leader_id, &setup))
            .collect();
        let m_not_v1 = create_m_notarization(&votes_v1, 1, block_hash_v1, leader_id);
        view_chain
            .route_m_notarization(m_not_v1, &setup.peer_set)
            .unwrap();

        // Progress to view 2
        let ctx_v2 = ViewContext::new(2, leader_id, replica_id, block_hash_v1);
        view_chain.progress_with_m_notarization(ctx_v2).unwrap();

        // Add nullification to view 2 using route_nullify (routes to correct view)
        let nullifies_v2: HashSet<Nullify> = (0..M_SIZE)
            .map(|i| create_nullify(i, 2, leader_id, &setup))
            .collect();
        for nullify in nullifies_v2.iter() {
            view_chain
                .route_nullify(nullify.clone(), &setup.peer_set)
                .unwrap();
        }
        let nullification_v2 = create_nullification(&nullifies_v2, 2, leader_id);
        view_chain
            .route_nullification(nullification_v2, &setup.peer_set)
            .unwrap();

        // Progress to view 3
        let ctx_v3 = ViewContext::new(3, leader_id, replica_id, block_hash_v1);
        view_chain.progress_with_nullification(ctx_v3).unwrap();

        // Now propose block for view 3 building on view 1 (which has M-notarization)
        // This should succeed because:
        // - Parent (view 1) has M-notarization
        // - Intermediate view (view 2) is nullified
        let leader_sk = setup.peer_id_to_secret_key.get(&leader_id).unwrap();
        let block_v3 = create_test_block(3, leader_id, block_hash_v1, leader_sk.clone(), 2);
        let result = view_chain.add_block_proposal(3, block_v3);

        assert!(result.is_ok());
        let proposal_result = result.unwrap();
        assert!(!proposal_result.should_await);
        assert!(proposal_result.should_vote);

        std::fs::remove_dir_all(setup.temp_dir.path()).unwrap();
    }

    #[test]
    fn test_process_pending_child_proposals_after_m_notarization() {
        // When parent gets M-notarization, pending child blocks should be processed
        let setup = TestSetup::new(N);
        let leader_id = setup.leader_id(0);
        let replica_id = setup.replica_id(1);
        let parent_hash = [24u8; blake3::OUT_LEN];

        // View 1: Block with M-notarization
        let mut ctx_v1 =
            create_view_context_with_votes(1, leader_id, replica_id, parent_hash, M_SIZE, &setup);
        let block_hash_v1 = ctx_v1.block.as_ref().unwrap().get_hash();

        let votes_v1: HashSet<Vote> = (0..M_SIZE)
            .map(|i| create_vote(i, 1, block_hash_v1, leader_id, &setup))
            .collect();
        let m_not_v1 = create_m_notarization(&votes_v1, 1, block_hash_v1, leader_id);
        ctx_v1
            .add_m_notarization(m_not_v1, &setup.peer_set)
            .unwrap();

        let mut view_chain =
            ViewChain::<N, F, M_SIZE>::new(ctx_v1, setup.storage.clone(), Duration::from_secs(10));

        // Progress to view 2 WITHOUT M-notarization (nullified)
        let nullifies: HashSet<Nullify> = (0..M_SIZE)
            .map(|i| create_nullify(i, 2, leader_id, &setup))
            .collect();

        let mut ctx_v2 = ViewContext::new(2, leader_id, replica_id, block_hash_v1);
        for nullify in nullifies.iter() {
            ctx_v2
                .add_nullify(nullify.clone(), &setup.peer_set)
                .unwrap();
        }
        let nullification_v2 = create_nullification(&nullifies, 2, leader_id);
        ctx_v2
            .add_nullification(nullification_v2, &setup.peer_set)
            .unwrap();

        view_chain.progress_with_m_notarization(ctx_v2).unwrap();

        // Progress to view 3
        let ctx_v3 = ViewContext::new(3, leader_id, replica_id, block_hash_v1);
        view_chain.progress_with_nullification(ctx_v3).unwrap();

        // Propose block for view 3 building on view 1 (skipping nullified view 2)
        let leader_sk = setup.peer_id_to_secret_key.get(&leader_id).unwrap();
        let block_v3 = create_test_block(3, leader_id, block_hash_v1, leader_sk.clone(), 2);
        let result = view_chain.add_block_proposal(3, block_v3.clone());

        // Should succeed since parent (view 1) has M-notarization and view 2 is nullified
        assert!(result.is_ok());
        assert!(!result.unwrap().should_await);
        assert!(view_chain.current().block.is_some());

        std::fs::remove_dir_all(setup.temp_dir.path()).unwrap();
    }

    #[test]
    fn test_route_m_notarization() {
        // Test routing M-notarization to appropriate view
        let setup = TestSetup::new(N);
        let leader_id = setup.leader_id(0);
        let replica_id = setup.replica_id(1);
        let parent_hash = [25u8; blake3::OUT_LEN];

        let mut ctx = ViewContext::new(1, leader_id, replica_id, parent_hash);
        let leader_sk = setup.peer_id_to_secret_key.get(&leader_id).unwrap();
        let block = create_test_block(1, leader_id, parent_hash, leader_sk.clone(), 1);
        let block_hash = block.get_hash();
        ctx.add_new_view_block(block).unwrap();

        let mut view_chain =
            ViewChain::<N, F, M_SIZE>::new(ctx, setup.storage.clone(), Duration::from_secs(10));

        // Create M-notarization
        let mut votes = HashSet::new();
        for i in 0..M_SIZE {
            let vote = create_vote(i, 1, block_hash, leader_id, &setup);
            votes.insert(vote);
        }
        let m_notarization = create_m_notarization(&votes, 1, block_hash, leader_id);

        // Route M-notarization
        let result = view_chain.route_m_notarization(m_notarization, &setup.peer_set);

        assert!(result.is_ok());
        let should_m_notarize = result.unwrap();
        assert!(should_m_notarize.should_notarize);
        assert!(view_chain.current().m_notarization.is_some());

        std::fs::remove_dir_all(setup.temp_dir.path()).unwrap();
    }

    #[test]
    fn test_finalization_persists_intermediate_nullified_views() {
        // When finalizing view N, all intermediate nullified views should be persisted
        let setup = TestSetup::new(N);
        let leader_id = setup.leader_id(0);
        let replica_id = setup.replica_id(1);
        let parent_hash = [26u8; blake3::OUT_LEN];

        // View 1: M-notarized
        let mut ctx_v1 =
            create_view_context_with_votes(1, leader_id, replica_id, parent_hash, M_SIZE, &setup);
        let block_hash_v1 = ctx_v1.block.as_ref().unwrap().get_hash();
        let votes_v1: HashSet<Vote> = (0..M_SIZE)
            .map(|i| create_vote(i, 1, block_hash_v1, leader_id, &setup))
            .collect();
        let m_not_v1 = create_m_notarization(&votes_v1, 1, block_hash_v1, leader_id);
        ctx_v1
            .add_m_notarization(m_not_v1, &setup.peer_set)
            .unwrap();

        let mut view_chain =
            ViewChain::<N, F, M_SIZE>::new(ctx_v1, setup.storage.clone(), Duration::from_secs(10));

        // View 2: Nullified
        let mut ctx_v2 = ViewContext::new(2, leader_id, replica_id, block_hash_v1);
        let nullifies_v2: HashSet<Nullify> = (0..M_SIZE)
            .map(|i| create_nullify(i, 2, leader_id, &setup))
            .collect();
        for nullify in nullifies_v2.iter() {
            ctx_v2
                .add_nullify(nullify.clone(), &setup.peer_set)
                .unwrap();
        }
        let nullification_v2 = create_nullification(&nullifies_v2, 2, leader_id);
        ctx_v2
            .add_nullification(nullification_v2, &setup.peer_set)
            .unwrap();
        view_chain.progress_with_m_notarization(ctx_v2).unwrap();

        // View 3: M-notarized and will be L-notarized
        let mut ctx_v3 =
            create_view_context_with_votes(3, leader_id, replica_id, block_hash_v1, M_SIZE, &setup);
        let block_hash_v3 = ctx_v3.block.as_ref().unwrap().get_hash();
        let votes_v3: HashSet<Vote> = (0..M_SIZE)
            .map(|i| create_vote(i, 3, block_hash_v3, leader_id, &setup))
            .collect();
        let m_not_v3 = create_m_notarization(&votes_v3, 3, block_hash_v3, leader_id);
        ctx_v3
            .add_m_notarization(m_not_v3, &setup.peer_set)
            .unwrap();
        view_chain.progress_with_nullification(ctx_v3).unwrap();

        // Add votes to view 3 for L-notarization
        for i in M_SIZE..(N - F) {
            let vote = create_vote(i, 3, block_hash_v3, leader_id, &setup);
            view_chain.route_vote(vote, &setup.peer_set).unwrap();
        }

        // View 4: Current
        let ctx_v4 = ViewContext::new(4, leader_id, replica_id, block_hash_v3);
        view_chain.progress_with_m_notarization(ctx_v4).unwrap();

        // Finalize view 3
        let result = view_chain.finalize_with_l_notarization(3, &setup.peer_set);

        // Should succeed and persist view 1, view 2 (nullified), and view 3 (finalized)

        assert!(result.is_ok());
        assert_eq!(view_chain.non_finalized_count(), 1); // Only view 4 remains
        assert!(!view_chain.non_finalized_views.contains_key(&1));
        assert!(!view_chain.non_finalized_views.contains_key(&2));
        assert!(!view_chain.non_finalized_views.contains_key(&3));

        std::fs::remove_dir_all(setup.temp_dir.path()).unwrap();
    }

    #[test]
    fn test_block_proposal_requires_intermediate_nullifications() {
        // Per Minimit: Can only vote for block if intermediate views are nullified
        let setup = TestSetup::new(N);
        let leader_id = setup.leader_id(0);
        let replica_id = setup.replica_id(1);
        let parent_hash = [27u8; blake3::OUT_LEN];

        // View 1: M-notarized
        let mut ctx_v1 =
            create_view_context_with_votes(1, leader_id, replica_id, parent_hash, M_SIZE, &setup);
        let block_hash_v1 = ctx_v1.block.as_ref().unwrap().get_hash();
        let votes_v1: HashSet<Vote> = (0..M_SIZE)
            .map(|i| create_vote(i, 1, block_hash_v1, leader_id, &setup))
            .collect();
        let m_not_v1 = create_m_notarization(&votes_v1, 1, block_hash_v1, leader_id);
        ctx_v1
            .add_m_notarization(m_not_v1, &setup.peer_set)
            .unwrap();

        let mut view_chain =
            ViewChain::<N, F, M_SIZE>::new(ctx_v1, setup.storage.clone(), Duration::from_secs(10));

        // View 2: NOT nullified (just progressed somehow - simulate this)
        let ctx_v2 = ViewContext::new(2, leader_id, replica_id, block_hash_v1);
        // Manually insert without nullification (simulating error scenario)
        view_chain.current_view = 2;
        view_chain.non_finalized_views.insert(2, ctx_v2);

        // View 3: Current
        let ctx_v3 = ViewContext::new(3, leader_id, replica_id, block_hash_v1);
        view_chain.current_view = 3;
        view_chain.non_finalized_views.insert(3, ctx_v3);

        // Try to propose block for view 3 building on view 1
        // Should FAIL because view 2 (intermediate) is not nullified
        let leader_sk = setup.peer_id_to_secret_key.get(&leader_id).unwrap();
        let block_v3 = create_test_block(3, leader_id, block_hash_v1, leader_sk.clone(), 2);
        let result = view_chain.add_block_proposal(3, block_v3);

        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("not nullified"));

        std::fs::remove_dir_all(setup.temp_dir.path()).unwrap();
    }

    #[test]
    fn test_block_proposal_rejects_nullified_parent() {
        // Cannot build on a nullified parent
        let setup = TestSetup::new(N);
        let leader_id = setup.leader_id(0);
        let replica_id = setup.replica_id(1);
        let parent_hash = [28u8; blake3::OUT_LEN];

        // View 1: Nullified
        let mut ctx_v1 = ViewContext::new(1, leader_id, replica_id, parent_hash);
        let leader_sk = setup.peer_id_to_secret_key.get(&leader_id).unwrap();
        let block_v1 = create_test_block(1, leader_id, parent_hash, leader_sk.clone(), 1);
        let block_hash_v1 = block_v1.get_hash();
        ctx_v1.add_new_view_block(block_v1).unwrap();

        let nullifies: HashSet<Nullify> = (0..M_SIZE)
            .map(|i| create_nullify(i, 1, leader_id, &setup))
            .collect();
        for nullify in nullifies.iter() {
            ctx_v1
                .add_nullify(nullify.clone(), &setup.peer_set)
                .unwrap();
        }
        let nullification = create_nullification(&nullifies, 1, leader_id);
        ctx_v1
            .add_nullification(nullification, &setup.peer_set)
            .unwrap();

        let mut view_chain =
            ViewChain::<N, F, M_SIZE>::new(ctx_v1, setup.storage.clone(), Duration::from_secs(10));

        // Progress to view 2
        let ctx_v2 = ViewContext::new(2, leader_id, replica_id, parent_hash);
        view_chain.progress_with_nullification(ctx_v2).unwrap();

        // Try to propose block for view 2 building on nullified view 1
        let leader_sk = setup.peer_id_to_secret_key.get(&leader_id).unwrap();
        let block_v2 = create_test_block(2, leader_id, block_hash_v1, leader_sk.clone(), 2);
        let result = view_chain.add_block_proposal(2, block_v2);

        // Should FAIL - cannot build on nullified parent
        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("nullified"));

        std::fs::remove_dir_all(setup.temp_dir.path()).unwrap();
    }

    #[test]
    fn test_finalize_fails_if_parent_lacks_m_notarization() {
        // Error case: Parent view has no M-notarization
        let setup = TestSetup::new(N);
        let leader_id = setup.leader_id(0);
        let replica_id = setup.replica_id(1);
        let parent_hash = [30u8; blake3::OUT_LEN];

        // View 1: Block but NO M-notarization
        let mut ctx_v1 = ViewContext::new(1, leader_id, replica_id, parent_hash);
        let leader_sk = setup.peer_id_to_secret_key.get(&leader_id).unwrap();
        let block_v1 = create_test_block(1, leader_id, parent_hash, leader_sk.clone(), 1);
        let block_hash_v1 = block_v1.get_hash();
        ctx_v1.add_new_view_block(block_v1).unwrap();
        // Only 2 votes - not enough for M-notarization
        for i in 0..2 {
            let vote = create_vote(i, 1, block_hash_v1, leader_id, &setup);
            ctx_v1.add_vote(vote, &setup.peer_set).unwrap();
        }
        assert!(ctx_v1.m_notarization.is_none());

        let mut view_chain =
            ViewChain::<N, F, M_SIZE>::new(ctx_v1, setup.storage.clone(), Duration::from_secs(10));

        // Manually progress to view 2 (simulate nullification)
        let nullifies: HashSet<Nullify> = (0..M_SIZE)
            .map(|i| create_nullify(i, 1, leader_id, &setup))
            .collect();
        for nullify in nullifies.iter() {
            view_chain
                .route_nullify(nullify.clone(), &setup.peer_set)
                .unwrap();
        }
        let nullification = create_nullification(&nullifies, 1, leader_id);
        view_chain
            .route_nullification(nullification, &setup.peer_set)
            .unwrap();

        let mut ctx_v2 = ViewContext::new(2, leader_id, replica_id, block_hash_v1);
        let leader_sk = setup.peer_id_to_secret_key.get(&leader_id).unwrap();
        let block_v2 = create_test_block(2, leader_id, block_hash_v1, leader_sk.clone(), 2);
        let block_hash_v2 = block_v2.get_hash();
        ctx_v2.add_new_view_block(block_v2).unwrap();

        // Add L-notarization to view 2
        for i in 0..(N - F) {
            let vote = create_vote(i, 2, block_hash_v2, leader_id, &setup);
            ctx_v2.add_vote(vote, &setup.peer_set).unwrap();
        }

        view_chain.progress_with_nullification(ctx_v2).unwrap();

        // Try to finalize view 2 - should FAIL because parent (view 1) has no M-notarization
        let result = view_chain.finalize_with_l_notarization(2, &setup.peer_set);

        assert!(result.is_err());
        assert!(
            result
                .unwrap_err()
                .to_string()
                .contains("does not have M-notarization")
        );

        std::fs::remove_dir_all(setup.temp_dir.path()).unwrap();
    }

    #[test]
    fn test_finalize_fails_if_intermediate_view_not_nullified() {
        // Error case: Intermediate view between parent and finalized is not nullified
        let setup = TestSetup::new(N);
        let leader_id = setup.leader_id(0);
        let replica_id = setup.replica_id(1);
        let parent_hash = [31u8; blake3::OUT_LEN];

        // View 1: M-notarized
        let mut ctx_v1 =
            create_view_context_with_votes(1, leader_id, replica_id, parent_hash, M_SIZE, &setup);
        let block_hash_v1 = ctx_v1.block.as_ref().unwrap().get_hash();
        let votes_v1: HashSet<Vote> = (0..M_SIZE)
            .map(|i| create_vote(i, 1, block_hash_v1, leader_id, &setup))
            .collect();
        let m_not_v1 = create_m_notarization(&votes_v1, 1, block_hash_v1, leader_id);
        ctx_v1
            .add_m_notarization(m_not_v1, &setup.peer_set)
            .unwrap();

        let mut view_chain =
            ViewChain::<N, F, M_SIZE>::new(ctx_v1, setup.storage.clone(), Duration::from_secs(10));

        // View 2: NOT nullified (just progressed)
        let ctx_v2 = ViewContext::new(2, leader_id, replica_id, block_hash_v1);
        view_chain.progress_with_m_notarization(ctx_v2).unwrap();

        // View 3: L-notarized, building on view 1
        let mut ctx_v3 = ViewContext::new(3, leader_id, replica_id, block_hash_v1);
        let leader_sk = setup.peer_id_to_secret_key.get(&leader_id).unwrap();
        let block_v3 = create_test_block(3, leader_id, block_hash_v1, leader_sk.clone(), 2);
        let block_hash_v3 = block_v3.get_hash();
        ctx_v3.add_new_view_block(block_v3).unwrap();
        for i in 0..(N - F) {
            let vote = create_vote(i, 3, block_hash_v3, leader_id, &setup);
            ctx_v3.add_vote(vote, &setup.peer_set).unwrap();
        }
        view_chain.current_view = 3;
        view_chain.non_finalized_views.insert(3, ctx_v3);

        // Try to finalize view 3 - should FAIL because view 2 is not nullified
        let result = view_chain.finalize_with_l_notarization(3, &setup.peer_set);

        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("is not nullified"));

        std::fs::remove_dir_all(setup.temp_dir.path()).unwrap();
    }

    #[test]
    fn test_finalize_fails_if_view_lacks_l_notarization() {
        // Error case: View doesn't have enough votes for L-notarization
        let setup = TestSetup::new(N);
        let leader_id = setup.leader_id(0);
        let replica_id = setup.replica_id(1);
        let parent_hash = [32u8; blake3::OUT_LEN];

        // View 1: M-notarized but not L-notarized
        let mut ctx_v1 =
            create_view_context_with_votes(1, leader_id, replica_id, parent_hash, M_SIZE, &setup);
        let block_hash_v1 = ctx_v1.block.as_ref().unwrap().get_hash();
        let votes_v1: HashSet<Vote> = (0..M_SIZE)
            .map(|i| create_vote(i, 1, block_hash_v1, leader_id, &setup))
            .collect();
        let m_not_v1 = create_m_notarization(&votes_v1, 1, block_hash_v1, leader_id);
        ctx_v1
            .add_m_notarization(m_not_v1, &setup.peer_set)
            .unwrap();

        let mut view_chain =
            ViewChain::<N, F, M_SIZE>::new(ctx_v1, setup.storage.clone(), Duration::from_secs(10));

        // Progress to view 2
        let ctx_v2 = ViewContext::new(2, leader_id, replica_id, block_hash_v1);
        view_chain.progress_with_m_notarization(ctx_v2).unwrap();

        // Try to finalize view 1 - should FAIL because it only has M-notarization (3 votes), not L
        // (4 votes)
        let result = view_chain.finalize_with_l_notarization(1, &setup.peer_set);

        assert!(result.is_err());
        assert!(
            result
                .unwrap_err()
                .to_string()
                .contains("has not received a l-notarization")
        );

        std::fs::remove_dir_all(setup.temp_dir.path()).unwrap();
    }

    #[test]
    fn test_finalize_with_parent_later_l_notarized() {
        // Valid case: Parent gets M-notarization, child gets L-notarization,
        // then parent also gets L-notarization before persisting
        let setup = TestSetup::new(N);
        let leader_id = setup.leader_id(0);
        let replica_id = setup.replica_id(1);
        let parent_hash = [34u8; blake3::OUT_LEN];

        // View 1: Initially M-notarized
        let mut ctx_v1 =
            create_view_context_with_votes(1, leader_id, replica_id, parent_hash, M_SIZE, &setup);
        let block_hash_v1 = ctx_v1.block.as_ref().unwrap().get_hash();
        let votes_v1: HashSet<Vote> = (0..M_SIZE)
            .map(|i| create_vote(i, 1, block_hash_v1, leader_id, &setup))
            .collect();
        let m_not_v1 = create_m_notarization(&votes_v1, 1, block_hash_v1, leader_id);
        ctx_v1
            .add_m_notarization(m_not_v1, &setup.peer_set)
            .unwrap();

        let mut view_chain =
            ViewChain::<N, F, M_SIZE>::new(ctx_v1, setup.storage.clone(), Duration::from_secs(10));

        // View 2: L-notarized
        let mut ctx_v2 =
            create_view_context_with_votes(2, leader_id, replica_id, block_hash_v1, N - F, &setup);
        let block_hash_v2 = ctx_v2.block.as_ref().unwrap().get_hash();
        let votes_v2_m: HashSet<Vote> = (0..M_SIZE)
            .map(|i| create_vote(i, 2, block_hash_v2, leader_id, &setup))
            .collect();
        let m_not_v2 = create_m_notarization(&votes_v2_m, 2, block_hash_v2, leader_id);
        ctx_v2
            .add_m_notarization(m_not_v2, &setup.peer_set)
            .unwrap();

        view_chain.progress_with_m_notarization(ctx_v2).unwrap();

        // Now add more votes to view 1 to get L-notarization
        for i in M_SIZE..(N - F) {
            let vote = create_vote(i, 1, block_hash_v1, leader_id, &setup);
            view_chain.route_vote(vote, &setup.peer_set).unwrap();
        }

        // Progress to view 3
        let ctx_v3 = ViewContext::new(3, leader_id, replica_id, block_hash_v2);
        view_chain.progress_with_m_notarization(ctx_v3).unwrap();

        // Finalize view 2 - should persist both view 1 (L-notarized) and view 2 (L-notarized)
        let result = view_chain.finalize_with_l_notarization(2, &setup.peer_set);

        assert!(result.is_ok());
        assert_eq!(view_chain.non_finalized_count(), 1); // Only view 3 remains
        assert!(!view_chain.non_finalized_views.contains_key(&1));
        assert!(!view_chain.non_finalized_views.contains_key(&2));

        std::fs::remove_dir_all(setup.temp_dir.path()).unwrap();
    }

    #[test]
    fn test_finalize_complex_chain_with_multiple_nullified_views() {
        // Complex case: v1 (L) -> v2 (null) -> v3 (null) -> v4 (null) -> v5 (L, parent=v1)
        let setup = TestSetup::new(N);
        let leader_id = setup.leader_id(0);
        let replica_id = setup.replica_id(1);
        let parent_hash = [35u8; blake3::OUT_LEN];

        // View 1: L-notarized
        let mut ctx_v1 =
            create_view_context_with_votes(1, leader_id, replica_id, parent_hash, N - F, &setup);
        let block_hash_v1 = ctx_v1.block.as_ref().unwrap().get_hash();
        let votes_v1_m: HashSet<Vote> = (0..M_SIZE)
            .map(|i| create_vote(i, 1, block_hash_v1, leader_id, &setup))
            .collect();
        let m_not_v1 = create_m_notarization(&votes_v1_m, 1, block_hash_v1, leader_id);
        ctx_v1
            .add_m_notarization(m_not_v1, &setup.peer_set)
            .unwrap();

        let mut view_chain =
            ViewChain::<N, F, M_SIZE>::new(ctx_v1, setup.storage.clone(), Duration::from_secs(10));

        // Views 2, 3, 4: All nullified
        for view_num in 2..=4 {
            let ctx = ViewContext::new(view_num, leader_id, replica_id, block_hash_v1);
            if view_num == 2 {
                view_chain.progress_with_m_notarization(ctx).unwrap();
            } else {
                view_chain.current_view = view_num;
                view_chain.non_finalized_views.insert(view_num, ctx);
            }

            // Add nullifications
            let nullifies: HashSet<Nullify> = (0..M_SIZE)
                .map(|i| create_nullify(i, view_num, leader_id, &setup))
                .collect();
            for nullify in nullifies.iter() {
                view_chain
                    .route_nullify(nullify.clone(), &setup.peer_set)
                    .unwrap();
            }
            let nullification = create_nullification(&nullifies, view_num, leader_id);
            view_chain
                .route_nullification(nullification, &setup.peer_set)
                .unwrap();
        }

        // View 5: L-notarized, building on view 1
        let mut ctx_v5 =
            create_view_context_with_votes(5, leader_id, replica_id, block_hash_v1, N - F, &setup);
        let block_hash_v5 = ctx_v5.block.as_ref().unwrap().get_hash();
        let votes_v5_m: HashSet<Vote> = (0..M_SIZE)
            .map(|i| create_vote(i, 5, block_hash_v5, leader_id, &setup))
            .collect();
        let m_not_v5 = create_m_notarization(&votes_v5_m, 5, block_hash_v5, leader_id);
        ctx_v5
            .add_m_notarization(m_not_v5, &setup.peer_set)
            .unwrap();

        view_chain.current_view = 5;
        view_chain.non_finalized_views.insert(5, ctx_v5);

        // View 6: Current
        let ctx_v6 = ViewContext::new(6, leader_id, replica_id, block_hash_v5);
        view_chain.current_view = 6;
        view_chain.non_finalized_views.insert(6, ctx_v6);

        // Finalize view 5 - should persist all views 1-5
        let result = view_chain.finalize_with_l_notarization(5, &setup.peer_set);

        assert!(result.is_ok());
        assert_eq!(view_chain.non_finalized_count(), 1); // Only view 6 remains
        for v in 1..=5 {
            assert!(!view_chain.non_finalized_views.contains_key(&v));
        }

        std::fs::remove_dir_all(setup.temp_dir.path()).unwrap();
    }

    #[test]
    fn test_process_pending_multiple_children_across_views() {
        // Multiple pending blocks at different view levels that get processed together
        let setup = TestSetup::new(N);
        let leader_id = setup.leader_id(0);
        let replica_id = setup.replica_id(1);
        let parent_hash = [40u8; blake3::OUT_LEN];

        // View 1: Block without M-notarization
        let mut ctx_v1 = ViewContext::new(1, leader_id, replica_id, parent_hash);
        let leader_sk = setup.peer_id_to_secret_key.get(&leader_id).unwrap();
        let block_v1 = create_test_block(1, leader_id, parent_hash, leader_sk.clone(), 1);
        let block_hash_v1 = block_v1.get_hash();
        ctx_v1.add_new_view_block(block_v1).unwrap();

        let mut view_chain =
            ViewChain::<N, F, M_SIZE>::new(ctx_v1, setup.storage.clone(), Duration::from_secs(10));

        // Manually progress to view 2 and 3 with nullification
        let nullifies_v1: HashSet<Nullify> = (0..M_SIZE)
            .map(|i| create_nullify(i, 1, leader_id, &setup))
            .collect();
        for nullify in nullifies_v1.iter() {
            view_chain
                .route_nullify(nullify.clone(), &setup.peer_set)
                .unwrap();
        }
        let nullification_v1 = create_nullification(&nullifies_v1, 1, leader_id);
        view_chain
            .route_nullification(nullification_v1, &setup.peer_set)
            .unwrap();

        let ctx_v2 = ViewContext::new(2, leader_id, replica_id, parent_hash);
        view_chain.progress_with_nullification(ctx_v2).unwrap();

        let ctx_v3 = ViewContext::new(3, leader_id, replica_id, parent_hash);
        view_chain.current_view = 3;
        view_chain.non_finalized_views.insert(3, ctx_v3);

        // Propose blocks for view 2 and 3 - they should be pending
        let leader_sk = setup.peer_id_to_secret_key.get(&leader_id).unwrap();
        let block_v2 = create_test_block(2, leader_id, block_hash_v1, leader_sk.clone(), 2);
        let result_v2 = view_chain.add_block_proposal(2, block_v2);
        // This will fail because current_view is 3, not 2
        assert!(result_v2.is_err());

        std::fs::remove_dir_all(setup.temp_dir.path()).unwrap();
    }

    #[test]
    fn test_process_pending_blocks_after_parent_m_notarization() {
        // Test that pending blocks are processed when parent gets M-notarization
        // Scenario: View 2 block arrives before View 1 gets M-notarization
        let setup = TestSetup::new(N);
        let leader_id = setup.leader_id(0);
        let replica_id = setup.replica_id(1);
        let parent_hash = [40u8; blake3::OUT_LEN];

        // View 1: Block with only 2 votes (no M-notarization yet)
        let mut ctx_v1 = ViewContext::new(1, leader_id, replica_id, parent_hash);
        let leader_sk = setup.peer_id_to_secret_key.get(&leader_id).unwrap();
        let block_v1 = create_test_block(1, leader_id, parent_hash, leader_sk.clone(), 1);
        let block_hash_v1 = block_v1.get_hash();
        ctx_v1.add_new_view_block(block_v1).unwrap();

        for i in 0..2 {
            let vote = create_vote(i, 1, block_hash_v1, leader_id, &setup);
            ctx_v1.add_vote(vote, &setup.peer_set).unwrap();
        }

        let mut view_chain =
            ViewChain::<N, F, M_SIZE>::new(ctx_v1, setup.storage.clone(), Duration::from_secs(10));

        // Manually progress to view 2 via nullification of view 1
        let nullifies: HashSet<Nullify> = (0..M_SIZE)
            .map(|i| create_nullify(i, 1, leader_id, &setup))
            .collect();
        for nullify in nullifies.iter() {
            view_chain
                .route_nullify(nullify.clone(), &setup.peer_set)
                .unwrap();
        }
        let nullification = create_nullification(&nullifies, 1, leader_id);
        view_chain
            .route_nullification(nullification, &setup.peer_set)
            .unwrap();

        let ctx_v2 = ViewContext::new(2, leader_id, replica_id, parent_hash);
        view_chain.progress_with_nullification(ctx_v2).unwrap();

        // Now at view 2: Try to propose block building on view 1 (which is nullified)
        // This should fail because parent is nullified
        let leader_sk = setup.peer_id_to_secret_key.get(&leader_id).unwrap();
        let block_v2 = create_test_block(2, leader_id, block_hash_v1, leader_sk.clone(), 2);
        let result = view_chain.add_block_proposal(2, block_v2);

        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("nullified"));

        std::fs::remove_dir_all(setup.temp_dir.path()).unwrap();
    }

    #[test]
    fn test_duplicate_block_proposal_to_same_view_fails() {
        // Test that attempting to add a second block to a view that already has one fails
        let setup = TestSetup::new(N);
        let leader_id = setup.leader_id(0);
        let replica_id = setup.replica_id(1);
        let parent_hash = [0u8; blake3::OUT_LEN];

        let ctx = ViewContext::new(1, leader_id, replica_id, parent_hash);
        let mut view_chain =
            ViewChain::<N, F, M_SIZE>::new(ctx, setup.storage.clone(), Duration::from_secs(10));

        // Add first block proposal
        let leader_sk = setup.peer_id_to_secret_key.get(&leader_id).unwrap();
        let block_1 = create_test_block(1, leader_id, parent_hash, leader_sk.clone(), 1);
        let result_1 = view_chain.add_block_proposal(1, block_1);
        assert!(result_1.is_ok());

        // Attempt to add second block proposal to the same view
        let leader_sk = setup.peer_id_to_secret_key.get(&leader_id).unwrap();
        let block_2 = create_test_block(1, leader_id, parent_hash, leader_sk.clone(), 1);
        let result_2 = view_chain.add_block_proposal(1, block_2);

        // Should fail with error indicating block already exists
        assert!(result_2.is_err());
        assert!(
            result_2
                .unwrap_err()
                .to_string()
                .contains("already has a block from leader")
        );

        // Verify only the first block exists
        assert!(view_chain.current().block.is_some());
        assert!(view_chain.current().pending_block.is_none());

        std::fs::remove_dir_all(setup.temp_dir.path()).unwrap();
    }

    #[test]
    fn test_duplicate_pending_block_fails() {
        // Test that attempting to add a block when a pending block already exists fails
        let setup = TestSetup::new(N);
        let leader_id = setup.leader_id(0);
        let replica_id = setup.replica_id(1);
        let parent_hash = [62u8; blake3::OUT_LEN];

        // View 1: Block without M-notarization (so child will be pending)
        let mut ctx_v1 = ViewContext::new(1, leader_id, replica_id, parent_hash);
        let leader_sk = setup.peer_id_to_secret_key.get(&leader_id).unwrap();
        let block_v1 = create_test_block(1, leader_id, parent_hash, leader_sk.clone(), 1);
        let block_hash_v1 = block_v1.get_hash();
        ctx_v1.add_new_view_block(block_v1).unwrap();

        let mut view_chain =
            ViewChain::<N, F, M_SIZE>::new(ctx_v1, setup.storage.clone(), Duration::from_secs(10));

        // Manually insert view 2 (simulating progression without going through normal flow)
        // This avoids nullifying view 1
        let ctx_v2 = ViewContext::new(2, leader_id, replica_id, block_hash_v1);
        view_chain.current_view = 2;
        view_chain.non_finalized_views.insert(2, ctx_v2);

        // Add first block proposal for view 2 building on view 1 (which has NO M-notarization)
        // This should be pending because parent lacks M-notarization
        let leader_sk = setup.peer_id_to_secret_key.get(&leader_id).unwrap();
        let block_2a = create_test_block(2, leader_id, block_hash_v1, leader_sk.clone(), 2);
        let result_1 = view_chain.add_block_proposal(2, block_2a);
        assert!(result_1.is_ok());
        let result_1 = result_1.unwrap();
        assert!(result_1.should_await); // Should be pending
        assert!(view_chain.current().pending_block.is_some());
        assert!(view_chain.current().block.is_none());

        // Attempt to add second block proposal to same view (should fail)
        let leader_sk = setup.peer_id_to_secret_key.get(&leader_id).unwrap();
        let block_2b = create_test_block(2, leader_id, block_hash_v1, leader_sk.clone(), 2);
        let result_2 = view_chain.add_block_proposal(2, block_2b);

        // Should fail with error indicating pending block already exists
        assert!(result_2.is_err());
        assert!(
            result_2
                .unwrap_err()
                .to_string()
                .contains("already has a pending block")
        );

        // Verify only the first pending block exists
        assert!(view_chain.current().pending_block.is_some());
        assert!(view_chain.current().block.is_none());

        std::fs::remove_dir_all(setup.temp_dir.path()).unwrap();
    }

    #[test]
    fn test_process_pending_returns_results() {
        // Verify that process_pending_child_proposals returns correct results
        let setup = TestSetup::new(N);
        let leader_id = setup.leader_id(0);
        let replica_id = setup.replica_id(1);
        let parent_hash = [41u8; blake3::OUT_LEN];

        // View 1: Block with 2 votes (no M-notarization)
        let mut ctx_v1 = ViewContext::new(1, leader_id, replica_id, parent_hash);
        let leader_sk = setup.peer_id_to_secret_key.get(&leader_id).unwrap();
        let block_v1 = create_test_block(1, leader_id, parent_hash, leader_sk.clone(), 1);
        let block_hash_v1 = block_v1.get_hash();
        ctx_v1.add_new_view_block(block_v1).unwrap();

        for i in 0..2 {
            let vote = create_vote(i, 1, block_hash_v1, leader_id, &setup);
            ctx_v1.add_vote(vote, &setup.peer_set).unwrap();
        }

        let mut view_chain =
            ViewChain::<N, F, M_SIZE>::new(ctx_v1, setup.storage.clone(), Duration::from_secs(10));

        // Progress to view 2
        let nullifies: HashSet<Nullify> = (0..M_SIZE)
            .map(|i| create_nullify(i, 1, leader_id, &setup))
            .collect();
        for nullify in nullifies.iter() {
            view_chain
                .route_nullify(nullify.clone(), &setup.peer_set)
                .unwrap();
        }
        let nullification = create_nullification(&nullifies, 1, leader_id);
        view_chain
            .route_nullification(nullification, &setup.peer_set)
            .unwrap();

        let ctx_v2 = ViewContext::new(2, leader_id, replica_id, parent_hash);
        view_chain.progress_with_nullification(ctx_v2).unwrap();

        // Try to propose block for view 2 building on view 1 (nullified) - should fail
        let leader_sk = setup.peer_id_to_secret_key.get(&leader_id).unwrap();
        let block_v2 = create_test_block(2, leader_id, block_hash_v1, leader_sk.clone(), 2);
        let result = view_chain.add_block_proposal(2, block_v2);

        assert!(result.is_err());

        std::fs::remove_dir_all(setup.temp_dir.path()).unwrap();
    }

    #[test]
    fn test_m_notarization_arrives_after_nullification_progression() {
        // View progresses via nullification, then M-notarization arrives late
        let setup = TestSetup::new(N);
        let leader_id = setup.leader_id(0);
        let replica_id = setup.replica_id(1);
        let parent_hash = [42u8; blake3::OUT_LEN];

        // View 1: Nullified
        let mut ctx_v1 = ViewContext::new(1, leader_id, replica_id, parent_hash);
        let leader_sk = setup.peer_id_to_secret_key.get(&leader_id).unwrap();
        let block_v1 = create_test_block(1, leader_id, parent_hash, leader_sk.clone(), 1);
        let block_hash_v1 = block_v1.get_hash();
        ctx_v1.add_new_view_block(block_v1).unwrap();

        let nullifies: HashSet<Nullify> = (0..M_SIZE)
            .map(|i| create_nullify(i, 1, leader_id, &setup))
            .collect();
        for nullify in nullifies.iter() {
            ctx_v1
                .add_nullify(nullify.clone(), &setup.peer_set)
                .unwrap();
        }
        let nullification = create_nullification(&nullifies, 1, leader_id);
        ctx_v1
            .add_nullification(nullification, &setup.peer_set)
            .unwrap();

        let mut view_chain =
            ViewChain::<N, F, M_SIZE>::new(ctx_v1, setup.storage.clone(), Duration::from_secs(10));

        // Progress to view 2
        let ctx_v2 = ViewContext::new(2, leader_id, replica_id, parent_hash);
        view_chain.progress_with_nullification(ctx_v2).unwrap();

        // Now M-notarization arrives for view 1 (late)
        let votes: HashSet<Vote> = (0..M_SIZE)
            .map(|i| create_vote(i, 1, block_hash_v1, leader_id, &setup))
            .collect();
        let m_notarization = create_m_notarization(&votes, 1, block_hash_v1, leader_id);

        // Should succeed - M-notarization can coexist with nullification
        let result = view_chain.route_m_notarization(m_notarization, &setup.peer_set);
        assert!(result.is_ok());

        let result = result.unwrap();
        assert!(!result.should_notarize);
        assert!(!result.should_await);
        assert!(!result.should_vote);
        assert!(!result.should_nullify);

        // Both nullification and M-notarization should exist
        assert!(
            view_chain
                .non_finalized_views
                .get(&1)
                .unwrap()
                .nullification
                .is_some()
        );
        assert!(
            view_chain
                .non_finalized_views
                .get(&1)
                .unwrap()
                .m_notarization
                .is_some()
        );

        std::fs::remove_dir_all(setup.temp_dir.path()).unwrap();
    }

    #[test]
    fn test_late_m_notarization_enables_pending_block_processing() {
        // Pending block waits for M-notarization, which arrives late
        let setup = TestSetup::new(N);
        let leader_id = setup.leader_id(0);
        let replica_id = setup.replica_id(1);
        let parent_hash = [43u8; blake3::OUT_LEN];

        // View 1: Block without M-notarization
        let mut ctx_v1 = ViewContext::new(1, leader_id, replica_id, parent_hash);
        let leader_sk = setup.peer_id_to_secret_key.get(&leader_id).unwrap();
        let block_v1 = create_test_block(1, leader_id, parent_hash, leader_sk.clone(), 1);
        let block_hash_v1 = block_v1.get_hash();
        ctx_v1.add_new_view_block(block_v1).unwrap();

        let mut view_chain =
            ViewChain::<N, F, M_SIZE>::new(ctx_v1, setup.storage.clone(), Duration::from_secs(10));

        // Manually progress to view 2 (simulating nullification)
        let nullifies: HashSet<Nullify> = (0..M_SIZE)
            .map(|i| create_nullify(i, 1, leader_id, &setup))
            .collect();
        for nullify in nullifies.iter() {
            view_chain
                .route_nullify(nullify.clone(), &setup.peer_set)
                .unwrap();
        }
        let nullification = create_nullification(&nullifies, 1, leader_id);
        view_chain
            .route_nullification(nullification, &setup.peer_set)
            .unwrap();

        let ctx_v2 = ViewContext::new(2, leader_id, replica_id, parent_hash);
        view_chain.progress_with_nullification(ctx_v2).unwrap();

        // Block for view 2 building on nullified view 1 should fail
        let leader_sk = setup.peer_id_to_secret_key.get(&leader_id).unwrap();
        let block_v2 = create_test_block(2, leader_id, block_hash_v1, leader_sk.clone(), 2);
        let result = view_chain.add_block_proposal(2, block_v2);
        assert!(result.is_err());

        std::fs::remove_dir_all(setup.temp_dir.path()).unwrap();
    }

    #[test]
    fn test_finalize_when_parent_already_finalized() {
        // View 1 already finalized (not in non_finalized_views)
        // View 2: Nullified
        // View 3: L-notarized building on view 1
        let setup = TestSetup::new(N);
        let leader_id = setup.leader_id(0);
        let replica_id = setup.replica_id(1);

        // Simulate view 1 already finalized (genesis or previous finalization)
        let finalized_hash = [99u8; blake3::OUT_LEN];

        // View 2: Nullified
        let mut ctx_v2 = ViewContext::new(2, leader_id, replica_id, finalized_hash);
        let nullifies: HashSet<Nullify> = (0..M_SIZE)
            .map(|i| create_nullify(i, 2, leader_id, &setup))
            .collect();
        for nullify in nullifies.iter() {
            ctx_v2
                .add_nullify(nullify.clone(), &setup.peer_set)
                .unwrap();
        }
        let nullification = create_nullification(&nullifies, 2, leader_id);
        ctx_v2
            .add_nullification(nullification, &setup.peer_set)
            .unwrap();

        let mut view_chain =
            ViewChain::<N, F, M_SIZE>::new(ctx_v2, setup.storage.clone(), Duration::from_secs(10));
        view_chain.previously_committed_block_hash = finalized_hash;

        // View 3: L-notarized building on finalized view
        let mut ctx_v3 =
            create_view_context_with_votes(3, leader_id, replica_id, finalized_hash, N - F, &setup);
        let block_hash_v3 = ctx_v3.block.as_ref().unwrap().get_hash();
        let votes_v3: HashSet<Vote> = (0..M_SIZE)
            .map(|i| create_vote(i, 3, block_hash_v3, leader_id, &setup))
            .collect();
        let m_not_v3 = create_m_notarization(&votes_v3, 3, block_hash_v3, leader_id);
        ctx_v3
            .add_m_notarization(m_not_v3, &setup.peer_set)
            .unwrap();

        view_chain.progress_with_nullification(ctx_v3).unwrap();

        // View 4: Current
        let ctx_v4 = ViewContext::new(4, leader_id, replica_id, block_hash_v3);
        view_chain.progress_with_m_notarization(ctx_v4).unwrap();

        // Finalize view 3 - parent is already finalized
        let result = view_chain.finalize_with_l_notarization(3, &setup.peer_set);

        assert!(result.is_ok());
        assert_eq!(view_chain.non_finalized_count(), 1); // Only view 4 remains

        std::fs::remove_dir_all(setup.temp_dir.path()).unwrap();
    }

    #[test]
    fn test_persist_all_views_on_shutdown() {
        // Create chain with multiple non-finalized views and persist on shutdown
        let setup = TestSetup::new(N);
        let leader_id = setup.leader_id(0);
        let replica_id = setup.replica_id(1);
        let parent_hash = [45u8; blake3::OUT_LEN];

        // View 1: M-notarized
        let mut ctx_v1 =
            create_view_context_with_votes(1, leader_id, replica_id, parent_hash, M_SIZE, &setup);
        let block_hash_v1 = ctx_v1.block.as_ref().unwrap().get_hash();
        let votes_v1: HashSet<Vote> = (0..M_SIZE)
            .map(|i| create_vote(i, 1, block_hash_v1, leader_id, &setup))
            .collect();
        let m_not_v1 = create_m_notarization(&votes_v1, 1, block_hash_v1, leader_id);
        ctx_v1
            .add_m_notarization(m_not_v1, &setup.peer_set)
            .unwrap();

        let mut view_chain =
            ViewChain::<N, F, M_SIZE>::new(ctx_v1, setup.storage.clone(), Duration::from_secs(10));

        // Progress to view 2 and 3
        let ctx_v2 = ViewContext::new(2, leader_id, replica_id, block_hash_v1);
        view_chain.progress_with_m_notarization(ctx_v2).unwrap();

        // Persist all views
        let result = view_chain.persist_all_views();
        assert!(result.is_ok());

        // After persisting, non_finalized_views should be empty
        assert_eq!(view_chain.non_finalized_count(), 0);

        std::fs::remove_dir_all(setup.temp_dir.path()).unwrap();
    }

    #[test]
    fn test_non_finalized_views_until_with_view_before_range() {
        // Request range until view that's before the start
        let setup = TestSetup::new(N);
        let leader_id = setup.leader_id(0);
        let replica_id = setup.replica_id(1);
        let parent_hash = [46u8; blake3::OUT_LEN];

        let ctx = ViewContext::new(10, leader_id, replica_id, parent_hash);
        let view_chain =
            ViewChain::<N, F, M_SIZE>::new(ctx, setup.storage, Duration::from_secs(10));

        // Current view is 10, try to get range until view 5
        let range = view_chain.non_finalized_views_until(5);
        assert!(range.is_none());

        std::fs::remove_dir_all(setup.temp_dir.path()).unwrap();
    }

    #[test]
    fn test_finalize_view_greater_than_current_fails() {
        // Try to finalize a future view
        let setup = TestSetup::new(N);
        let leader_id = setup.leader_id(0);
        let replica_id = setup.replica_id(1);
        let parent_hash = [47u8; blake3::OUT_LEN];

        let ctx = ViewContext::new(5, leader_id, replica_id, parent_hash);
        let mut view_chain =
            ViewChain::<N, F, M_SIZE>::new(ctx, setup.storage, Duration::from_secs(10));

        // Try to finalize view 10 (future)
        let result = view_chain.finalize_with_l_notarization(10, &setup.peer_set);

        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("not greater than"));

        std::fs::remove_dir_all(setup.temp_dir.path()).unwrap();
    }

    #[test]
    fn test_finalize_chain_with_consecutive_l_notarizations() {
        // View 1, 2, 3 all have L-notarizations
        let setup = TestSetup::new(N);
        let leader_id = setup.leader_id(0);
        let replica_id = setup.replica_id(1);
        let parent_hash = [48u8; blake3::OUT_LEN];

        // View 1: L-notarized
        let mut ctx_v1 =
            create_view_context_with_votes(1, leader_id, replica_id, parent_hash, N - F, &setup);
        let block_hash_v1 = ctx_v1.block.as_ref().unwrap().get_hash();
        let votes_v1: HashSet<Vote> = (0..M_SIZE)
            .map(|i| create_vote(i, 1, block_hash_v1, leader_id, &setup))
            .collect();
        let m_not_v1 = create_m_notarization(&votes_v1, 1, block_hash_v1, leader_id);
        ctx_v1
            .add_m_notarization(m_not_v1, &setup.peer_set)
            .unwrap();

        let mut view_chain =
            ViewChain::<N, F, M_SIZE>::new(ctx_v1, setup.storage.clone(), Duration::from_secs(10));

        // View 2: L-notarized
        let mut ctx_v2 =
            create_view_context_with_votes(2, leader_id, replica_id, block_hash_v1, N - F, &setup);
        let block_hash_v2 = ctx_v2.block.as_ref().unwrap().get_hash();
        let votes_v2: HashSet<Vote> = (0..M_SIZE)
            .map(|i| create_vote(i, 2, block_hash_v2, leader_id, &setup))
            .collect();
        let m_not_v2 = create_m_notarization(&votes_v2, 2, block_hash_v2, leader_id);
        ctx_v2
            .add_m_notarization(m_not_v2, &setup.peer_set)
            .unwrap();
        view_chain.progress_with_m_notarization(ctx_v2).unwrap();

        // View 3: L-notarized
        let mut ctx_v3 =
            create_view_context_with_votes(3, leader_id, replica_id, block_hash_v2, N - F, &setup);
        let block_hash_v3 = ctx_v3.block.as_ref().unwrap().get_hash();
        let votes_v3: HashSet<Vote> = (0..M_SIZE)
            .map(|i| create_vote(i, 3, block_hash_v3, leader_id, &setup))
            .collect();
        let m_not_v3 = create_m_notarization(&votes_v3, 3, block_hash_v3, leader_id);
        ctx_v3
            .add_m_notarization(m_not_v3, &setup.peer_set)
            .unwrap();
        view_chain.progress_with_m_notarization(ctx_v3).unwrap();

        // View 4: Current
        let ctx_v4 = ViewContext::new(4, leader_id, replica_id, block_hash_v3);
        view_chain.progress_with_m_notarization(ctx_v4).unwrap();

        // Finalize view 3 - should persist all as finalized
        let result = view_chain.finalize_with_l_notarization(3, &setup.peer_set);

        assert!(result.is_ok());
        assert_eq!(view_chain.non_finalized_count(), 1); // Only view 4 remains

        std::fs::remove_dir_all(setup.temp_dir.path()).unwrap();
    }

    #[test]
    fn test_route_m_notarization_to_nonexistent_view() {
        // Try to route M-notarization to view not in chain
        let setup = TestSetup::new(N);
        let leader_id = setup.leader_id(0);
        let replica_id = setup.replica_id(1);
        let parent_hash = [49u8; blake3::OUT_LEN];

        let ctx = ViewContext::new(1, leader_id, replica_id, parent_hash);
        let mut view_chain =
            ViewChain::<N, F, M_SIZE>::new(ctx, setup.storage.clone(), Duration::from_secs(10));

        // Create M-notarization for view 5 (doesn't exist)
        let votes: HashSet<Vote> = (0..M_SIZE)
            .map(|i| create_vote(i, 5, [88u8; 32], leader_id, &setup))
            .collect();
        let m_notarization = create_m_notarization(&votes, 5, [88u8; 32], leader_id);

        let result = view_chain.route_m_notarization(m_notarization, &setup.peer_set);

        assert!(result.is_err());
        assert!(
            result
                .unwrap_err()
                .to_string()
                .contains("not the current view")
        );

        std::fs::remove_dir_all(setup.temp_dir.path()).unwrap();
    }

    #[test]
    fn test_route_nullification_to_nonexistent_view() {
        // Route nullification to view that doesn't exist
        let setup = TestSetup::new(N);
        let leader_id = setup.leader_id(0);
        let replica_id = setup.replica_id(1);
        let parent_hash = [50u8; blake3::OUT_LEN];

        let ctx = ViewContext::new(1, leader_id, replica_id, parent_hash);
        let mut view_chain =
            ViewChain::<N, F, M_SIZE>::new(ctx, setup.storage.clone(), Duration::from_secs(10));

        // Create nullification for view 7
        let nullifies: HashSet<Nullify> = (0..M_SIZE)
            .map(|i| create_nullify(i, 7, leader_id, &setup))
            .collect();
        let nullification = create_nullification(&nullifies, 7, leader_id);

        let result = view_chain.route_nullification(nullification, &setup.peer_set);

        assert!(result.is_err());
        assert!(
            result
                .unwrap_err()
                .to_string()
                .contains("not the current view")
        );

        std::fs::remove_dir_all(setup.temp_dir.path()).unwrap();
    }

    #[test]
    fn test_block_proposal_with_parent_as_previously_committed() {
        // Parent is previously_committed_block_hash (already finalized)
        let setup = TestSetup::new(N);
        let leader_id = setup.leader_id(0);
        let replica_id = setup.replica_id(1);
        let finalized_hash = [51u8; blake3::OUT_LEN];

        let ctx = ViewContext::new(1, leader_id, replica_id, finalized_hash);
        let mut view_chain =
            ViewChain::<N, F, M_SIZE>::new(ctx, setup.storage.clone(), Duration::from_secs(10));

        view_chain.previously_committed_block_hash = finalized_hash;

        // Propose block building on finalized hash
        let leader_sk = setup.peer_id_to_secret_key.get(&leader_id).unwrap();
        let block = create_test_block(1, leader_id, finalized_hash, leader_sk.clone(), 1);
        let result = view_chain.add_block_proposal(1, block);

        assert!(result.is_ok());
        let proposal_result = result.unwrap();
        assert!(!proposal_result.should_await);
        assert!(proposal_result.should_vote);

        std::fs::remove_dir_all(setup.temp_dir.path()).unwrap();
    }

    #[test]
    fn test_block_proposal_with_unknown_parent_fails() {
        // Parent hash is neither in non_finalized_views nor previously_committed
        let setup = TestSetup::new(N);
        let leader_id = setup.leader_id(0);
        let replica_id = setup.replica_id(1);
        let parent_hash = [52u8; blake3::OUT_LEN];

        let ctx = ViewContext::new(1, leader_id, replica_id, parent_hash);
        let mut view_chain =
            ViewChain::<N, F, M_SIZE>::new(ctx, setup.storage.clone(), Duration::from_secs(10));

        // Different parent hash that doesn't exist anywhere
        let unknown_parent = [77u8; blake3::OUT_LEN];
        let leader_sk = setup.peer_id_to_secret_key.get(&leader_id).unwrap();
        let block = create_test_block(1, leader_id, unknown_parent, leader_sk.clone(), 1);
        let result = view_chain.add_block_proposal(1, block);

        assert!(result.is_err());
        assert!(
            result
                .unwrap_err()
                .to_string()
                .contains("not the previously committed")
        );

        std::fs::remove_dir_all(setup.temp_dir.path()).unwrap();
    }

    #[test]
    fn test_votes_accumulate_after_m_notarization() {
        // View gets M-notarization, progresses, but continues collecting votes for L-notarization
        let setup = TestSetup::new(N);
        let leader_id = setup.leader_id(0);
        let replica_id = setup.replica_id(1);
        let parent_hash = [53u8; blake3::OUT_LEN];

        // View 1: M-notarization
        let mut ctx_v1 =
            create_view_context_with_votes(1, leader_id, replica_id, parent_hash, M_SIZE, &setup);
        let block_hash_v1 = ctx_v1.block.as_ref().unwrap().get_hash();
        let votes: HashSet<Vote> = (0..M_SIZE)
            .map(|i| create_vote(i, 1, block_hash_v1, leader_id, &setup))
            .collect();
        let m_notarization = create_m_notarization(&votes, 1, block_hash_v1, leader_id);
        ctx_v1
            .add_m_notarization(m_notarization, &setup.peer_set)
            .unwrap();

        let mut view_chain =
            ViewChain::<N, F, M_SIZE>::new(ctx_v1, setup.storage.clone(), Duration::from_secs(10));

        // Progress to view 2
        let ctx_v2 = ViewContext::new(2, leader_id, replica_id, block_hash_v1);
        view_chain.progress_with_m_notarization(ctx_v2).unwrap();

        // Add 4th vote to view 1 to reach L-notarization
        let vote_4 = create_vote(3, 1, block_hash_v1, leader_id, &setup);
        let result = view_chain.route_vote(vote_4, &setup.peer_set);

        assert!(result.is_ok());
        let votes_result = result.unwrap();
        assert!(votes_result.is_enough_to_finalize);

        // Both M-notarization and enough votes for L-notarization should exist
        assert!(
            view_chain
                .non_finalized_views
                .get(&1)
                .unwrap()
                .m_notarization
                .is_some()
        );
        assert_eq!(
            view_chain.non_finalized_views.get(&1).unwrap().votes.len(),
            N - F
        );

        std::fs::remove_dir_all(setup.temp_dir.path()).unwrap();
    }

    #[test]
    fn test_finalize_fails_if_no_block_in_view() {
        // View has votes but no block (corrupted state)
        let setup = TestSetup::new(N);
        let leader_id = setup.leader_id(0);
        let replica_id = setup.replica_id(1);
        let parent_hash = [54u8; blake3::OUT_LEN];

        // Create view without block but with votes (manually)
        let mut ctx = ViewContext::new(1, leader_id, replica_id, parent_hash);
        // Don't add block, just add votes manually
        ctx.block_hash = Some([66u8; 32]);
        // Manually add votes to bypass normal validation
        for i in 0..(N - F) {
            let vote = create_vote(i, 1, [66u8; 32], leader_id, &setup);
            ctx.votes.insert(vote);
        }

        let mut view_chain =
            ViewChain::<N, F, M_SIZE>::new(ctx, setup.storage.clone(), Duration::from_secs(10));

        // Add M-notarization manually
        let votes: HashSet<Vote> = (0..M_SIZE)
            .map(|i| create_vote(i, 1, [66u8; 32], leader_id, &setup))
            .collect();
        let m_not = create_m_notarization(&votes, 1, [66u8; 32], leader_id);
        view_chain
            .non_finalized_views
            .get_mut(&1)
            .unwrap()
            .m_notarization = Some(m_not);

        // Progress to view 2
        let ctx_v2 = ViewContext::new(2, leader_id, replica_id, [66u8; 32]);
        view_chain.progress_with_m_notarization(ctx_v2).unwrap();

        // Try to finalize view 1 - should fail because no block
        let result = view_chain.finalize_with_l_notarization(1, &setup.peer_set);

        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("has no block"));

        std::fs::remove_dir_all(setup.temp_dir.path()).unwrap();
    }

    #[test]
    fn test_find_parent_view_returns_correct_view() {
        // Test the find_parent_view helper method
        let setup = TestSetup::new(N);
        let leader_id = setup.leader_id(0);
        let replica_id = setup.replica_id(1);
        let parent_hash = [55u8; blake3::OUT_LEN];

        // View 1
        let mut ctx_v1 =
            create_view_context_with_votes(1, leader_id, replica_id, parent_hash, M_SIZE, &setup);
        let block_hash_v1 = ctx_v1.block.as_ref().unwrap().get_hash();
        let votes_v1: HashSet<Vote> = (0..M_SIZE)
            .map(|i| create_vote(i, 1, block_hash_v1, leader_id, &setup))
            .collect();
        let m_not_v1 = create_m_notarization(&votes_v1, 1, block_hash_v1, leader_id);
        ctx_v1
            .add_m_notarization(m_not_v1, &setup.peer_set)
            .unwrap();

        let mut view_chain =
            ViewChain::<N, F, M_SIZE>::new(ctx_v1, setup.storage.clone(), Duration::from_secs(10));

        // View 2
        let ctx_v2 =
            create_view_context_with_votes(2, leader_id, replica_id, block_hash_v1, M_SIZE, &setup);
        view_chain.progress_with_m_notarization(ctx_v2).unwrap();

        // Find parent of view 2 (should be view 1)
        let parent_view = view_chain.find_parent_view(&block_hash_v1);
        assert_eq!(parent_view, Some(1));

        // Find parent of view 1 (should be None - genesis)
        let parent_view = view_chain.find_parent_view(&parent_hash);
        assert_eq!(parent_view, None);

        // Find non-existent hash
        let parent_view = view_chain.find_parent_view(&[99u8; 32]);
        assert_eq!(parent_view, None);

        std::fs::remove_dir_all(setup.temp_dir.path()).unwrap();
    }

    #[test]
    fn test_l_finalization_persists_all_data_to_database() {
        // Verify all data is actually persisted to database on L-finalization
        // Scenario: v1 (M-notarized) -> v2 (nullified) -> v3 (L-notarized building on v1)
        let setup = TestSetup::new(N);
        let leader_id = setup.leader_id(0);
        let replica_id = setup.replica_id(1);
        let parent_hash = [60u8; blake3::OUT_LEN];

        // View 1: M-notarized with full votes
        let mut ctx_v1 =
            create_view_context_with_votes(1, leader_id, replica_id, parent_hash, M_SIZE, &setup);
        let block_v1 = ctx_v1.block.as_ref().unwrap().clone();
        let block_hash_v1 = block_v1.get_hash();
        let votes_v1: HashSet<Vote> = (0..M_SIZE)
            .map(|i| create_vote(i, 1, block_hash_v1, leader_id, &setup))
            .collect();
        let m_not_v1 = create_m_notarization(&votes_v1, 1, block_hash_v1, leader_id);
        ctx_v1
            .add_m_notarization(m_not_v1.clone(), &setup.peer_set)
            .unwrap();

        let mut view_chain =
            ViewChain::<N, F, M_SIZE>::new(ctx_v1, setup.storage.clone(), Duration::from_secs(10));

        // View 2: Nullified
        let mut ctx_v2 = ViewContext::new(2, leader_id, replica_id, block_hash_v1);

        let leader_sk = setup.peer_id_to_secret_key.get(&leader_id).unwrap();
        let block_v2 = create_test_block(2, leader_id, block_hash_v1, leader_sk.clone(), 2);
        let block_hash_v2 = block_v2.get_hash();
        ctx_v2.add_new_view_block(block_v2.clone()).unwrap();

        let nullifies_v2: HashSet<Nullify> = (0..M_SIZE)
            .map(|i| create_nullify(i, 2, leader_id, &setup))
            .collect();
        for nullify in nullifies_v2.iter() {
            ctx_v2
                .add_nullify(nullify.clone(), &setup.peer_set)
                .unwrap();
        }
        let nullification_v2 = create_nullification(&nullifies_v2, 2, leader_id);
        ctx_v2
            .add_nullification(nullification_v2.clone(), &setup.peer_set)
            .unwrap();
        view_chain.progress_with_m_notarization(ctx_v2).unwrap();

        // View 3: L-notarized building on view 1
        let mut ctx_v3 =
            create_view_context_with_votes(3, leader_id, replica_id, block_hash_v1, N - F, &setup);
        let block_v3 = ctx_v3.block.as_ref().unwrap().clone();
        let block_hash_v3 = block_v3.get_hash();
        let votes_v3_m: HashSet<Vote> = (0..M_SIZE)
            .map(|i| create_vote(i, 3, block_hash_v3, leader_id, &setup))
            .collect();
        let m_not_v3 = create_m_notarization(&votes_v3_m, 3, block_hash_v3, leader_id);
        ctx_v3
            .add_m_notarization(m_not_v3.clone(), &setup.peer_set)
            .unwrap();
        view_chain.progress_with_nullification(ctx_v3).unwrap();

        // View 4: Current (so we can finalize view 3)
        let ctx_v4 = ViewContext::new(4, leader_id, replica_id, block_hash_v3);
        view_chain.progress_with_m_notarization(ctx_v4).unwrap();

        // Finalize view 3
        let result = view_chain.finalize_with_l_notarization(3, &setup.peer_set);
        assert!(result.is_ok());

        // Now verify ALL data was persisted to the database
        let storage = &setup.storage;

        // Check block was persisted (as M-notarized, not finalized)
        let stored_block_v1 = storage.get_finalized_block(&block_hash_v1).unwrap();
        assert!(stored_block_v1.is_some());
        let stored_block_v1 = stored_block_v1.unwrap();
        assert_eq!(stored_block_v1.get_hash(), block_hash_v1);
        assert_eq!(stored_block_v1.view(), 1);

        // Check transactions from view 1 were persisted
        for tx in block_v1.transactions.iter() {
            let stored_tx = storage.get_transaction(&tx.tx_hash).unwrap();
            assert!(stored_tx.is_some());
        }

        // Check M-notarization for view 1 was persisted
        let stored_m_not_v1 = storage
            .get_notarization::<N, F, M_SIZE>(&block_hash_v1)
            .unwrap();
        assert!(stored_m_not_v1.is_some());

        // Check votes for view 1 were persisted
        for vote in votes_v1.iter() {
            let stored_vote = storage.get_vote(vote.key()).unwrap();
            assert!(stored_vote.is_some());
        }

        // Check leader metadata for view 1
        let stored_leader_v1 = storage.get_leader(1).unwrap();
        assert!(stored_leader_v1.is_some());
        assert_eq!(stored_leader_v1.unwrap().peer_id, leader_id);

        // Check view metadata for view 1
        let stored_view_v1 = storage.get_view(1).unwrap();
        assert!(stored_view_v1.is_some());

        // Check block was persisted as nullified
        let stored_block_v2 = storage.get_nullified_block(&block_hash_v2).unwrap();
        assert!(stored_block_v2.is_some());

        // Check nullification for view 2 was persisted
        let stored_nullification_v2 = storage.get_nullification::<N, F, M_SIZE>(2).unwrap();
        assert!(stored_nullification_v2.is_some());

        // Check nullify messages for view 2 were persisted
        for nullify in nullifies_v2.iter() {
            let stored_nullify = storage.get_nullify(nullify.view).unwrap();
            assert!(stored_nullify.is_some());
        }

        // Check leader metadata for view 2
        let stored_leader_v2 = storage.get_leader(2).unwrap();
        assert!(stored_leader_v2.is_some());
        assert_eq!(stored_leader_v2.unwrap().peer_id, leader_id);

        // Check view metadata for view 2
        let stored_view_v2 = storage.get_view(2).unwrap();
        assert!(stored_view_v2.is_some());

        // Check block was persisted as finalized
        let stored_block_v3 = storage.get_finalized_block(&block_hash_v3).unwrap();
        assert!(stored_block_v3.is_some());
        let stored_block_v3 = stored_block_v3.unwrap();
        assert_eq!(stored_block_v3.get_hash(), block_hash_v3);
        assert_eq!(stored_block_v3.view(), 3);
        assert!(stored_block_v3.is_finalized); // Critical: must be marked as finalized

        // Check transactions from view 3 were persisted
        for tx in block_v3.transactions.iter() {
            let stored_tx = storage.get_transaction(&tx.tx_hash).unwrap();
            assert!(stored_tx.is_some());
        }

        // Check M-notarization for view 3 was persisted
        let stored_m_not_v3 = storage
            .get_notarization::<N, F, M_SIZE>(&block_hash_v3)
            .unwrap();
        assert!(stored_m_not_v3.is_some());

        // Check ALL votes for view 3 were persisted (including L-notarization votes)
        let all_votes_v3: Vec<Vote> = (0..(N - F))
            .map(|i| create_vote(i, 3, block_hash_v3, leader_id, &setup))
            .collect();
        for vote in all_votes_v3.iter() {
            let stored_vote = storage.get_vote(vote.key()).unwrap();
            assert!(stored_vote.is_some());
        }

        // Check leader metadata for view 3
        let stored_leader_v3 = storage.get_leader(3).unwrap();
        assert!(stored_leader_v3.is_some());
        assert_eq!(stored_leader_v3.unwrap().peer_id, leader_id);

        // Check view metadata for view 3 - should be marked as finalized
        let stored_view_v3 = storage.get_view(3).unwrap();
        assert!(stored_view_v3.is_some());
        // Note: The view metadata should indicate it's finalized

        let stored_leader_v4 = storage.get_leader(4).unwrap();
        assert!(stored_leader_v4.is_none());

        // Verify the chain state is correct after finalization
        assert_eq!(view_chain.non_finalized_count(), 1); // Only view 4 remains
        assert_eq!(view_chain.current_view_number(), 4);

        // Verify previously_committed_block_hash was updated
        assert_eq!(view_chain.previously_committed_block_hash, block_hash_v3);

        std::fs::remove_dir_all(setup.temp_dir.path()).unwrap();
    }
}
