//! Runtime abstraction for production and deterministic testing.
//!
//! This module provides a unified runtime interface that works with both:
//! - Production: Tokio async runtime
//! - Testing: Commonware deterministic simulator

/// Re-export the runtime context for use throughout the crate.
#[cfg(not(test))]
pub use commonware_runtime::tokio::Context;

#[cfg(test)]
pub use commonware_runtime::deterministic::Context;

/// Re-export runner trait for starting the runtime.
pub use commonware_runtime::Runner;

/// Re-export common runtime utilities.
pub use commonware_runtime::{Clock, Metrics, Quota, Spawner};

// Re-export commonware_runtime traits for generic bounds
pub use commonware_runtime::{Network, Resolver};
