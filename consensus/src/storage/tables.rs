use redb::TableDefinition;

pub const ACCOUNTS: TableDefinition<&[u8], &[u8]> = TableDefinition::new("accounts");
pub const BLOCKS: TableDefinition<&[u8], &[u8]> = TableDefinition::new("blocks");
pub const VOTES: TableDefinition<&[u8], &[u8]> = TableDefinition::new("votes");
pub const NOTARIZATIONS: TableDefinition<&[u8], &[u8]> = TableDefinition::new("notarizations");
pub const NULLIFICATIONS: TableDefinition<&[u8], &[u8]> = TableDefinition::new("nullifications");
pub const VIEWS: TableDefinition<&[u8], &[u8]> = TableDefinition::new("views");
pub const LEADERS: TableDefinition<&[u8], &[u8]> = TableDefinition::new("leaders");
pub const STATE: TableDefinition<&[u8], &[u8]> = TableDefinition::new("state");
pub const MEMPOOL: TableDefinition<&[u8], &[u8]> = TableDefinition::new("mempool");
