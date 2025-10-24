use rand::{CryptoRng, Rng};

use crate::scalar::Scalar;

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
