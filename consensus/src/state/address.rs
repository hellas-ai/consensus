use rkyv::{Archive, Deserialize, Serialize};

use crate::crypto::transaction_crypto::TxPublicKey;

const ADDRESS_LENGTH: usize = 32;

/// A 32-byte account address.
///
/// For Ed25519, the address is the public key.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Archive, Deserialize, Serialize)]
pub struct Address(pub [u8; ADDRESS_LENGTH]);

impl Address {
    /// Creates an address from an Ed25519 public key
    /// For Ed25519, address = public key bytes (both 32 bytes)
    pub fn from_public_key(public_key: &TxPublicKey) -> Self {
        Self(public_key.to_bytes())
    }

    /// Creates an address from raw bytes
    pub fn from_bytes(bytes: [u8; 32]) -> Self {
        Self(bytes)
    }

    /// System address for minting (address(0))
    pub const MINT_AUTHORITY: Address = Address([0u8; 32]);

    /// Returns the bytes of the address
    pub fn as_bytes(&self) -> &[u8; 32] {
        &self.0
    }

    /// Converts back to public key (for Ed25519, address = pubkey)
    pub fn to_public_key(
        &self,
    ) -> Result<TxPublicKey, crate::crypto::transaction_crypto::SignatureError> {
        TxPublicKey::from_bytes(&self.0)
    }
}

impl std::fmt::Display for Address {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        // Display first 8 bytes in hex (like Solana's short format)
        write!(f, "{}", hex::encode(&self.0[..8]))
    }
}

impl AsRef<[u8]> for Address {
    fn as_ref(&self) -> &[u8] {
        &self.0
    }
}
