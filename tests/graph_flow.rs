//! P11 §7a-M02 acceptance integration tests for the audio graph engine.
//!
//! These are the governance-mandated integration tests verifying the two M02
//! acceptance criteria:
//!
//! 1. **Source→Sink single-path processing + AudioFrame consistency** — a
//!    recognisable signal travels unchanged through a chain of nodes.
//! 2. **10-node × 1000-cycle stability** — a longer chain runs a thousand
//!    cycles without panicking, deadlocking, or corrupting the signal.
//!
//! They also cover multi-channel (stereo) frame consistency and diamond
//! fan-out / fan-in topology.

use audio_core::{AudioFrame, AudioNode, PortDescriptor, ProcessContext, SampleFormat};
use audio_graph::{Graph, GraphConfig};

// ---------------------------------------------------------------------------
// Local test-only AudioNode implementations.
//
// Integration tests are separate crates and cannot see the crate's private
// `#[cfg(test)] mod tests` helpers, so the canonical SourceNode / PassThroughNode
// / SumNode patterns are re-defined here verbatim (mirroring `src/graph.rs`).
// ---------------------------------------------------------------------------

/// A source node: zero inputs, one output, no-op `process`. Its output scratch
/// is seeded externally via [`Graph::feed`] and left untouched each cycle.
struct SourceNode {
    out_p: [PortDescriptor; 1],
}
impl SourceNode {
    fn new(channels: u16) -> Self {
        Self {
            out_p: [PortDescriptor::output(channels, SampleFormat::F32)],
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
    fn process(&mut self, _ctx: &mut ProcessContext, _i: &[AudioFrame], _o: &mut [AudioFrame]) {
        // Intentionally a no-op: the output scratch is seeded via Graph::feed.
    }
}

/// A pass-through node: one mono/stereo input, one output, copies input→output
/// sample-for-sample (RT-safe bounded copy, never panics).
struct PassThroughNode {
    in_p: [PortDescriptor; 1],
    out_p: [PortDescriptor; 1],
}
impl PassThroughNode {
    fn new(channels: u16) -> Self {
        Self {
            in_p: [PortDescriptor::input(channels, SampleFormat::F32)],
            out_p: [PortDescriptor::output(channels, SampleFormat::F32)],
        }
    }
}
impl AudioNode for PassThroughNode {
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
        // Bounded, alloc-free copy.
        out.samples[..n].copy_from_slice(&inp.samples[..n]);
    }
}

/// A two-input summing (mixer) node: adds two input frames sample-wise into a
/// single output. Used for the diamond fan-out/fan-in topology.
struct SumNode {
    in_p: [PortDescriptor; 2],
    out_p: [PortDescriptor; 1],
}
impl SumNode {
    fn new(channels: u16) -> Self {
        Self {
            in_p: [
                PortDescriptor::input(channels, SampleFormat::F32),
                PortDescriptor::input(channels, SampleFormat::F32),
            ],
            out_p: [PortDescriptor::output(channels, SampleFormat::F32)],
        }
    }
}
impl AudioNode for SumNode {
    fn inputs(&self) -> &[PortDescriptor] {
        &self.in_p
    }
    fn outputs(&self) -> &[PortDescriptor] {
        &self.out_p
    }
    fn process(&mut self, _ctx: &mut ProcessContext, i: &[AudioFrame], o: &mut [AudioFrame]) {
        let Some(out) = o.get_mut(0) else {
            return;
        };
        // Zero the output first.
        for s in &mut out.samples {
            *s = 0.0;
        }
        // Add each connected input.
        for inp in i {
            let n = inp.samples.len().min(out.samples.len());
            for k in 0..n {
                out.samples[k] += inp.samples[k];
            }
        }
    }
}

/// Approximate float equality (uses `<`, never `==`).
fn approx_eq(a: f32, b: f32) -> bool {
    (a - b).abs() < 1e-6
}

/// Element-wise approximate equality for two slices.
fn slices_approx_eq(a: &[f32], b: &[f32]) -> bool {
    a.len() == b.len() && a.iter().zip(b).all(|(&x, &y)| approx_eq(x, y))
}

// ---------------------------------------------------------------------------
// Acceptance #1: Source→Sink single-path processing + AudioFrame consistency.
// ---------------------------------------------------------------------------

/// A recognisable ramp travels unchanged through SourceNode → PassThroughNode →
/// PassThroughNode after a single `process_cycle`. The output must be
/// sample-identical (epsilon 1e-6) to the seeded input.
#[test]
fn source_to_sink_passes_samples_unchanged() {
    const N: usize = 16;
    let mut g = Graph::new();
    let src = g.add_node(Box::new(SourceNode::new(1)));
    let mid = g.add_node(Box::new(PassThroughNode::new(1)));
    let sink = g.add_node(Box::new(PassThroughNode::new(1)));
    g.link((src, 0), (mid, 0)).unwrap();
    g.link((mid, 0), (sink, 0)).unwrap();
    g.compile(GraphConfig::new(N, 48_000, 1)).unwrap();

    // Seed a recognisable ramp 0.0, 0.1, 0.2, ...
    let ramp: Vec<f32> = (0..N).map(|k| k as f32 * 0.1).collect();
    g.feed(src, 0, &AudioFrame::from_planar(1, 48_000, ramp.clone()));

    let mut ctx = ProcessContext::new(N, 0, 48_000);
    g.process_cycle(&mut ctx).unwrap();

    let out = g.read_output(sink, 0).expect("sink output exists");
    assert!(
        slices_approx_eq(&out.samples, &ramp),
        "ramp not preserved through source→pass→pass; got {:?}",
        out.samples
    );
}

// ---------------------------------------------------------------------------
// Acceptance #2: 10-node × 1000-cycle stability (no panic / no deadlock,
// signal preserved).
// ---------------------------------------------------------------------------

/// A chain of 10 nodes (1 SourceNode + 8 PassThroughNode + 1 final
/// PassThroughNode) runs `process_cycle` 1000 times. The test passing means no
/// panic occurred; additionally the ramp signal must survive intact through all
/// 10 nodes after the 1000th cycle.
#[test]
fn ten_node_chain_thousand_cycles_no_panic() {
    const N: usize = 32;
    const CHAIN_LEN: usize = 10; // 1 source + 9 downstream pass-throughs
    const CYCLES: usize = 1000;

    let mut g = Graph::new();
    let src = g.add_node(Box::new(SourceNode::new(1)));
    let mut prev = src;
    let mut last = src;
    for _ in 0..(CHAIN_LEN - 1) {
        let node = g.add_node(Box::new(PassThroughNode::new(1)));
        g.link((prev, 0), (node, 0)).unwrap();
        prev = node;
        last = node;
    }
    g.compile(GraphConfig::new(N, 48_000, 1)).unwrap();

    let ramp: Vec<f32> = (0..N).map(|k| (k as f32) / (N as f32)).collect();
    g.feed(src, 0, &AudioFrame::from_planar(1, 48_000, ramp.clone()));

    let mut ctx = ProcessContext::new(N, 0, 48_000);
    for _ in 0..CYCLES {
        // If process_cycle ever panics or deadlocks, the test fails here.
        g.process_cycle(&mut ctx).unwrap();
    }

    let out = g.read_output(last, 0).expect("chain tail output exists");
    assert!(
        slices_approx_eq(&out.samples, &ramp),
        "ramp corrupted after {CYCLES} cycles across {CHAIN_LEN} nodes"
    );
}

// ---------------------------------------------------------------------------
// Multi-channel (stereo) AudioFrame consistency.
// ---------------------------------------------------------------------------

/// The same source→pass→pass chain but with `channels = 2`. Both channel
/// slices (planar layout: `[ch0..., ch1...]`) must be preserved independently.
#[test]
fn stereo_two_channel_signal_preserved() {
    const N: usize = 8;
    let mut g = Graph::new();
    let src = g.add_node(Box::new(SourceNode::new(2)));
    let mid = g.add_node(Box::new(PassThroughNode::new(2)));
    let sink = g.add_node(Box::new(PassThroughNode::new(2)));
    g.link((src, 0), (mid, 0)).unwrap();
    g.link((mid, 0), (sink, 0)).unwrap();
    g.compile(GraphConfig::new(N, 48_000, 2)).unwrap();

    // Build a stereo planar buffer: ch0 = rising ramp, ch1 = falling ramp.
    let mut stereo = Vec::with_capacity(N * 2);
    for k in 0..N {
        stereo.push(k as f32 * 0.05); // ch0
    }
    for k in 0..N {
        stereo.push(1.0 - k as f32 * 0.05); // ch1
    }
    g.feed(src, 0, &AudioFrame::from_planar(2, 48_000, stereo.clone()));

    let mut ctx = ProcessContext::new(N, 0, 48_000);
    g.process_cycle(&mut ctx).unwrap();

    let out = g.read_output(sink, 0).expect("stereo sink output exists");
    assert_eq!(out.channels, 2);
    assert!(
        slices_approx_eq(out.channel_slice(0), &stereo[..N]),
        "stereo ch0 not preserved"
    );
    assert!(
        slices_approx_eq(out.channel_slice(1), &stereo[N..]),
        "stereo ch1 not preserved"
    );
}

// ---------------------------------------------------------------------------
// Diamond fan-out / fan-in topology.
// ---------------------------------------------------------------------------

/// `src → {a, b} → sum`: the source fans out to two pass-through branches that
/// are then summed by a `SumNode`. Since both branches carry the identical
/// ramp, the sum must equal `2 × ramp`.
#[test]
fn diamond_fanout_fanin() {
    const N: usize = 8;
    let mut g = Graph::new();
    let src = g.add_node(Box::new(SourceNode::new(1)));
    let a = g.add_node(Box::new(PassThroughNode::new(1)));
    let b = g.add_node(Box::new(PassThroughNode::new(1)));
    let sum = g.add_node(Box::new(SumNode::new(1)));
    g.link((src, 0), (a, 0)).unwrap();
    g.link((src, 0), (b, 0)).unwrap();
    g.link((a, 0), (sum, 0)).unwrap();
    g.link((b, 0), (sum, 1)).unwrap();
    g.compile(GraphConfig::new(N, 48_000, 1)).unwrap();

    let ramp: Vec<f32> = (0..N).map(|k| k as f32 * 0.1).collect();
    g.feed(src, 0, &AudioFrame::from_planar(1, 48_000, ramp.clone()));

    let mut ctx = ProcessContext::new(N, 0, 48_000);
    g.process_cycle(&mut ctx).unwrap();

    let out = g.read_output(sum, 0).expect("sum output exists");
    // Each branch carries `ramp`; the sum is `2 * ramp`.
    let expected: Vec<f32> = ramp.iter().map(|&v| v * 2.0).collect();
    assert!(
        slices_approx_eq(&out.samples, &expected),
        "diamond sum mismatch; got {:?} expected {:?}",
        out.samples,
        expected
    );
}
