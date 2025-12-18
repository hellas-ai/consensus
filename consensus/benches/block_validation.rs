//! Benchmarks for block validation performance
//!
//! Measures the time to validate blocks with different:
//! - Block sizes (10, 100, 1000, 10000 transactions)
//! - Transaction types (Transfer, Mint, CreateAccount, mixed)
//! - Account access patterns (cold DB lookups vs warm cache)
//!
//! This helps determine if synchronous validation is acceptable
//! for the consensus critical path.

use criterion::{BenchmarkId, Criterion, black_box, criterion_group, criterion_main};
use std::sync::Arc;

use consensus::{
    crypto::{aggregated::BlsSecretKey, transaction_crypto::TxSecretKey},
    state::{
        account::Account,
        address::Address,
        block::{Block, BlockHeader},
        transaction::Transaction,
    },
    storage::store::ConsensusStore,
    validation::{BlockValidator, PendingStateWriter},
};
use tempfile::tempdir;

/// Generate a keypair and return (secret_key, address)
fn gen_keypair() -> (TxSecretKey, Address) {
    let sk = TxSecretKey::generate(&mut rand::rngs::OsRng);
    let pk = sk.public_key();
    let addr = Address::from_public_key(&pk);
    (sk, addr)
}

/// Create a funded account in the database and return its keypair
fn create_funded_account(store: &ConsensusStore, balance: u64) -> (TxSecretKey, Address) {
    let sk = TxSecretKey::generate(&mut rand::rngs::OsRng);
    let pk = sk.public_key();
    let addr = Address::from_public_key(&pk);
    let account = Account::new(pk, balance, 0);
    store.put_account(&account).unwrap();
    (sk, addr)
}

/// Create a block with transfer transactions from multiple senders
fn create_transfer_block(senders: &[(TxSecretKey, Address)], num_txs: usize) -> Block {
    let recipient = Address::from_bytes([0xFFu8; 32]);

    let transactions: Vec<Arc<Transaction>> = senders
        .iter()
        .cycle()
        .take(num_txs)
        .enumerate()
        .map(|(i, (sk, sender))| {
            // Calculate nonce based on how many times this sender has been used
            let nonce = (i / senders.len()) as u64;

            Arc::new(Transaction::new_transfer(
                *sender, recipient, 10, // small amount
                nonce, 1, // small fee
                sk,
            ))
        })
        .collect();

    create_block_with_transactions(transactions)
}

/// Create a block with mint transactions
fn create_mint_block(num_txs: usize) -> Block {
    let transactions: Vec<Arc<Transaction>> = (0..num_txs)
        .map(|i| {
            let (sk, sender) = gen_keypair();
            let recipient = Address::from_bytes([0xFFu8; 32]);

            Arc::new(Transaction::new_mint(
                sender, recipient, 100,
                i as u64, // Each sender has unique nonce starting at their index
                &sk,
            ))
        })
        .collect();

    create_block_with_transactions(transactions)
}

/// Create a block with mixed transaction types (70% transfer, 20% mint, 10% create)
fn create_mixed_block(senders: &[(TxSecretKey, Address)], num_txs: usize) -> Block {
    let recipient = Address::from_bytes([0xFFu8; 32]);

    let transactions: Vec<Arc<Transaction>> = (0..num_txs)
        .map(|i| {
            let tx_type = i % 10;

            if tx_type < 7 {
                // 70% transfers
                let sender_index = i % senders.len();
                let (sk, sender) = &senders[sender_index];
                let nonce = (i / senders.len()) as u64;

                Arc::new(Transaction::new_transfer(
                    *sender, recipient, 10, nonce, 1, sk,
                ))
            } else if tx_type < 9 {
                // 20% mints
                let (sk, sender) = gen_keypair();
                Arc::new(Transaction::new_mint(sender, recipient, 100, 0, &sk))
            } else {
                // 10% create account
                let (sk, sender) = &senders[i % senders.len()];
                let new_addr = Address::from_bytes([(i & 0xFF) as u8; 32]);
                let nonce = (i / senders.len()) as u64;

                Arc::new(Transaction::new_create_account(
                    *sender, new_addr, nonce, 1, sk,
                ))
            }
        })
        .collect();

    create_block_with_transactions(transactions)
}

/// Create a block from a list of transactions
fn create_block_with_transactions(transactions: Vec<Arc<Transaction>>) -> Block {
    let leader_sk = BlsSecretKey::generate(&mut rand::thread_rng());
    let leader_pk = leader_sk.public_key();
    let leader_id = leader_pk.to_peer_id();

    let header = BlockHeader {
        view: 1,
        parent_block_hash: [0u8; 32],
        timestamp: 0,
    };

    // Create a dummy signature first, then sign properly
    let dummy_sig = leader_sk.sign(&[0u8; 32]);

    let mut block = Block {
        header,
        transactions,
        leader: leader_id,
        leader_signature: dummy_sig,
        hash: None,
        is_finalized: false,
        height: 1,
    };

    // Sign the block
    let block_hash = block.get_hash();
    block.leader_signature = leader_sk.sign(&block_hash);

    block
}

/// Setup a validator with funded accounts in the database
fn setup_validator_with_accounts(
    num_accounts: usize,
    balance_per_account: u64,
) -> (
    BlockValidator,
    Vec<(TxSecretKey, Address)>,
    tempfile::TempDir,
) {
    let temp_dir = tempdir().unwrap();
    let db_path = temp_dir.path().join("bench.redb");
    let store = Arc::new(ConsensusStore::open(&db_path).unwrap());

    // Create funded accounts
    let accounts: Vec<_> = (0..num_accounts)
        .map(|_| create_funded_account(&store, balance_per_account))
        .collect();

    // Create validator
    let (writer, reader) = PendingStateWriter::new(store, 0);
    drop(writer); // We don't need the writer for benchmarks
    let validator = BlockValidator::new(reader);

    (validator, accounts, temp_dir)
}

fn bench_validate_transfers(c: &mut Criterion) {
    let mut group = c.benchmark_group("block_validation/transfers");
    group.sample_size(50); // Reduce sample size for large blocks

    for num_txs in [10, 100, 1000, 10000] {
        // Use enough accounts to spread the load (avoid nonce contention)
        let num_accounts = (num_txs / 10).max(10);
        let balance_per_account = 1_000_000_000; // Enough for many transactions

        group.bench_with_input(BenchmarkId::new("txs", num_txs), &num_txs, |b, &n| {
            let (validator, accounts, _temp_dir) =
                setup_validator_with_accounts(num_accounts, balance_per_account);

            // Create the block once (outside the benchmark loop)
            let block = create_transfer_block(&accounts, n);

            b.iter(|| {
                // Note: This will fail after first iteration due to nonce/balance,
                // but we're measuring the validation time, not success
                let result = validator.validate_block(black_box(&block));
                black_box(result)
            });
        });
    }
    group.finish();
}

fn bench_validate_mints(c: &mut Criterion) {
    let mut group = c.benchmark_group("block_validation/mints");
    group.sample_size(50);

    for num_txs in [10, 100, 1000, 10000] {
        group.bench_with_input(BenchmarkId::new("txs", num_txs), &num_txs, |b, &n| {
            // Mints don't require pre-funded accounts
            let temp_dir = tempdir().unwrap();
            let db_path = temp_dir.path().join("bench.redb");
            let store = Arc::new(ConsensusStore::open(&db_path).unwrap());
            let (writer, reader) = PendingStateWriter::new(store, 0);
            drop(writer);
            let validator = BlockValidator::new(reader);

            let block = create_mint_block(n);

            b.iter(|| {
                let result = validator.validate_block(black_box(&block));
                black_box(result)
            });
        });
    }
    group.finish();
}

fn bench_validate_mixed(c: &mut Criterion) {
    let mut group = c.benchmark_group("block_validation/mixed");
    group.sample_size(50);

    for num_txs in [10, 100, 1000, 10000] {
        let num_accounts = (num_txs / 10).max(10);
        let balance_per_account = 1_000_000_000;

        group.bench_with_input(BenchmarkId::new("txs", num_txs), &num_txs, |b, &n| {
            let (validator, accounts, _temp_dir) =
                setup_validator_with_accounts(num_accounts, balance_per_account);

            let block = create_mixed_block(&accounts, n);

            b.iter(|| {
                let result = validator.validate_block(black_box(&block));
                black_box(result)
            });
        });
    }
    group.finish();
}

fn bench_signature_verification(c: &mut Criterion) {
    let mut group = c.benchmark_group("block_validation/signatures_only");

    for num_txs in [10, 100, 1000] {
        group.bench_with_input(BenchmarkId::new("txs", num_txs), &num_txs, |b, &n| {
            // Create transactions
            let transactions: Vec<Transaction> = (0..n)
                .map(|i| {
                    let (sk, sender) = gen_keypair();
                    let recipient = Address::from_bytes([0xFFu8; 32]);
                    Transaction::new_transfer(sender, recipient, 100, i as u64, 10, &sk)
                })
                .collect();

            b.iter(|| {
                let mut valid_count = 0;
                for tx in &transactions {
                    if tx.verify() {
                        valid_count += 1;
                    }
                }
                black_box(valid_count)
            });
        });
    }
    group.finish();
}

fn bench_single_signature(c: &mut Criterion) {
    c.bench_function("signature/single_verify", |b| {
        let (sk, sender) = gen_keypair();
        let recipient = Address::from_bytes([0xFFu8; 32]);
        let tx = Transaction::new_transfer(sender, recipient, 100, 0, 10, &sk);

        b.iter(|| black_box(tx.verify()));
    });
}

fn bench_account_lookup(c: &mut Criterion) {
    let mut group = c.benchmark_group("block_validation/account_lookup");

    // Cold lookup (first access from DB)
    group.bench_function("cold_single", |b| {
        let temp_dir = tempdir().unwrap();
        let db_path = temp_dir.path().join("bench.redb");
        let store = Arc::new(ConsensusStore::open(&db_path).unwrap());

        let (_, addr) = create_funded_account(&store, 1_000_000);

        let (writer, reader) = PendingStateWriter::new(Arc::clone(&store), 0);
        drop(writer);

        b.iter(|| {
            let account = reader.get_account(black_box(&addr));
            black_box(account)
        });
    });

    group.finish();
}

fn bench_batch_signature_verification(c: &mut Criterion) {
    let mut group = c.benchmark_group("block_validation/batch_signatures");
    group.sample_size(50);

    for num_txs in [10, 100, 1000, 10000] {
        // Create a block with valid signatures
        let transactions: Vec<Arc<Transaction>> = (0..num_txs)
            .map(|i| {
                let (sk, sender) = gen_keypair();
                let recipient = Address::from_bytes([0xFFu8; 32]);
                Arc::new(Transaction::new_transfer(
                    sender, recipient, 100, i as u64, 10, &sk,
                ))
            })
            .collect();

        let block = create_block_with_transactions(transactions);

        group.bench_with_input(BenchmarkId::new("batch", num_txs), &num_txs, |b, _| {
            b.iter(|| black_box(block.verify_block_txs_signatures()));
        });

        // Also benchmark individual verification for comparison
        group.bench_with_input(BenchmarkId::new("individual", num_txs), &num_txs, |b, _| {
            b.iter(|| {
                let mut valid = true;
                for tx in &block.transactions {
                    if !tx.verify() {
                        valid = false;
                        break;
                    }
                }
                black_box(valid)
            });
        });
    }
    group.finish();
}

criterion_group!(
    benches,
    bench_validate_transfers,
    bench_validate_mints,
    bench_validate_mixed,
    bench_signature_verification,
    bench_single_signature,
    bench_account_lookup,
    bench_batch_signature_verification,
);
criterion_main!(benches);
