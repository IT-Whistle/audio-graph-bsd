//! Phase-C hot-reload tests (ROADMAP §5 strategy B + §9.1 G1).
//!
//! 1. `install` atomically publishes a new compiled graph; the very next
//!    `process_cycle` runs on the NEW graph (lossless swap, no RT pause).
//! 2. G1: `RtHandle::process_cycle` performs a wait-free `arc-swap` load
//!    followed by the alloc-free `Graph::process_cycle` — 0 allocations across
//!    1000 cycles. Uses a thread-local counting allocator (same pattern as
//!    `rt_alloc_free.rs`) so only the measuring thread's allocations count.
#![cfg(feature = "distributed")]

use std::alloc::{GlobalAlloc, Layout, System};
use std::cell::Cell;

use audio_core_bsd::{AudioFrame, AudioNode, PortDescriptor, PortDirection, ProcessContext, SampleFormat};
use audio_graph_bsd::{Graph, GraphConfig, RtHandle};

// ---------------------------------------------------------------------------
// Thread-local counting allocator.
// ---------------------------------------------------------------------------

struct CountingAllocator;

thread_local! {
    static RT_MEASURING: Cell<bool> = const { Cell::new(false) };
    static RT_ALLOC_COUNT: Cell<usize> = const { Cell::new(0) };
}

unsafe impl GlobalAlloc for CountingAllocator {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        let _ = RT_MEASURING.try_with(|m| {
            if m.get() {
                let _ = RT_ALLOC_COUNT.try_with(|c| c.set(c.get().saturating_add(1)));
            }
        });
        System.alloc(layout)
    }
    unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
        System.dealloc(ptr, layout);
    }
}

#[global_allocator]
static A: CountingAllocator = CountingAllocator;

fn rt_start_measuring() {
    RT_ALLOC_COUNT.with(|c| c.set(0));
    RT_MEASURING.with(|m| m.set(true));
}

fn rt_stop_and_count() -> usize {
    RT_MEASURING.with(|m| m.set(false));
    RT_ALLOC_COUNT.with(|c| c.get())
}

// ---------------------------------------------------------------------------
// Test nodes.
// ---------------------------------------------------------------------------

struct SourceNode {
    out: [PortDescriptor; 1],
}
impl SourceNode {
    fn new() -> Self {
        Self {
            out: [PortDescriptor::new(PortDirection::Output, 1, SampleFormat::F32)],
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

struct GainNode {
    g: f32,
    inp: [PortDescriptor; 1],
    out: [PortDescriptor; 1],
}
impl GainNode {
    fn new(g: f32) -> Self {
        Self {
            g,
            inp: [PortDescriptor::new(PortDirection::Input, 1, SampleFormat::F32)],
            out: [PortDescriptor::new(PortDirection::Output, 1, SampleFormat::F32)],
        }
    }
}
impl AudioNode for GainNode {
    fn inputs(&self) -> &[PortDescriptor] {
        &self.inp
    }
    fn outputs(&self) -> &[PortDescriptor] {
        &self.out
    }
    fn process(&mut self, _: &mut ProcessContext, i: &[AudioFrame], o: &mut [AudioFrame]) {
        let (Some(inp), Some(out)) = (i.first(), o.get_mut(0)) else {
            return;
        };
        let n = inp.samples.len().min(out.samples.len());
        for k in 0..n {
            out.samples[k] = inp.samples[k] * self.g;
        }
    }
}

/// Build src(no-op, feed-seeded) → gain(g), compiled and fed with all-ones.
fn build_gain_graph(g: f32) -> (Graph, usize) {
    let mut graph = Graph::new();
    let src = graph.add_node(Box::new(SourceNode::new()));
    let gain = graph.add_node(Box::new(GainNode::new(g)));
    graph.link((src, 0), (gain, 0)).unwrap();
    graph.compile(GraphConfig::new(4, 48_000, 1)).unwrap();
    graph.feed(src, 0, &AudioFrame::from_planar(1, 48_000, vec![1.0; 4]));
    (graph, gain)
}

// ---------------------------------------------------------------------------
// Hot-swap correctness.
// ---------------------------------------------------------------------------

#[test]
fn install_publishes_new_graph_for_next_cycle() {
    let (g1, gain1) = build_gain_graph(2.0);
    let handle = RtHandle::new(g1);

    let mut ctx = ProcessContext::new(4, 0, 48_000);
    // Cycle 1: gain 2.0 → output 1.0 * 2.0 = 2.0.
    handle.process_cycle(&mut ctx).unwrap();
    let g1 = handle.graph();
    let out1 = g1.read_output(gain1, 0).unwrap();
    assert!(
        out1.samples.iter().all(|&s| (s - 2.0).abs() < 1e-6),
        "first graph (gain 2.0) should produce 2.0"
    );
    drop(g1);

    // Publish a brand-new compiled graph (gain 0.5) without stopping RT.
    let (g2, gain2) = build_gain_graph(0.5);
    handle.install(g2);

    // Cycle 2: must run on the NEW graph → output 1.0 * 0.5 = 0.5.
    handle.process_cycle(&mut ctx).unwrap();
    let g2g = handle.graph();
    let out2 = g2g.read_output(gain2, 0).unwrap();
    assert!(
        out2.samples.iter().all(|&s| (s - 0.5).abs() < 1e-6),
        "after install, the new graph (gain 0.5) should produce 0.5"
    );
}

// ---------------------------------------------------------------------------
// G1: RT alloc-free (arc-swap load + process_cycle).
// ---------------------------------------------------------------------------

#[test]
fn rt_handle_process_cycle_is_alloc_free_across_1000_cycles() {
    let (graph, _gain) = build_gain_graph(1.0);
    let handle = RtHandle::new(graph);
    let mut ctx = ProcessContext::new(4, 0, 48_000);

    // Warmup: the FIRST arc-swap `load` may allocate once (epoch / guard
    // initialisation). This mirrors real RT usage — the RT thread performs its
    // first load during startup, BEFORE entering the steady processing loop.
    // The steady-state contract (alloc=0 every cycle after the first) is what
    // the measurement below verifies.
    handle.process_cycle(&mut ctx).unwrap();

    // MEASUREMENT WINDOW — bracket ONLY steady-state process_cycle (incl. the
    // arc-swap load). Must be 0 allocations: load is wait-free after warmup,
    // Graph::process_cycle is allocation-free.
    rt_start_measuring();
    for _ in 0..1000 {
        handle.process_cycle(&mut ctx).unwrap();
    }
    let n = rt_stop_and_count();
    assert_eq!(
        n, 0,
        "RT path (hot-reload): steady-state process_cycle (arc-swap load + Graph::process_cycle) allocated {n} times across 1000 cycles — RT-safe violation"
    );
}
