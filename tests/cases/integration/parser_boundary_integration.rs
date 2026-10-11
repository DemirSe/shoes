//! Wire-path regressions for the repaired VLESS addon and XUDP metadata parsers.
//!
//! Each scenario sends synthetic authentication data to a real shoes process over an
//! actual TCP/TLS connection. A malformed frame must end with a controlled connection
//! error: the server closes the connection, forwards nothing to the address in the
//! rejected bytes, and still serves a fresh healthy authenticated connection.
//!
//! A connection-task panic under the default unwinding profile can look like a clean
//! close from the client, so these tests are paired with the dedicated replay's
//! bounded panic check: the replay fails the record when the run log contains a
//! panic diagnostic. The fixture inherits server stderr into that log. The tests
//! themselves bound every scenario, including setup, TLS connect, healthy exchange,
//! and cleanup.

use std::error::Error;
use std::net::SocketAddr;
use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

use shoes_test_support as common;

use common::certs::generate_test_cert_files;
use common::process::ProcessGuard;
use common::test_fixture::{TEST_UUID, start_shoes_server};
use common::test_servers::{TestServer, start_udp_echo_server_with_suffix};
use common::vless::{
    VlessDestination, VlessUdpClient, XUDP_OPTION_DATA, XUDP_STATUS_NEW, XudpClient, XudpFrame,
    encode_vless_udp_request, encode_vless_xudp_request, parse_uuid,
};
use tempfile::NamedTempFile;
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};
use tokio::net::{TcpStream, UdpSocket};
use tokio::time::{Instant, timeout, timeout_at};

type TestResult<T = ()> = Result<T, Box<dyn Error>>;

/// Bound for one connection-level observation or a healthy echo exchange.
const SCENARIO_TIMEOUT: Duration = Duration::from_secs(10);
/// Bound for one whole test, including setup, TLS connect, healthy exchange, and cleanup.
const SCENARIO_DEADLINE: Duration = Duration::from_secs(45);
/// Bound for "the rejected bytes produced no outbound datagram".
const QUIET_WINDOW: Duration = Duration::from_millis(750);

#[derive(Debug)]
struct InsecureCertVerifier;

impl rustls::client::danger::ServerCertVerifier for InsecureCertVerifier {
    fn verify_server_cert(
        &self,
        _end_entity: &rustls::pki_types::CertificateDer<'_>,
        _intermediates: &[rustls::pki_types::CertificateDer<'_>],
        _server_name: &rustls::pki_types::ServerName<'_>,
        _ocsp_response: &[u8],
        _now: rustls::pki_types::UnixTime,
    ) -> Result<rustls::client::danger::ServerCertVerified, rustls::Error> {
        Ok(rustls::client::danger::ServerCertVerified::assertion())
    }

    fn verify_tls12_signature(
        &self,
        _message: &[u8],
        _cert: &rustls::pki_types::CertificateDer<'_>,
        _dss: &rustls::DigitallySignedStruct,
    ) -> Result<rustls::client::danger::HandshakeSignatureValid, rustls::Error> {
        Ok(rustls::client::danger::HandshakeSignatureValid::assertion())
    }

    fn verify_tls13_signature(
        &self,
        _message: &[u8],
        _cert: &rustls::pki_types::CertificateDer<'_>,
        _dss: &rustls::DigitallySignedStruct,
    ) -> Result<rustls::client::danger::HandshakeSignatureValid, rustls::Error> {
        Ok(rustls::client::danger::HandshakeSignatureValid::assertion())
    }

    fn supported_verify_schemes(&self) -> Vec<rustls::SignatureScheme> {
        vec![
            rustls::SignatureScheme::RSA_PKCS1_SHA256,
            rustls::SignatureScheme::RSA_PKCS1_SHA384,
            rustls::SignatureScheme::RSA_PKCS1_SHA512,
            rustls::SignatureScheme::ECDSA_NISTP256_SHA256,
            rustls::SignatureScheme::ECDSA_NISTP384_SHA384,
            rustls::SignatureScheme::ECDSA_NISTP521_SHA512,
            rustls::SignatureScheme::RSA_PSS_SHA256,
            rustls::SignatureScheme::RSA_PSS_SHA384,
            rustls::SignatureScheme::RSA_PSS_SHA512,
            rustls::SignatureScheme::ED25519,
        ]
    }
}

/// Start a shoes custom-TLS listener whose `test.local` target enables VLESS/Vision.
///
/// This is the same `tls_targets` wire path that `with_shoes_vision_server` uses.
async fn start_custom_tls_vision_server() -> TestResult<(ProcessGuard, NamedTempFile, String, u16)>
{
    let mut ports = common::port_helper::PortHelper::new();
    let (ip, port) = ports.get_listener_port();
    let (cert_path, key_path) = generate_test_cert_files()?;
    let config = format!(
        r#"
- address: "{ip}:{port}"
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
    let (guard, config_file) = start_shoes_server(&config)?;
    ports.wait_for_all_ports().await?;
    Ok((guard, config_file, ip, port))
}

async fn start_udp_echo() -> TestResult<(TestServer, SocketAddr)> {
    let echo = start_udp_echo_server_with_suffix("127.0.0.1", 0, b"").await?;
    let address = echo.local_addr();
    Ok((echo, address))
}

async fn connect_custom_tls(
    ip: &str,
    port: u16,
) -> TestResult<tokio_rustls::client::TlsStream<TcpStream>> {
    let config = rustls::ClientConfig::builder()
        .dangerous()
        .with_custom_certificate_verifier(Arc::new(InsecureCertVerifier))
        .with_no_client_auth();
    let connector = tokio_rustls::TlsConnector::from(Arc::new(config));
    let server_name = rustls::pki_types::ServerName::try_from("test.local".to_string())?;
    let stream = TcpStream::connect((ip, port)).await?;
    Ok(connector.connect(server_name, stream).await?)
}

/// The non-Vision MUX branch answers with the VLESS response header before it reads frames.
async fn expect_vless_response_header<S>(stream: &mut S) -> TestResult
where
    S: AsyncRead + Unpin,
{
    let mut header = [0u8; 2];
    timeout(SCENARIO_TIMEOUT, stream.read_exact(&mut header)).await??;
    assert_eq!(header, [0u8, 0u8], "unexpected VLESS response header");
    Ok(())
}

async fn expect_server_rejection<S>(stream: &mut S) -> TestResult
where
    S: AsyncRead + Unpin,
{
    let mut buffer = [0u8; 64];
    let mut received = Vec::new();
    let deadline = Instant::now() + SCENARIO_TIMEOUT;
    loop {
        match timeout_at(deadline, stream.read(&mut buffer)).await {
            Err(_) => {
                return Err("server kept the malformed connection open".into());
            }
            Ok(Err(_)) => break,
            Ok(Ok(0)) => break,
            Ok(Ok(length)) => {
                received.extend_from_slice(&buffer[..length]);
                if received.len() > 4 {
                    return Err(format!(
                        "server sent {} unexpected bytes instead of closing",
                        received.len()
                    )
                    .into());
                }
            }
        }
    }
    assert!(
        received.is_empty(),
        "server answered the rejected request with {received:?}"
    );
    Ok(())
}

/// Errors that a server may legitimately produce while it rejects the request.
fn expected_rejection_error(error: &std::io::Error) -> bool {
    matches!(
        error.kind(),
        std::io::ErrorKind::BrokenPipe
            | std::io::ErrorKind::ConnectionReset
            | std::io::ErrorKind::ConnectionAborted
            | std::io::ErrorKind::NotConnected
            | std::io::ErrorKind::UnexpectedEof
            | std::io::ErrorKind::TimedOut
    )
}

async fn send_bytes<S>(stream: &mut S, bytes: &[u8]) -> TestResult
where
    S: AsyncWrite + Unpin,
{
    // The server may close the connection before the whole rejected request is
    // written. Only that expected early rejection is tolerated; any other write or
    // flush error is propagated instead of being silently discarded.
    if let Err(error) = stream.write_all(bytes).await
        && !expected_rejection_error(&error)
    {
        return Err(error.into());
    }
    if let Err(error) = stream.flush().await
        && !expected_rejection_error(&error)
    {
        return Err(error.into());
    }
    Ok(())
}

async fn expect_quiet_socket(socket: &UdpSocket) -> TestResult {
    let mut buffer = [0u8; 2048];
    match timeout(QUIET_WINDOW, socket.recv_from(&mut buffer)).await {
        Err(_) => Ok(()),
        Ok(Ok((length, peer))) => {
            Err(format!("rejected input forwarded {length} bytes to {peer}").into())
        }
        Ok(Err(error)) => Err(error.into()),
    }
}

/// Build a VLESS request whose addon envelope carries `addons` and a real destination.
///
/// The healthy request supplies the version, user id, command, and destination layout;
/// only the addon envelope is replaced.
fn malformed_addon_request(
    uuid: [u8; 16],
    destination: &VlessDestination,
    addons: &[u8],
    payload: &[u8],
) -> TestResult<Vec<u8>> {
    let healthy = encode_vless_udp_request(uuid, destination)?;
    let mut request = Vec::with_capacity(healthy.len() + addons.len() + payload.len() + 4);
    request.extend_from_slice(&healthy[..17]);
    request.push(u8::try_from(addons.len())?);
    request.extend_from_slice(addons);
    request.extend_from_slice(&healthy[18..]);
    request.extend_from_slice(&u16::try_from(payload.len())?.to_be_bytes());
    request.extend_from_slice(payload);
    Ok(request)
}

#[tokio::test]
async fn parser_boundary_rejects_malformed_vless_addons_on_custom_tls_vision_path() -> TestResult {
    timeout(SCENARIO_DEADLINE, vless_addon_rejection_scenario()).await?
}

async fn vless_addon_rejection_scenario() -> TestResult {
    let (_shoes, _shoes_config, ip, port) = start_custom_tls_vision_server().await?;
    let (_echo, echo_address) = start_udp_echo().await?;
    let uuid = parse_uuid(TEST_UUID)?;
    let observer = UdpSocket::bind("127.0.0.1:0").await?;
    let observed_destination = VlessDestination::Ip(observer.local_addr()?);

    // The prior defect: a one-byte continuation-only length varint panicked the reader.
    let mut truncated_field = connect_custom_tls(&ip, port).await?;
    let request = malformed_addon_request(
        uuid,
        &observed_destination,
        &[0x0a, 0x80],
        b"must-not-be-forwarded",
    )?;
    send_bytes(&mut truncated_field, &request).await?;
    expect_server_rejection(&mut truncated_field).await?;
    expect_quiet_socket(&observer).await?;

    // The declared field length is longer than the addon envelope, so the command and
    // destination bytes that follow must never be consumed as flow data.
    let mut envelope_overrun = connect_custom_tls(&ip, port).await?;
    let request = malformed_addon_request(
        uuid,
        &observed_destination,
        &[0x0a, 0x10],
        b"must-not-be-forwarded",
    )?;
    send_bytes(&mut envelope_overrun, &request).await?;
    expect_server_rejection(&mut envelope_overrun).await?;
    expect_quiet_socket(&observer).await?;

    // Healthy authenticated control on the same custom-TLS listener.
    let stream = connect_custom_tls(&ip, port).await?;
    let mut client =
        VlessUdpClient::connect(stream, uuid, VlessDestination::Ip(echo_address)).await?;
    client.send_packet(b"parser-boundary-healthy").await?;
    assert_eq!(
        client.recv_packet(Duration::from_secs(5)).await?,
        b"parser-boundary-healthy"
    );
    Ok(())
}

#[tokio::test]
async fn parser_boundary_rejects_malformed_xudp_metadata_on_authenticated_mux() -> TestResult {
    timeout(SCENARIO_DEADLINE, xudp_metadata_rejection_scenario()).await?
}

async fn xudp_metadata_rejection_scenario() -> TestResult {
    let (_shoes, _shoes_config, ip, port) = start_custom_tls_vision_server().await?;
    let (_echo, echo_address) = start_udp_echo().await?;
    let uuid = parse_uuid(TEST_UUID)?;
    let observer = UdpSocket::bind("127.0.0.1:0").await?;
    let observer_port = observer.local_addr()?.port();

    // Exact prior panic input: complete five-byte metadata without the UDP port.
    let mut missing_port = connect_custom_tls(&ip, port).await?;
    send_bytes(&mut missing_port, &encode_vless_xudp_request(uuid)).await?;
    expect_vless_response_header(&mut missing_port).await?;
    send_bytes(
        &mut missing_port,
        &[0x00, 0x05, 0x00, 0x01, 0x01, 0x00, 0x02],
    )
    .await?;
    expect_server_rejection(&mut missing_port).await?;
    expect_quiet_socket(&observer).await?;

    // The declared metadata ends inside the IPv4 address. The byte after the envelope
    // would complete 127.0.0.1:observer_port if a parser read past the declared slice.
    let mut borrowed_address = connect_custom_tls(&ip, port).await?;
    send_bytes(&mut borrowed_address, &encode_vless_xudp_request(uuid)).await?;
    expect_vless_response_header(&mut borrowed_address).await?;
    let mut frame = Vec::new();
    frame.extend_from_slice(&0x000bu16.to_be_bytes());
    frame.extend_from_slice(&0x0002u16.to_be_bytes());
    frame.extend_from_slice(&[0x01, 0x00, 0x02]);
    frame.extend_from_slice(&observer_port.to_be_bytes());
    frame.extend_from_slice(&[0x01, 127, 0, 0]);
    frame.push(1);
    send_bytes(&mut borrowed_address, &frame).await?;
    expect_server_rejection(&mut borrowed_address).await?;
    expect_quiet_socket(&observer).await?;

    // Healthy authenticated MUX control on the same listener.
    let stream = connect_custom_tls(&ip, port).await?;
    let mut client = XudpClient::connect(stream, uuid).await?;
    client
        .send_frame(&XudpFrame {
            session_id: 7,
            status: XUDP_STATUS_NEW,
            options: XUDP_OPTION_DATA,
            destination: Some(VlessDestination::Ip(echo_address)),
            payload: Some(b"parser-boundary-mux".to_vec()),
        })
        .await?;
    let response = client.recv_frame(Duration::from_secs(5)).await?;
    assert_eq!(
        response.payload.as_deref(),
        Some(&b"parser-boundary-mux"[..])
    );
    Ok(())
}
