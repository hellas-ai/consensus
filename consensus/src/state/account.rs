use crate::crypto::transaction_crypto::SerializableTxPublicKey;
use crate::crypto::transaction_crypto::TxPublicKey;
use rkyv::{Archive, Deserialize, Serialize};

/// [`Account`] represents an account in the consensus protocol.
///
/// An account is identified by its public key.
/// It contains the account's balance and a current nonce.
/// The nonce is used to prevent replay attacks.
#[derive(Archive, Deserialize, Serialize, Clone, Debug)]
pub struct Account {
    /// The account's public key
    pub public_key: SerializableTxPublicKey,
    /// The account's balance
    pub balance: u64,
    /// The account's current nonce
    pub nonce: u64,
}

impl Account {
    pub fn new(public_key: TxPublicKey, balance: u64, nonce: u64) -> Self {
        Self {
            public_key: SerializableTxPublicKey::from(&public_key),
            balance,
            nonce,
        }
    }
}

impl PartialEq for Account {
    fn eq(&self, other: &Self) -> bool {
        self.public_key.bytes == other.public_key.bytes
    }
}

impl Eq for Account {}
