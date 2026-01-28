//! Transaction builder with fluent API

use crate::error::Result;
use crate::transaction::types::SignedTransaction;
use crate::types::{Address, Hash};
use crate::wallet::Wallet;
use consensus::state::transaction::Transaction;

/// Builder for constructing transactions.
///
/// # Example
/// ```ignore
/// use hellas_sdk::{TxBuilder, Wallet, Address};
///
/// let wallet = Wallet::generate();
/// let recipient = Address::from_hex("...")?;
///
/// let tx = TxBuilder::transfer(recipient, 1000)
///     .with_fee(10)
///     .sign(&wallet, 0)?;
/// ```
pub struct TxBuilder {
    instruction: TxInstruction,
    fee: u64,
}

enum TxInstruction {
    Transfer { to: Address, amount: u64 },
    Mint { to: Address, amount: u64 },
    Burn { address: Address, amount: u64 },
    CreateAccount { address: Address },
}

impl TxBuilder {
    /// Create a transfer transaction.
    ///
    /// # Arguments
    /// * `to` - Recipient address
    /// * `amount` - Amount to transfer
    pub fn transfer(to: Address, amount: u64) -> Self {
        Self {
            instruction: TxInstruction::Transfer { to, amount },
            fee: 0,
        }
    }

    /// Create a mint transaction (testnet only).
    ///
    /// # Arguments
    /// * `to` - Recipient address
    /// * `amount` - Amount to mint
    pub fn mint(to: Address, amount: u64) -> Self {
        Self {
            instruction: TxInstruction::Mint { to, amount },
            fee: 0,
        }
    }

    /// Create a burn transaction.
    ///
    /// # Arguments
    /// * `address` - Address to burn from
    /// * `amount` - Amount to burn
    pub fn burn(address: Address, amount: u64) -> Self {
        Self {
            instruction: TxInstruction::Burn { address, amount },
            fee: 0,
        }
    }

    /// Create an account creation transaction.
    ///
    /// # Arguments
    /// * `address` - New account address
    pub fn create_account(address: Address) -> Self {
        Self {
            instruction: TxInstruction::CreateAccount { address },
            fee: 0,
        }
    }

    /// Set the transaction fee (default: 0).
    pub fn with_fee(mut self, fee: u64) -> Self {
        self.fee = fee;
        self
    }

    /// Sign the transaction with the given wallet and nonce.
    ///
    /// # Arguments
    /// * `wallet` - The wallet to sign with (becomes the sender)
    /// * `nonce` - The account nonce (fetch via `client.account().get_nonce()`)
    ///
    /// # Returns
    /// A signed transaction ready for submission.
    pub fn sign(self, wallet: &Wallet, nonce: u64) -> Result<SignedTransaction> {
        let sender = consensus::state::address::Address::from(*wallet.address());

        let tx = match self.instruction {
            TxInstruction::Transfer { to, amount } => Transaction::new_transfer(
                sender,
                to.into(),
                amount,
                nonce,
                self.fee,
                &consensus::crypto::transaction_crypto::TxSecretKey::from_bytes(
                    &wallet.to_secret_bytes(),
                ),
            ),
            TxInstruction::Mint { to, amount } => Transaction::new_mint(
                sender,
                to.into(),
                amount,
                nonce,
                &consensus::crypto::transaction_crypto::TxSecretKey::from_bytes(
                    &wallet.to_secret_bytes(),
                ),
            ),
            TxInstruction::Burn { address, amount } => Transaction::new_burn(
                sender,
                address.into(),
                amount,
                nonce,
                self.fee,
                &consensus::crypto::transaction_crypto::TxSecretKey::from_bytes(
                    &wallet.to_secret_bytes(),
                ),
            ),
            TxInstruction::CreateAccount { address } => Transaction::new_create_account(
                sender,
                address.into(),
                nonce,
                self.fee,
                &consensus::crypto::transaction_crypto::TxSecretKey::from_bytes(
                    &wallet.to_secret_bytes(),
                ),
            ),
        };

        // Serialize the transaction using rkyv
        let bytes = rkyv::to_bytes::<rkyv::rancor::Error>(&tx)
            .map_err(|e| crate::error::Error::Serialization(e.to_string()))?
            .to_vec();
        let tx_hash = Hash::from_bytes(tx.tx_hash);

        Ok(SignedTransaction { bytes, tx_hash })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn build_transfer() {
        let wallet = Wallet::generate();
        let recipient = Address::from_bytes([1u8; 32]);

        let tx = TxBuilder::transfer(recipient, 1000)
            .with_fee(10)
            .sign(&wallet, 0)
            .unwrap();

        assert!(!tx.bytes.is_empty());
        assert_ne!(tx.tx_hash.0, [0u8; 32]);
    }

    #[test]
    fn build_mint() {
        let wallet = Wallet::generate();
        let recipient = Address::from_bytes([2u8; 32]);

        let tx = TxBuilder::mint(recipient, 500).sign(&wallet, 1).unwrap();

        assert!(!tx.bytes.is_empty());
    }
}
