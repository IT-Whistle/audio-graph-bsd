# Changelog

All notable changes to this project will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

## [0.4.1] - 2026-08-01
### Added — test hardening (heatmap 강도 충족, 기능 변화 없음)
- `tests/rt_flush_regression.rs`: RT-safety 교차 회귀 게이트 — `process_cycle` + `flush_sinks` 교차 1000회 패턴에서 `process_cycle` 구간 alloc=0(thread-local CountingAllocator, MEASURING 윈도우가 process_cycle에만 스코프). flush가 RT 경로에 할당 누출시키지 않음 실증.
- `tests/flush_property.rs`: proptest — 임의 신호 flush round-trip 비트 보존, 임의 ring capacity ring-full no-panic, plain 노드 NotFlushable (heatmap Property i4).
- `benches/process_cycle_latency.rs`: `10_node_with_sink_flush_256f` 케이스 추가(cycle + between-cycle flush 지역, NodeSlot delegate 회귀 가드).
- Miri UB-free 확장: `flush_sinks`(5) + `rt_flush_regression`(2) 통합 경래.
- FreeBSD 네이티브 cargo test 통과(테스트노드 192.168.39.2 FreeBSD 15.1-RELEASE-p1, default ~66 + distributed ~91 passed).

## [0.4.0] - 2026-08-01
### Added — flush-gap (engine-changes §4, Option A)
- `Flushable` trait + `FlushError` enum (`src/flush.rs`): the contract a sink node fulfills to drain its stashed frame off the RT thread.
- `SinkNode` trait (`AudioNode + Flushable`, blanket-impl) so the engine tracks flushable sinks by type — no `Any` downcast.
- `impl Flushable for RingSink` (OFF-RT clone+push, maps `rtrb::PushError` → `FlushError::RingFull`).
- `Graph::add_sink(Box<dyn SinkNode>) -> NodeId`: registers a flushable sink (internal `NodeSlot` enum: `Plain` / `Sink`).
- `Graph::flush_sinks(&mut self) -> (usize, Option<FlushError>)`: drains every sink between cycles (off-RT). First error reported, remaining sinks still flushed.
- `Graph::flush_sink(&mut self, NodeId) -> Result<(), FlushError>`: targeted flush; returns `NotFlushable` for plain nodes, `NodeNotFound` for missing ids.
- Integration tests (`tests/flush_sinks.rs`): stash→consumer round-trip, ring-full reporting, plain/missing-node rejection, targeted flush.
- Resolves the outbound audio gap blocking sonicbrew M09/M10/M12 (graph → client shipping).

### Changed
- `Graph` internal node storage is now `Vec<NodeSlot>` (enum) instead of `Vec<Box<dyn AudioNode>>`. `NodeSlot` implements `AudioNode` by delegation, so `add_node` / `process_cycle` / `link` / `compile` / `read_*` / topology / partition APIs are unchanged. Existing `add_node(Box<dyn AudioNode>)` API is fully preserved.

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

[Unreleased]: https://github.com/IT-Whistle/audio-graph-bsd/compare/v0.4.1...HEAD
[0.4.1]: https://github.com/IT-Whistle/audio-graph-bsd/releases/tag/v0.4.1
[0.4.0]: https://github.com/IT-Whistle/audio-graph-bsd/releases/tag/v0.4.0
[0.3.0]: https://github.com/IT-Whistle/audio-graph-bsd/releases/tag/v0.3.0
[0.1.0]: https://github.com/IT-Whistle/audio-graph-bsd/releases/tag/v0.1.0
