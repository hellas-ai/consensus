use anyhow::Result;
use blst::{
    BLST_ERROR, blst_hash_to_g1, blst_lendian_from_scalar, blst_p1, blst_p1_add_or_double,
    blst_p1_affine, blst_p1_affine_compress, blst_p1_affine_in_g1, blst_p1_affine_serialize,
    blst_p1_deserialize, blst_p1_from_affine, blst_p1_mult, blst_p1_to_affine, blst_p2,
    blst_p2_affine, blst_p2_affine_compress, blst_p2_generator, blst_p2_mult, blst_p2_to_affine,
    blst_scalar, blst_scalar_from_le_bytes, blst_sk_add_n_check, blst_sk_inverse,
    blst_sk_mul_n_check, blst_sk_sub_n_check, min_sig::*,
};
use rand::{CryptoRng, Rng};

/// The domain separation tag for BLS signatures.
const DST: &[u8] = b"BLS_SIG_BLS12381G1_XMD:SHA-256_SSWU_RO_NUL_";

/// A scalar value in the BLS12-381 scalar field Fr.
///
/// Scalars are elements of the finite field Fr with order r, where r is a 255-bit prime.
/// They are used throughout BLS cryptography for:
/// - Private keys (secret scalars)
/// - Random values in signature generation
/// - Lagrange coefficients in threshold signature schemes
/// - Shares in Shamir's secret sharing
///
/// All arithmetic operations are performed modulo r, the order of the BLS12-381 curve.
/// The field r is: 0x73eda753299d7d483339d80809a1d80553bda402fffe5bfeffffffff00000001
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Scalar {
    inner: blst_scalar,
}

impl Scalar {
    /// Creates a scalar from a 32-byte little-endian representation.
    ///
    /// # Arguments
    /// * `bytes` - Exactly 32 bytes in little-endian order
    ///
    /// # Returns
    /// A scalar value reduced modulo r
    ///
    /// # Note
    /// Values larger than r are automatically reduced modulo r.
    pub fn from_bytes_le(bytes: [u8; 32]) -> Self {
        let mut blst_scalar = blst_scalar::default();
        unsafe {
            blst_scalar_from_le_bytes(&mut blst_scalar, bytes.as_ptr(), 32);
        };
        Self { inner: blst_scalar }
    }

    /// Creates a scalar from a u64 value.
    pub fn from_u64(value: u64) -> Self {
        let mut bytes = [0; 32];
        bytes[..8].copy_from_slice(&value.to_le_bytes());
        Self::from_bytes_le(bytes)
    }

    /// Generates a random scalar using a cryptographically secure RNG.
    ///
    /// The generated scalar is uniformly distributed in the range [0, r-1].
    ///
    /// # Arguments
    /// * `rng` - A cryptographically secure random number generator
    pub fn random<R: CryptoRng + Rng>(rng: &mut R) -> Self {
        let mut bytes = [0; 32];
        rng.fill_bytes(&mut bytes);
        Self::from_bytes_le(bytes)
    }

    /// Converts the scalar to its 32-byte little-endian representation.
    ///
    /// # Returns
    /// Exactly 32 bytes representing the scalar in little-endian order
    pub fn to_bytes_le(&self) -> [u8; 32] {
        let mut bytes = [0; 32];
        unsafe {
            blst_lendian_from_scalar(bytes.as_mut_ptr(), &self.inner);
        };
        bytes
    }

    /// Returns the additive identity element (zero) in the scalar field.
    pub fn zero() -> Self {
        Self {
            inner: blst_scalar::default(),
        }
    }

    /// Returns the multiplicative identity element (one) in the scalar field.
    pub fn one() -> Self {
        Self::from_u64(1)
    }

    /// Checks whether this scalar is the additive identity (zero).
    ///
    /// # Returns
    /// `true` if the scalar equals zero, `false` otherwise
    pub fn is_zero(&self) -> bool {
        let bytes = self.to_bytes_le();
        bytes.iter().all(|&b| b == 0)
    }

    /// Performs modular addition: `(self + other) mod r`.
    ///
    /// # Arguments
    /// * `other` - The scalar to add to `self`
    ///
    /// # Returns
    /// A new scalar containing the sum
    pub fn add(&self, other: &Scalar) -> Scalar {
        let mut result = blst_scalar::default();
        unsafe {
            blst_sk_add_n_check(&mut result, &self.inner, &other.inner);
        }
        Scalar { inner: result }
    }

    /// Performs modular subtraction: `(self - other) mod r`.
    ///
    /// # Arguments
    /// * `other` - The scalar to subtract from `self`
    ///
    /// # Returns
    /// A new scalar containing the difference
    pub fn sub(&self, other: &Scalar) -> Scalar {
        let mut result = blst_scalar::default();
        unsafe {
            blst_sk_sub_n_check(&mut result, &self.inner, &other.inner);
        }
        Scalar { inner: result }
    }

    /// Performs modular multiplication: `(self * other) mod r`.
    ///
    /// # Arguments
    /// * `other` - The scalar to multiply with `self`
    ///
    /// # Returns
    /// A new scalar containing the product
    pub fn mul(&self, other: &Scalar) -> Scalar {
        let mut result = blst_scalar::default();
        unsafe {
            blst_sk_mul_n_check(&mut result, &self.inner, &other.inner);
        }
        Scalar { inner: result }
    }

    /// Computes the modular inverse: `self^(-1) mod r`.
    ///
    /// The inverse exists for all non-zero scalars since r is prime.
    ///
    /// # Returns
    /// - `Some(inverse)` if the scalar is non-zero
    /// - `None` if the scalar is zero (undefined inverse)
    pub fn inverse(&self) -> Option<Scalar> {
        if self.is_zero() {
            return None;
        }
        let mut result = blst_scalar::default();
        unsafe {
            blst_sk_inverse(&mut result, &self.inner);
        };
        Some(Scalar { inner: result })
    }

    /// Performs modular division: `(self * other^(-1)) mod r`.
    ///
    /// This is equivalent to `self * other.inverse()` when the inverse exists.
    ///
    /// # Arguments
    /// * `other` - The scalar to divide by
    ///
    /// # Returns
    /// - `Some(result)` if `other` is non-zero
    /// - `None` if `other` is zero (division by zero)
    pub fn div(&self, other: &Scalar) -> Option<Scalar> {
        if other.is_zero() {
            return None;
        }
        other.inverse().map(|inverse| self.mul(&inverse))
    }

    /// Returns a reference to the underlying `blst_scalar`.
    ///
    /// This method provides low-level access to the BLST library representation.
    /// Use with caution - direct manipulation may violate invariants.
    pub fn as_blst_scalar(&self) -> &blst_scalar {
        &self.inner
    }
}

/// Polynomial over scalar field Fr
pub struct Polynomial {
    coefficients: Vec<Scalar>,
}

impl Polynomial {
    /// Create polynomial with given coefficients
    /// coefficients[0] is constant term
    pub fn new(coefficients: Vec<Scalar>) -> Self {
        Polynomial { coefficients }
    }

    /// Generate random polynomial of given degree with specified constant term
    pub fn random<R: Rng + CryptoRng>(degree: usize, constant_term: Scalar, rng: &mut R) -> Self {
        let mut coefficients = vec![constant_term];
        for _ in 0..degree {
            coefficients.push(Scalar::random(rng));
        }
        Polynomial::new(coefficients)
    }

    /// Evaluate polynomial at point x using Horner's method
    /// P(x) = a_0 + a_1*x + a_2*x^2 + ... + a_n*x^n
    pub fn evaluate(&self, x: &Scalar) -> Scalar {
        if self.coefficients.is_empty() {
            return Scalar::zero();
        }

        // Horner's method: work backwards from highest degree
        let mut result = self.coefficients.last().unwrap().clone();

        for coeff in self.coefficients.iter().rev().skip(1) {
            result = result.mul(x).add(coeff);
        }

        result
    }

    /// Get the constant term (secret)
    pub fn constant_term(&self) -> &Scalar {
        &self.coefficients[0]
    }

    /// Get polynomial degree
    pub fn degree(&self) -> usize {
        self.coefficients.len().saturating_sub(1)
    }
}

/// Share of a secret in Shamir's secret sharing scheme
#[derive(Clone, Debug)]
pub struct Share {
    /// The x-coordinate of the share
    pub x: Scalar,
    /// The y-coordinate of the share
    pub y: Scalar,
}

/// Shamir's secret sharing scheme
pub struct ShamirSharing {
    /// Threshold for reconstruction
    threshold: usize,
    /// Total number of shares
    total_shares: usize,
}

impl ShamirSharing {
    pub fn new(threshold: usize, total_shares: usize) -> Self {
        debug_assert!(threshold > 0 && threshold <= total_shares);
        Self {
            threshold,
            total_shares,
        }
    }

    /// Split a secret into shares
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

    /// Compute Lagrange coefficient λ_i for reconstruction at x=0
    /// λ_i = ∏(j≠i) x_j / (x_j - x_i)
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

    /// Reconstruct secret from threshold shares using Lagrange interpolation
    /// secret = ∑ y_i * λ_i
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

/// Key share for a peer in the consensus protocol
#[derive(Clone)]
pub struct KeyShare {
    pub id: u64,
    pub secret_scalar: Scalar,
    pub public_key: PublicKey,
}

/// Partial signature from a participant
#[derive(Clone)]
pub struct PartialSignature {
    pub id: u64,
    pub signature: Signature,
}

/// BLS Threshold Signature Scheme
pub struct ThresholdBLS {
    threshold: usize,
    total_participants: usize,
    shamir: ShamirSharing,
}

impl ThresholdBLS {
    pub fn new(threshold: usize, total_participants: usize) -> Self {
        Self {
            threshold,
            total_participants,
            shamir: ShamirSharing::new(threshold, total_participants),
        }
    }

    /// Convert scalar to public key (PK = scalar * G2_generator)
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

    /// Trusted setup for the BLS threshold signature scheme
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

    /// Verify a signature
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
