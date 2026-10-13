//! Factory functions for creating TCP client handlers from config.

use std::sync::Arc;

use log::debug;

use crate::client_proxy_selector::{ClientProxySelector, ConnectAction, ConnectRule};
use crate::config::{ClientProxyConfig, RuleActionConfig, RuleConfig, TlsClientConfig};
use crate::h2mux::H2MuxClientHandler;
use crate::rustls_config_util::create_client_config;
use crate::tcp::chain_builder::build_client_chain_group;
use crate::tcp::tcp_handler::TcpClientHandler;
use crate::tls_client_handler::TlsClientHandler;
use crate::uuid_util::parse_uuid;
use crate::vless::vless_client_handler::VlessTcpClientHandler;

pub fn create_tcp_client_handler(
    client_proxy_config: ClientProxyConfig,
    default_sni_hostname: Option<String>,
) -> Box<dyn TcpClientHandler> {
    match client_proxy_config {
        ClientProxyConfig::Direct => {
            panic!("Tried to create a direct tcp client handler");
        }
        ClientProxyConfig::Vless {
            user_id,
            udp_enabled,
            h2mux,
        } => {
            let handler: Box<dyn TcpClientHandler> =
                Box::new(VlessTcpClientHandler::new(&user_id, udp_enabled));
            if let Some(h2mux_config) = h2mux {
                Box::new(H2MuxClientHandler::new(
                    Arc::from(handler),
                    h2mux_config.to_options(),
                ))
            } else {
                handler
            }
        }
        ClientProxyConfig::Tls(tls_client_config) => {
            let TlsClientConfig {
                verify,
                server_fingerprints,
                sni_hostname,
                alpn_protocols,
                tls_buffer_size,
                protocol,
                key,
                cert,
                vision,
            } = tls_client_config;

            let sni_hostname = if sni_hostname.is_unspecified() {
                if let Some(ref hostname) = default_sni_hostname {
                    debug!(
                        "Using default sni hostname for TLS client connection: {}",
                        hostname
                    );
                }
                default_sni_hostname
            } else {
                sni_hostname.into_option()
            };

            let key_and_cert_bytes = key.zip(cert).map(|(key, cert)| {
                // Certificates are already embedded as PEM data during config validation
                let cert_bytes = cert.as_bytes().to_vec();
                let key_bytes = key.as_bytes().to_vec();

                (key_bytes, cert_bytes)
            });

            let client_config = Arc::new(create_client_config(
                verify,
                server_fingerprints.into_vec(),
                alpn_protocols.into_vec(),
                sni_hostname.is_some(),
                key_and_cert_bytes,
                false, // tls13_only
            ));

            let server_name = match sni_hostname {
                Some(s) => rustls::pki_types::ServerName::try_from(s).unwrap(),
                // This is unused, since enable_sni is false, but connect_with still requires a
                // parameter.
                None => "example.com".try_into().unwrap(),
            };

            if vision {
                let ClientProxyConfig::Vless {
                    user_id,
                    udp_enabled,
                    h2mux: _, // h2mux not supported with vision
                } = protocol.as_ref()
                else {
                    // Validated when loading config
                    unreachable!();
                };
                let user_id_bytes = parse_uuid(user_id)
                    .expect("Invalid user_id UUID")
                    .into_boxed_slice();
                Box::new(TlsClientHandler::new_vision_vless(
                    client_config,
                    tls_buffer_size,
                    server_name,
                    user_id_bytes,
                    *udp_enabled,
                ))
            } else {
                let handler = create_tcp_client_handler(*protocol, None);

                Box::new(TlsClientHandler::new(
                    client_config,
                    tls_buffer_size,
                    server_name,
                    handler,
                ))
            }
        }
        ClientProxyConfig::Reality {
            public_key,
            short_id,
            sni_hostname,
            cipher_suites,
            vision,
            protocol,
        } => {
            // Decode public key from base64url
            let public_key_bytes =
                crate::reality::decode_public_key(&public_key).expect("Invalid REALITY public key");

            // Decode short ID from hex string
            let short_id_bytes =
                crate::reality::decode_short_id(&short_id).expect("Invalid REALITY short_id");

            // Determine SNI hostname
            let sni_hostname = sni_hostname.or(default_sni_hostname.clone());
            let server_name = match sni_hostname {
                Some(s) => rustls::pki_types::ServerName::try_from(s)
                    .unwrap()
                    .to_owned(),
                None => {
                    panic!("REALITY client requires sni_hostname to be specified");
                }
            };

            let cipher_suites = cipher_suites.into_vec();

            if vision {
                let ClientProxyConfig::Vless {
                    user_id,
                    udp_enabled,
                    h2mux: _, // h2mux not supported with vision
                } = protocol.as_ref()
                else {
                    unreachable!("Vision requires VLESS (should be validated during config load)")
                };
                let user_id_bytes = parse_uuid(user_id)
                    .expect("Invalid user_id UUID")
                    .into_boxed_slice();
                Box::new(
                    crate::reality_client_handler::RealityClientHandler::new_vision_vless(
                        public_key_bytes,
                        short_id_bytes,
                        server_name,
                        cipher_suites,
                        user_id_bytes,
                        *udp_enabled,
                    ),
                )
            } else {
                let inner_handler = create_tcp_client_handler(*protocol, None);
                Box::new(crate::reality_client_handler::RealityClientHandler::new(
                    public_key_bytes,
                    short_id_bytes,
                    server_name,
                    cipher_suites,
                    inner_handler,
                ))
            }
        }
    }
}

pub fn create_tcp_client_proxy_selector(rules: Vec<RuleConfig>) -> ClientProxySelector {
    let rules = rules
        .into_iter()
        .map(|rule_config| {
            let RuleConfig { masks, action } = rule_config;
            let connect_action = match action {
                RuleActionConfig::Allow {
                    override_address,
                    client_chains,
                } => {
                    let chain_group = build_client_chain_group(client_chains);
                    ConnectAction::new_allow(override_address, chain_group)
                }
                RuleActionConfig::Block => ConnectAction::new_block(),
            };
            ConnectRule::new(masks.into_vec(), connect_action)
        })
        .collect::<Vec<_>>();
    ClientProxySelector::new(rules)
}
