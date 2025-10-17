use crate::crypto::{
    aggregated::{BlsPublicKey, BlsSignature},
    conversions::ArkSerdeWrapper,
};
use rkyv::{Archive, Deserialize, Serialize, deserialize, rancor::Error};

/// [`Transaction`] represents a transaction in the consensus protocol.
///
/// A transaction is a message that is sent between two parties.
/// In an initial version, it contains the sender, recipient, amount,
/// nonce, timestamp, and fee. In the future, we will expand its scope.
/// The signature is used to verify the transaction.
#[derive(Archive, Deserialize, Serialize, Debug, Clone)]
pub struct Transaction {
    /// The sender of the transaction
    #[rkyv(with = ArkSerdeWrapper)]
    pub sender: BlsPublicKey,
    /// The recipient of the transaction
    pub recipient: [u8; 32],
    /// The amount of the transaction
    pub amount: u64,
    /// The nonce of the transaction. This value is
    /// incremental and used to prevent replay attacks.
    pub nonce: u64,
    /// The timestamp of the transaction, as measured by the
    /// peer proposing such transaction.
    pub timestamp: u64,
    /// The fee of the transaction. This value is used to
    /// prioritize transactions in the mempool.
    pub fee: u64,
    /// The hash of the transaction's body content
    pub tx_hash: [u8; blake3::OUT_LEN],
    /// The sender's signature of the transaction's body content
    #[rkyv(with = ArkSerdeWrapper)]
    pub signature: BlsSignature,
}

impl Transaction {
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        sender: BlsPublicKey,
        recipient: [u8; 32],
        amount: u64,
        nonce: u64,
        timestamp: u64,
        fee: u64,
        tx_hash: [u8; blake3::OUT_LEN],
        signature: BlsSignature,
    ) -> Self {
        Self {
            sender,
            recipient,
            amount,
            nonce,
            timestamp,
            fee,
            tx_hash,
            signature,
        }
    }

    /// Computes the transaction from its bytes
    pub fn from_tx_bytes(bytes: &[u8]) -> Self {
        let tx_hash = blake3::hash(bytes);
        let archived = unsafe { rkyv::access_unchecked::<ArchivedTransaction>(bytes) };
        let mut tx = deserialize::<Transaction, Error>(archived).expect("Failed to deserialize");
        tx.tx_hash = tx_hash.into();
        tx
    }

    /// Verifies the transaction
    pub fn verify(&self) -> bool {
        self.sender.verify(&self.tx_hash, &self.signature)
    }
}

impl PartialEq for Transaction {
    fn eq(&self, other: &Self) -> bool {
        self.tx_hash == other.tx_hash
    }
}

impl Eq for Transaction {}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::crypto::aggregated::BlsSecretKey;
    use crate::storage::conversions::serialize_for_db;
    use rand::thread_rng;

    fn gen_keypair() -> (BlsSecretKey, BlsPublicKey) {
        let mut rng = thread_rng();
        let sk = BlsSecretKey::generate(&mut rng);
        let pk = sk.public_key();
        (sk, pk)
    }

    #[test]
    fn verify_true_with_matching_signature() {
        let (sk, pk) = gen_keypair();
        let body = b"tx-body";
        let tx_hash: [u8; blake3::OUT_LEN] = blake3::hash(body).into();
        let sig = sk.sign(&tx_hash);

        let tx = Transaction::new(pk.clone(), [1u8; 32], 10, 1, 123, 2, tx_hash, sig);
        assert!(tx.verify());
    }

    #[test]
    fn verify_false_when_signature_or_hash_mismatch() {
        let (sk1, pk1) = gen_keypair();
        let (sk2, _pk2) = gen_keypair();

        let body = b"tx-body-x";
        let tx_hash: [u8; blake3::OUT_LEN] = blake3::hash(body).into();

        // Use a signature made with a different secret key
        let bad_sig = sk2.sign(&tx_hash);
        let tx_bad_sig = Transaction::new(pk1.clone(), [2u8; 32], 5, 1, 1, 1, tx_hash, bad_sig);
        assert!(!tx_bad_sig.verify());

        // Use a signature over a different hash
        let sig = sk1.sign(&tx_hash);
        let other_hash: [u8; blake3::OUT_LEN] = blake3::hash(b"different").into();
        let tx_bad_hash = Transaction::new(pk1.clone(), [2u8; 32], 5, 1, 1, 1, other_hash, sig);
        assert!(!tx_bad_hash.verify());
    }

    #[test]
    fn equality_is_by_tx_hash_only() {
        let (sk, pk) = gen_keypair();
        let h: [u8; blake3::OUT_LEN] = blake3::hash(b"same").into();
        let sig = sk.sign(&h);

        let a = Transaction::new(pk.clone(), [9u8; 32], 100, 1, 1, 1, h, sig.clone());
        // Different amount/nonce/fee, but same tx_hash → equal
        let b = Transaction::new(pk.clone(), [9u8; 32], 999, 999, 1, 2, h, sig);
        assert_eq!(a, b);
    }

    #[test]
    fn from_tx_bytes_sets_hash_to_bytes_digest() {
        let (sk, pk) = gen_keypair();
        let body = b"payload";
        let tx_hash_original: [u8; blake3::OUT_LEN] = blake3::hash(body).into();
        let sig = sk.sign(&tx_hash_original);

        let tx = Transaction::new(pk.clone(), [3u8; 32], 1, 2, 3, 4, tx_hash_original, sig);
        let bytes = serialize_for_db(&tx).expect("serialize");
        let restored = Transaction::from_tx_bytes(bytes.as_slice());

        // tx_hash is set to blake3(bytes), not the original content hash
        let expected: [u8; blake3::OUT_LEN] = blake3::hash(bytes.as_slice()).into();
        assert_eq!(restored.tx_hash, expected);

        // Other fields round-trip via rkyv deserialize
        assert_eq!(restored.recipient, tx.recipient);
        assert_eq!(restored.amount, tx.amount);
        assert_eq!(restored.nonce, tx.nonce);
        assert_eq!(restored.timestamp, tx.timestamp);
        assert_eq!(restored.fee, tx.fee);

        // Signature was created over the original tx_hash, so verify is expected to be false now
        assert!(!restored.verify());
    }
}
