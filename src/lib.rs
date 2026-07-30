//! Real-time-safe directed node-graph audio processing engine for Rust.
//!
//! `audio-graph` schedules and drives a directed acyclic graph (DAG) of
//! [`AudioNode`][audio_core_bsd::AudioNode]s on a dedicated real-time (RT) audio
//! thread. The engine performs topological sorting at compile time, pre-allocates
//! every scratch buffer, and then runs [`Graph::process_cycle`] over the
//! pre-scheduled order without ever allocating, locking, or panicking.
//!
//! # Architecture
//!
//! 1. **Build phase** (non-RT): the caller adds nodes with [`Graph::add_node`]
//!    and wires output ports to input ports with [`Graph::link`].
//! 2. **Compile phase** (non-RT): [`Graph::compile`] runs Kahn's topological
//!    sort, rejects cycles, fixes the execution order, and pre-allocates a
//!    per-port scratch [`AudioFrame`][audio_core_bsd::AudioFrame] for every node.
//! 3. **RT phase**: [`Graph::process_cycle`] is invoked once per audio cycle
//!    on the RT thread. It copies upstream outputs into downstream inputs
//!    (bounded, alloc-free) and calls each node's `process` in dependency order.
//!
//! # Real-time safety boundary — CRITICAL
//!
//! [`Graph::process_cycle`] is the RT entry point. It **MUST NOT** allocate,
//! lock, panic, or perform any system call. This is achieved by:
//!
//! - Running the topological sort in [`Graph::compile`] (never in `process_cycle`).
//! - Pre-allocating **all** scratch frames in `compile()` so `process_cycle`
//!   never grows a `Vec`.
//! - Using only bounded `for` loops and slice indexing over pre-sized buffers.
//!
//! Conformance can be verified at test time with a counting allocator that
//! fails on any allocation observed inside `process_cycle`.
//!
//! # Example
//!
//! A minimal two-node graph (source → gain) driven for one cycle. The source
//! is a no-op node whose output is seeded via [`Graph::feed`]; the gain node
//! scales it by 0.5:
//!
//! ```
//! use audio_core_bsd::{
//!     AudioFrame, AudioNode, PortDescriptor, PortDirection, ProcessContext, SampleFormat,
//! };
//! use audio_graph_bsd::{Graph, GraphConfig};
//!
//! /// A source node with one output and a no-op `process`: its output scratch
//! /// is seeded externally via `Graph::feed` and left untouched each cycle.
//! struct SourceNode {
//!     out_p: [PortDescriptor; 1],
//! }
//! impl SourceNode {
//!     fn new() -> Self {
//!         Self {
//!             out_p: [PortDescriptor::output(1, SampleFormat::F32)],
//!         }
//!     }
//! }
//! impl AudioNode for SourceNode {
//!     fn inputs(&self) -> &[PortDescriptor] { &[] }
//!     fn outputs(&self) -> &[PortDescriptor] { &self.out_p }
//!     fn process(&mut self, _ctx: &mut ProcessContext, _i: &[AudioFrame], _o: &mut [AudioFrame]) {}
//! }
//!
//! /// Scales its single mono input by a fixed gain.
//! struct GainNode {
//!     gain: f32,
//!     in_p: [PortDescriptor; 1],
//!     out_p: [PortDescriptor; 1],
//! }
//! impl GainNode {
//!     fn new(gain: f32) -> Self {
//!         Self {
//!             gain,
//!             in_p: [PortDescriptor::input(1, SampleFormat::F32)],
//!             out_p: [PortDescriptor::output(1, SampleFormat::F32)],
//!         }
//!     }
//! }
//! impl AudioNode for GainNode {
//!     fn inputs(&self) -> &[PortDescriptor] { &self.in_p }
//!     fn outputs(&self) -> &[PortDescriptor] { &self.out_p }
//!     fn process(&mut self, _ctx: &mut ProcessContext, i: &[AudioFrame], o: &mut [AudioFrame]) {
//!         let (Some(inp), Some(out)) = (i.first(), o.get_mut(0)) else { return };
//!         let n = inp.samples.len().min(out.samples.len());
//!         for k in 0..n {
//!             out.samples[k] = inp.samples[k] * self.gain;
//!         }
//!     }
//! }
//!
//! let mut g = Graph::new();
//! let src = g.add_node(Box::new(SourceNode::new()));
//! let dst = g.add_node(Box::new(GainNode::new(0.5)));
//! g.link((src, 0), (dst, 0)).unwrap();
//! g.compile(GraphConfig::new(8, 48_000, 1)).unwrap();
//!
//! // Seed the source output with all-ones (survives process_cycle because
//! // SourceNode::process is a no-op), run one cycle.
//! g.feed(src, 0, &AudioFrame::from_planar(1, 48_000, vec![1.0; 8]));
//! let mut ctx = ProcessContext::new(8, 0, 48_000);
//! g.process_cycle(&mut ctx).unwrap();
//! assert!(g
//!     .read_output(dst, 0)
//!     .unwrap()
//!     .samples
//!     .iter()
//!     .all(|&x| (x - 0.5).abs() < 1e-6));
//! ```

#![cfg_attr(docsrs, feature(doc_cfg))]
#![warn(missing_docs)]
#![warn(clippy::all, clippy::pedantic)]

mod error;
mod graph;
mod ring;
mod topology;

// The serializable topology model — only present under the `topology` feature.
// When the feature is off the module, the serde dependency, and every type
// below are entirely absent, preserving the 0.1.0 serde-free contract.
#[cfg(feature = "topology")]
mod topology_pub;

// The distributed-prep abstractions — only present under the `distributed`
// feature (which implies `topology`). Provides serializable partition models,
// the network-link node contract, and the partition-hint generator. No sockets,
// Raft, or netmap — those live in sonicbrew (M07/M09).
#[cfg(feature = "distributed")]
mod distributed;

// Hot-reload handle (Phase C, ROADMAP §5 strategy B): atomic snapshot swap of a
// compiled Graph via `arc-swap`. The control thread builds a new Graph and
// publishes it; the RT thread does a wait-free load + alloc-free process_cycle.
#[cfg(feature = "distributed")]
mod hot_reload;

pub use error::GraphError;
pub use graph::{Graph, GraphConfig, LinkId, NodeId, PortIdx};
pub use ring::{RingSink, RingSource};

#[cfg(feature = "topology")]
pub use topology_pub::{
    Mutation, NodeSnapshot, PortDir, PortMeta, SampleFmt, SnapshotEdge, SnapshotSource,
    TopologyEvent, TopologyObserver, TopologySnapshot,
};

#[cfg(feature = "distributed")]
pub use distributed::{
    BoundaryPort, GraphPartition, NetworkLinkNode, PartitionHint, PortKind, RemoteNode,
    TransportHint,
};

#[cfg(feature = "distributed")]
pub use hot_reload::RtHandle;
