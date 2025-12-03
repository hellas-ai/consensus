//! Cryptographic primitives for transaction signing (Ed25519)
//!
//! This module provides Ed25519 signatures for user transactions,
//! separate from the BLS signatures used in consensus.

use ed25519_dalek::{
    PUBLIC_KEY_LENGTH, SECRET_KEY_LENGTH, SIGNATURE_LENGTH, Signature, Signer, SigningKey,
    Verifier, VerifyingKey,
};
use rand::{CryptoRng, RngCore};
use rkyv::{Archive, Deserialize, Serialize};
use thiserror::Error;

type Result<T> = std::result::Result<T, SignatureError>;

/// A 32-byte Ed25519 public key for transaction signing
#[derive(Clone, Debug)]
pub struct TxPublicKey(pub VerifyingKey);

/// A 32-byte Ed25519 secret key for transaction signing
#[derive(Clone)]
pub struct TxSecretKey(pub SigningKey);

/// A 64-byte Ed25519 signature
#[derive(Clone, Debug)]
pub struct TxSignature(pub Signature);

impl TxSecretKey {
    /// Generate a new random secret key using the provided RNG
    pub fn generate<R: CryptoRng + RngCore>(rng: &mut R) -> Self {
        let signing_key = SigningKey::generate(rng);
        Self(signing_key)
    }

    /// Create from raw bytes
    pub fn from_bytes(bytes: &[u8; SECRET_KEY_LENGTH]) -> Result<Self> {
        let signing_key = SigningKey::from_bytes(bytes);
        Ok(Self(signing_key))
    }

    /// Get the corresponding public key
    pub fn public_key(&self) -> TxPublicKey {
        TxPublicKey(self.0.verifying_key())
    }

    /// Sign a message
    pub fn sign(&self, message: &[u8]) -> TxSignature {
        TxSignature(self.0.sign(message))
    }

    /// Export to bytes
    pub fn to_bytes(&self) -> [u8; SECRET_KEY_LENGTH] {
        self.0.to_bytes()
    }
}

impl TxPublicKey {
    /// Create from raw bytes
    pub fn from_bytes(bytes: &[u8; PUBLIC_KEY_LENGTH]) -> Result<Self> {
        let verifying_key =
            VerifyingKey::from_bytes(bytes).map_err(|_| SignatureError::InvalidPublicKey)?;
        Ok(Self(verifying_key))
    }

    /// Verify a signature
    pub fn verify(&self, message: &[u8], signature: &TxSignature) -> bool {
        self.0.verify(message, &signature.0).is_ok()
    }

    /// Export to bytes
    pub fn to_bytes(&self) -> [u8; PUBLIC_KEY_LENGTH] {
        self.0.to_bytes()
    }
}

impl TxSignature {
    /// Create from raw bytes
    pub fn from_bytes(bytes: &[u8; SIGNATURE_LENGTH]) -> Result<Self> {
        let signature = Signature::from_bytes(bytes);
        Ok(Self(signature))
    }

    /// Export to bytes
    pub fn to_bytes(&self) -> [u8; SIGNATURE_LENGTH] {
        self.0.to_bytes()
    }
}

#[derive(Debug, Error)]
pub enum SignatureError {
    InvalidPublicKey,
    InvalidSignature,
    InvalidSecretKey,
}

impl std::fmt::Display for SignatureError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::InvalidPublicKey => write!(f, "Invalid public key"),
            Self::InvalidSignature => write!(f, "Invalid signature"),
            Self::InvalidSecretKey => write!(f, "Invalid secret key"),
        }
    }
}

/// Wrapper for rkyv serialization of TxPublicKey
#[derive(Archive, Deserialize, Serialize, Clone, Debug)]
pub struct SerializableTxPublicKey {
    pub bytes: [u8; PUBLIC_KEY_LENGTH],
}

impl From<&TxPublicKey> for SerializableTxPublicKey {
    fn from(pk: &TxPublicKey) -> Self {
        Self {
            bytes: pk.to_bytes(),
        }
    }
}

impl TryFrom<SerializableTxPublicKey> for TxPublicKey {
    type Error = SignatureError;
    fn try_from(spk: SerializableTxPublicKey) -> Result<Self> {
        TxPublicKey::from_bytes(&spk.bytes)
    }
}

/// Wrapper for rkyv serialization of TxSignature
#[derive(Archive, Deserialize, Serialize, Clone, Debug)]
pub struct SerializableTxSignature {
    pub bytes: [u8; SIGNATURE_LENGTH],
}

impl From<&TxSignature> for SerializableTxSignature {
    fn from(sig: &TxSignature) -> Self {
        Self {
            bytes: sig.to_bytes(),
        }
    }
}

impl TryFrom<&SerializableTxSignature> for TxSignature {
    type Error = SignatureError;
    fn try_from(ssig: &SerializableTxSignature) -> Result<Self> {
        TxSignature::from_bytes(&ssig.bytes)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sign_and_verify() {
        let sk = TxSecretKey::generate(&mut rand::rngs::OsRng);
        let pk = sk.public_key();

        let message = b"hello world";
        let signature = sk.sign(message);

        assert!(pk.verify(message, &signature));
        assert!(!pk.verify(b"wrong message", &signature));
    }

    #[test]
    fn roundtrip_bytes() {
        let sk = TxSecretKey::generate(&mut rand::rngs::OsRng);
        let pk = sk.public_key();
        let sig = sk.sign(b"test");

        // Roundtrip public key
        let pk_bytes = pk.to_bytes();
        let pk2 = TxPublicKey::from_bytes(&pk_bytes).unwrap();
        assert_eq!(pk.to_bytes(), pk2.to_bytes());

        // Roundtrip signature
        let sig_bytes = sig.to_bytes();
        let sig2 = TxSignature::from_bytes(&sig_bytes).unwrap();
        assert!(pk.verify(b"test", &sig2));
    }
}
