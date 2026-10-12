use std::io::{self, Seek, Write};
use std::net::{IpAddr, SocketAddr};
use std::process::{Command, Stdio};
use std::time::Duration;

use shoes_test_support::port_helper::PortHelper;
use shoes_test_support::test_fixture::ProcessGuard;
use shoes_test_support::test_servers::start_tcp_stream_echo_server;
use shoes_test_support::vless::{VlessDestination, VlessUdpClient, parse_uuid};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;
use tokio::time::{sleep, timeout};

const VLESS_UUID: &str = "8f1a1b2c-3d4e-4f50-8192-a3b4c5d6e7f8";

fn start_process(
    config: &str,
    reload: bool,
) -> io::Result<(ProcessGuard, tempfile::NamedTempFile, u32)> {
    start_process_with_limits(config, reload, "reload_grace_secs: 6")
}

fn start_process_with_limits(
    config: &str,
    reload: bool,
    limits: &str,
) -> io::Result<(ProcessGuard, tempfile::NamedTempFile, u32)> {
    let mut file = tempfile::NamedTempFile::new()?;
    writeln!(file, "- global_limits: {{{limits}}}")?;
    file.write_all(config.as_bytes())?;
    file.flush()?;
    let binary = std::env::var_os("SHOES_TEST_SHOES_BIN")
        .unwrap_or_else(|| env!("CARGO_BIN_EXE_shoes").into());
    let mut command = Command::new(binary);
    command
        .args(["-t", "1"])
        .env("RUST_LOG", "warn")
        .stdout(Stdio::null())
        .stderr(Stdio::inherit());
    if !reload {
        command.arg("--no-reload");
    }
    command.arg(file.path());
    let child = command.spawn()?;
    let pid = child.id();
    Ok((ProcessGuard::new(child, "lifecycle-shoes"), file, pid))
}

/// Open a VLESS TCP connection to `destination` through the server on `port`.
async fn vless_tcp_connect(port: u16, destination: SocketAddr) -> io::Result<TcpStream> {
    let uuid = parse_uuid(VLESS_UUID)?;
    let mut stream = TcpStream::connect(("127.0.0.1", port)).await?;
    let mut request = Vec::with_capacity(22);
    request.push(0); // version
    request.extend_from_slice(&uuid);
    request.push(0); // addon length
    request.push(1); // command: TCP CONNECT
    request.extend_from_slice(&destination.port().to_be_bytes());
    match destination.ip() {
        IpAddr::V4(v4) => {
            request.push(1);
            request.extend_from_slice(&v4.octets());
        }
        IpAddr::V6(v6) => {
            request.push(3);
            request.extend_from_slice(&v6.octets());
        }
    }
    stream.write_all(&request).await?;
    // Consume the VLESS response header: version byte and addon length.
    let mut response_header = [0u8; 2];
    stream.read_exact(&mut response_header).await?;
    assert_eq!(response_header, [0, 0]);
    Ok(stream)
}

async fn round_trip(stream: &mut TcpStream, message: &[u8]) -> io::Result<()> {
    timeout(Duration::from_secs(2), async {
        stream.write_all(message).await?;
        let mut reply = vec![0; message.len()];
        stream.read_exact(&mut reply).await?;
        assert_eq!(reply, message);
        Ok(())
    })
    .await?
}

fn vless_server_config(proxy_port: u16) -> String {
    format!(
        r#"- address: '0.0.0.0:{proxy_port}'
  protocol:
    type: vless
    user_id: "{VLESS_UUID}"
"#
    )
}

#[tokio::test]
async fn default_admission_exceeds_previous_connection_and_stream_caps() -> io::Result<()> {
    let mut ports = PortHelper::new();
    let (_, proxy_port) = ports.get_localhost_listener_port();
    let (_, echo_port) = ports.get_localhost_listener_port();
    let _echo = start_tcp_stream_echo_server("0.0.0.0", echo_port).await?;
    let config = vless_server_config(proxy_port);
    let (_process, _config, _) = start_process(&config, false)?;
    ports.wait_for_all_ports().await?;
    let destination: SocketAddr = format!("127.0.0.1:{echo_port}")
        .parse()
        .map_err(io::Error::other)?;
    let mut connections = Vec::new();
    for _ in 0..520 {
        let mut stream = vless_tcp_connect(proxy_port, destination).await?;
        round_trip(&mut stream, b"admitted").await?;
        connections.push(stream);
    }
    round_trip(&mut connections[0], b"still active").await?;
    round_trip(connections.last_mut().unwrap(), b"also active").await?;
    Ok(())
}

#[tokio::test]
async fn yaml_connection_limit_rejects_new_work_without_closing_existing_work() -> io::Result<()> {
    let mut ports = PortHelper::new();
    let (_, proxy_port) = ports.get_localhost_listener_port();
    let (_, echo_port) = ports.get_localhost_listener_port();
    let _echo = start_tcp_stream_echo_server("0.0.0.0", echo_port).await?;
    let config = vless_server_config(proxy_port);
    let (_process, _config, _) = start_process_with_limits(&config, false, "max_connections: 2")?;
    ports.wait_for_all_ports().await?;
    let destination: SocketAddr = format!("127.0.0.1:{echo_port}")
        .parse()
        .map_err(io::Error::other)?;
    // The readiness probe must relinquish its reservation before both real connections open.
    let mut connections = timeout(Duration::from_secs(3), async {
        loop {
            let mut streams = Vec::new();
            for _ in 0..2 {
                let Ok(mut stream) = vless_tcp_connect(proxy_port, destination).await else {
                    break;
                };
                if round_trip(&mut stream, b"accepted").await.is_err() {
                    break;
                }
                streams.push(stream);
            }
            if streams.len() == 2 {
                return Ok::<_, io::Error>(streams);
            }
            drop(streams);
            sleep(Duration::from_millis(10)).await;
        }
    })
    .await??;
    let mut rejected = TcpStream::connect(("127.0.0.1", proxy_port)).await?;
    let mut byte = [0];
    let result = timeout(Duration::from_secs(2), rejected.read(&mut byte)).await?;
    assert!(matches!(result, Ok(0) | Err(_)));
    for stream in &mut connections {
        round_trip(stream, b"still active").await?;
    }
    Ok(())
}

#[tokio::test]
async fn tcp_reload_drains_existing_connections_within_grace() -> io::Result<()> {
    let mut ports = PortHelper::new();
    let (_, proxy_port) = ports.get_localhost_listener_port();
    let (_, echo_port) = ports.get_localhost_listener_port();
    let _echo = start_tcp_stream_echo_server("0.0.0.0", echo_port).await?;
    let config = vless_server_config(proxy_port);
    let (_process, mut file, _) = start_process(&config, true)?;
    ports.wait_for_all_ports().await?;
    let (_, replacement_port) = ports.get_localhost_listener_port();
    let destination: SocketAddr = format!("127.0.0.1:{echo_port}")
        .parse()
        .map_err(io::Error::other)?;
    let mut existing = vless_tcp_connect(proxy_port, destination).await?;
    round_trip(&mut existing, b"before reload").await?;
    // Change the address to observe actual retirement. An unchanged address now
    // reuses its listening socket and deliberately has no connection-refused gap.
    file.as_file_mut().set_len(0)?;
    file.rewind()?;
    writeln!(file, "- global_limits: {{reload_grace_secs: 6}}")?;
    file.write_all(vless_server_config(replacement_port).as_bytes())?;
    file.flush()?;

    // Observe listener retirement rather than assuming notification timing.
    timeout(Duration::from_secs(3), async {
        while TcpStream::connect(("127.0.0.1", proxy_port)).await.is_ok() {
            sleep(Duration::from_millis(20)).await;
        }
    })
    .await?;
    round_trip(&mut existing, b"during reload").await?;
    let mut replacement = timeout(Duration::from_secs(5), async {
        loop {
            match vless_tcp_connect(replacement_port, destination).await {
                Ok(stream) => break stream,
                Err(_) => sleep(Duration::from_millis(20)).await,
            }
        }
    })
    .await?;
    round_trip(&mut existing, b"after reload").await?;
    round_trip(&mut replacement, b"new generation").await?;
    let mut byte = [0];
    let closed = timeout(Duration::from_secs(8), existing.read(&mut byte)).await?;
    assert!(
        matches!(closed, Ok(0) | Err(_)),
        "old generation survived its grace deadline"
    );
    round_trip(&mut replacement, b"still active").await?;
    Ok(())
}

#[cfg(target_os = "linux")]
fn process_usage(pid: u32) -> io::Result<(usize, usize)> {
    let descriptors = std::fs::read_dir(format!("/proc/{pid}/fd"))?.count();
    let status = std::fs::read_to_string(format!("/proc/{pid}/status"))?;
    let resident_kib = status
        .lines()
        .find_map(|line| {
            let mut fields = line.split_whitespace();
            (fields.next()? == "VmRSS:")
                .then(|| fields.next()?.parse::<usize>().ok())
                .flatten()
        })
        .ok_or_else(|| io::Error::other("missing process RSS"))?;
    Ok((descriptors, resident_kib))
}

#[cfg(target_os = "linux")]
#[tokio::test]
async fn quiet_udp_association_churn_releases_descriptors_and_bounds_rss() -> io::Result<()> {
    use shoes_test_support::test_servers::start_udp_echo_server;

    let mut ports = PortHelper::new();
    let (_, port) = ports.get_localhost_listener_port();
    let (echo_ip, echo_port) = ports.get_port();
    let _echo = start_udp_echo_server(&echo_ip, echo_port).await?;
    let config = vless_server_config(port);
    let (_process, _config, pid) = start_process(&config, false)?;
    ports.wait_for_all_ports().await?;
    let destination = VlessDestination::Ip(
        format!("{echo_ip}:{echo_port}")
            .parse()
            .map_err(io::Error::other)?,
    );
    let uuid = parse_uuid(VLESS_UUID)?;
    let churn = |count| {
        let destination = destination.clone();
        async move {
            for _ in 0..count {
                let stream = TcpStream::connect(("127.0.0.1", port)).await?;
                let mut client = VlessUdpClient::connect(stream, uuid, destination.clone()).await?;
                client.send_packet(b"quiet").await?;
                let reply = client.recv_packet(Duration::from_secs(2)).await?;
                assert_eq!(reply, b"quiet [ECHO]");
                drop(client);
            }
            io::Result::Ok(())
        }
    };
    timeout(Duration::from_secs(10), churn(64)).await??;
    sleep(Duration::from_millis(100)).await;
    let baseline = process_usage(pid)?;
    for _ in 0..2 {
        timeout(Duration::from_secs(20), churn(512)).await??;
        timeout(Duration::from_secs(3), async {
            while process_usage(pid)?.0 > baseline.0 + 2 {
                sleep(Duration::from_millis(20)).await;
            }
            io::Result::Ok(())
        })
        .await??;
    }
    let final_usage = process_usage(pid)?;
    eprintln!("UDP churn: warm={baseline:?}, final={final_usage:?} (FDs, RSS KiB)");
    // Allow allocator caching; the former leak retained 64 KiB and a socket per association.
    assert!(
        final_usage.1 <= baseline.1 + 16 * 1024,
        "RSS kept growing after quiet association churn"
    );
    Ok(())
}
