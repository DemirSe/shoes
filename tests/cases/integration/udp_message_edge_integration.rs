//! Wire regressions for empty UDP datagrams and for replies that arrive after a
//! client half-close.
//!
//! Each scenario drives a real shoes process over a real TCP connection with a
//! real loopback UDP peer:
//! - legacy VLESS UDP (bidirectional message copy) with an empty datagram,
//! - XUDP session framing with an empty datagram,
//! - a delayed peer reply after the client closed its write side.
//!
//! An empty datagram is a message, not end-of-stream, and a half-closed client
//! must still receive the reply it is waiting for. Every scenario is bounded,
//! including setup, exchange, and cleanup.

use std::error::Error;
use std::net::SocketAddr;
use std::time::Duration;

use shoes_test_support as common;

use common::process::ProcessGuard;
use common::test_fixture::{TEST_UUID, start_shoes_server};
use common::test_servers::{TestServer, start_udp_response_server};
use common::vless::{
    VlessDestination, VlessUdpClient, XUDP_OPTION_DATA, XUDP_STATUS_KEEP, XUDP_STATUS_NEW,
    XudpClient, XudpFrame, encode_vless_udp_packet, encode_vless_udp_request, parse_uuid,
};
use tempfile::NamedTempFile;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpStream, UdpSocket};
use tokio::time::{Instant, timeout, timeout_at};

type TestResult<T = ()> = Result<T, Box<dyn Error>>;

/// Bound for one exchange.
const SCENARIO_TIMEOUT: Duration = Duration::from_secs(10);
/// Bound for one whole test, including setup and cleanup.
const SCENARIO_DEADLINE: Duration = Duration::from_secs(45);

/// Start a plain VLESS server with UDP enabled.
async fn start_vless_udp_server() -> TestResult<(String, u16, ProcessGuard, NamedTempFile)> {
    let mut ports = common::port_helper::PortHelper::new();
    let (ip, port) = ports.get_listener_port();
    let config = format!(
        r#"
- address: "{ip}:{port}"
  protocol:
    type: vless
    user_id: "{TEST_UUID}"
    udp_enabled: true
"#
    );
    let (guard, config_file) = start_shoes_server(&config)?;
    ports.wait_for_all_ports().await?;
    Ok((ip, port, guard, config_file))
}

/// A peer that returns every datagram unchanged, so an empty request has an
/// empty reply.
async fn start_reflecting_udp_peer() -> TestResult<(TestServer, SocketAddr)> {
    start_reflecting_udp_peer_on("127.0.0.1").await
}

/// The same peer on one address family.
async fn start_reflecting_udp_peer_on(family: &str) -> TestResult<(TestServer, SocketAddr)> {
    let peer = start_udp_response_server(family, 0, |payload, _| payload.to_vec()).await?;
    let address = peer.local_addr();
    Ok((peer, address))
}

/// Read one VLESS UDP reply: the response header, then a length-prefixed packet.
async fn read_vless_udp_reply(stream: &mut TcpStream) -> TestResult<Vec<u8>> {
    let mut header = [0u8; 2];
    timeout(SCENARIO_TIMEOUT, stream.read_exact(&mut header)).await??;
    assert_eq!(header, [0, 0], "unexpected VLESS response header");

    let mut length = [0u8; 2];
    timeout(SCENARIO_TIMEOUT, stream.read_exact(&mut length)).await??;
    let length = u16::from_be_bytes(length) as usize;

    let mut payload = vec![0u8; length];
    timeout(SCENARIO_TIMEOUT, stream.read_exact(&mut payload)).await??;
    Ok(payload)
}

#[tokio::test]
async fn an_empty_datagram_crosses_the_vless_udp_session() -> TestResult {
    let deadline = Instant::now() + SCENARIO_DEADLINE;
    let (ip, port, _guard, _config) = start_vless_udp_server().await?;
    let (_peer, peer_address) = start_reflecting_udp_peer().await?;

    let stream = TcpStream::connect((ip.as_str(), port)).await?;
    let mut client = VlessUdpClient::connect(
        stream,
        parse_uuid(TEST_UUID)?,
        VlessDestination::Ip(peer_address),
    )
    .await?;

    client.send_packet(&[]).await?;
    let reply = timeout_at(deadline, client.recv_packet(SCENARIO_TIMEOUT)).await??;
    assert!(
        reply.is_empty(),
        "an empty datagram must come back empty, got {reply:?}"
    );

    client.send_packet(b"after the empty datagram").await?;
    let reply = timeout_at(deadline, client.recv_packet(SCENARIO_TIMEOUT)).await??;
    assert_eq!(
        reply, b"after the empty datagram",
        "the session must stay usable after an empty datagram"
    );
    Ok(())
}

#[tokio::test]
async fn an_empty_datagram_crosses_the_xudp_session() -> TestResult {
    let deadline = Instant::now() + SCENARIO_DEADLINE;
    let (ip, port, _guard, _config) = start_vless_udp_server().await?;
    let (_peer, peer_address) = start_reflecting_udp_peer().await?;

    let stream = TcpStream::connect((ip.as_str(), port)).await?;
    let mut client = XudpClient::connect(stream, parse_uuid(TEST_UUID)?).await?;

    let empty_frame = XudpFrame {
        session_id: 7,
        status: XUDP_STATUS_NEW,
        options: XUDP_OPTION_DATA,
        destination: Some(VlessDestination::Ip(peer_address)),
        payload: Some(Vec::new()),
    };
    client.send_frame(&empty_frame).await?;
    let reply = timeout_at(deadline, client.recv_frame(SCENARIO_TIMEOUT)).await??;
    assert_eq!(
        reply.options & XUDP_OPTION_DATA,
        XUDP_OPTION_DATA,
        "the reply must carry a data section"
    );
    assert_eq!(
        reply.payload.as_deref(),
        Some(&[][..]),
        "an empty datagram must come back empty"
    );

    let full_frame = XudpFrame {
        session_id: 7,
        status: XUDP_STATUS_KEEP,
        options: XUDP_OPTION_DATA,
        destination: None,
        payload: Some(b"after the empty datagram".to_vec()),
    };
    client.send_frame(&full_frame).await?;
    let reply = timeout_at(deadline, client.recv_frame(SCENARIO_TIMEOUT)).await??;
    assert_eq!(
        reply.payload.as_deref(),
        Some(&b"after the empty datagram"[..]),
        "the session must stay usable after an empty datagram"
    );
    Ok(())
}

/// Exact payload sizes through the legacy VLESS UDP session on both families.
#[tokio::test]
async fn udp_payloads_up_to_8192_round_trip_on_both_families() -> TestResult {
    let deadline = Instant::now() + SCENARIO_DEADLINE;
    let (ip, port, _guard, _config) = start_vless_udp_server().await?;
    let payloads = [1usize, 512, 1472, 8192];
    let mut sizes = Vec::new();

    for family in ["127.0.0.1", "::1"] {
        let (peer, peer_address) = start_reflecting_udp_peer_on(family).await?;
        let stream = TcpStream::connect((ip.as_str(), port)).await?;
        let mut client = VlessUdpClient::connect(
            stream,
            parse_uuid(TEST_UUID)?,
            VlessDestination::Ip(peer_address),
        )
        .await?;
        for size in payloads {
            let payload: Vec<u8> = (0..size).map(|index| (index % 251) as u8).collect();
            client.send_packet(&payload).await?;
            let reply = timeout_at(deadline, client.recv_packet(SCENARIO_TIMEOUT)).await??;
            assert_eq!(
                reply, payload,
                "{size} bytes must round trip exactly over {family}"
            );
            sizes.push(size);
        }
        drop(peer);
    }

    assert_eq!(sizes.len(), payloads.len() * 2);
    Ok(())
}

/// The same exact payload sizes through an XUDP session on both families.
#[tokio::test]
async fn xudp_payloads_up_to_8192_round_trip_on_both_families() -> TestResult {
    let deadline = Instant::now() + SCENARIO_DEADLINE;
    let (ip, port, _guard, _config) = start_vless_udp_server().await?;

    for family in ["127.0.0.1", "::1"] {
        let (peer, peer_address) = start_reflecting_udp_peer_on(family).await?;
        let stream = TcpStream::connect((ip.as_str(), port)).await?;
        let mut client = XudpClient::connect(stream, parse_uuid(TEST_UUID)?).await?;
        for size in [1usize, 1472, 8192] {
            let payload: Vec<u8> = (0..size).map(|index| (index % 251) as u8).collect();
            client
                .send_frame(&XudpFrame {
                    session_id: 11,
                    status: XUDP_STATUS_NEW,
                    options: XUDP_OPTION_DATA,
                    destination: Some(VlessDestination::Ip(peer_address)),
                    payload: Some(payload.clone()),
                })
                .await?;
            let reply = timeout_at(deadline, client.recv_frame(SCENARIO_TIMEOUT)).await??;
            assert_eq!(
                reply.payload.as_deref(),
                Some(payload.as_slice()),
                "{size} bytes must round trip exactly over {family}"
            );
        }
        drop(peer);
    }
    Ok(())
}

#[tokio::test]
async fn a_delayed_reply_is_delivered_after_the_client_half_closes() -> TestResult {
    let deadline = Instant::now() + SCENARIO_DEADLINE;
    let (ip, port, _guard, _config) = start_vless_udp_server().await?;

    // A peer that answers only after the client has closed its write side.
    let peer = UdpSocket::bind("127.0.0.1:0").await?;
    let peer_address = peer.local_addr()?;
    let peer_task = tokio::spawn(async move {
        let mut buffer = vec![0u8; 2048];
        let (length, client) = peer.recv_from(&mut buffer).await.unwrap();
        tokio::time::sleep(Duration::from_millis(400)).await;
        let mut reply = b"delayed:".to_vec();
        reply.extend_from_slice(&buffer[..length]);
        peer.send_to(&reply, client).await.unwrap();
    });

    let mut stream = TcpStream::connect((ip.as_str(), port)).await?;
    stream
        .write_all(&encode_vless_udp_request(
            parse_uuid(TEST_UUID)?,
            &VlessDestination::Ip(peer_address),
        )?)
        .await?;
    stream.write_all(&encode_vless_udp_packet(b"ping")?).await?;
    stream.flush().await?;

    // The client is done sending, but it still expects the reply.
    stream.shutdown().await?;

    let reply = timeout_at(deadline, read_vless_udp_reply(&mut stream)).await??;
    assert_eq!(
        reply, b"delayed:ping",
        "a reply that arrives after the client half-close must be delivered"
    );

    timeout_at(deadline, peer_task).await??;
    Ok(())
}
