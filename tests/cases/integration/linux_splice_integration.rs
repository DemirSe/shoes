//! End-to-end Linux splice relay checks through a real shoes process.
//!
//! The inbound is a plain VLESS TCP listener, so both relay directions are raw
//! TCP sockets and the fast path is eligible. Every check binds loopback only.
//! The unit tests in `src/copy_bidirectional.rs` prove that the fast path runs
//! and stays byte-exact; these checks prove the real server path stays exact and
//! holds and releases its pipes.

use std::io::{self, Write};
use std::net::{IpAddr, SocketAddr};
use std::process::{Command, Stdio};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use shoes_test_support::port_helper::PortHelper;
use shoes_test_support::test_fixture::ProcessGuard;
use shoes_test_support::test_servers::start_tcp_stream_echo_server;
use shoes_test_support::vless::parse_uuid;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::Mutex;
use tokio::time::{sleep, timeout};

const VLESS_UUID: &str = "8f1a1b2c-3d4e-4f50-8192-a3b4c5d6e7f8";
const VLESS_VERSION: u8 = 0;
const VLESS_COMMAND_TCP: u8 = 1;
const VLESS_ADDRESS_IPV4: u8 = 1;
const VLESS_ADDRESS_IPV6: u8 = 3;
const VLESS_RESPONSE_HEADER: [u8; 2] = [0, 0];

/// Starts a shoes process and returns its guard, config file, and process id.
fn start_process(config: &str) -> io::Result<(ProcessGuard, tempfile::NamedTempFile, u32)> {
    let mut file = tempfile::NamedTempFile::new()?;
    file.write_all(config.as_bytes())?;
    file.flush()?;
    let binary = std::env::var_os("SHOES_TEST_SHOES_BIN")
        .unwrap_or_else(|| env!("CARGO_BIN_EXE_shoes").into());
    let child = Command::new(binary)
        .args(["-t", "1", "--no-reload"])
        .env("RUST_LOG", "warn")
        .stdout(Stdio::null())
        .stderr(Stdio::inherit())
        .arg(file.path())
        .spawn()?;
    let pid = child.id();
    Ok((ProcessGuard::new(child, "splice-shoes"), file, pid))
}

fn vless_server_config(ip: &str, port: u16) -> String {
    format!(
        r#"- address: '{ip}:{port}'
  protocol:
    type: vless
    user_id: "{VLESS_UUID}"
"#
    )
}

/// A VLESS TCP CONNECT request with the payload in the same write.
fn vless_tcp_request(destination: SocketAddr, payload: &[u8]) -> io::Result<Vec<u8>> {
    let uuid = parse_uuid(VLESS_UUID)?;
    let mut request = Vec::with_capacity(23 + payload.len());
    request.push(VLESS_VERSION);
    request.extend_from_slice(&uuid);
    request.push(0); // addon length
    request.push(VLESS_COMMAND_TCP);
    request.extend_from_slice(&destination.port().to_be_bytes());
    match destination.ip() {
        IpAddr::V4(address) => {
            request.push(VLESS_ADDRESS_IPV4);
            request.extend_from_slice(&address.octets());
        }
        IpAddr::V6(address) => {
            request.push(VLESS_ADDRESS_IPV6);
            request.extend_from_slice(&address.octets());
        }
    }
    request.extend_from_slice(payload);
    Ok(request)
}

/// Opens a VLESS TCP connection, sends the request and payload together, and
/// consumes the response header.
async fn vless_tcp_connect(
    proxy_port: u16,
    destination: SocketAddr,
    payload: &[u8],
) -> io::Result<TcpStream> {
    let mut stream = TcpStream::connect(("127.0.0.1", proxy_port)).await?;
    stream
        .write_all(&vless_tcp_request(destination, payload)?)
        .await?;
    let mut response = [0u8; 2];
    stream.read_exact(&mut response).await?;
    assert_eq!(response, VLESS_RESPONSE_HEADER);
    Ok(stream)
}

fn pattern(len: usize) -> Vec<u8> {
    (0..len)
        .map(|index| (index as u8).wrapping_mul(37).wrapping_add(11))
        .collect()
}

/// Pipe descriptors a process holds, which the relay fast path needs.
fn pipe_descriptor_count(pid: u32) -> usize {
    let Ok(entries) = std::fs::read_dir(format!("/proc/{pid}/fd")) else {
        return 0;
    };
    entries
        .filter_map(|entry| entry.ok())
        .filter(|entry| {
            std::fs::read_link(entry.path())
                .map(|target| target.to_string_lossy().starts_with("pipe:"))
                .unwrap_or(false)
        })
        .count()
}

/// Accepts TCP connections and holds them unread until `draining` is set.
///
/// The readiness probe connects and closes, so the listener stays open for the
/// relayed connection.
async fn start_stalled_sink(ip: &str, port: u16) -> io::Result<(SocketAddr, Arc<AtomicBool>)> {
    let listener = TcpListener::bind(format!("{ip}:{port}")).await?;
    let address = listener.local_addr()?;
    let draining = Arc::new(AtomicBool::new(false));
    let flag = draining.clone();
    tokio::spawn(async move {
        let mut held: Vec<TcpStream> = Vec::new();
        let mut buffer = vec![0; 64 * 1024];
        loop {
            tokio::select! {
                accepted = listener.accept() => match accepted {
                    Ok((stream, _)) => held.push(stream),
                    Err(_) => return,
                },
                _ = sleep(Duration::from_millis(20)) => {
                    if flag.load(Ordering::Relaxed) {
                        for stream in &mut held {
                            while let Ok(length) = stream.read(&mut buffer).await {
                                if length == 0 {
                                    break;
                                }
                            }
                        }
                        held.clear();
                    }
                }
            }
        }
    });
    Ok((address, draining))
}

/// Sends `reply` on every accepted connection, half-closes its write side, and
/// records what the peer sends afterwards.
async fn start_reply_then_close_server(
    ip: &str,
    port: u16,
    reply: Vec<u8>,
) -> io::Result<(SocketAddr, Arc<Mutex<Vec<u8>>>)> {
    let listener = TcpListener::bind(format!("{ip}:{port}")).await?;
    let address = listener.local_addr()?;
    let received = Arc::new(Mutex::new(Vec::new()));
    let recorded = received.clone();
    tokio::spawn(async move {
        while let Ok((mut stream, _)) = listener.accept().await {
            let reply = reply.clone();
            let recorded = recorded.clone();
            tokio::spawn(async move {
                if stream.write_all(&reply).await.is_err() || stream.shutdown().await.is_err() {
                    return;
                }
                let mut buffer = vec![0; 64 * 1024];
                while let Ok(length) = stream.read(&mut buffer).await {
                    if length == 0 {
                        break;
                    }
                    recorded.lock().await.extend_from_slice(&buffer[..length]);
                }
            });
        }
    });
    Ok((address, received))
}

#[tokio::test]
async fn a_vless_relay_moves_exact_bytes_and_keeps_the_client_first_half_close() -> io::Result<()> {
    let mut ports = PortHelper::new();
    let (proxy_ip, proxy_port) = ports.get_localhost_listener_port();
    let (echo_ip, echo_port) = ports.get_localhost_listener_port();
    let _echo = start_tcp_stream_echo_server(&echo_ip, echo_port).await?;
    let (_process, _config, _) = start_process(&vless_server_config(&proxy_ip, proxy_port))?;
    ports.wait_for_all_ports().await?;

    let destination: SocketAddr = format!("{echo_ip}:{echo_port}")
        .parse()
        .map_err(io::Error::other)?;
    // The payload travels in the same write as the request header, so the relay
    // sees queued prefix data before the fast path can take over.
    let payload = pattern(256 * 1024);
    let mut stream = vless_tcp_connect(proxy_port, destination, &payload).await?;
    let mut echoed = vec![0; payload.len()];
    timeout(Duration::from_secs(20), stream.read_exact(&mut echoed)).await??;
    assert_eq!(echoed, payload, "the relayed payload must be byte exact");

    // The client closes first; the echo reply must still arrive completely.
    stream.shutdown().await?;
    let mut trailing = [0u8; 1];
    let read = timeout(Duration::from_secs(10), stream.read(&mut trailing)).await??;
    assert_eq!(
        read, 0,
        "the relay must half-close the client side after the echo ends"
    );
    Ok(())
}

#[tokio::test]
async fn a_vless_relay_keeps_the_destination_first_half_close() -> io::Result<()> {
    let mut ports = PortHelper::new();
    let (proxy_ip, proxy_port) = ports.get_localhost_listener_port();
    let (peer_ip, peer_port) = ports.get_localhost_listener_port();
    let reply = pattern(64 * 1024);
    let (destination, received) =
        start_reply_then_close_server(&peer_ip, peer_port, reply.clone()).await?;
    let (_process, _config, _) = start_process(&vless_server_config(&proxy_ip, proxy_port))?;
    ports.wait_for_all_ports().await?;

    let mut stream = vless_tcp_connect(proxy_port, destination, &[]).await?;
    let mut echoed = Vec::new();
    let mut buffer = vec![0; 16 * 1024];
    loop {
        let read = timeout(Duration::from_secs(20), stream.read(&mut buffer)).await??;
        if read == 0 {
            break;
        }
        echoed.extend_from_slice(&buffer[..read]);
    }
    assert_eq!(
        echoed, reply,
        "the reply must arrive completely before its end of stream"
    );

    let request = pattern(32 * 1024);
    stream.write_all(&request).await?;
    stream.shutdown().await?;

    let recorded = timeout(Duration::from_secs(10), async {
        loop {
            if received.lock().await.len() >= request.len() {
                return received.lock().await.clone();
            }
            sleep(Duration::from_millis(20)).await;
        }
    })
    .await?;
    assert_eq!(
        recorded, request,
        "the client write must survive the destination half-close"
    );
    Ok(())
}

#[tokio::test]
async fn a_vless_relay_moves_exact_bytes_to_an_ipv6_destination() -> io::Result<()> {
    let mut ports = PortHelper::new();
    let (proxy_ip, proxy_port) = ports.get_localhost_listener_port();
    let (echo_ip, echo_port) = ports.get_ipv6_listener_port();
    let echo = start_tcp_stream_echo_server(&echo_ip, echo_port).await;
    let Ok(_echo) = echo else {
        eprintln!("SKIP: IPv6 loopback is unavailable");
        return Ok(());
    };
    let (_process, _config, _) = start_process(&vless_server_config(&proxy_ip, proxy_port))?;
    ports.wait_for_all_ports().await?;

    let destination: SocketAddr = format!("[{echo_ip}]:{echo_port}")
        .parse()
        .map_err(io::Error::other)?;
    let payload = pattern(128 * 1024);
    let mut stream = vless_tcp_connect(proxy_port, destination, &payload).await?;
    let mut echoed = vec![0; payload.len()];
    timeout(Duration::from_secs(20), stream.read_exact(&mut echoed)).await??;
    assert_eq!(echoed, payload, "IPv6 relayed payload must be byte exact");
    stream.shutdown().await?;
    Ok(())
}

#[tokio::test]
async fn the_relay_holds_and_releases_its_pipes() -> io::Result<()> {
    let mut ports = PortHelper::new();
    let (proxy_ip, proxy_port) = ports.get_localhost_listener_port();
    let (sink_ip, sink_port) = ports.get_localhost_listener_port();
    let (destination, draining) = start_stalled_sink(&sink_ip, sink_port).await?;
    let (_process, _config, pid) = start_process(&vless_server_config(&proxy_ip, proxy_port))?;
    ports.wait_for_all_ports().await?;

    // Sessions that complete must leave no pipe behind.
    for _ in 0..3 {
        let mut stream = vless_tcp_connect(proxy_port, destination, b"probe").await?;
        stream.shutdown().await?;
    }
    sleep(Duration::from_millis(200)).await;
    let baseline = pipe_descriptor_count(pid);

    // The sink never reads, so the relay stalls with bytes in its pipe.
    let mut stalled = vless_tcp_connect(proxy_port, destination, &pattern(512 * 1024)).await?;
    let observed = timeout(Duration::from_secs(10), async {
        loop {
            let count = pipe_descriptor_count(pid);
            if count >= baseline + 2 {
                return count;
            }
            sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .map_err(|_| {
        io::Error::new(
            io::ErrorKind::TimedOut,
            format!("no splice pipe appeared: baseline {baseline}"),
        )
    })?;
    assert!(observed >= baseline + 2);

    // The sink drains, then the session ends, so the relay releases its pipe.
    draining.store(true, Ordering::Relaxed);
    stalled.shutdown().await?;
    timeout(Duration::from_secs(20), async {
        loop {
            if pipe_descriptor_count(pid) <= baseline {
                return;
            }
            sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .map_err(|_| {
        io::Error::new(
            io::ErrorKind::TimedOut,
            format!("splice pipes did not close: baseline {baseline}"),
        )
    })?;
    Ok(())
}
