# shoes (server fork)

This repository is a Linux-focused server fork of [`shoes` 0.3.2](https://github.com/cfal/shoes)
(Rust proxy server by cfal). It is not Xray-core, and binaries built from this
tree are not the upstream `shoes` release. The fork serves one deployment
profile — VLESS + REALITY + Vision over TCP. Stock Xray/Hiddify wire
compatibility on Android, Windows, and Linux is a requirement, not a claim of
a completed platform test matrix. See [verification limits](docs/guvenlik-ve-limitler.md).

## Start Here

| Task | Authoritative guide |
|---|---|
| Build, test or maintain the fork | [CONTRIBUTING.md](CONTRIBUTING.md) |
| Understand the code and lifecycle | [Architecture](docs/mimari.md) |
| Look up YAML fields | [CONFIG.md](CONFIG.md) |
| Understand the restricted server profile | [Production policy](docs/konfigurasyon.md) |
| Deploy, health-check or roll back | [Operations runbook](docs/operasyon.md) |
| Find all documentation | [Documentation index](docs/README.md) |
| Review pending fork changes | [CHANGELOG.md](CHANGELOG.md) |

The source tree, local verification and live deployment are distinct states.
No documentation example, test result or source change authorizes a production change.

## Supported Protocols

### Proxy Protocols
- **VLESS** (the optional VLESS fallback is not configured in this fork)
- **H2MUX** (HTTP/2 multiplexing for VLESS)

### Transport Protocols
- **TLS**
- **XTLS Reality**
- **XTLS Vision** (for VLESS)
- **XUDP** (UDP multiplexing over VLESS)

### TUN/VPN Mode
- **TUN device support** - Layer 3 VPN for transparent proxying
- Linux-first; TUN/FFI code is retained for unix targets including Android/iOS.
  Intended client deployments require TUN on with no bypass rules; current
  verification limits are documented above.

## Features

- **TCP transport** with direct IPv4 and IPv6 outbound support
- **TLS with SNI routing**: Route by Server Name Indication
- **Upstream proxy chaining**: Multi-hop chains with load balancing
- **Rule-based routing**: Route by IP/CIDR or hostname masks
- **Named PEM certificates**: Define once, reference everywhere
- **TLS fingerprint authentication**: Certificate pinning for TLS
- **Hot reloading**: Transactional TCP config replacement; TUN replacement requires restart.
  The recorded production unit uses `--no-reload`.
- **Unix socket support**: Bind to Unix domain sockets
- **Memory (GNU/Linux CLI)**: 60-second jemalloc background decay; `src/memory.rs` trim stays glibc-only

For advanced access control (IP allowlist/blocklists), see [tobaru](https://github.com/cfal/tobaru).

## Installation

Build this fork from this source tree. Upstream release binaries and
`cargo install shoes` do not contain this fork's changes.
Use the locked offline workflow described in
[CONTRIBUTING.md](./CONTRIBUTING.md):

```bash
CARGO_TARGET_DIR="$HOME/.cache/shoes-target" \
cargo build -p shoes --bin shoes --release --locked --offline
```

Point `CARGO_TARGET_DIR` outside the repository before building; `/tmp` is
often a small tmpfs, so prefer a directory under your home directory or cache.

## Usage

```
shoes [OPTIONS] <config.yaml> [config.yaml...]

OPTIONS:
    -t, --threads NUM    Set the number of worker threads (default: CPU count)
    -l, --log-file PATH  Log to file (repeatable; "-" means stderr; default: stderr)
    -d, --dry-run        Parse the config and exit
    --no-reload          Disable automatic config reloading on file changes
    --production-profile Enforce the public IPv4:443 whatsapp.net REALITY/Vision profile
    -V, --version        Print version information and exit

COMMANDS:
    generate-reality-keypair    Generate a new Reality X25519 keypair
    generate-vless-user-id      Generate a random VLESS user ID (UUID v4)
```

### Examples
```bash
# Run with a single config file
shoes config.yaml

# Run with multiple config files
shoes server1.yaml server2.yaml rules.yaml

# Run with custom thread count
shoes --threads 8 config.yaml

# Validate configuration without starting
shoes --dry-run config.yaml

# Run without hot-reloading
shoes --no-reload config.yaml

# Generate Reality keypair
shoes generate-reality-keypair

# Generate a VLESS user ID
shoes generate-vless-user-id
```

## Configuration

See [CONFIG.md](./CONFIG.md) for the complete YAML configuration reference.

This server fork offers an opt-in `--production-profile` validator. General
examples below are not its deployment profile. Enable the validator only through
an approved deployment. Invalid TCP reloads retain the running generation;
`--dry-run` failures return a nonzero status. Server `user_id` accepts a scalar or
a UUID list (`user_ids` alias); the shared UUID's ten-device limit is administrative,
not inferred from connection or IP counts.

CI requires pre-provisioned offline runners; see [.github/OFFLINE-CI.md](.github/OFFLINE-CI.md).

## Examples

Use the [example catalog](examples/README.md) for general REALITY, Vision,
TLS/VLESS and TUN examples. They are illustrative profiles, not production-ready deployments.
Use the [production skeleton](docs/konfigurasyon.md#üretim-profili-iskeleti-yer-tutucular)
for this server's stricter requirements and [CONFIG.md](CONFIG.md) for chaining and H2MUX fields.
Keep real addresses, UUIDs, keys, short IDs and configuration files out of the repository.

## Deployment

Deployment is manual and owner-approved, for the existing operator's Linux/systemd
server layout only. Follow the [operations runbook](docs/operasyon.md): verify the
artifact SHA-256, stage under `/var/lib/shoes-maintenance/<id>/` with a verified
`rollback.bin`, install to a noncolliding sibling path and atomically `mv`, restart
the unit, observe bounded health, and revert manually if a check fails.
The local deployment script and its offline tests were removed on 2026-10-16
(owner decision). No workflow contacts the VPS. Backup cleanup is a separately
approved operation; maintenance backups are not pruned automatically.

## Similar Projects

- [SagerNet/sing-box](https://github.com/SagerNet/sing-box)
- [v2fly/v2ray-core](https://github.com/v2fly/v2ray-core)
- [XTLS/Xray-core](https://github.com/XTLS/Xray-core)
