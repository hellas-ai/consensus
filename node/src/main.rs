//! Hellas Validator Node Binary
//!
//! This binary runs a validator node for the Hellas consensus network.
//!
//! # Usage
//!
//! Run a node with a configuration file:
//! ```bash
//! cargo run --package node -- run --config node/config/node0.toml
//! ```
//!
//! Regenerate local network configs (if needed):
//! ```bash
//! cargo run --package node -- generate-configs
//! ```

use std::fs;
use std::path::PathBuf;

use anyhow::{Context, Result};
use clap::{Parser, Subcommand};
use slog::{Drain, Logger, o};

use ark_serialize::CanonicalSerialize;
use commonware_runtime::Runner;
use commonware_runtime::tokio::Runner as TokioRunner;
use consensus::crypto::aggregated::BlsSecretKey;
use node::ValidatorNode;
use p2p::ValidatorIdentity;

/// Network size constants
const N: usize = 6;
const F: usize = 1;
const M_SIZE: usize = 3;

/// Base ports for local network
const BASE_P2P_PORT: u16 = 9000;
const BASE_GRPC_PORT: u16 = 50051;
const PORT_GAP: u16 = 100;

/// Fixed seed for deterministic key generation
const LOCAL_NETWORK_SEED: u64 = 42;

#[derive(Parser, Debug)]
#[command(name = "hellas-node")]
#[command(about = "Hellas validator node for the Minimmit consensus protocol")]
#[command(version)]
struct Args {
    #[command(subcommand)]
    command: Command,

    /// Log level (trace, debug, info, warn, error)
    #[arg(long, default_value = "info", global = true)]
    log_level: String,
}

#[derive(Subcommand, Debug)]
enum Command {
    /// Run a validator node
    Run {
        /// Path to the configuration file
        #[arg(short, long)]
        config: PathBuf,

        /// Override the node index (defaults to rpc.peer_id from config)
        #[arg(long)]
        node_index: Option<usize>,
    },

    /// Regenerate local network config files
    GenerateConfigs {
        /// Output directory
        #[arg(short, long, default_value = "node/config")]
        output_dir: PathBuf,

        /// Output test fixtures as JSON to stdout instead of writing config files
        #[arg(long)]
        json: bool,
    },
}

fn main() -> Result<()> {
    let args = Args::parse();
    let logger = create_logger(&args.log_level);

    match args.command {
        Command::Run { config, node_index } => run_node(config, node_index, logger),
        Command::GenerateConfigs { output_dir, json } => {
            if json {
                generate_fixtures_json()
            } else {
                generate_configs(&output_dir, logger)
            }
        }
    }
}

/// Run a validator node from configuration.
fn run_node(
    config_path: PathBuf,
    node_index_override: Option<usize>,
    logger: Logger,
) -> Result<()> {
    slog::info!(logger, "Loading configuration"; "path" => %config_path.display());

    let config = node::NodeConfig::from_path(&config_path)
        .with_context(|| format!("Failed to load config from {}", config_path.display()))?;

    let node_index = node_index_override.unwrap_or(config.rpc.peer_id as usize);

    slog::info!(
        logger,
        "Configuration loaded";
        "node_index" => node_index,
        "p2p_addr" => %config.p2p.listen_addr,
        "grpc_addr" => %config.rpc.listen_addr,
    );

    // Generate deterministic identity
    let identities = generate_deterministic_identities(N);
    let identity = identities
        .into_iter()
        .nth(node_index)
        .context("Node index out of range")?;

    slog::info!(logger, "Starting validator"; "peer_id" => identity.peer_id());

    // Create parent directory of the storage path (storage.path is a file, not a directory)
    if let Some(parent) = std::path::Path::new(&config.storage.path).parent() {
        fs::create_dir_all(parent)
            .with_context(|| format!("Failed to create storage directory: {}", parent.display()))?;
    }

    let node = ValidatorNode::<N, F, M_SIZE>::from_config(config, identity, logger.clone())?;

    slog::info!(logger, "Node spawned, waiting for P2P bootstrap...");

    let executor = TokioRunner::default();
    executor.start(|_ctx| async move {
        if let Err(e) = node.run().await {
            slog::error!(logger, "Node error"; "error" => %e);
        }
    });

    Ok(())
}

/// Generate local network configuration files.
fn generate_configs(output_dir: &std::path::Path, logger: Logger) -> Result<()> {
    slog::info!(logger, "Generating configs"; "output_dir" => %output_dir.display());

    fs::create_dir_all(output_dir)?;

    let identities = generate_deterministic_identities(N);

    // Collect peer info
    let peers: Vec<PeerInfo> = identities
        .iter()
        .enumerate()
        .map(|(i, id)| {
            let mut bls_buf = Vec::new();
            id.bls_public_key()
                .0
                .serialize_compressed(&mut bls_buf)
                .unwrap();

            PeerInfo {
                index: i,
                bls_pubkey: hex::encode(&bls_buf),
                ed25519_pubkey: hex::encode(id.ed25519_public_key().as_ref()),
                peer_id: id.peer_id(),
                p2p_port: BASE_P2P_PORT + (i as u16 * PORT_GAP),
                grpc_port: BASE_GRPC_PORT + i as u16,
            }
        })
        .collect();

    // Generate genesis accounts
    let genesis_accounts = generate_genesis_accounts();

    // Generate each node's config
    for i in 0..N {
        let content = build_config(i, &peers, &genesis_accounts);
        let path = output_dir.join(format!("node{}.toml", i));
        fs::write(&path, &content)?;
        slog::info!(logger, "Generated"; "file" => %path.display());
    }

    // Generate run script
    let script = build_run_script(output_dir);
    let script_path = output_dir.join("run-local-network.sh");
    fs::write(&script_path, &script)?;

    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(&script_path, fs::Permissions::from_mode(0o755))?;
    }

    println!("\nGenerated {} config files in {}", N, output_dir.display());
    println!("\nTo run the local network:");
    println!("  {}", script_path.display());
    println!("\nOr run individual nodes:");
    for i in 0..N {
        println!(
            "  cargo run -p node -- run -c {}/node{}.toml",
            output_dir.display(),
            i
        );
    }

    Ok(())
}

struct PeerInfo {
    index: usize,
    bls_pubkey: String,
    ed25519_pubkey: String,
    peer_id: u64,
    p2p_port: u16,
    grpc_port: u16,
}

/// Generate deterministic genesis account keys for testing.
fn generate_genesis_accounts() -> Vec<(String, u64)> {
    use rand::SeedableRng;
    // Use a different seed for genesis accounts
    let mut rng = rand::rngs::StdRng::seed_from_u64(1337);

    vec![
        {
            let key = ed25519_dalek::SigningKey::generate(&mut rng);
            let pubkey_hex = hex::encode(key.verifying_key().as_bytes());
            (pubkey_hex, 1_000_000_000)
        },
        {
            let key = ed25519_dalek::SigningKey::generate(&mut rng);
            let pubkey_hex = hex::encode(key.verifying_key().as_bytes());
            (pubkey_hex, 500_000_000)
        },
    ]
}

/// Output all deterministic test fixtures as JSON.
fn generate_fixtures_json() -> Result<()> {
    let identities = generate_deterministic_identities(N);

    let peers: Vec<_> = identities
        .iter()
        .enumerate()
        .map(|(i, id)| {
            let mut bls_buf = Vec::new();
            id.bls_public_key()
                .0
                .serialize_compressed(&mut bls_buf)
                .unwrap();

            serde_json::json!({
                "index": i,
                "bls_pubkey": hex::encode(&bls_buf),
                "ed25519_public_key": hex::encode(id.ed25519_public_key().as_ref()),
                "bls_peer_id": id.peer_id().to_string(),
            })
        })
        .collect();

    let genesis_accounts: Vec<_> = generate_genesis_accounts()
        .into_iter()
        .map(|(public_key, balance)| {
            serde_json::json!({ "public_key": public_key, "balance": balance })
        })
        .collect();

    let fixtures = serde_json::json!({
        "node_count": N,
        "faulty_count": F,
        "peers": peers,
        "genesis_accounts": genesis_accounts,
    });

    println!("{}", serde_json::to_string_pretty(&fixtures)?);
    Ok(())
}

fn build_config(node_idx: usize, peers: &[PeerInfo], genesis_accounts: &[(String, u64)]) -> String {
    let node = &peers[node_idx];

    let consensus_peers = peers
        .iter()
        .map(|p| format!("    \"{}\",", p.bls_pubkey))
        .collect::<Vec<_>>()
        .join("\n");

    let p2p_validators = peers
        .iter()
        .filter(|p| p.index != node_idx)
        .map(|p| format!(
            "[[p2p.validators]]\ned25519_public_key = \"{}\"\nbls_peer_id = \"{}\"\naddress = \"127.0.0.1:{}\"\n",
            p.ed25519_pubkey, p.peer_id, p.p2p_port
        ))
        .collect::<Vec<_>>()
        .join("\n");

    let genesis_accounts_toml = genesis_accounts
        .iter()
        .map(|(pubkey, balance)| {
            format!(
                "[[consensus.genesis_accounts]]\npublic_key = \"{}\"\nbalance = {}",
                pubkey, balance
            )
        })
        .collect::<Vec<_>>()
        .join("\n\n");

    format!(
        r#"# Hellas Local Network - Node {node_idx}
# Generated with seed {seed}. Do not edit peer keys manually.

[consensus]
n = {n}
f = {f}
view_timeout = {{ secs = 5, nanos = 0 }}
leader_manager = "round_robin"
network = "local"
peers = [
{consensus_peers}
]

{genesis_accounts_toml}

[storage]
path = "./data/node{node_idx}.redb"

[p2p]
listen_addr = "127.0.0.1:{p2p_port}"
external_addr = "127.0.0.1:{p2p_port}"
total_number_peers = {n}
maximum_number_faulty_peers = {f}
bootstrap_timeout_ms = 30000
ping_interval_ms = 200
cluster_id = "hellas-local"

{p2p_validators}
[rpc]
listen_addr = "127.0.0.1:{grpc_port}"
max_concurrent_streams = 100
request_timeout_secs = 30
peer_id = {node_idx}
network = "local"
total_validators = {n}
f = {f}

[identity]
"#,
        node_idx = node_idx,
        seed = LOCAL_NETWORK_SEED,
        n = N,
        f = F,
        consensus_peers = consensus_peers,
        p2p_port = node.p2p_port,
        p2p_validators = p2p_validators,
        grpc_port = node.grpc_port,
    )
}

fn build_run_script(config_dir: &std::path::Path) -> String {
    format!(
        r#"#!/bin/bash
# Hellas Local Network Runner
set -e

LOG_DIR="${{TMPDIR:-/tmp}}/hellas-local"
mkdir -p "$LOG_DIR"

echo "Building..."
cargo build --package node --release

echo "Starting 6 validators..."
for i in $(seq 0 5); do
    ./target/release/node run --config {config_dir}/node$i.toml > "$LOG_DIR/node$i.log" 2>&1 &
    echo "  Node $i started (PID: $!)"
done

echo ""
echo "All nodes started. Logs in $LOG_DIR"
echo "Press Ctrl+C to stop."

trap "pkill -f 'node run --config'" EXIT
wait
"#,
        config_dir = config_dir.display()
    )
}

fn generate_deterministic_identities(count: usize) -> Vec<ValidatorIdentity> {
    use rand::SeedableRng;
    let mut rng = rand::rngs::StdRng::seed_from_u64(LOCAL_NETWORK_SEED);
    (0..count)
        .map(|_| ValidatorIdentity::from_bls_key(BlsSecretKey::generate(&mut rng)))
        .collect()
}

fn create_logger(level: &str) -> Logger {
    use slog::Level;
    let level = match level.to_lowercase().as_str() {
        "trace" => Level::Trace,
        "debug" => Level::Debug,
        "info" => Level::Info,
        "warn" => Level::Warning,
        "error" => Level::Error,
        _ => Level::Info,
    };

    let decorator = slog_term::TermDecorator::new().build();
    let drain = slog_term::FullFormat::new(decorator).build().fuse();
    let drain = slog::LevelFilter::new(drain, level).fuse();
    let drain = slog_async::Async::new(drain).build().fuse();
    Logger::root(drain, o!("version" => env!("CARGO_PKG_VERSION")))
}

