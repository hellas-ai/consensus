use std::hash::{Hash, Hasher};

use crate::{
    crypto::{
        aggregated::{BlsPublicKey, BlsSignature},
        conversions::ArkSerdeWrapper,
    },
    state::now,
};
use rkyv::{Archive, Deserialize, Serialize, deserialize, rancor::Error, with::Skip};

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
    #[rkyv(with = Skip)]
    pub timestamp: u64,
    /// The fee of the transaction. This value is used to
    /// prioritize transactions in the mempool.
    pub fee: u64,
    /// The hash of the transaction's body content
    #[rkyv(with = Skip)]
    pub tx_hash: [u8; blake3::OUT_LEN],
    /// The sender's signature of the transaction's body content
    #[rkyv(with = ArkSerdeWrapper)]
    pub signature: BlsSignature,
}

impl Transaction {
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
        let timestamp = now();
        let archived = unsafe { rkyv::access_unchecked::<ArchivedTransaction>(bytes) };
        let mut tx = deserialize::<Transaction, Error>(archived).expect("Failed to deserialize");
        tx.tx_hash = tx_hash.into();
        tx.timestamp = timestamp;
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
