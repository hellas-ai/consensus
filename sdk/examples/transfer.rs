//! Example: Send a transfer transaction
//!
//! Run with:
//! ```
//! cargo run -p hellas-sdk --example transfer
//! ```

use hellas_sdk::{Address, HellasClient, TxBuilder, Wallet};
use std::time::Duration;

#[tokio::main]
async fn main() -> hellas_sdk::Result<()> {
    // Connect to local network
    let client = HellasClient::connect("http://localhost:50051").await?;
    println!("Connected to node");

    // Generate a new wallet (or load existing)
    let wallet = Wallet::generate();
    println!("Sender address: {}", wallet.address());

    // Get current balance
    let balance = client.account().get_balance(wallet.address()).await?;
    println!("Balance: {}", balance);

    // For testing: use a mint transaction to give ourselves some tokens
    let nonce = client.account().get_nonce(wallet.address()).await?;
    println!("Current nonce: {}", nonce);

    // Build a mint transaction (testnet only)
    let mint_tx = TxBuilder::mint(*wallet.address(), 1_000_000).sign(&wallet, nonce)?;

    println!("Submitting mint transaction: {}", mint_tx.tx_hash);

    // Submit and wait for confirmation
    match client
        .submit_and_wait(mint_tx, Duration::from_secs(30))
        .await
    {
        Ok(receipt) => {
            println!("✓ Minted tokens!");
            println!("  Block hash: {}", receipt.block_hash);
            println!("  Block height: {}", receipt.block_height);
        }
        Err(e) => {
            println!("Mint failed (expected if not genesis account): {}", e);
        }
    }

    // Now do a transfer
    let recipient = Address::from_bytes([0x42; 32]);
    println!("\nTransferring 100 to {}", recipient);

    let nonce = client.account().get_nonce(wallet.address()).await?;
    let transfer_tx = TxBuilder::transfer(recipient, 100)
        .with_fee(1)
        .sign(&wallet, nonce)?;

    println!("Submitting transfer: {}", transfer_tx.tx_hash);

    match client
        .submit_and_wait(transfer_tx, Duration::from_secs(30))
        .await
    {
        Ok(receipt) => {
            println!("✓ Transfer confirmed!");
            println!("  Block hash: {}", receipt.block_hash);
            println!("  Block height: {}", receipt.block_height);
        }
        Err(e) => {
            println!("Transfer failed: {}", e);
        }
    }

    // Query blocks
    println!("\nQuerying latest block...");
    if let Some(block) = client.blocks().get_latest().await? {
        println!("Latest block:");
        println!("  Hash: {}", block.hash);
        println!("  Height: {}", block.height);
        println!("  Transactions: {}", block.transactions.len());
    }

    Ok(())
}
