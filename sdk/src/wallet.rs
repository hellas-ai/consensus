//! Wallet for key management and transaction signing

use crate::error::{Error, Result};
use crate::types::Address;
use consensus::crypto::transaction_crypto::{TxPublicKey, TxSecretKey, TxSignature};
use rand::rngs::OsRng;

/// A wallet containing a keypair for signing transactions.
///
/// The wallet stores an Ed25519 keypair used to sign transactions.
/// The address is derived from the public key.
pub struct Wallet {
    secret_key: TxSecretKey,
    public_key: TxPublicKey,
    address: Address,
}

impl Wallet {
    /// Generate a new random wallet.
    ///
    /// # Example
    /// ```
    /// use hellas_sdk::Wallet;
    ///
    /// let wallet = Wallet::generate();
    /// println!("New wallet address: {}", wallet.address());
    /// ```
    pub fn generate() -> Self {
        let secret_key = TxSecretKey::generate(&mut OsRng);
        Self::from_secret_key_inner(secret_key)
    }

    /// Create wallet from secret key bytes.
    ///
    /// # Arguments
    /// * `bytes` - 32-byte Ed25519 secret key
    pub fn from_secret_key(bytes: &[u8; 32]) -> Self {
        let secret_key = TxSecretKey::from_bytes(bytes);
        Self::from_secret_key_inner(secret_key)
    }

    /// Create wallet from hex-encoded secret key.
    ///
    /// # Arguments
    /// * `hex` - 64-character hex string
    pub fn from_hex(hex: &str) -> Result<Self> {
        let bytes = hex::decode(hex).map_err(|e| Error::Parse(e.to_string()))?;
        if bytes.len() != 32 {
            return Err(Error::InvalidArgument(format!(
                "Secret key must be 32 bytes, got {}",
                bytes.len()
            )));
        }
        let mut arr = [0u8; 32];
        arr.copy_from_slice(&bytes);
        Ok(Self::from_secret_key(&arr))
    }

    fn from_secret_key_inner(secret_key: TxSecretKey) -> Self {
        let public_key = secret_key.public_key();
        let address = Address::from_bytes(public_key.to_bytes());
        Self {
            secret_key,
            public_key,
            address,
        }
    }

    /// Get the wallet's address.
    pub fn address(&self) -> &Address {
        &self.address
    }

    /// Get the public key.
    pub fn public_key(&self) -> &TxPublicKey {
        &self.public_key
    }

    /// Sign a transaction hash.
    ///
    /// # Arguments
    /// * `tx_hash` - 32-byte transaction hash to sign
    pub fn sign(&self, tx_hash: &[u8; 32]) -> TxSignature {
        self.secret_key.sign(tx_hash)
    }

    /// Export secret key bytes.
    ///
    /// # Warning
    /// Handle with care - this exposes the raw secret key.
    pub fn to_secret_bytes(&self) -> [u8; 32] {
        self.secret_key.to_bytes()
    }

    /// Export secret key as hex string.
    ///
    /// # Warning
    /// Handle with care - this exposes the raw secret key.
    pub fn to_secret_hex(&self) -> String {
        hex::encode(self.secret_key.to_bytes())
    }
}

impl std::fmt::Debug for Wallet {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Wallet")
            .field("address", &self.address)
            .finish_non_exhaustive()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn generate_wallet() {
        let wallet = Wallet::generate();
        assert!(!wallet.address().to_hex().is_empty());
    }

    #[test]
    fn wallet_from_hex_roundtrip() {
        let wallet = Wallet::generate();
        let hex = wallet.to_secret_hex();
        let wallet2 = Wallet::from_hex(&hex).unwrap();
        assert_eq!(wallet.address(), wallet2.address());
    }

    #[test]
    fn sign_and_verify() {
        let wallet = Wallet::generate();
        let message = [42u8; 32];
        let signature = wallet.sign(&message);
        assert!(wallet.public_key().verify(&message, &signature));
    }
}
