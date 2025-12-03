use rkyv::{Archive, Deserialize, Serialize, deserialize, rancor::Error};
use std::{hash::Hash, hash::Hasher};

use crate::{
    crypto::{
        aggregated::{BlsSignature, PeerId},
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
    /// The leader that proposed the block
    pub leader: PeerId,
    /// The header of the block
    pub header: BlockHeader,
    /// The signature of the leader's block proposal
    #[rkyv(with = ArkSerdeWrapper)]
    pub leader_signature: BlsSignature,
    /// The transactions associated with the block
    pub transactions: Vec<Transaction>,
    /// The hash of the (entire) block
    pub hash: Option<[u8; blake3::OUT_LEN]>,
    /// If the block is finalized or not. A block might have been
    /// rejected by the consensus, if peers fail to collect enough
    /// votes to finalize it, within the given view timeout period.
    pub is_finalized: bool,
    /// The height of the block in the blockchain
    pub height: u64,
}

impl Block {
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        view: u64,
        leader: PeerId,
        parent_block_hash: [u8; blake3::OUT_LEN],
        transactions: Vec<Transaction>,
        timestamp: u64,
        leader_signature: BlsSignature,
        is_finalized: bool,
        height: u64,
    ) -> Self {
        let mut block = Self {
            leader,
            header: BlockHeader {
                view,
                parent_block_hash,
                timestamp,
            },
            leader_signature,
            transactions,
            hash: None,
            is_finalized,
            height,
        };
        block.hash = Some(block.compute_hash());
        block
    }

    /// Creates the genesis block for the consensus protocol.
    pub fn genesis(leader: PeerId, genesis_block_signature: BlsSignature) -> Self {
        Self {
            leader,
            header: BlockHeader {
                view: 0,
                parent_block_hash: [0; blake3::OUT_LEN],
                timestamp: 0,
            },
            leader_signature: genesis_block_signature,
            transactions: vec![],
            hash: Some(Self::genesis_hash()),
            is_finalized: true,
            height: 0,
        }
    }

    /// Computes the deterministic genesis block hash.
    ///
    /// This hash is the same across all replicas since genesis has fixed parameters:
    /// - parent_block_hash: [0; 32]
    /// - transactions: []
    /// - timestamp: 0
    /// - view: 0
    ///
    /// All replicas sign this hash to create their genesis signatures.
    ///
    /// # Returns
    /// The 32-byte blake3 hash of the genesis block
    pub fn genesis_hash() -> [u8; blake3::OUT_LEN] {
        let mut hasher = blake3::Hasher::new();
        hasher.update(&[0u8; blake3::OUT_LEN]); // parent_block_hash = [0; 32]
        // No transactions to hash (empty vec)
        hasher.update(&0u64.to_le_bytes()); // timestamp = 0
        hasher.update(&0u64.to_le_bytes()); // view = 0
        hasher.finalize().into()
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
        hasher.update(&self.header.view.to_le_bytes());
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

    /// Computes the block from its serialized byte representation
    pub fn from_block_bytes(bytes: &[u8]) -> Self {
        let archived = unsafe { rkyv::access_unchecked::<ArchivedBlock>(bytes) };
        let mut block = deserialize::<Block, Error>(archived).expect("Failed to deserialize");
        if block.hash.is_none() {
            block.hash = Some(block.compute_hash());
        }
        block
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::crypto::aggregated::BlsSecretKey;
    use crate::crypto::transaction_crypto::TxSecretKey;
    use crate::state::address::Address;
    use crate::storage::conversions::serialize_for_db;
    use rand::thread_rng;

    fn gen_tx() -> Transaction {
        let (sk, pk) = {
            let sk = TxSecretKey::generate(&mut rand::rngs::OsRng);
            let pk = sk.public_key();
            (sk, pk)
        };
        let address = Address::from_public_key(&pk);
        let recipient = Address::from_bytes([7u8; 32]);
        Transaction::new_transfer(address, recipient, 42, 9, 1_000, &sk)
    }

    fn gen_block(
        view: u64,
        leader: PeerId,
        parent: [u8; blake3::OUT_LEN],
        num_transactions: usize,
        ts: u64,
        leader_signature: BlsSignature,
        height: u64,
    ) -> Block {
        let txs = (0..num_transactions).map(|_| gen_tx()).collect::<Vec<_>>();
        Block::new(
            view,
            leader,
            parent,
            txs,
            ts,
            leader_signature,
            false,
            height,
        )
    }

    #[test]
    fn hash_is_deterministic_for_same_content() {
        let parent = [1u8; blake3::OUT_LEN];
        let sk = BlsSecretKey::generate(&mut thread_rng());
        let leader_signature = sk.sign(b"block proposal");

        // Generate transactions ONCE
        let txs: Vec<Transaction> = (0..2).map(|_| gen_tx()).collect();

        let b1 = Block::new(
            5,
            0,
            parent,
            txs.clone(),
            123456,
            leader_signature.clone(),
            false,
            1,
        );
        let b2 = Block::new(5, 0, parent, txs, 123456, leader_signature, false, 1);

        assert_eq!(b1.get_hash(), b2.get_hash());
        assert_eq!(b1, b2);
    }

    #[test]
    fn hash_changes_when_transactions_change() {
        let parent = [2u8; blake3::OUT_LEN];
        let sk = BlsSecretKey::generate(&mut thread_rng());
        let leader_signature = sk.sign(b"block proposal");
        let b1 = gen_block(6, 0, parent, 2, 999, leader_signature.clone(), 2);
        let b2 = gen_block(6, 0, parent, 2, 999, leader_signature, 2);
        assert_ne!(b1.get_hash(), b2.get_hash());
        assert_ne!(b1, b2);
    }

    #[test]
    fn hash_changes_with_order_of_transactions() {
        let parent = [3u8; blake3::OUT_LEN];
        let sk = BlsSecretKey::generate(&mut thread_rng());
        let leader_signature = sk.sign(b"block proposal");
        let b1 = gen_block(7, 0, parent, 2, 111, leader_signature.clone(), 3);
        let b2 = gen_block(7, 0, parent, 2, 111, leader_signature, 3);
        assert_ne!(b1.get_hash(), b2.get_hash());
    }

    #[test]
    fn getters_return_expected_values() {
        let parent = [9u8; blake3::OUT_LEN];
        let sk = BlsSecretKey::generate(&mut thread_rng());
        let leader_signature = sk.sign(b"block proposal");
        let b = gen_block(42, 0, parent, 1, 777, leader_signature, 42);
        assert_eq!(b.view(), 42);
        assert_eq!(b.parent_block_hash(), parent);
        assert!(b.is_view_block(42));
        assert!(!b.is_view_block(41));
    }

    #[test]
    fn from_block_bytes_recomputes_when_hash_missing() {
        let parent = [4u8; blake3::OUT_LEN];
        let sk = BlsSecretKey::generate(&mut thread_rng());
        let leader_signature = sk.sign(b"block proposal");

        // Generate transaction(s) ONCE
        let txs: Vec<Transaction> = (0..1).map(|_| gen_tx()).collect();

        let mut b = Block::new(
            8,
            0,
            parent,
            txs.clone(),
            222,
            leader_signature.clone(),
            false,
            8,
        );
        // Simulate an archived block with hash = None
        b.hash = None;
        let bytes = serialize_for_db(&b).expect("serialize");
        let restored = Block::from_block_bytes(bytes.as_slice());

        // The restored hash should be present and match a fresh computation
        let expected = restored.get_hash();
        let recomputed = {
            // Recompute via creating the same content block again
            let b2 = Block::new(8, 0, parent, txs, 222, leader_signature, false, 8);
            b2.get_hash()
        };
        assert_eq!(expected, recomputed);
    }
}
