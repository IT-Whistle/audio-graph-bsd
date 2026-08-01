//! RT-safety regression for the flush-gap (engine-changes §7.1).
//!
//! Verifies that interleaving `flush_sinks()` between cycles does NOT pollute
//! the RT `process_cycle` path with allocations. The measurement window is
//! scoped to ONLY `process_cycle` (toggled per iteration); `flush_sinks` (which
//! legitimately clones/pushes — off-RT) runs outside the window. After 1000
//! interleaved (cycle, flush) pairs, the cumulative RT allocation count must be
//! 0.
//!
//! Uses a thread-local counting allocator (same pattern as `rt_alloc_free.rs`)
//! so only the measuring thread's allocations are counted.

use std::alloc::{GlobalAlloc, Layout, System};
use std::cell::Cell;

use audio_core_bsd::{AudioFrame, AudioNode, PortDescriptor, PortDirection, ProcessContext, SampleFormat};
use audio_graph_bsd::{Graph, GraphConfig, RingSink, RingSource};

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

/// A mono gain node (1 in / 1 out).
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

#[test]
fn process_cycle_stays_alloc_free_with_interleaved_flush() {
    const N: usize = 128;
    let (mut prod_in, cons_in) = rtrb::RingBuffer::<AudioFrame>::new(64);
    let (prod_out, mut cons_out) = rtrb::RingBuffer::<AudioFrame>::new(64);

    let mut g = Graph::new();
    let rsrc = g.add_node(Box::new(RingSource::new(cons_in, 1, 48_000, N)));
    let gain = g.add_node(Box::new(Gain::new(1.0)));
    // Register the sink via add_sink so flush_sinks can drain it.
    let rsink = g.add_sink(Box::new(RingSink::new(prod_out, 1, 48_000, N)));
    g.link((rsrc, 0), (gain, 0)).unwrap();
    g.link((gain, 0), (rsink, 0)).unwrap();
    g.compile(GraphConfig::new(N, 48_000, 1)).unwrap();

    // Pre-fill the input ring OUTSIDE the measurement.
    for _ in 0..32 {
        let _ = prod_in.push(AudioFrame::from_planar(1, 48_000, vec![0.25; N]));
    }
    let mut ctx = ProcessContext::new(N, 0, 48_000);

    // 1000 interleaved (cycle, flush) pairs. The RT window brackets ONLY
    // process_cycle; flush_sinks runs with MEASURING=false (its clone+push is
    // legitimately off-RT).
    RT_ALLOC_COUNT.with(|c| c.set(0));
    for _ in 0..1000 {
        RT_MEASURING.with(|m| m.set(true));
        g.process_cycle(&mut ctx).unwrap();
        RT_MEASURING.with(|m| m.set(false));
        // Drain the stash between cycles (off-RT). The consumer pops so the
        // ring never fills up (avoids RingFull noise — we want the happy path).
        let _ = g.flush_sinks();
        // Pop on the worker side (off-RT) so the ring has room.
        while cons_out.pop().is_ok() {}
    }

    let n = RT_ALLOC_COUNT.with(|c| c.get());
    assert_eq!(
        n, 0,
        "RT path: process_cycle allocated {n} times across 1000 cycles even though \
         flush_sinks runs outside the RT window — flush-gap leaked allocations into the RT path"
    );
}

#[test]
fn flush_sinks_between_cycles_does_not_corrupt_signal() {
    // Sanity: interleaved flush ships the correct signal end-to-end.
    const N: usize = 16;
    let (mut prod_in, cons_in) = rtrb::RingBuffer::<AudioFrame>::new(8);
    let (prod_out, mut cons_out) = rtrb::RingBuffer::<AudioFrame>::new(8);

    let mut g = Graph::new();
    let rsrc = g.add_node(Box::new(RingSource::new(cons_in, 1, 48_000, N)));
    let gain = g.add_node(Box::new(Gain::new(2.0)));
    let rsink = g.add_sink(Box::new(RingSink::new(prod_out, 1, 48_000, N)));
    g.link((rsrc, 0), (gain, 0)).unwrap();
    g.link((gain, 0), (rsink, 0)).unwrap();
    g.compile(GraphConfig::new(N, 48_000, 1)).unwrap();

    // Push a known signal (0.5 ramp) into the input ring.
    let _ = prod_in.push(AudioFrame::from_planar(1, 48_000, vec![0.5; N]));
    let mut ctx = ProcessContext::new(N, 0, 48_000);

    g.process_cycle(&mut ctx).unwrap();
    let (count, err) = g.flush_sinks();
    assert_eq!(count, 1);
    assert!(err.is_none());

    let frame = cons_out.pop().expect("flushed frame");
    // gain 2.0 → 0.5 * 2.0 = 1.0.
    assert!(frame.samples.iter().all(|&s| (s - 1.0).abs() < 1e-6));
}
