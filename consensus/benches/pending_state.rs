use consensus::crypto::transaction_crypto::TxSecretKey;
use consensus::state::address::Address;
use consensus::storage::store::ConsensusStore;
use consensus::validation::{PendingStateWriter, StateDiff};
use criterion::{BenchmarkId, Criterion, black_box, criterion_group, criterion_main};
use std::sync::Arc;

fn bench_add_m_notarized_diff(c: &mut Criterion) {
    let mut group = c.benchmark_group("add_m_notarized_diff");

    for num_accounts in [1, 10, 100] {
        group.bench_with_input(
            BenchmarkId::new("accounts", num_accounts),
            &num_accounts,
            |b, &n| {
                // Setup OUTSIDE the benchmark loop
                let path =
                    std::env::temp_dir().join(format!("bench_{}.redb", rand::random::<u64>()));
                let store = Arc::new(ConsensusStore::open(&path).unwrap());
                let (mut writer, _) = PendingStateWriter::new(store, 0);

                let addresses: Vec<_> = (0..n)
                    .map(|_| {
                        let sk = TxSecretKey::generate(&mut rand::rngs::OsRng);
                        Address::from_public_key(&sk.public_key())
                    })
                    .collect();

                // Pre-create diffs to avoid allocation in benchmark
                let mut view = 1u64;

                b.iter(|| {
                    let mut diff = StateDiff::new();
                    for addr in &addresses {
                        diff.add_balance_change(*addr, 100, 0);
                    }
                    writer.add_m_notarized_diff(view, Arc::new(diff));
                    view += 1;
                });

                // Cleanup
                let _ = std::fs::remove_file(&path);
            },
        );
    }
    group.finish();
}

fn bench_get_account(c: &mut Criterion) {
    let mut group = c.benchmark_group("get_account");

    for pending_views in [1, 5, 10, 20] {
        group.bench_with_input(
            BenchmarkId::new("pending_views", pending_views),
            &pending_views,
            |b, &n| {
                // Setup OUTSIDE the benchmark loop
                let path =
                    std::env::temp_dir().join(format!("bench_{}.redb", rand::random::<u64>()));
                let store = Arc::new(ConsensusStore::open(&path).unwrap());
                let (mut writer, reader) = PendingStateWriter::new(store, 0);

                let sk = TxSecretKey::generate(&mut rand::rngs::OsRng);
                let addr = Address::from_public_key(&sk.public_key());

                // Setup pending views BEFORE benchmark
                for v in 1..=n {
                    let mut diff = StateDiff::new();
                    if v == 1 {
                        diff.add_created_account(addr, 1000);
                    } else {
                        diff.add_balance_change(addr, 100, v as u64 - 1);
                    }
                    writer.add_m_notarized_diff(v as u64, Arc::new(diff));
                }

                // Only benchmark the read
                b.iter(|| {
                    black_box(reader.get_account(&addr));
                });

                let _ = std::fs::remove_file(&path);
            },
        );
    }
    group.finish();
}

fn bench_snapshot_load(c: &mut Criterion) {
    let path = std::env::temp_dir().join(format!("bench_{}.redb", rand::random::<u64>()));
    let store = Arc::new(ConsensusStore::open(&path).unwrap());
    let (_, reader) = PendingStateWriter::new(store, 0);

    c.bench_function("snapshot_load", |b| {
        b.iter(|| {
            black_box(reader.load());
        });
    });

    let _ = std::fs::remove_file(&path);
}

criterion_group!(
    benches,
    bench_add_m_notarized_diff,
    bench_get_account,
    bench_snapshot_load
);
criterion_main!(benches);
