/// Integration tests for cross-protocol UDP support.
///
/// These tests verify UDP bridging between the retained transports:
/// - VLESS XUDP server (session-based inbound) -> VLESS H2MUX client -> VLESS server
/// - VLESS H2MUX server (inbound) -> VLESS XUDP client -> sing-box VLESS server
use shoes_test_support as common;

use common::port_helper::PortHelper;
use common::socks5::Socks5UdpAssociation;
use common::test_fixture::{start_shoes_server, start_singbox_server as start_singbox};
use common::test_servers::start_udp_echo_server;

use std::net::SocketAddr;
use std::time::Duration;
use tokio::time::sleep;

const VLESS_UUID: &str = "b85798ef-e9dc-46a4-9a87-8da4499d36d0";

/// Test: VLESS XUDP server (shoes) -> VLESS H2MUX client (shoes) -> VLESS server (shoes)
///
/// This tests a session-based XUDP inbound being bridged to the H2MUX
/// multiplexed client transport.
/// Architecture:
///   sing-box VLESS client (XUDP) -> shoes VLESS server -> shoes VLESS H2MUX client -> shoes VLESS server -> UDP echo
#[tokio::test]
async fn test_vless_server_to_h2mux_client_cross_protocol() -> Result<(), Box<dyn std::error::Error>>
{
    let mut port_helper = PortHelper::new();
    let (shoes_vless_ip, shoes_vless_port) = port_helper.get_listener_port();
    let (shoes_h2mux_ip, shoes_h2mux_port) = port_helper.get_listener_port();
    let (singbox_socks_ip, singbox_socks_port) = port_helper.get_listener_port();
    let (udp_echo_ip, udp_echo_port) = port_helper.get_port();

    eprintln!(
        "[TEST] Cross-protocol: VLESS(shoes)={} -> H2MUX client(shoes) -> VLESS server(shoes)={} -> echo={}",
        shoes_vless_port, shoes_h2mux_port, udp_echo_port
    );

    // Start UDP echo server
    let _echo_server = start_udp_echo_server(&udp_echo_ip, udp_echo_port).await?;

    // Start shoes with a VLESS server that chains UDP to a VLESS H2MUX client,
    // plus the final VLESS server that terminates the H2MUX session.
    let shoes_config = format!(
        r#"
- address: "{}:{}"
  protocol:
    type: vless
    user_id: "{}"
  rules:
    - mask: 0.0.0.0/0
      action: allow
      client_proxy:
        address: "{}:{}"
        protocol:
          type: vless
          user_id: "{}"
          h2mux:
            max_connections: 1
            min_streams: 1

- address: "{}:{}"
  protocol:
    type: vless
    user_id: "{}"
"#,
        shoes_vless_ip,
        shoes_vless_port,
        VLESS_UUID,
        shoes_h2mux_ip,
        shoes_h2mux_port,
        VLESS_UUID,
        shoes_h2mux_ip,
        shoes_h2mux_port,
        VLESS_UUID
    );
    let (_shoes_guard, _shoes_cfg) = start_shoes_server(&shoes_config)?;

    // Start sing-box as VLESS CLIENT with SOCKS inbound (to test from)
    let singbox_client_config = format!(
        r#"{{
  "log": {{ "level": "debug" }},
  "inbounds": [{{
    "type": "socks",
    "tag": "socks-in",
    "listen": "{}",
    "listen_port": {}
  }}],
  "outbounds": [{{
    "type": "vless",
    "tag": "vless-out",
    "server": "{}",
    "server_port": {},
    "uuid": "{}",
    "packet_encoding": "xudp"
  }}],
  "route": {{ "final": "vless-out" }}
}}"#,
        singbox_socks_ip, singbox_socks_port, shoes_vless_ip, shoes_vless_port, VLESS_UUID
    );
    let (_singbox_client_guard, _singbox_client_cfg) = start_singbox(&singbox_client_config)?;

    // Wait for all servers
    port_helper.wait_for_all_ports().await?;
    sleep(Duration::from_millis(500)).await;

    // Test UDP through the cross-protocol chain
    let association = Socks5UdpAssociation::connect(&singbox_socks_ip, singbox_socks_port).await?;
    let target: SocketAddr = format!("{udp_echo_ip}:{udp_echo_port}").parse()?;

    let response = association
        .send_to(target, b"Cross-protocol VLESS->H2MUX test!")
        .await?;

    let response_str = std::str::from_utf8(&response)?;
    eprintln!("[TEST] Response: {:?}", response_str);
    assert!(response_str.contains("Cross-protocol VLESS->H2MUX test!"));
    assert!(response_str.ends_with(" [ECHO]"));

    eprintln!("[TEST] VLESS server -> H2MUX client cross-protocol test passed!");
    Ok(())
}

/// Test: VLESS H2MUX server (shoes) -> VLESS XUDP client (shoes) -> VLESS server (sing-box)
///
/// This tests an H2MUX inbound being bridged to a session-based XUDP client.
/// Architecture:
///   sing-box VLESS+h2mux client -> shoes VLESS server -> shoes VLESS client (XUDP) -> sing-box VLESS server -> UDP echo
#[tokio::test]
async fn test_h2mux_server_to_vless_client_cross_protocol() -> Result<(), Box<dyn std::error::Error>>
{
    let mut port_helper = PortHelper::new();
    let (shoes_vless_ip, shoes_vless_port) = port_helper.get_listener_port();
    let (singbox_vless_ip, singbox_vless_port) = port_helper.get_listener_port();
    let (singbox_socks_ip, singbox_socks_port) = port_helper.get_listener_port();
    let (udp_echo_ip, udp_echo_port) = port_helper.get_port();

    eprintln!(
        "[TEST] Cross-protocol: H2MUX(shoes)={} -> VLESS client(shoes) -> VLESS server(sing-box)={} -> echo={}",
        shoes_vless_port, singbox_vless_port, udp_echo_port
    );

    // Start UDP echo server
    let _echo_server = start_udp_echo_server(&udp_echo_ip, udp_echo_port).await?;

    // Start sing-box as VLESS SERVER (final destination for the VLESS client)
    let singbox_vless_config = format!(
        r#"{{
  "log": {{ "level": "debug" }},
  "inbounds": [{{
    "type": "vless",
    "tag": "vless-in",
    "listen": "{}",
    "listen_port": {},
    "users": [{{ "uuid": "{}" }}]
  }}],
  "outbounds": [{{ "type": "direct", "tag": "direct" }}],
  "route": {{ "final": "direct" }}
}}"#,
        singbox_vless_ip, singbox_vless_port, VLESS_UUID
    );
    let (_singbox_vless_guard, _singbox_vless_cfg) = start_singbox(&singbox_vless_config)?;

    // Start shoes with a VLESS server (H2MUX auto-detected) that chains UDP to a
    // VLESS XUDP client.
    let shoes_config = format!(
        r#"
- address: "{}:{}"
  protocol:
    type: vless
    user_id: "{}"
  rules:
    - mask: 0.0.0.0/0
      action: allow
      client_proxy:
        address: "{}:{}"
        protocol:
          type: vless
          user_id: "{}"
"#,
        shoes_vless_ip,
        shoes_vless_port,
        VLESS_UUID,
        singbox_vless_ip,
        singbox_vless_port,
        VLESS_UUID
    );
    let (_shoes_guard, _shoes_cfg) = start_shoes_server(&shoes_config)?;

    // Start sing-box as VLESS+h2mux CLIENT with SOCKS inbound (to test from)
    let singbox_client_config = format!(
        r#"{{
  "log": {{ "level": "debug" }},
  "inbounds": [{{
    "type": "socks",
    "tag": "socks-in",
    "listen": "{}",
    "listen_port": {}
  }}],
  "outbounds": [{{
    "type": "vless",
    "tag": "vless-out",
    "server": "{}",
    "server_port": {},
    "uuid": "{}",
    "multiplex": {{ "enabled": true, "protocol": "h2mux", "max_connections": 1, "min_streams": 1, "max_streams": 0 }}
  }}],
  "route": {{ "final": "vless-out" }}
}}"#,
        singbox_socks_ip, singbox_socks_port, shoes_vless_ip, shoes_vless_port, VLESS_UUID
    );
    let (_singbox_client_guard, _singbox_client_cfg) = start_singbox(&singbox_client_config)?;

    // Wait for all servers
    port_helper.wait_for_all_ports().await?;
    sleep(Duration::from_millis(500)).await;

    // Test UDP through the cross-protocol chain
    let association = Socks5UdpAssociation::connect(&singbox_socks_ip, singbox_socks_port).await?;
    let target: SocketAddr = format!("{udp_echo_ip}:{udp_echo_port}").parse()?;

    let response = association
        .send_to(target, b"Cross-protocol H2MUX->VLESS test!")
        .await?;

    let response_str = std::str::from_utf8(&response)?;
    eprintln!("[TEST] Response: {:?}", response_str);
    assert!(response_str.contains("Cross-protocol H2MUX->VLESS test!"));
    assert!(response_str.ends_with(" [ECHO]"));

    eprintln!("[TEST] H2MUX server -> VLESS client cross-protocol test passed!");
    Ok(())
}
