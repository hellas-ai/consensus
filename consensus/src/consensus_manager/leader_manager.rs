use crate::{crypto::aggregated::PeerId, state::leader::Leader};

use anyhow::Result;
use serde::{Deserialize, Serialize};

/// [`LeaderManager`] trait for managing leader selection.
///
/// One essential aspect of any derived implementation is the ability to
/// select the leader for a given view, in a fully deterministic and reproducible way.
/// This is crucial for the safety of the consensus protocol, and to make sure
/// that replicas are always able to agree on the leader for a view, given its past
/// state.
pub trait LeaderManager: Send {
    /// Selects the leader for a given view.
    fn leader_for_view(&self, view: u64) -> Result<Leader>;
}

/// [`RoundRobinLeaderManager`] implements the round-robin leader selection strategy.
///
/// This strategy selects the leader for a given view by using a round-robin approach.
/// The leader is selected by the index of the replica in the list of replicas.
pub(crate) struct RoundRobinLeaderManager {
    /// The number of replicas in the consensus protocol.
    n: usize,

    /// The vector of all available replicas in the consensus protocol.
    pub replicas: Vec<PeerId>,
}

impl RoundRobinLeaderManager {
    /// Creates a new round-robin leader manager.
    ///
    /// # Arguments
    /// * `n` - The total number of replicas in the consensus protocol
    /// * `replicas` - A **sorted** vector of replica peer IDs
    ///
    /// # Panics
    ///
    /// Panics if:
    /// - The `replicas` vector is not sorted in ascending order
    /// - The length of `replicas` does not match `n`
    ///
    /// # Safety Note
    ///
    /// The caller MUST ensure that all replicas in the network use the exact same
    /// sorted ordering. Using `PeerSet::sorted_peer_ids` is the recommended way
    /// to guarantee this invariant.
    pub fn new(n: usize, replicas: Vec<PeerId>) -> Self {
        assert_eq!(
            replicas.len(),
            n,
            "Replicas count mismatch: expected {}, got {}",
            n,
            replicas.len()
        );

        // Verify that the replicas are sorted (for deterministic round-robin leader selection)
        for i in 1..replicas.len() {
            assert!(
                replicas[i - 1] < replicas[i],
                "Replicas must be sorted in ascending order. \
                Found {} >= {} at indices {} and {}. \
                Use PeerSet::sorted_peer_ids to ensure proper ordering.",
                replicas[i - 1],
                replicas[i],
                i - 1,
                i
            );
        }

        Self { n, replicas }
    }
}

impl LeaderManager for RoundRobinLeaderManager {
    fn leader_for_view(&self, view: u64) -> Result<Leader> {
        let leader_index = view as usize % self.n;
        let leader_peer_id = self.replicas[leader_index];
        Ok(Leader::new(leader_peer_id, view))
    }
}

/// [`LeaderSelectionStrategy`] represents the strategy used to select the leader for a given view.
#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum LeaderSelectionStrategy {
    /// [`RoundRobin`] selects the leader for a given view by using a round-robin approach.
    /// The leader is selected by the index of the replica in the (deterministic) list of replicas.
    RoundRobin,

    /// [`Random`] selects the leader for a given view by using a random approach.
    /// The leader is selected by a random number generator.
    Random,

    /// [`ProofOfStake`] selects the leader for a given view by using a proof-of-stake approach.
    /// The leader is selected by the proof-of-stake mechanism.
    ProofOfStake,
}
