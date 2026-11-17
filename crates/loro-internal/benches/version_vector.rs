use criterion::{criterion_group, criterion_main, BatchSize, Criterion};
use loro_internal::{version::ImVersionVector, LoroDoc, PeerID};
use rand::{rngs::StdRng, Rng, SeedableRng};
use std::hint::black_box;

/// Random peer ids, like the ones Loro assigns.
fn random_peers(count: usize) -> Vec<PeerID> {
    let mut rng = StdRng::seed_from_u64(count as u64);
    (0..count).map(|_| rng.gen()).collect()
}

fn im_version_vector(c: &mut Criterion) {
    let mut b = c.benchmark_group("ImVersionVector");
    for count in [8usize, 256, 4096] {
        let peers = random_peers(count);
        let base: ImVersionVector = {
            let mut vv = ImVersionVector::new();
            for &peer in &peers {
                vv.insert(peer, 1);
            }
            vv
        };
        // The DAG caches one vector per change: its parent's, cloned, with one
        // entry changed.
        b.bench_function(format!("clone and update x1000, {count} peers"), |b| {
            b.iter(|| {
                let mut versions = Vec::with_capacity(1000);
                let mut vv = base.clone();
                for i in 0..1000 {
                    vv.insert(peers[i % count], i as i32 + 2);
                    versions.push(vv.clone());
                }
                versions
            })
        });
        b.bench_function(format!("get x1000, {count} peers"), |b| {
            b.iter(|| {
                let mut sum = 0;
                for i in 0..1000 {
                    sum += base.get(&peers[i % count]).copied().unwrap_or(0);
                }
                sum
            })
        });
        b.bench_function(format!("iter, {count} peers"), |b| {
            b.iter(|| base.iter().map(|(_, &c)| c).sum::<i32>())
        });
        let mut newer = base.clone();
        newer.insert(peers[0], 5);
        b.bench_function(format!("partial_cmp, {count} peers"), |b| {
            b.iter(|| black_box(&base).partial_cmp(black_box(&newer)))
        });
        b.bench_function(format!("merge, {count} peers"), |b| {
            b.iter(|| {
                let mut vv = base.clone();
                vv.merge(&newer);
                vv
            })
        });
    }
    b.finish();
}

/// Builds a history in which `peers` peers take turns, so every change is
/// its own DAG node, and measures computing the version vectors of past
/// versions in a freshly imported document (whose vector cache is empty).
fn dag_version_vectors(c: &mut Criterion) {
    let mut b = c.benchmark_group("DAG version vectors");
    b.sample_size(10);
    for peers in [8u64, 256] {
        let doc = LoroDoc::new_auto_commit();
        let text = doc.get_text("text");
        let mut versions = vec![];
        for i in 0..5000u64 {
            doc.set_peer_id(i % peers).unwrap();
            text.insert_utf8(0, "a").unwrap();
            doc.commit_then_renew();
            if i % 50 == 0 {
                versions.push(doc.oplog_frontiers());
            }
        }
        let snapshot = doc
            .export(loro_internal::loro::ExportMode::Snapshot)
            .unwrap();
        b.bench_function(
            format!("frontiers_to_vv x100, 5000 changes, {peers} peers"),
            |b| {
                b.iter_batched(
                    || {
                        let doc = LoroDoc::new();
                        doc.import(&snapshot).unwrap();
                        doc
                    },
                    |doc| {
                        for f in versions.iter().rev() {
                            black_box(doc.frontiers_to_vv(f));
                        }
                        doc
                    },
                    BatchSize::PerIteration,
                )
            },
        );
    }
    b.finish();
}

criterion_group!(benches, im_version_vector, dag_version_vectors);
criterion_main!(benches);
