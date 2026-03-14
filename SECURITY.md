# Security Policy

## Reporting a Vulnerability

If you discover a security vulnerability in peat-mesh, please report it responsibly. **Do not open a public GitHub issue for security vulnerabilities.**

### How to Report

You have two options:

1. **Email**: Send a detailed report to [security@defenseunicorns.com](mailto:security@defenseunicorns.com)
2. **GitHub Security Advisories**: Use the [private vulnerability reporting](https://github.com/defenseunicorns/peat-mesh/security/advisories/new) feature on this repository

### What to Include

- Description of the vulnerability
- Steps to reproduce
- Potential impact
- Suggested fix (if any)

### Response Timeline

- **Acknowledgment**: Within 3 business days
- **Initial assessment**: Within 10 business days
- **Fix timeline**: Dependent on severity

### Disclosure Policy

- We will acknowledge reporters in the remediation PR (unless anonymity is requested)
- We follow coordinated disclosure practices
- We aim to release patches before public disclosure

## Supported Versions

| Version | Supported |
|---------|-----------|
| latest  | Yes       |

## Security-Relevant Areas

peat-mesh is a P2P mesh networking library. The following areas are particularly security-sensitive:

- **Mesh networking**: Peer discovery, topology management, and message routing
- **QUIC transport**: TLS configuration, connection establishment, and stream multiplexing
- **Certificate enrollment**: Identity issuance, validation, and chain-of-trust verification
- **CRDT sync**: Conflict-free replicated data type synchronization and state convergence

When integrating peat-mesh, follow these practices:

- Validate peer identities before accepting connections
- Use the provided certificate enrollment flow rather than manual certificate management
- Keep dependencies up to date
- Monitor QUIC transport configurations for secure defaults
