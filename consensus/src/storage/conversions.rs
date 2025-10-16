use anyhow::Result;
use rkyv::{
    Archive, Archived, api::high::to_bytes_with_alloc, ser::allocator::Arena, util::AlignedVec,
};

// Helper functions for working with archived data
pub fn access_archived<T: Archive>(bytes: &[u8]) -> &Archived<T> {
    unsafe { rkyv::access_unchecked::<Archived<T>>(bytes) }
}

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
