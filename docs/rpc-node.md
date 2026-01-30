# RPC Nodes

RPC nodes are non-validator nodes that synchronize finalized blockchain state and serve the gRPC API to external clients. They offload query traffic from validators, preventing the consensus network from being overwhelmed by wallet connections.

## Overview

```
┌─────────────────────────────────────────────────────────────────────────────┐
│                             Hellas Network                                  │
├─────────────────────────────────────────────────────────────────────────────┤
│                                                                             │
│    ┌─────────────┐       ┌─────────────┐       ┌─────────────┐              │
│    │ Validator 1 │◄─────►│ Validator 2 │◄─────►│ Validator 3 │              │
│    │ (Consensus) │       │ (Consensus) │       │ (Consensus) │              │
│    └──────┬──────┘       └──────┬──────┘       └─────────────┘              │
│           │                     │                                           │
│           │ Finalized           │ Finalized                                 │
│           │ Blocks Only         │ Blocks Only                               │
│           ▼                     ▼                                           │
│    ┌─────────────┐       ┌─────────────┐                                    │
│    │  RPC Node 1 │       │  RPC Node 2 │                                    │
│    │  (Sync)     │       │  (Sync)     │                                    │
│    └──────┬──────┘       └──────┬──────┘                                    │
│           │ gRPC                │ gRPC                                      │
│           ▼                     ▼                                           │
│    ┌─────────────┐       ┌─────────────┐                                    │
│    │   Wallets   │       │  Explorers  │                                    │
│    └─────────────┘       └─────────────┘                                    │
│                                                                             │
└─────────────────────────────────────────────────────────────────────────────┘
```

### Key Characteristics

| Property | Validator Node | RPC Node |
|----------|---------------|----------|
| BLS Keys | ✅ Required | ❌ None |
| Ed25519 Keys | ✅ Required | ✅ Required |
| Consensus Participation | ✅ Votes & Proposes | ❌ Sync Only |
| Block Types Received | M + L-notarized | L-notarized only |
| gRPC API | ✅ Full | ✅ Full (read-only) |
| P2P Connection Type | Validator-Validator | Validator-RPC |

---

## Architecture

### Connection Model

RPC nodes connect to one or more validators to receive finalized blocks. Validators enforce a configurable connection limit (`max_rpc_connections`) to prevent resource exhaustion.

```rust
// P2P configuration on validators
pub struct P2PConfig {
    /// Maximum RPC node connections allowed
    pub max_rpc_connections: usize,  // default: 100
    // ...
}
```

**Connection Lifecycle:**

1. RPC node initiates TCP connection to validator
2. Handshake includes `rpc_mode: true` flag
3. Validator checks connection count against `max_rpc_connections`
4. If limit exceeded: **immediate `ConnectionRefused` error**
5. If accepted: RPC node added to `RpcPeerManager`

### Block Synchronization

RPC nodes receive finalized blocks via **push-based P2P broadcast**. Validators broadcast L-notarized (finalized) blocks to all connected RPC peers.

```rust
/// Message routing for RPC nodes
enum P2PMessage {
    // RPC nodes receive these:
    FinalizedBlock(Block),        // L-notarized blocks
    
    // RPC nodes DO NOT receive these:
    Proposal(Proposal),           // Consensus only
    Vote(Vote),                   // Consensus only
    MNotarization(MNotarization), // Consensus only
    Transaction(Transaction),     // Mempool gossip
}
```

**Sync Flow:**

```
Validator                           RPC Node
    │                                   │
    │  ──── FinalizedBlock(h=100) ───►  │
    │                                   │  Store block
    │  ──── FinalizedBlock(h=101) ───►  │
    │                                   │  Store block
    │                                   │
    │  ◄──── GetBlock(h=99) ──────────  │  (Catchup if needed)
    │  ──── BlockResponse(h=99) ─────►  │
    │                                   │
```

### State Machine

RPC nodes run a simplified state machine focused on block synchronization:

```
┌─────────────┐     Bootstrap      ┌─────────────┐
│   Booting   │ ─────────────────► │   Syncing   │
└─────────────┘                    └──────┬──────┘
                                          │
                                   Caught up to tip
                                          │
                                          ▼
                                   ┌─────────────┐
                                   │    Ready    │ ◄─── New blocks arrive
                                   └─────────────┘
```

**States:**

| State | Description |
|-------|-------------|
| `Booting` | Establishing P2P connections to validators |
| `Syncing` | Catching up to chain tip via block requests |
| `Ready` | Fully synced, serving gRPC queries |

---

## Configuration

### RPC Node Config (`rpc-node.toml`)

```toml
[p2p]
listen_addr = "0.0.0.0:9100"
external_addr = "1.2.3.4:9100"
rpc_mode = true  # Identifies as RPC node

# Validators to connect to (at least one required)
[[p2p.validators]]
ed25519_public_key = "abc123..."
address = "validator1.example.com:9000"

[[p2p.validators]]
ed25519_public_key = "def456..."
address = "validator2.example.com:9000"

[rpc]
listen_addr = "0.0.0.0:50051"
max_concurrent_streams = 1000

[storage]
path = "/var/lib/hellas/rpc-node.redb"
```

### Validator Config Addition

```toml
[p2p]
# Existing fields...
max_rpc_connections = 100  # Limit RPC node connections
```

---

## Implementation Details

### P2P Handshake Extension

The P2P handshake is extended to include node type:

```rust
/// Extended handshake message
pub struct Handshake {
    pub ed25519_public_key: PublicKey,
    pub node_type: NodeType,  // NEW
    // ...
}

pub enum NodeType {
    Validator { bls_peer_id: PeerId },
    Rpc,
}
```

### RpcNode Struct

```rust
/// RPC node orchestrator (no consensus engine)
pub struct RpcNode {
    /// P2P network handle
    p2p_handle: P2PHandle,
    
    /// Block storage
    storage: Arc<ConsensusStore>,
    
    /// gRPC server join handle
    grpc_handle: JoinHandle<()>,
    
    /// Block sync state machine
    sync: BlockSync,
    
    /// Shutdown signal
    shutdown: Arc<AtomicBool>,
}

impl RpcNode {
    /// Create from configuration (no BLS keys needed)
    pub fn spawn(config: RpcNodeConfig, logger: Logger) -> Result<Self>;
    
    /// Wait for initial sync to complete
    pub async fn wait_ready(&self);
    
    /// Latest synced block height
    pub fn latest_height(&self) -> u64;
    
    /// Graceful shutdown
    pub fn shutdown(self, timeout: Duration) -> Result<()>;
}
```

### CLI Usage

```bash
# Run as validator (existing)
cargo run -p node -- run --config node0.toml

# Run as RPC node (new)
cargo run -p node -- rpc-run --config rpc-node.toml
```

---

## gRPC API

RPC nodes expose the same gRPC services as validators, but with some restrictions:

| Service | RPC Node Support | Notes |
|---------|-----------------|-------|
| `AccountService` | ✅ Full | Balance, nonce queries |
| `BlockService` | ✅ Full | Block queries by hash/height |
| `TransactionService` | ⚠️ Read-only | `GetTx` works, `SubmitTx` forwards to validator |
| `NodeService` | ✅ Full | Health, sync status |
| `SubscriptionService` | ✅ Full | Block/tx event streams |
| `AdminService` | ❌ Disabled | No admin operations |

### Transaction Submission

When a client submits a transaction to an RPC node:

1. RPC node validates transaction format
2. Forwards to connected validator via P2P
3. Returns success once validator ACKs receipt

---

## Deployment Considerations

### Recommended Topology

- **Small network (< 10 validators)**: 2-3 RPC nodes per validator
- **Production network**: Dedicated RPC tier behind load balancer

```
                    ┌─────────────┐
                    │   Load      │
                    │  Balancer   │
                    └──────┬──────┘
                           │
         ┌─────────────────┼─────────────────┐
         ▼                 ▼                 ▼
   ┌───────────┐    ┌───────────┐    ┌───────────┐
   │ RPC Node 1│    │ RPC Node 2│    │ RPC Node 3│
   └─────┬─────┘    └─────┬─────┘    └─────┬─────┘
         │                │                │
         └────────────────┼────────────────┘
                          ▼
              ┌─────────────────────┐
              │  Validator Network  │
              └─────────────────────┘
```

### Resource Requirements

| Resource | RPC Node | Validator |
|----------|----------|-----------|
| CPU | 2 cores | 4+ cores |
| RAM | 4 GB | 8+ GB |
| Storage | Chain size | Chain size + mempool |
| Network | Low bandwidth | High bandwidth |

---

## Security Considerations

1. **No Consensus Keys**: RPC nodes have no BLS keys, so they cannot forge votes or proposals even if compromised.

2. **Connection Limits**: `max_rpc_connections` prevents DoS against validators.

3. **Read-Only by Default**: RPC nodes cannot modify consensus state; they only sync finalized blocks.

4. **Transaction Forwarding**: Transactions submitted to RPC nodes are forwarded to validators, maintaining security properties.
