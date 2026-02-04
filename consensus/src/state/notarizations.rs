use std::hash::{Hash, Hasher};

use rkyv::{Archive, Deserialize, Serialize};

use crate::crypto::aggregated::{BlsPublicKey, BlsSignature, PeerId};
use crate::crypto::conversions::ArkSerdeWrapper;
use crate::state::peer::PeerSet;

/// [`Vote`] represents a vote for a given block.
///
/// A vote corresponds to an authenticated block, from a given peer.
#[derive(Archive, Deserialize, Serialize, Clone, Debug)]
pub struct Vote {
    /// The view number for which the vote is being cast
    pub view: u64,
    /// The hash of the block that is being voted for
    pub block_hash: [u8; blake3::OUT_LEN],
    /// The signature of block by the peer that is voting
    /// for the current block
    #[rkyv(with = ArkSerdeWrapper)]
    pub signature: BlsSignature,
    /// The public key of the peer that is
    /// voting for the current block
    pub peer_id: PeerId,
    /// The leader's ID of the view
    pub leader_id: PeerId,
}

impl Vote {
    pub fn new(
        view: u64,
        block_hash: [u8; blake3::OUT_LEN],
        signature: BlsSignature,
        peer_id: PeerId,
        leader_id: PeerId,
    ) -> Self {
        Self {
            view,
            block_hash,
            signature,
            peer_id,
            leader_id,
        }
    }

    /// Verifies if the block has been successfully signed by its author
    /// Note: this does not verify that the [`PeerId`] matches the public key
    /// of the peer that signed the block. This should be verified by the caller, beforehand.
    pub fn verify(&self, peer_public_key: &BlsPublicKey) -> bool {
        peer_public_key.verify(&self.block_hash, &self.signature)
    }
}

impl Hash for Vote {
    fn hash<H: Hasher>(&self, state: &mut H) {
        self.view.hash(state);
        self.block_hash.hash(state);
        self.peer_id.hash(state);
    }
}

impl PartialEq for Vote {
    fn eq(&self, other: &Self) -> bool {
        self.view == other.view
            && self.block_hash == other.block_hash
            && self.peer_id == other.peer_id
    }
}

impl Eq for Vote {}

/// [`MNotarization`] represents a (2f + 1)-signature quorum for a given block (also
/// referred to as a M-notarization). An M-notarization is required to progress to the next view,
/// but not to finalize a block. Moreover, a block for view `v` can receive a M-notarization,
/// but not being finalized for that view (which happens if the consensus cannot finalize it
/// within the given view timeout period).
///
/// The type parameters `N` and `F` correspond to the total number of peers and the number of
/// faulty peers, respectively. The type parameter `M_SIZE` corresponds to the size of the
/// aggregated signature.
#[derive(Archive, Deserialize, Serialize, Clone, Debug)]
pub struct MNotarization<const N: usize, const F: usize, const M_SIZE: usize> {
    /// The view number for which the M-notarization is being cast
    pub view: u64,
    /// The hash of the block that has been notarized
    pub block_hash: [u8; blake3::OUT_LEN],
    /// The aggregated signature of the block by the peers that have notarized it
    #[rkyv(with = ArkSerdeWrapper)]
    pub aggregated_signature: BlsSignature,
    /// The peer IDs of the peers that have notarized the block
    pub peer_ids: [PeerId; M_SIZE],
    /// The leader's ID of the view
    pub leader_id: PeerId,
}

impl<const N: usize, const F: usize, const M_SIZE: usize> MNotarization<N, F, M_SIZE> {
    pub fn new(
        view: u64,
        block_hash: [u8; blake3::OUT_LEN],
        aggregated_signature: BlsSignature,
        peer_ids: [PeerId; M_SIZE],
        leader_id: PeerId,
    ) -> Self {
        Self {
            view,
            block_hash,
            aggregated_signature,
            peer_ids,
            leader_id,
        }
    }

    /// Verifies the underlying M-notarization aggregated block signature
    pub fn verify(&self, peer_set: &PeerSet) -> bool {
        let public_keys = self
            .peer_ids
            .iter()
            .map(|peer_id| {
                peer_set
                    .get_public_key(peer_id)
                    .expect("Peer ID not found in peer set")
                    .clone()
            })
            .collect::<Vec<BlsPublicKey>>();
        BlsPublicKey::aggregate(&public_keys).verify(&self.block_hash, &self.aggregated_signature)
    }
}

impl<const N: usize, const F: usize, const M_SIZE: usize> Hash for MNotarization<N, F, M_SIZE> {
    fn hash<H: Hasher>(&self, state: &mut H) {
        self.view.hash(state);
        self.block_hash.hash(state);
        self.peer_ids.hash(state);
    }
}

impl<const N: usize, const F: usize, const M_SIZE: usize> PartialEq
    for MNotarization<N, F, M_SIZE>
{
    fn eq(&self, other: &Self) -> bool {
        self.view == other.view
            && self.block_hash == other.block_hash
            && self.peer_ids == other.peer_ids
    }
}

impl<const N: usize, const F: usize, const M_SIZE: usize> Eq for MNotarization<N, F, M_SIZE> {}

/// [`LNotarization`] represents a (n - f)-signature quorum for a given block (also
/// referred to as an L-notarization). An L-notarization is the finality proof for a block,
/// meaning the block is committed and can never be reverted.
///
/// Light clients can use the L-notarization to verify that a block was correctly finalized
/// by aggregating the public keys of the signing validators and verifying the BLS signature.
///
/// The type parameters `N` and `F` correspond to the total number of peers and the number of
/// faulty peers, respectively. Uses a `Vec<PeerId>` to accommodate varying numbers of signers
/// (between N-F and N).
#[derive(Archive, Deserialize, Serialize, Clone, Debug)]
pub struct LNotarization<const N: usize, const F: usize> {
    /// The view number when the block was finalized
    pub view: u64,
    /// The hash of the finalized block
    pub block_hash: [u8; blake3::OUT_LEN],
    /// The aggregated BLS signature from n-f validators
    #[rkyv(with = ArkSerdeWrapper)]
    pub aggregated_signature: BlsSignature,
    /// The peer IDs of the validators who signed (N-F to N signers)
    pub peer_ids: Vec<PeerId>,
    /// The block height for easier lookup
    pub height: u64,
}

impl<const N: usize, const F: usize> LNotarization<N, F> {
    pub fn new(
        view: u64,
        block_hash: [u8; blake3::OUT_LEN],
        aggregated_signature: BlsSignature,
        peer_ids: Vec<PeerId>,
        height: u64,
    ) -> Self {
        Self {
            view,
            block_hash,
            aggregated_signature,
            peer_ids,
            height,
        }
    }

    /// Verifies the L-notarization aggregated BLS signature against the validator set.
    ///
    /// Returns `true` if:
    /// 1. At least N - F validators signed
    /// 2. The aggregated signature is valid for the block hash
    pub fn verify(&self, peer_set: &PeerSet) -> bool {
        // Collect public keys for all signing peers
        let public_keys: Vec<BlsPublicKey> = self
            .peer_ids
            .iter()
            .filter_map(|peer_id| peer_set.get_public_key(peer_id).ok().cloned())
            .collect();

        // Require at least n-f signatures for finality
        if public_keys.len() < N - F {
            return false;
        }

        BlsPublicKey::aggregate(&public_keys).verify(&self.block_hash, &self.aggregated_signature)
    }
}

impl<const N: usize, const F: usize> Hash for LNotarization<N, F> {
    fn hash<H: Hasher>(&self, state: &mut H) {
        self.view.hash(state);
        self.block_hash.hash(state);
        self.height.hash(state);
    }
}

impl<const N: usize, const F: usize> PartialEq for LNotarization<N, F> {
    fn eq(&self, other: &Self) -> bool {
        self.view == other.view
            && self.block_hash == other.block_hash
            && self.height == other.height
    }
}

impl<const N: usize, const F: usize> Eq for LNotarization<N, F> {}
