//! Distributed-prep abstractions — the `distributed` feature (ROADMAP §3.3).
//!
//! This entire module sits behind `#[cfg(feature = "distributed")]`. It
//! provides the **data models, partition hints, and the network-link node
//! contract** that sonicbrew's distributed runtime (M07 session-store, M09
//! `net-rtp-aes67`) builds on. `audio-graph-bsd` itself implements no sockets,
//! Raft consensus, or netmap plumbing — those concerns live in sonicbrew. The
//! crate's dependency surface stays minimal: `serde` (via `topology`) for the
//! serializable models and `arc-swap` (reserved for the Phase-C hot-reload
//! atomic swap, unused here).
//!
//! # Real-time safety boundary
//!
//! [`GraphPartition`] simply wraps a [`Graph`](crate::Graph) and delegates
//! [`GraphPartition::compile`] / [`GraphPartition::process_cycle`] to it, so
//! the same RT-safety contract (alloc-free `process_cycle`) holds. A network
//! link node implementing [`NetworkLinkNode`] MUST follow the rtrb bridge
//! pattern documented on that trait and demonstrated by
//! [`RingSource`](crate::RingSource) / [`RingSink`](crate::RingSink): the
//! worker thread performs `RTP`/`AES67` recv/send and ring `push` (allocation
//! allowed, non-RT); the RT `process` performs only wait-free ring `pop` and
//! bounded copies.

#![cfg(feature = "distributed")]

use crate::error::GraphError;
use crate::graph::{Graph, GraphConfig, NodeId, PortIdx};
use crate::topology_pub::PortDir;
use audio_core_bsd::{AudioNode, ProcessContext};

/// Hint at which sonicbrew transport (M09) will carry a network link.
///
/// `audio-graph-bsd` never interprets this value; sonicbrew's
/// `net-rtp-aes67` crate selects the concrete socket/pacing implementation.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, serde::Serialize, serde::Deserialize)]
pub enum TransportHint {
    /// Plain `RTP`/`UDP` transport.
    Rtp,
    /// `AES67`-compliant `RTP` (`PTP`-aligned, professional media).
    Aes67,
    /// A caller-defined transport chosen by sonicbrew.
    Custom,
}

/// Serializable model of a node living on a remote host.
///
/// This describes **where** a node runs and **how** to reach it, not the node's
/// DSP. The actual transport implementation is sonicbrew's responsibility (M09).
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct RemoteNode {
    /// The remote node's identifier (as assigned by the originating graph).
    pub node_id: NodeId,
    /// Hostname or IP address of the remote host.
    pub host: String,
    /// Network port (e.g. `RTP` port) on the remote host.
    pub port: u16,
    /// Suggested transport for reaching this remote.
    pub transport_hint: TransportHint,
}

/// Marks a port as a local scratch port or a network link to a [`RemoteNode`].
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub enum PortKind {
    /// A local scratch port whose data never crosses the network.
    Local,
    /// A network link: this port's audio is exchanged with `remote`.
    Network {
        /// The remote node on the far side of the link.
        remote: RemoteNode,
    },
}

/// A port that sits on the local/remote boundary.
///
/// Combines the local `(node, port)` coordinates with the kind of port (local
/// scratch vs. network link) and its [`PortDir`]. Boundary ports are the
/// attachment points where audio crosses from a local partition onto the
/// network (or arrives from it).
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct BoundaryPort {
    /// The local node hosting the boundary port.
    pub node: NodeId,
    /// The port index on `node`.
    pub port: PortIdx,
    /// Whether this port is local scratch or a network link.
    pub kind: PortKind,
    /// Direction of the port (input or output), mirroring the node's own
    /// [`PortDescriptor`](audio_core_bsd::PortDescriptor).
    pub direction: PortDir,
}

/// Describes one local partition: which nodes run locally and which ports cross
/// the network boundary.
///
/// This is a **hint**, not a mandate. sonicbrew's session-store (M07) decides
/// the actual partitioning; `audio-graph-bsd` only derives candidate hints via
/// [`Graph::partition_hints`](crate::Graph::partition_hints).
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct PartitionHint {
    /// Node ids that run in this local partition.
    pub local_nodes: Vec<NodeId>,
    /// Ports in this partition that cross the network boundary.
    pub boundary_ports: Vec<BoundaryPort>,
}

/// A single local partition wrapping a [`Graph`] plus its boundary ports.
///
/// `GraphPartition` is a thin convenience wrapper: it owns the local sub-graph
/// and the boundary declaration, and delegates [`GraphPartition::compile`] and
/// [`GraphPartition::process_cycle`] straight to the inner [`Graph`]. The
/// RT-safety contract of [`Graph::process_cycle`] is therefore unchanged.
pub struct GraphPartition {
    /// The local sub-graph.
    graph: Graph,
    /// Boundary ports for this partition.
    boundary: Vec<BoundaryPort>,
}

impl GraphPartition {
    /// Creates a new partition wrapping `graph` with the given `boundary`
    /// ports.
    #[must_use]
    pub fn new(graph: Graph, boundary: Vec<BoundaryPort>) -> Self {
        Self { graph, boundary }
    }

    /// Compiles the inner graph (delegates to [`Graph::compile`]).
    ///
    /// # Errors
    ///
    /// Returns [`GraphError::AlreadyCompiled`] if the graph was already
    /// compiled, or [`GraphError::CycleDetected`] if the topology contains a
    /// cycle.
    pub fn compile(&mut self, config: GraphConfig) -> Result<(), GraphError> {
        self.graph.compile(config)
    }

    /// Processes one audio cycle on the inner graph (delegates to
    /// [`Graph::process_cycle`]).
    ///
    /// # Errors
    ///
    /// Returns [`GraphError::NotCompiled`] if the graph has not been compiled.
    pub fn process_cycle(&mut self, ctx: &mut ProcessContext) -> Result<(), GraphError> {
        self.graph.process_cycle(ctx)
    }

    /// Returns the boundary ports of this partition.
    #[must_use]
    pub fn boundary(&self) -> &[BoundaryPort] {
        &self.boundary
    }

    /// Returns a shared reference to the inner graph.
    #[must_use]
    pub fn graph(&self) -> &Graph {
        &self.graph
    }

    /// Returns a mutable reference to the inner graph.
    pub fn graph_mut(&mut self) -> &mut Graph {
        &mut self.graph
    }
}

/// Contract for a network link node — a node that exchanges audio with a
/// [`RemoteNode`] across the network.
///
/// A network link node **MUST** follow the rtrb bridge pattern demonstrated by
/// [`RingSource`](crate::RingSource) and [`RingSink`](crate::RingSink):
///
/// - The **worker thread** performs the `RTP`/`AES67` recv/send and pushes
///   received frames into an [`rtrb`] ring producer. Allocation and blocking
///   I/O are allowed here — this path is **not** real-time.
/// - The RT `process` implementation performs only a **wait-free** ring
///   consumer `pop` (or a bounded copy into a pre-sized stash) — no allocation,
///   lock, panic, or syscall.
///
/// sonicbrew's `net-rtp-aes67` crate (M09) is the canonical implementor.
pub trait NetworkLinkNode: AudioNode {
    /// Returns the remote node this link communicates with.
    #[must_use]
    fn remote(&self) -> &RemoteNode;
}

#[cfg(test)]
mod tests {
    use super::*;
    use audio_core_bsd::{
        AudioFrame, AudioNode, PortDescriptor, PortDirection, ProcessContext, SampleFormat,
    };

    /// A minimal test node with one mono input and one mono output (f32). Used
    /// to build a tiny graph for partition / delegation tests.
    struct PassthroughNode {
        gain: f32,
        in_port: [PortDescriptor; 1],
        out_port: [PortDescriptor; 1],
    }

    impl PassthroughNode {
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

    impl AudioNode for PassthroughNode {
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

    #[test]
    fn remote_node_serde_roundtrip() {
        let node = RemoteNode {
            node_id: 7,
            host: "192.168.1.42".to_string(),
            port: 5004,
            transport_hint: TransportHint::Rtp,
        };
        let bytes = bincode::serialize(&node).expect("serialize");
        let back: RemoteNode = bincode::deserialize(&bytes).expect("deserialize");
        assert_eq!(node, back);
    }

    #[test]
    fn transport_hint_serde_roundtrip() {
        for hint in [
            TransportHint::Rtp,
            TransportHint::Aes67,
            TransportHint::Custom,
        ] {
            let bytes = bincode::serialize(&hint).expect("serialize");
            let back: TransportHint = bincode::deserialize(&bytes).expect("deserialize");
            assert_eq!(hint, back);
        }
    }

    #[test]
    fn port_kind_local_vs_network() {
        let local = PortKind::Local;
        let remote = RemoteNode {
            node_id: 1,
            host: "host".to_string(),
            port: 10,
            transport_hint: TransportHint::Aes67,
        };
        let net = PortKind::Network {
            remote: remote.clone(),
        };
        assert_eq!(local, PortKind::Local);
        let PortKind::Network { remote: r } = &net else {
            panic!("expected Network");
        };
        assert_eq!(r, &remote);
    }

    #[test]
    fn partition_hint_construction() {
        let bp = BoundaryPort {
            node: 0,
            port: 0,
            kind: PortKind::Network {
                remote: RemoteNode {
                    node_id: 9,
                    host: "h".to_string(),
                    port: 1,
                    transport_hint: TransportHint::Custom,
                },
            },
            direction: PortDir::Output,
        };
        let hint = PartitionHint {
            local_nodes: vec![0, 1],
            boundary_ports: vec![bp.clone()],
        };
        assert_eq!(hint.local_nodes, vec![0, 1]);
        assert_eq!(hint.boundary_ports, vec![bp]);
    }

    #[test]
    fn graph_partition_delegates_compile_and_process() {
        let mut g = Graph::new();
        let a = g.add_node(Box::new(PassthroughNode::new(1.0)));
        let b = g.add_node(Box::new(PassthroughNode::new(1.0)));
        g.link((a, 0), (b, 0)).unwrap();

        let boundary = vec![BoundaryPort {
            node: b,
            port: 0,
            kind: PortKind::Network {
                remote: RemoteNode {
                    node_id: 100,
                    host: "remote".to_string(),
                    port: 5004,
                    transport_hint: TransportHint::Rtp,
                },
            },
            direction: PortDir::Output,
        }];
        let mut part = GraphPartition::new(g, boundary);
        // Delegate: compile + process_cycle must succeed and not panic.
        part.compile(GraphConfig::new(8, 48_000, 1)).unwrap();
        let mut ctx = ProcessContext::new(8, 0, 48_000);
        part.process_cycle(&mut ctx).unwrap();
        // Boundary accessor.
        assert_eq!(part.boundary().len(), 1);
        assert_eq!(part.boundary()[0].node, b);
        // Inner graph accessors.
        assert_eq!(part.graph().node_count(), 2);
        assert_eq!(part.graph().link_count(), 1);
    }

    #[test]
    fn partition_hints_returns_one_hint_with_boundary() {
        let mut g = Graph::new();
        let a = g.add_node(Box::new(PassthroughNode::new(1.0)));
        // Declare a boundary on node a's port 0 (an output port).
        let remote = RemoteNode {
            node_id: 5,
            host: "far".to_string(),
            port: 6004,
            transport_hint: TransportHint::Aes67,
        };
        let hints = g.partition_hints(&[(a, 0, remote.clone())]);
        assert_eq!(hints.len(), 1);
        let hint = &hints[0];
        // Local set = all nodes in the graph.
        assert_eq!(hint.local_nodes, vec![a]);
        // One boundary port, Network, Output (port 0 is an output).
        assert_eq!(hint.boundary_ports.len(), 1);
        let bp = &hint.boundary_ports[0];
        assert_eq!(bp.node, a);
        assert_eq!(bp.port, 0);
        assert_eq!(bp.direction, PortDir::Output);
        let PortKind::Network { remote: r } = &bp.kind else {
            panic!("expected Network");
        };
        assert_eq!(r, &remote);
    }

    #[test]
    fn partition_hints_skips_out_of_range_port() {
        let mut g = Graph::new();
        let a = g.add_node(Box::new(PassthroughNode::new(1.0)));
        // Port 99 does not exist on either side -> declaration is skipped, but
        // the local set is still the full graph.
        let remote = RemoteNode {
            node_id: 5,
            host: "far".to_string(),
            port: 6004,
            transport_hint: TransportHint::Rtp,
        };
        let hints = g.partition_hints(&[(a, 99, remote)]);
        assert_eq!(hints.len(), 1);
        assert_eq!(hints[0].local_nodes, vec![a]);
        assert!(hints[0].boundary_ports.is_empty());
    }
}
