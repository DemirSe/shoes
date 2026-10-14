//! Factory functions for creating TCP server handlers from config.

use std::sync::Arc;

use rustc_hash::FxHashMap;

use crate::client_proxy_selector::ClientProxySelector;
use crate::config::{ClientChainHop, ClientConfig};
use crate::config::{
    ConfigSelection, RealityServerConfig, RuleConfig, ServerProxyConfig, TlsServerConfig,
};
use crate::option_util::{NoneOrSome, OneOrSome};
use crate::reality::RealityServerTarget;
use crate::resolver::Resolver;
use crate::rustls_config_util::create_server_config;
use crate::tcp::chain_builder::build_client_proxy_chain;
use crate::tcp::tcp_handler::TcpServerHandler;
use crate::tls_server_handler::{
    InnerProtocol, TlsServerHandler, TlsServerTarget, VisionVlessConfig,
};
use crate::vless::vless_server_handler::{VlessTcpServerHandler, VlessUsers};

use super::tcp_client_handler_factory::create_tcp_client_proxy_selector;

/// Create a TCP server handler from config.
///
/// # Arguments
/// * `server_proxy_config` - The protocol configuration
/// * `client_proxy_selector` - Selector for outbound proxy routing
/// * `resolver` - DNS resolver
pub fn create_tcp_server_handler(
    server_proxy_config: ServerProxyConfig,
    client_proxy_selector: &Arc<ClientProxySelector>,
    resolver: &Arc<dyn Resolver>,
) -> Box<dyn TcpServerHandler> {
    match server_proxy_config {
        ServerProxyConfig::Vless {
            user_id,
            udp_enabled,
            fallback,
        } => Box::new(VlessTcpServerHandler::new_users(
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
                        create_tls_server_target(config, client_proxy_selector, resolver),
                    )
                })
                .collect::<FxHashMap<String, TlsServerTarget>>();
            let default_tls_target = default_tls_target
                .map(|config| create_tls_server_target(*config, client_proxy_selector, resolver));
            let reality_server_targets = reality_targets
                .into_iter()
                .map(|(sni, config)| {
                    (
                        sni,
                        create_reality_server_target(config, client_proxy_selector, resolver),
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

fn create_target_selector(
    override_rules: NoneOrSome<ConfigSelection<RuleConfig>>,
    client_proxy_selector: &Arc<ClientProxySelector>,
) -> Arc<ClientProxySelector> {
    // Compute effective selector: if override_rules exist, create new selector; otherwise use parent's
    if !override_rules.is_empty() {
        let rules = override_rules
            .map(ConfigSelection::unwrap_config)
            .into_vec();
        Arc::new(create_tcp_client_proxy_selector(rules))
    } else {
        client_proxy_selector.clone()
    }
}

fn create_target_inner_protocol(
    protocol: ServerProxyConfig,
    vision: bool,
    effective_selector: &Arc<ClientProxySelector>,
    resolver: &Arc<dyn Resolver>,
) -> InnerProtocol {
    if vision {
        // Vision requires VLESS protocol (validated in config/mod.rs)
        if let ServerProxyConfig::Vless {
            user_id,
            udp_enabled,
            fallback,
        } = &protocol
        {
            let user_id_bytes = VlessUsers::new(user_id);
            InnerProtocol::VisionVless(VisionVlessConfig {
                user_id: user_id_bytes,
                udp_enabled: *udp_enabled,
                fallback: fallback.clone(),
            })
        } else {
            unreachable!("Vision requires VLESS (should be validated during config load)")
        }
    } else {
        let handler = create_tcp_server_handler(protocol, effective_selector, resolver);
        InnerProtocol::Normal(handler)
    }
}

fn create_tls_server_target(
    tls_server_config: TlsServerConfig,
    client_proxy_selector: &Arc<ClientProxySelector>,
    resolver: &Arc<dyn Resolver>,
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

    let effective_selector = create_target_selector(override_rules, client_proxy_selector);
    let inner_protocol =
        create_target_inner_protocol(protocol, vision, &effective_selector, resolver);

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

    let effective_selector = create_target_selector(override_rules, client_proxy_selector);
    let inner_protocol =
        create_target_inner_protocol(protocol, vision, &effective_selector, resolver);

    // Build dest client chain: if specified use it, otherwise default to direct
    let dest_client_chain = {
        let hops = dest_client_chain.into_vec();
        if hops.is_empty() {
            // Default to direct connection
            build_client_proxy_chain(OneOrSome::One(ClientChainHop::Single(
                ConfigSelection::Config(ClientConfig::default()),
            )))
        } else if hops.len() == 1 {
            build_client_proxy_chain(OneOrSome::One(hops.into_iter().next().unwrap()))
        } else {
            build_client_proxy_chain(OneOrSome::Some(hops))
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::address::{NetLocation, NetLocationMask};
    use crate::client_proxy_selector::ConnectDecision;
    use crate::config::{RuleActionConfig, RuleConfig};
    use crate::option_util::NoneOrSome;
    use crate::tcp::tcp_handler::TcpServerSetupResult;
    use tokio::io::AsyncWriteExt;

    const USERS: [&str; 2] = [
        "550e8400-e29b-41d4-a716-446655440000",
        "550e8400-e29b-41d4-a716-446655440001",
    ];

    #[derive(Debug)]
    struct NoNetworkResolver;

    impl Resolver for NoNetworkResolver {
        fn resolve_location(
            &self,
            _: &NetLocation,
        ) -> std::pin::Pin<
            Box<
                dyn std::future::Future<Output = std::io::Result<Vec<std::net::SocketAddr>>> + Send,
            >,
        > {
            panic!("factory tests must not resolve or connect to the cover")
        }
    }

    fn targets(
        protocol: ServerProxyConfig,
        vision: bool,
        rules: NoneOrSome<ConfigSelection<RuleConfig>>,
        parent: &Arc<ClientProxySelector>,
        resolver: &Arc<dyn Resolver>,
    ) -> [TlsServerTarget; 2] {
        let cert = rcgen::generate_simple_self_signed(vec!["whatsapp.net".into()]).unwrap();
        let tls = create_tls_server_target(
            TlsServerConfig {
                cert: cert.cert.pem(),
                key: cert.signing_key.serialize_pem(),
                alpn_protocols: NoneOrSome::One("h2".into()),
                client_ca_certs: NoneOrSome::None,
                client_fingerprints: NoneOrSome::None,
                vision,
                protocol: protocol.clone(),
                override_rules: rules.clone(),
            },
            parent,
            resolver,
        );
        let reality = create_reality_server_target(
            RealityServerConfig {
                limit_fallback_upload: None,
                limit_fallback_download: None,
                private_key: "AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA".into(),
                short_ids: OneOrSome::Some(vec!["".into(), "0102030405060708".into()]),
                dest: NetLocation::from_str("whatsapp.net:443", None).unwrap(),
                max_time_diff: Some(1234),
                min_client_version: Some([1, 2, 3]),
                max_client_version: Some([4, 5, 6]),
                cipher_suites: NoneOrSome::None,
                vision,
                protocol,
                dest_client_chain: NoneOrSome::None,
                override_rules: rules,
            },
            parent,
            resolver,
        );
        [tls, reality]
    }

    fn parts(target: &TlsServerTarget) -> (&Arc<ClientProxySelector>, &InnerProtocol) {
        match target {
            TlsServerTarget::Tls {
                server_config,
                effective_selector,
                inner_protocol,
            } => {
                assert_eq!(server_config.alpn_protocols, [b"h2".to_vec()]);
                (effective_selector, inner_protocol)
            }
            TlsServerTarget::Reality(target) => {
                assert_eq!(target.private_key, [0; 32]);
                assert_eq!(target.short_ids, [[0; 8], [1, 2, 3, 4, 5, 6, 7, 8]]);
                assert_eq!(target.dest.to_string(), "whatsapp.net:443");
                assert_eq!(target.max_time_diff, Some(1234));
                assert_eq!(target.min_client_version, Some([1, 2, 3]));
                assert_eq!(target.max_client_version, Some([4, 5, 6]));
                (&target.effective_selector, &target.inner_protocol)
            }
        }
    }

    #[tokio::test]
    async fn target_selectors_preserve_parent_override_order_and_normal_handler() {
        let resolver: Arc<dyn Resolver> = Arc::new(NoNetworkResolver);
        let parent = Arc::new(create_tcp_client_proxy_selector(
            vec![RuleConfig::default()],
        ));
        let block = RuleConfig {
            masks: OneOrSome::One(NetLocationMask::ANY),
            action: RuleActionConfig::Block,
        };
        for rules in [
            NoneOrSome::None,
            NoneOrSome::Some(vec![
                ConfigSelection::Config(block),
                ConfigSelection::Config(RuleConfig::default()),
            ]),
        ] {
            let overridden = !rules.is_empty();
            let targets = targets(
                ServerProxyConfig::Vless {
                    user_id: OneOrSome::Some(USERS.iter().map(|s| s.to_string()).collect()),
                    udp_enabled: true,
                    fallback: None,
                },
                false,
                rules,
                &parent,
                &resolver,
            );
            assert_eq!(
                Arc::ptr_eq(parts(&targets[0]).0, parts(&targets[1]).0),
                !overridden
            );
            for target in &targets {
                let (selector, inner) = parts(target);
                assert_eq!(Arc::ptr_eq(selector, &parent), !overridden);
                let decision = selector
                    .judge(
                        NetLocation::from_str("127.0.0.1:443", None).unwrap().into(),
                        &resolver,
                    )
                    .await
                    .unwrap();
                assert_eq!(matches!(decision, ConnectDecision::Block), overridden);
                let InnerProtocol::Normal(handler) = inner else {
                    panic!("expected normal VLESS handler");
                };
                for user in USERS {
                    let (io, mut peer) = tokio::io::duplex(128);
                    let mut request = vec![0];
                    request.extend(crate::uuid_util::parse_uuid(user).unwrap());
                    request.extend([0, 1, 0x01, 0xbb, 1, 127, 0, 0, 1]);
                    peer.write_all(&request).await.unwrap();
                    let TcpServerSetupResult::TcpForward { proxy_selector, .. } =
                        handler.setup_server_stream(Box::new(io)).await.unwrap()
                    else {
                        panic!("expected TCP setup");
                    };
                    assert!(Arc::ptr_eq(&proxy_selector, selector));
                }
            }
        }
    }

    #[test]
    fn target_vision_preserves_all_users_udp_and_existing_fallback() {
        let resolver: Arc<dyn Resolver> = Arc::new(NoNetworkResolver);
        let parent = Arc::new(ClientProxySelector::new(Vec::new()));
        for users in [
            OneOrSome::One(USERS[0].into()),
            OneOrSome::Some(USERS.iter().map(|s| s.to_string()).collect()),
        ] {
            for udp_enabled in [false, true] {
                for fallback in [
                    None,
                    Some(NetLocation::from_str("whatsapp.net:443", None).unwrap()),
                ] {
                    for target in targets(
                        ServerProxyConfig::Vless {
                            user_id: users.clone(),
                            udp_enabled,
                            fallback: fallback.clone(),
                        },
                        true,
                        NoneOrSome::None,
                        &parent,
                        &resolver,
                    ) {
                        let (selector, inner) = parts(&target);
                        assert!(Arc::ptr_eq(selector, &parent));
                        let InnerProtocol::VisionVless(config) = inner else {
                            panic!("expected Vision VLESS");
                        };
                        for user in users.iter() {
                            assert!(
                                config
                                    .user_id
                                    .contains(&crate::uuid_util::parse_uuid(user).unwrap())
                            );
                        }
                        assert!(!config.user_id.contains(&[9; 16]));
                        assert_eq!(config.udp_enabled, udp_enabled);
                        assert_eq!(config.fallback, fallback);
                    }
                }
            }
        }
    }

    fn cover_config(
        hops: NoneOrSome<ClientChainHop>,
        rules: NoneOrSome<ConfigSelection<RuleConfig>>,
    ) -> RealityServerConfig {
        RealityServerConfig {
            private_key: "AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA".into(),
            short_ids: OneOrSome::One("".into()),
            dest: NetLocation::from_str("whatsapp.net:443", None).unwrap(),
            protocol: ServerProxyConfig::Vless {
                user_id: OneOrSome::One(USERS[0].into()),
                udp_enabled: true,
                fallback: None,
            },
            dest_client_chain: hops,
            override_rules: rules,
            limit_fallback_upload: None,
            limit_fallback_download: None,
            max_time_diff: None,
            min_client_version: None,
            max_client_version: None,
            cipher_suites: NoneOrSome::None,
            vision: false,
        }
    }

    #[tokio::test]
    async fn e04_cover_factory_zero_one_many_pools_order_udp_and_override_independence() {
        use crate::client_proxy_chain::resolver_tests::{assert_hops, hostname, vless_config};
        let resolver: Arc<dyn Resolver> = Arc::new(NoNetworkResolver);
        let parent = Arc::new(create_tcp_client_proxy_selector(
            vec![RuleConfig::default()],
        ));
        let block = RuleConfig {
            masks: OneOrSome::One(NetLocationMask::ANY),
            action: RuleActionConfig::Block,
        };
        let hop = |port, udp| {
            ClientChainHop::Single(ConfigSelection::Config(vless_config(hostname(port), udp)))
        };
        for (input, expected) in [
            (NoneOrSome::None, vec![vec![(None, true)]]),
            (NoneOrSome::Some(Vec::new()), vec![vec![(None, true)]]),
            (
                NoneOrSome::One(hop(10001, false)),
                vec![vec![(Some(hostname(10001)), false)]],
            ),
            (
                NoneOrSome::Some(vec![hop(10002, true)]),
                vec![vec![(Some(hostname(10002)), true)]],
            ),
            (
                NoneOrSome::Some(vec![
                    hop(10003, true),
                    ClientChainHop::Pool(OneOrSome::Some(vec![
                        ConfigSelection::Config(vless_config(hostname(10004), false)),
                        ConfigSelection::Config(vless_config(hostname(10005), true)),
                    ])),
                    hop(10006, false),
                ]),
                vec![
                    vec![(Some(hostname(10003)), true)],
                    vec![
                        (Some(hostname(10004)), false),
                        (Some(hostname(10005)), true),
                    ],
                    vec![(Some(hostname(10006)), false)],
                ],
            ),
            (
                NoneOrSome::Some(vec![
                    ClientChainHop::Single(ConfigSelection::Config(ClientConfig::default())),
                    hop(10007, false),
                    hop(10008, true),
                ]),
                vec![
                    vec![(None, true)],
                    vec![(Some(hostname(10007)), false)],
                    vec![(Some(hostname(10008)), true)],
                ],
            ),
        ] {
            for rules in [
                NoneOrSome::None,
                NoneOrSome::One(ConfigSelection::Config(block.clone())),
            ] {
                let overridden = !rules.is_empty();
                let config = cover_config(input.clone(), rules);
                let TlsServerTarget::Reality(target) =
                    create_reality_server_target(config, &parent, &resolver)
                else {
                    unreachable!()
                };
                assert_eq!(target.dest, hostname(443));
                assert_hops(&target.dest_client_chain, &expected);
                assert_eq!(
                    Arc::ptr_eq(&target.effective_selector, &parent),
                    !overridden
                );
                let decision = target
                    .effective_selector
                    .judge(hostname(443).into(), &resolver)
                    .await
                    .unwrap();
                assert_eq!(matches!(decision, ConnectDecision::Block), overridden);
            }
        }
    }

    #[test]
    fn e04_cover_factory_transport_wrappers_keep_whatsapp_sni_without_io() {
        use crate::client_proxy_chain::resolver_tests::{
            assert_first_proxy_sni, assert_hops, hostname,
        };
        use crate::config::ClientProxyConfig;
        let resolver: Arc<dyn Resolver> = Arc::new(NoNetworkResolver);
        let parent = Arc::new(ClientProxySelector::new(Vec::new()));
        for kind in ["tls", "reality"] {
            for explicit_sni in [false, true] {
                // Parse ordinary client configs so the real recursive factory runs.
                let sni = if explicit_sni {
                    "sni_hostname: whatsapp.net"
                } else {
                    ""
                };
                let security = if kind == "tls" {
                    "verify: false"
                } else {
                    "public_key: AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA\n  short_id: ''"
                };
                let yaml = format!(
                    "address: whatsapp.net:443\nprotocol:\n  type: {kind}\n  {security}\n  {sni}\n  protocol:\n    type: vless\n    user_id: {}\n    udp_enabled: true\n",
                    USERS[0]
                );
                let client: ClientConfig = serde_yaml::from_str(&yaml).unwrap();
                assert!(matches!(
                    client.protocol,
                    ClientProxyConfig::Tls(_) | ClientProxyConfig::Reality { .. }
                ));
                let config = cover_config(
                    NoneOrSome::One(ClientChainHop::Single(ConfigSelection::Config(
                        client.clone(),
                    ))),
                    NoneOrSome::None,
                );
                let TlsServerTarget::Reality(target) =
                    create_reality_server_target(config, &parent, &resolver)
                else {
                    unreachable!()
                };
                assert_hops(
                    &target.dest_client_chain,
                    &[vec![(Some(hostname(443)), true)]],
                );
                assert_first_proxy_sni(&target.dest_client_chain);
                assert_eq!(target.dest, hostname(443));
            }
        }
    }
}
