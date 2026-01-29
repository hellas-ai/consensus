//! Common types used throughout the SDK

use std::fmt;
use std::str::FromStr;

use crate::error::Error;

/// 32-byte address (derived from Ed25519 public key).
#[derive(Clone, Copy, PartialEq, Eq, Hash, Default)]
pub struct Address(pub [u8; 32]);

impl Address {
    /// Create from raw bytes.
    pub fn from_bytes(bytes: [u8; 32]) -> Self {
        Self(bytes)
    }

    /// Create from hex string (64 characters).
    pub fn from_hex(hex: &str) -> Result<Self, Error> {
        let bytes = hex::decode(hex)?;
        if bytes.len() != 32 {
            return Err(Error::InvalidArgument(format!(
                "Address must be 32 bytes, got {}",
                bytes.len()
            )));
        }
        let mut arr = [0u8; 32];
        arr.copy_from_slice(&bytes);
        Ok(Self(arr))
    }

    /// Export to bytes.
    pub fn to_bytes(&self) -> [u8; 32] {
        self.0
    }

    /// Export as hex string.
    pub fn to_hex(&self) -> String {
        hex::encode(self.0)
    }
}

impl fmt::Display for Address {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", hex::encode(self.0))
    }
}

impl fmt::Debug for Address {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "Address({})", self.to_hex())
    }
}

impl FromStr for Address {
    type Err = Error;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        Self::from_hex(s)
    }
}

impl From<consensus::state::address::Address> for Address {
    fn from(addr: consensus::state::address::Address) -> Self {
        Self(addr.0)
    }
}

impl From<Address> for consensus::state::address::Address {
    fn from(addr: Address) -> Self {
        consensus::state::address::Address(addr.0)
    }
}

/// 32-byte hash (block hash, transaction hash).
#[derive(Clone, Copy, PartialEq, Eq, Hash, Default)]
pub struct Hash(pub [u8; 32]);

impl Hash {
    /// Create from raw bytes.
    pub fn from_bytes(bytes: [u8; 32]) -> Self {
        Self(bytes)
    }

    /// Create from hex string (64 characters).
    pub fn from_hex(hex: &str) -> Result<Self, Error> {
        let bytes = hex::decode(hex)?;
        if bytes.len() != 32 {
            return Err(Error::InvalidArgument(format!(
                "Hash must be 32 bytes, got {}",
                bytes.len()
            )));
        }
        let mut arr = [0u8; 32];
        arr.copy_from_slice(&bytes);
        Ok(Self(arr))
    }

    /// Export to bytes.
    pub fn to_bytes(&self) -> [u8; 32] {
        self.0
    }

    /// Export as hex string.
    pub fn to_hex(&self) -> String {
        hex::encode(self.0)
    }
}

impl fmt::Display for Hash {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", hex::encode(self.0))
    }
}

impl fmt::Debug for Hash {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "Hash({})", self.to_hex())
    }
}

impl FromStr for Hash {
    type Err = Error;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        Self::from_hex(s)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn address_roundtrip() {
        let bytes = [42u8; 32];
        let addr = Address::from_bytes(bytes);
        assert_eq!(addr.to_bytes(), bytes);

        let hex = addr.to_hex();
        let addr2 = Address::from_hex(&hex).unwrap();
        assert_eq!(addr, addr2);
    }

    #[test]
    fn hash_roundtrip() {
        let bytes = [0xab; 32];
        let hash = Hash::from_bytes(bytes);
        assert_eq!(hash.to_bytes(), bytes);

        let hex = hash.to_hex();
        let hash2 = Hash::from_hex(&hex).unwrap();
        assert_eq!(hash, hash2);
    }

    #[test]
    fn invalid_hex_length() {
        assert!(Address::from_hex("abcd").is_err());
        assert!(Hash::from_hex("1234").is_err());
    }
}
