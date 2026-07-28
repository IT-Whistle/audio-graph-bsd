//! `simple_route.rs` — minimal end-to-end `audio-graph` lifecycle.
//!
//! Builds a single-route graph `Source -> Gain(0.5)`, compiles it, then streams
//! a 440 Hz sine through three cycles. On each cycle the source's output port
//! is re-seeded via [`audio_graph::Graph::feed`], the engine runs one
//! [`audio_graph::Graph::process_cycle`], and the gain node's output is read
//! back and printed next to its input to confirm `output == input * gain`.
//!
//! Run with:
//!
//! ```sh
//! cargo run --example simple_route
//! ```

use audio_core::{
    AudioFrame, AudioNode, PortDescriptor, PortDirection, ProcessContext, SampleFormat,
};
use audio_graph::{Graph, GraphConfig};

/// A source node: zero inputs, one mono output, no-op `process`.
///
/// Its output scratch is seeded externally via `Graph::feed` and left untouched
/// each cycle — exactly how a real source / gateway node behaves inside the
/// engine.
struct SourceNode {
    out_port: [PortDescriptor; 1],
}

impl SourceNode {
    fn new() -> Self {
        Self {
            out_port: [PortDescriptor::new(
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
        &self.out_port
    }
    fn process(
        &mut self,
        _ctx: &mut ProcessContext,
        _in_frames: &[AudioFrame],
        _out_frames: &mut [AudioFrame],
    ) {
        // Intentionally a no-op: the output scratch survives from `Graph::feed`.
    }
}

/// A gain node: one mono input, one mono output, scales by a fixed `gain`.
struct GainNode {
    gain: f32,
    in_port: [PortDescriptor; 1],
    out_port: [PortDescriptor; 1],
}

impl GainNode {
    fn new(gain: f32) -> Self {
        Self {
            gain,
            in_port: [PortDescriptor::new(
                PortDirection::Input,
                1,
                SampleFormat::F32,
            )],
            out_port: [PortDescriptor::new(
                PortDirection::Output,
                1,
                SampleFormat::F32,
            )],
        }
    }
}

impl AudioNode for GainNode {
    fn inputs(&self) -> &[PortDescriptor] {
        &self.in_port
    }
    fn outputs(&self) -> &[PortDescriptor] {
        &self.out_port
    }
    fn process(
        &mut self,
        _ctx: &mut ProcessContext,
        in_frames: &[AudioFrame],
        out_frames: &mut [AudioFrame],
    ) {
        let (Some(inp), Some(out)) = (in_frames.first(), out_frames.get_mut(0)) else {
            return;
        };
        let n = inp.samples.len().min(out.samples.len());
        for k in 0..n {
            out.samples[k] = inp.samples[k] * self.gain;
        }
    }
}

/// Frames processed per cycle (kept small so the printed sample table is readable).
const BLOCK: usize = 8;
/// Sample rate (Hz).
const SR: u32 = 48_000;
/// Gain applied by the `GainNode`.
const GAIN: f32 = 0.5;
/// Tone frequency (Hz).
const FREQ: f32 = 440.0;
/// Number of cycles to stream.
const CYCLES: usize = 3;

fn main() {
    // -- 1. Build -----------------------------------------------------------
    println!(
        "== audio-graph: simple_route  (Source -> Gain({})) ==",
        GAIN
    );
    println!("[build   ] Graph::new(); add source + gain; link (src,0) -> (gain,0)");

    let mut graph = Graph::new();
    let src = graph.add_node(Box::new(SourceNode::new()));
    let gain = graph.add_node(Box::new(GainNode::new(GAIN)));
    graph
        .link((src, 0), (gain, 0))
        .expect("linking source -> gain");

    // -- 2. Compile ---------------------------------------------------------
    println!(
        "[compile] Graph::compile({{ num_frames = {}, sample_rate = {}, channels = 1 }})",
        BLOCK, SR
    );
    graph
        .compile(GraphConfig::new(BLOCK, SR, 1))
        .expect("compile must succeed on a simple chain");
    assert!(graph.is_compiled());

    // -- 3. Run a few cycles ------------------------------------------------
    println!(
        "[run     ] streaming {} cycles of a {} Hz sine, {} samples/cycle",
        CYCLES, FREQ, BLOCK
    );

    let mut ctx = ProcessContext::new(BLOCK, 0, SR);
    let omega = 2.0 * std::f32::consts::PI * FREQ / SR as f32;
    let mut phase = 0.0_f32;

    for cycle in 0..CYCLES {
        // (a) Synthesize one block of a 440 Hz sine and seed the source output.
        let mut input = AudioFrame::silence(1, BLOCK, SR);
        for (i, s) in input.samples.iter_mut().enumerate() {
            *s = (phase + i as f32 * omega).sin();
        }
        phase += BLOCK as f32 * omega;
        graph.feed(src, 0, &input);

        // (b) Process one cycle on the (would-be) RT thread.
        ctx.num_frames = BLOCK;
        ctx.sample_position = (cycle * BLOCK) as u64;
        ctx.sample_rate = SR;
        graph
            .process_cycle(&mut ctx)
            .expect("process_cycle on a compiled graph");

        // (c) Read the gain node's I/O and confirm output == input * gain.
        let out = graph.read_output(gain, 0).expect("gain output exists");
        let inp = graph.read_input(gain, 0).expect("gain input exists");

        let in_head: Vec<f32> = inp.samples.iter().take(4).copied().collect();
        let out_head: Vec<f32> = out.samples.iter().take(4).copied().collect();
        let max_err = out
            .samples
            .iter()
            .zip(inp.samples.iter())
            .map(|(&o, &i)| (o - i * GAIN).abs())
            .fold(0.0_f32, f32::max);

        println!("[cycle {} ] input  = {:>.5?}", cycle, in_head);
        println!("[cycle {} ] output = {:>.5?}", cycle, out_head);
        println!(
            "[cycle {} ] max |out - in*{}| = {:.2e}  -> {}",
            cycle,
            GAIN,
            max_err,
            if max_err < 1e-6 { "OK" } else { "MISMATCH" }
        );
        assert!(
            max_err < 1e-6,
            "gain node did not scale correctly on cycle {cycle}"
        );
    }

    println!("[done    ] all {CYCLES} cycles produced output == input * {GAIN}");
}
