use ark_serialize::{CanonicalDeserialize, CanonicalSerialize};
use rkyv::{
    Archive, Deserialize, Place, Serialize,
    rancor::{self, Fallible},
    ser::{Allocator, Writer},
};

// ============================================================================
// Ark <-> rkyv Bridge
// ============================================================================

/// Bridge type for serializing ark-serialize types with rkyv
/// This converts ark types to/from byte vectors for rkyv serialization
pub struct ArkSerdeWrapper;

impl<T> rkyv::with::ArchiveWith<T> for ArkSerdeWrapper
where
    T: CanonicalSerialize + CanonicalDeserialize,
{
    type Archived = rkyv::Archived<Vec<u8>>;
    type Resolver = rkyv::Resolver<Vec<u8>>;

    fn resolve_with(field: &T, resolver: Self::Resolver, out: Place<Self::Archived>) {
        // Serialize the ark type to bytes
        let mut bytes = Vec::new();
        field.serialize_compressed(&mut bytes).unwrap();
        bytes.resolve(resolver, out);
    }
}

impl<T, S> rkyv::with::SerializeWith<T, S> for ArkSerdeWrapper
where
    T: CanonicalSerialize + CanonicalDeserialize,
    S: Allocator + Fallible + Writer + ?Sized,
{
    fn serialize_with(field: &T, serializer: &mut S) -> Result<Self::Resolver, S::Error> {
        // Serialize the ark type to bytes
        let mut bytes = Vec::new();
        field.serialize_compressed(&mut bytes).unwrap();
        bytes.serialize(serializer)
    }
}

impl<T, D> rkyv::with::DeserializeWith<rkyv::Archived<Vec<u8>>, T, D> for ArkSerdeWrapper
where
    T: CanonicalDeserialize + CanonicalSerialize,
    D: Fallible + ?Sized,
    D::Error: rancor::Source,
{
    fn deserialize_with(
        field: &rkyv::Archived<Vec<u8>>,
        deserializer: &mut D,
    ) -> Result<T, D::Error> {
        // Deserialize the bytes back to the ark type
        let bytes: Vec<u8> = field.deserialize(deserializer)?;
        Ok(T::deserialize_compressed(&bytes[..]).expect("Failed to deserialize ark type"))
    }
}
