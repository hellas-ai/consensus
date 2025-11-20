//! Integration Tests for Minimmit Consensus
//!
//! This module contains end-to-end integration tests for the Minimmit BFT consensus protocol.
//! These tests verify the correctness of the full consensus system by simulating networks
//! of replicas reaching agreement under various conditions.
//!
//! ## Test Organization
//!
//! - **`e2e_consensus`**: End-to-end tests for basic consensus functionality
//! - **`network_simulator`**: Network simulation infrastructure for routing messages between replicas
//! - **`test_helpers`**: Common utilities, fixtures, and helper functions
//!
//! ## Running Integration Tests
//!
//! Integration tests are marked with `#[ignore]` by default as they are longer-running:
//!
//!
//! # Run all integration tests
//! cargo test --lib -- --ignored --nocapture
//!
//! # Run specific test
//! cargo test --lib test_e2e_consensus_multiple_views -- --ignored --nocapture
//! //!
//! ## Test Configuration
//!
//! - **N = 6**: Total number of replicas (3F + 1)
//! - **F = 1**: Maximum number of Byzantine replicas tolerated
//! - **M_SIZE = 3**: Size of Merkle proof (2F + 1)

mod e2e_consensus;
mod network_simulator;
mod test_helpers;
