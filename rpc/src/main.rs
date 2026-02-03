//! RPC Node binary.

use std::path::PathBuf;

use anyhow::{Context, Result};
use clap::Parser;
use rand::rngs::OsRng;
use slog::{Drain, Logger};

use rpc::{RpcConfig, RpcIdentity, RpcNode};

/// Hellas RPC Node - syncs finalized blocks and serves read-only queries.
#[derive(Parser, Debug)]
#[command(name = "rpc-node")]
#[command(about = "Hellas RPC Node for block sync and queries")]
struct Args {
    /// Path to configuration file (TOML).
    #[arg(short, long, default_value = "rpc-config.toml")]
    config: PathBuf,

    /// Override gRPC bind address.
    #[arg(long)]
    grpc_addr: Option<String>,

    /// Override P2P listen address.
    #[arg(long)]
    p2p_addr: Option<String>,

    /// Override data directory.
    #[arg(long)]
    data_dir: Option<PathBuf>,
}

fn create_logger() -> Logger {
    let decorator = slog_term::TermDecorator::new().build();
    let drain = slog_term::FullFormat::new(decorator).build().fuse();
    let drain = slog_async::Async::new(drain).build().fuse();
    Logger::root(drain, slog::o!())
}

#[tokio::main]
async fn main() -> Result<()> {
    let args = Args::parse();
    let logger = create_logger();

    slog::info!(logger, "Starting Hellas RPC Node";
        "config" => %args.config.display()
    );

    // Load configuration
    let mut config = if args.config.exists() {
        RpcConfig::load(args.config.to_str().unwrap()).context("Failed to load configuration")?
    } else {
        slog::warn!(logger, "Config file not found, using defaults");
        RpcConfig::default()
    };

    // Apply CLI overrides
    if let Some(addr) = args.grpc_addr {
        config.grpc_addr = addr.parse().context("Invalid gRPC address")?;
    }
    if let Some(addr) = args.p2p_addr {
        config.p2p_addr = addr.parse().context("Invalid P2P address")?;
    }
    if let Some(dir) = args.data_dir {
        config.data_dir = dir;
    }

    // Generate or load identity
    // TODO: Support loading from file
    let identity = RpcIdentity::generate(&mut OsRng);
    slog::info!(logger, "Generated RPC identity";
        "public_key" => hex::encode(identity.public_key_bytes())
    );

    // Create and run node (N=6 validators, F=1 faulty)
    let mut node = RpcNode::<6, 1>::new(config, identity, logger.clone())?;

    // Handle Ctrl+C - get the shutdown notify so we can signal from the handler
    let node_shutdown = node.get_shutdown_signal();
    tokio::spawn(async move {
        tokio::signal::ctrl_c().await.ok();
        node_shutdown
            .0
            .store(true, std::sync::atomic::Ordering::Release);
        node_shutdown.1.notify_waiters();
    });

    node.run().await?;

    slog::info!(logger, "RPC node stopped");
    Ok(())
}
