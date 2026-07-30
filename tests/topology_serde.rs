//! Phase-A RT-safety / serialization gates (ROADMAP §9.1 G3 + G4).
//!
//! - **G4**: `TopologySnapshot` / `Mutation` / `TopologyEvent` survive a serde
//!   roundtrip through `bincode` with byte-exact equality. This is the contract
//!   sonicbrew session-store (M07) relies on to replicate topology over Raft.
//! - **G3**: snapshot extraction (`&self`) is safe immediately before and after
//!   a `process_cycle` (`&mut self`) — the borrow checker enforces they cannot
//!   run concurrently on one `Graph`, and the snapshot path touches no RT data.
#![cfg(feature = "topology")]

use audio_core_bsd::{AudioNode, PortDescriptor, ProcessContext, SampleFormat};
use audio_graph_bsd::{
    Graph, GraphConfig, Mutation, NodeSnapshot, PortDir, PortMeta, SampleFmt, SnapshotEdge,
    SnapshotSource, TopologyEvent, TopologySnapshot,
};

// ===== G4: serde roundtrip (bincode) =====

#[test]
fn topology_snapshot_serde_roundtrip_bincode() {
    // A representative topology: 1 source (0 in / 1 out) -> 1 sink-ish node
    // (1 in / 1 out), stereo F32.
    let snap = TopologySnapshot {
        nodes: vec![
            NodeSnapshot {
                id: 0,
                inputs: vec![],
                outputs: vec![PortMeta {
                    direction: PortDir::Output,
                    channels: 2,
                    sample_format: SampleFmt::F32,
                }],
            },
            NodeSnapshot {
                id: 1,
                inputs: vec![PortMeta {
                    direction: PortDir::Input,
                    channels: 2,
                    sample_format: SampleFmt::F32,
                }],
                outputs: vec![PortMeta {
                    direction: PortDir::Output,
                    channels: 2,
                    sample_format: SampleFmt::F32,
                }],
            },
        ],
        edges: vec![SnapshotEdge {
            from: (0, 0),
            to: (1, 0),
        }],
    };
    let bytes = bincode::serialize(&snap).expect("serialize");
    let back: TopologySnapshot = bincode::deserialize(&bytes).expect("deserialize");
    assert_eq!(
        snap, back,
        "TopologySnapshot serde roundtrip not byte-exact"
    );
}

#[test]
fn mutation_serde_roundtrip_all_variants() {
    let mutations = [
        Mutation::AddNode(NodeSnapshot {
            id: 3,
            inputs: vec![],
            outputs: vec![PortMeta {
                direction: PortDir::Output,
                channels: 6,
                sample_format: SampleFmt::I16,
            }],
        }),
        Mutation::RemoveNode(7),
        Mutation::AddLink(SnapshotEdge {
            from: (0, 0),
            to: (1, 0),
        }),
        Mutation::RemoveLink(2),
    ];
    for m in mutations {
        let bytes = bincode::serialize(&m).expect("serialize");
        let back: Mutation = bincode::deserialize(&bytes).expect("deserialize");
        assert_eq!(m, back, "Mutation serde roundtrip failed for {m:?}");
    }
}

#[test]
fn topology_event_serde_roundtrip_all_variants() {
    let events = [
        TopologyEvent::NodeAdded(NodeSnapshot {
            id: 5,
            inputs: vec![],
            outputs: vec![PortMeta {
                direction: PortDir::Output,
                channels: 1,
                sample_format: SampleFmt::F64,
            }],
        }),
        TopologyEvent::NodeRemoved(9),
        TopologyEvent::LinkAdded(SnapshotEdge {
            from: (1, 0),
            to: (2, 0),
        }),
        TopologyEvent::LinkRemoved(4),
        TopologyEvent::GraphCompiled,
        TopologyEvent::GraphReset,
    ];
    for e in events {
        let bytes = bincode::serialize(&e).expect("serialize");
        let back: TopologyEvent = bincode::deserialize(&bytes).expect("deserialize");
        assert_eq!(e, back, "TopologyEvent serde roundtrip failed for {e:?}");
    }
}

#[test]
fn snapshot_apply_then_serde_roundtrip() {
    // Apply a mutation, then confirm the edited snapshot still roundtrips.
    let mut snap = TopologySnapshot::default();
    snap.apply(&Mutation::AddNode(NodeSnapshot {
        id: 0,
        inputs: vec![],
        outputs: vec![PortMeta {
            direction: PortDir::Output,
            channels: 1,
            sample_format: SampleFmt::F32,
        }],
    }));
    snap.apply(&Mutation::AddLink(SnapshotEdge {
        from: (0, 0),
        to: (0, 0),
    }));
    let bytes = bincode::serialize(&snap).expect("serialize");
    let back: TopologySnapshot = bincode::deserialize(&bytes).expect("deserialize");
    assert_eq!(snap, back);
    assert_eq!(back.nodes.len(), 1);
    assert_eq!(back.edges.len(), 1);
}

// ===== G3: snapshot path does not interfere with process_cycle =====

/// A source node: 0 inputs, 1 output, no-op `process` (its output scratch is
/// seeded via `Graph::feed` and left untouched each cycle — the canonical
/// 0.1.0 source pattern).
struct SourceNode {
    out: [PortDescriptor; 1],
}
impl SourceNode {
    fn new() -> Self {
        Self {
            out: [PortDescriptor::output(1, SampleFormat::F32)],
        }
    }
}
impl AudioNode for SourceNode {
    fn inputs(&self) -> &[PortDescriptor] {
        &[]
    }
    fn outputs(&self) -> &[PortDescriptor] {
        &self.out
    }
    fn process(
        &mut self,
        _ctx: &mut ProcessContext,
        _inp: &[audio_core_bsd::AudioFrame],
        _out: &mut [audio_core_bsd::AudioFrame],
    ) {
        // Intentionally a no-op: output scratch is seeded via Graph::feed.
    }
}

/// A 1-in/1-out pass-through node so the compiled graph has a real consumer.
struct Passthrough {
    inp: [PortDescriptor; 1],
    out: [PortDescriptor; 1],
}
impl Passthrough {
    fn new() -> Self {
        Self {
            inp: [PortDescriptor::input(1, SampleFormat::F32)],
            out: [PortDescriptor::output(1, SampleFormat::F32)],
        }
    }
}
impl AudioNode for Passthrough {
    fn inputs(&self) -> &[PortDescriptor] {
        &self.inp
    }
    fn outputs(&self) -> &[PortDescriptor] {
        &self.out
    }
    fn process(
        &mut self,
        _ctx: &mut ProcessContext,
        inp: &[audio_core_bsd::AudioFrame],
        out: &mut [audio_core_bsd::AudioFrame],
    ) {
        let (Some(i), Some(o)) = (inp.first(), out.get_mut(0)) else {
            return;
        };
        let n = i.samples.len().min(o.samples.len());
        for k in 0..n {
            o.samples[k] = i.samples[k];
        }
    }
}

#[test]
fn snapshot_extraction_is_safe_around_process_cycle() {
    // G3: the snapshot path (read-only &self) may run immediately before and
    // after a process_cycle (&mut self) on the SAME graph. The borrow checker
    // forbids concurrency; this test proves sequential use is sound and that
    // taking a snapshot does not disturb the RT cycle's output.
    let mut g = Graph::new();
    let a = g.add_node(Box::new(SourceNode::new()));
    let b = g.add_node(Box::new(Passthrough::new()));
    g.link((a, 0), (b, 0)).unwrap();
    g.compile(GraphConfig::new(8, 48_000, 1)).unwrap();

    // Snapshot BEFORE a cycle — read-only, no RT disturbance.
    let snap_before = g.topology_snapshot();
    assert_eq!(snap_before.nodes.len(), 2);
    assert_eq!(snap_before.edges.len(), 1);

    // Run one cycle: seed a, process, read b's output.
    g.feed(
        a,
        0,
        &audio_core_bsd::AudioFrame::from_planar(1, 48_000, vec![0.5_f32; 8]),
    );
    let mut ctx = ProcessContext::new(8, 0, 48_000);
    g.process_cycle(&mut ctx).unwrap();
    let out = g.read_output(b, 0).unwrap();
    assert!(out.samples.iter().all(|&s| (s - 0.5).abs() < 1e-6));

    // Snapshot AFTER the cycle — topology unchanged, output untouched.
    let snap_after = g.topology_snapshot();
    assert_eq!(snap_before, snap_after);
}
