//! Serializable topology model — the `topology` feature (ROADMAP §3.2).
//!
//! This entire module is behind `#[cfg(feature = "topology")]`. When the
//! feature is disabled every type here vanishes and the crate stays
//! serde-free, preserving the 0.1.0 minimal-dependency contract.
//!
//! # Why a serialization *layer*?
//!
//! `audio-graph-bsd` owns nodes as `Box<dyn AudioNode>` trait objects, which
//! cannot be serialized. Instead of serializing node implementations, this
//! module captures their **port metadata** (direction / channel count / sample
//! format) in serializable mirror types. A [`TopologySnapshot`] is therefore a
//! structural description of the graph — enough for the sonicbrew session-store
//! (M07) to replicate topology over Raft and for a control thread to rebuild a
//! runnable [`Graph`](crate::Graph) by pairing each [`NodeSnapshot`] with a
//! concrete node factory (see [`Graph::from_snapshot`](crate::Graph::from_snapshot)).
//!
//! # Real-time safety (G3)
//!
//! Nothing in this module touches the RT path. [`SnapshotSource`] takes
//! `&self`, and all mutation APIs are associated functions or run on a
//! control/worker thread. The borrow checker guarantees that a
//! [`TopologySnapshot`] edit cannot run concurrently with
//! [`Graph::process_cycle`](crate::Graph::process_cycle) on the same `Graph`,
//! because `process_cycle` requires `&mut self`.

#![cfg(feature = "topology")]

use crate::graph::{LinkId, NodeId, PortIdx};

// =====================================================================================
// Mirror types for audio-core-bsd port metadata.
//
// audio-core-bsd's PortDescriptor / PortDirection / SampleFormat carry NO serde
// derives (they are a separate, minimal-dependency crate). We re-declare
// serde-enabled mirrors here so a TopologySnapshot can be (de)serialized without
// forcing serde onto audio-core-bsd. PortMeta provides lossless conversions in
// both directions.
// =====================================================================================

/// Serializable mirror of [`audio_core_bsd::PortDirection`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, serde::Serialize, serde::Deserialize)]
pub enum PortDir {
    /// The port consumes audio from an upstream node.
    Input,
    /// The port produces audio for a downstream node.
    Output,
}

/// Serializable mirror of [`audio_core_bsd::SampleFormat`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, serde::Serialize, serde::Deserialize)]
pub enum SampleFmt {
    /// 32-bit float (the default sample format).
    F32,
    /// 64-bit float.
    F64,
    /// 16-bit signed integer.
    I16,
    /// 32-bit signed integer.
    I32,
}

/// Serializable mirror of [`audio_core_bsd::PortDescriptor`].
///
/// Stores the three fields needed to describe a port's contract: its
/// direction, channel count, and sample format. Convert to/from a
/// `PortDescriptor` with [`PortMeta::from_descriptor`] /
/// [`PortMeta::to_descriptor`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct PortMeta {
    /// Whether this port is an input or output.
    pub direction: PortDir,
    /// Number of channels carried by the port.
    pub channels: u16,
    /// Numeric format of each sample.
    pub sample_format: SampleFmt,
}

impl PortMeta {
    /// Constructs a `PortMeta` from an audio-core-bsd
    /// [`PortDescriptor`](audio_core_bsd::PortDescriptor).
    ///
    /// This is a lossless field-by-field mapping; the reverse conversion is
    /// [`PortMeta::to_descriptor`].
    #[must_use]
    pub fn from_descriptor(d: audio_core_bsd::PortDescriptor) -> Self {
        Self {
            direction: match d.direction {
                audio_core_bsd::PortDirection::Input => PortDir::Input,
                audio_core_bsd::PortDirection::Output => PortDir::Output,
            },
            channels: d.channels,
            sample_format: match d.sample_format {
                audio_core_bsd::SampleFormat::F32 => SampleFmt::F32,
                audio_core_bsd::SampleFormat::F64 => SampleFmt::F64,
                audio_core_bsd::SampleFormat::I16 => SampleFmt::I16,
                audio_core_bsd::SampleFormat::I32 => SampleFmt::I32,
            },
        }
    }

    /// Converts this `PortMeta` back into an audio-core-bsd
    /// [`PortDescriptor`](audio_core_bsd::PortDescriptor).
    #[must_use]
    pub fn to_descriptor(self) -> audio_core_bsd::PortDescriptor {
        audio_core_bsd::PortDescriptor::new(
            match self.direction {
                PortDir::Input => audio_core_bsd::PortDirection::Input,
                PortDir::Output => audio_core_bsd::PortDirection::Output,
            },
            self.channels,
            match self.sample_format {
                SampleFmt::F32 => audio_core_bsd::SampleFormat::F32,
                SampleFmt::F64 => audio_core_bsd::SampleFormat::F64,
                SampleFmt::I16 => audio_core_bsd::SampleFormat::I16,
                SampleFmt::I32 => audio_core_bsd::SampleFormat::I32,
            },
        )
    }
}

// =====================================================================================
// Snapshot types.
// =====================================================================================

/// A directed edge captured in a [`TopologySnapshot`].
///
/// Mirrors the internal graph edge: an output port feeding an input port. The
/// `(NodeId, PortIdx)` pairs are kept as plain tuples so the snapshot
/// serializes compactly and stays format-agnostic.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct SnapshotEdge {
    /// Source `(node, output-port)`.
    pub from: (NodeId, PortIdx),
    /// Destination `(node, input-port)`.
    pub to: (NodeId, PortIdx),
}

/// Structural snapshot of a single node: its id plus the port metadata of
/// every input and output port.
///
/// The node's *implementation* (`Box<dyn AudioNode>`) is deliberately not
/// captured — it cannot be serialized. A rebuild pairs this metadata with a
/// concrete factory (see [`Graph::from_snapshot`](crate::Graph::from_snapshot)).
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct NodeSnapshot {
    /// Stable id of the node within the graph.
    pub id: NodeId,
    /// Metadata for every input port, in the order the node reports them.
    pub inputs: Vec<PortMeta>,
    /// Metadata for every output port, in the order the node reports them.
    pub outputs: Vec<PortMeta>,
}

/// A serializable, metadata-only snapshot of an entire graph topology.
///
/// Contains the node list (with per-port metadata) and the edge list. This is
/// the unit the sonicbrew session-store (M07) replicates over Raft and the unit
/// a control thread rebuilds a [`Graph`](crate::Graph) from.
///
/// Because nodes are `Box<dyn AudioNode>` trait objects, the snapshot stores
/// **port metadata, not node implementations**. Rebuilding a runnable graph
/// requires a factory that supplies a concrete `AudioNode` per
/// [`NodeId`](crate::NodeId).
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize, Default)]
pub struct TopologySnapshot {
    /// All nodes, indexed by their stable [`NodeId`].
    pub nodes: Vec<NodeSnapshot>,
    /// All directed edges (output-port -> input-port).
    pub edges: Vec<SnapshotEdge>,
}

impl TopologySnapshot {
    /// Creates an empty snapshot.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Finds a node snapshot by id.
    #[must_use]
    pub fn node(&self, id: NodeId) -> Option<&NodeSnapshot> {
        self.nodes.iter().find(|n| n.id == id)
    }

    /// Applies an incremental [`Mutation`] to this snapshot in place.
    ///
    /// This is a pure data edit over the snapshot's `nodes` / `edges` vectors —
    /// it never touches a live [`Graph`](crate::Graph) and runs on a
    /// control/worker thread (Non-RT). The rules are:
    ///
    /// - [`Mutation::AddNode`]: inserts the node, or replaces an existing node
    ///   with the same id.
    /// - [`Mutation::RemoveNode`]: drops the node **and** every edge that
    ///   touches it (dangling edges are meaningless once the node is gone).
    /// - [`Mutation::AddLink`]: appends the edge.
    /// - [`Mutation::RemoveLink`]: removes the edge at the given positional
    ///   [`LinkId`] (edges are indexed in insertion order; removal shifts the
    ///   indices of subsequent edges, exactly like the live `Graph`).
    pub fn apply(&mut self, mutation: &Mutation) {
        match mutation {
            Mutation::AddNode(ns) => {
                if let Some(existing) = self.nodes.iter_mut().find(|n| n.id == ns.id) {
                    *existing = ns.clone();
                } else {
                    self.nodes.push(ns.clone());
                }
            }
            Mutation::RemoveNode(id) => {
                self.nodes.retain(|n| n.id != *id);
                // Drop every edge touching the removed node so the snapshot
                // never carries dangling references.
                self.edges.retain(|e| e.from.0 != *id && e.to.0 != *id);
            }
            Mutation::AddLink(se) => {
                self.edges.push(*se);
            }
            Mutation::RemoveLink(link_id) => {
                // LinkId is positional into `edges`; removal shifts later ids.
                if *link_id < self.edges.len() {
                    self.edges.remove(*link_id);
                }
            }
        }
    }
}

// =====================================================================================
// Events + Mutations.
// =====================================================================================

/// An observable change to a graph's topology.
///
/// Emitted on the non-RT mutation path (e.g. from `add_node` / `link`) and
/// delivered to subscribers registered via
/// [`Graph::subscribe_topology`](crate::Graph::subscribe_topology). The RT
/// [`Graph::process_cycle`](crate::Graph::process_cycle) path **never** emits
/// events.
#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
pub enum TopologyEvent {
    /// A node was added; carries its structural snapshot.
    NodeAdded(NodeSnapshot),
    /// A node was removed; carries its id.
    NodeRemoved(NodeId),
    /// A link was added; carries the edge.
    LinkAdded(SnapshotEdge),
    /// A link was removed; carries its id.
    LinkRemoved(LinkId),
    /// The graph finished compiling (topological sort + scratch allocation).
    GraphCompiled,
    /// The graph was reset to an uncompiled state.
    GraphReset,
}

/// A pure-data edit applicable to a [`TopologySnapshot`] via
/// [`TopologySnapshot::apply`].
///
/// Mutations are themselves serializable, so a session store can log/replicate
/// a stream of `Mutation`s and replay them onto a baseline snapshot.
#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
pub enum Mutation {
    /// Add (or replace) the node with the given snapshot.
    AddNode(NodeSnapshot),
    /// Remove the node with the given id (and its incident edges).
    RemoveNode(NodeId),
    /// Add the given edge.
    AddLink(SnapshotEdge),
    /// Remove the edge at the given positional [`LinkId`].
    RemoveLink(LinkId),
}

// =====================================================================================
// Traits (observed topology — non-RT).
// =====================================================================================

/// A source of [`TopologySnapshot`] data.
///
/// Implemented by [`Graph`](crate::Graph) so external observers can read the
/// current topology as serializable metadata. The method takes `&self`, so it
/// is safe to call from a read-only context — but note that a `&self` borrow
/// cannot coexist with the `&mut self` that
/// [`Graph::process_cycle`](crate::Graph::process_cycle) requires, so the
/// borrow checker prevents concurrent snapshot reads and RT processing on the
/// same `Graph`.
pub trait SnapshotSource {
    /// Returns a serializable, metadata-only snapshot of the current topology.
    #[must_use]
    fn topology_snapshot(&self) -> TopologySnapshot;
}

/// An observer of [`TopologyEvent`]s.
///
/// Implement this to react to topology changes (e.g. to persist them, forward
/// them over the network, or invalidate a cache). Events arrive on the
/// non-RT mutation path only.
pub trait TopologyObserver {
    /// Called for each topology event emitted by an observed graph.
    fn on_topology_event(&mut self, event: &TopologyEvent);
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{Graph, GraphConfig};
    use audio_core_bsd::{
        AudioFrame, AudioNode, PortDescriptor, PortDirection, ProcessContext, SampleFormat,
    };

    /// Test gain node: 1 mono in, 1 mono out, scales by `gain`.
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

    /// Test source node: 0 in, 1 mono out, no-op process.
    struct SourceNode {
        out_p: [PortDescriptor; 1],
    }
    impl SourceNode {
        fn new() -> Self {
            Self {
                out_p: [PortDescriptor::output(1, SampleFormat::F32)],
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
        fn process(&mut self, _ctx: &mut ProcessContext, _i: &[AudioFrame], _o: &mut [AudioFrame]) {
        }
    }

    fn mono_out() -> PortMeta {
        PortMeta {
            direction: PortDir::Output,
            channels: 1,
            sample_format: SampleFmt::F32,
        }
    }

    fn mono_in() -> PortMeta {
        PortMeta {
            direction: PortDir::Input,
            channels: 1,
            sample_format: SampleFmt::F32,
        }
    }

    // --- PortMeta roundtrip via PortDescriptor ---

    #[test]
    fn portmeta_from_descriptor_roundtrips_all_variants() {
        for dir in [PortDirection::Input, PortDirection::Output] {
            for (channels, fmt) in [
                (1_u16, SampleFormat::F32),
                (2_u16, SampleFormat::F64),
                (6_u16, SampleFormat::I16),
                (8_u16, SampleFormat::I32),
            ] {
                let d = PortDescriptor::new(dir, channels, fmt);
                let meta = PortMeta::from_descriptor(d);
                assert_eq!(meta.to_descriptor(), d, "roundtrip failed for {d:?}");
            }
        }
    }

    #[test]
    fn snapshot_edge_is_copy_and_eq() {
        let a = SnapshotEdge {
            from: (0, 0),
            to: (1, 0),
        };
        let b = a; // Copy
        assert_eq!(a, b);
        let c = SnapshotEdge {
            from: (0, 0),
            to: (2, 0),
        };
        assert_ne!(a, c);
    }

    // --- TopologySnapshot::apply ---

    #[test]
    fn apply_add_node_inserts_then_replaces() {
        let mut snap = TopologySnapshot::new();
        let ns = NodeSnapshot {
            id: 3,
            inputs: vec![mono_in()],
            outputs: vec![mono_out()],
        };
        snap.apply(&Mutation::AddNode(ns.clone()));
        assert_eq!(snap.node(3), Some(&ns));

        // Same id with different metadata replaces in place.
        let replaced = NodeSnapshot {
            id: 3,
            inputs: vec![],
            outputs: vec![mono_out()],
        };
        snap.apply(&Mutation::AddNode(replaced.clone()));
        assert_eq!(snap.nodes.len(), 1, "replace must not duplicate");
        assert_eq!(snap.node(3), Some(&replaced));
    }

    #[test]
    fn apply_add_and_remove_link_by_positional_id() {
        let mut snap = TopologySnapshot::new();
        let e0 = SnapshotEdge {
            from: (0, 0),
            to: (1, 0),
        };
        let e1 = SnapshotEdge {
            from: (1, 0),
            to: (2, 0),
        };
        snap.apply(&Mutation::AddLink(e0));
        snap.apply(&Mutation::AddLink(e1));
        assert_eq!(snap.edges, vec![e0, e1]);

        // Remove link id 0 (positional). After removal, the former e1 shifts
        // to index 0.
        snap.apply(&Mutation::RemoveLink(0));
        assert_eq!(snap.edges, vec![e1]);

        // Out-of-range removal is a safe no-op.
        snap.apply(&Mutation::RemoveLink(99));
        assert_eq!(snap.edges, vec![e1]);
    }

    #[test]
    fn apply_remove_node_drops_incident_edges() {
        let mut snap = TopologySnapshot::new();
        snap.nodes.push(NodeSnapshot {
            id: 0,
            inputs: vec![],
            outputs: vec![mono_out()],
        });
        snap.nodes.push(NodeSnapshot {
            id: 1,
            inputs: vec![mono_in()],
            outputs: vec![mono_out()],
        });
        snap.edges.push(SnapshotEdge {
            from: (0, 0),
            to: (1, 0),
        });
        // Remove node 0: the edge (0,0)->(1,0) touches it and must vanish.
        snap.apply(&Mutation::RemoveNode(0));
        assert!(snap.node(0).is_none());
        assert!(snap.edges.is_empty());
    }

    // --- Graph::topology_snapshot() mirrors structure ---

    #[test]
    fn graph_topology_snapshot_matches_structure() {
        let mut g = Graph::new();
        let src = g.add_node(Box::new(SourceNode::new()));
        let gain = g.add_node(Box::new(GainNode::new(0.5)));
        let _link = g.link((src, 0), (gain, 0)).unwrap();

        let snap = SnapshotSource::topology_snapshot(&g);
        assert_eq!(snap.nodes.len(), 2);
        assert_eq!(snap.edges.len(), 1);

        // Source: 0 inputs, 1 output.
        let src_ns = snap.node(src).unwrap();
        assert!(src_ns.inputs.is_empty());
        assert_eq!(src_ns.outputs.len(), 1);
        assert_eq!(src_ns.outputs[0], mono_out());

        // Gain: 1 input, 1 output.
        let gain_ns = snap.node(gain).unwrap();
        assert_eq!(gain_ns.inputs.len(), 1);
        assert_eq!(gain_ns.inputs[0], mono_in());
        assert_eq!(gain_ns.outputs.len(), 1);
        assert_eq!(gain_ns.outputs[0], mono_out());

        // Edge mirrors the link.
        assert_eq!(
            snap.edges[0],
            SnapshotEdge {
                from: (src, 0),
                to: (gain, 0)
            }
        );
    }

    // --- Graph::from_snapshot rebuilds a compilable graph ---

    #[test]
    fn from_snapshot_rebuilds_compilable_graph() {
        // Build a source graph, snapshot it, rebuild via factory, compile.
        let mut orig = Graph::new();
        let src = orig.add_node(Box::new(SourceNode::new()));
        let gain = orig.add_node(Box::new(GainNode::new(0.25)));
        orig.link((src, 0), (gain, 0)).unwrap();
        let snap = SnapshotSource::topology_snapshot(&orig);

        // Factory maps NodeId -> concrete node. NodeId order is preserved, so
        // `src` is still 0 and `gain` is still 1 here; but map generically by
        // snapshot id to be robust to remapping.
        let mut rebuilt = Graph::from_snapshot(&snap, &mut |id| {
            if id == src {
                Some(Box::new(SourceNode::new()) as Box<dyn AudioNode>)
            } else if id == gain {
                Some(Box::new(GainNode::new(0.25)) as Box<dyn AudioNode>)
            } else {
                None
            }
        })
        .expect("factory supplies all nodes");

        assert_eq!(rebuilt.node_count(), 2);
        assert_eq!(rebuilt.link_count(), 1);
        rebuilt
            .compile(GraphConfig::new(8, 48_000, 1))
            .expect("rebuilt graph compiles");
    }

    #[test]
    fn from_snapshot_errors_when_factory_returns_none() {
        let snap = TopologySnapshot {
            nodes: vec![NodeSnapshot {
                id: 7,
                inputs: vec![],
                outputs: vec![mono_out()],
            }],
            edges: vec![],
        };
        // Factory refuses to supply node 7.
        let result = Graph::from_snapshot(&snap, &mut |_| None);
        assert!(result.is_err());
    }
}
