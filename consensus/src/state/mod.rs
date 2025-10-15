use std::time::{SystemTime, UNIX_EPOCH};

pub mod block;
pub mod leader;
pub mod nullify;
pub mod transaction;
pub mod view;
pub mod voters;

pub fn now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_secs()
}
