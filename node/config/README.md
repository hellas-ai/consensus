# Local Network Configuration Files

This directory contains pre-generated configuration files for a 6-validator local development network.

## Quick Start

Run the full local network:

```bash
./node/config/run-local-network.sh
```

Or run individual nodes in separate terminals:

```bash
cargo run -p node -- run -c node/config/node0.toml
cargo run -p node -- run -c node/config/node1.toml
cargo run -p node -- run -c node/config/node2.toml
cargo run -p node -- run -c node/config/node3.toml
cargo run -p node -- run -c node/config/node4.toml
cargo run -p node -- run -c node/config/node5.toml
```

## Files

| File | Description |
|------|-------------|
| `node0.toml` - `node5.toml` | Complete configuration for each validator |
| `run-local-network.sh` | Script to run all 6 validators |

## Network Ports

| Node | P2P Port | gRPC Port |
|------|----------|-----------|
| 0 | 9000 | 50051 |
| 1 | 9100 | 50052 |
| 2 | 9200 | 50053 |
| 3 | 9300 | 50054 |
| 4 | 9400 | 50055 |
| 5 | 9500 | 50056 |

## Data Directories

Node data is stored in `./data/node{index}/`.

## Regenerating Configs

If you need to regenerate the configuration files:

```bash
cargo run -p node -- generate-configs --output-dir node/config
```

Note: The configs use a deterministic seed (42) so regenerating will produce identical keys.
