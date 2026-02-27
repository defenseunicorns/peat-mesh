# Changelog

All notable changes to this project will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [0.2.0] - 2026-02-20

### Added

- Extract sync modules from peat-protocol into peat-mesh (automerge_sync, sync_channel, sync_forwarding)
- Wire AutomergeSyncCoordinator into peat-mesh-node binary with 5s polling task
- Update deployment guide for Automerge sync stack

### Fixed

- Apply cargo fmt and clippy fixes for CI compliance

## [0.1.0] - 2026-02-18

### Added

- Initial extraction as standalone crate from peat-protocol
- Complete Peat-Lite CRDT encodings, query handler, and InMemoryBackend
- Kubernetes support: discovery, headless service, StatefulSet (ADR-0001)
- Kubernetes deployment infrastructure: Dockerfile, Helm chart, health probes (ADR-0001 Phase 2)
- HTTP/WS service broker with REST API and WebSocket event streaming
- Device keypair (Ed25519) and formation key security primitives
- mDNS, static, and Kubernetes peer discovery strategies
- Topology management with partition detection and autonomous operation
- Beacon broadcasting and observation for geographic mesh awareness
- GOA CI script for Radicle patches
- ADR-0001 through ADR-0009 transferred from peat repo
- README with CI and usage details

### Fixed

- CI pipe buffer deadlock (redirect verbose output to log file)
- Add crates.io metadata for discoverability
