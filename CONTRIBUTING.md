# Contributing

This is a Linux-first server fork of `shoes` 0.3.2. It keeps VLESS, REALITY,
Vision, TCP, UDP, and XUDP; stock Xray/Hiddify client compatibility is required.
TUN/FFI code is retained. Removed protocols (VMess, Trojan, Shadowsocks/ShadowTLS,
HTTP/SOCKS) stay removed. See [decisions](docs/kararlar.md) and
[the owner record](SERVER-FEATURE-DECISIONS.md).

Document the evidence and limits of performance or security claims. Do not
infer speed gains or full protection from short functional tests.

## Toolchain

Observed local verification environment (2026-10-15): Rust/Cargo 1.99.0,
`x86_64-unknown-linux-gnu`, sing-box 1.14.2. These are tested versions, not an
established minimum supported Rust version or a cross-platform compatibility matrix.
The project does not currently pin a toolchain file; record actual versions for each release.

Use a pre-provisioned Rust toolchain and dependency cache via command-local
environment variables. Do not modify shell profiles. Adjust these paths to your
local installation:

```bash
CARGO_HOME="$HOME/.local/share/shoes-toolchain/cargo" \
RUSTUP_HOME="$HOME/.local/share/shoes-toolchain/rustup" \
PATH="$HOME/.local/share/shoes-toolchain/cargo/bin:$PATH" \
CARGO_TARGET_DIR="$HOME/.cache/shoes-target" \
cargo test --workspace --locked --offline --lib --bins
```

Provision the native compiler/linker tools required by the locked dependencies
as well (C compiler, linker, make and CMake as required by the cached builds).
The application is Rust, but AWS-LC and jemalloc contain native/C code.
Missing tools, Rust targets or caches are a preparation blocker: do not solve
them by downloading during verification. Provisioning is a separate approved task.

Always pass `--locked --offline`. Never download crates or test peers.
Point `CARGO_TARGET_DIR` outside the repository. `/tmp` is often a small
tmpfs that large builds can exhaust; prefer a directory under your home
directory or cache.

## Commands

| Purpose | Command |
|---|---|
| Unit tests | `cargo test --workspace --locked --offline --lib --bins` |
| Integration tests | `cargo test -p shoes --locked --offline --features integration-tests --test integration -- --test-threads=1` |
| FFI tests | `cargo test -p shoes --locked --offline --lib --features ffi -- --test-threads=1` |
| Format check | `cargo fmt --all -- --check` |
| Lint | `cargo clippy --workspace --locked --offline --all-targets --all-features -- -D warnings` |
| Release build | `cargo build -p shoes --bin shoes --release --locked --offline` |

Apply the command-local environment above to each Cargo build/test/lint command.
With that target directory, the release binary is `$HOME/.cache/shoes-target/release/shoes`.

Notes:

- Some integration tests need a real sing-box peer via `SHOES_TEST_SING_BOX_BIN`.
  Without it they fail; they do not skip.
  For peer-independent lifecycle tests, insert `server_lifecycle::` after
  `--test integration` in the integration command above.
  Set the peer path command-locally, for example
  `SHOES_TEST_SING_BOX_BIN=/path/to/preinstalled/sing-box cargo test ...`.
  Do not download the peer. Capture fixture output privately; it can contain synthetic configurations.
- Never pass `--all-features` to `cargo test`. It also enables
  `external-network-tests`, `privileged-tests`, and `stress-tests`, which need
  external services, root/TUN, or benchmarks and are outside the offline set.
- CI is offline and manual (`workflow_dispatch`) on pre-provisioned self-hosted
  runners. Jobs must deny non-loopback test/build traffic and must never contact
  the production server. Runner control traffic is separately managed. See
  `.github/OFFLINE-CI.md`.

## Change and release checklist

1. Read the current [architecture](docs/mimari.md), [configuration reference](CONFIG.md)
   and [scope decisions](docs/kararlar.md) for the affected area.
2. Keep protocol behavior and production policy unchanged unless separately approved.
   Add regression coverage, then update the authoritative document rather than copying its contents elsewhere.
3. Run unit, FFI, integration, format and Clippy checks above for Rust changes.
4. Check documentation links, changed examples, and `git diff --check`.
   A plain docs edit does not require rerunning unrelated throughput benchmarks.
5. Update [CHANGELOG.md](CHANGELOG.md) under `Unreleased (fork)` for user-visible changes.
6. Record source commit, any local-diff identity, actual tool/peer versions, target triple,
   check results and binary SHA-256 in the private release handoff. A commit alone cannot identify a dirty build.
7. Follow the separately approved [deployment runbook](docs/operasyon.md).
   Building, testing or approving a release is not deployment authorization.

Deployment is manual and targets the existing operator's Linux/systemd server
layout, not a fresh server. Owner decision 2026-10-16: the local deployment
script and its tests were removed as unnecessary maintenance burden. Follow
[docs/operasyon.md](docs/operasyon.md) for hash verification, staging and
`rollback.bin`, atomic install, restart, bounded health observation and manual
rollback. Live steps always need separate owner approval.

## Documentation ownership

- [README.md](README.md): project entry point and CLI usage.
- [CONFIG.md](CONFIG.md): YAML fields and defaults; `docs/konfigurasyon.md` only summarizes the production policy.
- [CONTRIBUTING.md](CONTRIBUTING.md): toolchain, verification commands and contributor/release checklist.
- [docs/operasyon.md](docs/operasyon.md): deployment, health, rollback and retention procedure.
- [docs/kararlar.md](docs/kararlar.md): current scope; the owner record preserves decisions.
- `report.md`, dated performance sections and old changelog entries: history, not current live-state evidence.

Use `implemented locally`, `verified locally`, and `deployed` as distinct statuses.
Live status requires a dated approved deployment receipt; never infer it from a source change.
Keep the existing language split: English entry/reference docs, Turkish operational docs.

## Scope rules

- Make the smallest complete change. Preserve unrelated work.
- Adding dependencies or editing `Cargo.lock` needs owner approval.
- Deployment, live maintenance, reboot, tuning, staging, committing, or pushing
  all need owner approval. Do not perform them unasked.
- Never include credentials, raw configurations, or packet payloads in issues,
  reports, or public documents.
- Keep logs diagnostic: log phases, error kinds and lengths rather than UUIDs,
  key material, authentication tags or raw packet bytes.
