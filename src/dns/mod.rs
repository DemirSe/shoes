//! DNS resolver module with configurable DNS servers.
//!
//! Supports:
//! - System resolver (NativeResolver)
//! - UDP DNS
//! - TCP DNS
//! - DNS-over-TLS (DoT)
//! - DNS-over-HTTPS (DoH), including HTTP/3
//!
//! The `hickory-resolver` dependency enables `tls-aws-lc-rs`, `https-aws-lc-rs`,
//! and `h3-aws-lc-rs` in Cargo.toml. These are dependency features enabled in
//! this build, not shoes crate features to select with `--features`.
//!
//! TCP-based protocols (tcp://, tls://, https://) support routing through
//! proxy chains via the ProxyRuntimeProvider.

mod builder;
mod composite_resolver;
mod hickory_resolver;
mod parsed;
mod proxy_runtime;

pub use builder::build_dns_registry;
pub use parsed::{IpStrategy, ParsedDnsUrl};
