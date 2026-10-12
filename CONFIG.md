# Configuration Reference

shoes uses YAML configuration files. Multiple configuration types can be combined in a single file or split across multiple files.

## Table of Contents
- [Configuration Structure](#configuration-structure)
- [Server Config](#server-config)
- [Server Protocols](#server-protocols)
- [TUN Config](#tun-config)
- [Client Config](#client-config)
- [Client Protocols](#client-protocols)
- [Rules System](#rules-system)
- [Named Groups](#named-groups)
- [Named PEMs](#named-pems)
- [Advanced Features](#advanced-features)
- [Command Line](#command-line)

## Configuration Structure

A configuration file is a YAML array containing one or more configuration entries. Each entry can be:

- **Server Config** - Defines a proxy server instance
- **TUN Config** - Defines a TUN/VPN device for transparent proxying
- **Client Config Group** - Defines reusable upstream proxy configurations
- **Rule Config Group** - Defines reusable routing rules
- **Named PEM** - Defines reusable certificate/key data

```yaml
# Server configs have 'address' or 'path'
- address: "0.0.0.0:8080"
  protocol: ...

# TUN configs have 'device_name' or 'device_fd'
- device_name: "tun0"
  address: "10.0.0.1"
  ...

# Client config groups have 'client_group'
- client_group: my-upstream
  client_proxy: ...

# Rule config groups have 'rule_group'
- rule_group: my-rules
  rules: ...

# Named PEMs have 'pem'
- pem: my-cert
  path: /path/to/cert.pem
```

## Server Config

```yaml
# Bind to IP address and port
address: "0.0.0.0:8080"        # IPv4
address: "[::]:8080"           # IPv6
address: "0.0.0.0:443-445"     # Port range

# OR bind to multiple addresses and port ranges (`address` is also accepted)
addresses:
  - "127.0.0.1:22223"
  - "172.17.0.1:22223-22225"

# OR bind to Unix socket (TCP only)
path: "/tmp/shoes.sock"

# Protocol configuration (required)
protocol: ServerProxyConfig

# Transport layer (default: tcp)
transport: tcp

# TCP settings (only when transport: tcp)
tcp_settings:
  no_delay: true               # Default: true

# Routing rules (default: allow-all-direct)
rules: string | [RuleConfig]
```

## Server Protocols

### VLESS
```yaml
protocol:
  type: vless
  user_id: string | [string]   # UUID or UUID list; alias: user_ids
  udp_enabled: true            # Default: true (enables XUDP)
  fallback: string?            # Optional fallback destination for failed auth (e.g., "127.0.0.1:80")
```

Existing single-UUID profiles remain valid. A list authenticates any listed UUID
once at connection setup, including REALITY/Vision, UDP and XUDP. Duplicate UUIDs,
empty lists and invalid UUIDs are rejected. Vision replies use the UUID that
authenticated the connection, not the first list entry.

UUID count and connection/IP limits are not device counts. The owner selected
administrative enforcement of at most ten devices for the shared UUID. Keep the
shared profile and stock-client protocol; do not describe this as automatic device
count enforcement. Multiple UUIDs do not alter the data transfer path.

### TLS Server
```yaml
protocol:
  type: tls

  # Standard TLS targets (by SNI)
  tls_targets:                 # Aliases: sni_targets, targets
    "example.com":
      cert: string             # Certificate (path or named PEM)
      key: string              # Private key (path or named PEM)
      alpn_protocols: [string] # Optional ALPN
      client_ca_certs: [string] # Optional client CA certs
      client_fingerprints: [string] # Optional client cert fingerprints
      vision: false            # Enable Vision (requires VLESS inner protocol)
      protocol: ServerProxyConfig
      override_rules: [RuleConfig] # Optional rule override

  # Default TLS target (for unmatched/no SNI)
  default_tls_target:          # Aliases: default_target
    cert: string
    key: string
    # ... same fields as tls_targets

  # Reality targets (by SNI)
  reality_targets:
    "www.cloudflare.com":
      private_key: string      # X25519 private key (base64url)
      short_ids: [string]      # Valid client IDs (hex, 0-16 chars)
      dest: string             # Fallback destination (e.g., "example.com:443")
      dest_client_chain: ClientChain?  # Optional proxy chain for reaching dest
      max_time_diff: 60000     # Max timestamp diff in ms (default: 60000)
      min_client_version: [1, 8, 0]  # Optional [major, minor, patch]
      max_client_version: [2, 0, 0]  # Optional [major, minor, patch]
      cipher_suites: [string]  # Optional TLS 1.3 cipher suites (see below)
      vision: false            # Enable Vision (requires VLESS inner protocol)
      protocol: ServerProxyConfig
      override_rules: [RuleConfig]

  # Buffer size for TLS (optional, min 16384)
  tls_buffer_size: int
```

### REALITY transparent cover rate controls

Each REALITY target accepts optional `limit_fallback_upload` and
`limit_fallback_download` mappings. Each mapping has these unsigned integer fields:

- `after_bytes`: Initial bytes without throttling. The default is zero.
- `bytes_per_sec`: Sustained bytes per second. Zero or an omitted mapping disables that direction.
- `burst_bytes_per_sec`: Bucket capacity in bytes, despite the field name. Zero or omission uses `bytes_per_sec` as the capacity.

The bucket starts full at the threshold. It refills continuously at `bytes_per_sec`, up to its capacity.
Upload means client to cover. Download means cover to client.
Each forwarded connection has independent buckets. There is no shared traffic cap.
Successful authenticated REALITY, Vision, and VLESS forwarding does not use these controls.

Accounting uses bytes that the fallback writer accepts, including TLS record headers.
Preread cover records, cover remainder bytes, and pending client bytes use the same accounting as live traffic.
The initial ClientHello reaches the cover before authentication, without throttling.
This bootstrap exclusion preserves the existing handshake sequence.
Those already-forwarded bytes reduce the upload `after_bytes` allowance, but do not consume bucket tokens or incur later debt.
The controls do not change the cover destination, SNI, or optional VLESS fallback.

Byte limits delay traffic. They do not promise that every reply byte arrives.

The copy loop has two shutdown rules. A direction that reaches end-of-file shuts down its peer direction.
The default rule fails that shutdown after five seconds.
Fallback sessions use the progress-aware rule.
That rule extends the deadline while either direction still moves bytes.
A stall without progress then fails after five seconds.
The rule also caps the total wait at sixty seconds after the first half-close.
Bytes that stay unwritten at that point are lost.

Normal shutdowns complete at once. The sixty-second limit applies only while a shutdown stays pending.
Any write error still ends the copy at once. The error path can drop queued bytes in either direction.

## TUN Config

TUN (network TUNnel) devices operate at the IP layer (Layer 3), allowing shoes to act as a transparent VPN.

```yaml
# Linux: Create TUN device by name
device_name: string            # Device name (e.g., "tun0")
address: string                # Device IP address (e.g., "10.0.0.1")
netmask: string?               # Netmask (e.g., "255.255.255.0")
destination: string?           # Gateway/destination (Linux only)

# iOS/Android: Use existing file descriptor
device_fd: int                 # FD from VpnService (Android) or NEPacketTunnelProvider (iOS)

# Common settings
mtu: 1500                      # Default: 1500 (Linux), 9000 (Android), 4064 (iOS)
tcp_enabled: true              # Default: true
udp_enabled: true              # Default: true
icmp_enabled: true             # Default: true

# Routing rules
rules: [RuleConfig]
```

**Platform notes:**
- **Linux**: Requires root or `CAP_NET_ADMIN`. Creates device with specified name/address.
- **Android**: Use `device_fd` from `VpnService.Builder.establish()`. Routes configured via VpnService.
- **iOS**: Use `device_fd` from `NEPacketTunnelProvider.packetFlow`.

**Example (Linux):**
```yaml
- device_name: "tun0"
  address: "10.0.0.1"
  netmask: "255.255.255.0"
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
            user_id: "uuid"
```

## Client Config

Used in rules to specify upstream proxies.

```yaml
address: string                # Proxy server address (e.g., "proxy.example.com:1080")
protocol: ClientProxyConfig
transport: tcp                 # Default: tcp
bind_interface: string         # Optional, Linux/Android/Fuchsia only

tcp_settings:
  no_delay: true
```

## Client Protocols

### Direct
```yaml
protocol:
  type: direct
```

### VLESS
```yaml
protocol:
  type: vless
  user_id: string
  h2mux:                         # Optional h2mux multiplexing (see below)
    max_connections: 4
    min_streams: 4
    max_streams: 0
    padding: false
```

### H2MUX Multiplexing

H2MUX multiplexes multiple proxy streams over a single HTTP/2 connection, reducing connection overhead. Compatible with sing-box. Available for the VLESS client protocol.

```yaml
h2mux:
  max_connections: 4           # Maximum connections to maintain (default: 4)
  min_streams: 4               # Min streams before opening new connection (default: 4)
  max_streams: 0               # Max streams per connection, 0 = unlimited (default: 0)
  padding: false               # Enable padding for traffic obfuscation (default: false)
```

**Server support:** H2MUX is auto-detected on servers. No configuration needed.

### TLS Client
```yaml
protocol:
  type: tls
  verify: true                 # Default: true
  server_fingerprints: [string]
  sni_hostname: string
  alpn_protocols: [string]
  tls_buffer_size: int
  cert: string                 # Client certificate for mTLS
  key: string                  # Client key for mTLS
  vision: false                # Enable Vision (requires VLESS inner protocol)
  protocol: ClientProxyConfig
```

### Reality Client
```yaml
protocol:
  type: reality
  public_key: string           # Server's X25519 public key (base64url)
  short_id: string             # Your client ID (hex, 0-16 chars)
  sni_hostname: string         # SNI to send (must match server's reality_targets key)
  cipher_suites: [string]      # Optional TLS 1.3 cipher suites (see below)
  vision: false                # Enable Vision (requires VLESS inner protocol)
  protocol: ClientProxyConfig  # Inner protocol (typically VLESS)
```

**Reality cipher suites:** Valid values are `TLS_AES_128_GCM_SHA256`, `TLS_AES_256_GCM_SHA384`, `TLS_CHACHA20_POLY1305_SHA256`. If not specified, all three are offered/supported.

## Rules System

Rules determine how incoming connections are routed.

### Rule Config
```yaml
rules:
  - masks: string | [string]   # IP/CIDR or hostname masks
    action: allow | block
    # For action: allow
    override_address: string?  # Optional address override
    client_chain: ClientChain | [ClientChain]  # Proxy chain(s) for routing
```

### Client Chains

Client chains define how traffic is routed through upstream proxies. Each chain is a sequence of "hops" - proxies that traffic passes through in order.

```yaml
# Single proxy (simplest form)
client_chain: my-proxy-group           # Reference a named group
client_chain:                          # Or inline config
  address: "proxy.example.com:443"
  protocol:
    type: vless
    user_id: "550e8400-e29b-41d4-a716-446655440000"

# Multi-hop chain (traffic goes: client -> hop1 -> hop2 -> target)
client_chain:
  chain:
    - first-proxy-group
    - second-proxy-group

# Multiple chains (round-robin selection)
client_chains:
  - us-proxy-group                     # Chain 1: single hop
  - chain: [proxy1, proxy2]            # Chain 2: multi-hop

# Load balancing at a hop (pool)
client_chain:
  chain:
    - pool: [us-proxies, eu-proxies]   # Round-robin between pool members
    - final-proxy
```

**Migration note:** The `client_proxy` / `client_proxies` fields still work but are deprecated. Please migrate to `client_chain` / `client_chains`.

### Mask Syntax
```yaml
# IP/CIDR masks
masks: "0.0.0.0/0"             # All IPv4
masks: "::/0"                  # All IPv6
masks: "192.168.0.0/16"        # Subnet
masks: "10.0.0.1:80"           # Specific IP and port

# Hostname masks
masks: "*.google.com"          # Wildcard subdomain
masks: "example.com"           # Exact match

# Multiple masks
masks:
  - "192.168.0.0/16"
  - "10.0.0.0/8"
  - "*.internal.com"
```

### Built-in Rule Groups
- `allow-all-direct` - Allow all connections, direct routing
- `block-all` - Block all connections

### Example Rules
```yaml
rules:
  # Direct connection for local networks
  - masks: ["192.168.0.0/16", "10.0.0.0/8", "172.16.0.0/12"]
    action: allow
    client_chain:
      protocol:
        type: direct

  # Block specific domains
  - masks: ["*.ads.example.com", "tracking.example.com"]
    action: block

  # Route through upstream proxy
  - masks: "0.0.0.0/0"
    action: allow
    client_chain:
      address: "proxy.example.com:443"
      protocol:
        type: vless
        user_id: "550e8400-e29b-41d4-a716-446655440000"
```

## Named Groups

### Client Proxy Group
```yaml
- client_group: my-upstream
  client_proxies:              # Define proxies in this group
    - address: "proxy1.example.com:443"
      protocol:
        type: vless
        user_id: "550e8400-e29b-41d4-a716-446655440000"
    - address: "proxy2.example.com:443"
      protocol:
        type: vless
        user_id: "550e8400-e29b-41d4-a716-446655440000"

# Reference in rules
- address: "0.0.0.0:8080"
  protocol:
    type: vless
    user_id: "550e8400-e29b-41d4-a716-446655440000"
  rules:
    - masks: "0.0.0.0/0"
      action: allow
      client_chain: my-upstream  # Reference by name
```

### Rule Group
```yaml
- rule_group: standard-rules
  rules:
    - masks: ["192.168.0.0/16"]
      action: allow
      client_chain:
        protocol:
          type: direct
    - masks: "0.0.0.0/0"
      action: allow
      client_chain: my-upstream

# Reference in server config
- address: "0.0.0.0:8080"
  protocol:
    type: vless
    user_id: "550e8400-e29b-41d4-a716-446655440000"
  rules: standard-rules        # Reference by name
```

## Named PEMs

Define certificates once and reference throughout configuration.

```yaml
# From file
- pem: my-cert
  path: /path/to/certificate.pem

# Inline data
- pem: my-key
  data: |
    -----BEGIN PRIVATE KEY-----
    ...
    -----END PRIVATE KEY-----

# Reference in config
- address: "0.0.0.0:443"
  protocol:
    type: tls
    tls_targets:
      "example.com":
        cert: my-cert          # Reference by name
        key: my-key
        protocol:
          type: vless
          user_id: "550e8400-e29b-41d4-a716-446655440000"
```

## Advanced Features

### Vision (XTLS-Vision)

Vision optimizes TLS-in-TLS scenarios by detecting inner TLS traffic and switching to direct mode for zero-copy performance.

**Requirements:**
- Inner protocol MUST be VLESS
- Works with both TLS and Reality

```yaml
# TLS + Vision
protocol:
  type: tls
  tls_targets:
    "example.com":
      cert: cert.pem
      key: key.pem
      vision: true
      alpn_protocols: ["http/1.1"]
      protocol:
        type: vless
        user_id: "uuid"

# Reality + Vision
protocol:
  type: tls
  reality_targets:
    "www.google.com":
      private_key: "..."
      short_ids: ["..."]
      dest: "www.google.com:443"
      vision: true
      protocol:
        type: vless
        user_id: "uuid"
```

### XUDP Multiplexing

Automatically enabled for VLESS when `udp_enabled: true`. Multiplexes UDP traffic over a single connection.

### Proxy Chaining

**Protocol nesting** (wrap one protocol in another):

```yaml
client_chain:
  address: "proxy.example.com:443"
  protocol:
    type: tls
    protocol:
      type: vless
      user_id: "550e8400-e29b-41d4-a716-446655440000"
```

**Multi-hop chains** (route through multiple proxies sequentially):

```yaml
client_chain:
  chain:
    - address: "proxy1.example.com:443"
      protocol:
        type: vless
        user_id: "550e8400-e29b-41d4-a716-446655440000"
    - address: "proxy2.example.com:443"
      protocol:
        type: tls
        protocol:
          type: vless
          user_id: "uuid"
```

### Hot Reloading

Configuration changes are automatically detected and applied without restarting. Disable with `--no-reload` flag.

TCP reloads prepare and bind the complete replacement before stopping existing
accept loops. Unchanged addresses retain their listening sockets. Invalid YAML,
protocol settings, duplicate addresses or failed new binds retain the current
generation. Existing TCP connections drain according to `reload_grace_secs`.
Atomic file replacement is detected by watching the containing directories.
Both the configured path and its existing canonical target are watched, including
targets in a different directory. A successful reload refreshes these watches after
symlink retargeting. If canonicalization fails, the configured path remains watched;
this does not guarantee detection of every change to a missing symlink target.

New addresses must bind while the old listening sockets are still open. Address
family changes or overlapping wildcard scopes can therefore be rejected by the
kernel, especially with dual-stack IPv6 listeners. Such a rejection retains the
current generation. Use a separately approved controlled restart for conflicting
address transitions; no automatic stop-and-rebind fallback is attempted.

TUN live replacement is rejected without stopping the existing tunnel; use a
separately approved controlled restart for TUN configuration changes.
Existing Unix socket paths are never blindly deleted or overwritten during startup.

### mTLS (Mutual TLS)

Require client certificates for authentication:

```yaml
# Server side
protocol:
  type: tls
  tls_targets:
    "example.com":
      cert: server.crt
      key: server.key
      client_ca_certs: [ca.crt]  # Required CA
      client_fingerprints: ["sha256:..."]  # Optional specific certs
      protocol: ...

# Client side
client_chain:
  address: "example.com:443"
  protocol:
    type: tls
    cert: client.crt
    key: client.key
    protocol: ...
```

## Command Line

```bash
shoes [OPTIONS] <config.yaml> [config.yaml...]

OPTIONS:
  -t, --threads NUM    Worker threads (default: CPU count)
  -d, --dry-run        Parse config and exit
  --no-reload          Disable hot-reloading
  --production-profile Enforce this fork's deployment policy

COMMANDS:
  generate-reality-keypair    Generate Reality X25519 keypair
  generate-vless-user-id      Generate a random VLESS user ID (UUID v4)
```

`--dry-run` returns a nonzero status for invalid configuration. Unsupported UDP
listeners are rejected; UDP/XUDP remain available inside the TCP tunnel.

`--production-profile` applies on initial load, dry-run and every reload. It requires
exactly one explicit public IPv4 TCP 443 listener, exactly `whatsapp.net` for REALITY
SNI and destination port 443, Vision, VLESS with UDP/XUDP enabled, and no optional
VLESS fallback or additional TLS targets. It is opt-in to preserve local fixtures.
The flag must be enabled in a separately approved deployment; this code change
does not validate or modify the live server profile. Client TUN routing and bypass
policy remain separate verification tasks.

## Tips

### Generate Keys

**Reality keypair:**
```bash
shoes generate-reality-keypair
```

**VLESS user ID:**
```bash
shoes generate-vless-user-id
```

**TLS certificate fingerprint:**
```bash
openssl x509 -in cert.pem -noout -fingerprint -sha256
```

### Security Best Practices

- Use strong, random passwords
- Keep private keys secure
- Use `127.0.0.1` instead of `0.0.0.0` for local-only access
- Use firewall rules to restrict access
- Enable client certificate authentication for sensitive services
- Use Vision with Reality for maximum privacy

### Performance Tips

- Enable `vision: true` for TLS-in-TLS scenarios
- Use `tcp_settings.no_delay: true` for low latency
- Keep the TLS buffer size at or above the 16 KiB minimum

### Linux Splice Relay

On Linux, the TCP relay copies payload through the kernel for eligible
directions. The option is automatic and has no configuration key.

- A direction qualifies only when both ends are raw TCP sockets.
- Vision qualifies only after its read or write side switches to direct mode.
  The two sides switch independently.
- Padded Vision, ordinary TLS, H2MUX, XUDP, and rate-limited fallback traffic
  keep the ordinary copy path.
- Buffered bytes, crypto tails, and flush barriers drain before the switch.
- If the kernel or the sandbox denies `splice`, the relay keeps ordinary
  copying, and no payload is lost or reordered.
- The relay reads the local socket buffers once per direction. If either side
  reports less than 32 KiB for send or receive, that direction keeps ordinary
  copying and allocates no pipe. The check uses the kernel-reported sizes only:
  it cannot see a later socket change or a remote receive window.
- Other platforms always use ordinary copying.

### Common Issues

- **"Address already in use"**: Change port or stop conflicting service
- **"Permission denied"**: Ports < 1024 require root/admin
- **Reality connection fails**: Verify keys match, UUID matches, SNI matches server's reality_targets key
- **Vision not working**: Ensure inner protocol is VLESS
- **Config validation fails**: Run with `--dry-run` for detailed errors
