//! RPC node identity - Ed25519 only (no BLS).

use commonware_cryptography::{Signer, ed25519};
use rand::{CryptoRng, RngCore};
use zeroize::Zeroize;

/// Identity for an RPC node.
///
/// Unlike validators, RPC nodes only need an Ed25519 keypair for P2P networking.
/// They do not participate in consensus and do not require BLS keys.
///
/// # Security
/// The seed is zeroized on drop to prevent secret key material from lingering in memory.
pub struct RpcIdentity {
    ed25519_key: ed25519::PrivateKey,
    seed: u64,
}

impl RpcIdentity {
    /// Generate a new random identity.
    pub fn generate<R: RngCore + CryptoRng>(rng: &mut R) -> Self {
        let seed = rng.next_u64();
        Self {
            ed25519_key: ed25519::PrivateKey::from_seed(seed),
            seed,
        }
    }

    /// Create identity from a specific seed (for deterministic testing or persistence).
    pub fn from_seed(seed: u64) -> Self {
        Self {
            ed25519_key: ed25519::PrivateKey::from_seed(seed),
            seed,
        }
    }

    /// Get the seed used to generate this identity.
    ///
    /// # Security
    /// Only use this for persistence. The seed is equivalent to the private key.
    pub fn seed(&self) -> u64 {
        self.seed
    }

    /// Get the Ed25519 public key.
    pub fn public_key(&self) -> ed25519::PublicKey {
        self.ed25519_key.public_key()
    }

    /// Clone the Ed25519 private key (for P2P signer).
    pub fn clone_ed25519_private_key(&self) -> ed25519::PrivateKey {
        ed25519::PrivateKey::from_seed(self.seed)
    }

    /// Get the Ed25519 public key bytes.
    pub fn public_key_bytes(&self) -> [u8; 32] {
        let pk = self.ed25519_key.public_key();
        let mut bytes = [0u8; 32];
        bytes.copy_from_slice(pk.as_ref());
        bytes
    }

    /// Save identity seed to a file (hex-encoded).
    ///
    /// # Security
    /// The file will contain the private key material. Ensure proper file permissions.
    pub fn save_to_file(&self, path: &std::path::Path) -> std::io::Result<()> {
        use std::io::Write;
        // Create parent directories if they don't exist
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let mut file = std::fs::File::create(path)?;
        writeln!(file, "{:016x}", self.seed)?;
        Ok(())
    }

    /// Load identity from a seed file (hex-encoded).
    pub fn load_from_file(path: &std::path::Path) -> std::io::Result<Self> {
        let contents = std::fs::read_to_string(path)?;
        let seed = u64::from_str_radix(contents.trim(), 16)
            .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e))?;
        Ok(Self::from_seed(seed))
    }

    /// Load identity from file if it exists, otherwise generate and save.
    pub fn load_or_generate<R: RngCore + CryptoRng>(
        path: Option<&std::path::Path>,
        rng: &mut R,
    ) -> std::io::Result<Self> {
        match path {
            Some(p) if p.exists() => Self::load_from_file(p),
            Some(p) => {
                let identity = Self::generate(rng);
                identity.save_to_file(p)?;
                Ok(identity)
            }
            None => Ok(Self::generate(rng)),
        }
    }
}

impl Drop for RpcIdentity {
    fn drop(&mut self) {
        self.seed.zeroize();
    }
}

impl Clone for RpcIdentity {
    fn clone(&self) -> Self {
        Self::from_seed(self.seed)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use rand::rngs::OsRng;

    #[test]
    fn test_generate_identity() {
        let identity = RpcIdentity::generate(&mut OsRng);
        let pk = identity.public_key();
        assert_eq!(pk.as_ref().len(), 32);
    }

    #[test]
    fn test_from_seed_deterministic() {
        let id1 = RpcIdentity::from_seed(12345);
        let id2 = RpcIdentity::from_seed(12345);
        assert_eq!(id1.public_key_bytes(), id2.public_key_bytes());
    }

    #[test]
    fn test_public_key_bytes() {
        let identity = RpcIdentity::generate(&mut OsRng);
        let bytes = identity.public_key_bytes();
        assert_eq!(bytes.len(), 32);
    }

    #[test]
    fn test_clone() {
        let identity = RpcIdentity::generate(&mut OsRng);
        let cloned = identity.clone();
        assert_eq!(identity.public_key_bytes(), cloned.public_key_bytes());
    }
}
