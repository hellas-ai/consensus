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

## Light Client Finality Proofs

Light clients connecting to RPC nodes need cryptographic proof that block data is correct. Without proofs, clients must trust the RPC node—which defeats the purpose of blockchain verification.

### Problem Statement

```
┌──────────────┐                    ┌──────────────┐
│    Wallet    │ ◄── Block Data ─── │   RPC Node   │
└──────────────┘                    └──────────────┘
       │                                    │
       ▼                                    ▼
   ❓ Trust?                         ❓ Honest?
```

**Current limitation**: RPC nodes serve block data, but clients have no way to verify:
- Block was actually finalized (L-notarized)
- Validator set attested to the block
- Data hasn't been tampered with

### Solution: L-Notarization Certificates

Store and expose the BLS threshold signatures that prove block finalization.

#### L-Notarization Structure

```rust
/// L-Notarization certificate proving block finality.
/// 
/// Created when a block receives n-f votes (L-notarization).
/// This is the cryptographic proof of consensus finalization.
#[derive(Archive, Deserialize, Serialize, Clone, Debug)]
pub struct LNotarization<const N: usize, const F: usize, const L_SIZE: usize> {
    /// View number when block was finalized
    pub view: u64,
    
    /// Hash of the finalized block
    pub block_hash: [u8; 32],
    
    /// Aggregated BLS signature from n-f validators
    pub aggregated_signature: BlsSignature,
    
    /// Peer IDs of validators who signed (n-f validators)
    pub peer_ids: [PeerId; L_SIZE],
    
    /// Block height for easier lookup
    pub height: u64,
}

impl<const N: usize, const F: usize, const L_SIZE: usize> LNotarization<N, F, L_SIZE> {
    /// Verify the L-notarization against the validator set
    pub fn verify(&self, peer_set: &PeerSet) -> bool {
        let public_keys: Vec<BlsPublicKey> = self.peer_ids
            .iter()
            .filter_map(|id| peer_set.get_public_key(id).cloned())
            .collect();
        
        // Require n-f signatures
        if public_keys.len() < N - F {
            return false;
        }
        
        BlsPublicKey::aggregate(&public_keys)
            .verify(&self.block_hash, &self.aggregated_signature)
    }
}
```

#### Storage Changes

```rust
// New table in ConsensusStore
const L_NOTARIZATIONS: TableDefinition<'_, &[u8], &[u8]> = 
    TableDefinition::new("l_notarizations");

impl ConsensusStore {
    /// Store L-notarization certificate alongside finalized block
    pub fn put_l_notarization<const N: usize, const F: usize, const L_SIZE: usize>(
        &self, 
        cert: &LNotarization<N, F, L_SIZE>
    ) -> Result<()>;
    
    /// Retrieve L-notarization by block hash
    pub fn get_l_notarization<const N: usize, const F: usize, const L_SIZE: usize>(
        &self, 
        block_hash: &[u8; 32]
    ) -> Result<Option<LNotarization<N, F, L_SIZE>>>;
    
    /// Retrieve L-notarization by height
    pub fn get_l_notarization_by_height<const N: usize, const F: usize, const L_SIZE: usize>(
        &self, 
        height: u64
    ) -> Result<Option<LNotarization<N, F, L_SIZE>>>;
}
```

### ConsensusService gRPC API

New gRPC service for light client verification:

```protobuf
syntax = "proto3";
package hellas.consensus;

service ConsensusService {
    // Get L-notarization certificate for a block
    rpc GetLNotarization(GetLNotarizationRequest) 
        returns (GetLNotarizationResponse);
    
    // Get L-notarization certificate by height
    rpc GetLNotarizationByHeight(GetLNotarizationByHeightRequest) 
        returns (GetLNotarizationResponse);
    
    // Get validator set for verification
    rpc GetValidatorSet(GetValidatorSetRequest) 
        returns (GetValidatorSetResponse);
    
    // Get aggregated BLS public key for epoch
    rpc GetAggregatedPublicKey(GetAggregatedPublicKeyRequest) 
        returns (GetAggregatedPublicKeyResponse);
}

message GetLNotarizationRequest {
    bytes block_hash = 1;
}

message GetLNotarizationResponse {
    uint64 view = 1;
    bytes block_hash = 2;
    bytes aggregated_signature = 3;  // BLS signature
    repeated uint64 peer_ids = 4;    // Validators who signed
    uint64 height = 5;
}

message GetValidatorSetResponse {
    repeated Validator validators = 1;
}

message Validator {
    uint64 peer_id = 1;
    bytes bls_public_key = 2;
    uint64 stake = 3;  // For weighted voting (future)
}
```

### Verification Flow

```
┌──────────────-┐                         ┌──────────────┐
│  Light Client │                         │   RPC Node   │
└──────┬───────-┘                         └──────┬───────┘
       │                                        │
       │ 1. GetBlock(height=100)                │
       │ ─────────────────────────────────────► │
       │                                        │
       │ ◄───────────────── Block(h=100) ───── │
       │                                        │
       │ 2. GetLNotarization(block_hash)        │
       │ ─────────────────────────────────────► │
       │                                        │
       │ ◄────────── LNotarization cert ─────── │
       │                                        │
       │ 3. GetValidatorSet()                   │  (cached)
       │ ─────────────────────────────────────► │
       │                                        │
       │ ◄───────── ValidatorSet ────────────── │
       │                                        │
       ▼                                        │
┌──────────────┐                               │
│ Verify BLS   │                               │
│ signature    │                               │
│ locally      │                               │
└──────────────┘
```

**Light client verification steps**:

1. Request block data from RPC node
2. Request L-notarization certificate for that block
3. Fetch validator set (can be cached for epoch duration)
4. Aggregate public keys of validators who signed
5. Verify BLS signature: `verify(aggregated_pubkey, block_hash, signature)`

### On-Chain Verification

For bridges and smart contracts, the verification can happen on-chain:

```solidity
// Pseudo-Solidity for bridge contract
contract HellasLightClient {
    // Cached validator set (updated on epoch change)
    mapping(uint64 => bytes) public aggregatedPubKeys;  // epoch => pubkey
    
    /// Verify a block was finalized by Hellas validators
    function verifyBlock(
        bytes32 blockHash,
        bytes calldata blsSignature,
        uint64[] calldata signerPeerIds,
        uint64 epoch
    ) external view returns (bool) {
        // 1. Check quorum (n-f signers)
        require(signerPeerIds.length >= QUORUM_SIZE, "Insufficient signers");
        
        // 2. Aggregate public keys of signers
        bytes memory aggregatedKey = aggregateKeys(signerPeerIds, epoch);
        
        // 3. Verify BLS signature
        return BLS.verify(aggregatedKey, blockHash, blsSignature);
    }
}
```

### Merkle Proofs for State Verification (Phase 2)

> [!NOTE]
> **This is a Phase 2 enhancement.** Phase 1 (L-notarization) is sufficient for proving block finality. Merkle proofs are only needed when clients must verify *specific* transactions or account states without downloading entire blocks.

Beyond block finality, clients may need to prove specific state (account balance, transaction inclusion). This requires adding a merkle root to the block header:

#### BlockHeader Changes (Phase 2)

```rust
pub struct BlockHeader {
    pub view: u64,
    pub parent_block_hash: [u8; 32],
    pub timestamp: u64,
    pub txs_merkle_root: [u8; 32],  // NEW: Merkle root of transactions
}
```

**Why this change?**
- Without merkle root: Light client must download all transactions to verify one
- With merkle root: Light client downloads only the merkle proof path (log₂(n) hashes)

#### State Tree Structure

```
                    State Root (in BlockHeader)
                         │
            ┌────────────┴────────────┐
            │                         │
      Accounts Root              Txs Root
            │                         │
     ┌──────┴──────┐           ┌──────┴──────┐
     │             │           │             │
   Acc 1        Acc 2       Tx 1          Tx 2
```

#### Merkle Proof Structure

```rust
/// Merkle proof for state inclusion
pub struct StateMerkleProof {
    /// Leaf value (e.g., account data)
    pub leaf: Vec<u8>,
    
    /// Merkle path (sibling hashes from leaf to root)
    pub proof: Vec<[u8; 32]>,
    
    /// Index of leaf in tree
    pub leaf_index: u64,
}

impl StateMerkleProof {
    /// Verify leaf is included in the merkle tree with given root
    pub fn verify(&self, root: [u8; 32]) -> bool {
        let mut hash = blake3::hash(&self.leaf);
        
        for (i, sibling) in self.proof.iter().enumerate() {
            let bit = (self.leaf_index >> i) & 1;
            hash = if bit == 0 {
                blake3::hash(&[hash.as_bytes(), sibling].concat())
            } else {
                blake3::hash(&[sibling, hash.as_bytes()].concat())
            };
        }
        
        hash.as_bytes() == &root
    }
}
```

### Implementation Phases

| Phase | Scope | Key Components | Use Cases |
|-------|-------|----------------|-----------|
| **Phase 1** | Block finality proofs | `LNotarization` struct, storage, `ConsensusService` gRPC API | Light clients verify blocks were finalized |
| **Phase 2** | State inclusion proofs | `txs_merkle_root` in BlockHeader, `StateMerkleProof`, proof gRPC endpoints | Light clients verify tx/account inclusion without full blocks |
| **Phase 3** | Bridge support | On-chain BLS verifier contract, epoch transitions, validator set updates | Cross-chain bridges, trustless relayers |

---

## Security Considerations

1. **No Consensus Keys**: RPC nodes have no BLS keys, so they cannot forge votes or proposals even if compromised.

2. **Connection Limits**: `max_rpc_connections` prevents DoS against validators.

3. **Read-Only by Default**: RPC nodes cannot modify consensus state; they only sync finalized blocks.

4. **Transaction Forwarding**: Transactions submitted to RPC nodes are forwarded to validators, maintaining security properties.
