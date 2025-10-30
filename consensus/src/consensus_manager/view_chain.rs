//! The [`ViewChain`] is a data structure that represents the chain of views in the consensus protocol,
//! that have not yet been finalized by a supra-majority vote (n-f) or a nullification.
//!
//! The [`ViewChain`] is a modular component that is responsible solely for the following tasks:
//!
//! - Routing messages to the appropriate view context and communicating the decision event produced by the [`ViewContext`] to the higher-level components,
//!   such as the [`ViewProgressManager`].
//! - Finalize and persist the oldest non-finalized views. That said, the [`ViewChain`] does not perform
//!   the decision logic for finalization, this is left to the higher-level components, such as the [`ViewProgressManager`].
//! - Ensure the SM Sync Invariance and the Non-finalization M-Notarization Progression Invariance are respected (see below).
//!
//! The current logic relies fundamentally on the following State Machine Replication invariant:
//!   
//! INVARIANT (SM Sync Invariance):
//!
//! If view `v` is finalized by either a nullification or a l-notarization, then all smaller views `w < v` must also
//! be finalized. Otherwise, these means that AT LEAST (f + 1) replicas have NOT nullified or voted for view `w < v`, but they
//! have done so for view `v`, this means that at least one honest replica has not followed the protocol, and is therefore
//! faulty. This is in direct contraction with the fact that at least `n - f` replicas out of `n >= 5 f + 1` are honest.
//!
//! A direct consequence of this invariant reasoning is that the [`ViewChain`] must always have consecutive non-finalized views,
//! i.e. the non-finalized views must form a contiguous range of view numbers.
//!
//! INVARIANT (Non-finalization M-Notarization Progression Invariance):
//!
//! In order for a view to progress but not be considered finalized, it must have only received a m-notarization,
//! as both nullifications and l-notarizations are considered finalizations.
//!
//! Moreover, it is important to notice that a view `v` cannot receive both a l-notarization and a nullification (see the
//! original paper for the proof).
//!
//! Therefore, the [`ViewChain`] is composed of a chain of consecutive non-finalized views
//!
//! v1 -> ... -> vk -> v(k+1) -> ... -> vn
//!
//! Where `vn` is the current (non-finalized) view (which is always present), and `v1` is the oldest non-finalized view.
//! Moreover, it follows that all views `v1, ..., vn-1` must have received a m-notarization (as there was a view progression event).
//!
//! `vn` is the only view that has not yet received a m-notarization (neither a nullification nor a l-notarization).

use std::{collections::HashMap, time::Duration};

use anyhow::Result;

use crate::{
    consensus_manager::view_context::{
        CollectedNullificationsResult, CollectedVotesResult, ShouldMNotarize, ViewContext,
    },
    state::{
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
    /// These views have achieved M-notarization and the protocol has progressed
    /// past them, but they haven't achieved L-notarization yet. We continue
    /// collecting votes for potential finalization.
    ///
    /// This map contains at least one entry, namely that corresponding to the current view number.
    non_finalized_views: HashMap<u64, ViewContext<N, F, M_SIZE>>,

    /// The persistence storage for the consensus protocol
    /// This is used to persist the view contexts and the votes/nullifications/notarizations
    /// whenever a view in the [`ViewChain`] is finalized by the state machine replication protocol.
    persistence_storage: ConsensusStore,

    /// The timeout period for a view to be considered nullified by the current replica
    _view_timeout: Duration,
}

impl<const N: usize, const F: usize, const M_SIZE: usize> ViewChain<N, F, M_SIZE> {
    /// Creates a new [`ViewChain`] from the given context
    ///
    /// # Arguments
    /// * `initial_view` - The
    pub fn new(
        initial_view: ViewContext<N, F, M_SIZE>,
        persistence_storage: ConsensusStore,
        view_timeout: Duration,
    ) -> Self {
        Self {
            current_view: initial_view.view_number,
            non_finalized_views: HashMap::from([(initial_view.view_number, initial_view)]),
            persistence_storage,
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

    /// Returns the current view number
    pub fn current_view_number(&self) -> u64 {
        self.current_view
    }

    /// Returns the number of unfinalized views
    pub fn unfinalized_count(&self) -> usize {
        self.non_finalized_views.len()
    }

    /// Returns the range of view numbers for the unfinalized views
    pub fn unfinalized_view_numbers_range(&self) -> std::ops::RangeInclusive<u64> {
        let current_view = self.current_view;
        let least_non_finalized_view = self
            .current_view
            .saturating_sub(self.unfinalized_count() as u64)
            + 1;
        least_non_finalized_view..=current_view
    }

    /// Routes a vote to the appropriate view context
    pub fn route_vote(&mut self, vote: Vote, peers: &PeerSet) -> Result<CollectedVotesResult> {
        if let Some(ctx) = self.non_finalized_views.get_mut(&vote.view) {
            let view_number = ctx.view_number;

            // NOTE: If the view number is not the current view, we check if the view has progressed without a m-notarization,
            // this is to ensure that the view chain is not left in an invalid state.
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

    /// Routes a nullify message to the appropriate view context
    pub fn route_nullify(&mut self, nullify: Nullify, peers: &PeerSet) -> Result<bool> {
        if let Some(ctx) = self.non_finalized_views.get_mut(&nullify.view) {
            let view_number = ctx.view_number;

            // NOTE: If the view number is not the current view, we check if the view has progressed without a m-notarization,
            // this is to ensure that the view chain is not left in an invalid state.
            if view_number != self.current_view {
                ctx.has_view_progressed_without_m_notarization()?;
            }

            ctx.add_nullify(nullify, peers)?;

            if ctx.nullification.is_some() {
                // NOTE: If the nullification is present, we check the finalization invariant is respected.
                self.check_finalization_invariant(view_number);
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

    /// Routes an M-notarization to the appropriate view context
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

    /// Routes a nullification to the appropriate view context
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

    /// Progressed to the next view with M-notarization. This operation boils down to insert a new view context for the next view.
    ///
    /// The current view context is either left intact has it received a m-notarization (not a finalizing event such as
    /// a nullification or a l-notarization).
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

        // 3. Update the current view to the next view.
        // NOTE: We don't persist yet the current view, as it has not been finalized yet.
        // Moreover, we keep the current view context in the `non_finalized_views` map, as it has not been finalized yet.
        self.current_view = new_view_ctx.view_number;
        self.non_finalized_views
            .insert(new_view_ctx.view_number, new_view_ctx);

        Ok(())
    }

    /// Progresses to the next view with nullification. Since nullifications are finalizing events,
    /// it follows by invariance that the only remaining non-finalized view is the `current_view`.
    ///
    /// Therefore, this method must remove the `current_view` from the `non_finalized_views` map and insert a new view context for the next view.
    ///
    /// Persists the nullified view and continues into persistent storage, removing the `current_view` from the `non_finalized_views` map.
    pub fn progress_with_nullification(
        &mut self,
        next_view_ctx: ViewContext<N, F, M_SIZE>,
        peers: &PeerSet,
    ) -> Result<()> {
        // 1. First check that the finalization invariant is respected.
        self.check_finalization_invariant(self.current_view);

        // 2. Check that the next view context is the next view.
        if next_view_ctx.view_number != self.current_view + 1 {
            return Err(anyhow::anyhow!(
                "View number {} is not the next view number {}",
                next_view_ctx.view_number,
                self.current_view + 1
            ));
        }

        // 3. Check that the current view has indeed received a nullification.
        if self.current().nullification.is_none() {
            return Err(anyhow::anyhow!(
                "The current view {} has not received a nullification, but the view has progressed with a nullification",
                self.current_view
            ));
        }

        let current_view_ctx = self.current();

        // 4. Persist the nullified view to the persistence storage.
        self.persist_nullified_view(current_view_ctx, peers)?;

        // 5. Remove the current view from the `non_finalized_views` map.
        self.non_finalized_views.remove(&self.current_view);

        // 6. Update the current view to the next view.
        self.current_view = next_view_ctx.view_number;
        self.non_finalized_views
            .insert(next_view_ctx.view_number, next_view_ctx);

        Ok(())
    }

    /// Finalizes a view with a nullification. Since nullifications are finalizing events,
    /// we remove the view from the `non_finalized_views` map and persist the nullified view to the persistence storage.
    ///
    /// This method should ONLY be called for a `view_number` that is NOT the current view,
    /// otherwise the caller should instead call the `progress_with_nullification` method.
    pub fn finalize_with_nullification(&mut self, view_number: u64, peers: &PeerSet) -> Result<()> {
        // 1. Check that the view number is not the current view.
        if view_number == self.current_view {
            return Err(anyhow::anyhow!(
                "View number {} is the current view, use the `progress_with_nullification` method instead",
                view_number
            ));
        }

        // 2. First check that the finalization invariant is respected.
        self.check_finalization_invariant(view_number);

        // 3. Persist the nullified view to the persistence storage, and remove the view from the `non_finalized_views` map.
        if let Some(ctx) = self.non_finalized_views.get(&view_number) {
            // 4. Check that the view has indeed received a nullification.
            if ctx.nullification.is_none() {
                return Err(anyhow::anyhow!(
                    "View number {} has not received a nullification, but the view has been finalized with a nullification",
                    view_number
                ));
            }

            self.persist_nullified_view(ctx, peers)?;
            self.non_finalized_views.remove(&view_number);

            return Ok(());
        }

        Err(anyhow::anyhow!(
            "View number {} is not an unfinalized view",
            view_number
        ))
    }

    /// Progresses to the next view with a l-notarization. Since l-notarizations are finalizing events,
    /// it follows by invariance that the only remaining non-finalized view is the `current_view`.
    ///
    /// Therefore, this method must remove the `current_view` from the `non_finalized_views` map and insert a new view context for the next view.
    ///
    /// Persists the finalized view and continues into persistent storage, removing the `current_view` from the `non_finalized_views` map.
    pub fn progress_with_l_notarization(
        &mut self,
        next_view_ctx: ViewContext<N, F, M_SIZE>,
        peers: &PeerSet,
    ) -> Result<()> {
        // 1. First check that the finalization invariant is respected.
        self.check_finalization_invariant(self.current_view);

        // 2. Check that the next view context is the next view.
        if next_view_ctx.view_number != self.current_view + 1 {
            return Err(anyhow::anyhow!(
                "View number {} is not the next view number {}",
                next_view_ctx.view_number,
                self.current_view + 1
            ));
        }

        let current_view_ctx = self.current();

        // 3. Check that the current view has indeed received a l-notarization.
        if current_view_ctx.votes.len() < N - F {
            return Err(anyhow::anyhow!(
                "The current view {} has not received a l-notarization, but the view has progressed with a l-notarization",
                self.current_view
            ));
        }

        // 4. Persist the finalized view to the persistence storage, and remove the view from the `non_finalized_views` map.
        self.persist_l_notarized_view(current_view_ctx, peers)?;
        self.non_finalized_views.remove(&self.current_view);

        // 5. Update the current view to the next view.
        self.current_view = next_view_ctx.view_number;
        self.non_finalized_views
            .insert(next_view_ctx.view_number, next_view_ctx);

        Ok(())
    }

    /// Finalizes a view with a l-notarization. Since l-notarizations are finalizing events
    /// we remove the view from the `non_finalized_views` map and persist the finalized view to the persistence storage.
    ///
    /// This method should ONLY be called for a `view_number` that is NOT the current view,
    /// otherwise the caller should instead call the `progress_with_l_notarization` method.
    pub fn finalize_with_l_notarization(
        &mut self,
        finalized_view: u64,
        peers: &PeerSet,
    ) -> Result<()> {
        if self.current_view == finalized_view {
            return Err(anyhow::anyhow!(
                "View number {} is the current view, use the `progress_with_l_notarization` method instead",
                finalized_view
            ));
        }

        // 2. First check that the finalization invariant is respected.
        self.check_finalization_invariant(finalized_view);

        // 3. Persist the finalized view to the persistence storage, and remove the view from the `non_finalized_views` map.
        if let Some(ctx) = self.non_finalized_views.get(&finalized_view) {
            // 4. Check that the view has indeed received a l-notarization.
            if ctx.votes.len() < N - F {
                return Err(anyhow::anyhow!(
                    "View number {} has not received a l-notarization, but the view has been finalized with a l-notarization",
                    finalized_view
                ));
            }

            self.persist_l_notarized_view(ctx, peers)?;
            self.non_finalized_views.remove(&finalized_view);

            return Ok(());
        }

        Err(anyhow::anyhow!(
            "View number {} is not an unfinalized view",
            finalized_view
        ))
    }

    /// Persists a l-notarized view to the persistence storage.
    fn persist_l_notarized_view(
        &self,
        ctx: &ViewContext<N, F, M_SIZE>,
        peers: &PeerSet,
    ) -> Result<()> {
        let view_number = ctx.view_number;

        // 1. Persist both the block with `is_finalized` set to true, and the transactions associated with the block.
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

    /// Persists a nullified view to the persistence storage.
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

    /// Persists all the view contexts in the `non_finalized_views` map to the persistence storage.
    ///
    /// This method is mostly used when the consensus engine is gracefully shutting down.
    pub fn persist_all_views(&mut self) -> Result<()> {
        for (_view_number, ctx) in self.non_finalized_views.drain() {
            if let Some(ref block) = ctx.block {
                self.persistence_storage.put_non_finalized_block(block)?;
            }
        }

        Ok(())
    }

    #[inline]
    fn check_finalization_invariant(&self, view_number: u64) {
        assert!(
            view_number == self.current_view - self.unfinalized_count() as u64 + 1,
            "State machine synchronization invariant violation: View {view_number} does not correspond to the oldest unfinalized view {}, therefore previous views were left in an invalid state",
            self.current_view - self.unfinalized_count() as u64 + 1
        );
    }
}
