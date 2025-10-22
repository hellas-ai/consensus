use rkyv::{Archive, Deserialize, Serialize, deserialize, rancor::Error};
use std::{hash::Hash, hash::Hasher};

use crate::{crypto::aggregated::PeerId, state::transaction::Transaction};

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
    pub fn new(
        view: u64,
        leader: PeerId,
        parent_block_hash: [u8; blake3::OUT_LEN],
        transactions: Vec<Transaction>,
        timestamp: u64,
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
            transactions,
            hash: None,
            is_finalized,
            height,
        };
        block.hash = Some(block.compute_hash());
        block
    }

    /// Creates the genesis block for the consensus protocol.
    pub fn genesis() -> Self {
        Self {
            leader: 0,
            header: BlockHeader {
                view: 0,
                parent_block_hash: [0; blake3::OUT_LEN],
                timestamp: 0,
            },
            transactions: vec![],
            hash: None,
            is_finalized: false,
            height: 0,
        }
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
    use crate::storage::conversions::serialize_for_db;
    use rand::thread_rng;

    fn gen_tx(body: &[u8]) -> Transaction {
        let (sk, pk) = {
            let mut rng = thread_rng();
            let sk = BlsSecretKey::generate(&mut rng);
            let pk = sk.public_key();
            (sk, pk)
        };
        let tx_hash: [u8; blake3::OUT_LEN] = blake3::hash(body).into();
        let sig = sk.sign(&tx_hash);
        Transaction::new(pk, [7u8; 32], 42, 9, 1_000, 3, tx_hash, sig)
    }

    fn gen_block(
        view: u64,
        leader: PeerId,
        parent: [u8; blake3::OUT_LEN],
        tx_bodies: &[&[u8]],
        ts: u64,
        height: u64,
    ) -> Block {
        let txs = tx_bodies.iter().map(|b| gen_tx(b)).collect::<Vec<_>>();
        Block::new(view, leader, parent, txs, ts, false, height)
    }

    #[test]
    fn hash_is_deterministic_for_same_content() {
        let parent = [1u8; blake3::OUT_LEN];
        let b1 = gen_block(5, 0, parent, &[b"a", b"b"], 123456, 1);
        let b2 = gen_block(5, 0, parent, &[b"a", b"b"], 123456, 1);
        assert_eq!(b1.get_hash(), b2.get_hash());
        assert_eq!(b1, b2);
    }

    #[test]
    fn hash_changes_when_transactions_change() {
        let parent = [2u8; blake3::OUT_LEN];
        let b1 = gen_block(6, 0, parent, &[b"a", b"b"], 999, 2);
        let b2 = gen_block(6, 0, parent, &[b"a", b"c"], 999, 2);
        assert_ne!(b1.get_hash(), b2.get_hash());
        assert_ne!(b1, b2);
    }

    #[test]
    fn hash_changes_with_order_of_transactions() {
        let parent = [3u8; blake3::OUT_LEN];
        let b1 = gen_block(7, 0, parent, &[b"x", b"y"], 111, 3);
        let b2 = gen_block(7, 0, parent, &[b"y", b"x"], 111, 3);
        assert_ne!(b1.get_hash(), b2.get_hash());
    }

    #[test]
    fn getters_return_expected_values() {
        let parent = [9u8; blake3::OUT_LEN];
        let b = gen_block(42, 0, parent, &[b"a"], 777, 42);
        assert_eq!(b.view(), 42);
        assert_eq!(b.parent_block_hash(), parent);
        assert!(b.is_view_block(42));
        assert!(!b.is_view_block(41));
    }

    #[test]
    fn from_block_bytes_recomputes_when_hash_missing() {
        let parent = [4u8; blake3::OUT_LEN];
        let mut b = gen_block(8, 0, parent, &[b"z"], 222, 8);
        // Simulate an archived block with hash = None
        b.hash = None;
        let bytes = serialize_for_db(&b).expect("serialize");
        let restored = Block::from_block_bytes(bytes.as_slice());
        // The restored hash should be present and match a fresh computation
        let expected = restored.get_hash();
        let recomputed = {
            // Recompute via creating the same content block again
            let b2 = gen_block(8, 0, parent, &[b"z"], 222, 8);
            b2.get_hash()
        };
        assert_eq!(expected, recomputed);
    }
}
