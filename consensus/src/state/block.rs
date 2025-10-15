use rkyv::{Archive, Deserialize, Serialize};
use std::{hash::Hash, hash::Hasher};

use crate::{
    crypto::{
        aggregated::{AggregatedSignature, BlsPublicKey, BlsSignature},
        conversions::ArkSerdeWrapper,
    },
    state::View,
};

#[derive(Archive, Deserialize, Serialize, Clone, Debug)]
pub struct BlockHeader {
    pub view: View,
    pub parent_block_hash: [u8; blake3::OUT_LEN],
    pub timestamp: u64,
}

#[derive(Archive, Deserialize, Serialize, Clone, Debug)]
pub struct Block {
    pub header: BlockHeader,
    pub transactions: Vec<Transaction>,
    pub hash: Option<[u8; blake3::OUT_LEN]>,
}

impl Block {
    pub fn new(
        view: u64,
        parent_block_hash: [u8; blake3::OUT_LEN],
        transactions: Vec<Transaction>,
        timestamp: u64,
    ) -> Self {
        let mut block = Self {
            header: BlockHeader {
                view,
                parent_block_hash,
                timestamp,
            },
            transactions,
            hash: None,
        };
        block.hash = Some(block.compute_hash());
        block
    }

    fn compute_hash(&self) -> [u8; blake3::OUT_LEN] {
        let mut hasher = blake3::Hasher::new();
        hasher.update(&self.header.parent_block_hash);
        hasher.update(
            &self
                .transactions
                .iter()
                .enumerate()
                .map(|(i, t)| {
                    let mut hasher = blake3::Hasher::new();
                    hasher.update(&i.to_le_bytes());
                    hasher.update(&t.compute_hash());
                    hasher.finalize().into()
                })
                .collect::<Vec<[u8; blake3::OUT_LEN]>>()
                .concat(),
        );
        hasher.update(&self.header.timestamp.to_le_bytes());
        hasher.finalize().into()
    }

    pub fn get_hash(&self) -> [u8; blake3::OUT_LEN] {
        self.hash.unwrap_or_else(|| self.compute_hash())
    }

    pub fn view(&self) -> u64 {
        self.header.view
    }

    pub fn parent_block_hash(&self) -> [u8; blake3::OUT_LEN] {
        self.header.parent_block_hash
    }

    pub fn is_view_block(&self, v: u64) -> bool {
        self.header.view == v
    }
}

impl PartialEq for Block {
    fn eq(&self, other: &Self) -> bool {
        self.get_hash() == other.get_hash()
    }
}

impl Eq for Block {}

impl Hash for Block {
    fn hash<H: Hasher>(&self, state: &mut H) {
        self.get_hash().hash(state);
    }
}

#[derive(Archive, Deserialize, Serialize, Clone, Debug)]
pub struct Vote {
    pub block: Block,
    #[rkyv(with = ArkSerdeWrapper)]
    pub signature: BlsSignature,
    #[rkyv(with = ArkSerdeWrapper)]
    pub public_key: BlsPublicKey,
}

impl Vote {
    pub fn new(block: Block, signature: BlsSignature, public_key: BlsPublicKey) -> Self {
        Self {
            block,
            signature,
            public_key,
        }
    }

    pub fn verify(&self) -> bool {
        self.public_key
            .verify(&self.block.get_hash(), &self.signature)
    }
}

#[derive(Archive, Deserialize, Serialize, Clone, Debug)]
pub struct MNotarization<const N: usize, const F: usize, const M_SIZE: usize> {
    pub block: Block,
    #[rkyv(with = ArkSerdeWrapper)]
    pub aggregated_signature: AggregatedSignature<M_SIZE>,
}

#[derive(Archive, Deserialize, Serialize, Clone, Debug)]
pub struct LNotarization<const N: usize, const F: usize, const L_SIZE: usize> {
    pub block: Block,
    pub aggregated_signature: AggregatedSignature<L_SIZE>,
}

#[derive(Archive, Deserialize, Serialize, Clone, Debug)]
pub struct Transaction(Vec<u8>);

impl Transaction {
    pub fn new(data: Vec<u8>) -> Self {
        Self(data)
    }

    pub fn compute_hash(&self) -> [u8; blake3::OUT_LEN] {
        let mut hasher = blake3::Hasher::new();
        hasher.update(&self.0);
        hasher.finalize().into()
    }
}

impl Hash for Transaction {
    fn hash<H: Hasher>(&self, state: &mut H) {
        self.compute_hash().hash(state);
    }
}

impl PartialEq for Transaction {
    fn eq(&self, other: &Self) -> bool {
        self.compute_hash() == other.compute_hash()
    }
}

impl Eq for Transaction {}
