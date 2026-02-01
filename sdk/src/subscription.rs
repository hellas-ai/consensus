//! Subscription client for event streams (placeholder)

// TODO: Implement subscription streaming for:
// - New blocks
// - Transaction confirmations
// - Account updates

use crate::client::GrpcChannel;

/// Client for subscription operations.
///
/// This is a placeholder for future implementation.
pub struct SubscriptionClient {
    _channel: GrpcChannel,
}

impl SubscriptionClient {
    pub(crate) fn new(channel: GrpcChannel) -> Self {
        Self { _channel: channel }
    }
}
