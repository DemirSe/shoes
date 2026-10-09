//! Factory functions for creating TCP server handlers from config.

use std::net::IpAddr;
use std::sync::Arc;

use rustc_hash::FxHashMap;

use crate::client_proxy_selector::ClientProxySelector;
use crate::config::{ClientChainHop, ClientConfig};
use crate::config::{ConfigSelection, RealityServerConfig, ServerProxyConfig, TlsServerConfig};
use crate::option_util::OneOrSome;
use crate::reality::RealityServerTarget;
use crate::resolver::Resolver;
use crate::rustls_config_util::create_server_config;
use crate::tcp::chain_builder::build_client_proxy_chain;
use crate::tcp::tcp_handler::TcpServerHandler;
use crate::tls_server_handler::{
    InnerProtocol, TlsServerHandler, TlsServerTarget, VisionVlessConfig,
};
use crate::uuid_util::parse_uuid;
use crate::vless::vless_server_handler::VlessTcpServerHandler;

use super::tcp_client_handler_factory::create_tcp_client_proxy_selector;

/// Create a TCP server handler from config.
///
/// # Arguments
/// * `server_proxy_config` - The protocol configuration
/// * `client_proxy_selector` - Selector for outbound proxy routing
/// * `resolver` - DNS resolver
/// * `bind_ip` - Optional bind IP forwarded to nested TLS/REALITY targets
pub fn create_tcp_server_handler(
    server_proxy_config: ServerProxyConfig,
    client_proxy_selector: &Arc<ClientProxySelector>,
    resolver: &Arc<dyn Resolver>,
    bind_ip: Option<IpAddr>,
) -> Box<dyn TcpServerHandler> {
    match server_proxy_config {
        ServerProxyConfig::Vless {
            user_id,
            udp_enabled,
            fallback,
        } => Box::new(VlessTcpServerHandler::new(
            &user_id,
            udp_enabled,
            client_proxy_selector.clone(),
            resolver.clone(),
            fallback,
        )),
        ServerProxyConfig::Tls {
            tls_targets,
            default_tls_target,
            reality_targets,
            tls_buffer_size,
        } => {
            let mut all_targets = tls_targets
                .into_iter()
                .map(|(sni, config)| {
                    (
                        sni,
                        create_tls_server_target(config, client_proxy_selector, resolver, bind_ip),
                    )
                })
                .collect::<FxHashMap<String, TlsServerTarget>>();
            let default_tls_target = default_tls_target.map(|config| {
                create_tls_server_target(*config, client_proxy_selector, resolver, bind_ip)
            });
            let reality_server_targets = reality_targets
                .into_iter()
                .map(|(sni, config)| {
                    (
                        sni,
                        create_reality_server_target(
                            config,
                            client_proxy_selector,
                            resolver,
                            bind_ip,
                        ),
                    )
                })
                .collect::<FxHashMap<String, TlsServerTarget>>();
            all_targets.extend(reality_server_targets);
            Box::new(TlsServerHandler::new(
                all_targets,
                default_tls_target,
                tls_buffer_size,
                resolver.clone(),
            ))
        }
    }
}

fn create_tls_server_target(
    tls_server_config: TlsServerConfig,
    client_proxy_selector: &Arc<ClientProxySelector>,
    resolver: &Arc<dyn Resolver>,
    bind_ip: Option<IpAddr>,
) -> TlsServerTarget {
    let TlsServerConfig {
        cert,
        key,
        alpn_protocols,
        client_ca_certs,
        client_fingerprints,
        vision,
        protocol,
        override_rules,
    } = tls_server_config;

    // Certificates are already embedded as PEM data during config validation
    let cert_bytes = cert.as_bytes().to_vec();
    let key_bytes = key.as_bytes().to_vec();

    let client_ca_certs = client_ca_certs
        .into_iter()
        .map(|cert| cert.as_bytes().to_vec())
        .collect();

    let server_config = Arc::new(create_server_config(
        &cert_bytes,
        &key_bytes,
        client_ca_certs,
        &alpn_protocols.into_vec(),
        &client_fingerprints.into_vec(),
    ));

    // Compute effective selector: if override_rules exist, create new selector; otherwise use parent's
    let effective_selector = if !override_rules.is_empty() {
        let rules = override_rules
            .map(ConfigSelection::unwrap_config)
            .into_vec();
        Arc::new(create_tcp_client_proxy_selector(rules, resolver.clone()))
    } else {
        client_proxy_selector.clone()
    };

    // Create inner_protocol based on protocol type
    let inner_protocol = if vision {
        // Vision requires VLESS protocol (validated in config/mod.rs)
        if let ServerProxyConfig::Vless {
            user_id,
            udp_enabled,
            fallback,
        } = &protocol
        {
            let user_id_bytes = parse_uuid(user_id)
                .expect("Invalid user_id UUID")
                .into_boxed_slice();
            InnerProtocol::VisionVless(VisionVlessConfig {
                user_id: user_id_bytes,
                udp_enabled: *udp_enabled,
                fallback: fallback.clone(),
            })
        } else {
            unreachable!("Vision requires VLESS (should be validated during config load)")
        }
    } else {
        let handler = create_tcp_server_handler(protocol, &effective_selector, resolver, bind_ip);
        InnerProtocol::Normal(handler)
    };

    TlsServerTarget::Tls {
        server_config,
        effective_selector,
        inner_protocol,
    }
}

fn create_reality_server_target(
    reality_server_config: RealityServerConfig,
    client_proxy_selector: &Arc<ClientProxySelector>,
    resolver: &Arc<dyn Resolver>,
    bind_ip: Option<IpAddr>,
) -> TlsServerTarget {
    let RealityServerConfig {
        limit_fallback_upload,
        limit_fallback_download,
        private_key,
        short_ids,
        dest,
        max_time_diff,
        min_client_version,
        max_client_version,
        cipher_suites,
        vision,
        protocol,
        dest_client_chain,
        override_rules,
    } = reality_server_config;

    // Decode private key from base64url (validated during config load)
    let private_key_bytes = crate::reality::decode_private_key(&private_key)
        .expect("Invalid REALITY private key (should be validated during config load)");

    // Decode short IDs from hex strings (validated during config load)
    // OneOrSome ensures at least one short_id is always present (default is all zeros)
    let short_id_bytes: Vec<[u8; 8]> = short_ids
        .into_vec()
        .into_iter()
        .map(|s| {
            crate::reality::decode_short_id(&s)
                .expect("Invalid REALITY short_id (should be validated during config load)")
        })
        .collect();

    // Compute effective selector: if override_rules exist, create new selector; otherwise use parent's
    let effective_selector = if !override_rules.is_empty() {
        let rules = override_rules
            .map(ConfigSelection::unwrap_config)
            .into_vec();
        Arc::new(create_tcp_client_proxy_selector(rules, resolver.clone()))
    } else {
        client_proxy_selector.clone()
    };

    // Create inner_protocol based on protocol type
    let inner_protocol = if vision {
        // Vision requires VLESS protocol (validated in config/mod.rs)
        if let ServerProxyConfig::Vless {
            user_id,
            udp_enabled,
            fallback,
        } = &protocol
        {
            let user_id_bytes = parse_uuid(user_id)
                .expect("Invalid user_id UUID")
                .into_boxed_slice();
            InnerProtocol::VisionVless(VisionVlessConfig {
                user_id: user_id_bytes,
                udp_enabled: *udp_enabled,
                fallback: fallback.clone(),
            })
        } else {
            unreachable!("Vision requires VLESS (should be validated during config load)")
        }
    } else {
        let handler = create_tcp_server_handler(protocol, &effective_selector, resolver, bind_ip);
        InnerProtocol::Normal(handler)
    };

    // Build dest client chain: if specified use it, otherwise default to direct
    let dest_client_chain = {
        let hops = dest_client_chain.into_vec();
        if hops.is_empty() {
            // Default to direct connection
            build_client_proxy_chain(
                OneOrSome::One(ClientChainHop::Single(ConfigSelection::Config(
                    ClientConfig::default(),
                ))),
                resolver.clone(),
            )
        } else if hops.len() == 1 {
            build_client_proxy_chain(
                OneOrSome::One(hops.into_iter().next().unwrap()),
                resolver.clone(),
            )
        } else {
            build_client_proxy_chain(OneOrSome::Some(hops), resolver.clone())
        }
    };

    TlsServerTarget::Reality(RealityServerTarget {
        limit_fallback_upload,
        limit_fallback_download,
        private_key: private_key_bytes,
        short_ids: short_id_bytes,
        dest,
        max_time_diff,
        min_client_version,
        max_client_version,
        cipher_suites: cipher_suites.into_vec(),
        effective_selector,
        inner_protocol,
        dest_client_chain,
    })
}
