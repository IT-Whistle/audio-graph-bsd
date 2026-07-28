# Changelog

All notable changes to this project will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

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

[Unreleased]: https://github.com/IT-Whistle/audio-graph/compare/v0.1.0...HEAD
[0.1.0]: https://github.com/IT-Whistle/audio-graph/releases/tag/v0.1.0
