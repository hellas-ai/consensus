use crate::{
    crypto::transaction_crypto::{SerializableTxSignature, TxSecretKey, TxSignature},
    state::address::Address,
};
use rkyv::{Archive, Deserialize, Serialize, deserialize, rancor::Error};

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
        address: Address,
        nonce: u64,
        fee: u64,
        secret_key: &TxSecretKey,
    ) -> Self {
        let tx_type = TransactionInstruction::CreateAccount { address };
        Self::create_signed(address, tx_type, nonce, fee, secret_key)
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

    /// Computes the transaction from its bytes
    pub fn from_tx_bytes(bytes: &[u8]) -> Self {
        let tx_hash = blake3::hash(bytes);
        let archived = unsafe { rkyv::access_unchecked::<ArchivedTransaction>(bytes) };
        let mut tx = deserialize::<Transaction, Error>(archived).expect("Failed to deserialize");
        tx.tx_hash = tx_hash.into();
        tx
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
        let sk = TxSecretKey::generate();
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

        let a = Transaction::new_transfer(
            Address::from_public_key(&pk),
            Address::from_public_key(&pk),
            100,
            1,
            1,
            &sk,
        );

        let b = Transaction::new_transfer(
            Address::from_public_key(&pk),
            Address::from_public_key(&pk),
            100,
            1,
            1,
            &sk,
        );

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
}
