//! Wire regressions for the UDP peer policy of the XUDP session path.
//!
//! The egress socket sends to one resolved peer. A datagram from any other
//! source is dropped instead of being presented to the client as the addressed
//! peer's reply, while a reply from the addressed peer is still delivered. Both
//! tests keep the stock topology: a TLS/Vision VLESS server reached through a
//! sing-box VLESS client, plus a direct XUDP client as extra coverage.

use std::net::SocketAddr;
use std::path::Path;
use std::time::Duration;

use shoes_test_support::certs::generate_test_cert_files;
use shoes_test_support::port_helper::PortHelper;
use shoes_test_support::test_fixture::{
    TEST_UUID, start_shoes_server, start_singbox_server as start_singbox,
};
use shoes_test_support::vless::{
    VlessDestination, VlessUdpClient, XUDP_OPTION_DATA, XUDP_STATUS_KEEP, XUDP_STATUS_NEW,
    XudpClient, XudpFrame, parse_uuid,
};
use tokio::net::{TcpStream, UdpSocket};
use tokio::time::{Instant, timeout, timeout_at};

const DROP_WINDOW: Duration = Duration::from_millis(500);
const REPLY_WINDOW: Duration = Duration::from_secs(5);
const SCENARIO_DEADLINE: Duration = Duration::from_secs(45);

/// The addressed peer answers the second request; the first request is answered
/// from a second loopback address that is not the peer.
async fn start_split_source_peer()
-> Result<(SocketAddr, tokio::task::JoinHandle<()>), Box<dyn std::error::Error>> {
    let peer = UdpSocket::bind("127.0.0.1:0").await?;
    let peer_address = peer.local_addr()?;
    let alternate = UdpSocket::bind("127.0.0.2:0").await?;
    assert_ne!(alternate.local_addr()?.ip(), peer_address.ip());

    let task = tokio::spawn(async move {
        let mut buffer = vec![0u8; 65_536];
        let (_, egress) = peer.recv_from(&mut buffer).await.unwrap();
        alternate.send_to(b"alternate reply", egress).await.unwrap();
        let (length, egress) = peer.recv_from(&mut buffer).await.unwrap();
        let mut reply = b"peer reply:".to_vec();
        reply.extend_from_slice(&buffer[..length]);
        peer.send_to(&reply, egress).await.unwrap();
    });
    Ok((peer_address, task))
}

#[tokio::test]
async fn test_xudp_drops_response_from_alternate_source() -> Result<(), Box<dyn std::error::Error>>
{
    let deadline = Instant::now() + SCENARIO_DEADLINE;
    let mut ports = PortHelper::new();
    let (shoes_ip, shoes_port) = ports.get_listener_port();
    let (singbox_ip, singbox_port) = ports.get_listener_port();
    let (peer_address, peer_task) = start_split_source_peer().await?;

    let (certificate, key) = generate_test_cert_files()?;
    let shoes_config = format!(
        r#"
- address: "{shoes_ip}:{shoes_port}"
  protocol:
    type: tls
    tls_targets:
      test.local:
        cert: {}
        key: {}
        vision: true
        protocol:
          type: vless
          user_id: "{TEST_UUID}"
          udp_enabled: true
"#,
        AsRef::<Path>::as_ref(&certificate).display(),
        AsRef::<Path>::as_ref(&key).display(),
    );
    let singbox_config = format!(
        r#"{{
  "log": {{"level": "warn"}},
  "inbounds": [{{
    "type": "vless",
    "tag": "vless-in",
    "listen": "{singbox_ip}",
    "listen_port": {singbox_port},
    "users": [{{"uuid": "{TEST_UUID}"}}]
  }}],
  "outbounds": [{{
    "type": "vless",
    "tag": "vless-out",
    "server": "{shoes_ip}",
    "server_port": {shoes_port},
    "uuid": "{TEST_UUID}",
    "flow": "xtls-rprx-vision",
    "tls": {{
      "enabled": true,
      "server_name": "test.local",
      "insecure": true
    }},
    "packet_encoding": "xudp"
  }}],
  "route": {{"final": "vless-out"}}
}}"#,
    );

    let (_shoes_guard, _shoes_config) = start_shoes_server(&shoes_config)?;
    let (_singbox_guard, _singbox_config) = start_singbox(&singbox_config)?;
    ports.wait_for_all_ports().await?;

    let stream = TcpStream::connect((singbox_ip.as_str(), singbox_port)).await?;
    let mut client = VlessUdpClient::connect(
        stream,
        parse_uuid(TEST_UUID)?,
        VlessDestination::Ip(peer_address),
    )
    .await?;

    client.send_packet(b"first").await?;
    let dropped = timeout(DROP_WINDOW, client.recv_packet(REPLY_WINDOW)).await;
    assert!(
        dropped.is_err(),
        "a reply from an alternate source must be dropped, got {dropped:?}"
    );

    // The addressed peer's reply is still delivered through the same association.
    client.send_packet(b"second").await?;
    let reply = timeout_at(deadline, client.recv_packet(REPLY_WINDOW)).await??;
    assert_eq!(
        reply, b"peer reply:second",
        "the addressed peer's reply must be delivered"
    );

    timeout_at(deadline, peer_task).await??;
    Ok(())
}

#[tokio::test]
async fn test_xudp_direct_client_drops_alternate_source_and_keeps_the_peer()
-> Result<(), Box<dyn std::error::Error>> {
    let deadline = Instant::now() + SCENARIO_DEADLINE;
    let mut ports = PortHelper::new();
    let (shoes_ip, shoes_port) = ports.get_listener_port();
    let (peer_address, peer_task) = start_split_source_peer().await?;

    let shoes_config = format!(
        r#"
- address: "{shoes_ip}:{shoes_port}"
  protocol:
    type: vless
    user_id: "{TEST_UUID}"
    udp_enabled: true
"#
    );
    let (_shoes_guard, _shoes_config) = start_shoes_server(&shoes_config)?;
    ports.wait_for_all_ports().await?;

    let stream = TcpStream::connect((shoes_ip.as_str(), shoes_port)).await?;
    let mut client = XudpClient::connect(stream, parse_uuid(TEST_UUID)?).await?;

    client
        .send_frame(&XudpFrame {
            session_id: 7,
            status: XUDP_STATUS_NEW,
            options: XUDP_OPTION_DATA,
            destination: Some(VlessDestination::Ip(peer_address)),
            payload: Some(b"first".to_vec()),
        })
        .await?;
    let dropped = timeout(DROP_WINDOW, client.recv_frame(REPLY_WINDOW)).await;
    assert!(
        dropped.is_err(),
        "a reply from an alternate source must be dropped, got {dropped:?}"
    );

    client
        .send_frame(&XudpFrame {
            session_id: 7,
            status: XUDP_STATUS_KEEP,
            options: XUDP_OPTION_DATA,
            destination: None,
            payload: Some(b"second".to_vec()),
        })
        .await?;
    let reply = timeout_at(deadline, client.recv_frame(REPLY_WINDOW)).await??;
    assert_eq!(
        reply.payload.as_deref(),
        Some(&b"peer reply:second"[..]),
        "the addressed peer's reply must be delivered"
    );

    timeout_at(deadline, peer_task).await??;
    Ok(())
}
