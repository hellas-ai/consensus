use redb::TableDefinition;

pub const ACCOUNTS: TableDefinition<&[u8], &[u8]> = TableDefinition::new("accounts");
pub const FINALIZED_BLOCKS: TableDefinition<&[u8], &[u8]> =
    TableDefinition::new("finalized_blocks");
pub const NON_FINALIZED_BLOCKS: TableDefinition<&[u8], &[u8]> =
    TableDefinition::new("non_finalized_blocks");
pub const NULLIFIED_BLOCKS: TableDefinition<&[u8], &[u8]> =
    TableDefinition::new("nullified_blocks");
pub const VOTES: TableDefinition<&[u8], &[u8]> = TableDefinition::new("votes");
pub const NOTARIZATIONS: TableDefinition<&[u8], &[u8]> = TableDefinition::new("notarizations");
pub const NULLIFIES: TableDefinition<&[u8], &[u8]> = TableDefinition::new("nullifies");
pub const NULLIFICATIONS: TableDefinition<&[u8], &[u8]> = TableDefinition::new("nullifications");
pub const VIEWS: TableDefinition<&[u8], &[u8]> = TableDefinition::new("views");
pub const LEADERS: TableDefinition<&[u8], &[u8]> = TableDefinition::new("leaders");
pub const STATE: TableDefinition<&[u8], &[u8]> = TableDefinition::new("state");
pub const MEMPOOL: TableDefinition<&[u8], &[u8]> = TableDefinition::new("mempool");
