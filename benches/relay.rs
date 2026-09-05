//! What the relay path costs per message.
//!
//! Neither link had a benchmark, so the costs on them were argued from reading
//! the code rather than measured. These cover the parts that are pure
//! computation: reading a subject, finding the edge that owns an entity, and
//! framing a message. Nothing here opens a socket or needs a broker, so what
//! it measures is the work the transports wrap rather than the transports.
//!
//! The relay path is per message and per payload, so these are all costs paid
//! at the tick rate multiplied by the number of clients.

use std::hint::black_box;

use criterion::{
    BatchSize, BenchmarkId, Criterion, Throughput, criterion_group, criterion_main,
};
use umwelt::internals::edge::Framer;
use umwelt::internals::region::subjects;
use umwelt::net::{EdgeName, Edges};
use umwelt::{EntityId, RegionId};

const REGION: RegionId = RegionId::from_raw(7);

fn edge_name(n: usize) -> EdgeName {
    EdgeName::new(format!("edge-{n:04}")).expect("a valid name")
}

fn entity(n: usize) -> EntityId {
    EntityId::from_raw(n as u32)
}

/// Reading the sender out of a command subject and the region out of a state
/// subject. One of these runs on every message that crosses the link.
fn bench_subjects(c: &mut Criterion) {
    let mut group = c.benchmark_group("relay/subject");
    let command = subjects::command(REGION, &edge_name(3));
    let state = subjects::state(REGION, &edge_name(3));

    group.bench_function("sender", |b| {
        b.iter(|| subjects::sender(black_box(command.as_str())).expect("parses"))
    });
    group.bench_function("origin", |b| {
        b.iter(|| subjects::origin(black_box(state.as_str())).expect("parses"))
    });
    group.finish();
}

/// Resolving which edge a command came from, which the region does once per
/// inbound message against however many edges are attached.
fn bench_admit(c: &mut Criterion) {
    let mut group = c.benchmark_group("relay/admit");
    for &attached in &[1usize, 8, 64] {
        let edges = Edges::new();
        for k in 0..attached {
            edges.admit(&edge_name(k));
        }
        // The last one admitted is the far end of the scan, which is the cost
        // an edge pays for being unlucky rather than for being busy.
        let last = edge_name(attached - 1);
        group.bench_with_input(
            BenchmarkId::from_parameter(attached),
            &attached,
            |b, _| b.iter(|| black_box(edges.admit(black_box(&last)))),
        );
    }
    group.finish();
}

/// Giving entities back one at a time, which is what a client disconnecting
/// does and what an edge detaching does for every entity it held.
fn bench_release(c: &mut Criterion) {
    let mut group = c.benchmark_group("relay/release");
    for &held in &[64usize, 1024, 8192] {
        group.throughput(Throughput::Elements(held as u64));
        group.bench_with_input(BenchmarkId::from_parameter(held), &held, |b, _| {
            b.iter_batched(
                || {
                    let edges = Edges::new();
                    let id = edges.admit(&edge_name(0));
                    for k in 0..held {
                        edges.claim(id, entity(k)).expect("a fresh entity");
                    }
                    edges
                },
                |edges| {
                    for k in 0..held {
                        black_box(edges.release(entity(k)));
                    }
                },
                BatchSize::SmallInput,
            )
        });
    }
    group.finish();
}

/// Which edge manages an entity. A region asks this once per payload it sends,
/// so it is the most frequent call on the set.
fn bench_edge_for(c: &mut Criterion) {
    let mut group = c.benchmark_group("relay/edge_for");
    let edges = Edges::new();
    let id = edges.admit(&edge_name(0));
    for k in 0..8_192 {
        edges.claim(id, entity(k)).expect("a fresh entity");
    }
    group.bench_function("held", |b| {
        b.iter(|| black_box(edges.edge_for(black_box(entity(4_096)))))
    });
    group.bench_function("absent", |b| {
        b.iter(|| black_box(edges.edge_for(black_box(entity(100_000)))))
    });
    group.finish();
}

/// Putting a message on a stream and taking it off again. A client's reliable
/// traffic is framed one message at a time, and a burst arrives as one read
/// holding many.
fn bench_framer(c: &mut Criterion) {
    let mut group = c.benchmark_group("relay/framer");
    let body = vec![0xABu8; 1_200];

    let mut out = Vec::new();
    group.bench_function("frame", |b| {
        b.iter(|| Framer::frame(black_box(&body), black_box(&mut out)))
    });

    // Taking messages off a buffer that holds several: the cost of reaching
    // the second one depends on how the first was removed.
    for &messages in &[1usize, 16, 64] {
        let mut framed = Vec::new();
        let mut one = Vec::new();
        for _ in 0..messages {
            Framer::frame(&body, &mut one);
            framed.extend_from_slice(&one);
        }
        group.throughput(Throughput::Elements(messages as u64));
        group.bench_with_input(
            BenchmarkId::new("take", messages),
            &messages,
            |b, _| {
                b.iter_batched(
                    || {
                        let mut f = Framer::new();
                        f.push(&framed);
                        f
                    },
                    |mut f| {
                        while let Ok(Some(m)) = f.take() {
                            black_box(m);
                        }
                    },
                    BatchSize::SmallInput,
                )
            },
        );
    }
    group.finish();
}

criterion_group!(
    benches,
    bench_subjects,
    bench_admit,
    bench_release,
    bench_edge_for,
    bench_framer
);
criterion_main!(benches);
