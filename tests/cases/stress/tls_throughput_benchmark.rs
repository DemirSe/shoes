//! Compares the Shoes TLS stream with a direct tokio-rustls stream.

use std::error::Error;
use std::io;
use std::path::Path;
use std::sync::Arc;
use std::time::{Duration, Instant};

use shoes_test_support as common;
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};
use tokio::net::TcpStream;

use common::test_fixture::start_shoes_server;
use common::test_servers::{
    TlsVersion, generate_test_cert, start_tcp_stream_echo_server, start_tls_stream_echo_server,
};

const TEST_SIZES: &[usize] = &[1024, 10 * 1024, 100 * 1024, 1024 * 1024, 10 * 1024 * 1024];
const TRANSFER_TIMEOUT: Duration = Duration::from_secs(30);
const TEST_UUID: &str = "b85798ef-e9dc-46a4-9a87-8da4499d36d0";

type TestResult<T = ()> = Result<T, Box<dyn Error>>;

#[derive(Debug)]
struct BenchmarkResult {
    size: usize,
    duration: Duration,
}

impl BenchmarkResult {
    fn throughput_mbps(&self) -> f64 {
        (self.size as f64 * 8.0) / (self.duration.as_secs_f64() * 1_000_000.0)
    }
}

async fn tls_client(
    address: std::net::SocketAddr,
    cert_path: &Path,
) -> TestResult<tokio_rustls::client::TlsStream<TcpStream>> {
    let mut root_store = rustls::RootCertStore::empty();
    let certs = rustls_pemfile::certs(&mut io::BufReader::new(std::fs::File::open(cert_path)?))
        .collect::<Result<Vec<_>, _>>()?;
    let (added, ignored) = root_store.add_parsable_certificates(certs);
    if added == 0 || ignored != 0 {
        return Err(format!("loaded {added} certificates and ignored {ignored}").into());
    }

    let config = Arc::new(
        rustls::ClientConfig::builder()
            .with_root_certificates(root_store)
            .with_no_client_auth(),
    );
    let connector = tokio_rustls::TlsConnector::from(config);
    let server_name = rustls::pki_types::ServerName::try_from("test.local")?.to_owned();
    Ok(connector
        .connect(server_name, TcpStream::connect(address).await?)
        .await?)
}

/// Send a VLESS TCP CONNECT request and consume the two-byte response header.
async fn vless_connect_tcp(
    stream: &mut tokio_rustls::client::TlsStream<TcpStream>,
    destination: std::net::SocketAddr,
) -> TestResult {
    let uuid = common::vless::parse_uuid(TEST_UUID)?;
    let mut request = Vec::with_capacity(22);
    request.push(0); // version
    request.extend_from_slice(&uuid);
    request.push(0); // addon length
    request.push(1); // command: TCP CONNECT
    request.extend_from_slice(&destination.port().to_be_bytes());
    match destination.ip() {
        std::net::IpAddr::V4(v4) => {
            request.push(1);
            request.extend_from_slice(&v4.octets());
        }
        std::net::IpAddr::V6(v6) => {
            request.push(3);
            request.extend_from_slice(&v6.octets());
        }
    }
    stream.write_all(&request).await?;

    let mut response_header = [0u8; 2];
    stream.read_exact(&mut response_header).await?;
    assert_eq!(response_header, [0, 0]);
    Ok(())
}

async fn benchmark_stream<S>(stream: S, size: usize) -> TestResult<BenchmarkResult>
where
    S: AsyncRead + AsyncWrite + Unpin,
{
    let expected = vec![0x42; size];
    let mut received = vec![0; size];
    let (mut reader, mut writer) = tokio::io::split(stream);
    let start = Instant::now();

    tokio::time::timeout(TRANSFER_TIMEOUT, async {
        tokio::try_join!(
            async {
                writer.write_all(&expected).await?;
                writer.flush().await?;
                writer.shutdown().await
            },
            async {
                reader.read_exact(&mut received).await?;
                Ok::<_, io::Error>(())
            }
        )
    })
    .await
    .map_err(|_| io::Error::new(io::ErrorKind::TimedOut, "TLS transfer timed out"))??;

    assert_eq!(received, expected);
    Ok(BenchmarkResult {
        size,
        duration: start.elapsed(),
    })
}

async fn benchmark_shoes(size: usize) -> TestResult<BenchmarkResult> {
    let echo = start_tcp_stream_echo_server("127.0.0.1", 0).await?;
    let (cert_path, key_path) = generate_test_cert()?;
    let mut ports = common::port_helper::PortHelper::new();
    let (proxy_ip, proxy_port) = ports.get_listener_port();
    let config = format!(
        r#"
- address: "{proxy_ip}:{proxy_port}"
  protocol:
    type: tls
    tls_targets:
      test.local:
        cert: {}
        key: {}
        protocol:
          type: vless
          user_id: "{TEST_UUID}"
"#,
        AsRef::<Path>::as_ref(&cert_path).display(),
        AsRef::<Path>::as_ref(&key_path).display(),
    );
    let (_shoes, _config) = start_shoes_server(&config)?;
    ports.wait_for_all_ports().await?;

    let address = format!("{proxy_ip}:{proxy_port}").parse()?;
    let mut stream = tls_client(address, &cert_path).await?;
    vless_connect_tcp(&mut stream, echo.local_addr()).await?;
    benchmark_stream(stream, size).await
}

async fn benchmark_tokio_rustls(size: usize) -> TestResult<BenchmarkResult> {
    let (cert_path, key_path) = generate_test_cert()?;
    let server =
        start_tls_stream_echo_server("127.0.0.1", 0, &cert_path, &key_path, TlsVersion::Tls13Only)
            .await?;
    benchmark_stream(tls_client(server.local_addr(), &cert_path).await?, size).await
}

#[tokio::test]
#[ignore = "manual throughput benchmark"]
async fn test_tls_throughput_comparison() -> TestResult {
    eprintln!("size_bytes,shoes_mbps,tokio_rustls_mbps,difference_percent");
    for &size in TEST_SIZES {
        let shoes = benchmark_shoes(size).await?;
        let tokio = benchmark_tokio_rustls(size).await?;
        let difference =
            (shoes.throughput_mbps() - tokio.throughput_mbps()) / tokio.throughput_mbps() * 100.0;
        eprintln!(
            "{size},{:.2},{:.2},{difference:.1}",
            shoes.throughput_mbps(),
            tokio.throughput_mbps(),
        );
    }
    Ok(())
}
