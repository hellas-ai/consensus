//! Example: Send a transfer from a genesis account
//!
//! Run with:
//! ```
//! # First start the local network:
//! ./node/config/run-local-network.sh
//!
//! # Then in another terminal:
//! cargo run -p hellas-sdk --example transfer
//! ```

use hellas_sdk::{Address, HellasClient, TxBuilder, Wallet};
use std::time::Duration;

/// Generate the first genesis wallet using the same seed as node config generation
fn genesis_wallet() -> Wallet {
    use rand::SeedableRng;
    let mut rng = rand::rngs::StdRng::seed_from_u64(1337);
    let key = ed25519_dalek::SigningKey::generate(&mut rng);
    Wallet::from_secret_key(&key.to_bytes())
}

#[tokio::main]
async fn main() -> hellas_sdk::Result<()> {
    println!("=== Hellas SDK Transfer Example ===\n");

    // Connect to local network (node0)
    let client = HellasClient::connect("http://localhost:50051").await?;
    println!("✓ Connected to node");

    // Load the genesis wallet (has 1,000,000,000 initial balance)
    let wallet = genesis_wallet();
    println!("Genesis address: {}", wallet.address());

    // Get current balance
    let balance = client.account().get_balance(wallet.address()).await?;
    println!("Balance: {}", balance);

    if balance == 0 {
        println!("\n⚠ No balance - is the local network running?");
        println!("Start it with: ./node/config/run-local-network.sh");
        return Ok(());
    }

    // Get current nonce
    let nonce = client.account().get_nonce(wallet.address()).await?;
    println!("Current nonce: {}", nonce);

    // Create a recipient address
    let recipient = Address::from_bytes([0x42; 32]);
    let amount = 1000;

    println!("\n--- Sending Transfer ---");
    println!("To: {}", recipient);
    println!("Amount: {}", amount);

    // Build and sign transaction
    let tx = TxBuilder::transfer(recipient, amount)
        .with_fee(1)
        .sign(&wallet, nonce)?;

    println!("Tx hash: {}", tx.tx_hash);

    // Submit and wait for confirmation
    match client.submit_and_wait(tx, Duration::from_secs(30)).await {
        Ok(receipt) => {
            println!("\n✓ Transfer confirmed!");
            println!("  Block hash: {}", receipt.block_hash);
            println!("  Block height: {}", receipt.block_height);

            // Check new balance
            let new_balance = client.account().get_balance(wallet.address()).await?;
            println!("\nNew balance: {} (was {})", new_balance, balance);
        }
        Err(e) => {
            println!("\n✗ Transfer failed: {}", e);
        }
    }

    // Query latest block
    println!("\n--- Latest Block ---");
    if let Some(block) = client.blocks().get_latest().await? {
        println!("Hash: {}", block.hash);
        println!("Height: {}", block.height);
        println!("Transactions: {}", block.transactions.len());
    }

    Ok(())
}
