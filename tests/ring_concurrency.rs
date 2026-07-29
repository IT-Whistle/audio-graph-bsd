//! Concurrency test: the lock-free `rtrb` ring boundary is safe to drive from
//! the RT graph thread while a worker thread produces frames concurrently.
//!
//! A worker thread pushes a fixed number of distinct frames into an `rtrb`
//! `Producer`; the main thread drives a `RingSource`-fronted graph in a
//! `process_cycle` loop, popping and verifying every frame arrives intact.
//! This exercises the cross-thread lock-free contract described in
//! [`audio_graph_bsd::RingSource`].
//!
//! The test is deterministic: a bounded frame count and a join on the worker
//! thread make it flake-free.

use std::thread;

use audio_core_bsd::{
    AudioFrame, AudioNode, PortDescriptor, PortDirection, ProcessContext, SampleFormat,
};
use audio_graph_bsd::{Graph, GraphConfig};

/// Each produced frame is filled with a unique sentinel value so that
/// corruption or reordering is detectable.
const NUM_FRAMES: usize = 64;
const FRAME_SIZE: usize = 16;

/// Produces `NUM_FRAMES` frames from a worker thread while the main thread
/// drains them through a `RingSource`-fronted graph. Every frame must arrive
/// with its sentinel intact, proving the lock-free boundary is
/// concurrency-safe.
#[test]
fn ring_buffer_producer_consumer_across_threads() {
    let (mut producer, consumer) = rtrb::RingBuffer::<AudioFrame>::new(NUM_FRAMES);

    // Worker thread: push NUM_FRAMES distinct frames.
    let worker = thread::spawn(move || {
        for i in 0..NUM_FRAMES {
            let frame = AudioFrame::from_planar(
                1,
                48_000,
                (0..FRAME_SIZE).map(|_| i as f32).collect::<Vec<_>>(),
            );
            // Retry-push: the ring may briefly be full while the consumer
            // catches up. This is the standard rtrb producer pattern and never
            // touches the RT thread.
            while producer.push(frame.clone()).is_err() {
                thread::yield_now();
            }
        }
    });

    // Main thread: build a minimal graph (RingSource → identity GainNode) and
    // drain frames via process_cycle.
    let mut g = Graph::new();
    let rsrc = g.add_node(Box::new(audio_graph_bsd::RingSource::new(
        consumer, 1, 48_000, FRAME_SIZE,
    )));

    // A unity-gain node so the graph has an edge to exercise.
    struct IdentityNode {
        in_p: [PortDescriptor; 1],
        out_p: [PortDescriptor; 1],
    }
    impl AudioNode for IdentityNode {
        fn inputs(&self) -> &[PortDescriptor] {
            &self.in_p
        }
        fn outputs(&self) -> &[PortDescriptor] {
            &self.out_p
        }
        fn process(&mut self, _ctx: &mut ProcessContext, i: &[AudioFrame], o: &mut [AudioFrame]) {
            let (Some(inp), Some(out)) = (i.first(), o.get_mut(0)) else {
                return;
            };
            let n = inp.samples.len().min(out.samples.len());
            out.samples[..n].copy_from_slice(&inp.samples[..n]);
        }
    }
    let identity = IdentityNode {
        in_p: [PortDescriptor::input(1, SampleFormat::F32)],
        out_p: [PortDescriptor::output(1, SampleFormat::F32)],
    };
    let id_node = g.add_node(Box::new(identity));
    g.link((rsrc, 0), (id_node, 0)).unwrap();
    g.compile(GraphConfig::new(FRAME_SIZE, 48_000, 1)).unwrap();

    let mut ctx = ProcessContext::new(FRAME_SIZE, 0, 48_000);
    let mut received: Vec<i32> = Vec::with_capacity(NUM_FRAMES);

    // Drain exactly NUM_FRAMES distinct frames. The RingSource holds the last
    // frame on underrun, so we track distinct sentinel values as they arrive.
    let mut cycles = 0;
    while received.len() < NUM_FRAMES {
        // Bound the loop to avoid an infinite hang if something goes wrong.
        cycles += 1;
        assert!(
            cycles < NUM_FRAMES * 200,
            "consumer loop exceeded reasonable cycle bound; received {}/{}",
            received.len(),
            NUM_FRAMES
        );

        g.process_cycle(&mut ctx).unwrap();
        let out = g.read_output(id_node, 0).expect("identity output exists");
        if let Some(&first) = out.samples.first() {
            let sentinel = first.round() as i32;
            // Only record if this is a *new* produced frame (not a held repeat).
            if sentinel as usize == received.len() {
                // Verify the whole frame matches the sentinel.
                for &s in &out.samples {
                    assert!(
                        (s - sentinel as f32).abs() < 1e-6,
                        "frame {sentinel} corrupted: sample {s} != sentinel"
                    );
                }
                received.push(sentinel);
            }
        }
    }

    // Join the worker; a panic there would surface here.
    worker.join().expect("worker thread panicked");

    // Every sentinel 0..NUM_FRAMES must have arrived exactly once, in order.
    assert_eq!(
        received,
        (0..NUM_FRAMES as i32).collect::<Vec<_>>(),
        "frames missing or out of order"
    );
}

#[allow(dead_code)]
fn _ensure_port_direction_imported() -> PortDirection {
    PortDirection::Output
}
