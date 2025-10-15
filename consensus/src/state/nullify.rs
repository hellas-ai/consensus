use rkyv::{Archive, Deserialize, Serialize};

use crate::{
    crypto::{
        aggregated::{AggregatedSignature, BlsPublicKey, BlsSignature},
        conversions::ArkSerdeWrapper,
    },
    state::View,
};

#[derive(Archive, Deserialize, Serialize, Clone, Debug)]
pub struct Nullify {
    pub view: View,
    #[rkyv(with = ArkSerdeWrapper)]
    pub signature: BlsSignature,
    #[rkyv(with = ArkSerdeWrapper)]
    pub public_key: BlsPublicKey,
}

impl Nullify {
    pub fn new(view: View, signature: BlsSignature, public_key: BlsPublicKey) -> Self {
        Self {
            view,
            signature,
            public_key,
        }
    }

    pub fn verify(&self) -> bool {
        self.public_key
            .verify(&self.view.to_le_bytes(), &self.signature)
    }
}

#[derive(Archive, Deserialize, Serialize, Clone, Debug)]
pub struct Nullification<const N: usize, const F: usize, const M_SIZE: usize> {
    pub view: View,
    #[rkyv(with = ArkSerdeWrapper)]
    pub signature: AggregatedSignature<M_SIZE>,
}

impl<const N: usize, const F: usize, const M_SIZE: usize> Nullification<N, F, M_SIZE> {
    pub fn new(view: View, signature: AggregatedSignature<M_SIZE>) -> Self {
        Self { view, signature }
    }

    pub fn verify(&self) -> bool {
        self.signature.verify(&self.view.to_le_bytes())
    }
}
