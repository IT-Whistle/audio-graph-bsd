# audio-graph-bsd

[![License: BSD-2-Clause](https://img.shields.io/badge/license-BSD--2--Clause-blue.svg)](./LICENSE)
[![Crates.io](https://img.shields.io/crates/v/audio-graph-bsd.svg)](https://crates.io/crates/audio-graph-bsd)

Real-time-safe directed node-graph audio processing engine — schedules
[`AudioNode`][audio-core-bsd]s in topological order, pre-allocates every scratch
buffer at compile time, and runs an allocation-free
[`process_cycle`](Graph::process_cycle) on the RT thread.

> **Note:** The doc comments on each public item are the primary reference.
> This README is an overview only.

## Role

This crate is a standalone graph engine that depends on
[`audio-core-bsd`](https://crates.io/crates/audio-core-bsd) for the node contract
(`AudioNode`, `AudioFrame`, `ProcessContext`, `PortDescriptor`). It provides:

- A **directed acyclic graph** of `AudioNode`s wired output-port → input-port.
- **Topological scheduling** at compile time, with cycle rejection.
- **Pre-allocated RT scratch** so the per-cycle driver never allocates, locks,
  panics, or performs a system call.
- **rtrb-backed bridge nodes** for shuttling audio to/from worker threads.
- A trait-based Rust public API surface (not a C FFI).

## Core API

| Item | Description |
|------|-------------|
| [`Graph`] | The engine: owns nodes, edges, and the pre-allocated per-port scratch frames. |
| [`GraphConfig`] | Compile-time config fixing `num_frames` / `sample_rate` / `channels` for every scratch buffer. |
| [`NodeId`] / [`PortIdx`] / [`LinkId`] | Stable `usize` identifiers for nodes, ports, and links. |
| [`GraphError`] | Construction/compile error enum (thiserror-backed): cycle, port, direction, compatibility, state. |
| [`RingSource`] | Source node draining a worker thread via a wait-free `rtrb` consumer. |
| [`RingSink`] | Sink node tapping graph output for a worker thread consumer. |

The graph moves through three phases:

1. **Build** — [`Graph::add_node`] registers nodes and [`Graph::link`] wires
   output ports to input ports (with direction / channel / format validation).
2. **Compile** — [`Graph::compile`] runs the topological sort, rejects cycles,
   and pre-allocates every per-port scratch frame. Allocation is only permitted
   here.
3. **Run** — [`Graph::process_cycle`] drives one audio cycle on the RT thread.
   [`Graph::feed`] seeds external inputs before a cycle;
   [`Graph::read_output`] / [`Graph::read_input`] tap results after.

## Dependencies

| Dependency | Version | Purpose |
|------------|---------|---------|
| `audio-core-bsd` | 0.1.0 | `AudioNode` / `AudioFrame` / `ProcessContext` / `PortDescriptor` contract. |
| `rtrb` | 0.3 | Lock-free ring buffer backing `RingSource` / `RingSink`. |
| `thiserror` | 2.0 | `GraphError` derive. |

`proptest` is a **dev-only** dependency (property tests) and is not shipped with
the crate. `serde` / `tracing` are **not** currently included.

## Status

**0.x — experimental.** The API is not yet frozen. Breaking changes are
expected before a 1.0 release.

- edition: 2021
- MSRV: 1.85
- license: BSD-2-Clause

## Example

A minimal two-node graph (source → gain) driven for one cycle. The source is a
no-op node whose output scratch is seeded via [`Graph::feed`]; the gain node
scales it by 0.5:

```rust
use audio_core_bsd::{
    AudioFrame, AudioNode, PortDescriptor, PortDirection, ProcessContext, SampleFormat,
};
use audio_graph_bsd::{Graph, GraphConfig};

// A source node: one mono output, no-op `process` (its output scratch is seeded
// externally via `Graph::feed` and left untouched each cycle).
struct SourceNode {
    out_p: [PortDescriptor; 1],
}
impl SourceNode {
    fn new() -> Self {
        Self {
            out_p: [PortDescriptor::output(1, SampleFormat::F32)],
        }
    }
}
impl AudioNode for SourceNode {
    fn inputs(&self) -> &[PortDescriptor] { &[] }
    fn outputs(&self) -> &[PortDescriptor] { &self.out_p }
    fn process(&mut self, _ctx: &mut ProcessContext, _i: &[AudioFrame], _o: &mut [AudioFrame]) {}
}

// A gain node: one mono input, one mono output, scales by `gain`.
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
    fn inputs(&self) -> &[PortDescriptor] { &self.in_p }
    fn outputs(&self) -> &[PortDescriptor] { &self.out_p }
    fn process(&mut self, _ctx: &mut ProcessContext, i: &[AudioFrame], o: &mut [AudioFrame]) {
        let (Some(inp), Some(out)) = (i.first(), o.get_mut(0)) else { return };
        let n = inp.samples.len().min(out.samples.len());
        for k in 0..n {
            out.samples[k] = inp.samples[k] * self.gain;
        }
    }
}

// 1. Build: add a source and a gain node, wire src:0 -> gain:0.
let mut g = Graph::new();
let src = g.add_node(Box::new(SourceNode::new()));
let dst = g.add_node(Box::new(GainNode::new(0.5)));
g.link((src, 0), (dst, 0)).unwrap();

// 2. Compile: topological sort + pre-allocate every scratch frame.
g.compile(GraphConfig::new(8, 48_000, 1)).unwrap();

// 3. Run: seed the source, drive one cycle, read the scaled output.
g.feed(src, 0, &AudioFrame::from_planar(1, 48_000, vec![1.0; 8]));
let mut ctx = ProcessContext::new(8, 0, 48_000);
g.process_cycle(&mut ctx).unwrap();
assert!(g
    .read_output(dst, 0)
    .unwrap()
    .samples
    .iter()
    .all(|&x| (x - 0.5).abs() < 1e-6));
```

A larger, runnable version of this pattern — streaming a 440 Hz sine through
multiple cycles — lives at
[`examples/simple_route.rs`](./examples/simple_route.rs):

```sh
cargo run --example simple_route
```

## Real-time safety boundary

[`Graph::process_cycle`] is the RT (real-time) entry point. It **must not**
allocate, acquire locks, panic, or perform any system call, or it will cause
audio dropouts (xruns), priority-inversion stalls, or undefined behaviour.

This is achieved structurally by separating the three phases:

- The topological sort runs in [`Graph::compile`] — **never** in `process_cycle`.
- **All** per-port scratch frames are pre-sized in `compile()`, so `process_cycle`
  never grows a `Vec`.
- `process_cycle` uses only bounded `for` loops and slice copies over
  pre-sized buffers; unconnected inputs are zeroed in place.

Conformance is verified at test time with a counting allocator that fails on
any allocation observed inside `process_cycle` across 1000 cycles (see the RT
alloc-free integration test).

## License

BSD-2-Clause. See [`LICENSE`](./LICENSE).
