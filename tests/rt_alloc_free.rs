//! RT-safety verification: `Graph::process_cycle` performs zero heap
//! allocations.
//!
//! Uses a counting global allocator (TESTING-STANDARDS §3.2 / BUILD-PLAN
//! §4.3). The build/compile phase allocates freely; the measurement window
//! brackets ONLY the `process_cycle` loop, asserting zero allocations across
//! 1000 cycles.
//!
//! # Why thread-local counters
//!
//! A global `MEASURING` flag is unreliable in a `cargo test` harness: even a
//! single test binary runs the test body on one thread while the libtest
//! harness performs bookkeeping on others, and under full-suite parallel
//! execution those background allocations intermittently land inside the
//! measurement window, producing false positives (observed: 4 phantom
//! allocations in ~1/5 full-suite runs).
//!
//! The fix is to count allocations **per thread**: the allocator increments a
//! thread-local counter only while the *current* thread has set its measuring
//! flag. Background/harness threads have their own independent counters that are
//! never read, so the RT-thread measurement is deterministic regardless of
//! what other threads do. This mirrors the RT-safety contract itself: a single
//! dedicated RT thread is the only thing that ever touches the RT path.

use std::alloc::{GlobalAlloc, Layout, System};
use std::cell::Cell;

use audio_core_bsd::{AudioFrame, AudioNode, PortDescriptor, ProcessContext, SampleFormat};
use audio_graph_bsd::{Graph, GraphConfig};

// ---------------------------------------------------------------------------
// Counting global allocator with THREAD-LOCAL measurement windows.
// ---------------------------------------------------------------------------

struct CountingAllocator;

thread_local! {
    /// Per-thread "am I currently measuring?" flag.
    static RT_MEASURING: Cell<bool> = const { Cell::new(false) };
    /// Per-thread allocation counter (only incremented while measuring).
    static RT_ALLOC_COUNT: Cell<usize> = const { Cell::new(0) };
}

unsafe impl GlobalAlloc for CountingAllocator {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        // `try_with` returns Err before this thread's locals exist (e.g. during
        // their own lazy init, causing re-entrancy). In that case the measuring
        // flag is necessarily unset, so we skip counting — safe and correct.
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

/// Enables per-thread counting and resets the counter to zero.
fn rt_start_measuring() {
    RT_ALLOC_COUNT.with(|c| c.set(0));
    RT_MEASURING.with(|m| m.set(true));
}

/// Disables per-thread counting and returns how many allocations were observed.
fn rt_stop_and_count() -> usize {
    RT_MEASURING.with(|m| m.set(false));
    RT_ALLOC_COUNT.with(|c| c.get())
}

// ---------------------------------------------------------------------------
// Local test-only AudioNode implementations (integration tests cannot see the
// crate's private `#[cfg(test)]` helpers).
// ---------------------------------------------------------------------------

/// A source node: zero inputs, one output, no-op `process`.
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

/// A mono gain node: 1 in, 1 out, scales by `gain`.
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

// ---------------------------------------------------------------------------
// The alloc=0 proof — two measurement windows in one test.
// ---------------------------------------------------------------------------

/// `Graph::process_cycle` must perform **zero** heap allocations across 1000
/// cycles, verified for two independent topologies:
///
/// 1. A plain gain-node chain (`src → gain(0.5) → gain(2.0)`).
/// 2. An rtrb-backed ring topology (`RingSource → gain → RingSink`).
///
/// Each sub-scenario has its own bracketed measurement window; build/compile
/// allocations are excluded. The `RingSink::flush` clone-path is deliberately
/// NOT called inside the loop (it is a worker-thread operation).
#[test]
fn process_cycle_is_alloc_free_across_1000_cycles() {
    // ======================================================================
    // Sub-scenario 1: gain-node chain.
    // ======================================================================
    {
        let mut g = Graph::new();
        let src = g.add_node(Box::new(SourceNode::new(1)));
        let a = g.add_node(Box::new(GainNode::new(0.5)));
        let b = g.add_node(Box::new(GainNode::new(2.0)));
        g.link((src, 0), (a, 0)).unwrap();
        g.link((a, 0), (b, 0)).unwrap();
        g.compile(GraphConfig::new(256, 48_000, 1)).unwrap();
        g.feed(src, 0, &AudioFrame::from_planar(1, 48_000, vec![0.5; 256]));
        let mut ctx = ProcessContext::new(256, 0, 48_000);

        rt_start_measuring();
        for _ in 0..1000 {
            g.process_cycle(&mut ctx).unwrap();
        }
        let n = rt_stop_and_count();
        assert_eq!(
            n, 0,
            "RT path (gain chain): process_cycle allocated {n} times across 1000 cycles — RT-safe violation"
        );
    } // g drops here (dealloc only, not counted)

    // ======================================================================
    // Sub-scenario 2: rtrb-backed ring topology.
    // ======================================================================
    {
        const N: usize = 128;
        let (mut prod_in, cons_in) = rtrb::RingBuffer::<AudioFrame>::new(4);
        let (prod_out, cons_out) = rtrb::RingBuffer::<AudioFrame>::new(4);

        let mut g = Graph::new();
        let rsrc = g.add_node(Box::new(audio_graph_bsd::RingSource::new(
            cons_in, 1, 48_000, N,
        )));
        let gain = g.add_node(Box::new(GainNode::new(1.0)));
        let rsink = g.add_node(Box::new(audio_graph_bsd::RingSink::new(
            prod_out, 1, 48_000, N,
        )));
        g.link((rsrc, 0), (gain, 0)).unwrap();
        g.link((gain, 0), (rsink, 0)).unwrap();
        g.compile(GraphConfig::new(N, 48_000, 1)).unwrap();

        // Pre-fill the input ring so RingSource has frames to pop during the
        // measured loop (done OUTSIDE the measurement window).
        for _ in 0..4 {
            let _ = prod_in.push(AudioFrame::from_planar(1, 48_000, vec![0.25; N]));
        }
        let mut ctx = ProcessContext::new(N, 0, 48_000);

        rt_start_measuring();
        for _ in 0..1000 {
            g.process_cycle(&mut ctx).unwrap();
            // NOTE: RingSink::flush() is deliberately NOT called here — it
            // clones (allocates) and is a worker-thread operation. The RT path
            // only copies into the pre-sized stash.
        }
        let n = rt_stop_and_count();
        assert_eq!(
            n, 0,
            "RT path (ring nodes): process_cycle allocated {n} times across 1000 cycles — RT-safe violation"
        );

        drop(cons_out);
    }
}
