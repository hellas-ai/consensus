//! Subscription client for event streams (placeholder)

// TODO: Implement subscription streaming for:
// - New blocks
// - Transaction confirmations
// - Account updates

/// Client for subscription operations.
///
/// This is a placeholder for future implementation.
pub struct SubscriptionClient {
    _channel: tonic::transport::Channel,
}

impl SubscriptionClient {
    pub(crate) fn new(channel: tonic::transport::Channel) -> Self {
        Self { _channel: channel }
    }
}
