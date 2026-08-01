//! Lock-free ring-buffer bridges between worker threads and the RT graph.
//!
//! [`RingSource`] feeds audio **into** the graph from a worker producer, and
//! [`RingSink`] taps audio **out of** the graph for a worker consumer. Both
//! wrap an [`rtrb`] ring, whose `pop`/`push` operations are wait-free and
//! therefore safe to touch from the real-time thread — subject to the caveat
//! documented on [`RingSink`] (the `push` path clones, so it is deferred to a
//! non-RT [`RingSink::flush`]).

use audio_core_bsd::{
    AudioFrame, AudioNode, PortDescriptor, PortDirection, ProcessContext, SampleFormat,
};

/// A source node that drains audio from a worker thread via a lock-free ring.
///
/// `RingSource` holds the `Consumer` end of an [`rtrb`] ring. On every cycle it
/// pops at most one [`AudioFrame`] (wait-free) and writes it into its single
/// output port. If the ring is empty it repeats the last frame received, so the
/// downstream signal holds rather than clicking to silence.
///
/// # Real-time safety
///
/// `process` performs only a wait-free `pop` and bounded slice copies — no
/// allocation, lock, panic, or syscall. The `last` cache is pre-sized in
/// [`RingSource::new`] so mirroring an incoming frame never grows it; if an
/// incoming frame is shorter/longer than `last` the copy is bounded to the
/// smaller length (no realloc).
pub struct RingSource {
    /// Consumer end of the lock-free ring (wait-free `pop`).
    consumer: rtrb::Consumer<AudioFrame>,
    /// The single output port descriptor.
    out_port: [PortDescriptor; 1],
    /// Last frame received; reused when the ring is empty to avoid clicks.
    last: AudioFrame,
}

impl RingSource {
    /// Creates a new source draining `consumer`.
    ///
    /// `last` is pre-allocated to silence of the given size so that cycles
    /// before the first frame arrives emit silence rather than uninitialised
    /// data.
    #[must_use]
    pub fn new(
        consumer: rtrb::Consumer<AudioFrame>,
        channels: u16,
        sample_rate: u32,
        num_frames: usize,
    ) -> Self {
        Self {
            consumer,
            out_port: [PortDescriptor::new(
                PortDirection::Output,
                channels,
                SampleFormat::F32,
            )],
            last: AudioFrame::silence(channels, num_frames, sample_rate),
        }
    }

    /// Returns a shared reference to the underlying consumer (for diagnostics).
    #[must_use]
    pub fn consumer(&self) -> &rtrb::Consumer<AudioFrame> {
        &self.consumer
    }

    /// Returns a shared reference to the last received frame.
    #[must_use]
    pub fn last(&self) -> &AudioFrame {
        &self.last
    }
}

impl AudioNode for RingSource {
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
        out_frames: &mut [AudioFrame],
    ) {
        let Some(out) = out_frames.get_mut(0) else {
            return;
        };
        // Wait-free pop. On success, copy into both the output and `last`;
        // on failure, reuse `last`.
        if let Ok(frame) = self.consumer.pop() {
            out.channels = frame.channels;
            out.sample_rate = frame.sample_rate;
            let n = frame.samples.len().min(out.samples.len());
            out.samples[..n].copy_from_slice(&frame.samples[..n]);

            // Mirror into the pre-sized `last` cache (bounded, alloc-free).
            self.last.channels = frame.channels;
            self.last.sample_rate = frame.sample_rate;
            let cn = frame.samples.len().min(self.last.samples.len());
            self.last.samples[..cn].copy_from_slice(&frame.samples[..cn]);
        } else {
            // Repeat the last frame: bounded copy, no alloc.
            out.channels = self.last.channels;
            out.sample_rate = self.last.sample_rate;
            let n = self.last.samples.len().min(out.samples.len());
            out.samples[..n].copy_from_slice(&self.last.samples[..n]);
        }
    }
}

/// A sink node that stashes audio from the graph for a worker thread to drain.
///
/// `RingSink` holds the `Producer` end of an [`rtrb`] ring plus a stash frame.
/// On every cycle it copies its single input into the stash (bounded, RT-safe).
/// The stash is **not** pushed to the ring inside `process`, because
/// [`rtrb::Producer::push`] takes ownership and would force a heap clone of the
/// frame — that clone must happen off the real-time thread.
///
/// Call [`RingSink::flush`](Self::flush) from a worker thread to ship the
/// stashed frame across the ring to its consumer.
///
/// # Real-time safety
///
/// `process` performs only a bounded copy into the pre-sized `stash` — no
/// allocation, lock, panic, or syscall. The cloning `push` is deferred to the
/// non-RT [`RingSink::flush`].
pub struct RingSink {
    /// Producer end of the lock-free ring (pushed from a worker thread).
    producer: rtrb::Producer<AudioFrame>,
    /// The single input port descriptor.
    in_port: [PortDescriptor; 1],
    /// Stashed copy of the most recent input; drained by `flush`.
    stash: AudioFrame,
}

impl RingSink {
    /// Creates a new sink feeding `producer`.
    ///
    /// `stash` is pre-allocated to silence of the given size so the RT copy
    /// never grows it.
    #[must_use]
    pub fn new(
        producer: rtrb::Producer<AudioFrame>,
        channels: u16,
        sample_rate: u32,
        num_frames: usize,
    ) -> Self {
        Self {
            producer,
            in_port: [PortDescriptor::new(
                PortDirection::Input,
                channels,
                SampleFormat::F32,
            )],
            stash: AudioFrame::silence(channels, num_frames, sample_rate),
        }
    }

    /// Pushes the stashed frame into the ring.
    ///
    /// This **clones** the stash (allocating) and is therefore intended for the
    /// worker thread, **not** the real-time thread. The stash itself is left
    /// in place so the next cycle can overwrite it.
    ///
    /// # Errors
    ///
    /// Returns [`rtrb::PushError`] containing the unpushed frame when the ring
    /// has no free slot (treat as an xrun / dropped frame).
    pub fn flush(&mut self) -> Result<(), rtrb::PushError<AudioFrame>> {
        self.producer.push(self.stash.clone())
    }

    /// Returns a shared reference to the underlying producer (for diagnostics).
    #[must_use]
    pub fn producer(&self) -> &rtrb::Producer<AudioFrame> {
        &self.producer
    }

    /// Returns a shared reference to the stashed frame.
    #[must_use]
    pub fn stash(&self) -> &AudioFrame {
        &self.stash
    }
}

impl crate::Flushable for RingSink {
    /// Pushes the stashed frame into the ring (OFF-RT ONLY — clones/allocates).
    ///
    /// This is the [`Flushable`](crate::Flushable) contract the graph engine
    /// calls in the between-cycle window via
    /// [`Graph::flush_sinks`](crate::Graph::flush_sinks). It is equivalent to
    /// the inherent [`RingSink::flush`] but returns the graph-level
    /// [`FlushError`](crate::FlushError).
    fn flush(&mut self) -> Result<(), crate::FlushError> {
        self.producer
            .push(self.stash.clone())
            .map_err(|_| crate::FlushError::RingFull(1))
    }
}

impl AudioNode for RingSink {
    fn inputs(&self) -> &[PortDescriptor] {
        &self.in_port
    }
    fn outputs(&self) -> &[PortDescriptor] {
        &[]
    }
    fn process(
        &mut self,
        _ctx: &mut ProcessContext,
        in_frames: &[AudioFrame],
        _out_frames: &mut [AudioFrame],
    ) {
        let Some(inp) = in_frames.first() else {
            return;
        };
        // Bounded copy into the pre-sized stash — no allocation.
        self.stash.channels = inp.channels;
        self.stash.sample_rate = inp.sample_rate;
        let n = inp.samples.len().min(self.stash.samples.len());
        self.stash.samples[..n].copy_from_slice(&inp.samples[..n]);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use audio_core_bsd::{AudioNode, PortDirection, ProcessContext};

    /// Approximate float equality (uses `<`, never `==`).
    fn approx_eq(a: f32, b: f32) -> bool {
        (a - b).abs() < 1e-6
    }

    #[test]
    fn ring_source_outputs_one_port_and_no_inputs() {
        let (_producer, consumer) = rtrb::RingBuffer::<AudioFrame>::new(4);
        let src = RingSource::new(consumer, 1, 48_000, 8);
        assert_eq!(src.inputs().len(), 0);
        assert_eq!(src.outputs().len(), 1);
        assert_eq!(src.outputs()[0].direction, PortDirection::Output);
        assert_eq!(src.outputs()[0].channels, 1);
        assert_eq!(src.outputs()[0].sample_format, SampleFormat::F32);
    }

    #[test]
    fn ring_sink_inputs_one_port_and_no_outputs() {
        let (producer, _consumer) = rtrb::RingBuffer::<AudioFrame>::new(4);
        let sink = RingSink::new(producer, 2, 48_000, 8);
        assert_eq!(sink.inputs().len(), 1);
        assert_eq!(sink.outputs().len(), 0);
        assert_eq!(sink.inputs()[0].channels, 2);
        assert_eq!(sink.inputs()[0].direction, PortDirection::Input);
    }

    #[test]
    fn ring_source_pops_and_outputs_frame() {
        let (mut producer, consumer) = rtrb::RingBuffer::<AudioFrame>::new(4);
        producer
            .push(AudioFrame::from_planar(1, 48_000, vec![0.5; 8]))
            .unwrap();

        let mut src = RingSource::new(consumer, 1, 48_000, 8);
        let mut ctx = ProcessContext::new(8, 0, 48_000);
        let mut out = [AudioFrame::silence(1, 8, 48_000)];
        src.process(&mut ctx, &[], &mut out);
        assert!(out[0].samples.iter().all(|&s| approx_eq(s, 0.5)));
        // last should also mirror the popped frame.
        assert!(src.last().samples.iter().all(|&s| approx_eq(s, 0.5)));
    }

    #[test]
    fn ring_source_repeats_last_when_empty() {
        let (_producer, consumer) = rtrb::RingBuffer::<AudioFrame>::new(4);
        let mut src = RingSource::new(consumer, 1, 48_000, 8);
        let mut ctx = ProcessContext::new(8, 0, 48_000);
        let mut out = [AudioFrame::silence(1, 8, 48_000)];
        // Fill out with a sentinel to detect overwrite.
        for s in &mut out[0].samples {
            *s = 9.0;
        }
        // No frame available -> repeats last (silence) -> out becomes 0.0.
        src.process(&mut ctx, &[], &mut out);
        assert!(out[0].samples.iter().all(|&s| approx_eq(s, 0.0)));
    }

    #[test]
    fn ring_source_holds_after_underrun() {
        // After receiving one non-silence frame, an underrun must hold it.
        let (mut producer, consumer) = rtrb::RingBuffer::<AudioFrame>::new(4);
        producer
            .push(AudioFrame::from_planar(1, 48_000, vec![0.7; 4]))
            .unwrap();
        let mut src = RingSource::new(consumer, 1, 48_000, 4);
        let mut ctx = ProcessContext::new(4, 0, 48_000);
        let mut out = [AudioFrame::silence(1, 4, 48_000)];
        src.process(&mut ctx, &[], &mut out); // pops 0.7
        assert!(out[0].samples.iter().all(|&s| approx_eq(s, 0.7)));
        // Now underrun: must hold 0.7, not collapse to silence.
        src.process(&mut ctx, &[], &mut out);
        assert!(out[0].samples.iter().all(|&s| approx_eq(s, 0.7)));
    }

    #[test]
    fn ring_sink_stashes_input() {
        let (producer, _consumer) = rtrb::RingBuffer::<AudioFrame>::new(4);
        let mut sink = RingSink::new(producer, 1, 48_000, 8);
        let inp = [AudioFrame::from_planar(1, 48_000, vec![0.25; 8])];
        let mut ctx = ProcessContext::new(8, 0, 48_000);
        let mut out_dummy: [AudioFrame; 0] = [];
        sink.process(&mut ctx, &inp, &mut out_dummy);
        assert!(sink.stash().samples.iter().all(|&s| approx_eq(s, 0.25)));
    }

    #[test]
    fn ring_sink_flush_pushes_to_consumer() {
        let (producer, mut consumer) = rtrb::RingBuffer::<AudioFrame>::new(4);
        let mut sink = RingSink::new(producer, 1, 48_000, 4);
        let inp = [AudioFrame::from_planar(1, 48_000, vec![1.0; 4])];
        let mut ctx = ProcessContext::new(4, 0, 48_000);
        let mut out_dummy: [AudioFrame; 0] = [];
        sink.process(&mut ctx, &inp, &mut out_dummy);
        sink.flush().unwrap();
        let popped = consumer.pop().unwrap();
        assert!(popped.samples.iter().all(|&s| approx_eq(s, 1.0)));
    }

    #[test]
    fn ring_sink_flush_full_ring_errors() {
        let (producer, _consumer) = rtrb::RingBuffer::<AudioFrame>::new(1);
        let mut sink = RingSink::new(producer, 1, 48_000, 2);
        // Fill the ring (capacity 1).
        sink.flush().unwrap();
        // Second flush must fail with PushError::Full.
        assert!(sink.flush().is_err());
    }

    #[test]
    fn ring_source_last_starts_silent() {
        let (_producer, consumer) = rtrb::RingBuffer::<AudioFrame>::new(2);
        let src = RingSource::new(consumer, 1, 48_000, 4);
        assert_eq!(src.last().channels, 1);
        assert_eq!(src.last().sample_rate, 48_000);
        assert!(src.last().samples.iter().all(|&s| approx_eq(s, 0.0)));
    }

    #[test]
    fn ring_sink_stash_starts_silent() {
        let (producer, _consumer) = rtrb::RingBuffer::<AudioFrame>::new(2);
        let sink = RingSink::new(producer, 2, 44_100, 4);
        assert_eq!(sink.stash().channels, 2);
        assert_eq!(sink.stash().sample_rate, 44_100);
        assert_eq!(sink.stash().samples.len(), 8);
        assert!(sink.stash().samples.iter().all(|&s| approx_eq(s, 0.0)));
    }
}
