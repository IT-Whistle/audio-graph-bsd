//! Hot-reload handle: atomic snapshot swap of a compiled [`Graph`] (Phase C,
//! ROADMAP §5 strategy B).
//!
//! Gated behind the `distributed` feature (which brings in `arc-swap`).
//!
//! The control thread builds and compiles a NEW [`Graph`], then calls
//! [`RtHandle::install`] to atomically publish it. The RT thread calls
//! [`RtHandle::process_cycle`], which performs a wait-free load of the current
//! graph and runs one cycle. The old graph stays alive (via its `Arc`) until no
//! RT cycle is using it, then is dropped — so hot-reload is **lossless and
//! pause-free**.
//!
//! # Real-time safety
//!
//! `process_cycle` performs only a wait-free [`arc_swap::ArcSwap::load`] (no
//! allocation, no lock, no syscall) followed by the already-alloc-free
//! [`Graph::process_cycle`]. The single-RT-thread invariant on
//! [`crate::GraphScratch`] still holds: the RT thread is the sole mutator of the
//! loaded graph's scratch.

#![cfg(feature = "distributed")]

use std::sync::Arc;

use arc_swap::ArcSwap;

use crate::{Graph, GraphError};
use audio_core_bsd::ProcessContext;

/// A real-time handle to a [`Graph`] that can be atomically swapped without
/// stopping the RT thread.
///
/// Wrap a compiled [`Graph`] in an `RtHandle`; the RT thread drives cycles via
/// [`RtHandle::process_cycle`], and the control thread publishes replacements
/// via [`RtHandle::install`].
///
/// # Example
///
/// ```no_run
/// # use audio_graph_bsd::{Graph, GraphConfig, RtHandle};
/// # use audio_core_bsd::ProcessContext;
/// # let compiled_graph: Graph = { /* ...build + compile... */ Graph::new() };
/// let handle = RtHandle::new(compiled_graph);
///
/// // RT thread:
/// let mut ctx = ProcessContext::new(256, 0, 48_000);
/// handle.process_cycle(&mut ctx).unwrap();
///
/// // Control thread — publish a freshly-built graph without stopping RT:
/// let new_graph: Graph = { /* ...rebuild + recompile... */ Graph::new() };
/// handle.install(new_graph);
/// ```
pub struct RtHandle {
    inner: ArcSwap<Graph>,
}

impl RtHandle {
    /// Creates a handle owning `graph` (typically already compiled).
    #[must_use]
    pub fn new(graph: Graph) -> Self {
        Self {
            inner: ArcSwap::from_pointee(graph),
        }
    }

    /// Atomically publishes a new `graph`. **Control thread only.**
    ///
    /// The previously-installed graph is dropped once no RT cycle holds a guard
    /// to it (epoch-based reclamation via `arc-swap`).
    pub fn install(&self, graph: Graph) {
        self.inner.store(Arc::new(graph));
    }

    /// Loads the current graph and runs one RT cycle.
    ///
    /// Wait-free load (`arc-swap`); the graph's [`Graph::process_cycle`] is
    /// allocation-free. Returns [`GraphError::NotCompiled`] if the loaded graph
    /// was never compiled.
    ///
    /// # Errors
    ///
    /// Propagates [`GraphError`] from [`Graph::process_cycle`].
    pub fn process_cycle(&self, ctx: &mut ProcessContext) -> Result<(), GraphError> {
        // Wait-free load returns a Guard<Arc<Graph>>; derefs to &Graph.
        // process_cycle is &self, so no &mut-from-shared hazard.
        let guard = self.inner.load();
        guard.process_cycle(ctx)
    }

    /// Loads the current graph (wait-free) and returns a guard to it.
    ///
    /// Use for diagnostics or to call read-only `&self` accessors on the
    /// installed graph (e.g. [`Graph::read_output`]) after a cycle. The guard
    /// keeps the graph alive for the duration of the borrow.
    #[must_use]
    pub fn graph(&self) -> arc_swap::Guard<Arc<Graph>> {
        self.inner.load()
    }
}
