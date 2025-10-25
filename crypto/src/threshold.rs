use anyhow::Result;
use blst::{
    BLST_ERROR, blst_hash_to_g1, blst_p1, blst_p1_add_or_double, blst_p1_affine,
    blst_p1_affine_compress, blst_p1_affine_in_g1, blst_p1_affine_serialize, blst_p1_deserialize,
    blst_p1_from_affine, blst_p1_mult, blst_p1_to_affine, blst_p2, blst_p2_affine,
    blst_p2_affine_compress, blst_p2_generator, blst_p2_mult, blst_p2_to_affine, min_sig::*,
};
use rand::{CryptoRng, Rng};

use crate::{polynomial::Polynomial, scalar::Scalar};

/// The domain separation tag for BLS signatures.
const DST: &[u8] = b"BLS_SIG_BLS12381G1_XMD:SHA-256_SSWU_RO_NUL_";

/// Share of a secret in Shamir's secret sharing scheme.
/// Each share consists of an x-coordinate (index) and y-coordinate (secret share).
#[derive(Clone, Debug)]
pub struct Share {
    /// The x-coordinate (participant index from 1 to n)
    pub x: Scalar,
    /// The y-coordinate (secret share value)
    pub y: Scalar,
}

/// Shamir's secret sharing scheme for threshold cryptography.
/// Allows splitting a secret into n shares where any t shares can reconstruct the secret.
pub struct ShamirSharing {
    /// Minimum shares needed to reconstruct the secret
    threshold: usize,
    /// Total number of shares to generate
    total_shares: usize,
}

impl ShamirSharing {
    /// Create a new Shamir sharing scheme.
    ///
    /// # Arguments
    /// * `threshold` - Minimum shares needed for reconstruction (t)
    /// * `total_shares` - Total shares to generate (n)
    ///
    /// # Panics
    /// Panics if threshold > total_shares or threshold == 0
    pub fn new(threshold: usize, total_shares: usize) -> Self {
        debug_assert!(threshold > 0 && threshold <= total_shares);
        Self {
            threshold,
            total_shares,
        }
    }

    /// Split a secret into shares using Shamir's scheme.
    /// Creates a random polynomial of degree t-1 with the secret as constant term,
    /// then evaluates at points 1, 2, ..., n to generate shares.
    ///
    /// # Arguments
    /// * `secret` - The secret to split
    /// * `rng` - Cryptographically secure random number generator
    ///
    /// # Returns
    /// Vector of n shares, any t of which can reconstruct the secret
    pub fn split<R: Rng + CryptoRng>(&self, secret: Scalar, rng: &mut R) -> Vec<Share> {
        // Create polynomial with secret as constant term
        let poly = Polynomial::random(self.threshold - 1, secret, rng);

        // Evaluate at points 1, 2, 3, ..., n
        let mut shares = Vec::with_capacity(self.total_shares);
        for i in 1..=self.total_shares {
            let x = Scalar::from_u64(i as u64);
            let y = poly.evaluate(&x);
            shares.push(Share { x, y });
        }

        shares
    }

    /// Compute Lagrange coefficient λ_i for interpolation at x=0.
    /// λ_i = ∏_{j≠i} x_j / (x_j - x_i)
    ///
    /// Used in reconstruction to weight each share appropriately.
    ///
    /// # Arguments
    /// * `shares` - Slice of shares to use for interpolation
    /// * `i` - Index of the share to compute the coefficient for
    ///
    /// # Returns
    /// Lagrange coefficient λ_i for the given share
    pub fn lagrange_coefficient(&self, shares: &[Share], i: usize) -> Scalar {
        let mut numerator = Scalar::one();
        let mut denominator = Scalar::one();

        for (j, share_j) in shares.iter().enumerate() {
            if i != j {
                // numerator *= x_j
                numerator = numerator.mul(&share_j.x);

                // denominator *= (x_j - x_i)
                let diff = share_j.x.sub(&shares[i].x);
                denominator = denominator.mul(&diff);
            }
        }

        // Return numerator / denominator
        numerator
            .div(&denominator)
            .expect("Division by zero in Lagrange coefficient")
    }

    /// Reconstruct secret from threshold shares using Lagrange interpolation.
    /// Combines shares with formula: secret = ∑ y_i × λ_i
    ///
    /// # Arguments
    /// * `shares` - At least t shares to reconstruct from
    ///
    /// # Panics
    /// Panics if fewer than threshold shares provided
    pub fn reconstruct(&self, shares: &[Share]) -> Scalar {
        debug_assert!(
            shares.len() >= self.threshold,
            "Not enough shares to reconstruct"
        );

        let shares_to_use = &shares[..self.threshold];
        let mut secret = Scalar::zero();

        for (i, share) in shares_to_use.iter().enumerate() {
            let lambda = self.lagrange_coefficient(shares_to_use, i);
            let term = share.y.mul(&lambda);
            secret = secret.add(&term);
        }

        secret
    }
}

/// Key share for a participant in threshold BLS signature scheme.
/// Contains the participant's secret scalar and corresponding public key.
#[derive(Clone)]
pub struct KeyShare {
    /// Participant identifier
    pub id: u64,
    /// Secret scalar share
    pub secret_scalar: Scalar,
    /// Public key corresponding to secret_scalar
    pub public_key: PublicKey,
}

/// Partial signature from a single participant.
/// Contains the participant ID and their signature share.
#[derive(Clone)]
pub struct PartialSignature {
    /// Participant identifier
    pub id: u64,
    /// Signature share
    pub signature: Signature,
}

/// BLS threshold signature scheme using Shamir's secret sharing.
/// Enables distributed signing where t-out-of-n participants can create a valid signature.
pub struct ThresholdBLS {
    threshold: usize,
    total_participants: usize,
    shamir: ShamirSharing,
}

impl ThresholdBLS {
    /// Create a new threshold BLS scheme.
    ///
    /// # Arguments
    /// * `threshold` - Minimum participants needed for valid signature (t)
    /// * `total_participants` - Total number of participants (n)
    pub fn new(threshold: usize, total_participants: usize) -> Self {
        Self {
            threshold,
            total_participants,
            shamir: ShamirSharing::new(threshold, total_participants),
        }
    }

    /// Convert a scalar (private key) to its corresponding BLS public key.
    ///
    /// Computes PK = scalar × G₂, where G₂ is the generator point of the G₂ group.
    /// This is the standard BLS key derivation: public key is private key times generator.
    ///
    /// # Arguments
    /// * `scalar` - The private key scalar
    ///
    /// # Returns
    /// The corresponding BLS public key, or error if key derivation fails
    ///
    /// # Security
    /// This operation is computationally expensive but secure.
    /// The resulting public key can be safely shared.
    fn scalar_to_public_key(scalar: &Scalar) -> Result<PublicKey> {
        unsafe {
            // Get G2 generator
            let generator = blst_p2_generator();

            // Multiply generator by scalar
            let mut pk_point = blst_p2::default();
            blst_p2_mult(&mut pk_point, generator, scalar.to_bytes_le().as_ptr(), 255);

            // Convert to affine
            let mut pk_affine = blst_p2_affine::default();
            blst_p2_to_affine(&mut pk_affine, &pk_point);

            // Serialize
            let mut pk_bytes = [0u8; 96];
            blst_p2_affine_compress(pk_bytes.as_mut_ptr(), &pk_affine);

            // Create PublicKey from bytes
            PublicKey::from_bytes(&pk_bytes)
                .map_err(|e| anyhow::anyhow!("Failed to create public key: {:?}", e))
        }
    }

    /// Generate master keypair and distribute key shares.
    /// Creates a random master secret, derives public key, then splits secret into shares.
    ///
    /// # Arguments
    /// * `rng` - Cryptographically secure random number generator
    ///
    /// # Returns
    /// Tuple of (master_public_key, key_shares)
    pub fn trusted_setup<R: Rng + CryptoRng>(
        &self,
        rng: &mut R,
    ) -> Result<(PublicKey, Vec<KeyShare>)> {
        let master_scalar = Scalar::random(rng);
        let master_pk = Self::scalar_to_public_key(&master_scalar)
            .map_err(|e| anyhow::anyhow!("Failed to create master public key: {:?}", e))?;

        // Create shares using Shamir's Secret Sharing
        let shares = self.shamir.split(master_scalar, rng);

        // Convert shares to KeyShare objects
        let mut key_shares = Vec::with_capacity(self.total_participants);
        for (idx, share) in shares.iter().enumerate() {
            let share_pk = Self::scalar_to_public_key(&share.y)
                .map_err(|e| anyhow::anyhow!("Failed to create share public key: {:?}", e))?;

            key_shares.push(KeyShare {
                id: (idx + 1) as u64,
                secret_scalar: share.y.clone(),
                public_key: share_pk,
            });
        }

        Ok((master_pk, key_shares))
    }

    /// Create a partial signature for a message using a key share.
    ///
    /// # Arguments
    /// * `key_share` - Participant's key share
    /// * `message` - Message to sign
    ///
    /// # Returns
    /// Partial signature that can be combined with others
    pub fn partial_sign(key_share: &KeyShare, message: &[u8]) -> Result<PartialSignature> {
        unsafe {
            // Hash message to G1 point
            let mut hash_point = blst_p1::default();
            blst_hash_to_g1(
                &mut hash_point,
                message.as_ptr(),
                message.len(),
                DST.as_ptr(),
                DST.len(),
                std::ptr::null(),
                0,
            );

            // Multiply hash by secret scalar: sig = H(m) * scalar
            let mut sig_point = blst_p1::default();
            blst_p1_mult(
                &mut sig_point,
                &hash_point,
                key_share.secret_scalar.to_bytes_le().as_ptr(),
                255,
            );

            // Convert to affine
            let mut sig_affine = blst_p1_affine::default();
            blst_p1_to_affine(&mut sig_affine, &sig_point);

            // Serialize
            let mut sig_bytes = [0u8; 48];
            blst_p1_affine_compress(sig_bytes.as_mut_ptr(), &sig_affine);

            // Create Signature from bytes
            let signature = Signature::from_bytes(&sig_bytes)
                .map_err(|e| anyhow::anyhow!("Failed to create signature: {:?}", e))?;

            Ok(PartialSignature {
                id: key_share.id,
                signature,
            })
        }
    }

    /// Aggregate partial signatures into a complete signature.
    /// Uses Lagrange interpolation in the exponent to combine signatures correctly.
    ///
    /// # Arguments
    /// * `partial_signatures` - At least t partial signatures to aggregate
    ///
    /// # Returns
    /// Complete BLS signature
    pub fn aggregate(&self, partial_signatures: &[PartialSignature]) -> Result<Signature> {
        if partial_signatures.len() < self.threshold {
            return Err(anyhow::anyhow!(
                "Not enough partial signatures to aggregate: got {}, expected {}",
                partial_signatures.len(),
                self.threshold
            ));
        }

        // Use exactly threshold signatures
        let sigs_to_use = &partial_signatures[..self.threshold];

        // Build shares for Lagrange coefficient computation
        let shares: Vec<Share> = sigs_to_use
            .iter()
            .map(|ps| Share {
                x: Scalar::from_u64(ps.id),
                y: Scalar::zero(), // Not used for coefficient calculation
            })
            .collect();

        let lambdas: Vec<Scalar> = (0..self.threshold)
            .map(|i| self.shamir.lagrange_coefficient(&shares, i))
            .collect();

        // Steps 2 & 3: Multiply each signature by its Lagrange coefficient and aggregate
        // σ = ∏ σᵢ^λᵢ = ∑ (σᵢ * λᵢ) in additive notation
        let mut aggregated_point = blst_p1::default();

        for (i, (partial, lambda)) in sigs_to_use.iter().zip(lambdas.iter()).enumerate() {
            let sig_bytes = partial.signature.to_bytes();
            let mut sig_point = blst_p1_affine::default();

            unsafe {
                // Deserialize signature to affine point
                let result = blst_p1_deserialize(&mut sig_point, sig_bytes.as_ptr());
                if result != BLST_ERROR::BLST_SUCCESS {
                    return Err(anyhow::anyhow!(
                        "Failed to deserialize signature {}: {:?}",
                        i,
                        result
                    ));
                }

                // Check point is valid
                if !blst_p1_affine_in_g1(&sig_point) {
                    return Err(anyhow::anyhow!("Signature {}th point not in G1", i));
                }
            }

            // Scalar multiplication: sig_point * lambda
            let mut multiplied_point = blst_p1::default();
            let lambda_bytes = lambda.to_bytes_le();

            unsafe {
                // Convert affine point to projective point
                let mut proj_sig_point = blst_p1::default();
                blst_p1_from_affine(&mut proj_sig_point, &sig_point);

                // Perform scalar multiplication
                blst_p1_mult(
                    &mut multiplied_point,
                    &proj_sig_point,
                    lambda_bytes.as_ptr(),
                    255, // number of bits in scalar
                );
            }

            // Add to aggregated result
            if i == 0 {
                // First point - just copy
                aggregated_point = multiplied_point;
            } else {
                // Add to accumulator
                unsafe {
                    blst_p1_add_or_double(
                        &mut aggregated_point,
                        &aggregated_point,
                        &multiplied_point,
                    );
                }
            }
        }

        // Step 4: Convert aggregated point back to Signature
        // First convert to affine for serialization
        let mut aggregated_affine = blst_p1_affine::default();
        unsafe {
            blst_p1_to_affine(&mut aggregated_affine, &aggregated_point);
        }

        // Serialize the point
        let mut serialized = [0u8; 96]; // Uncompressed G1 point is 96 bytes
        unsafe {
            blst_p1_affine_serialize(serialized.as_mut_ptr(), &aggregated_affine);
        }

        // Create Signature from serialized bytes
        let final_signature = Signature::from_bytes(&serialized)
            .map_err(|e| anyhow::anyhow!("Failed to create final signature with error: {:?}", e))?;

        Ok(final_signature)
    }

    /// Verify a signature against a public key and message.
    ///
    /// # Arguments
    /// * `public_key` - BLS public key
    /// * `message` - Signed message
    /// * `signature` - Signature to verify
    ///
    /// # Returns
    /// Ok(()) if signature is valid, Error otherwise
    pub fn verify(public_key: &PublicKey, message: &[u8], signature: &Signature) -> Result<()> {
        let result = signature.verify(true, message, DST, &[], public_key, true);

        if result == BLST_ERROR::BLST_SUCCESS {
            Ok(())
        } else {
            Err(anyhow::anyhow!(
                "Signature verification failed: {:?}",
                result
            ))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use rand::thread_rng;

    #[test]
    fn test_scalar_arithmetic() {
        let a = Scalar::from_u64(5);
        let b = Scalar::from_u64(3);

        let sum = a.add(&b); // 5 + 3 = 8
        let diff = a.sub(&b); // 5 - 3 = 2
        let prod = a.mul(&b); // 5 * 3 = 15

        println!("Scalar arithmetic test passed");
        assert!(!sum.is_zero());
        assert!(!diff.is_zero());
        assert!(!prod.is_zero());
    }

    #[test]
    fn test_scalar_inverse() {
        let a = Scalar::from_u64(5);
        let a_inv = a.inverse().expect("Should have inverse");

        let product = a.mul(&a_inv);
        let one = Scalar::one();

        assert_eq!(product, one);
    }

    #[test]
    fn test_polynomial_evaluation() {
        // P(x) = 1 + 2x + 3x^2
        let coeffs = vec![
            Scalar::from_u64(1),
            Scalar::from_u64(2),
            Scalar::from_u64(3),
        ];

        let poly = Polynomial::new(coeffs);
        let x = Scalar::from_u64(5);
        let result = poly.evaluate(&x);

        // P(5) = 1 + 2*5 + 3*25 = 1 + 10 + 75 = 86
        println!("Polynomial evaluation test passed");
        assert!(!result.is_zero());
        assert_eq!(result, Scalar::from_u64(86));
    }

    #[test]
    fn test_shamir_secret_sharing() {
        let mut rng = thread_rng();
        let shamir = ShamirSharing::new(3, 5);

        let secret = Scalar::from_u64(42);
        let shares = shamir.split(secret.clone(), &mut rng);

        assert_eq!(shares.len(), 5);

        // Reconstruct from first 3 shares
        let reconstructed = shamir.reconstruct(&shares[..3]);

        println!("Shamir's Secret Sharing test passed");
        assert_eq!(reconstructed, secret);
    }

    #[test]
    fn test_threshold_signature() {
        let mut rng = thread_rng();
        let scheme = ThresholdBLS::new(3, 5);

        let (master_pk, key_shares) = scheme
            .trusted_setup(&mut rng)
            .expect("Key generation failed");

        let message = b"Test message";
        let partial_sigs: Vec<_> = key_shares
            .iter()
            .take(3)
            .map(|share| {
                ThresholdBLS::partial_sign(share, message).expect("Partial signing failed")
            })
            .collect();

        let signature = scheme.aggregate(&partial_sigs).expect("Aggregation failed");

        ThresholdBLS::verify(&master_pk, message, &signature).expect("Verification failed");

        println!("✓ Threshold signature test passed!");
    }
}
