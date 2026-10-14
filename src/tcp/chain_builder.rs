//! Builder functions for creating ClientProxyChain from config.

use crate::client_proxy_chain::{ClientChainGroup, ClientProxyChain, InitialHopEntry};
use crate::config::ConfigSelection;
use crate::config::{ClientChainHop, ClientConfig};
use crate::tcp::proxy_connector::ProxyConnector;
use crate::tcp::proxy_connector_impl::ProxyConnectorImpl;
use crate::tcp::socket_connector::SocketConnector;
use crate::tcp::socket_connector_impl::SocketConnectorImpl;

/// Build a ClientProxyChain from a client_chain configuration.
///
/// Creates InitialHopEntry (socket + optional proxy paired) from hop 0.
/// Creates ProxyConnectors for subsequent hops (1+).
/// `protocol: direct` at hop 0 creates InitialHopEntry::Direct.
pub fn build_client_proxy_chain(
    client_chain: crate::option_util::OneOrSome<ClientChainHop>,
) -> ClientProxyChain {
    let hops: Vec<Vec<ClientConfig>> = client_chain
        .into_vec()
        .into_iter()
        .map(|hop| match hop {
            ClientChainHop::Single(selection) => match selection {
                ConfigSelection::Config(config) => vec![config],
                ConfigSelection::GroupName(group_name) => {
                    // Config validation expands group references before this builder runs.
                    panic!(
                        "Group reference '{}' was not resolved during config validation.",
                        group_name
                    );
                }
            },
            ClientChainHop::Pool(selections) => selections
                .into_vec()
                .into_iter()
                .flat_map(|selection| match selection {
                    ConfigSelection::Config(config) => vec![config],
                    ConfigSelection::GroupName(group_name) => {
                        // Pool group references are also expanded during config validation.
                        panic!(
                            "Group reference '{}' was not resolved during config validation.",
                            group_name
                        );
                    }
                })
                .collect(),
        })
        .collect();

    if hops.is_empty() {
        // Deserialized OneOrSome rejects empty hop lists; direct callers must uphold this invariant.
        panic!("Client chain must have at least one hop");
    }

    // Build initial hop entries from hop 0.
    // Each entry pairs socket + optional proxy together to ensure atomic selection.
    let initial_hop: Vec<InitialHopEntry> = hops[0]
        .iter()
        .map(|config| {
            let socket =
                Box::new(SocketConnectorImpl::from_config(config)) as Box<dyn SocketConnector>;

            if config.protocol.is_direct() {
                // Direct: socket only, no proxy
                InitialHopEntry::Direct(socket)
            } else {
                // Proxy: socket + proxy paired
                let proxy = ProxyConnectorImpl::from_config(config.clone())
                    .map(|p| Box::new(p) as Box<dyn ProxyConnector>)
                    .expect("Failed to create ProxyConnector for non-direct config");
                InitialHopEntry::Proxy { socket, proxy }
            }
        })
        .collect();

    // Build proxy connectors for subsequent hops (1+)
    let subsequent_hops: Vec<Vec<Box<dyn ProxyConnector>>> = hops
        .into_iter()
        .skip(1) // Skip hop 0, already processed as initial_hop
        .enumerate()
        .map(|(hop_offset, hop_configs)| {
            let hop_index = hop_offset + 1; // Actual hop index for error messages
            hop_configs
                .into_iter()
                .map(|config| {
                    // Subsequent hops MUST NOT have direct protocol
                    if config.protocol.is_direct() {
                        // validate_direct_connector_positions rejects direct configs at hop 1+.
                        panic!(
                            "protocol: direct is only valid at hop 0. Found direct at hop {} with address {}",
                            hop_index,
                            config.address
                        );
                    }

                    ProxyConnectorImpl::from_config(config)
                        .map(|p| Box::new(p) as Box<dyn ProxyConnector>)
                        .expect("Failed to create ProxyConnector for subsequent hop")
                })
                .collect()
        })
        .collect();

    ClientProxyChain::new(initial_hop, subsequent_hops)
}

/// Build a "direct" ClientChainGroup (no proxy, just socket connection).
/// Uses the same pattern as build_client_chain_group with no chains configured.
pub fn build_direct_chain_group() -> ClientChainGroup {
    build_client_chain_group(crate::option_util::NoneOrSome::None)
}

/// Build a ClientChainGroup from config chains.
pub fn build_client_chain_group(
    client_chains: crate::option_util::NoneOrSome<crate::config::ClientChain>,
) -> ClientChainGroup {
    let chains: Vec<ClientProxyChain> = if client_chains.is_empty() {
        vec![build_client_proxy_chain(
            crate::option_util::OneOrSome::One(ClientChainHop::Single(ConfigSelection::Config(
                ClientConfig::default(),
            ))),
        )]
    } else {
        client_chains
            .into_vec()
            .into_iter()
            .map(|chain| build_client_proxy_chain(chain.hops))
            .collect()
    };

    ClientChainGroup::new(chains)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::address::NetLocation;
    use crate::config::{ClientChain, ClientProxyConfig};
    use crate::option_util::{NoneOrSome, OneOrSome};
    use std::net::{IpAddr, Ipv4Addr};

    fn vless_config(port: u16) -> ClientConfig {
        ClientConfig {
            address: NetLocation::from_ip_addr(IpAddr::V4(Ipv4Addr::new(127, 0, 0, 1)), port),
            protocol: ClientProxyConfig::Vless {
                user_id: "550e8400-e29b-41d4-a716-446655440000".to_string(),
                udp_enabled: true,
                h2mux: None,
            },
            ..Default::default()
        }
    }

    fn direct_config() -> ClientConfig {
        ClientConfig::default()
    }

    #[test]
    fn test_build_single_direct_hop() {
        let chain = build_client_proxy_chain(OneOrSome::One(ClientChainHop::Single(
            ConfigSelection::Config(direct_config()),
        )));

        // Direct creates 1 socket connector, 0 proxy connectors
        assert_eq!(chain.num_hops(), 1);
        assert!(chain.supports_udp());
    }

    #[test]
    fn test_build_single_proxy_hop() {
        let chain = build_client_proxy_chain(OneOrSome::One(ClientChainHop::Single(
            ConfigSelection::Config(vless_config(1080)),
        )));

        // Single proxy creates 1 socket connector, 1 proxy connector
        assert_eq!(chain.num_hops(), 1);
    }

    #[test]
    fn test_build_direct_then_proxy_chain() {
        let chain = build_client_proxy_chain(OneOrSome::Some(vec![
            ClientChainHop::Single(ConfigSelection::Config(direct_config())),
            ClientChainHop::Single(ConfigSelection::Config(vless_config(1080))),
        ]));

        // direct (hop 0) -> vless (hop 1)
        // InitialHopEntry::Direct + 1 subsequent hop = 2 hops total
        assert_eq!(chain.num_hops(), 2);
    }

    #[test]
    fn test_build_two_proxy_hops() {
        let chain = build_client_proxy_chain(OneOrSome::Some(vec![
            ClientChainHop::Single(ConfigSelection::Config(vless_config(1080))),
            ClientChainHop::Single(ConfigSelection::Config(vless_config(1081))),
        ]));

        // vless1 (hop 0) -> vless2 (hop 1)
        assert_eq!(chain.num_hops(), 2);
    }

    #[test]
    fn test_build_pool_at_hop0() {
        let chain =
            build_client_proxy_chain(OneOrSome::One(ClientChainHop::Pool(OneOrSome::Some(vec![
                ConfigSelection::Config(vless_config(1080)),
                ConfigSelection::Config(vless_config(1081)),
            ]))));

        // Pool of 2 proxies at hop 0
        assert_eq!(chain.num_hops(), 1);
    }

    #[test]
    fn test_build_empty_client_chains_creates_default() {
        let group = build_client_chain_group(NoneOrSome::None);
        // Default is a single direct chain
        assert!(group.supports_udp());
    }

    #[test]
    fn test_build_client_chain_group_with_chains() {
        let chains = NoneOrSome::Some(vec![
            ClientChain {
                hops: OneOrSome::One(ClientChainHop::Single(ConfigSelection::Config(
                    vless_config(1080),
                ))),
            },
            ClientChain {
                hops: OneOrSome::One(ClientChainHop::Single(ConfigSelection::Config(
                    direct_config(),
                ))),
            },
        ]);
        let group = build_client_chain_group(chains);
        // 2 chains in group
        assert!(group.supports_udp()); // direct chain supports UDP
    }

    #[test]
    #[should_panic(expected = "protocol: direct is only valid at hop 0")]
    fn test_direct_at_hop1_panics() {
        build_client_proxy_chain(OneOrSome::Some(vec![
            ClientChainHop::Single(ConfigSelection::Config(vless_config(1080))),
            ClientChainHop::Single(ConfigSelection::Config(direct_config())),
        ]));
    }

    #[test]
    #[should_panic(expected = "protocol: direct is only valid at hop 0")]
    fn test_direct_in_pool_at_hop1_panics() {
        build_client_proxy_chain(OneOrSome::Some(vec![
            ClientChainHop::Single(ConfigSelection::Config(vless_config(1080))),
            ClientChainHop::Pool(OneOrSome::Some(vec![
                ConfigSelection::Config(vless_config(1081)),
                ConfigSelection::Config(direct_config()),
            ])),
        ]));
    }

    #[test]
    #[should_panic(expected = "was not resolved during config validation")]
    fn test_unresolved_group_reference_panics() {
        build_client_proxy_chain(OneOrSome::One(ClientChainHop::Single(
            ConfigSelection::GroupName("unresolved_group".to_string()),
        )));
    }
}
