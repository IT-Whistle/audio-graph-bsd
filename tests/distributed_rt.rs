//! G6: network-link node RT-safety (ROADMAP §9.1).
//!
//! A network link node follows the same rtrb bridge pattern as `RingSource` /
//! `RingSink` (§4): worker thread does RTP recv/send + Producer push (alloc
//! OK, non-RT); the RT `process` does wait-free Consumer pop + bounded copy
//! (alloc-free). This test proves that pattern inside a `GraphPartition`:
//! `RingSource` (network input) → Gain → `RingSink` (network output), run for
//! 1000 `process_cycle` calls, must perform ZERO allocations.
//!
//! Uses a thread-local counting allocator (same pattern as `rt_alloc_free.rs`)
//! so only the measuring thread's allocations are counted — robust to
//! cargo-test parallelism.
#![cfg(feature = "distributed")]

use std::alloc::{GlobalAlloc, Layout, System};
use std::cell::Cell;

use audio_core_bsd::{AudioFrame, AudioNode, PortDescriptor, PortDirection, ProcessContext, SampleFormat};
use audio_graph_bsd::{Graph, GraphConfig, GraphPartition, RingSink, RingSource};

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
// A gain node for the partition's interior.
// ---------------------------------------------------------------------------

struct Gain {
    g: f32,
    inp: [PortDescriptor; 1],
    out: [PortDescriptor; 1],
}
impl Gain {
    fn new(g: f32) -> Self {
        Self {
            g,
            inp: [PortDescriptor::new(PortDirection::Input, 1, SampleFormat::F32)],
            out: [PortDescriptor::new(PortDirection::Output, 1, SampleFormat::F32)],
        }
    }
}
impl AudioNode for Gain {
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

/// A `GraphPartition` whose RT path traverses a network-input `RingSource`,
/// an interior gain node, and a network-output `RingSink` — the exact shape a
/// distributed partition has. `process_cycle` must be allocation-free.
#[test]
fn graph_partition_with_network_links_is_alloc_free() {
    const N: usize = 128;
    let (mut prod_in, cons_in) = rtrb::RingBuffer::<AudioFrame>::new(4);
    let (prod_out, _cons_out) = rtrb::RingBuffer::<AudioFrame>::new(4);

    let mut g = Graph::new();
    let rsrc = g.add_node(Box::new(RingSource::new(cons_in, 1, 48_000, N)));
    let gain = g.add_node(Box::new(Gain::new(1.0)));
    let rsink = g.add_node(Box::new(RingSink::new(prod_out, 1, 48_000, N)));
    g.link((rsrc, 0), (gain, 0)).unwrap();
    g.link((gain, 0), (rsink, 0)).unwrap();

    let mut partition = GraphPartition::new(g, vec![]);
    partition.compile(GraphConfig::new(N, 48_000, 1)).unwrap();

    // Pre-fill the input ring OUTSIDE the measurement window.
    for _ in 0..4 {
        let _ = prod_in.push(AudioFrame::from_planar(1, 48_000, vec![0.25; N]));
    }
    let mut ctx = ProcessContext::new(N, 0, 48_000);

    // MEASUREMENT WINDOW — bracket ONLY process_cycle.
    rt_start_measuring();
    for _ in 0..1000 {
        partition.process_cycle(&mut ctx).unwrap();
        // RingSink::flush() is deliberately NOT called — it clones (allocates)
        // and is a worker-thread operation. The RT path only copies into stash.
    }
    let n = rt_stop_and_count();
    assert_eq!(
        n, 0,
        "RT path (network-link partition): process_cycle allocated {n} times across 1000 cycles — RT-safe violation"
    );
}
