# Hellas Core

Core protocol implementation for the Hellas network: decentralized infrastructure for generative AI applications.

## Overview

Hellas is a decentralized protocol designed to power generative AI applications at scale. The network enables node operators to bring AI models online and monetize inference capacity, while application developers gain access to a permissionless marketplace of AI compute resources.

### Why Hellas

Traditional blockchain architectures introduce significant latency overhead that is incompatible with interactive AI workloads. A user querying a language model expects sub-second responses, not the multi-second finality times typical of general-purpose chains. Hellas addresses this through two key innovations:

1. **Minimmit Consensus**: A Byzantine-fault-tolerant State Machine Replication (SMR) protocol that achieves approximately 17% lower latency than state-of-the-art protocols like Alpenglow and Simplex in globally distributed networks. The key insight is decoupling view progression from transaction finality: while requiring `n-f` votes for finalization (L-notarizations), the protocol allows view progression with only `2f+1` votes (M-notarizations). This minimizes overhead on the consensus hot path.

2. **Specialized State Channels**: For high-frequency AI inference requests, Hellas employs state channels that allow participants to transact off-chain with on-chain settlement guarantees. This enables nodes to serve thousands of inference requests per second without requiring each request to pass through consensus.

Together, these innovations enable a network where AI model providers can offer low-latency inference services with cryptographic guarantees of payment and execution integrity.

**Research Paper**: [Minimmit: Minimizing Latency of Optimistic BFT SMR](https://arxiv.org/pdf/2508.10862)

## Protocol Summary

### Byzantine Fault Tolerance

Minimmit operates under the assumption that on a total of `n` validators, at most `f` may exhibit Byzantine (arbitrary) faults, where:

```
n >= 5f + 1
```

This is the minimum resilience required for 2-round finality in partially synchronous settings. For example:
- `f = 1` requires `n >= 6` validators
- `f = 2` requires `n >= 11` validators

### Dual Notarization System

The protocol uses two distinct vote thresholds for different purposes:

| Notarization | Threshold | Purpose |
|--------------|-----------|---------|
| **M-Notarization** | `2f + 1` votes | View progression (move to next view) |
| **L-Notarization** | `n - f` votes | Transaction finality (block committed) |

This separation is the source of Minimmit's latency advantage: validators can progress to the next view as soon as they observe `2f + 1` votes, without waiting for the full `n - f` required for finality.

### Protocol Flow

```
┌─────────────┐
│   Leader    │
│  (View v)   │
└──────┬──────┘
       │
       │ 1. Propose Block B
       │    (view, transactions, parent_hash)
       │
       ▼
┌──────────────────────────────────────┐
│      P2P Layer Broadcast             │
└──────┬────────────┬────────────┬─────┘
       │            │            │
       ▼            ▼            ▼
   ┌───────┐    ┌───────┐    ┌───────┐
   │Node 1 │    │Node 2 │    │Node n │
   └───┬───┘    └───┬───┘    └───┬───┘
       │            │            │
       │ 2. Validate Block B     │
       │    (signatures, state)  │
       │            │            │
       ▼            ▼            ▼
   ┌────────────────────────────────┐
   │   Consensus Layer (each node)  │
   │   - Check validity conditions  │
   │   - Vote if valid              │
   └────────┬───────────────────────┘
            │
            │ 3. Broadcast Vote(B, v)
            │
            ▼
   ┌────────────────────────────────┐
   │    Vote Collection             │
   │                                │
   │  2f+1 votes --> M-Notarization │  <-- View Progression
   │  (move to view v+1)            │
   │                                │
   │  n-f votes --> L-Notarization  │  <-- Finalization
   │  (finalize block B)            │
   └────────────────────────────────┘
```

### Nullification (Handling Faulty Leaders)

When a view stalls (leader is faulty or network issues prevent timely delivery), validators send nullify messages:

1. Timer expires without receiving a valid block proposal
2. Validator broadcasts `Nullify(view)` message
3. When `2f + 1` nullify messages are collected, validators progress to next view
4. This ensures liveness: even with Byzantine leaders, honest validators eventually reach a view with an honest leader

### Safety Guarantees

Validators vote for a block only if:
1. An M-notarization exists for the parent block
2. Nullifications exist for all intermediate views (between parent and current)
3. The block passes validation (signatures, state transitions)

This ensures no two inconsistent blocks can both receive L-notarizations, maintaining the consistency property.

## Architecture

The implementation consists of four primary architectural layers:

```
┌─────────────────────────────────────────────────────────────────────┐
│                         EXTERNAL ENVIRONMENT                         │
│  Users/Txns              Other Peers              Other Peers        │
└─────────┬────────────────────┬────────────────────────┬──────────────┘
          │                    │                        │
┌─────────▼────────────────────▼────────────────────────▼──────────────┐
│                    P2P COMMUNICATION LAYER                            │
│  - Message Broadcasting (Blocks, Votes, Nullifications)               │
│  - Peer Discovery and Connection Management                           │
│  - BLS Signature Verification                                         │
│  - Message Authentication and Deduplication                           │
└──────────────────────┬────────────────────┬──────────────────────────┘
                       │                    │
            ┌──────────▼────────┐  ┌────────▼──────────┐
┌───────────▼──────────┐  ┌─────▼──▼─────────┐  ┌──────▼────────────┐
│  BLOCK VALIDATION    │  │  CORE CONSENSUS   │  │  PERSISTENT       │
│  LAYER               │<-│  LAYER            │<-│  STORAGE LAYER    │
│                      │  │  (Minimmit)       │  │                   │
│  - Block Structure   │  │  - View Mgmt      │  │  - Finalized      │
│  - Signatures        │  │  - Voting Logic   │  │    Blocks         │
│  - State Validation  │  │  - M/L-Notarize   │  │  - Votes          │
│                      │  │  - Nullification  │  │  - Notarizations  │
│  Parallel Ed25519    │  │  - Finalization   │  │                   │
│  batch verification  │  │                   │  │  RocksDB          │
└──────────────────────┘  └───────────────────┘  └───────────────────┘
```

### Thread Model

The node runs on 3 OS threads plus 1 async runtime:

| Thread | Responsibilities |
|--------|-----------------|
| **P2P** (Tokio) | Network I/O, BLS signature verification, message routing |
| **Mempool** | Transaction validation, storage, block proposal building |
| **Consensus** | Block validation, voting, finalization, state management |

### Inter-Thread Communication

All threads communicate via lock-free SPSC ring buffers (`rtrb`):

```
┌─────────────┐                       ┌───────────────────┐     ┌─────────────┐
│    P2P      │                       │     MEMPOOL       │     │  CONSENSUS  │
│   Thread    │                       │     Thread        │     │   Thread    │
└─────────────┘                       └───────────────────┘     └─────────────┘
      │                                       │                       │
      │ consensus_msgs (BlockProposal+Votes)  │                       │
      ├──────────────────────────────────────────────────────────────>│
      │                                       │                       │
      │ transactions                          │                       │
      ├──────────────────────────────────────>│                       │
      │                                       │                       │
      │                                       │ proposal_request      │
      │                                       │<──────────────────────┤
      │                                       │                       │
      │                                       │ proposal_response     │
      │                                       ├──────────────────────>│
      │                                       │                       │
      │                                       │ finalized_notif       │
      │                                       │<──────────────────────┤
      │                                       │                       │
      │<──────────────────────────────────────────────────────────────┤
      │                     broadcast                                 │
```

## Project Structure

```
core/
├── consensus/          # Core consensus implementation
│   ├── src/
│   │   ├── consensus_manager/   # Minimmit state machine
│   │   │   ├── consensus_engine.rs
│   │   │   ├── state_machine.rs
│   │   │   ├── view_manager.rs
│   │   │   └── view_chain.rs
│   │   ├── crypto/              # BLS signatures, aggregation
│   │   ├── mempool/             # Transaction pool service
│   │   ├── state/               # Blocks, transactions, accounts
│   │   ├── storage/             # RocksDB persistence
│   │   └── validation/          # Block and transaction validation
│   └── benches/                 # Performance benchmarks
├── crypto/             # Threshold cryptography
├── grpc-client/        # External gRPC API
│   ├── proto/          # Protocol buffer definitions
│   └── src/
│       ├── server.rs   # gRPC server
│       └── services/   # Service implementations
├── node/               # High-level node orchestration
│   └── src/
│       ├── config.rs   # Unified configuration
│       └── node.rs     # ValidatorNode struct
├── p2p/                # Peer-to-peer networking
│   └── src/
│       ├── network.rs  # Iroh-based networking
│       └── protocols/  # Consensus, gossip, sync protocols
├── tests/              # Integration and E2E tests
│   └── src/
│       ├── e2e_consensus/
│       └── gossip/
└── docs/               # Additional documentation
```

## Getting Started

### Prerequisites

- Rust 1.75 or later
- RocksDB dependencies (automatically built)

### Building

```bash
# Clone the repository
git clone https://github.com/hellas-network/core.git
cd core

# Build all crates
cargo build --release

# Run tests
cargo test

# Run benchmarks
cargo bench
```

### Running a Node

```rust
use node::{NodeConfig, ValidatorNode};
use p2p::ValidatorIdentity;

// Load configuration from file
let config = NodeConfig::from_path("config.toml")?;

// Generate or load validator identity
let identity = ValidatorIdentity::generate();

// Create logger
let logger = create_logger();

// Spawn all services
let node = ValidatorNode::<6, 1, 3>::from_config(config, identity, logger)?;

// Wait for P2P bootstrap (connect to other validators)
node.wait_ready().await;

// Node is now participating in consensus...

// Graceful shutdown
node.shutdown(Duration::from_secs(10))?;
```

## Configuration

Configuration can be provided via TOML, YAML, or environment variables. See [`config.example.toml`](config.example.toml) for a complete reference with all available options.

### Quick Start Configuration

Copy the example configuration and modify for your environment:

```bash
cp config.example.toml config.toml
# Edit config.toml with your settings
```

### Minimal Configuration

```toml
[consensus]
n = 6                                    # Total validators (must be >= 5f + 1)
f = 1                                    # Max Byzantine faults
view_timeout = { secs = 5, nanos = 0 }   # Block proposal timeout
leader_manager = "RoundRobin"
network = "local"
peers = []                               # BLS public keys of all validators
genesis_accounts = []

[storage]
path = "/var/lib/hellas/data"

[p2p]
listen_addr = "0.0.0.0:9000"
external_addr = "1.2.3.4:9000"           # Your public IP
total_number_peers = 6
maximum_number_faulty_peers = 1
validators = []                          # Bootstrap peer info

[rpc]
listen_addr = "0.0.0.0:50051"
peer_id = 0                              # This validator's index
network = "local"
total_validators = 6
f = 1

[identity]
bls_secret_key_path = "/etc/hellas/keys/bls.key"
ed25519_secret_key_path = "/etc/hellas/keys/ed25519.key"
```

### Environment Variables

Configuration can be overridden via environment variables with the `NODE_` prefix. Nested fields use double underscore:

```bash
NODE_CONSENSUS__N=6
NODE_CONSENSUS__F=1
NODE_CONSENSUS__VIEW_TIMEOUT__SECS=10
NODE_STORAGE__PATH=/data/hellas
NODE_P2P__LISTEN_ADDR=0.0.0.0:9000
NODE_RPC__LISTEN_ADDR=0.0.0.0:8080
```

## Cryptography

### BLS Signatures

The protocol uses BLS12-381 signatures for:
- Block proposals (leader signature)
- Votes (validator signatures)
- Nullification messages

BLS signatures enable efficient aggregation: `n` individual signatures can be combined into a single constant-size threshold signature.

### Threshold Signatures

Two threshold schemes are used:
- `2f + 1` threshold for M-notarizations and nullifications
- `n - f` threshold for L-notarizations (finality)

### Transaction Signatures

Individual transactions use Ed25519 signatures with parallel batch verification for performance.

## Performance

### Block Validation Benchmarks

Measured on Apple M2 (8 cores):

| Transactions | Sequential | Parallel Batch | Speedup |
|--------------|------------|----------------|---------|
| 100          | 2.8ms      | 0.8ms          | 3.5x    |
| 500          | 14ms       | 2.1ms          | 6.7x    |
| 1000         | 28ms       | 3.2ms          | 8.75x   |
| 5000         | 140ms      | 14ms           | 10x     |

### Latency Characteristics

- **Optimistic responsiveness**: Transaction latency is proportional to actual network delay, not predetermined timeouts
- **2-round finality**: Blocks finalize in 2 network rounds under normal conditions
- **Adaptive view changes**: Views progress as soon as `2f + 1` votes are collected

## Storage

The persistent storage layer uses RocksDB with the following schema:

| Table | Key | Value |
|-------|-----|-------|
| Blocks | `block_hash` | Block data (view, txs, parent, sigs) |
| Votes | `(view, block_hash, peer_id)` | BLS signature |
| Nullifications | `(view, peer_id)` | BLS signature |
| Notarizations | `(view, block_hash)` | Type (M/L), threshold sig, bitmap |
| Accounts | `address` | Balance, nonce, state |

## Mempool

The mempool implements a two-pool design:

- **Pending**: Transactions ready for execution (continuous nonces)
- **Queued**: Transactions waiting for earlier nonces

Features:
- Fee-based priority ordering
- Replace-by-Fee (RBF) support
- Configurable size limits with eviction

## gRPC API

The node exposes a gRPC API for external interaction:

| Service | Methods |
|---------|---------|
| `AccountService` | `GetAccount`, `GetBalance`, `GetNonce` |
| `BlockService` | `GetBlock`, `GetBlockByHeight`, `GetLatestBlock` |
| `TransactionService` | `SubmitTransaction`, `GetTransaction` |
| `NodeService` | `GetStatus`, `GetPeers`, `GetHealth` |
| `SubscriptionService` | `SubscribeBlocks`, `SubscribeTransactions` |

## Testing

```bash
# Unit tests
cargo test

# Integration tests
cargo test --package tests

# E2E consensus tests
cargo test --package tests e2e_consensus

# Gossip protocol tests  
cargo test --package tests gossip

# Run with logging
RUST_LOG=debug cargo test
```

## Security Considerations

- All peer connections use authenticated encryption
- Messages include timestamps and sequence numbers to prevent replay attacks
- Rate limiting protects against denial-of-service attempts
- Peer connections are diversified to resist eclipse attacks

## Contributing

Contributions are welcome. Please ensure:

1. All tests pass (`cargo test`)
2. No clippy warnings (`cargo clippy -- -D warnings`)
3. Code is formatted (`cargo fmt`)
4. Documentation is updated for public APIs

## License

Licensed under the Apache License, Version 2.0. See [LICENSE](LICENSE) for details.

## References

- [Minimmit: Minimizing Latency of Optimistic BFT SMR](https://arxiv.org/pdf/2508.10862)
- [BLS12-381 Curve](https://hackmd.io/@benjaminion/bls12-381)
- [Commonware Runtime](https://github.com/commonwarexyz/monorepo)
