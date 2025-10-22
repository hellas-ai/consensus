use std::collections::HashSet;

use anyhow::Result;

use crate::{
    crypto::aggregated::{BlsSignature, PeerId},
    state::notarizations::Vote,
};

/// Data structure containing the aggregated signature and peer IDs for a notarization.
///
/// This struct is used to bundle the results of signature aggregation and peer ID
/// collection for both M-notarizations and L-notarizations.
pub(crate) struct NotarizationData<const N: usize> {
    /// Array of peer IDs participating in the notarization.
    /// Note: This may contain duplicate or zero values if there aren't enough distinct peers.
    pub(crate) peer_ids: [PeerId; N],
    /// The aggregated BLS signature from the participating peers.
    pub(crate) aggregated_signature: BlsSignature,
}

/// Creates notarization data by aggregating signatures and collecting peer IDs from votes.
///
/// This function takes the first N votes from the provided HashSet and:
/// 1. Aggregates their BLS signatures using `BlsSignature::aggregate`
/// 2. Collects their peer IDs into a fixed-size array
///
/// # Important Notes
///
/// - If the same peer has multiple votes, their peer ID may appear multiple times in the array or overwrite previous entries.
///
/// # Parameters
///
/// * `votes` - HashSet of votes to aggregate
///
/// # Returns
///
/// Returns `NotarizationData<N>` containing the aggregated signature and peer IDs.
///
/// # Errors
///
/// Returns an error if there are not enough votes to create notarization data.
///
/// # Examples
///
/// ```ignore
/// let votes = HashSet::from([vote1, vote2, vote3]);
/// let data: NotarizationData<3> = create_notarization_data(&votes)?;
/// ```
pub(crate) fn create_notarization_data<const N: usize>(
    votes: &HashSet<Vote>,
) -> Result<NotarizationData<N>> {
    if votes.len() < N {
        return Err(anyhow::anyhow!(
            "Not enough votes to create notarization data: {} < {}",
            votes.len(),
            N
        ));
    }

    let mut peer_ids: [PeerId; N] = [0; N];
    let signatures_iter = votes
        .iter()
        .enumerate()
        .map(|(i, v)| {
            peer_ids[i] = v.peer_id;
            &v.signature
        })
        .take(N);
    let aggregated_signature = BlsSignature::aggregate(signatures_iter);
    Ok(NotarizationData {
        peer_ids,
        aggregated_signature,
    })
}
