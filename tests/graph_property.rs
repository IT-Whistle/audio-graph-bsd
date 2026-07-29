//! Property-based tests (proptest) for the audio graph engine.
//!
//! These verify two invariants that must hold for arbitrary inputs:
//!
//! 1. **Topological correctness** — a random acyclic graph (edges only go
//!    low-id → high-id) always compiles and processes, and a known signal
//!    propagates as the product of the gains along its path.
//! 2. **Pass-through identity** — a `src → pass → pass` chain preserves an
//!    arbitrary signal exactly (within float epsilon).

use audio_core_bsd::{AudioFrame, AudioNode, PortDescriptor, ProcessContext, SampleFormat};
use audio_graph_bsd::{Graph, GraphConfig};
use proptest::prelude::*;

// ---------------------------------------------------------------------------
// Local test-only AudioNode implementations.
// ---------------------------------------------------------------------------

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
    fn process(&mut self, _ctx: &mut ProcessContext, _i: &[AudioFrame], _o: &mut [AudioFrame]) {}
}

struct GainNode {
    gain: f32,
    in_p: [PortDescriptor; 1],
    out_p: [PortDescriptor; 1],
}
impl GainNode {
    fn new(gain: f32) -> Self {
        Self {
            gain,
            in_p: [PortDescriptor::input(1, SampleFormat::F32)],
            out_p: [PortDescriptor::output(1, SampleFormat::F32)],
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
    fn process(&mut self, _ctx: &mut ProcessContext, i: &[AudioFrame], o: &mut [AudioFrame]) {
        let (Some(inp), Some(out)) = (i.first(), o.get_mut(0)) else {
            return;
        };
        let n = inp.samples.len().min(out.samples.len());
        for k in 0..n {
            out.samples[k] = inp.samples[k] * self.gain;
        }
    }
}

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
        out.samples[..n].copy_from_slice(&inp.samples[..n]);
    }
}

fn approx_eq(a: f32, b: f32) -> bool {
    (a - b).abs() < 1e-5
}

// ---------------------------------------------------------------------------
// Property 1: topological correctness on a random DAG.
// ---------------------------------------------------------------------------

proptest! {
    /// Build a random acyclic graph as a linear chain of gain nodes (edges only
    /// go low-id → high-id, guaranteeing acyclicity). After compile + one
    /// `process_cycle`, the final output must equal `seed × product(gains)`.
    #[test]
    fn topological_order_respects_edges_for_random_dag(
        num_nodes in 2usize..=8,
        gains in proptest::collection::vec(-2.0f32..2.0f32, 2..8),
        seed in -1.0f32..1.0f32,
    ) {
        // Use at most `num_nodes` gains (the chain is source + k gains).
        let k = gains.len().min(num_nodes).max(1);
        let chain_gains = &gains[..k];

        let mut g = Graph::new();
        let src = g.add_node(Box::new(SourceNode::new(1)));
        let mut prev = src;
        let mut last = src;
        for &gain in chain_gains {
            let node = g.add_node(Box::new(GainNode::new(gain)));
            g.link((prev, 0), (node, 0)).unwrap();
            prev = node;
            last = node;
        }
        g.compile(GraphConfig::new(8, 48_000, 1)).unwrap();

        let input = vec![seed; 8];
        g.feed(src, 0, &AudioFrame::from_planar(1, 48_000, input.clone()));

        let mut ctx = ProcessContext::new(8, 0, 48_000);
        g.process_cycle(&mut ctx).unwrap();

        // Expected: seed * product of all gains in the chain.
        let expected_gain: f32 = chain_gains.iter().copied().product();
        let expected = seed * expected_gain;

        let out = g.read_output(last, 0).expect("tail output exists");
        for &s in &out.samples {
            prop_assert!(
                approx_eq(s, expected),
                "topological propagation mismatch: got {}, expected {} (seed={}, gains={:?})",
                s, expected, seed, chain_gains
            );
        }
    }
}

// ---------------------------------------------------------------------------
// Property 2: pass-through identity for arbitrary signals.
// ---------------------------------------------------------------------------

proptest! {
    /// For arbitrary frame sizes and arbitrary sample values, a
    /// `src → pass → pass` chain preserves the signal exactly (epsilon 1e-5).
    #[test]
    fn passthrough_preserves_arbitrary_signal(
        num_frames in 1usize..256,
        signal in proptest::collection::vec(-1.0f32..1.0f32, 1..256),
    ) {
        let n = num_frames.min(signal.len()).max(1);
        let signal = signal[..n].to_vec();

        let mut g = Graph::new();
        let src = g.add_node(Box::new(SourceNode::new(1)));
        let p1 = g.add_node(Box::new(PassThroughNode::new(1)));
        let p2 = g.add_node(Box::new(PassThroughNode::new(1)));
        g.link((src, 0), (p1, 0)).unwrap();
        g.link((p1, 0), (p2, 0)).unwrap();
        g.compile(GraphConfig::new(n, 48_000, 1)).unwrap();

        g.feed(src, 0, &AudioFrame::from_planar(1, 48_000, signal.clone()));

        let mut ctx = ProcessContext::new(n, 0, 48_000);
        g.process_cycle(&mut ctx).unwrap();

        let out = g.read_output(p2, 0).expect("tail output exists");
        let m = out.samples.len().min(signal.len());
        for (k, (&actual, &expected)) in out.samples[..m].iter().zip(signal[..m].iter()).enumerate()
        {
            prop_assert!(
                approx_eq(actual, expected),
                "signal not preserved at index {k}: got {actual}, expected {expected}"
            );
        }
    }
}
