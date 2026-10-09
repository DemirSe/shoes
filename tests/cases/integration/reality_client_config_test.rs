//! Offline config regressions for generated retained client fixtures (REALITY and H2MUX).
//!
//! The fixture emits a Shoes VLESS front in front of the retained REALITY/Vision
//! client chain, with a stock sing-box HTTP entry added at build time. These tests
//! parse and validate the emitted Shoes profiles without starting any process or
//! opening any connection.

use shoes_test_support as common;

use common::test_fixture::{ProxyTestFixture, stock_vless_entry_config};
use shoes::config::{
    ClientChainHop, ClientProxyConfig, Config, ConfigSelection, RuleActionConfig,
    ServerProxyConfig, create_server_configs,
};

fn assert_generated_reality_client_profile(config: &str, expect_vision: bool) {
    assert!(
        !config.contains("type: http") && !config.contains("type: trojan"),
        "generated profile must not use retired protocol variants:\n{config}"
    );

    let parsed: Vec<Config> = serde_yaml::from_str(config).expect("generated profile must parse");
    let server = parsed
        .iter()
        .find_map(|config| match config {
            Config::Server(server) => Some(server),
            _ => None,
        })
        .expect("generated profile must contain a server config");
    assert!(
        matches!(server.protocol, ServerProxyConfig::Vless { .. }),
        "the front must be a retained VLESS server"
    );

    let selection = server.rules.iter().next().expect("front rule");
    let ConfigSelection::Config(rule) = selection else {
        panic!("expected an inline rule");
    };
    let RuleActionConfig::Allow { client_chains, .. } = &rule.action else {
        panic!("expected an allow rule");
    };
    let chain = client_chains.iter().next().expect("client chain");
    let ClientChainHop::Single(ConfigSelection::GroupName(group)) =
        chain.hops.iter().next().expect("chain hop")
    else {
        panic!("expected the front rule to reference the default client group");
    };
    assert_eq!(group, "default");

    let client_group = parsed
        .iter()
        .find_map(|config| match config {
            Config::ClientConfigGroup(group) if group.client_group == "default" => Some(group),
            _ => None,
        })
        .expect("generated profile must contain the default client group");
    let ConfigSelection::Config(client) = client_group
        .client_proxies
        .iter()
        .next()
        .expect("default client group must not be empty")
    else {
        panic!("expected an inline client in the default group");
    };
    let ClientProxyConfig::Reality {
        vision, protocol, ..
    } = &client.protocol
    else {
        panic!("expected a REALITY client");
    };
    assert_eq!(*vision, expect_vision);
    assert!(matches!(protocol.as_ref(), ClientProxyConfig::Vless { .. }));

    create_server_configs(parsed).expect("generated profile must validate");
}

#[test]
fn generated_shoes_reality_client_profile_parses() {
    let mut fixture = ProxyTestFixture::new()
        .with_shoes_reality_client()
        .with_singbox_reality_vless_server();
    let configs = fixture
        .preview_shoes_configs()
        .expect("generate shoes configs");
    assert_eq!(configs.len(), 1, "one shoes process for the client fixture");
    assert_generated_reality_client_profile(&configs[0], false);
}

#[test]
fn generated_shoes_reality_vision_client_profile_parses() {
    let mut fixture = ProxyTestFixture::new()
        .with_shoes_reality_vision_client()
        .with_singbox_reality_vision_server();
    let configs = fixture
        .preview_shoes_configs()
        .expect("generate shoes configs");
    assert_eq!(
        configs.len(),
        1,
        "one shoes process for the vision client fixture"
    );
    assert_generated_reality_client_profile(&configs[0], true);
}

#[test]
fn stock_vless_entry_config_matches_retained_front() {
    let config = stock_vless_entry_config("127.0.0.1", 18080, "127.0.0.1", 18081);
    let parsed: serde_json::Value = serde_json::from_str(&config).expect("valid JSON");
    assert_eq!(parsed["inbounds"][0]["type"], "http");
    assert_eq!(parsed["inbounds"][0]["listen_port"], 18080);
    assert_eq!(parsed["outbounds"][0]["type"], "vless");
    assert_eq!(parsed["outbounds"][0]["server"], "127.0.0.1");
    assert_eq!(parsed["outbounds"][0]["server_port"], 18081);
}

/// The restored H2MUX Shoes-client stress cases rely on the fixture emitting a
/// parseable retained profile: VLESS front plus a TLS+VLESS+h2mux client chain.
#[test]
fn generated_shoes_h2mux_client_profile_parses() {
    let mut fixture = ProxyTestFixture::new()
        .with_shoes_h2mux_vless_client()
        .with_shoes_vless_tls_server();
    let configs = fixture
        .preview_shoes_configs()
        .expect("generate shoes configs");
    assert_eq!(
        configs.len(),
        2,
        "client fixture plus the shoes upstream server"
    );
    let config = configs
        .iter()
        .find(|config| config.contains("client_group: default"))
        .expect("generated h2mux client profile");
    assert!(
        !config.contains("type: http"),
        "retired HTTP front must be gone:\n{config}"
    );

    let parsed: Vec<Config> = serde_yaml::from_str(config).expect("generated profile must parse");
    let server = parsed
        .iter()
        .find_map(|config| match config {
            Config::Server(server) => Some(server),
            _ => None,
        })
        .expect("generated profile must contain a server config");
    assert!(matches!(server.protocol, ServerProxyConfig::Vless { .. }));

    let client_group = parsed
        .iter()
        .find_map(|config| match config {
            Config::ClientConfigGroup(group) if group.client_group == "default" => Some(group),
            _ => None,
        })
        .expect("generated profile must contain the default client group");
    let ConfigSelection::Config(client) = client_group
        .client_proxies
        .iter()
        .next()
        .expect("default client group must not be empty")
    else {
        panic!("expected an inline client in the default group");
    };
    let ClientProxyConfig::Tls(tls) = &client.protocol else {
        panic!("expected a TLS client");
    };
    let ClientProxyConfig::Vless {
        h2mux: Some(h2mux), ..
    } = tls.protocol.as_ref()
    else {
        panic!("expected a VLESS client with H2MUX");
    };
    assert_eq!(h2mux.max_connections, Some(4));
    assert_eq!(h2mux.min_streams, Some(1));
}
