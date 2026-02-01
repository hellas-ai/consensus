use anyhow::{Context, Result};
use clap::{Args, Parser, Subcommand};
use hellas_sdk::{Address, Block, Hash, HellasClient, TxBuilder, TxStatus, Wallet};
use std::time::Duration;

#[derive(Parser)]
#[command(name = "hellas", about = "Hellas blockchain CLI", version)]
struct Cli {
    /// gRPC endpoint of a Hellas node
    #[arg(long, short, global = true, default_value = "http://localhost:50051", env = "HELLAS_ENDPOINT")]
    endpoint: String,

    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Wallet key management (offline)
    Wallet {
        #[command(subcommand)]
        command: WalletCommand,
    },
    /// Account queries
    Account {
        #[command(subcommand)]
        command: AccountCommand,
    },
    /// Block queries
    Block {
        #[command(subcommand)]
        command: BlockCommand,
    },
    /// Transaction operations
    Tx {
        #[command(subcommand)]
        command: TxCommand,
    },
}

#[derive(Subcommand)]
enum WalletCommand {
    /// Generate a new random wallet
    Generate,
    /// Show address for a given secret key
    Inspect {
        /// Secret key as hex (64 chars)
        #[arg(long, env = "HELLAS_SECRET_KEY")]
        secret_key: String,
    },
}

#[derive(Subcommand)]
enum AccountCommand {
    /// Get account info (balance, nonce)
    Get {
        address: String,
        /// Show pending (M-notarized) state instead of finalized
        #[arg(long)]
        pending: bool,
    },
    /// Get account balance
    Balance { address: String },
    /// Get next valid nonce
    Nonce { address: String },
}

#[derive(Subcommand)]
enum BlockCommand {
    /// Get block by hash
    Get { hash: String },
    /// Get block by height
    Height { height: u64 },
    /// Get the latest finalized block
    Latest,
    /// Get a range of blocks
    Range {
        from: u64,
        #[arg(long, default_value = "10")]
        limit: u32,
    },
}

/// Common arguments for transaction submission
#[derive(Args)]
struct TxArgs {
    /// Sender secret key (hex)
    #[arg(long, env = "HELLAS_SECRET_KEY")]
    secret_key: String,
    /// Nonce override (auto-fetched if omitted)
    #[arg(long)]
    nonce: Option<u64>,
    /// Wait for finalization (timeout in seconds)
    #[arg(long)]
    wait: Option<u64>,
    /// Transaction fee
    #[arg(long, default_value = "0")]
    fee: u64,
}

#[derive(Subcommand)]
enum TxCommand {
    /// Send a transfer
    Transfer {
        #[arg(long)]
        to: String,
        #[arg(long)]
        amount: u64,
        #[command(flatten)]
        args: TxArgs,
    },
    /// Mint tokens (testnet)
    Mint {
        #[arg(long)]
        to: String,
        #[arg(long)]
        amount: u64,
        #[command(flatten)]
        args: TxArgs,
    },
    /// Burn tokens
    Burn {
        #[arg(long)]
        address: String,
        #[arg(long)]
        amount: u64,
        #[command(flatten)]
        args: TxArgs,
    },
    /// Create a new account
    CreateAccount {
        #[arg(long)]
        address: String,
        #[command(flatten)]
        args: TxArgs,
    },
    /// Get transaction status
    Status { hash: String },
}

fn parse_address(hex: &str) -> Result<Address> {
    Address::from_hex(hex).context("invalid address")
}

fn parse_hash(hex: &str) -> Result<Hash> {
    Hash::from_hex(hex).context("invalid hash")
}

fn print_block(b: &Block) {
    println!("hash:         {}", b.hash);
    println!("height:       {}", b.height);
    println!("view:         {}", b.view);
    println!("parent_hash:  {}", b.parent_hash);
    println!("timestamp:    {}", b.timestamp);
    println!("leader_id:    {}", b.leader_id);
    println!("transactions: {}", b.transactions.len());
}

#[tokio::main]
async fn main() -> Result<()> {
    let cli = Cli::parse();
    match cli.command {
        Command::Wallet { command } => handle_wallet(command),
        Command::Account { command } => handle_account(&cli.endpoint, command).await,
        Command::Block { command } => handle_block(&cli.endpoint, command).await,
        Command::Tx { command } => handle_tx(&cli.endpoint, command).await,
    }
}

fn handle_wallet(cmd: WalletCommand) -> Result<()> {
    match cmd {
        WalletCommand::Generate => {
            let w = Wallet::generate();
            println!("address:    {}", w.address());
            println!("secret_key: {}", w.to_secret_hex());
        }
        WalletCommand::Inspect { secret_key } => {
            let w = Wallet::from_hex(&secret_key).context("invalid secret key")?;
            println!("address: {}", w.address());
        }
    }
    Ok(())
}

async fn handle_account(endpoint: &str, cmd: AccountCommand) -> Result<()> {
    let client = HellasClient::connect(endpoint).await?;
    match cmd {
        AccountCommand::Get { address, pending } => {
            let addr = parse_address(&address)?;
            let acc = if pending {
                client.account().get_pending(&addr).await?
            } else {
                client.account().get(&addr).await?
            };
            let acc = acc.context("account not found")?;
            println!("address: {}", acc.address);
            println!("balance: {}", acc.balance);
            println!("nonce:   {}", acc.nonce);
        }
        AccountCommand::Balance { address } => {
            println!("{}", client.account().get_balance(&parse_address(&address)?).await?);
        }
        AccountCommand::Nonce { address } => {
            println!("{}", client.account().get_nonce(&parse_address(&address)?).await?);
        }
    }
    Ok(())
}

async fn handle_block(endpoint: &str, cmd: BlockCommand) -> Result<()> {
    let client = HellasClient::connect(endpoint).await?;
    match cmd {
        BlockCommand::Get { hash } => {
            let block = client.blocks().get(&parse_hash(&hash)?).await?.context("block not found")?;
            print_block(&block);
        }
        BlockCommand::Height { height } => {
            let block = client.blocks().get_by_height(height).await?.context("block not found")?;
            print_block(&block);
        }
        BlockCommand::Latest => {
            let block = client.blocks().get_latest().await?.context("no blocks yet")?;
            print_block(&block);
        }
        BlockCommand::Range { from, limit } => {
            for (i, block) in client.blocks().get_range(from, limit).await?.iter().enumerate() {
                if i > 0 { println!(); }
                print_block(block);
            }
        }
    }
    Ok(())
}

async fn handle_tx(endpoint: &str, cmd: TxCommand) -> Result<()> {
    if let TxCommand::Status { hash } = cmd {
        let client = HellasClient::connect(endpoint).await?;
        match client.get_transaction_status(&parse_hash(&hash)?).await? {
            TxStatus::Pending => println!("status: pending"),
            TxStatus::NotFound => println!("status: not found"),
            TxStatus::MNotarized { block_hash, block_height } => {
                println!("status:       m-notarized");
                println!("block_hash:   {block_hash}");
                println!("block_height: {block_height}");
            }
            TxStatus::Finalized { block_hash, block_height } => {
                println!("status:       finalized");
                println!("block_hash:   {block_hash}");
                println!("block_height: {block_height}");
            }
        }
        return Ok(());
    }

    let (args, builder) = match cmd {
        TxCommand::Transfer { to, amount, args } => {
            (args, TxBuilder::transfer(parse_address(&to)?, amount))
        }
        TxCommand::Mint { to, amount, args } => {
            (args, TxBuilder::mint(parse_address(&to)?, amount))
        }
        TxCommand::Burn { address, amount, args } => {
            (args, TxBuilder::burn(parse_address(&address)?, amount))
        }
        TxCommand::CreateAccount { address, args } => {
            (args, TxBuilder::create_account(parse_address(&address)?))
        }
        TxCommand::Status { .. } => unreachable!(),
    };

    let wallet = Wallet::from_hex(&args.secret_key).context("invalid secret key")?;
    let client = HellasClient::connect(endpoint).await?;
    let nonce = match args.nonce {
        Some(n) => n,
        None => client.account().get_nonce(wallet.address()).await?,
    };
    let tx = builder.with_fee(args.fee).sign(&wallet, nonce)?;

    match args.wait {
        Some(secs) => {
            let r = client
                .submit_and_wait(tx, Duration::from_secs(secs))
                .await?;
            println!("tx_hash:      {}", r.tx_hash);
            println!("block_hash:   {}", r.block_hash);
            println!("block_height: {}", r.block_height);
        }
        None => println!("tx_hash: {}", client.submit_transaction(tx).await?),
    }
    Ok(())
}
