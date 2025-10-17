use crate::crypto::aggregated::BlsPublicKey;
use crate::crypto::conversions::ArkSerdeWrapper;
use rkyv::{Archive, Deserialize, Serialize};

/// [`Account`] represents an account in the consensus protocol.
///
/// An account is identified by its public key.
/// It contains the account's balance and a current nonce.
/// The nonce is used to prevent replay attacks.
#[derive(Archive, Deserialize, Serialize, Clone, Debug)]
pub struct Account {
    /// The account's public key
    #[rkyv(with = ArkSerdeWrapper)]
    pub public_key: BlsPublicKey,
    /// The account's balance
    pub balance: u64,
    /// The account's current nonce
    pub nonce: u64,
}

impl Account {
    pub fn new(public_key: BlsPublicKey, balance: u64, nonce: u64) -> Self {
        Self {
            public_key,
            balance,
            nonce,
        }
    }
}

impl PartialEq for Account {
    fn eq(&self, other: &Self) -> bool {
        self.public_key.0 == other.public_key.0
    }
}

impl Eq for Account {}
