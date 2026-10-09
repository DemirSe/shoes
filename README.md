# shoes

shoes is a high-performance proxy server written in Rust.

## Supported Protocols

### Proxy Protocols
- **VLESS** (with fallback support)
- **H2MUX** (HTTP/2 multiplexing for VLESS)

### Transport Protocols
- **TLS**
- **XTLS Reality**
- **XTLS Vision** (for VLESS)
- **XUDP** (UDP multiplexing over VLESS)

### TUN/VPN Mode
- **TUN device support** - Layer 3 VPN for transparent proxying
- Supported platforms: Linux, Android, iOS

## Features

- **TCP transport** with direct IPv4 and IPv6 outbound support
- **TLS with SNI routing**: Route by Server Name Indication
- **Upstream proxy chaining**: Multi-hop chains with load balancing
- **Rule-based routing**: Route by IP/CIDR or hostname masks
- **Named PEM certificates**: Define once, reference everywhere
- **TLS fingerprint authentication**: Certificate pinning for TLS
- **Hot reloading**: Apply config changes without restart
- **Unix socket support**: Bind to Unix domain sockets

For advanced access control (IP allowlist/blocklists), see [tobaru](https://github.com/cfal/tobaru).

## Installation

Precompiled binaries for x86_64 and Apple aarch64 are available on [Github Releases](https://github.com/cfal/shoes/releases).

Or install with cargo:

```bash
cargo install shoes
```

## Usage

```
shoes [OPTIONS] <config.yaml> [config.yaml...]

OPTIONS:
    -t, --threads NUM    Set the number of worker threads (default: CPU count)
    -d, --dry-run        Parse the config and exit
    --no-reload          Disable automatic config reloading on file changes

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

## Examples

See the [examples](./examples) directory for all examples.

### Basic VLESS Server
```yaml
- address: 0.0.0.0:443
  protocol:
    type: vless
    user_id: b85798ef-e9dc-46a4-9a87-8da4499d36d0
    udp_enabled: true
```

### VLESS with Vision over TLS
```yaml
- address: 0.0.0.0:443
  protocol:
    type: tls
    tls_targets:
      "vless.example.com":
        cert: cert.pem
        key: key.pem
        vision: true
        alpn_protocols: ["http/1.1"]
        protocol:
          type: vless
          user_id: b85798ef-e9dc-46a4-9a87-8da4499d36d0
          udp_enabled: true
```

### Reality Server
```yaml
- address: 0.0.0.0:443
  protocol:
    type: tls
    reality_targets:
      "www.example.com":
        private_key: "YOUR_BASE64URL_PRIVATE_KEY"
        short_ids: ["0123456789abcdef", ""]
        dest: "www.example.com:443"
        protocol:
          type: vless
          user_id: b85798ef-e9dc-46a4-9a87-8da4499d36d0
          udp_enabled: true
```

### Upstream VLESS Chain with H2MUX
```yaml
- address: 0.0.0.0:443
  protocol:
    type: vless
    user_id: b85798ef-e9dc-46a4-9a87-8da4499d36d0
  rules:
    - masks: "0.0.0.0/0"
      action: allow
      client_chain:
        address: "server.example.com:443"
        protocol:
          type: vless
          user_id: b85798ef-e9dc-46a4-9a87-8da4499d36d0
          h2mux:
            max_connections: 4
            min_streams: 1
```

### Upstream VLESS Chain
```yaml
- address: 0.0.0.0:443
  protocol:
    type: vless
    user_id: b85798ef-e9dc-46a4-9a87-8da4499d36d0
  rules:
    - masks: "0.0.0.0/0"
      action: allow
      client_chain:
        address: "server.example.com:443"
        protocol:
          type: reality
          public_key: "SERVER_PUBLIC_KEY"
          short_id: "0123456789abcdef"
          sni_hostname: "www.example.com"
          protocol:
            type: vless
            user_id: b85798ef-e9dc-46a4-9a87-8da4499d36d0
```

### TUN VPN
```yaml
- device_name: tun0
  address: 10.0.0.1
  netmask: 255.255.255.0
  mtu: 1500
  tcp_enabled: true
  udp_enabled: true
  rules:
    - masks: "0.0.0.0/0"
      action: allow
      client_chain:
        address: "proxy.example.com:443"
        protocol:
          type: tls
          protocol:
            type: vless
            user_id: b85798ef-e9dc-46a4-9a87-8da4499d36d0
```

## Similar Projects

- [SagerNet/sing-box](https://github.com/SagerNet/sing-box)
- [v2fly/v2ray-core](https://github.com/v2fly/v2ray-core)
- [XTLS/Xray-core](https://github.com/XTLS/Xray-core)
