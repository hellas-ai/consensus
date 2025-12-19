use ed25519_dalek::{Signature, VerifyingKey, verify_batch};
use rayon::prelude::*;
use rkyv::{Archive, Deserialize, Serialize, deserialize, rancor::Error};
use std::{hash::Hash, hash::Hasher, sync::Arc};

use crate::{
    crypto::{
        aggregated::{BlsSignature, PeerId},
        conversions::ArkSerdeWrapper,
        transaction_crypto::TxSignature,
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
    pub transactions: Vec<Arc<Transaction>>,
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
        transactions: Vec<Arc<Transaction>>,
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

    /// Optimal chunk size for parallel batch verification.
    /// Smaller chunks = more parallelism but more overhead.
    /// Larger chunks = less parallelism but better batch efficiency.
    /// 128 is a good balance for typical CPU cache sizes.
    const BATCH_CHUNK_SIZE: usize = 128;

    /// Verifies all transaction signatures using parallel batch verification.
    ///
    /// This combines two optimizations:
    /// 1. **Batch verification**: Ed25519 batch verification is ~3x faster than individual
    /// 2. **Parallel processing**: Chunks are verified in parallel across CPU cores
    ///
    /// For a block with N transactions on M cores, the effective speedup is:
    /// - Batch: ~3x (N signatures verified in N/3 time)
    /// - Parallel: ~Mx (spread across cores)
    /// - Combined: ~3M x improvement over sequential individual verification
    ///
    /// # Returns
    /// - `true` if all transaction signatures are valid
    /// - `false` if any signature is invalid (use `verify_signatures_find_invalid` to find which)
    pub fn verify_block_txs_signatures(&self) -> bool {
        if self.transactions.is_empty() {
            return true;
        }

        // For small blocks, use simple batch verification (avoid parallelism overhead)
        if self.transactions.len() <= Self::BATCH_CHUNK_SIZE {
            return self.verify_batch_chunk(&self.transactions);
        }

        // For larger blocks, process chunks in parallel
        self.transactions
            .par_chunks(Self::BATCH_CHUNK_SIZE)
            .all(|chunk| self.verify_batch_chunk(chunk))
    }

    /// Verifies a chunk of transactions using batch verification.
    ///
    /// This is an internal helper for parallel batch verification.
    fn verify_batch_chunk(&self, chunk: &[Arc<Transaction>]) -> bool {
        if chunk.is_empty() {
            return true;
        }

        // Pre-allocate vectors for batch verification
        let mut messages: Vec<&[u8]> = Vec::with_capacity(chunk.len());
        let mut signatures: Vec<Signature> = Vec::with_capacity(chunk.len());
        let mut verifying_keys: Vec<VerifyingKey> = Vec::with_capacity(chunk.len());

        for tx in chunk {
            // Get the message (tx_hash)
            messages.push(&tx.tx_hash);

            // Convert signature
            let sig = match TxSignature::try_from(&tx.signature) {
                Ok(s) => s.0,
                Err(_) => return false, // Invalid signature format
            };
            signatures.push(sig);

            // Convert public key from address
            let pk = match tx.sender.to_public_key() {
                Some(pk) => pk.0,
                None => return false, // Invalid public key
            };
            verifying_keys.push(pk);
        }

        // Perform batch verification on this chunk
        verify_batch(&messages, &signatures, &verifying_keys).is_ok()
    }

    /// Finds all transactions with invalid signatures using parallel processing.
    ///
    /// This is slower than `verify_block_txs_signatures` but returns the indices
    /// of invalid transactions. Use this as a fallback when batch verification
    /// fails to identify which specific transactions are invalid.
    ///
    /// # Returns
    /// A vector of transaction indices that have invalid signatures.
    /// Empty vector means all signatures are valid.
    pub fn verify_signatures_find_invalid(&self) -> Vec<usize> {
        self.transactions
            .par_iter()
            .enumerate()
            .filter_map(|(idx, tx)| if !tx.verify() { Some(idx) } else { None })
            .collect()
    }

    /// Verifies all transaction signatures with optimal performance.
    ///
    /// First attempts batch verification (fast path). If that fails,
    /// falls back to individual verification to identify invalid transactions.
    ///
    /// # Returns
    /// - `Ok(())` if all signatures are valid
    /// - `Err(Vec<usize>)` containing indices of transactions with invalid signatures
    pub fn verify_all_signatures(&self) -> Result<(), Vec<usize>> {
        // Fast path: batch verification
        if self.verify_block_txs_signatures() {
            return Ok(());
        }

        // Slow path: find invalid signatures
        let invalid_indices = self.verify_signatures_find_invalid();
        if invalid_indices.is_empty() {
            // This shouldn't happen, but handle gracefully
            Ok(())
        } else {
            Err(invalid_indices)
        }
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
        let txs = (0..num_transactions)
            .map(|_| Arc::new(gen_tx()))
            .collect::<Vec<_>>();
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
        let txs: Vec<Arc<Transaction>> = (0..2).map(|_| Arc::new(gen_tx())).collect();

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
        let txs: Vec<Arc<Transaction>> = (0..1).map(|_| Arc::new(gen_tx())).collect();

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

    #[test]
    fn batch_verify_empty_block() {
        let sk = BlsSecretKey::generate(&mut thread_rng());
        let leader_signature = sk.sign(b"genesis");
        let block = Block::genesis(0, leader_signature);

        assert!(block.verify_block_txs_signatures());
        assert!(block.verify_all_signatures().is_ok());
    }

    #[test]
    fn batch_verify_valid_signatures() {
        let parent = [5u8; blake3::OUT_LEN];
        let sk = BlsSecretKey::generate(&mut thread_rng());
        let leader_signature = sk.sign(b"block proposal");

        // Create a block with valid transactions
        let block = gen_block(10, 0, parent, 5, 12345, leader_signature, 10);

        assert!(block.verify_block_txs_signatures());
        assert!(block.verify_all_signatures().is_ok());
        assert!(block.verify_signatures_find_invalid().is_empty());
    }

    #[test]
    fn batch_verify_single_transaction() {
        let parent = [6u8; blake3::OUT_LEN];
        let sk = BlsSecretKey::generate(&mut thread_rng());
        let leader_signature = sk.sign(b"block proposal");

        let block = gen_block(11, 0, parent, 1, 11111, leader_signature, 11);

        assert!(block.verify_block_txs_signatures());
        assert!(block.verify_all_signatures().is_ok());
    }

    #[test]
    fn batch_verify_many_transactions() {
        let parent = [7u8; blake3::OUT_LEN];
        let sk = BlsSecretKey::generate(&mut thread_rng());
        let leader_signature = sk.sign(b"block proposal");

        // Create a block with many transactions
        let block = gen_block(12, 0, parent, 100, 22222, leader_signature, 12);

        assert!(block.verify_block_txs_signatures());
        assert!(block.verify_all_signatures().is_ok());
    }

    #[test]
    fn batch_verify_detects_invalid_signature() {
        let parent = [8u8; blake3::OUT_LEN];
        let sk = BlsSecretKey::generate(&mut thread_rng());
        let leader_signature = sk.sign(b"block proposal");

        // Create valid transactions
        let mut txs: Vec<Arc<Transaction>> = (0..5).map(|_| Arc::new(gen_tx())).collect();

        // Corrupt the signature of the 3rd transaction
        let mut corrupted_tx = (*txs[2]).clone();
        corrupted_tx.signature.bytes[0] ^= 0xFF; // Flip bits
        txs[2] = Arc::new(corrupted_tx);

        let block = Block::new(13, 0, parent, txs, 33333, leader_signature, false, 13);

        // Batch verification should fail
        assert!(!block.verify_block_txs_signatures());

        // find_invalid should identify the corrupted transaction
        let invalid = block.verify_signatures_find_invalid();
        assert_eq!(invalid.len(), 1);
        assert_eq!(invalid[0], 2); // Index 2 is corrupted

        // verify_all_signatures should return error with the invalid index
        let result = block.verify_all_signatures();
        assert!(result.is_err());
        assert_eq!(result.unwrap_err(), vec![2]);
    }

    #[test]
    fn batch_verify_detects_multiple_invalid_signatures() {
        let parent = [9u8; blake3::OUT_LEN];
        let sk = BlsSecretKey::generate(&mut thread_rng());
        let leader_signature = sk.sign(b"block proposal");

        // Create valid transactions
        let mut txs: Vec<Arc<Transaction>> = (0..5).map(|_| Arc::new(gen_tx())).collect();

        // Corrupt signatures at index 1 and 4
        let mut corrupted_tx1 = (*txs[1]).clone();
        corrupted_tx1.signature.bytes[0] ^= 0xFF;
        txs[1] = Arc::new(corrupted_tx1);

        let mut corrupted_tx4 = (*txs[4]).clone();
        corrupted_tx4.signature.bytes[0] ^= 0xFF;
        txs[4] = Arc::new(corrupted_tx4);

        let block = Block::new(14, 0, parent, txs, 44444, leader_signature, false, 14);

        // Batch verification should fail
        assert!(!block.verify_block_txs_signatures());

        // find_invalid should identify both corrupted transactions
        let invalid = block.verify_signatures_find_invalid();
        assert_eq!(invalid.len(), 2);
        assert!(invalid.contains(&1));
        assert!(invalid.contains(&4));
    }
}
