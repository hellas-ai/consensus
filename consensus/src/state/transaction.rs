use crate::{
    crypto::transaction_crypto::{SerializableTxSignature, TxSecretKey, TxSignature},
    state::address::Address,
};
use rkyv::{Archive, Deserialize, Serialize, deserialize, rancor::Error};

/// Error type for transaction deserialization.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TransactionError {
    /// Transaction bytes were empty.
    EmptyBytes,
    /// Transaction bytes have invalid archive format.
    InvalidFormat,
    /// Failed to deserialize the transaction.
    DeserializationFailed,
}

impl std::fmt::Display for TransactionError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            TransactionError::EmptyBytes => write!(f, "Transaction bytes cannot be empty"),
            TransactionError::InvalidFormat => write!(f, "Invalid transaction archive format"),
            TransactionError::DeserializationFailed => {
                write!(f, "Failed to deserialize transaction")
            }
        }
    }
}

impl std::error::Error for TransactionError {}

/// Transaction instruction types
#[derive(Clone, Debug, Archive, Deserialize, Serialize)]
pub enum TransactionInstruction {
    /// Create a new account
    CreateAccount { address: Address },
    /// Burn tokens
    Burn { address: Address, amount: u64 },
    /// Mint new tokens to an address (permission-less for testnet)
    /// In production, this would require either initial token
    /// allocation or staking/minting tokens
    Mint { recipient: Address, amount: u64 },
    /// Transfer tokens from sender to recipient
    Transfer { recipient: Address, amount: u64 },
}

/// [`Transaction`] represents a transaction in the consensus protocol.
///
/// A transaction is a message that is sent between two parties.
/// In an initial version, it contains the sender, recipient, amount,
/// nonce, timestamp, and fee. In the future, we will expand its scope.
/// The signature is used to verify the transaction.
#[derive(Clone, Debug, Archive, Deserialize, Serialize)]
pub struct Transaction {
    /// The sender's address
    pub sender: Address,
    /// The instruction of the transaction
    pub instruction: TransactionInstruction,
    /// The nonce of the transaction. This value is
    /// incremental and used to prevent replay attacks.
    pub nonce: u64,
    /// The timestamp of the transaction, as measured by the
    /// peer proposing such transaction.
    pub timestamp: u64,
    /// The fee of the transaction. This value is used to
    /// prioritize transactions in the mempool.
    pub fee: u64,
    /// The hash of the transaction's body content
    pub tx_hash: [u8; blake3::OUT_LEN],
    /// The sender's signature of the transaction's body content
    pub signature: SerializableTxSignature,
}

impl Transaction {
    /// Creates a new transfer transaction
    pub fn new_transfer(
        sender: Address,
        recipient: Address,
        amount: u64,
        nonce: u64,
        fee: u64,
        secret_key: &TxSecretKey,
    ) -> Self {
        let tx_type = TransactionInstruction::Transfer { recipient, amount };
        Self::create_signed(sender, tx_type, nonce, fee, secret_key)
    }

    /// Creates a new mint transaction (testnet only)
    pub fn new_mint(
        sender: Address,
        recipient: Address,
        amount: u64,
        nonce: u64,
        secret_key: &TxSecretKey,
    ) -> Self {
        let tx_type = TransactionInstruction::Mint { recipient, amount };
        // Mint is free (no fee) for testnet
        Self::create_signed(sender, tx_type, nonce, 0, secret_key)
    }

    /// Creates a new create account transaction
    pub fn new_create_account(
        sender: Address,
        new_address: Address,
        nonce: u64,
        fee: u64,
        secret_key: &TxSecretKey,
    ) -> Self {
        let tx_type = TransactionInstruction::CreateAccount {
            address: new_address,
        };
        Self::create_signed(sender, tx_type, nonce, fee, secret_key)
    }

    /// Creates a new burn transaction
    pub fn new_burn(
        sender: Address,
        address: Address,
        amount: u64,
        nonce: u64,
        fee: u64,
        secret_key: &TxSecretKey,
    ) -> Self {
        let tx_type = TransactionInstruction::Burn { address, amount };
        Self::create_signed(sender, tx_type, nonce, fee, secret_key)
    }

    fn create_signed(
        sender: Address,
        instruction: TransactionInstruction,
        nonce: u64,
        fee: u64,
        secret_key: &TxSecretKey,
    ) -> Self {
        let timestamp = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_millis() as u64;

        // Compute hash of transaction content
        let tx_hash = Self::compute_hash(&sender, &instruction, nonce, timestamp, fee);
        let signature = SerializableTxSignature::from(&secret_key.sign(&tx_hash));

        Self {
            sender,
            instruction,
            nonce,
            timestamp,
            fee,
            tx_hash,
            signature,
        }
    }

    /// Computes the transaction from its bytes.
    ///
    /// # Safety
    /// This method uses unchecked deserialization for performance.
    /// Only use with trusted input (e.g., from storage or P2P after validation).
    /// For untrusted external input, use [`try_from_tx_bytes`] instead.
    ///
    /// # Panics
    /// Panics if the bytes cannot be deserialized into a valid Transaction.
    pub fn from_tx_bytes(bytes: &[u8]) -> Self {
        let tx_hash = blake3::hash(bytes);
        let archived = unsafe { rkyv::access_unchecked::<ArchivedTransaction>(bytes) };
        let mut tx = deserialize::<Transaction, Error>(archived).expect("Failed to deserialize");
        tx.tx_hash = tx_hash.into();
        tx
    }

    /// Safely deserialize a transaction from bytes.
    ///
    /// This method validates the archive format before deserializing,
    /// making it safe to use with untrusted external input (e.g., from gRPC clients).
    ///
    /// # Errors
    /// Returns an error if the bytes are malformed or cannot be deserialized.
    pub fn try_from_tx_bytes(bytes: &[u8]) -> Result<Self, TransactionError> {
        if bytes.is_empty() {
            return Err(TransactionError::EmptyBytes);
        }

        let tx_hash = blake3::hash(bytes);

        // Use safe access with validation
        let archived = rkyv::access::<ArchivedTransaction, Error>(bytes)
            .map_err(|_| TransactionError::InvalidFormat)?;

        let mut tx = deserialize::<Transaction, Error>(archived)
            .map_err(|_| TransactionError::DeserializationFailed)?;

        tx.tx_hash = tx_hash.into();
        Ok(tx)
    }

    /// Verifies the transaction signature
    pub fn verify(&self) -> bool {
        let sender_pk = self
            .sender
            .to_public_key()
            .expect("Failed to convert address to public key");
        let signature = TxSignature::try_from(&self.signature)
            .expect("Failed to convert signature to TxSignature");

        // Verify signature
        sender_pk.verify(&self.tx_hash, &signature)
    }

    /// Returns the amount being transferred/minted
    pub fn amount(&self) -> u64 {
        match &self.instruction {
            TransactionInstruction::Transfer { amount, .. } => *amount,
            TransactionInstruction::Burn { amount, .. } => *amount,
            TransactionInstruction::Mint { amount, .. } => *amount,
            TransactionInstruction::CreateAccount { .. } => 0,
        }
    }

    /// Returns the recipient address
    pub fn recipient(&self) -> Option<Address> {
        match &self.instruction {
            TransactionInstruction::Transfer { recipient, .. } => Some(*recipient),
            TransactionInstruction::Burn { address, .. } => Some(*address),
            TransactionInstruction::Mint { recipient, .. } => Some(*recipient),
            TransactionInstruction::CreateAccount { address, .. } => Some(*address),
        }
    }
}

impl Transaction {
    /// Discriminants for the transaction instructions
    const CREATE_ACCOUNT_DISCRIMINANT: u8 = 1;

    /// Discriminant for the burn instruction
    const BURN_DISCRIMINANT: u8 = 2;

    /// Discriminant for the mint instruction
    const MINT_DISCRIMINANT: u8 = 3;

    /// Discriminant for the transfer instruction
    const TRANSFER_DISCRIMINANT: u8 = 4;

    fn compute_hash(
        sender_address: &Address,
        tx_type: &TransactionInstruction,
        nonce: u64,
        timestamp: u64,
        fee: u64,
    ) -> [u8; blake3::OUT_LEN] {
        let mut hasher = blake3::Hasher::new();
        hasher.update(sender_address.as_bytes());

        // Serialize tx_type for hashing
        match tx_type {
            TransactionInstruction::CreateAccount { address } => {
                hasher.update(&[Self::CREATE_ACCOUNT_DISCRIMINANT]);
                hasher.update(address.as_bytes());
            }
            TransactionInstruction::Burn { address, amount } => {
                hasher.update(&[Self::BURN_DISCRIMINANT]);
                hasher.update(address.as_bytes());
                hasher.update(&amount.to_le_bytes());
            }
            TransactionInstruction::Mint { recipient, amount } => {
                hasher.update(&[Self::MINT_DISCRIMINANT]);
                hasher.update(recipient.as_bytes());
                hasher.update(&amount.to_le_bytes());
            }
            TransactionInstruction::Transfer { recipient, amount } => {
                hasher.update(&[Self::TRANSFER_DISCRIMINANT]);
                hasher.update(recipient.as_bytes());
                hasher.update(&amount.to_le_bytes());
            }
        }

        hasher.update(&nonce.to_le_bytes());
        hasher.update(&timestamp.to_le_bytes());
        hasher.update(&fee.to_le_bytes());

        *hasher.finalize().as_bytes()
    }
}

impl PartialEq for Transaction {
    fn eq(&self, other: &Self) -> bool {
        self.tx_hash == other.tx_hash
    }
}

impl Eq for Transaction {}

#[cfg(test)]
mod tests {
    use super::*;

    use crate::crypto::transaction_crypto::TxPublicKey;
    use crate::storage::conversions::serialize_for_db;

    fn gen_keypair() -> (TxSecretKey, TxPublicKey) {
        let sk = TxSecretKey::generate(&mut rand::rngs::OsRng);
        let pk = sk.public_key();
        (sk, pk)
    }

    #[test]
    fn verify_true_with_matching_signature() {
        let (sk, pk) = gen_keypair();

        let tx = Transaction::new_transfer(
            Address::from_public_key(&pk),
            Address::from_bytes([2u8; 32]),
            10,
            1,
            123,
            &sk,
        );
        assert!(tx.verify());
    }

    #[test]
    fn verify_false_when_signature_or_hash_mismatch() {
        let (sk, _pk) = gen_keypair();

        let tx_bad_sig = Transaction::new_transfer(
            Address::from_bytes([1u8; 32]),
            Address::from_bytes([2u8; 32]),
            5,
            1,
            1,
            &sk,
        );
        assert!(!tx_bad_sig.verify());
    }

    #[test]
    fn equality_is_by_tx_hash_only() {
        let (sk, pk) = gen_keypair();
        let fixed_timestamp = 1000000u64;

        let a = Transaction::new_transfer(
            Address::from_public_key(&pk),
            Address::from_public_key(&pk),
            100,
            1,
            1,
            &sk,
        );

        let mut b = a.clone();

        b.timestamp = fixed_timestamp;
        b.fee = 2;
        b.nonce = 0;
        b.instruction = TransactionInstruction::Mint {
            recipient: Address::from_public_key(&pk),
            amount: 5_000,
        };

        assert_eq!(a, b);
    }

    #[test]
    fn from_tx_bytes_sets_hash_to_bytes_digest() {
        let (sk, pk) = gen_keypair();

        let tx = Transaction::new_transfer(
            Address::from_public_key(&pk),
            Address::from_bytes([2u8; 32]),
            1,
            2,
            3,
            &sk,
        );
        let bytes = serialize_for_db(&tx).expect("serialize");
        let restored = Transaction::from_tx_bytes(bytes.as_slice());

        // tx_hash is set to blake3(bytes), not the original content hash
        let expected: [u8; blake3::OUT_LEN] = blake3::hash(bytes.as_slice()).into();
        assert_eq!(restored.tx_hash, expected);

        // Other fields round-trip via rkyv deserialize
        assert_eq!(restored.recipient(), tx.recipient());
        assert_eq!(restored.amount(), tx.amount());
        assert_eq!(restored.nonce, tx.nonce);
        assert_eq!(restored.timestamp, tx.timestamp);
        assert_eq!(restored.fee, tx.fee);

        // Signature was created over the original tx_hash, so verify is expected to be false now
        assert!(!restored.verify());
    }

    #[test]
    fn new_mint_creates_valid_transaction() {
        let (sk, pk) = gen_keypair();
        let sender = Address::from_public_key(&pk);
        let recipient = Address::from_bytes([5u8; 32]);

        let tx = Transaction::new_mint(sender, recipient, 1000, 0, &sk);

        assert!(tx.verify());
        assert_eq!(tx.amount(), 1000);
        assert_eq!(tx.recipient(), Some(recipient));
        assert_eq!(tx.nonce, 0);
        assert_eq!(tx.fee, 0); // Mint is free for testnet
        assert_eq!(tx.sender, sender);
    }

    #[test]
    fn new_create_account_creates_valid_transaction() {
        let (sk, pk) = gen_keypair();
        let address = Address::from_public_key(&pk);
        let new_address = Address::from_bytes([1u8; 32]);

        let tx = Transaction::new_create_account(address, new_address, 0, 100, &sk);

        assert!(tx.verify());
        assert_eq!(tx.amount(), 0); // CreateAccount has no amount
        assert_eq!(tx.recipient(), Some(new_address));
        assert_eq!(tx.nonce, 0);
        assert_eq!(tx.fee, 100);
    }

    #[test]
    fn transfer_instruction_fields() {
        let (sk, pk) = gen_keypair();
        let sender = Address::from_public_key(&pk);
        let recipient = Address::from_bytes([9u8; 32]);

        let tx = Transaction::new_transfer(sender, recipient, 500, 42, 10, &sk);

        assert_eq!(tx.amount(), 500);
        assert_eq!(tx.recipient(), Some(recipient));
        assert_eq!(tx.nonce, 42);
        assert_eq!(tx.fee, 10);
        assert_eq!(tx.sender, sender);

        match &tx.instruction {
            TransactionInstruction::Transfer {
                recipient: r,
                amount: a,
            } => {
                assert_eq!(*r, recipient);
                assert_eq!(*a, 500);
            }
            _ => panic!("Expected Transfer instruction"),
        }
    }

    #[test]
    fn mint_instruction_fields() {
        let (sk, pk) = gen_keypair();
        let sender = Address::from_public_key(&pk);
        let recipient = Address::from_bytes([7u8; 32]);

        let tx = Transaction::new_mint(sender, recipient, 9999, 5, &sk);

        match &tx.instruction {
            TransactionInstruction::Mint {
                recipient: r,
                amount: a,
            } => {
                assert_eq!(*r, recipient);
                assert_eq!(*a, 9999);
            }
            _ => panic!("Expected Mint instruction"),
        }
    }

    #[test]
    fn create_account_instruction_fields() {
        let (sk, pk) = gen_keypair();
        let sender_address = Address::from_public_key(&pk);
        let new_address = Address::from_bytes([1u8; 32]);

        let tx = Transaction::new_create_account(sender_address, new_address, 1, 50, &sk);

        match &tx.instruction {
            TransactionInstruction::CreateAccount { address: a } => {
                assert_eq!(*a, new_address);
            }
            _ => panic!("Expected CreateAccount instruction"),
        }
    }

    #[test]
    fn different_instructions_produce_different_hashes() {
        let (sk, pk) = gen_keypair();
        let sender = Address::from_public_key(&pk);
        let recipient = Address::from_bytes([3u8; 32]);

        // Create transactions with same parameters but different instructions
        let transfer = Transaction::new_transfer(sender, recipient, 100, 0, 10, &sk);
        let mint = Transaction::new_mint(sender, recipient, 100, 0, &sk);

        // They should have different hashes (different instruction discriminant)
        assert_ne!(transfer.tx_hash, mint.tx_hash);
    }

    #[test]
    fn same_content_produces_same_hash() {
        let (_sk, pk) = gen_keypair();
        let sender = Address::from_public_key(&pk);
        let recipient = Address::from_bytes([4u8; 32]);

        // Compute hash manually
        let instruction = TransactionInstruction::Transfer {
            recipient,
            amount: 100,
        };
        let nonce = 5u64;
        let timestamp = 123456789u64;
        let fee = 10u64;

        let hash1 = Transaction::compute_hash(&sender, &instruction, nonce, timestamp, fee);
        let hash2 = Transaction::compute_hash(&sender, &instruction, nonce, timestamp, fee);

        assert_eq!(hash1, hash2);
    }

    #[test]
    fn different_nonce_produces_different_hash() {
        let sender = Address::from_bytes([1u8; 32]);
        let recipient = Address::from_bytes([2u8; 32]);
        let instruction = TransactionInstruction::Transfer {
            recipient,
            amount: 100,
        };

        let hash1 = Transaction::compute_hash(&sender, &instruction, 0, 1000, 10);
        let hash2 = Transaction::compute_hash(&sender, &instruction, 1, 1000, 10);

        assert_ne!(hash1, hash2);
    }

    #[test]
    fn different_fee_produces_different_hash() {
        let sender = Address::from_bytes([1u8; 32]);
        let recipient = Address::from_bytes([2u8; 32]);
        let instruction = TransactionInstruction::Transfer {
            recipient,
            amount: 100,
        };

        let hash1 = Transaction::compute_hash(&sender, &instruction, 0, 1000, 10);
        let hash2 = Transaction::compute_hash(&sender, &instruction, 0, 1000, 20);

        assert_ne!(hash1, hash2);
    }

    #[test]
    fn different_amount_produces_different_hash() {
        let sender = Address::from_bytes([1u8; 32]);
        let recipient = Address::from_bytes([2u8; 32]);

        let instruction1 = TransactionInstruction::Transfer {
            recipient,
            amount: 100,
        };
        let instruction2 = TransactionInstruction::Transfer {
            recipient,
            amount: 200,
        };

        let hash1 = Transaction::compute_hash(&sender, &instruction1, 0, 1000, 10);
        let hash2 = Transaction::compute_hash(&sender, &instruction2, 0, 1000, 10);

        assert_ne!(hash1, hash2);
    }

    #[test]
    fn different_recipient_produces_different_hash() {
        let sender = Address::from_bytes([1u8; 32]);
        let recipient1 = Address::from_bytes([2u8; 32]);
        let recipient2 = Address::from_bytes([3u8; 32]);

        let instruction1 = TransactionInstruction::Transfer {
            recipient: recipient1,
            amount: 100,
        };
        let instruction2 = TransactionInstruction::Transfer {
            recipient: recipient2,
            amount: 100,
        };

        let hash1 = Transaction::compute_hash(&sender, &instruction1, 0, 1000, 10);
        let hash2 = Transaction::compute_hash(&sender, &instruction2, 0, 1000, 10);

        assert_ne!(hash1, hash2);
    }

    #[test]
    fn verify_fails_with_wrong_sender_address() {
        let (sk, _pk) = gen_keypair();
        let (_sk2, pk2) = gen_keypair();

        // Create transaction with pk2's address but sign with sk (pk's key)
        // This simulates someone trying to spend from another account
        let wrong_sender = Address::from_public_key(&pk2);
        let recipient = Address::from_bytes([5u8; 32]);

        let tx = Transaction::new_transfer(wrong_sender, recipient, 100, 0, 10, &sk);

        // Verification should fail because sender address doesn't match signing key
        assert!(!tx.verify());
    }

    #[test]
    fn tx_hash_is_32_bytes() {
        let (sk, pk) = gen_keypair();
        let sender = Address::from_public_key(&pk);
        let recipient = Address::from_bytes([1u8; 32]);

        let tx = Transaction::new_transfer(sender, recipient, 100, 0, 10, &sk);

        assert_eq!(tx.tx_hash.len(), 32);
    }

    #[test]
    fn zero_amount_transfer_is_valid() {
        let (sk, pk) = gen_keypair();
        let sender = Address::from_public_key(&pk);
        let recipient = Address::from_bytes([1u8; 32]);

        let tx = Transaction::new_transfer(sender, recipient, 0, 0, 10, &sk);

        assert!(tx.verify());
        assert_eq!(tx.amount(), 0);
    }

    #[test]
    fn max_amount_transfer_is_valid() {
        let (sk, pk) = gen_keypair();
        let sender = Address::from_public_key(&pk);
        let recipient = Address::from_bytes([1u8; 32]);

        let tx = Transaction::new_transfer(sender, recipient, u64::MAX, 0, 10, &sk);

        assert!(tx.verify());
        assert_eq!(tx.amount(), u64::MAX);
    }

    #[test]
    fn self_transfer_is_valid() {
        let (sk, pk) = gen_keypair();
        let address = Address::from_public_key(&pk);

        // Transfer to self
        let tx = Transaction::new_transfer(address, address, 100, 0, 10, &sk);

        assert!(tx.verify());
        assert_eq!(tx.recipient(), Some(address));
        assert_eq!(tx.sender, address);
    }
}
