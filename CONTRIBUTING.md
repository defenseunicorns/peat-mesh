# Contributing to peat-mesh

Thank you for your interest in contributing to peat-mesh. This document covers development setup, testing, and the pull request process.

## Getting Started

1. Fork the repository and clone your fork
2. Create a feature branch from `main`
3. Make your changes
4. Run pre-commit checks
5. Submit a pull request

## Development Setup

### Prerequisites

- Rust stable toolchain (install via [rustup](https://rustup.rs))
- Platform-specific dependencies for optional features:
  - **Bluetooth**: BlueZ 5.48+ and D-Bus development libraries (Linux)
  - **Kubernetes**: `kubectl` configured for a cluster (for integration testing)

### Feature Flags

peat-mesh uses Cargo feature flags to gate optional functionality:

| Feature | Description |
|---------|-------------|
| `automerge-backend` | Automerge CRDT storage with Iroh sync and redb persistence |
| `broker` | HTTP/WebSocket service broker (Axum-based) |
| `kubernetes` | Kubernetes-native peer discovery via CRDs |
| `bluetooth` | BLE mesh transport via peat-btle |
| `lite-bridge` | Lightweight transport bridge via peat-lite |
| `node` | Binary target combining all features above |

### Building

```bash
# Default build (core library only)
cargo build

# Individual features
cargo build --features automerge-backend
cargo build --features broker
cargo build --features kubernetes
cargo build --features bluetooth
cargo build --features lite-bridge

# Combined features
cargo build --features automerge-backend,broker

# Full node binary (all features)
cargo build --features node
```

## Testing

```bash
# Unit tests (default features)
cargo test

# Tests with specific features enabled
cargo test --features automerge-backend
cargo test --features automerge-backend,broker

# Run a specific integration test
cargo test --test topology_manager_e2e
```

Integration tests live in the `tests/` directory and cover transport failover, partition healing, metrics, topology management, and negentropy sync.

## Pre-Commit Checks

Before submitting a PR, ensure all of the following pass locally:

```bash
cargo fmt --check
cargo clippy -- -D warnings
cargo test
cargo build --features automerge-backend
cargo build --features broker
cargo build --features bluetooth
```

The CI pipeline runs these same checks on every PR.

## Branching Strategy

We use **trunk-based development** on `main` with short-lived feature branches:

- Branch from `main` for all changes
- Keep branches small and focused (prefer multiple small PRs over one large one)
- Squash-and-merge to `main`

## Commit Requirements

- **GPG-signed commits are required.** Configure commit signing per [GitHub's documentation](https://docs.github.com/en/authentication/managing-commit-signature-verification).
- Write clear, descriptive commit messages

## Pull Request Process

1. Open a PR against `main` with a clear description of the change
2. Focus each PR on a single concern
3. Ensure CI passes (fmt, clippy, tests, feature builds)
4. PRs require at least one approving review from a CODEOWNERS member
5. PRs are squash-merged to maintain a clean history

## Radicle Patches

peat-mesh also accepts contributions via [Radicle](https://radicle.xyz). The repository includes a `.goa` CI script that automatically runs format checks, clippy, tests, and per-feature builds against incoming patches. Community patches require a delegate to comment `ok-to-test` before CI runs.

## Architectural Changes

For significant architectural changes, open an issue first to discuss the approach. Reference the relevant ADR (Architecture Decision Record) if one exists, or propose a new one.

## Reporting Issues

Use GitHub Issues to report bugs or request features. Include steps to reproduce, expected vs. actual behavior, and relevant log output.

## Code of Conduct

All contributors are expected to follow our [Code of Conduct](CODE_OF_CONDUCT.md).

## License

By contributing, you agree that your contributions will be licensed under the [Apache License 2.0](LICENSE).
