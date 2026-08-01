//! Criterion micro-benchmark for the real-time `process_cycle` path.
//!
//! Measures the per-cycle latency of driving a 10-node gain chain (the same
//! topology used by the P11 section 7a-M02 stability acceptance test) plus a
//! 1-node trivial graph, at the common baseline block size (256 frames, 48 kHz,
//! mono). The RT path is allocation-free; these numbers are the engine's
//! scheduling + copy overhead on top of each node's own work.
//!
//! Run with: `cargo bench -p audio-graph`
//! Compare a baseline with: `cargo bench -- --save baseline`, then
//! `cargo bench -- --baseline baseline` to surface regressions.

use criterion::{criterion_group, criterion_main, Criterion};

use audio_core_bsd::{
    AudioFrame, AudioNode, PortDescriptor, PortDirection, ProcessContext, SampleFormat,
};
use audio_graph_bsd::{Graph, GraphConfig, RingSink, RingSource};

// ---------------------------------------------------------------------------
// Minimal test nodes (benches are a separate crate, same as integration tests).
// ---------------------------------------------------------------------------

/// A source: one mono output, no-op `process` (output seeded via `Graph::feed`).
struct SourceNode {
    out_p: [PortDescriptor; 1],
}
impl SourceNode {
    fn new() -> Self {
        Self {
            out_p: [PortDescriptor::new(
                PortDirection::Output,
                1,
                SampleFormat::F32,
            )],
        }
    }
}
impl AudioNode for SourceNode {
    fn inputs(&self) -> &[PortDescriptor] {
        &[]
    }
    fn outputs(&self) -> &[PortDescriptor] {
        &self.out_p
    }
    fn process(&mut self, _: &mut ProcessContext, _: &[AudioFrame], _: &mut [AudioFrame]) {}
}

/// A mono gain node: 1 in, 1 out, scales by `gain`.
struct GainNode {
    gain: f32,
    in_p: [PortDescriptor; 1],
    out_p: [PortDescriptor; 1],
}
impl GainNode {
    fn new(gain: f32) -> Self {
        Self {
            gain,
            in_p: [PortDescriptor::new(
                PortDirection::Input,
                1,
                SampleFormat::F32,
            )],
            out_p: [PortDescriptor::new(
                PortDirection::Output,
                1,
                SampleFormat::F32,
            )],
        }
    }
}
impl AudioNode for GainNode {
    fn inputs(&self) -> &[PortDescriptor] {
        &self.in_p
    }
    fn outputs(&self) -> &[PortDescriptor] {
        &self.out_p
    }
    fn process(&mut self, _: &mut ProcessContext, i: &[AudioFrame], o: &mut [AudioFrame]) {
        let (Some(inp), Some(out)) = (i.first(), o.get_mut(0)) else {
            return;
        };
        let n = inp.samples.len().min(out.samples.len());
        for k in 0..n {
            out.samples[k] = inp.samples[k] * self.gain;
        }
    }
}

/// Builds a chain of `nodes` gain nodes behind a source, compiled for `frames`.
fn build_chain(nodes: usize, frames: usize) -> (Graph, usize) {
    let mut g = Graph::new();
    let src = g.add_node(Box::new(SourceNode::new()));
    let mut prev = src;
    for _ in 0..nodes {
        let n = g.add_node(Box::new(GainNode::new(0.999)));
        g.link((prev, 0), (n, 0)).unwrap();
        prev = n;
    }
    g.compile(GraphConfig::new(frames, 48_000, 1)).unwrap();
    g.feed(
        src,
        0,
        &AudioFrame::from_planar(1, 48_000, vec![0.5; frames]),
    );
    (g, prev)
}

/// Like `build_chain` but ends in a flushable `RingSink` (registered via
/// `add_sink`) fed by a `RingSource`, so a benchmark can measure the
/// `process_cycle` + between-cycle `flush_sinks` pattern (engine-changes §4.2).
fn build_sink_chain(nodes: usize, frames: usize) -> (Graph, usize) {
    let (mut prod_in, cons_in) = rtrb::RingBuffer::<AudioFrame>::new(64);
    let (prod_out, _cons_out) = rtrb::RingBuffer::<AudioFrame>::new(4096);
    let mut g = Graph::new();
    let src = g.add_node(Box::new(RingSource::new(cons_in, 1, 48_000, frames)));
    let mut prev = src;
    for _ in 0..nodes {
        let n = g.add_node(Box::new(GainNode::new(1.0)));
        g.link((prev, 0), (n, 0)).unwrap();
        prev = n;
    }
    let sink = g.add_sink(Box::new(RingSink::new(prod_out, 1, 48_000, frames)));
    g.link((prev, 0), (sink, 0)).unwrap();
    g.compile(GraphConfig::new(frames, 48_000, 1)).unwrap();
    // Pre-fill the input ring so the source has data each cycle.
    for _ in 0..16 {
        let _ = prod_in.push(AudioFrame::from_planar(1, 48_000, vec![0.5; frames]));
    }
    (g, sink)
}

fn bench_process_cycle(c: &mut Criterion) {
    let mut group = c.benchmark_group("process_cycle");

    // 1-node trivial graph: pure engine overhead (topo walk + 1 process call).
    {
        let (g, _) = build_chain(1, 256);
        let mut ctx = ProcessContext::new(256, 0, 48_000);
        group.bench_function("1_node_256f", |b| {
            b.iter(|| {
                // `process_cycle` mutates the graph's scratch in place; its
                // side effects prevent the optimiser from eliding the call.
                g.process_cycle(&mut ctx).unwrap();
            });
        });
    }

    // 10-node chain at 256 frames: the M02 acceptance topology.
    {
        let (g, _) = build_chain(10, 256);
        let mut ctx = ProcessContext::new(256, 0, 48_000);
        group.bench_function("10_node_chain_256f", |b| {
            b.iter(|| {
                // `process_cycle` mutates the graph's scratch in place; its
                // side effects prevent the optimiser from eliding the call.
                g.process_cycle(&mut ctx).unwrap();
            });
        });
    }

    // 10-node chain at 1024 frames: larger block, amortised scheduling cost.
    {
        let (g, _) = build_chain(10, 1024);
        let mut ctx = ProcessContext::new(1024, 0, 48_000);
        group.bench_function("10_node_chain_1024f", |b| {
            b.iter(|| {
                // `process_cycle` mutates the graph's scratch in place; its
                // side effects prevent the optimiser from eliding the call.
                g.process_cycle(&mut ctx).unwrap();
            });
        });
    }

    // 10-node chain ending in a flushable sink: cycle + between-cycle flush
    // (measures the full engine-changes §4.2 RT-loop pattern, incl. the
    // off-RT clone+push of flush_sinks). Regression guard for the NodeSlot
    // delegate cost + flush overhead.
    {
        let (mut g, _sink) = build_sink_chain(10, 256);
        let mut ctx = ProcessContext::new(256, 0, 48_000);
        group.bench_function("10_node_with_sink_flush_256f", |b| {
            b.iter(|| {
                g.process_cycle(&mut ctx).unwrap();
                let _ = g.flush_sinks();
            });
        });
    }

    group.finish();
}

criterion_group!(benches, bench_process_cycle);
criterion_main!(benches);
