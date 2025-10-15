use rkyv::{Archive, Deserialize, Serialize};
use std::{hash::Hash, hash::Hasher};

use crate::{
    crypto::{
        aggregated::{AggregatedSignature, BlsPublicKey, BlsSignature},
        conversions::ArkSerdeWrapper,
    },
    state::transaction::Transaction,
};

/// [`BlockHeader`] represents the header of a block.
#[derive(Archive, Deserialize, Serialize, Clone, Debug)]
pub struct BlockHeader {
    /// The view number corresponding to when the block was proposed
    pub view: u64,
    /// The hash of the parent block
    pub parent_block_hash: [u8; blake3::OUT_LEN],
    /// The timestamp of the block, as measured by the
    /// peer (leader) proposing such block.
    pub timestamp: u64,
}

/// [`Block`] represents a block in the consensus protocol.
///
/// A block is a collection of transactions and a header.
/// The header contains the view number, the hash of the parent block,
/// and the timestamp of the block. The transactions are the actual
/// data of the block.
#[derive(Archive, Deserialize, Serialize, Clone, Debug)]
pub struct Block {
    /// The header of the block
    pub header: BlockHeader,
    /// The transactions associated with the block
    pub transactions: Vec<Transaction>,
    /// The hash of the (entire) block
    pub hash: Option<[u8; blake3::OUT_LEN]>,
    /// If the block is finalized or not. A block might have been
    /// rejected by the consensus, if peers fail to collect enough
    /// votes to finalize it, within the given view timeout period.
    pub is_finalized: bool,
}

impl Block {
    pub fn new(
        view: u64,
        parent_block_hash: [u8; blake3::OUT_LEN],
        transactions: Vec<Transaction>,
        timestamp: u64,
        is_finalized: bool,
    ) -> Self {
        let mut block = Self {
            header: BlockHeader {
                view,
                parent_block_hash,
                timestamp,
            },
            transactions,
            hash: None,
            is_finalized,
        };
        block.hash = Some(block.compute_hash());
        block
    }

    /// Computes the hash of the block, as a concatenation of the hash of the parent block,
    /// the hash of the transactions, and the timestamp.
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
                    hasher.update(&t.tx_hash);
                    hasher.finalize().into()
                })
                .collect::<Vec<[u8; blake3::OUT_LEN]>>()
                .concat(),
        );
        hasher.update(&self.header.timestamp.to_le_bytes());
        hasher.finalize().into()
    }

    /// Returns the hash of the block
    #[inline]
    pub fn get_hash(&self) -> [u8; blake3::OUT_LEN] {
        self.hash.unwrap_or_else(|| self.compute_hash())
    }

    /// Returns the view number of the block
    #[inline]
    pub fn view(&self) -> u64 {
        self.header.view
    }

    /// Returns the hash of the parent block
    #[inline]
    pub fn parent_block_hash(&self) -> [u8; blake3::OUT_LEN] {
        self.header.parent_block_hash
    }

    /// Returns whether the block is for a given view
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

/// [`Vote`] represents a vote for a given block.
///
/// A vote corresponds to an authenticated block, from a given peer.
#[derive(Archive, Deserialize, Serialize, Clone, Debug)]
pub struct Vote {
    /// The block that is being voted for
    pub block: Block,
    /// The signature of block by the peer that is voting
    /// for the current block
    #[rkyv(with = ArkSerdeWrapper)]
    pub signature: BlsSignature,
    /// The public key of the peer that is
    /// voting for the current block
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

    /// Verifies if the block has been successfully signed by its author
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
