use rkyv::{Archive, Deserialize, Serialize};

use crate::crypto::{
    aggregated::{AggregatedSignature, BlsPublicKey, BlsSignature, PeerId},
    conversions::ArkSerdeWrapper,
};

#[derive(Archive, Deserialize, Serialize, Clone, Debug)]
pub struct Nullify {
    pub view: u64,
    #[rkyv(with = ArkSerdeWrapper)]
    pub signature: BlsSignature,
    pub peer_id: PeerId,
}

impl Nullify {
    pub fn new(view: u64, signature: BlsSignature, peer_id: PeerId) -> Self {
        Self {
            view,
            signature,
            peer_id,
        }
    }

    /// Verifies if the nullify message has been successfully signed by the peer with the given public key.
    /// Note: this does not verify that the [`PeerId`] matches the public key
    /// of the peer that signed the nullify message. This should be verified by the caller, beforehand.
    pub fn verify(&self, public_key: &BlsPublicKey) -> bool {
        let hash = blake3::hash(
            [self.view.to_le_bytes(), self.peer_id.to_le_bytes()]
                .concat()
                .as_slice(),
        );
        public_key.verify(hash.as_bytes().as_slice(), &self.signature)
    }
}

#[derive(Archive, Deserialize, Serialize, Clone, Debug)]
pub struct Nullification<const N: usize, const F: usize, const M_SIZE: usize> {
    pub view: u64,
    #[rkyv(with = ArkSerdeWrapper)]
    pub signature: AggregatedSignature<M_SIZE>,
}

impl<const N: usize, const F: usize, const M_SIZE: usize> Nullification<N, F, M_SIZE> {
    pub fn new(view: u64, signature: AggregatedSignature<M_SIZE>) -> Self {
        Self { view, signature }
    }

    pub fn verify(&self) -> bool {
        self.signature.verify(&self.view.to_le_bytes())
    }
}
