//! Configuration types for the proxy server.
//!
//! This module contains all the configuration types used by the proxy server,
//! organized into submodules by functionality:
//!
//! - [`common`]: Shared helpers and constants
//! - [`transport`]: Transport layer types (TCP, UDP)
//! - [`selection`]: ConfigSelection for referencing groups or inline configs
//! - [`server`]: Server-side protocol configurations
//! - [`client`]: Client-side protocol configurations
//! - [`rules`]: Rule configurations for traffic routing
//! - [`groups`]: Top-level configuration groups and the Config enum
//! - [`dns`]: DNS server configuration

pub mod client;
pub mod common;
pub mod dns;
pub mod global;
pub mod groups;
pub mod rules;
pub mod selection;
pub mod server;
pub mod transport;
pub mod tun;

// Re-export all public types for convenience
#[allow(unused_imports)]
pub use client::{ClientConfig, ClientProxyConfig, H2MuxConfig, TlsClientConfig};
pub use common::DEFAULT_REALITY_SHORT_ID;
pub use dns::{DnsConfig, DnsConfigGroup, DnsServerSpec, ExpandedDnsGroup, ExpandedDnsSpec};
pub use global::GlobalLimits;
pub use groups::{ClientConfigGroup, Config, NamedPem, PemSource};
pub use rules::{ClientChain, ClientChainHop, RuleActionConfig, RuleConfig};
pub use selection::ConfigSelection;
pub use server::{
    RealityServerConfig, ServerConfig, ServerProxyConfig, TlsServerConfig, direct_allow_rule,
};
pub use transport::{BindLocation, TcpConfig, Transport};
pub use tun::TunConfig;
