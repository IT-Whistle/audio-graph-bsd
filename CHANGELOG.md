# Changelog

All notable changes to this project will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

## [0.3.0] - 2026-07-30
### Added — Track 1 (topology API, `topology` feature)
- `TopologySnapshot` serializable topology model (nodes + edges + port metadata), `serde`-derived.
- `SnapshotEdge`, `NodeSnapshot`, `PortMeta` (with lossless `audio-core-bsd` `PortDescriptor` conversion), `PortDir`, `SampleFmt` serialization mirrors.
- `TopologyEvent` (6 variants) and `Mutation` (4 variants) enums, `serde`-derived.
- `SnapshotSource` / `TopologyObserver` traits.
- `Graph::topology_snapshot`, `Graph::from_snapshot` (node-factory rebuild — metadata-only), `Graph::subscribe_topology` (`mpsc` events on the non-RT mutation path).
- G3 (mutation ≠ RT) + G4 (serde roundtrip) gates.

### Added — Track 2 (distributed-prep, `distributed` feature)
- `RemoteNode`, `PortKind`, `BoundaryPort`, `PartitionHint`, `TransportHint` serializable models.
- `GraphPartition` (per-partition `compile` / `process_cycle`).
- `NetworkLinkNode` trait (the rtrb-bridge contract a network link node follows; impl by sonicbrew M09).
- `Graph::partition_hints`.
- G5 (partition-independent execution) + G6 (network-link RT alloc-free) gates.

### Added — Track 3 (Phase C hot-reload)
- `RtHandle`: atomic snapshot swap of a compiled `Graph` via `arc-swap` (ROADMAP §5 strategy B). `install` (control thread) + `process_cycle` / `graph` (RT thread, wait-free load). Lossless, pause-free hot-reload.
- `Graph::process_cycle` refactored to `&self` (scratch behind `UnsafeCell`, single-RT-thread invariant) so it is callable on a graph loaded from `ArcSwap<Graph>`. `unsafe impl Sync for Graph` (sound under the single-RT-thread invariant).
- G1 (RT alloc-free incl. arc-swap load, post-warmup) gate verified; Miri UB-free over the distributed lib.

### Changed
- `serde` and `arc-swap` are **optional** deps (`topology` / `distributed` features). The default build stays serde-free and 0.1.0-compatible.

## [0.1.0] - 2026-07-29
### Added
- `Graph` real-time-safe directed node-graph engine (`add_node` / `link` / `compile` / `process_cycle`).
- Topological scheduling via Kahn's algorithm with compile-time cycle detection.
- Pre-allocated per-port RT scratch frames so `process_cycle` is allocation-free.
- `GraphConfig` compile-time configuration fixing `num_frames` / `sample_rate` / `channels`.
- `NodeId` / `PortIdx` / `LinkId` stable identifiers.
- `GraphError` construction/compile error enum (thiserror-backed).
- `RingSource` / `RingSink` rtrb-backed bridge nodes for worker-thread I/O.
- Comprehensive test suite: unit, property (proptest), integration, RT-safety (alloc-free over 1000 cycles), and concurrency.

[Unreleased]: https://github.com/IT-Whistle/audio-graph-bsd/compare/v0.3.0...HEAD
[0.3.0]: https://github.com/IT-Whistle/audio-graph-bsd/releases/tag/v0.3.0
[0.1.0]: https://github.com/IT-Whistle/audio-graph-bsd/releases/tag/v0.1.0
