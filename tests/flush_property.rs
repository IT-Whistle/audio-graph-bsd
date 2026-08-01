//! Property tests for the flush-gap (heatmap Property axis, i4).
//!
//! - `flush_roundtrip_preserves_arbitrary_signal`: an arbitrary signal seeded
//!   into a source survives cycle + flush + consumer-pop bit-exactly.
//! - `flush_into_full_ring_reports_ringfull_no_panic`: flushing past the ring
//!   capacity reports `RingFull` and never panics, for any small capacity.

use audio_core_bsd::{AudioFrame, AudioNode, PortDescriptor, ProcessContext, SampleFormat};
use audio_graph_bsd::{FlushError, Graph, GraphConfig, RingSink};
use proptest::prelude::*;

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

proptest! {
    /// The signal seeded into the source is preserved bit-exactly through
    /// process_cycle (RingSink stashes it) + flush_sinks (ships it) + consumer
    /// pop, for any length and any sample values in [-1, 1].
    #[test]
    fn flush_roundtrip_preserves_arbitrary_signal(
        values in prop::collection::vec(-1.0f32..1.0, 1..256)
    ) {
        let n = values.len();
        let (producer, mut consumer) = rtrb::RingBuffer::<AudioFrame>::new(4);

        let mut g = Graph::new();
        let src = g.add_node(Box::new(SourceNode::new()));
        let sink = g.add_sink(Box::new(RingSink::new(producer, 1, 48_000, n)));
        g.link((src, 0), (sink, 0)).unwrap();
        g.compile(GraphConfig::new(n, 48_000, 1)).unwrap();

        g.feed(src, 0, &AudioFrame::from_planar(1, 48_000, values.clone()));
        let mut ctx = ProcessContext::new(n, 0, 48_000);
        g.process_cycle(&mut ctx).unwrap();

        let (count, err) = g.flush_sinks();
        prop_assert_eq!(count, 1);
        prop_assert!(err.is_none());

        let frame = consumer.pop().expect("flushed frame must be available");
        prop_assert_eq!(frame.samples, values, "flush must preserve the signal bit-exactly");
    }

    /// Flushing past a small ring capacity must report `RingFull` (not panic)
    /// once the ring fills, and the error must be the documented variant.
    #[test]
    fn flush_into_full_ring_reports_ringfull_no_panic(capacity in 1usize..8) {
        // Keep the consumer alive (don't pop) so the ring fills up.
        let (producer, _consumer) = rtrb::RingBuffer::<AudioFrame>::new(capacity);

        let mut g = Graph::new();
        let src = g.add_node(Box::new(SourceNode::new()));
        let sink = g.add_sink(Box::new(RingSink::new(producer, 1, 48_000, 4)));
        g.link((src, 0), (sink, 0)).unwrap();
        g.compile(GraphConfig::new(4, 48_000, 1)).unwrap();
        g.feed(src, 0, &AudioFrame::from_planar(1, 48_000, vec![0.1; 4]));

        let mut ctx = ProcessContext::new(4, 0, 48_000);
        let mut saw_full = false;
        // Run more cycles + flushes than the ring can hold. After `capacity`
        // successful pushes, subsequent flushes must hit RingFull.
        for _ in 0..(capacity + 3) {
            g.process_cycle(&mut ctx).unwrap();
            let (_, err) = g.flush_sinks();
            if matches!(err, Some(FlushError::RingFull(_))) {
                saw_full = true;
            }
        }
        prop_assert!(
            saw_full,
            "expected at least one RingFull after flushing past capacity {capacity}"
        );
    }

    /// `flush_sink` on a plain (non-sink) node id always returns
    /// `NotFlushable`, regardless of how many plain nodes precede it.
    #[test]
    fn flush_sink_plain_node_always_not_flushable(plain_count in 0usize..10) {
        let mut g = Graph::new();
        let mut last = 0;
        for _ in 0..plain_count {
            last = g.add_node(Box::new(SourceNode::new()));
        }
        g.compile(GraphConfig::new(4, 48_000, 1)).unwrap();
        if plain_count > 0 {
            prop_assert_eq!(g.flush_sink(last), Err(FlushError::NotFlushable(last)));
        }
    }
}
