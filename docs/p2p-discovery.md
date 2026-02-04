# Commonware P2P Discovery Mechanism

This document describes how the `commonware-p2p` library's discovery network operates, specifically focusing on peer authorization and dynamic peer discovery.

## Overview

The `commonware-p2p` crate provides a discovery-based P2P networking layer that enables:

1. **Authenticated Communication**: All peers are identified by cryptographic keys (Ed25519)
2. **Automatic Peer Discovery**: Using bit vectors to efficiently share knowledge of dialable peers
3. **Multiplexed Messaging**: Multiple logical channels over a single connection

## Oracle-Based Peer Authorization

### The Core Concept

The discovery network operates on a fundamental assumption: **all peers must be synchronized on the composition of peer sets at specific indices**.

```
Peer Set = (index: u64, peers: Set<PublicKey>)
```

Each node maintains an **Oracle** that tracks which public keys are authorized to participate in the network. The Oracle:

1. Registers peer sets at specific indices via `oracle.update(index, peers)`
2. Allows new peer sets to be added dynamically (supporting transitions like DKG)
3. Enables peers to accept connections only from authorized public keys

### How It Works

```rust
// Register a peer set with the oracle
let peer_keys: Vec<ed25519::PublicKey> = vec![pk1, pk2, pk3];
let peer_set = Set::try_from(peer_keys).unwrap();
oracle.update(0, peer_set).await;
```

When a peer attempts to connect:
1. The listener verifies the dialer's public key during the cryptographic handshake
2. The tracker actor checks if the dialer's public key belongs to any registered peer set
3. Connection is accepted only if the public key is authorized

### Dynamic Peer Set Updates

The Oracle supports **dynamic updates** to peer sets:

```rust
// The Oracle can be called multiple times to register new peer sets
oracle.update(0, initial_validators).await;
// Later, add new peers
oracle.update(1, validators_plus_rpc_nodes).await;
```

Key behaviors:
- Multiple peer sets can be tracked concurrently (`tracked_peer_sets` config)
- A peer (listener) will accept connections from dialers in **any** registered set
- This enables network transitions where some nodes have newer peer set information

## Peer Discovery Process

### Bootstrap Phase

1. Node starts with a list of `bootstrappers` (known addresses)
2. Node dials bootstrappers and performs cryptographic handshake
3. Node sends its own `Info` (signed address attestation)
4. Node sends `BitVec` for tracked peer sets

### BitVec Exchange

Each `BitVec` contains:
- `index`: The peer set index
- `bits`: A bit vector where '1' = knows peer's address

Example with 4 peers (A, B, C, D):
```
Node A's BitVec: [1, 0, 1, 0]  // Knows addresses of peers A (self) and C
Node B's BitVec: [1, 1, 1, 0]  // Knows addresses of peers A, B (self), and C
```

When Node B receives Node A's BitVec, it can see Node A doesn't know about peer B, so it sends a `Peers` message with B's info.

### Address Gossip

The `Payload::Peers` message contains:
- `socket`: SocketAddr of the peer
- `timestamp`: When the address was attested
- `public_key`: The peer's public key
- `signature`: Cryptographic signature over socket and timestamp

Peers only gossip `Info` for peers they currently have active connections with, ensuring the information is fresh.

## Implications for RPC Nodes

### The Challenge

RPC nodes need to connect to validators to sync blocks via P2P. However:

1. Validators register peer sets containing only other validators
2. RPC nodes are **not** included in validators' Oracle
3. Validators reject RPC node connections as unauthorized

### Solution Approaches

#### Option 1: Pre-registration (Current Approach)

Include RPC node public keys in validators' peer sets at startup:

```rust
// In validator setup
let mut all_peers = validator_public_keys.clone();
all_peers.push(rpc_node_public_key);
oracle.update(0, Set::try_from(all_peers).unwrap()).await;
```

**Pros**: Simple, works with static networks
**Cons**: Requires knowing RPC node identity in advance

#### Option 2: Dynamic Oracle Updates

Update validators' Oracle after startup to include RPC nodes:

```rust
// Validators maintain Oracle reference
// When RPC node joins, broadcast its public key
// Validators update their Oracle
oracle.update(1, updated_peer_set).await;
```

**Pros**: Supports dynamic RPC node addition
**Cons**: Requires coordination mechanism (blockchain, config update)

#### Option 3: Separate Discovery Network

RPC nodes use gRPC instead of P2P for block fetching:

```rust
// RPC node queries validators via gRPC
let response = validator_grpc_client.get_blocks(request).await?;
```

**Pros**: Avoids P2P authorization complexity
**Cons**: Adds gRPC server requirement to validators

## Configuration Reference

Key `discovery::Config` parameters:

| Parameter | Description |
|-----------|-------------|
| `tracked_peer_sets` | Max concurrent peer sets to track (for transitions) |
| `gossip_bit_vec_frequency` | How often to send BitVec messages (ms) |
| `peer_gossip_max_count` | Max peers to gossip per BitVec response |
| `synchronize_backoff` | Retry backoff for peer synchronization |

## Summary

The commonware-p2p discovery network is designed for **authorized, authenticated** peer-to-peer communication. Key takeaways:

1. **Oracle is mandatory**: All participating peers must be registered via `oracle.update()`
2. **Symmetric registration**: For peer A to connect to peer B, B must have A in its Oracle
3. **Dynamic updates supported**: Multiple peer sets can be registered at different indices
4. **Discovery is automatic**: Once peers are authorized, address discovery happens via BitVec gossip

## TODOs

### 1. Dynamic RPC-to-Validator Connection

Currently, validators must pre-register RPC node public keys in their Oracle at startup. This requires:
- [ ] Implement a mechanism for RPC nodes to dynamically join the network without validator restarts
- [ ] Options:
  - **gRPC-only sync**: RPC nodes use gRPC instead of P2P (simpler, already works)
  - **Oracle update API**: Validators expose an authenticated endpoint to register new RPC peers
  - **Config file watch**: Validators reload peer config on file change

### 2. Integrate RPC Node into `node` Crate

The `rpc/src/main.rs` binary should be merged into the `node` crate as a subcommand.

**Current structure:**
```
node/src/main.rs       → `hellas-node run --config ...`
rpc/src/main.rs        → `rpc-node --config ...` (separate binary)
```

**Target structure:**
```
node/src/main.rs       → `hellas-node run --config ...`        (validator)
                       → `hellas-node rpc --config ...`         (RPC node)
                       → `hellas-node generate-configs ...`     (config gen)
```

**Implementation steps:**

1. **Add `Rpc` subcommand to `node/src/main.rs`**:
   ```rust
   #[derive(Subcommand, Debug)]
   enum Command {
       Run { config: PathBuf, node_index: Option<usize> },
       Rpc { config: PathBuf },  // NEW
       GenerateConfigs { output_dir: PathBuf },
   }
   ```

2. **Move RPC logic to node crate**:
   - Create `node/src/rpc_runner.rs` with `run_rpc_node()` function
   - Import from `rpc` crate: `use rpc::{RpcConfig, RpcIdentity, RpcNode};`
   - Handle CLI args, config loading, identity generation

3. **Shared components**:
   - `create_logger()` - already exists in both, unify
   - `ctrlc_handler()` - similar shutdown logic
   - Config file structure - consider unified config format

4. **Config file changes**:
   - Add RPC-specific section to `config.example.toml`
   - Or use separate `rpc-config.toml` with shared validator list

5. **Files to modify**:
   | File | Change |
   |------|--------|
   | `node/src/main.rs` | Add `Rpc` subcommand, import rpc crate |
   | `node/Cargo.toml` | Add `rpc` dependency |
   | `node/src/rpc_runner.rs` | NEW: RPC node runner logic |
   | `rpc/src/main.rs` | Can be removed after migration |

### 3. Validator gRPC Server Scope

- [ ] Should validators expose gRPC at all?  
  - **Yes for**: Transaction submission (clients need to submit txs)
  - **No for**: Block queries (should go through RPC nodes)
- [ ] Consider: Txs submitted to RPC nodes → P2P broadcast to validators
- [ ] This would require adding tx broadcast to RPC node's P2P layer

### 4. RPC Node Features to Complete

- [ ] Transaction submission proxy (forward to validators via P2P)
- [ ] WebSocket subscriptions for real-time block updates
- [ ] State queries (account balances, contract state)
- [ ] REST API layer on top of gRPC
