//! Benchmarks for mempool block proposal latency
//!
//! Measures the time from ProposalRequest to ProposalResponse under various conditions.

use criterion::{BenchmarkId, Criterion, black_box, criterion_group, criterion_main};
use rtrb::RingBuffer;
use std::sync::{
    Arc,
    atomic::{AtomicBool, Ordering},
};
use std::time::Duration;

use consensus::{
    crypto::transaction_crypto::TxSecretKey,
    mempool::{MempoolService, ProposalRequest},
    state::{address::Address, transaction::Transaction},
    storage::store::ConsensusStore,
    validation::PendingStateWriter,
};
use tempfile::tempdir;

fn create_test_transaction(nonce: u64) -> Transaction {
    let sk = TxSecretKey::generate(&mut rand::rngs::OsRng);
    let pk = sk.public_key();
    Transaction::new_transfer(
        Address::from_public_key(&pk),
        Address::from_bytes([2u8; 32]),
        100,
        nonce,
        10,
        &sk,
    )
}

fn setup_mempool() -> (
    MempoolService,
    rtrb::Producer<Transaction>,
    rtrb::Producer<ProposalRequest>,
    rtrb::Consumer<consensus::mempool::ProposalResponse>,
    Arc<AtomicBool>,
) {
    let temp_dir = tempdir().unwrap();
    let db_path = temp_dir.path().join("bench.redb");
    let storage = Arc::new(ConsensusStore::open(&db_path).unwrap());
    let (writer, _reader) = PendingStateWriter::new(storage, 0);
    let pending_state_reader = writer.reader();

    let (tx_producer, tx_consumer) = RingBuffer::new(1024);

    let shutdown = Arc::new(AtomicBool::new(false));
    let logger = slog::Logger::root(slog::Discard, slog::o!());

    let (service, channels) = MempoolService::spawn(
        tx_consumer,
        pending_state_reader,
        Arc::clone(&shutdown),
        logger,
    );

    (
        service,
        tx_producer,
        channels.proposal_req_producer,
        channels.proposal_resp_consumer,
        shutdown,
    )
}

fn bench_proposal_empty_pool(c: &mut Criterion) {
    let mut group = c.benchmark_group("mempool_proposal");

    group.bench_function("empty_pool", |b| {
        let (mut service, _tx_prod, mut req_prod, mut resp_cons, shutdown) = setup_mempool();

        b.iter(|| {
            let request = ProposalRequest {
                view: 1,
                max_txs: 1000,
                max_bytes: 1024 * 1024,
                parent_block_hash: [0u8; 32],
            };
            req_prod.push(request).unwrap();

            // Wait for response
            let deadline = std::time::Instant::now() + Duration::from_secs(1);
            while std::time::Instant::now() < deadline {
                if let Ok(response) = resp_cons.pop() {
                    black_box(response);
                    break;
                }
                std::thread::yield_now();
            }
        });

        shutdown.store(true, Ordering::Release);
        service.shutdown();
    });

    group.finish();
}

fn bench_proposal_with_transactions(c: &mut Criterion) {
    let mut group = c.benchmark_group("mempool_proposal");

    for tx_count in [100, 1000, 10000].iter() {
        group.bench_with_input(
            BenchmarkId::new("pool_size", tx_count),
            tx_count,
            |b, &count| {
                let (mut service, mut tx_prod, mut req_prod, mut resp_cons, shutdown) =
                    setup_mempool();

                // Pre-populate pool
                for i in 0..count {
                    let tx = create_test_transaction(i as u64);
                    let _ = tx_prod.push(tx);
                }

                // Let mempool process transactions
                std::thread::sleep(Duration::from_millis(100));

                b.iter(|| {
                    let request = ProposalRequest {
                        view: 1,
                        max_txs: 1000,
                        max_bytes: 1024 * 1024,
                        parent_block_hash: [0u8; 32],
                    };
                    req_prod.push(request).unwrap();

                    let deadline = std::time::Instant::now() + Duration::from_secs(1);
                    while std::time::Instant::now() < deadline {
                        if let Ok(response) = resp_cons.pop() {
                            black_box(response);
                            break;
                        }
                        std::thread::yield_now();
                    }
                });

                shutdown.store(true, Ordering::Release);
                service.shutdown();
            },
        );
    }

    group.finish();
}

fn bench_proposal_varying_max_txs(c: &mut Criterion) {
    let mut group = c.benchmark_group("mempool_proposal_max_txs");

    // Pool with 10k transactions, vary max_txs selection
    for max_txs in [100, 500, 1000, 2000].iter() {
        group.bench_with_input(BenchmarkId::new("select", max_txs), max_txs, |b, &max| {
            let (mut service, mut tx_prod, mut req_prod, mut resp_cons, shutdown) = setup_mempool();

            // Pre-populate with 10k transactions
            for i in 0..10000 {
                let tx = create_test_transaction(i as u64);
                let _ = tx_prod.push(tx);
            }
            std::thread::sleep(Duration::from_millis(200));

            b.iter(|| {
                let request = ProposalRequest {
                    view: 1,
                    max_txs: max,
                    max_bytes: 10 * 1024 * 1024, // 10MB to not constrain by bytes
                    parent_block_hash: [0u8; 32],
                };
                req_prod.push(request).unwrap();

                let deadline = std::time::Instant::now() + Duration::from_secs(1);
                while std::time::Instant::now() < deadline {
                    if let Ok(response) = resp_cons.pop() {
                        black_box(response);
                        break;
                    }
                    std::thread::yield_now();
                }
            });

            shutdown.store(true, Ordering::Release);
            service.shutdown();
        });
    }

    group.finish();
}

fn bench_proposal_large_pool(c: &mut Criterion) {
    let mut group = c.benchmark_group("mempool_proposal_large");

    // Increase sample size time for larger operations
    group.measurement_time(Duration::from_secs(10));

    // Pool with 100k transactions, vary selection size
    for max_txs in [10_000, 20_000, 50_000].iter() {
        group.bench_with_input(
            BenchmarkId::new("select_from_100k", max_txs),
            max_txs,
            |b, &max| {
                let (mut service, mut tx_prod, mut req_prod, mut resp_cons, shutdown) =
                    setup_mempool();

                // Pre-populate with 100k transactions
                for i in 0..100_000 {
                    let tx = create_test_transaction(i as u64);
                    let _ = tx_prod.push(tx);
                }
                // Give mempool more time to process 100k txs
                std::thread::sleep(Duration::from_millis(500));

                b.iter(|| {
                    let request = ProposalRequest {
                        view: 1,
                        max_txs: max,
                        max_bytes: 100 * 1024 * 1024, // 100MB to not constrain by bytes
                        parent_block_hash: [0u8; 32],
                    };
                    req_prod.push(request).unwrap();

                    let deadline = std::time::Instant::now() + Duration::from_secs(5);
                    while std::time::Instant::now() < deadline {
                        if let Ok(response) = resp_cons.pop() {
                            black_box(response);
                            break;
                        }
                        std::thread::yield_now();
                    }
                });

                shutdown.store(true, Ordering::Release);
                service.shutdown();
            },
        );
    }

    group.finish();
}

criterion_group!(
    benches,
    bench_proposal_empty_pool,
    bench_proposal_with_transactions,
    bench_proposal_varying_max_txs,
    bench_proposal_large_pool,
);
criterion_main!(benches);
