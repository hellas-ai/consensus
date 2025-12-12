//! Mempool Service - Transaction Pool and Block Proposal Builder
//!
//! This module provides a dedicated OS thread for managing the transaction mempool.
//! It receives transactions from P2P/RPC, stores them in a fee-priority queue, and
//! builds block proposals on request from the consensus engine.
//!
//! ## Architecture
//!
//!
//! P2P/RPC ──[tx_channel]──► Mempool ◄──[proposal_req_channel]── Consensus
//!                              │
//!                              ├──[proposal_resp_channel]──► Consensus
//!                              │
//!                              ◄──[finalized_channel]────── Consensus
//!
//!
//! ## Data Flow
//!
//! 1. Transaction Ingestion: P2P and RPC threads push transactions via tx_producer
//! 2. Validation: Basic signature verification via Transaction::verify()
//! 3. Storage: Valid transactions stored in fee-priority queue
//! 4. Proposal Building: On ProposalRequest, select highest-fee transactions
//! 5. Finalization Cleanup: On FinalizedNotification, remove included transactions
//!
//! ## Thread Safety
//!
//! All communication uses lock-free ring buffers.
//! The mempool runs on its own dedicated OS thread, avoiding contention with
//! consensus and networking.

mod pool;
mod service;
mod types;

pub use pool::TransactionPool;
pub use service::{MempoolChannels, MempoolService};
pub use types::{FinalizedNotification, ProposalRequest, ProposalResponse, ValidatedTransaction};
