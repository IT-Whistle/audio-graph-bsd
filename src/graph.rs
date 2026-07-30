//! The real-time-safe audio graph engine.
//!
//! [`Graph`] owns a set of [`AudioNode`](audio_core_bsd::AudioNode) trait objects,
//! the directed edges between their ports, and the pre-allocated scratch frames
//! used to shuttle audio between nodes on the real-time thread. The compile /
//! build phases allocate freely; [`Graph::process_cycle`] does not.

use std::cell::UnsafeCell;

use crate::error::GraphError;
use crate::topology::{topological_sort, Edge};
use audio_core_bsd::{AudioFrame, AudioNode, PortDirection, ProcessContext};

// The `topology` feature brings in the serializable snapshot model and the
// subscriber channel used to emit TopologyEvent on the non-RT mutation path.
#[cfg(feature = "topology")]
use crate::topology_pub::{
    NodeSnapshot, PortMeta, SnapshotEdge, SnapshotSource, TopologyEvent, TopologySnapshot,
};

// The `distributed` feature (which implies `topology`) brings in the
// distributed-prep models used by `partition_hints` below.
#[cfg(feature = "distributed")]
use crate::distributed::{BoundaryPort, PartitionHint, PortKind, RemoteNode};
#[cfg(feature = "distributed")]
use crate::topology_pub::PortDir;

/// Identifier of a node within a [`Graph`]. Stable for the lifetime of the graph.
pub type NodeId = usize;

/// Index of a port on a node, in the order reported by the node's
/// [`inputs`](audio_core_bsd::AudioNode::inputs) /
/// [`outputs`](audio_core_bsd::AudioNode::outputs).
pub type PortIdx = usize;

/// Identifier of a link returned by [`Graph::link`]. Equal to the link's
/// position in insertion order.
pub type LinkId = usize;

/// Compile-time configuration fixing the size of every scratch buffer.
///
/// All fields are fixed at [`Graph::compile`] time so that
/// [`Graph::process_cycle`] can rely on every buffer being pre-sized to exactly
/// `channels * num_frames` samples.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct GraphConfig {
    /// Number of audio frames processed per cycle (per channel).
    pub num_frames: usize,
    /// Sample rate in Hz.
    pub sample_rate: u32,
    /// Channel count carried by every port.
    pub channels: u16,
}

impl GraphConfig {
    /// Creates a new configuration from its raw fields.
    #[must_use]
    pub const fn new(num_frames: usize, sample_rate: u32, channels: u16) -> Self {
        Self {
            num_frames,
            sample_rate,
            channels,
        }
    }
}

/// The real-time-mutated state of a [`Graph`], held behind a single
/// [`UnsafeCell`].
///
/// All three fields are touched on every [`Graph::process_cycle`]:
/// `nodes` via [`AudioNode::process`](audio_core_bsd::AudioNode::process)
/// (which takes `&mut self`), and the scratch frame vectors via the
/// upstream-output copy. `nodes` lives here — rather than as a plain field
/// — because calling `process(&mut self, …)` from a `&self` entry point is
/// impossible without interior mutability; bundling it with the scratch
/// vectors keeps the whole RT-mutated surface behind **one** cell.
///
/// # Soundness
///
/// The [`UnsafeCell`] wrapping this struct is sound because the real-time
/// thread is the *sole* mutator during processing, and the caller guarantees
/// that no `process_cycle` / `compile` / `feed` runs concurrently on the same
/// [`Graph`] (single-RT-thread invariant). Every `&mut self` build/compile
/// method uses the safe [`UnsafeCell::get_mut`] accessor; only
/// [`Graph::process_cycle`] (`&self`) and the read-only `&self` accessors
/// (`read_output`, `read_input`, `node_count`, …) use the `unsafe`
/// dereference, each annotated with a `# Safety` rationale.
struct GraphScratch {
    /// The nodes, indexed by [`NodeId`].
    nodes: Vec<Box<dyn AudioNode>>,
    /// Per-node, per-input-port scratch frame. Indexed `[node][port]`.
    input_scratch: Vec<Vec<AudioFrame>>,
    /// Per-node, per-output-port scratch frame. Indexed `[node][port]`.
    output_scratch: Vec<Vec<AudioFrame>>,
}

/// A real-time-safe directed acyclic graph of audio nodes.
///
/// A `Graph` moves through three phases:
///
/// 1. **Build** — call [`Graph::add_node`] to register nodes and
///    [`Graph::link`] to wire output ports to input ports.
/// 2. **Compile** — call [`Graph::compile`] with a [`GraphConfig`]. This runs
///    the topological sort, rejects cycles, and pre-allocates every scratch
///    frame.
/// 3. **Run** — call [`Graph::process_cycle`] once per audio cycle on the RT
///    thread. Use [`Graph::feed`] to seed external inputs before a cycle and
///    [`Graph::read_output`] / [`Graph::read_input`] to tap results after.
///
/// See the crate-level documentation for the real-time safety contract.
pub struct Graph {
    /// The directed edges (output-port -> input-port).
    edges: Vec<Edge>,
    /// Node execution order, filled by [`Graph::compile`].
    execution_order: Vec<NodeId>,
    /// Compile-time configuration.
    config: GraphConfig,
    /// Whether [`Graph::compile`] has run.
    compiled: bool,
    /// RT scratch + nodes, accessed via `&self` on the single RT thread.
    ///
    /// [`UnsafeCell`] + the single-RT-thread invariant makes `&self`-based
    /// `process_cycle` sound; the cell is never shared across threads for
    /// mutation. See `GraphScratch` for the soundness argument.
    scratch: UnsafeCell<GraphScratch>,
    /// Subscribers notified of topology events on the non-RT mutation path.
    /// Absent entirely without the `topology` feature (no dependency on
    /// `mpsc` or `TopologyEvent` in the default build).
    #[cfg(feature = "topology")]
    subscribers: Vec<std::sync::mpsc::Sender<TopologyEvent>>,
}

impl Graph {
    /// Creates an empty graph.
    #[must_use]
    pub fn new() -> Self {
        Self {
            edges: Vec::new(),
            execution_order: Vec::new(),
            config: GraphConfig::new(0, 0, 0),
            compiled: false,
            scratch: UnsafeCell::new(GraphScratch {
                nodes: Vec::new(),
                input_scratch: Vec::new(),
                output_scratch: Vec::new(),
            }),
            #[cfg(feature = "topology")]
            subscribers: Vec::new(),
        }
    }

    /// Adds a node to the graph and returns its stable [`NodeId`].
    ///
    /// The returned id equals the node's index in insertion order and never
    /// changes for the lifetime of the graph. Scratch frames for the node's
    /// ports are allocated later in [`Graph::compile`].
    #[must_use]
    pub fn add_node(&mut self, node: Box<dyn AudioNode>) -> NodeId {
        let scratch = self.scratch.get_mut();
        let id = scratch.nodes.len();
        scratch.nodes.push(node);
        // Keep the scratch index aligned with the node id; the actual per-port
        // frames are allocated in compile().
        scratch.input_scratch.push(Vec::new());
        scratch.output_scratch.push(Vec::new());
        // Notify topology subscribers (non-RT; no-op without the feature).
        #[cfg(feature = "topology")]
        self.emit_node_added(id);
        id
    }

    /// Links an output port to an input port, validating both endpoints and
    /// their compatibility.
    ///
    /// The `from` port must be an **output** and the `to` port an **input**;
    /// their channel counts and sample formats must match. The edge is stored
    /// but the graph is **not** recompiled — call [`Graph::compile`] (or design
    /// the full topology before compiling) before processing.
    ///
    /// # Errors
    ///
    /// Returns [`GraphError::NodeNotFound`] if either node does not exist,
    /// [`GraphError::PortNotFound`] if a port index is out of range,
    /// [`GraphError::PortDirectionMismatch`] if the directions are wrong, or
    /// [`GraphError::PortIncompatible`] if channel/format differ.
    pub fn link(
        &mut self,
        from: (NodeId, PortIdx),
        to: (NodeId, PortIdx),
    ) -> Result<LinkId, GraphError> {
        let (from_node, from_port) = from;
        let (to_node, to_port) = to;

        // Validate ports and directions without holding mutable borrows across
        // the later edges.push().
        let (from_desc, to_desc) = {
            let scratch = self.scratch.get_mut();
            let from_n = scratch
                .nodes
                .get(from_node)
                .ok_or(GraphError::NodeNotFound(from_node))?;
            let to_n = scratch
                .nodes
                .get(to_node)
                .ok_or(GraphError::NodeNotFound(to_node))?;
            let from_desc = from_n
                .outputs()
                .get(from_port)
                .ok_or(GraphError::PortNotFound {
                    node: from_node,
                    port: from_port,
                })?;
            let to_desc = to_n.inputs().get(to_port).ok_or(GraphError::PortNotFound {
                node: to_node,
                port: to_port,
            })?;
            (*from_desc, *to_desc)
        };

        if from_desc.direction != PortDirection::Output || to_desc.direction != PortDirection::Input
        {
            return Err(GraphError::PortDirectionMismatch { from, to });
        }
        if from_desc.channels != to_desc.channels
            || from_desc.sample_format != to_desc.sample_format
        {
            return Err(GraphError::PortIncompatible { from, to });
        }

        let link_id = self.edges.len();
        self.edges.push(Edge { from, to });
        // Notify topology subscribers (non-RT; no-op without the feature).
        #[cfg(feature = "topology")]
        self.emit_event(&TopologyEvent::LinkAdded(SnapshotEdge { from, to }));
        Ok(link_id)
    }

    /// Compiles the graph: topologically sorts the nodes and pre-allocates every
    /// scratch frame.
    ///
    /// This is the only place allocation is permitted. After `compile`
    /// succeeds, [`Graph::process_cycle`] is guaranteed to be allocation-free.
    ///
    /// # Errors
    ///
    /// Returns [`GraphError::AlreadyCompiled`] if called twice, or
    /// [`GraphError::CycleDetected`] if the topology contains a cycle.
    pub fn compile(&mut self, config: GraphConfig) -> Result<(), GraphError> {
        if self.compiled {
            return Err(GraphError::AlreadyCompiled);
        }
        let order = topological_sort(self.scratch.get_mut().nodes.len(), &self.edges)
            .map_err(|remaining| GraphError::CycleDetected { nodes: remaining })?;
        self.execution_order = order;
        self.config = config;

        // Pre-allocate every per-port scratch frame so process_cycle never
        // allocates. This is the ONLY place allocation is permitted.
        let scratch = self.scratch.get_mut();
        scratch.input_scratch = Vec::with_capacity(scratch.nodes.len());
        scratch.output_scratch = Vec::with_capacity(scratch.nodes.len());
        for node in &scratch.nodes {
            let in_slots: Vec<AudioFrame> = node
                .inputs()
                .iter()
                .map(|_| {
                    AudioFrame::silence(config.channels, config.num_frames, config.sample_rate)
                })
                .collect();
            let out_slots: Vec<AudioFrame> = node
                .outputs()
                .iter()
                .map(|_| {
                    AudioFrame::silence(config.channels, config.num_frames, config.sample_rate)
                })
                .collect();
            scratch.input_scratch.push(in_slots);
            scratch.output_scratch.push(out_slots);
        }
        self.compiled = true;
        // Notify topology subscribers that the graph is now compiled (non-RT).
        #[cfg(feature = "topology")]
        self.emit_event(&TopologyEvent::GraphCompiled);
        Ok(())
    }

    /// Processes one audio cycle on the real-time thread.
    ///
    /// For each node in dependency order this copies connected upstream outputs
    /// into the node's input scratch (or zeroes unconnected inputs), then
    /// invokes the node's
    /// [`process`](audio_core_bsd::AudioNode::process). The whole pass is bounded
    /// and allocation-free: every slice is pre-sized by [`Graph::compile`] and
    /// only bounded `for` loops / slice copies are used.
    ///
    /// # Safety (caller invariant)
    ///
    /// This method takes `&self` (not `&mut self`) so it can be called on a
    /// [`Graph`] loaded from a shared handle such as `ArcSwap<Graph>`. The
    /// interior mutability of the scratch cell is sound **only** because the
    /// caller guarantees the real-time thread is single and exclusive — no
    /// other `process_cycle`, `compile`, `feed`, or `read_*` may run
    /// concurrently on the same [`Graph`]. In practice the control thread
    /// builds and compiles a *new* `Graph` before swapping it in, so the old
    /// instance being processed is never touched from another thread.
    ///
    /// # Real-time safety
    ///
    /// This method performs **no** allocation, locking, panicking, or system
    /// call. The single `Err(NotCompiled)` return on the uncompiled path is a
    /// stack-only branch (the variant carries no heap data); on the happy path
    /// the method returns `Ok(())`.
    ///
    /// # Errors
    ///
    /// Returns [`GraphError::NotCompiled`] if [`Graph::compile`] has not been
    /// called.
    pub fn process_cycle(&self, ctx: &mut ProcessContext) -> Result<(), GraphError> {
        if !self.compiled {
            return Err(GraphError::NotCompiled);
        }

        // SAFETY: the caller guarantees no concurrent access to this Graph's
        // scratch — see the method-level "# Safety" note. The mutable borrow
        // below is exclusive on the single RT thread and never overlaps a
        // `&mut self` build/compile call (the borrow checker forbids `&self`
        // and `&mut self` from coexisting on the same Graph).
        let GraphScratch {
            nodes,
            input_scratch,
            output_scratch,
        } = unsafe { &mut *self.scratch.get() };

        for &n in &self.execution_order {
            // (a) Fill this node's input slots from upstream outputs, or zero them.
            let Some(in_slots) = input_scratch.get_mut(n) else {
                continue;
            };
            for (pi, slot) in in_slots.iter_mut().enumerate() {
                let mut sourced = false;
                for edge in &self.edges {
                    if edge.to == (n, pi) {
                        let (src, src_port) = edge.from;
                        if let Some(src_slots) = output_scratch.get(src) {
                            if let Some(src_frame) = src_slots.get(src_port) {
                                slot.channels = src_frame.channels;
                                slot.sample_rate = src_frame.sample_rate;
                                let copy_len = src_frame.samples.len().min(slot.samples.len());
                                // Bounded, alloc-free copy over pre-sized slices.
                                slot.samples[..copy_len]
                                    .copy_from_slice(&src_frame.samples[..copy_len]);
                            }
                        }
                        sourced = true;
                        break;
                    }
                }
                if !sourced {
                    // No upstream edge: feed silence.
                    for s in &mut slot.samples {
                        *s = 0.0;
                    }
                }
            }

            // (b) Invoke the node over the filled input slots and its output scratch.
            let Some(out_slots) = output_scratch.get_mut(n) else {
                continue;
            };
            let Some(node) = nodes.get_mut(n) else {
                continue;
            };
            node.process(ctx, in_slots.as_slice(), out_slots.as_mut_slice());
        }

        Ok(())
    }

    /// Seeds a node's output port from an external frame, before a cycle.
    ///
    /// Performs a bounded copy into the pre-sized scratch slot — no
    /// reallocation. Used to inject audio into a source node's output before
    /// calling [`Graph::process_cycle`]. Out-of-range node/port is a silent
    /// no-op (never panics).
    pub fn feed(&mut self, node: NodeId, port: PortIdx, src: &AudioFrame) {
        if let Some(slots) = self.scratch.get_mut().output_scratch.get_mut(node) {
            if let Some(dst) = slots.get_mut(port) {
                dst.channels = src.channels;
                dst.sample_rate = src.sample_rate;
                let copy_len = src.samples.len().min(dst.samples.len());
                dst.samples[..copy_len].copy_from_slice(&src.samples[..copy_len]);
            }
        }
    }

    /// Borrows a node's output frame after a cycle (tapping a node's output).
    ///
    /// Returns `None` if the node or port is out of range — never panics.
    #[must_use]
    pub fn read_output(&self, node: NodeId, port: PortIdx) -> Option<&AudioFrame> {
        // SAFETY: read-only shared borrow; sound under the single-RT-thread
        // invariant (never overlaps a mutable scratch access on this Graph).
        let scratch = unsafe { &*self.scratch.get() };
        scratch.output_scratch.get(node).and_then(|s| s.get(port))
    }

    /// Borrows the input frame that reached a node after a cycle.
    ///
    /// Sinks have zero outputs, so callers read a sink's consumed audio through
    /// its input slot. Returns `None` if the node or port is out of range.
    #[must_use]
    pub fn read_input(&self, node: NodeId, port: PortIdx) -> Option<&AudioFrame> {
        // SAFETY: read-only shared borrow; sound under the single-RT-thread
        // invariant (never overlaps a mutable scratch access on this Graph).
        let scratch = unsafe { &*self.scratch.get() };
        scratch.input_scratch.get(node).and_then(|s| s.get(port))
    }

    /// Returns the number of nodes in the graph.
    #[must_use]
    pub fn node_count(&self) -> usize {
        // SAFETY: read-only shared borrow; sound under the single-RT-thread
        // invariant (never overlaps a mutable scratch access on this Graph).
        unsafe { &*self.scratch.get() }.nodes.len()
    }

    /// Returns the number of links in the graph.
    #[must_use]
    pub fn link_count(&self) -> usize {
        self.edges.len()
    }

    /// Returns `true` if the graph has been compiled.
    #[must_use]
    pub fn is_compiled(&self) -> bool {
        self.compiled
    }

    /// Returns the compile-time [`GraphConfig`].
    ///
    /// Before [`Graph::compile`] this returns the default (zeroed) config.
    #[must_use]
    pub fn config(&self) -> GraphConfig {
        self.config
    }
}

impl Default for Graph {
    fn default() -> Self {
        Self::new()
    }
}

// SAFETY: `Graph` contains an `UnsafeCell<GraphScratch>` (interior mutability for
// the `&self` RT entry point), which makes it `!Sync` by default. However, a
// `Graph` is sound to SHARE across threads (e.g. inside an `ArcSwap<Graph>` for
// hot-reload) under the single-RT-thread invariant documented on
// `GraphScratch`: only the dedicated RT thread ever mutates a given graph
// instance's scratch (via `process_cycle`), and the control thread only ever
// builds/compiles a NEW instance before publishing it. No two threads ever
// access the SAME `Graph` instance's scratch concurrently. The `arc-swap`
// handle provides the atomic pointer swap; this impl lets the `Arc<Graph>` be
// `Send + Sync` so it can cross the control→RT thread boundary.
unsafe impl Sync for Graph {}

// =====================================================================================
// `topology` feature: serializable snapshot model + non-RT mutation/observer API.
//
// Everything in this section is `#[cfg(feature = "topology")]`. The default
// (0.1.0-compatible) build compiles none of it and stays serde-free.
//
// RT-safety (G3): `process_cycle` now takes `&self` (Phase-C prep), so the
// topology snapshot read and `process_cycle` are *both* `&self` and the borrow
// checker no longer forbids them from coexisting. RT/non-RT separation is
// therefore a **runtime** contract — the single-RT-thread invariant on
// `GraphScratch` — rather than a compile-time one. In the arc-swap hot-reload
// model the control thread only ever builds/reads a *new* `Graph` while the RT
// thread processes the *old* one, so they never touch the same instance.
// =====================================================================================
#[cfg(feature = "topology")]
impl SnapshotSource for Graph {
    fn topology_snapshot(&self) -> TopologySnapshot {
        // SAFETY: read-only shared borrow; sound under the single-RT-thread
        // invariant (never overlaps a mutable scratch access on this Graph).
        let scratch = unsafe { &*self.scratch.get() };
        let nodes = scratch
            .nodes
            .iter()
            .enumerate()
            .map(|(id, n)| NodeSnapshot {
                id,
                inputs: n
                    .inputs()
                    .iter()
                    .map(|d| PortMeta::from_descriptor(*d))
                    .collect(),
                outputs: n
                    .outputs()
                    .iter()
                    .map(|d| PortMeta::from_descriptor(*d))
                    .collect(),
            })
            .collect();
        let edges = self
            .edges
            .iter()
            .map(|e| SnapshotEdge {
                from: e.from,
                to: e.to,
            })
            .collect();
        TopologySnapshot { nodes, edges }
    }
}

#[cfg(feature = "topology")]
impl Graph {
    /// Builds a NEW [`Graph`] from a [`TopologySnapshot`] plus a node factory.
    ///
    /// This is a **non-RT, control-thread** operation. For each
    /// [`NodeSnapshot`] the `factory` is asked to supply a concrete
    /// `Box<dyn AudioNode>` for that node's id; the snapshot's port metadata is
    /// informational (the real node's ports, as reported by its
    /// `inputs()` / `outputs()`, are what `link` validates against). Edges are
    /// re-linked by remapping each snapshot [`NodeId`] to its new id in the
    /// rebuilt graph.
    ///
    /// The returned graph is **not compiled** — the caller compiles it with
    /// their own [`GraphConfig`].
    ///
    /// # Errors
    ///
    /// Returns [`GraphError::NodeNotFound`] if the factory returns `None` for a
    /// node the snapshot requires, or if an edge references a node id the
    /// factory did not supply. Returns the underlying [`GraphError`] from
    /// [`Graph::link`] if a rebuilt edge fails port validation.
    pub fn from_snapshot(
        snapshot: &TopologySnapshot,
        factory: &mut dyn FnMut(NodeId) -> Option<Box<dyn AudioNode>>,
    ) -> Result<Graph, GraphError> {
        let mut g = Graph::new();
        // Snapshot ids may be non-contiguous (e.g. after RemoveNode), so remap
        // each snapshot id to the new graph's contiguous id space.
        let mut id_map = std::collections::HashMap::<NodeId, NodeId>::new();
        for ns in &snapshot.nodes {
            let node = factory(ns.id).ok_or(GraphError::NodeNotFound(ns.id))?;
            let new_id = g.add_node(node);
            id_map.insert(ns.id, new_id);
        }
        for se in &snapshot.edges {
            let new_from = id_map
                .get(&se.from.0)
                .copied()
                .ok_or(GraphError::NodeNotFound(se.from.0))?;
            let new_to = id_map
                .get(&se.to.0)
                .copied()
                .ok_or(GraphError::NodeNotFound(se.to.0))?;
            g.link((new_from, se.from.1), (new_to, se.to.1))?;
        }
        Ok(g)
    }

    /// Subscribes to topology events via a `std::sync::mpsc` channel (non-RT).
    ///
    /// Returns a [`Receiver`](std::sync::mpsc::Receiver) that receives a
    /// [`TopologyEvent`] whenever the graph mutates on the control thread (a
    /// node is added/removed or a link is added/removed). The RT
    /// [`Graph::process_cycle`](crate::Graph::process_cycle) path **never**
    /// emits events.
    ///
    /// Senders whose receivers have been dropped are pruned automatically on
    /// the next emission.
    ///
    /// # Real-time safety (G3)
    ///
    /// Subscription is a control-thread mutation of the subscriber set, so it
    /// takes `&mut self` — an exclusive borrow. `process_cycle` is now `&self`
    /// (Phase-C prep), so this `&mut self` is the stronger constraint: it is
    /// statically impossible to hold the `&mut self` needed to subscribe *while*
    /// a `&self` `process_cycle` borrow is live on the same binding. Combined
    /// with the single-RT-thread invariant on `GraphScratch`, the RT path is
    /// never disrupted by a concurrent subscription on the same `Graph`.
    #[must_use]
    pub fn subscribe_topology(&mut self) -> std::sync::mpsc::Receiver<TopologyEvent> {
        let (tx, rx) = std::sync::mpsc::channel();
        self.subscribers.push(tx);
        rx
    }

    /// Emits a topology event to every live subscriber, pruning dead senders.
    ///
    /// Emits a topology event to every live subscriber, pruning dead senders.
    ///
    /// Non-RT: called only from the control-thread mutation path
    /// (`add_node` / `link`).
    fn emit_event(&mut self, event: &TopologyEvent) {
        self.subscribers.retain(|tx| tx.send(event.clone()).is_ok());
    }

    /// Builds and emits a `NodeAdded` event for the node at `id`.
    fn emit_node_added(&mut self, id: NodeId) {
        let snapshot = {
            let n = self
                .scratch
                .get_mut()
                .nodes
                .get(id)
                .expect("emit_node_added called with a just-assigned NodeId");
            NodeSnapshot {
                id,
                inputs: n
                    .inputs()
                    .iter()
                    .map(|d| PortMeta::from_descriptor(*d))
                    .collect(),
                outputs: n
                    .outputs()
                    .iter()
                    .map(|d| PortMeta::from_descriptor(*d))
                    .collect(),
            }
        };
        self.emit_event(&TopologyEvent::NodeAdded(snapshot));
    }
}

// =====================================================================================
// `distributed` feature: partition-hint derivation.
//
// The crate provides abstractions + hints only (no sockets/Raft/netmap). This
// generator runs on the control thread and never touches the RT path.
// =====================================================================================
#[cfg(feature = "distributed")]
impl Graph {
    /// Derives partition hints from a set of `(node, port, remote)` boundary
    /// declarations.
    ///
    /// Every node present in this graph is treated as a local node (the graph
    /// *is* the local partition). Each declared port becomes a
    /// [`BoundaryPort`](crate::BoundaryPort) with
    /// [`PortKind::Network`](crate::PortKind::Network). The port's direction is
    /// resolved from the node's own port descriptors: **output ports are
    /// checked first**, so a port index valid on both sides is treated as an
    /// output. A declaration whose port is out of range on both sides is
    /// skipped (the method never panics).
    ///
    /// This is a **hint generator** — sonicbrew's session-store (M07) decides
    /// the actual partitioning. Exactly one [`PartitionHint`] is returned,
    /// describing this single local partition.
    #[must_use]
    pub fn partition_hints(
        &self,
        boundaries: &[(NodeId, PortIdx, RemoteNode)],
    ) -> Vec<PartitionHint> {
        // Every node in this graph runs locally.
        let local_nodes: Vec<NodeId> = (0..self.node_count()).collect();
        let boundary_ports: Vec<BoundaryPort> = boundaries
            .iter()
            .filter_map(|(node, port, remote)| {
                let direction = self.boundary_port_direction(*node, *port)?;
                Some(BoundaryPort {
                    node: *node,
                    port: *port,
                    kind: PortKind::Network {
                        remote: remote.clone(),
                    },
                    direction,
                })
            })
            .collect();
        vec![PartitionHint {
            local_nodes,
            boundary_ports,
        }]
    }

    /// Resolves the [`PortDir`](crate::PortDir) of `(node, port)` by checking
    /// the node's output ports first, then inputs. Returns `None` if the port
    /// index is out of range on both sides.
    fn boundary_port_direction(&self, node: NodeId, port: PortIdx) -> Option<PortDir> {
        // SAFETY: read-only shared borrow; sound under the single-RT-thread
        // invariant (never overlaps a mutable scratch access on this Graph).
        let n = unsafe { &*self.scratch.get() }.nodes.get(node)?;
        if port < n.outputs().len() {
            Some(PortDir::Output)
        } else if port < n.inputs().len() {
            Some(PortDir::Input)
        } else {
            None
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use audio_core_bsd::{
        AudioFrame, AudioNode, PortDescriptor, PortDirection, ProcessContext, SampleFormat,
    };

    /// A minimal mono gain node for tests: 1 in, 1 out, scales by `gain`.
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
            let Some(inp) = in_frames.first() else {
                return;
            };
            let Some(out) = out_frames.get_mut(0) else {
                return;
            };
            let n = inp.samples.len().min(out.samples.len());
            for i in 0..n {
                out.samples[i] = inp.samples[i] * self.gain;
            }
        }
    }

    /// A test source node: zero inputs, one output, and a no-op `process`.
    ///
    /// Because `process` never touches the output scratch, whatever
    /// [`Graph::feed`] wrote into the source's output port survives the cycle
    /// and flows downstream — exactly the behaviour a real source/gateway has.
    struct SourceNode {
        out_port: [PortDescriptor; 1],
    }

    impl SourceNode {
        fn new(channels: u16) -> Self {
            Self {
                out_port: [PortDescriptor::new(
                    PortDirection::Output,
                    channels,
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
            // Intentionally a no-op: the output scratch is seeded via Graph::feed.
        }
    }

    /// Approximate float equality for assertions (uses `<`, never `==`).
    fn approx_eq(a: f32, b: f32) -> bool {
        (a - b).abs() < 1e-6
    }

    #[test]
    fn empty_graph_compiles_and_runs() {
        let mut g = Graph::new();
        assert_eq!(g.node_count(), 0);
        assert_eq!(g.link_count(), 0);
        g.compile(GraphConfig::new(64, 48_000, 1)).unwrap();
        let mut ctx = ProcessContext::new(64, 0, 48_000);
        g.process_cycle(&mut ctx).unwrap();
    }

    #[test]
    fn default_equals_new() {
        let a = Graph::new();
        let b = Graph::default();
        assert_eq!(a.node_count(), b.node_count());
        assert_eq!(a.link_count(), b.link_count());
    }

    #[test]
    fn add_node_returns_sequential_ids() {
        let mut g = Graph::new();
        let a = g.add_node(Box::new(GainNode::new(1.0)));
        let b = g.add_node(Box::new(GainNode::new(1.0)));
        assert_eq!(a, 0);
        assert_eq!(b, 1);
        assert_eq!(g.node_count(), 2);
    }

    #[test]
    fn link_valid_ports_succeeds() {
        let mut g = Graph::new();
        let s = g.add_node(Box::new(GainNode::new(1.0)));
        let d = g.add_node(Box::new(GainNode::new(1.0)));
        let link = g.link((s, 0), (d, 0)).unwrap();
        assert_eq!(link, 0);
        assert_eq!(g.link_count(), 1);
    }

    #[test]
    fn link_unknown_node_errors() {
        let mut g = Graph::new();
        let s = g.add_node(Box::new(GainNode::new(1.0)));
        assert_eq!(g.link((s, 0), (99, 0)), Err(GraphError::NodeNotFound(99)));
        assert_eq!(g.link((99, 0), (s, 0)), Err(GraphError::NodeNotFound(99)));
    }

    #[test]
    fn link_bad_port_errors() {
        let mut g = Graph::new();
        let s = g.add_node(Box::new(GainNode::new(1.0)));
        let d = g.add_node(Box::new(GainNode::new(1.0)));
        assert_eq!(
            g.link((s, 5), (d, 0)),
            Err(GraphError::PortNotFound { node: s, port: 5 })
        );
        assert_eq!(
            g.link((s, 0), (d, 9)),
            Err(GraphError::PortNotFound { node: d, port: 9 })
        );
    }

    #[test]
    fn process_before_compile_errors() {
        let g = Graph::new();
        let mut ctx = ProcessContext::new(64, 0, 48_000);
        assert_eq!(g.process_cycle(&mut ctx), Err(GraphError::NotCompiled));
    }

    #[test]
    fn compile_twice_errors() {
        let mut g = Graph::new();
        g.compile(GraphConfig::new(64, 48_000, 1)).unwrap();
        assert_eq!(
            g.compile(GraphConfig::new(64, 48_000, 1)),
            Err(GraphError::AlreadyCompiled)
        );
    }

    #[test]
    fn cycle_is_rejected_at_compile() {
        let mut g = Graph::new();
        let a = g.add_node(Box::new(GainNode::new(1.0)));
        let b = g.add_node(Box::new(GainNode::new(1.0)));
        g.link((a, 0), (b, 0)).unwrap();
        g.link((b, 0), (a, 0)).unwrap();
        let err = g.compile(GraphConfig::new(64, 48_000, 1)).unwrap_err();
        assert!(matches!(err, GraphError::CycleDetected { .. }));
    }

    #[test]
    fn end_to_end_gain_chain() {
        let mut g = Graph::new();
        let src = g.add_node(Box::new(SourceNode::new(1))); // seeded via feed
        let mid = g.add_node(Box::new(GainNode::new(0.5))); // half gain
        g.link((src, 0), (mid, 0)).unwrap();
        g.compile(GraphConfig::new(8, 48_000, 1)).unwrap();

        // Seed the source output with all-ones (survives process_cycle because
        // SourceNode::process is a no-op).
        g.feed(src, 0, &AudioFrame::from_planar(1, 48_000, vec![1.0; 8]));
        let mut ctx = ProcessContext::new(8, 0, 48_000);
        g.process_cycle(&mut ctx).unwrap();

        // mid input should mirror src output (1.0); mid output should be 0.5.
        let mid_in = g.read_input(mid, 0).unwrap();
        assert!(mid_in.samples.iter().all(|&s| approx_eq(s, 1.0)));
        let mid_out = g.read_output(mid, 0).unwrap();
        assert!(mid_out.samples.iter().all(|&s| approx_eq(s, 0.5)));
    }

    #[test]
    fn three_stage_chain_composes_gains() {
        let mut g = Graph::new();
        let src = g.add_node(Box::new(SourceNode::new(1)));
        let a = g.add_node(Box::new(GainNode::new(2.0)));
        let b = g.add_node(Box::new(GainNode::new(3.0)));
        let c = g.add_node(Box::new(GainNode::new(0.5)));
        g.link((src, 0), (a, 0)).unwrap();
        g.link((a, 0), (b, 0)).unwrap();
        g.link((b, 0), (c, 0)).unwrap();
        g.compile(GraphConfig::new(4, 48_000, 1)).unwrap();

        g.feed(src, 0, &AudioFrame::from_planar(1, 48_000, vec![1.0; 4]));
        let mut ctx = ProcessContext::new(4, 0, 48_000);
        g.process_cycle(&mut ctx).unwrap();

        // 1.0 * 2.0 * 3.0 * 0.5 = 3.0
        let out = g.read_output(c, 0).unwrap();
        assert!(out.samples.iter().all(|&s| approx_eq(s, 3.0)));
    }

    #[test]
    fn unconnected_input_is_silenced() {
        let mut g = Graph::new();
        let n = g.add_node(Box::new(GainNode::new(1.0)));
        g.compile(GraphConfig::new(4, 48_000, 1)).unwrap();
        let mut ctx = ProcessContext::new(4, 0, 48_000);
        g.process_cycle(&mut ctx).unwrap();
        let inp = g.read_input(n, 0).unwrap();
        assert!(inp.samples.iter().all(|&s| approx_eq(s, 0.0)));
    }

    #[test]
    fn read_out_of_range_returns_none() {
        let mut g = Graph::new();
        let n = g.add_node(Box::new(GainNode::new(1.0)));
        g.compile(GraphConfig::new(4, 48_000, 1)).unwrap();
        assert!(g.read_output(99, 0).is_none());
        assert!(g.read_output(n, 99).is_none());
        assert!(g.read_input(99, 0).is_none());
        assert!(g.read_input(n, 99).is_none());
    }

    #[test]
    fn feed_out_of_range_is_noop() {
        let mut g = Graph::new();
        let _n = g.add_node(Box::new(GainNode::new(1.0)));
        g.compile(GraphConfig::new(4, 48_000, 1)).unwrap();
        // Must not panic.
        g.feed(99, 0, &AudioFrame::silence(1, 4, 48_000));
        g.feed(0, 99, &AudioFrame::silence(1, 4, 48_000));
    }

    #[test]
    fn graphconfig_new_and_equality() {
        let a = GraphConfig::new(256, 48_000, 2);
        assert_eq!(a, GraphConfig::new(256, 48_000, 2));
        assert_ne!(a, GraphConfig::new(128, 48_000, 2));
        assert_eq!(a.num_frames, 256);
        assert_eq!(a.sample_rate, 48_000);
        assert_eq!(a.channels, 2);
    }

    #[test]
    fn is_compiled_and_config_reflect_state() {
        let mut g = Graph::new();
        assert!(!g.is_compiled());
        g.compile(GraphConfig::new(4, 44_100, 2)).unwrap();
        assert!(g.is_compiled());
        assert_eq!(g.config(), GraphConfig::new(4, 44_100, 2));
    }

    #[test]
    fn diamond_topology_processes_in_dependency_order() {
        // src(feed 1.0) -> {a(*2), b(*3)} ; a -> sink(*1).
        // b's output is unconnected (exercises the unconnected-output path).
        let mut g = Graph::new();
        let src = g.add_node(Box::new(SourceNode::new(1)));
        let a = g.add_node(Box::new(GainNode::new(2.0)));
        let b = g.add_node(Box::new(GainNode::new(3.0)));
        let sink = g.add_node(Box::new(GainNode::new(1.0)));
        g.link((src, 0), (a, 0)).unwrap();
        g.link((src, 0), (b, 0)).unwrap();
        g.link((a, 0), (sink, 0)).unwrap();
        let _ = b;
        g.compile(GraphConfig::new(4, 48_000, 1)).unwrap();

        g.feed(src, 0, &AudioFrame::from_planar(1, 48_000, vec![1.0; 4]));
        let mut ctx = ProcessContext::new(4, 0, 48_000);
        g.process_cycle(&mut ctx).unwrap();
        let out = g.read_output(sink, 0).unwrap();
        // sink input == a output == 1.0 * 2.0 = 2.0; sink gain 1.0 -> 2.0.
        assert!(out.samples.iter().all(|&s| approx_eq(s, 2.0)));
    }
}
