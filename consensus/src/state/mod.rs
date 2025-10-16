use std::time::{SystemTime, UNIX_EPOCH};

pub mod block;
pub mod leader;
pub mod notarizations;
pub mod nullify;
pub mod peer;
pub mod transaction;
pub mod view;

pub fn now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_secs()
}
