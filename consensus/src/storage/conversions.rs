use anyhow::Result;
use ark_serialize::CanonicalSerialize;
use rkyv::{
    Archive, Archived, api::high::to_bytes_with_alloc, ser::allocator::Arena, util::AlignedVec,
};

use crate::state::{
    account::Account, block::Block, leader::Leader, notarizations::MNotarization,
    nullify::Nullification, transaction::Transaction, view::View,
};

/// Accesses an archived value from a byte slice.
///
/// # Safety
///
/// The byte slice must represent a valid archived type when accessed at the
/// default root position. See the official rkyv documentation for more
/// https://docs.rs/rkyv/latest/rkyv/api/index.html.
pub unsafe fn access_archived<T: Archive>(bytes: &[u8]) -> &Archived<T> {
    unsafe { rkyv::access_unchecked::<Archived<T>>(bytes) }
}

/// Serializes a value for storage in the redb database using the rkyv library.
///
/// The value is serialized as an [`AlignedVec`] for storage in the redb database.
///
/// # Errors
///
/// Returns an error if the serialization fails.
///
/// # Example
///
/// ```rust,ignore
/// let value = Block { view: 1, parent_hash: "0x1234567890".to_string(), transactions: vec![] };
/// let bytes = serialize_for_db(&value).expect("Serialization failed");
/// ```
pub fn serialize_for_db<T>(value: &T) -> Result<AlignedVec>
where
    T: for<'a> rkyv::Serialize<
            rkyv::api::high::HighSerializer<
                AlignedVec,
                rkyv::ser::allocator::ArenaHandle<'a>,
                rkyv::rancor::Error,
            >,
        >,
{
    let mut arena = Arena::new();
    to_bytes_with_alloc::<_, rkyv::rancor::Error>(value, arena.acquire())
        .map_err(|e| anyhow::anyhow!("Serialization failed: {:?}", e))
}

/// Trait for types that can be stored in the redb database.
pub trait Storable {
    type Key: AsRef<[u8]>;
    type Value: AsRef<[u8]>;

    /// Returns the key for the value
    fn key(&self) -> Self::Key;

    /// Returns the value for the key
    fn value(&self) -> Result<Self::Value>;
}

impl Storable for Block {
    type Key = [u8; blake3::OUT_LEN];
    type Value = AlignedVec;

    fn key(&self) -> Self::Key {
        self.get_hash()
    }

    fn value(&self) -> Result<Self::Value> {
        serialize_for_db(self)
    }
}

impl Storable for Leader {
    type Key = [u8; 8];
    type Value = AlignedVec;

    fn key(&self) -> Self::Key {
        self.view.to_le_bytes()
    }

    fn value(&self) -> Result<Self::Value> {
        serialize_for_db(self)
    }
}

impl Storable for View {
    type Key = [u8; 8];
    type Value = AlignedVec;

    fn key(&self) -> Self::Key {
        self.view.to_le_bytes()
    }

    fn value(&self) -> Result<Self::Value> {
        serialize_for_db(self)
    }
}

impl Storable for Transaction {
    type Key = [u8; blake3::OUT_LEN];
    type Value = AlignedVec;

    fn key(&self) -> Self::Key {
        self.tx_hash
    }

    fn value(&self) -> Result<Self::Value> {
        serialize_for_db(self)
    }
}

impl<const N: usize, const F: usize, const M_SIZE: usize> Storable for MNotarization<N, F, M_SIZE> {
    type Key = [u8; blake3::OUT_LEN];
    type Value = AlignedVec;

    fn key(&self) -> Self::Key {
        self.block_hash
    }

    fn value(&self) -> Result<Self::Value> {
        serialize_for_db(self)
    }
}

impl<const N: usize, const F: usize, const L_SIZE: usize> Storable for Nullification<N, F, L_SIZE> {
    type Key = [u8; 8];
    type Value = AlignedVec;

    fn key(&self) -> Self::Key {
        self.view.to_le_bytes()
    }

    fn value(&self) -> Result<Self::Value> {
        serialize_for_db(self)
    }
}

impl Storable for Account {
    type Key = Vec<u8>;
    type Value = AlignedVec;

    fn key(&self) -> Self::Key {
        let mut writer = Vec::new();
        self.public_key.serialize_compressed(&mut writer).unwrap();
        writer
    }

    fn value(&self) -> Result<Self::Value> {
        serialize_for_db(self)
    }
}
