//! Engine-changes §4 (Option A): flush-gap integration tests.
//!
//! Verifies that a `RingSink` registered via `Graph::add_sink` has its stashed
//! frame drained to the ring consumer by `Graph::flush_sinks` (off-RT,
//! between cycles), and that `flush_sink` correctly rejects plain nodes.

use audio_core_bsd::{AudioFrame, AudioNode, PortDescriptor, ProcessContext, SampleFormat};
use audio_graph_bsd::{FlushError, Graph, GraphConfig, RingSink};

/// A source node: 0 inputs, 1 output, no-op process (output seeded via feed).
struct SourceNode {
    out: [PortDescriptor; 1],
}
impl SourceNode {
    fn new() -> Self {
        Self {
            out: [PortDescriptor::output(1, SampleFormat::F32)],
        }
    }
}
impl AudioNode for SourceNode {
    fn inputs(&self) -> &[PortDescriptor] {
        &[]
    }
    fn outputs(&self) -> &[PortDescriptor] {
        &self.out
    }
    fn process(&mut self, _: &mut ProcessContext, _: &[AudioFrame], _: &mut [AudioFrame]) {}
}

#[test]
fn flush_sinks_drains_stash_to_consumer() {
    const N: usize = 4;
    let (producer, mut consumer) = rtrb::RingBuffer::<AudioFrame>::new(8);

    let mut g = Graph::new();
    let src = g.add_node(Box::new(SourceNode::new()));
    // Register the sink via add_sink so the engine can flush it.
    let sink = g.add_sink(Box::new(RingSink::new(producer, 1, 48_000, N)));
    g.link((src, 0), (sink, 0)).unwrap();
    g.compile(GraphConfig::new(N, 48_000, 1)).unwrap();

    // Seed the source; run one cycle (RingSink stashes its input).
    g.feed(src, 0, &AudioFrame::from_planar(1, 48_000, vec![0.5; N]));
    let mut ctx = ProcessContext::new(N, 0, 48_000);
    g.process_cycle(&mut ctx).unwrap();

    // Before flush: the consumer ring is empty (stash not yet shipped).
    assert!(consumer.pop().is_err(), "consumer must be empty before flush");

    // Flush (OFF-RT, between cycles) — pushes the stash to the consumer.
    let (count, err) = g.flush_sinks();
    assert_eq!(count, 1, "exactly one sink should be flushed");
    assert!(err.is_none(), "flush should succeed on an empty ring");

    // The consumer now receives the stashed frame with the seeded signal.
    let frame = consumer.pop().expect("frame should be available after flush");
    assert!(
        frame.samples.iter().all(|&s| (s - 0.5).abs() < 1e-6),
        "flushed frame should carry the 0.5 signal"
    );
}

#[test]
fn flush_sinks_reports_ring_full_without_panic() {
    const N: usize = 2;
    // Capacity 1: the first flush fills it; the second must report RingFull.
    let (producer, _consumer) = rtrb::RingBuffer::<AudioFrame>::new(1);

    let mut g = Graph::new();
    let src = g.add_node(Box::new(SourceNode::new()));
    let _sink = g.add_sink(Box::new(RingSink::new(producer, 1, 48_000, N)));
    g.link((src, 0), (_sink, 0)).unwrap();
    g.compile(GraphConfig::new(N, 48_000, 1)).unwrap();

    g.feed(src, 0, &AudioFrame::from_planar(1, 48_000, vec![0.25; N]));
    let mut ctx = ProcessContext::new(N, 0, 48_000);
    g.process_cycle(&mut ctx).unwrap();

    // First flush fills the ring (capacity 1) — OK.
    let (count1, err1) = g.flush_sinks();
    assert_eq!(count1, 1);
    assert!(err1.is_none());

    // Second flush: ring is full → RingFull reported, no panic.
    let (count2, err2) = g.flush_sinks();
    assert_eq!(count2, 1);
    assert!(matches!(err2, Some(FlushError::RingFull(_))));
}

#[test]
fn flush_sink_on_plain_node_is_not_flushable() {
    let mut g = Graph::new();
    let n = g.add_node(Box::new(SourceNode::new())); // plain, not a sink
    g.compile(GraphConfig::new(4, 48_000, 1)).unwrap();
    assert_eq!(g.flush_sink(n), Err(FlushError::NotFlushable(n)));
}

#[test]
fn flush_sink_on_missing_node_is_not_found() {
    let mut g = Graph::new();
    g.compile(GraphConfig::new(4, 48_000, 1)).unwrap();
    assert_eq!(g.flush_sink(99), Err(FlushError::NodeNotFound(99)));
}

#[test]
fn flush_sink_drains_specific_sink_by_id() {
    const N: usize = 4;
    let (producer, mut consumer) = rtrb::RingBuffer::<AudioFrame>::new(8);
    let mut g = Graph::new();
    let src = g.add_node(Box::new(SourceNode::new()));
    let sink = g.add_sink(Box::new(RingSink::new(producer, 1, 48_000, N)));
    g.link((src, 0), (sink, 0)).unwrap();
    g.compile(GraphConfig::new(N, 48_000, 1)).unwrap();
    g.feed(src, 0, &AudioFrame::from_planar(1, 48_000, vec![0.75; N]));
    let mut ctx = ProcessContext::new(N, 0, 48_000);
    g.process_cycle(&mut ctx).unwrap();

    // Targeted flush of the specific sink node.
    g.flush_sink(sink).unwrap();
    let frame = consumer.pop().expect("frame after targeted flush");
    assert!(frame.samples.iter().all(|&s| (s - 0.75).abs() < 1e-6));
}
