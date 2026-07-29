//! The real-time-safe audio graph engine.
//!
//! [`Graph`] owns a set of [`AudioNode`](audio_core_bsd::AudioNode) trait objects,
//! the directed edges between their ports, and the pre-allocated scratch frames
//! used to shuttle audio between nodes on the real-time thread. The compile /
//! build phases allocate freely; [`Graph::process_cycle`] does not.

use crate::error::GraphError;
use crate::topology::{topological_sort, Edge};
use audio_core_bsd::{AudioFrame, AudioNode, PortDirection, ProcessContext};

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
    /// The nodes, indexed by [`NodeId`].
    nodes: Vec<Box<dyn AudioNode>>,
    /// The directed edges (output-port -> input-port).
    edges: Vec<Edge>,
    /// Node execution order, filled by [`Graph::compile`].
    execution_order: Vec<NodeId>,
    /// Compile-time configuration.
    config: GraphConfig,
    /// Whether [`Graph::compile`] has run.
    compiled: bool,
    /// Per-node, per-input-port scratch frame. Indexed `[node][port]`.
    input_scratch: Vec<Vec<AudioFrame>>,
    /// Per-node, per-output-port scratch frame. Indexed `[node][port]`.
    output_scratch: Vec<Vec<AudioFrame>>,
}

impl Graph {
    /// Creates an empty graph.
    #[must_use]
    pub fn new() -> Self {
        Self {
            nodes: Vec::new(),
            edges: Vec::new(),
            execution_order: Vec::new(),
            config: GraphConfig::new(0, 0, 0),
            compiled: false,
            input_scratch: Vec::new(),
            output_scratch: Vec::new(),
        }
    }

    /// Adds a node to the graph and returns its stable [`NodeId`].
    ///
    /// The returned id equals the node's index in insertion order and never
    /// changes for the lifetime of the graph. Scratch frames for the node's
    /// ports are allocated later in [`Graph::compile`].
    #[must_use]
    pub fn add_node(&mut self, node: Box<dyn AudioNode>) -> NodeId {
        let id = self.nodes.len();
        self.nodes.push(node);
        // Keep the scratch index aligned with the node id; the actual per-port
        // frames are allocated in compile().
        self.input_scratch.push(Vec::new());
        self.output_scratch.push(Vec::new());
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
            let from_n = self
                .nodes
                .get(from_node)
                .ok_or(GraphError::NodeNotFound(from_node))?;
            let to_n = self
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
        let order = topological_sort(self.nodes.len(), &self.edges)
            .map_err(|remaining| GraphError::CycleDetected { nodes: remaining })?;
        self.execution_order = order;
        self.config = config;

        // Pre-allocate every per-port scratch frame so process_cycle never
        // allocates. This is the ONLY place allocation is permitted.
        self.input_scratch = Vec::with_capacity(self.nodes.len());
        self.output_scratch = Vec::with_capacity(self.nodes.len());
        for node in &self.nodes {
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
            self.input_scratch.push(in_slots);
            self.output_scratch.push(out_slots);
        }
        self.compiled = true;
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
    pub fn process_cycle(&mut self, ctx: &mut ProcessContext) -> Result<(), GraphError> {
        if !self.compiled {
            return Err(GraphError::NotCompiled);
        }

        // Split the &mut self borrow into disjoint field borrows so the borrow
        // checker allows reading output_scratch while writing input_scratch and
        // invoking nodes[i] in the same loop.
        let Graph {
            nodes,
            edges,
            execution_order,
            input_scratch,
            output_scratch,
            ..
        } = self;

        for &n in execution_order.iter() {
            // (a) Fill this node's input slots from upstream outputs, or zero them.
            let Some(in_slots) = input_scratch.get_mut(n) else {
                continue;
            };
            for (pi, slot) in in_slots.iter_mut().enumerate() {
                let mut sourced = false;
                for edge in edges.iter() {
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
        if let Some(slots) = self.output_scratch.get_mut(node) {
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
        self.output_scratch.get(node).and_then(|s| s.get(port))
    }

    /// Borrows the input frame that reached a node after a cycle.
    ///
    /// Sinks have zero outputs, so callers read a sink's consumed audio through
    /// its input slot. Returns `None` if the node or port is out of range.
    #[must_use]
    pub fn read_input(&self, node: NodeId, port: PortIdx) -> Option<&AudioFrame> {
        self.input_scratch.get(node).and_then(|s| s.get(port))
    }

    /// Returns the number of nodes in the graph.
    #[must_use]
    pub fn node_count(&self) -> usize {
        self.nodes.len()
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
        let mut g = Graph::new();
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
