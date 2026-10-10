use std::sync::Arc;

use async_trait::async_trait;
use log::debug;
use subtle::ConstantTimeEq;
use tokio::io::AsyncWriteExt;

use crate::address::{Address, NetLocation};
use crate::async_stream::AsyncStream;
use crate::client_proxy_selector::ClientProxySelector;
use crate::crypto::CryptoTlsStream;
use crate::h2mux::{MUX_DESTINATION_HOST, MUX_DESTINATION_PORT, handle_h2mux_session};
use crate::resolver::Resolver;
use crate::stream_reader::StreamReader;
use crate::tcp::tcp_handler::{TcpServerHandler, TcpServerSetupResult};
use crate::util::write_all;
use crate::uuid_util::parse_uuid;
use crate::xudp::XudpMessageStream;

use super::vision_stream::VisionStream;
use super::vless_message_stream::VlessMessageStream;
use super::vless_util::{
    COMMAND_MUX, COMMAND_TCP, COMMAND_UDP, XTLS_VISION_FLOW, parse_addons_from_reader,
    parse_remote_location_from_reader,
};

pub struct VlessTcpServerHandler {
    user_id: Box<[u8]>,
    udp_enabled: bool,
    proxy_selector: Arc<ClientProxySelector>,
    resolver: Arc<dyn Resolver>,
    fallback: Option<NetLocation>,
}

impl std::fmt::Debug for VlessTcpServerHandler {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("VlessTcpServerHandler")
            .field("user_id", &self.user_id)
            .field("udp_enabled", &self.udp_enabled)
            .field("fallback", &self.fallback)
            .finish()
    }
}

impl VlessTcpServerHandler {
    pub fn new(
        user_id: &str,
        udp_enabled: bool,
        proxy_selector: Arc<ClientProxySelector>,
        resolver: Arc<dyn Resolver>,
        fallback: Option<NetLocation>,
    ) -> Self {
        Self {
            user_id: parse_uuid(user_id).unwrap().into_boxed_slice(),
            udp_enabled,
            proxy_selector,
            resolver,
            fallback,
        }
    }
}

const SERVER_RESPONSE_HEADER: &[u8] = &[
    0u8, // version
    0u8, // addons length
];

/// Forward the connection to a fallback destination when VLESS authentication fails.
///
/// This makes the server indistinguishable from a legitimate server by transparently
/// proxying failed auth attempts to the configured fallback destination.
///
/// Used by both `VlessTcpServerHandler` and `setup_custom_tls_vision_vless_server_stream`.
async fn vless_fallback_to_dest<S: AsyncStream + 'static>(
    client_stream: S,
    reader: StreamReader,
    fallback: &NetLocation,
    resolver: &Arc<dyn Resolver>,
) -> std::io::Result<TcpServerSetupResult> {
    debug!("VLESS FALLBACK: Connecting to fallback: {}", fallback);

    let unconsumed_data = reader.unparsed_data();
    let dest_addr = crate::resolver::resolve_single_address(resolver, fallback).await?;

    debug!("VLESS FALLBACK: Resolved {} to {}", fallback, dest_addr);

    let socket = crate::socket_util::new_tcp_socket(None, dest_addr.is_ipv6())?;
    let mut dest_stream: Box<dyn AsyncStream> = Box::new(socket.connect(dest_addr).await?);

    debug!(
        "VLESS FALLBACK: Connected to fallback, forwarding {} bytes",
        unconsumed_data.len()
    );

    if !unconsumed_data.is_empty() {
        write_all(&mut dest_stream, unconsumed_data).await?;
        dest_stream.flush().await?;
    }

    debug!("VLESS FALLBACK: Spawning bidirectional copy");

    // Return the transfer to the caller so cancellation owns the entire connection.
    Ok(TcpServerSetupResult::Session(Box::pin(async move {
        let mut client_stream = client_stream;
        let result = crate::copy_bidirectional::copy_bidirectional(
            &mut client_stream,
            &mut *dest_stream,
            false, // client doesn't need initial flush
            false, // dest doesn't need initial flush
        )
        .await;

        futures::join!(
            crate::util::shutdown_stream(&mut client_stream),
            crate::util::shutdown_stream(&mut dest_stream),
        );

        if let Err(e) = result {
            debug!("VLESS FALLBACK: Connection ended: {}", e);
        } else {
            debug!("VLESS FALLBACK: Connection completed");
        }
    })))
}

#[async_trait]
impl TcpServerHandler for VlessTcpServerHandler {
    async fn setup_server_stream(
        &self,
        mut server_stream: Box<dyn AsyncStream>,
    ) -> std::io::Result<TcpServerSetupResult> {
        let mut stream_reader = StreamReader::new_with_buffer_size(800);

        let client_version = stream_reader.peek_u8(&mut server_stream).await?;
        if client_version != 0 {
            debug!("VLESS version mismatch: expected 0, got {}", client_version);
            if let Some(ref fallback) = self.fallback {
                return vless_fallback_to_dest(
                    server_stream,
                    stream_reader,
                    fallback,
                    &self.resolver,
                )
                .await;
            }
            return Err(std::io::Error::other(format!(
                "invalid client protocol version, expected 0, got {client_version}"
            )));
        }

        let header = stream_reader.peek_slice(&mut server_stream, 17).await?;
        let target_id = &header[1..17];

        if self.user_id.ct_eq(target_id).unwrap_u8() == 0 {
            debug!("VLESS UUID mismatch");
            if let Some(ref fallback) = self.fallback {
                return vless_fallback_to_dest(
                    server_stream,
                    stream_reader,
                    fallback,
                    &self.resolver,
                )
                .await;
            }
            return Err(std::io::Error::other("Unknown user id"));
        }

        stream_reader.consume(17);

        let addon_length = stream_reader.read_u8(&mut server_stream).await?;
        if addon_length > 0 {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                "VLESS addons not supported in current configuration, use TLS protocol config for VISION support",
            ));
        }

        let instruction = stream_reader.read_u8(&mut server_stream).await?;

        match instruction {
            COMMAND_TCP => {
                let remote_location =
                    parse_remote_location_from_reader(&mut stream_reader, &mut server_stream)
                        .await?;

                // Check for h2mux magic destination
                if let Address::Hostname(host) = remote_location.address()
                    && host == MUX_DESTINATION_HOST
                    && remote_location.port() == MUX_DESTINATION_PORT
                {
                    // Send VLESS success response before spawning h2mux session
                    write_all(&mut server_stream, SERVER_RESPONSE_HEADER).await?;

                    let proxy_selector = self.proxy_selector.clone();
                    let resolver = self.resolver.clone();
                    let udp_enabled = self.udp_enabled;

                    // Pass any unparsed data for the h2mux session
                    let initial_data = stream_reader.unparsed_data_owned();

                    return Ok(TcpServerSetupResult::Session(Box::pin(async move {
                        if let Err(e) = handle_h2mux_session(
                            server_stream,
                            initial_data,
                            udp_enabled,
                            proxy_selector,
                            resolver,
                        )
                        .await
                        {
                            debug!("H2MUX session ended: {}", e);
                        }
                    })));
                }

                let unparsed_data = stream_reader.unparsed_data();

                Ok(TcpServerSetupResult::TcpForward {
                    remote_location,
                    stream: server_stream,
                    need_initial_flush: false,
                    connection_success_response: Some(
                        SERVER_RESPONSE_HEADER.to_vec().into_boxed_slice(),
                    ),
                    initial_remote_data: if unparsed_data.is_empty() {
                        None
                    } else {
                        Some(unparsed_data.to_vec().into_boxed_slice())
                    },
                    proxy_selector: self.proxy_selector.clone(),
                })
            }
            COMMAND_UDP => {
                if !self.udp_enabled {
                    return Err(std::io::Error::new(
                        std::io::ErrorKind::InvalidData,
                        "UDP not enabled",
                    ));
                }

                let remote_location =
                    parse_remote_location_from_reader(&mut stream_reader, &mut server_stream)
                        .await?;
                let unparsed_data = stream_reader.unparsed_data();

                write_all(&mut server_stream, SERVER_RESPONSE_HEADER).await?;
                let mut vless_stream = VlessMessageStream::new(server_stream);
                if !unparsed_data.is_empty() {
                    vless_stream.feed_initial_read_data(unparsed_data)?;
                }

                Ok(TcpServerSetupResult::BidirectionalUdp {
                    remote_location,
                    stream: Box::new(vless_stream),
                    need_initial_flush: false,
                    proxy_selector: self.proxy_selector.clone(),
                })
            }
            COMMAND_MUX => {
                if !self.udp_enabled {
                    return Err(std::io::Error::new(
                        std::io::ErrorKind::InvalidData,
                        "MUX/XUDP requires UDP to be enabled",
                    ));
                }

                // MUX/XUDP: Destination comes in XUDP frames, not VLESS header
                let unparsed_data = stream_reader.unparsed_data();
                write_all(&mut server_stream, SERVER_RESPONSE_HEADER).await?;
                let mut xudp_stream =
                    XudpMessageStream::new_with_resolver(server_stream, self.resolver.clone());
                if !unparsed_data.is_empty() {
                    xudp_stream.feed_initial_read_data(unparsed_data)?;
                }

                Ok(TcpServerSetupResult::SessionBasedUdp {
                    stream: Box::new(xudp_stream),
                    need_initial_flush: false,
                    proxy_selector: self.proxy_selector.clone(),
                })
            }
            unknown_protocol_type => {
                return Err(std::io::Error::new(
                    std::io::ErrorKind::InvalidData,
                    format!("Unknown requested protocol: {unknown_protocol_type}"),
                ));
            }
        }
    }
}

/// Setup a VISION+VLESS stream from a CryptoTlsStream (for REALITY+Vision support)
pub async fn setup_custom_tls_vision_vless_server_stream<IO>(
    mut tls_stream: CryptoTlsStream<IO>,
    user_id: &[u8],
    udp_enabled: bool,
    proxy_selector: Arc<ClientProxySelector>,
    resolver: &Arc<dyn Resolver>,
    fallback: Option<NetLocation>,
) -> std::io::Result<TcpServerSetupResult>
where
    IO: AsyncStream + 'static,
{
    tls_stream.require_record_framing()?;
    let mut stream_reader = StreamReader::new_with_buffer_size(800);

    let client_version = stream_reader.peek_u8(&mut tls_stream).await?;
    if client_version != 0 {
        debug!(
            "VLESS/Vision version mismatch: expected 0, got {}",
            client_version
        );
        if let Some(ref fb) = fallback {
            tls_stream.allow_streaming_reads();
            return vless_fallback_to_dest(tls_stream, stream_reader, fb, resolver).await;
        }
        return Err(std::io::Error::other(format!(
            "invalid client protocol version, expected 0, got {client_version}"
        )));
    }

    let header = stream_reader.peek_slice(&mut tls_stream, 17).await?;
    let target_id = &header[1..17];

    // Verify user ID using constant-time comparison to prevent timing attacks
    if user_id.ct_eq(target_id).unwrap_u8() == 0 {
        debug!("VLESS/Vision UUID mismatch");
        if let Some(ref fb) = fallback {
            tls_stream.allow_streaming_reads();
            return vless_fallback_to_dest(tls_stream, stream_reader, fb, resolver).await;
        }
        return Err(std::io::Error::other("Unknown user id"));
    }

    // Both checks passed - copy UUID for VisionStream, then consume version + UUID
    let mut user_uuid = [0u8; 16];
    user_uuid.copy_from_slice(target_id);
    stream_reader.consume(17);

    let addon_length = stream_reader.read_u8(&mut tls_stream).await?;
    let flow = if addon_length > 0 {
        parse_addons_from_reader(&mut stream_reader, &mut tls_stream, addon_length).await?
    } else {
        String::new()
    };

    let instruction = stream_reader.read_u8(&mut tls_stream).await?;

    match instruction {
        COMMAND_TCP => {
            if flow != XTLS_VISION_FLOW {
                return Err(std::io::Error::other("expected vision flow for TCP"));
            }

            debug!("Parsing remote location...");
            let remote_location =
                parse_remote_location_from_reader(&mut stream_reader, &mut tls_stream).await?;
            debug!("Remote location parsed: {}", remote_location);
            let unparsed_data = stream_reader.unparsed_data();

            debug!("Creating VISION stream (Custom TLS) for flow: {}", flow);
            let vision_stream = VisionStream::new_server(tls_stream, user_uuid, unparsed_data)?;

            Ok(TcpServerSetupResult::TcpForward {
                remote_location,
                stream: Box::new(vision_stream),
                need_initial_flush: false,
                connection_success_response: None, // VisionStream will send VLESS response with first write
                initial_remote_data: None,         // Data fed to VisionStream instead
                proxy_selector: proxy_selector.clone(),
            })
        }
        COMMAND_UDP => {
            if !udp_enabled {
                return Err(std::io::Error::new(
                    std::io::ErrorKind::InvalidData,
                    "UDP not enabled",
                ));
            }

            debug!("Parsing remote location...");
            let remote_location =
                parse_remote_location_from_reader(&mut stream_reader, &mut tls_stream).await?;
            debug!("Remote location parsed: {}", remote_location);
            let unparsed_data = stream_reader.unparsed_data();

            tls_stream.allow_streaming_reads();
            write_all(&mut tls_stream, SERVER_RESPONSE_HEADER).await?;
            let mut vless_stream = VlessMessageStream::new(tls_stream);
            if !unparsed_data.is_empty() {
                vless_stream.feed_initial_read_data(unparsed_data)?;
            }

            Ok(TcpServerSetupResult::BidirectionalUdp {
                remote_location,
                stream: Box::new(vless_stream),
                need_initial_flush: false,
                proxy_selector: proxy_selector.clone(),
            })
        }
        COMMAND_MUX => {
            if !udp_enabled {
                return Err(std::io::Error::new(
                    std::io::ErrorKind::InvalidData,
                    "MUX/XUDP requires UDP to be enabled",
                ));
            }
            // MUX/XUDP: Destination is NOT in the VLESS header - it comes in XUDP frames
            debug!("MUX/XUDP: No destination in VLESS header (destinations come in XUDP frames)");
            let unparsed_data = stream_reader.unparsed_data();

            if flow == XTLS_VISION_FLOW {
                debug!("Creating VISION+XUDP stream (Custom TLS) with session-based UDP sockets");

                // Create VISION stream (will send VLESS response automatically on first write)
                let vision_stream = VisionStream::new_server(tls_stream, user_uuid, unparsed_data)?;

                // Wrap VISION stream in XUDP stream
                let xudp_stream =
                    XudpMessageStream::new_with_resolver(Box::new(vision_stream), resolver.clone());

                Ok(TcpServerSetupResult::SessionBasedUdp {
                    stream: Box::new(xudp_stream),
                    need_initial_flush: false, // VisionStream sends VLESS response on first write
                    proxy_selector: proxy_selector.clone(),
                })
            } else {
                debug!(
                    "Creating XUDP stream (Custom TLS, no VISION) with session-based UDP sockets"
                );

                tls_stream.allow_streaming_reads();
                // Send VLESS response header immediately
                write_all(&mut tls_stream, SERVER_RESPONSE_HEADER).await?;

                // Wrap TLS stream in XUDP stream
                let mut xudp_stream =
                    XudpMessageStream::new_with_resolver(Box::new(tls_stream), resolver.clone());

                // Feed any unparsed data to XUDP stream
                if !unparsed_data.is_empty() {
                    xudp_stream.feed_initial_read_data(unparsed_data)?;
                }

                Ok(TcpServerSetupResult::SessionBasedUdp {
                    stream: Box::new(xudp_stream),
                    need_initial_flush: false, // Response already sent above
                    proxy_selector,
                })
            }
        }
        unknown_protocol_type => Err(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            format!("Unknown requested protocol: {unknown_protocol_type}"),
        )),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr};
    use std::pin::Pin;
    use std::sync::atomic::{AtomicUsize, Ordering};

    use bytes::{BufMut, BytesMut};
    use futures::future::poll_fn;
    use tokio::io::ReadBuf;

    use crate::crypto::{CryptoConnection, TlsReadMode};
    use crate::xudp::frame::{FrameMetadata, FrameOption, SessionStatus, TargetNetwork};

    /// A hostname that only the injected resolver answers. The reserved TLD keeps it
    /// unreachable outside this fixture.
    const SYNTHETIC_XUDP_HOSTNAME: &str = "xudp-resolver.invalid";
    const SYNTHETIC_UUID: &str = "550e8400-e29b-41d4-a716-446655440000";

    /// Answers the synthetic hostname with one address and counts every lookup.
    #[derive(Debug)]
    struct CountingResolver {
        answer: IpAddr,
        calls: Arc<AtomicUsize>,
    }

    impl Resolver for CountingResolver {
        fn resolve_location(
            &self,
            location: &NetLocation,
        ) -> std::pin::Pin<
            Box<dyn std::future::Future<Output = std::io::Result<Vec<SocketAddr>>> + Send>,
        > {
            self.calls.fetch_add(1, Ordering::SeqCst);
            let resolved = match location.address() {
                Address::Hostname(name) if name == SYNTHETIC_XUDP_HOSTNAME => {
                    Ok(vec![SocketAddr::new(self.answer, location.port())])
                }
                other => Err(std::io::Error::new(
                    std::io::ErrorKind::NotFound,
                    format!("synthetic resolver has no mapping for {other:?}"),
                )),
            };
            Box::pin(async move { resolved })
        }
    }

    fn counting_resolver(answer: IpAddr) -> (Arc<dyn Resolver>, Arc<AtomicUsize>) {
        let calls = Arc::new(AtomicUsize::new(0));
        (
            Arc::new(CountingResolver {
                answer,
                calls: calls.clone(),
            }),
            calls,
        )
    }

    fn synthetic_user_id() -> Vec<u8> {
        crate::uuid_util::parse_uuid(SYNTHETIC_UUID).unwrap()
    }

    fn hostname_destination(hostname: &str, port: u16) -> NetLocation {
        NetLocation::new(Address::from(hostname).unwrap(), port)
    }

    /// One XUDP New frame carrying `payload` for `destination`.
    fn xudp_new_frame(session_id: u16, destination: &NetLocation, payload: &[u8]) -> Vec<u8> {
        let metadata = FrameMetadata {
            session_id,
            status: SessionStatus::New,
            option: FrameOption::new().with_data(),
            target: Some(destination.clone()),
            network: Some(TargetNetwork::Udp),
        };
        let mut buffer = BytesMut::new();
        metadata.encode(&mut buffer).unwrap();
        buffer.put_u16(payload.len() as u16);
        buffer.extend_from_slice(payload);
        buffer.to_vec()
    }

    /// A VLESS header for COMMAND_MUX followed by the XUDP frames.
    fn vless_mux_request(user_id: &[u8], flow: Option<&str>, frame: &[u8]) -> Vec<u8> {
        let mut request = vec![0u8];
        request.extend_from_slice(user_id);
        match flow {
            Some(flow) => {
                let mut addons = vec![0x0a, flow.len() as u8];
                addons.extend_from_slice(flow.as_bytes());
                request.push(addons.len() as u8);
                request.extend_from_slice(&addons);
            }
            None => request.push(0),
        }
        request.push(3); // COMMAND_MUX
        request.extend_from_slice(frame);
        request
    }

    /// Reads one routed session message from a server XUDP stream.
    async fn read_routed(
        stream: &mut dyn crate::async_stream::AsyncSessionMessageStream,
        payload_len: usize,
    ) -> (SocketAddr, Vec<u8>) {
        let mut buffer = vec![0u8; payload_len];
        let mut read_buf = ReadBuf::new(&mut buffer);
        let (_session, address) =
            poll_fn(|cx| Pin::new(&mut *stream).poll_read_session_message(cx, &mut read_buf))
                .await
                .unwrap();
        (address, read_buf.filled().to_vec())
    }

    /// A completed TLS 1.3 pair over one duplex. The server side keeps record framing,
    /// which is what the custom-TLS VLESS setup paths require.
    async fn tls13_pair() -> (
        CryptoTlsStream<tokio::io::DuplexStream>,
        CryptoTlsStream<tokio::io::DuplexStream>,
    ) {
        let cert =
            rcgen::generate_simple_self_signed(vec![SYNTHETIC_XUDP_HOSTNAME.into()]).unwrap();
        let server_config =
            rustls::ServerConfig::builder_with_protocol_versions(&[&rustls::version::TLS13])
                .with_no_client_auth()
                .with_single_cert(
                    vec![cert.cert.der().clone()],
                    rustls::pki_types::PrivatePkcs8KeyDer::from(cert.signing_key.serialize_der())
                        .into(),
                )
                .unwrap();
        let mut roots = rustls::RootCertStore::empty();
        roots.add(cert.cert.der().clone()).unwrap();
        let client_config =
            rustls::ClientConfig::builder_with_protocol_versions(&[&rustls::version::TLS13])
                .with_root_certificates(roots)
                .with_no_client_auth();
        let (server_io, client_io) = tokio::io::duplex(65540);
        let server = CryptoConnection::new_rustls_server(
            rustls::ServerConnection::new(Arc::new(server_config)).unwrap(),
        );
        let client = CryptoConnection::new_rustls_client(
            rustls::ClientConnection::new(
                Arc::new(client_config),
                SYNTHETIC_XUDP_HOSTNAME.try_into().unwrap(),
            )
            .unwrap(),
        );
        let (server_tls, client_tls) = tokio::join!(
            CryptoTlsStream::handshake(server_io, server, TlsReadMode::PreserveRecords, &[]),
            CryptoTlsStream::handshake(client_io, client, TlsReadMode::Stream, &[]),
        );
        (server_tls.unwrap(), client_tls.unwrap())
    }

    /// The plain server path must resolve hostname frames through its configured
    /// resolver instead of the hard-coded native one.
    #[tokio::test]
    async fn plain_server_xudp_resolves_through_the_configured_resolver() {
        let (resolver, calls) = counting_resolver(IpAddr::V4(Ipv4Addr::LOCALHOST));
        let destination = hostname_destination(SYNTHETIC_XUDP_HOSTNAME, 5353);
        let frame = xudp_new_frame(10, &destination, b"x");
        let request = vless_mux_request(&synthetic_user_id(), None, &frame);

        let (io, mut peer) = tokio::io::duplex(65540);
        peer.write_all(&request).await.unwrap();
        let handler = VlessTcpServerHandler::new(
            SYNTHETIC_UUID,
            true,
            Arc::new(ClientProxySelector::new(Vec::new())),
            resolver,
            None,
        );
        let result = handler.setup_server_stream(Box::new(io)).await.unwrap();
        let TcpServerSetupResult::SessionBasedUdp { mut stream, .. } = result else {
            panic!("COMMAND_MUX must return a session-based UDP stream");
        };

        let (address, payload) = read_routed(&mut *stream, 1).await;
        assert_eq!(payload, b"x");
        assert_eq!(
            address,
            SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), 5353)
        );
        assert_eq!(
            calls.load(Ordering::SeqCst),
            1,
            "the hostname frame must resolve through the configured resolver"
        );
    }

    /// The VISION custom-TLS path must use the configured resolver too.
    #[tokio::test]
    async fn vision_custom_tls_xudp_resolves_through_the_configured_resolver() {
        let (resolver, calls) = counting_resolver(IpAddr::V6(Ipv6Addr::LOCALHOST));
        let destination = hostname_destination(SYNTHETIC_XUDP_HOSTNAME, 5353);
        let frame = xudp_new_frame(11, &destination, b"y");
        let request = vless_mux_request(&synthetic_user_id(), Some("xtls-rprx-vision"), &frame);

        let (server_tls, mut client_tls) = tls13_pair().await;
        client_tls.write_all(&request).await.unwrap();
        client_tls.flush().await.unwrap();
        let user_id = synthetic_user_id();

        let result = setup_custom_tls_vision_vless_server_stream(
            server_tls,
            &user_id,
            true,
            Arc::new(ClientProxySelector::new(Vec::new())),
            &resolver,
            None,
        )
        .await
        .unwrap();
        let TcpServerSetupResult::SessionBasedUdp { mut stream, .. } = result else {
            panic!("VISION + COMMAND_MUX must return a session-based UDP stream");
        };

        let (address, payload) = read_routed(&mut *stream, 1).await;
        assert_eq!(payload, b"y");
        assert_eq!(
            address,
            SocketAddr::new(IpAddr::V6(Ipv6Addr::LOCALHOST), 5353)
        );
        assert_eq!(calls.load(Ordering::SeqCst), 1);
    }

    /// The non-VISION custom-TLS path must use the configured resolver too.
    #[tokio::test]
    async fn custom_tls_xudp_resolves_through_the_configured_resolver() {
        let (resolver, calls) = counting_resolver(IpAddr::V4(Ipv4Addr::LOCALHOST));
        let destination = hostname_destination(SYNTHETIC_XUDP_HOSTNAME, 5353);
        let frame = xudp_new_frame(12, &destination, b"z");
        let request = vless_mux_request(&synthetic_user_id(), None, &frame);

        let (server_tls, mut client_tls) = tls13_pair().await;
        client_tls.write_all(&request).await.unwrap();
        client_tls.flush().await.unwrap();
        let user_id = synthetic_user_id();

        let result = setup_custom_tls_vision_vless_server_stream(
            server_tls,
            &user_id,
            true,
            Arc::new(ClientProxySelector::new(Vec::new())),
            &resolver,
            None,
        )
        .await
        .unwrap();
        let TcpServerSetupResult::SessionBasedUdp { mut stream, .. } = result else {
            panic!("custom TLS + COMMAND_MUX must return a session-based UDP stream");
        };

        let (address, payload) = read_routed(&mut *stream, 1).await;
        assert_eq!(payload, b"z");
        assert_eq!(
            address,
            SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), 5353)
        );
        assert_eq!(calls.load(Ordering::SeqCst), 1);
    }

    /// Literal destinations must stay exactly as the client sent them, with no lookup.
    #[tokio::test]
    async fn literal_ip_frames_keep_their_destination_without_resolution() {
        let (resolver, calls) = counting_resolver(IpAddr::V4(Ipv4Addr::LOCALHOST));
        let v4 = NetLocation::new(Address::Ipv4(Ipv4Addr::new(203, 0, 113, 7)), 5353);
        let v6 = NetLocation::new(Address::Ipv6(Ipv6Addr::LOCALHOST), 5354);
        let mut frame = xudp_new_frame(13, &v4, b"4");
        frame.extend_from_slice(&xudp_new_frame(14, &v6, b"6"));
        let request = vless_mux_request(&synthetic_user_id(), None, &frame);

        let (io, mut peer) = tokio::io::duplex(65540);
        peer.write_all(&request).await.unwrap();
        let handler = VlessTcpServerHandler::new(
            SYNTHETIC_UUID,
            true,
            Arc::new(ClientProxySelector::new(Vec::new())),
            resolver,
            None,
        );
        let result = handler.setup_server_stream(Box::new(io)).await.unwrap();
        let TcpServerSetupResult::SessionBasedUdp { mut stream, .. } = result else {
            panic!("COMMAND_MUX must return a session-based UDP stream");
        };

        let (first, first_payload) = read_routed(&mut *stream, 1).await;
        assert_eq!(first_payload, b"4");
        assert_eq!(
            first,
            SocketAddr::new(IpAddr::V4(Ipv4Addr::new(203, 0, 113, 7)), 5353)
        );
        let (second, second_payload) = read_routed(&mut *stream, 1).await;
        assert_eq!(second_payload, b"6");
        assert_eq!(
            second,
            SocketAddr::new(IpAddr::V6(Ipv6Addr::LOCALHOST), 5354)
        );
        assert_eq!(
            calls.load(Ordering::SeqCst),
            0,
            "literal destinations must not consult the resolver"
        );
    }
}
