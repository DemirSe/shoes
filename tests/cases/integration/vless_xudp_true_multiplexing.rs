//! Multiple-destination coverage through one SOCKS5 UDP association.

use std::collections::HashSet;
use std::error::Error;
use std::path::Path;
use std::sync::{
    Arc,
    atomic::{AtomicUsize, Ordering},
};
use std::time::Duration;

use shoes_test_support as common;
use tokio::net::{TcpListener, TcpStream, UdpSocket};
use tokio::sync::Notify;
use tokio::task::{JoinHandle, JoinSet};
use tokio::time::{Instant, timeout_at};

use common::certs::generate_test_cert_files;
use common::socks5::{
    Socks5UdpAssociation, SocksDestination, decode_udp_datagram, encode_udp_datagram,
};
use common::test_fixture::{TEST_UUID, start_shoes_server, start_singbox_server};
use common::test_servers::start_udp_echo_server_with_suffix;

type TestResult<T = ()> = Result<T, Box<dyn Error>>;

struct BackgroundTask(JoinHandle<()>);

impl Drop for BackgroundTask {
    fn drop(&mut self) {
        self.0.abort();
    }
}

/// The UDP DNS fixture either returns NXDOMAIN or deliberately never responds.
/// Only the proxy's explicitly configured loopback resolver can reach it.
async fn failing_dns(
    respond: bool,
) -> TestResult<(std::net::SocketAddr, Arc<Notify>, BackgroundTask)> {
    let socket = UdpSocket::bind("127.0.0.1:0").await?;
    let address = socket.local_addr()?;
    let seen = Arc::new(Notify::new());
    let notify = seen.clone();
    let task = tokio::spawn(async move {
        let mut buffer = [0; 4096];
        while let Ok((length, peer)) = socket.recv_from(&mut buffer).await {
            notify.notify_one();
            if !respond || length < 12 {
                continue;
            }
            // Keep only the question; drop EDNS/additional sections. No payload
            // or test configuration is printed by this fixture.
            let mut end = 12;
            while end < length && buffer[end] != 0 {
                let label = buffer[end] as usize;
                if label > 63 {
                    break;
                }
                end += label + 1;
            }
            if end >= length || buffer[end] != 0 || end + 5 > length {
                continue;
            }
            end += 5;
            let mut response = buffer[..end].to_vec();
            response[2..4].copy_from_slice(&[0x81, 0x83]);
            response[4..6].copy_from_slice(&[0, 1]);
            response[6..12].fill(0);
            if socket.send_to(&response, peer).await.is_err() {
                break;
            }
        }
    });
    Ok((address, seen, BackgroundTask(task)))
}

async fn assert_dns_fault_is_datagram_local(vision: bool, respond: bool) -> TestResult {
    let mut ports = common::port_helper::PortHelper::new();
    let (shoes_ip, shoes_port) = ports.get_listener_port();
    let (singbox_ip, singbox_port) = ports.get_listener_port();
    let echoes = [
        start_udp_echo_server_with_suffix("127.0.0.1", 0, b"").await?,
        start_udp_echo_server_with_suffix("127.0.0.1", 0, b"").await?,
    ];
    let (dns_address, dns_seen, _dns_task) = failing_dns(respond).await?;
    // Count real TCP transports so a client reconnect cannot disguise failure
    // isolation. The relay never changes or records VLESS/TLS payloads.
    let listener = TcpListener::bind("127.0.0.1:0").await?;
    let relay_address = listener.local_addr()?;
    let accepts = Arc::new(AtomicUsize::new(0));
    let count = accepts.clone();
    let destination = format!("{shoes_ip}:{shoes_port}");
    let _relay_task = BackgroundTask(tokio::spawn(async move {
        let mut transfers = JoinSet::new();
        loop {
            tokio::select! {
                accepted = listener.accept() => {
                    let Ok((mut client, _)) = accepted else { break; };
                    count.fetch_add(1, Ordering::Relaxed);
                    let destination = destination.clone();
                    transfers.spawn(async move {
                        if let Ok(mut server) = TcpStream::connect(destination).await {
                            let _ = tokio::io::copy_bidirectional(&mut client, &mut server).await;
                        }
                    });
                }
                Some(_) = transfers.join_next(), if !transfers.is_empty() => {}
            }
        }
    }));
    let (cert_path, key_path) = generate_test_cert_files()?;
    let protocol = if vision {
        format!(
            r#"
    type: tls
    tls_targets:
      whatsapp.net:
        cert: {}
        key: {}
        vision: true
        protocol:
          type: vless
          user_id: "{TEST_UUID}"
          udp_enabled: true
"#,
            AsRef::<Path>::as_ref(&cert_path).display(),
            AsRef::<Path>::as_ref(&key_path).display()
        )
    } else {
        format!("\n    type: vless\n    user_id: '{TEST_UUID}'\n    udp_enabled: true\n")
    };
    let shoes_config = format!(
        r#"
- address: "{shoes_ip}:{shoes_port}"
  dns:
    servers:
      - url: "udp://{dns_address}"
        ip_strategy: ipv4_only
        timeout_secs: 1
        attempts: 1
  protocol:{protocol}
"#
    );
    let mut outbound = serde_json::json!({
        "type": "vless", "tag": "proxy", "server": relay_address.ip().to_string(),
        "server_port": relay_address.port(), "uuid": TEST_UUID, "packet_encoding": "xudp"
    });
    if vision {
        outbound["flow"] = "xtls-rprx-vision".into();
        outbound["tls"] = serde_json::json!({
            "enabled": true, "server_name": "whatsapp.net", "insecure": true
        });
    }
    let singbox_config = serde_json::json!({
        "log": { "level": "warn" },
        "inbounds": [{ "type": "socks", "listen": singbox_ip, "listen_port": singbox_port }],
        "outbounds": [outbound], "route": { "final": "proxy" }
    })
    .to_string();
    let (_shoes, _shoes_config) = start_shoes_server(&shoes_config)?;
    let (_singbox, _singbox_config) = start_singbox_server(&singbox_config)?;
    ports.wait_for_all_ports().await?;
    let association = Socks5UdpAssociation::connect(&singbox_ip, singbox_port).await?;
    for echo in &echoes {
        assert_eq!(
            association.send_to(echo.local_addr(), b"before").await?,
            b"before"
        );
    }
    assert_eq!(
        accepts.load(Ordering::Relaxed),
        1,
        "destinations must share one TCP transport"
    );
    let request = encode_udp_datagram(
        &SocksDestination::Hostname("isolation-probe.test".into(), echoes[0].local_addr().port()),
        b"drop",
    )?;
    association
        .socket()
        .send_to(&request, association.relay_addr())
        .await?;
    tokio::time::timeout(Duration::from_secs(3), dns_seen.notified()).await?;
    for echo in echoes.iter().rev() {
        assert_eq!(
            association
                .send_to_with_timeout(
                    SocksDestination::Ip(echo.local_addr()),
                    b"after",
                    Duration::from_secs(6),
                )
                .await?,
            b"after"
        );
    }
    assert_eq!(
        accepts.load(Ordering::Relaxed),
        1,
        "DNS failure must not require reconnecting"
    );
    Ok(())
}

#[tokio::test]
async fn xudp_nxdomain_keeps_other_destinations_on_the_same_tcp_connection() -> TestResult {
    assert_dns_fault_is_datagram_local(false, true).await
}

#[tokio::test]
async fn xudp_dns_timeout_keeps_other_destinations_on_the_same_tcp_connection() -> TestResult {
    assert_dns_fault_is_datagram_local(false, false).await
}

#[tokio::test]
async fn vision_xudp_nxdomain_keeps_other_destinations_on_the_same_tcp_connection() -> TestResult {
    assert_dns_fault_is_datagram_local(true, true).await
}

#[tokio::test]
async fn vision_xudp_dns_timeout_keeps_other_destinations_on_the_same_tcp_connection() -> TestResult
{
    assert_dns_fault_is_datagram_local(true, false).await
}

#[tokio::test]
async fn one_socks_association_reaches_multiple_xudp_destinations() -> TestResult {
    let mut ports = common::port_helper::PortHelper::new();
    let (shoes_ip, shoes_port) = ports.get_listener_port();
    let (singbox_ip, singbox_port) = ports.get_listener_port();
    let echoes = [
        start_udp_echo_server_with_suffix("127.0.0.1", 0, b" [SERVER-1]").await?,
        start_udp_echo_server_with_suffix("127.0.0.1", 0, b" [SERVER-2]").await?,
        start_udp_echo_server_with_suffix("127.0.0.1", 0, b" [SERVER-3]").await?,
    ];

    let (cert_path, key_path) = generate_test_cert_files()?;
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
        AsRef::<Path>::as_ref(&cert_path).display(),
        AsRef::<Path>::as_ref(&key_path).display(),
    );
    let singbox_config = format!(
        r#"{{
  "log": {{ "level": "debug" }},
  "inbounds": [{{
    "type": "socks",
    "tag": "socks-in",
    "listen": "{singbox_ip}",
    "listen_port": {singbox_port}
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
  "route": {{ "final": "vless-out" }}
}}"#
    );

    let (_shoes, _shoes_config) = start_shoes_server(&shoes_config)?;
    let (_singbox, _singbox_config) = start_singbox_server(&singbox_config)?;
    ports.wait_for_all_ports().await?;

    let association = Socks5UdpAssociation::connect(&singbox_ip, singbox_port).await?;
    let cases = [
        (
            echoes[0].local_addr(),
            b"request-1".as_slice(),
            b" [SERVER-1]".as_slice(),
        ),
        (
            echoes[1].local_addr(),
            b"request-2".as_slice(),
            b" [SERVER-2]".as_slice(),
        ),
        (
            echoes[2].local_addr(),
            b"request-3".as_slice(),
            b" [SERVER-3]".as_slice(),
        ),
    ];

    for (destination, request, _) in cases {
        let datagram = encode_udp_datagram(&SocksDestination::Ip(destination), request)?;
        association
            .socket()
            .send_to(&datagram, association.relay_addr())
            .await?;
    }

    let deadline = Instant::now() + Duration::from_secs(10);
    let mut responses = HashSet::new();
    for _ in cases {
        let mut buffer = [0; 65_536];
        let (length, source) =
            timeout_at(deadline, association.socket().recv_from(&mut buffer)).await??;
        assert_eq!(source, association.relay_addr());
        responses.insert(decode_udp_datagram(&buffer[..length])?.to_vec());
    }
    let expected: HashSet<_> = cases
        .iter()
        .map(|(_, request, suffix)| [*request, *suffix].concat())
        .collect();
    assert_eq!(responses, expected);

    for (destination, _, suffix) in cases.iter().rev() {
        let response = association
            .send_to_with_timeout(
                SocksDestination::Ip(*destination),
                b"repeat",
                Duration::from_secs(5),
            )
            .await?;
        assert_eq!(response, [b"repeat".as_slice(), *suffix].concat());
    }
    Ok(())
}
