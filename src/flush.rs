//! Flushable sink contract + errors (engine-changes §4, Option A).
//!
//! A node whose output must be shipped off the RT thread (e.g. [`crate::RingSink`])
//! implements [`Flushable`]. The graph engine calls [`Flushable::flush`] in the
//! **between-cycle** (off-RT) window to push the stashed frame across the ring.
//!
//! `SinkNode` is the combined contract `AudioNode + Flushable` so the engine can
//! track flushable sinks by type, without `Any` downcast.

use crate::NodeId;

/// Error from flushing a sink's stashed frame across its ring.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum FlushError {
    /// The ring buffer was full (an xrun); the stashed frame was dropped.
    #[error("ring full (xrun): {0} frame(s) dropped")]
    RingFull(usize),
    /// The addressed node is not a flushable sink.
    #[error("node {0} is not a flushable sink")]
    NotFlushable(NodeId),
    /// The addressed node id does not exist.
    #[error("node {0} not found")]
    NodeNotFound(NodeId),
}

/// A node that buffers output off the RT thread and must be flushed between
/// cycles to actually ship its data.
///
/// `flush` is **OFF-RT ONLY**: it typically clones the stashed frame and pushes
/// it through an `rtrb::Producer` (allocating). Never call it from the RT
/// [`crate::Graph::process_cycle`] path.
pub trait Flushable {
    /// Push the stashed frame across the ring. Returns an error if the ring is
    /// full (treat as an xrun / dropped frame).
    ///
    /// # Errors
    ///
    /// Returns [`FlushError::RingFull`] if the ring has no free slot.
    fn flush(&mut self) -> Result<(), FlushError>;
}

/// A sink node: both an [`audio_core_bsd::AudioNode`] (so it can be scheduled in
/// the graph) and [`Flushable`] (so the engine can drain it between cycles).
///
/// Any type implementing both supertraits is automatically a `SinkNode`.
pub trait SinkNode: audio_core_bsd::AudioNode + Flushable {}

impl<T> SinkNode for T where T: audio_core_bsd::AudioNode + Flushable {}

#[cfg(test)]
mod tests {
    use super::FlushError;

    #[test]
    fn flush_error_display() {
        assert_eq!(
            FlushError::RingFull(3).to_string(),
            "ring full (xrun): 3 frame(s) dropped"
        );
        assert_eq!(
            FlushError::NotFlushable(7).to_string(),
            "node 7 is not a flushable sink"
        );
        assert_eq!(
            FlushError::NodeNotFound(2).to_string(),
            "node 2 not found"
        );
    }

    #[test]
    fn flush_error_clone_eq() {
        let e = FlushError::RingFull(1);
        assert_eq!(e.clone(), e);
        assert_ne!(FlushError::RingFull(1), FlushError::NodeNotFound(1));
    }
}
