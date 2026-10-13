use std::sync::Arc;
use std::time::Duration;

use bytes::Bytes;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::time::{Instant, timeout_at};

use crate::address::NetLocation;
use crate::async_stream::AsyncStream;
use crate::client_proxy_chain::ClientProxyChain;
use crate::client_proxy_selector::ClientProxySelector;
use crate::crypto::tls_deframer::TlsDeframer;
use crate::crypto::{CryptoConnection, CryptoTlsStream, TlsReadMode};
use crate::prepend_stream::PrependStream;
use crate::resolver::Resolver;
use crate::tcp::tcp_handler::{TcpClientSetupResult, TcpServerSetupResult};
use crate::tls_hello::{ParsedClientHello, parse_server_hello};
use crate::tls_server_handler::InnerProtocol;
use crate::util::{allocate_vec, write_all};

use super::common::CONTENT_TYPE_APPLICATION_DATA;
use super::fallback_rate_limit::{FallbackLimits, limit_writer};
use super::{RealityServerConfig, RealityServerConnection};
use crate::config::server::FallbackRateLimit;

#[derive(Debug)]
pub struct RealityServerTarget {
    pub limit_fallback_upload: Option<FallbackRateLimit>,
    pub limit_fallback_download: Option<FallbackRateLimit>,
    pub private_key: [u8; 32],
    pub short_ids: Vec<[u8; 8]>,
    pub dest: NetLocation,
    pub max_time_diff: Option<u64>, // in milliseconds
    pub min_client_version: Option<[u8; 3]>,
    pub max_client_version: Option<[u8; 3]>,
    pub cipher_suites: Vec<super::CipherSuite>,
    /// The effective proxy selector for this REALITY target.
    /// For Vision mode, this is passed to the VLESS setup function.
    /// Inner handler already has this selector from construction.
    pub effective_selector: Arc<ClientProxySelector>,
    /// What to do after REALITY termination - normal handler or Vision VLESS
    pub inner_protocol: InnerProtocol,
    /// Client chain for connecting to dest server (for fallback connections).
    pub dest_client_chain: ClientProxyChain,
}

impl RealityServerTarget {
    fn fallback_limits(&self, initial_upload: usize) -> FallbackLimits {
        FallbackLimits {
            upload: self.limit_fallback_upload,
            download: self.limit_fallback_download,
            initial_upload: initial_upload as u64,
        }
    }
}

/// Set up REALITY server stream with real-time mirroring for anti-probing.
///
/// Connect to dest IMMEDIATELY before auth processing, making timing
/// indistinguishable from a real reverse proxy. This defeats active probing.
///
/// Flow:
/// - Connect to dest immediately
/// - Forward ClientHello immediately (starts dest's handshake)
/// - Validate auth (fast, ~1ms, while dest is processing)
/// - Read dest's response (it's been processing in parallel)
/// - Branch based on auth:
///   - Auth failed: forward dest's response, continue bidirectional copy
///   - Auth succeeded: build REALITY response matching dest's structure
///
/// A cover that refuses, closes, or stalls stops the connection. A readable
/// answer that cannot back a REALITY handshake (a TLS 1.2 ServerHello, an alert,
/// or a HelloRetryRequest) is a cover response: it is relayed verbatim for
/// unauthenticated traffic, while an authenticated request stops.
#[inline]
pub async fn setup_reality_server_stream(
    server_stream: Box<dyn AsyncStream>,
    target: &RealityServerTarget,
    parsed_client_hello: ParsedClientHello,
    resolver: &Arc<dyn Resolver>,
) -> std::io::Result<TcpServerSetupResult> {
    let client_hello_frame = &parsed_client_hello.client_hello_frame;
    let client_pending = Bytes::copy_from_slice(parsed_client_hello.client_reader.unparsed_data());
    log::debug!(
        "REALITY ClientHello frame length: {}",
        client_hello_frame.len()
    );

    // Connect to dest before auth processing to minimize timing differences
    let TcpClientSetupResult {
        client_stream: mut dest_stream,
        early_data,
    } = target
        .dest_client_chain
        .connect_tcp(target.dest.clone().into(), resolver)
        .await
        .map_err(|e| {
            std::io::Error::new(
                std::io::ErrorKind::ConnectionRefused,
                format!("REALITY: Failed to connect to dest {}: {}", target.dest, e),
            )
        })?;

    debug_assert!(
        early_data.is_none(),
        "unexpected early_data from dest connection"
    );

    log::debug!(
        "REALITY: Connected to dest {}, forwarding ClientHello ({} bytes)",
        target.dest,
        client_hello_frame.len()
    );

    write_all(&mut dest_stream, client_hello_frame).await?;
    dest_stream.flush().await?;

    // Validate auth while dest processes the ClientHello. We don't short circuit on
    // permission denied before the read loop below so that the timing is always the
    // same. A client without TLS 1.3 cannot be authenticated at all; it stays
    // unauthenticated traffic that may relay through a usable cover.
    let authenticated_connection = if parsed_client_hello.supports_tls13 {
        let mut reality_conn = RealityServerConnection::new(RealityServerConfig {
            private_key: target.private_key,
            short_ids: target.short_ids.clone(),
            dest: target.dest.clone(),
            max_time_diff: target.max_time_diff,
            min_client_version: target.min_client_version,
            max_client_version: target.max_client_version,
            cipher_suites: target.cipher_suites.clone(),
        })?;
        match reality_conn.validate_client_hello(client_hello_frame) {
            Ok(()) => Some(reality_conn),
            Err(e) if e.kind() == std::io::ErrorKind::PermissionDenied => {
                log::warn!(
                    "REALITY: Auth failed ({}), treating as unauthenticated traffic",
                    e
                );
                None
            }
            Err(e) => {
                log::error!("REALITY: Unexpected error during auth: {}", e);
                return Err(e);
            }
        }
    } else {
        log::warn!("REALITY: Client does not support TLS 1.3, treating as unauthenticated traffic");
        None
    };

    // Read the cover response until it is usable for mirroring. Use the 512-byte
    // heuristic like XTLS/REALITY: first encrypted record > 512 bytes = combined
    // mode, <= 512 bytes = separate mode. A cover failure below (close, reset,
    // stall, malformed records) stops the connection instead of starting
    // transparent forwarding; a readable but unusable response follows the
    // authentication state.
    let deadline = Instant::now() + Duration::from_secs(5);
    let mut deframer = TlsDeframer::new();
    let mut dest_records: Vec<Bytes> = Vec::new();
    let mut buf = allocate_vec(8192).into_boxed_slice();

    loop {
        let new_records = match timeout_at(deadline, dest_stream.read(&mut buf)).await {
            Ok(Ok(0)) => {
                return Err(std::io::Error::new(
                    std::io::ErrorKind::UnexpectedEof,
                    "REALITY: Dest connection closed during TLS handshake",
                ));
            }
            Ok(Ok(n)) => {
                deframer.feed(&buf[..n]);
                deframer.next_records().map_err(|e| {
                    std::io::Error::new(
                        std::io::ErrorKind::InvalidData,
                        format!("REALITY: Malformed dest record: {}", e),
                    )
                })?
            }
            Ok(Err(e)) => {
                return Err(std::io::Error::new(
                    e.kind(),
                    format!("REALITY: Error reading from dest: {}", e),
                ));
            }
            Err(_) => {
                return Err(std::io::Error::new(
                    std::io::ErrorKind::TimedOut,
                    "REALITY: Timed out waiting for the dest TLS response",
                ));
            }
        };

        // When we get the first record, check that the cover answered a usable
        // ServerHello. A readable answer that is not one is still a cover response.
        if dest_records.is_empty() && !new_records.is_empty() {
            let parsed = match parse_server_hello(&new_records[0]) {
                Ok(parsed) => parsed,
                Err(e) => {
                    // A readable first record that is not a ServerHello (a cover alert
                    // or HelloRetryRequest) is a cover response, not a cover failure:
                    // unauthenticated traffic forwards the already-read bytes, while
                    // an authenticated request needs the TLS 1.3 structure and stops.
                    if authenticated_connection.is_some() {
                        return Err(std::io::Error::new(
                            std::io::ErrorKind::InvalidData,
                            format!("REALITY: Failed to parse dest ServerHello: {}", e),
                        ));
                    }
                    log::debug!(
                        "REALITY: Dest {} answered a non-ServerHello first record: {}, forwarding",
                        target.dest,
                        e
                    );
                    dest_records.extend(new_records);
                    return Ok(start_forward_to_dest(
                        server_stream,
                        dest_stream,
                        dest_records,
                        deframer.into_remaining_data(),
                        client_pending,
                        target.fallback_limits(client_hello_frame.len()),
                    ));
                }
            };
            if !parsed.is_tls13 {
                // A TLS 1.2 cover serves unauthenticated traffic only. An authenticated
                // REALITY request needs a TLS 1.3 cover to match its structure.
                if authenticated_connection.is_some() {
                    return Err(std::io::Error::new(
                        std::io::ErrorKind::InvalidData,
                        format!(
                            "REALITY: Dest {} answered TLS 1.2 for an authenticated request",
                            target.dest
                        ),
                    ));
                }
                log::debug!(
                    "REALITY: Dest {} is TLS 1.2, forwarding unauthenticated traffic",
                    target.dest
                );
                dest_records.extend(new_records);
                return Ok(start_forward_to_dest(
                    server_stream,
                    dest_stream,
                    dest_records,
                    deframer.into_remaining_data(),
                    client_pending,
                    target.fallback_limits(client_hello_frame.len()),
                ));
            }
            log::debug!("REALITY: Dest confirmed TLS 1.3");
        }

        dest_records.extend(new_records);

        // The compatibility ChangeCipherSpec record is optional in TLS 1.3, so the
        // encrypted flight is counted by record type instead of by position.
        let encrypted_records = dest_records
            .iter()
            .skip(1)
            .filter(|record| record.first() == Some(&CONTENT_TYPE_APPLICATION_DATA))
            .count();

        // Separate mode: ServerHello, optional CCS, then four encrypted records.
        if encrypted_records >= 4 {
            log::debug!(
                "REALITY: Separate mode detected, got {} records",
                dest_records.len()
            );
            break;
        } else if encrypted_records >= 1 {
            // Check if we have enough records using the 512-byte heuristic
            let first_encrypted = dest_records
                .iter()
                .skip(1)
                .find(|record| record.first() == Some(&CONTENT_TYPE_APPLICATION_DATA))
                .expect("one encrypted record was counted");
            if first_encrypted.len() > 512 {
                // Combined mode: first encrypted record > 512 bytes contains all messages
                log::debug!(
                    "REALITY: Combined mode detected (first encrypted record {} bytes > 512)",
                    first_encrypted.len()
                );
                break;
            }
        }
    }

    let remaining_data = deframer.into_remaining_data();

    log::debug!(
        "REALITY: Read {} records from dest ({} bytes remaining)",
        dest_records.len(),
        remaining_data.len()
    );

    let Some(mut reality_conn) = authenticated_connection else {
        log::debug!("REALITY: Unauthenticated traffic, forwarding through the cover");
        return Ok(start_forward_to_dest(
            server_stream,
            dest_stream,
            dest_records,
            remaining_data,
            client_pending,
            target.fallback_limits(client_hello_frame.len()),
        ));
    };

    log::debug!("REALITY: Auth succeeded, building response matching dest structure");

    drop(dest_stream);
    reality_conn.build_server_response(dest_records)?;

    let connection = CryptoConnection::new_reality_server(reality_conn);
    let mode = match target.inner_protocol {
        InnerProtocol::VisionVless(_) => TlsReadMode::PreserveRecords,
        _ => TlsReadMode::Stream,
    };
    let tls_stream =
        CryptoTlsStream::handshake(server_stream, connection, mode, &client_pending).await?;
    log::debug!("REALITY: TLS 1.3 handshake completed successfully");

    match &target.inner_protocol {
        InnerProtocol::Normal(handler) => handler.setup_server_stream(Box::new(tls_stream)).await,
        InnerProtocol::VisionVless(vision_cfg) => {
            crate::vless::vless_server_handler::setup_custom_tls_vision_vless_server_stream(
                tls_stream,
                &vision_cfg.user_id,
                vision_cfg.udp_enabled,
                target.effective_selector.clone(),
                resolver,
                vision_cfg.fallback.clone(),
            )
            .await
        }
    }
}

/// Return an owned fallback transfer after forwarding the ClientHello.
fn start_forward_to_dest(
    client_stream: Box<dyn AsyncStream>,
    dest_stream: Box<dyn AsyncStream>,
    dest_records: Vec<Bytes>,
    remaining_data: Bytes,
    client_pending: Bytes,
    limits: FallbackLimits,
) -> TcpServerSetupResult {
    TcpServerSetupResult::Session(Box::pin(async move {
        // Both preread directions enter the same pump as live bytes. A token wait
        // in either writer must not block reads or writes in the reverse direction.
        // A progress-aware shutdown also keeps a stalled half-close from cancelling
        // reply data that the limiter still has in flight.
        let mut client_stream = PrependStream::new(
            limit_writer(client_stream, limits.download, 0),
            (!client_pending.is_empty()).then(|| client_pending.to_vec().into_boxed_slice()),
        );
        let mut preread = Vec::new();
        for record in dest_records {
            preread.extend_from_slice(&record);
        }
        preread.extend_from_slice(&remaining_data);
        let mut dest_stream = PrependStream::new(
            limit_writer(dest_stream, limits.upload, limits.initial_upload),
            (!preread.is_empty()).then(|| preread.into_boxed_slice()),
        );

        let result = crate::copy_bidirectional::copy_bidirectional_with_progress_aware_shutdown(
            &mut client_stream,
            &mut dest_stream,
            false,
            false,
        )
        .await;

        futures::join!(
            crate::util::shutdown_stream(&mut client_stream),
            crate::util::shutdown_stream(&mut dest_stream),
        );

        if let Err(e) = result {
            log::debug!("REALITY FALLBACK: Connection ended with error: {}", e);
        }
    }))
}

#[cfg(test)]
mod cleanup_tests {
    use super::*;
    use std::io;
    use std::pin::Pin;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::task::{Context, Poll};
    use tokio::io::{AsyncRead, AsyncWrite, ReadBuf};

    /// A REALITY target whose destination is the local cover listener.
    fn local_cover_target(
        dest: NetLocation,
        private_key: [u8; 32],
        resolver: Arc<dyn Resolver>,
    ) -> RealityServerTarget {
        use crate::client_proxy_chain::InitialHopEntry;
        use crate::tcp::socket_connector_impl::SocketConnectorImpl;
        use crate::vless::vless_server_handler::VlessTcpServerHandler;

        let selector = Arc::new(ClientProxySelector::new(Vec::new()));
        RealityServerTarget {
            limit_fallback_upload: None,
            limit_fallback_download: None,
            private_key,
            short_ids: vec![[0; 8]],
            dest,
            max_time_diff: None,
            min_client_version: None,
            max_client_version: None,
            cipher_suites: Vec::new(),
            effective_selector: selector.clone(),
            inner_protocol: InnerProtocol::Normal(Box::new(VlessTcpServerHandler::new(
                "550e8400-e29b-41d4-a716-446655440000",
                true,
                selector,
                resolver,
                None,
            ))),
            dest_client_chain: ClientProxyChain::new(
                vec![InitialHopEntry::Direct(Box::new(
                    SocketConnectorImpl::from_config(&crate::config::ClientConfig::default()),
                ))],
                Vec::new(),
            ),
        }
    }

    /// Remove the optional TLS 1.3 compatibility CCS record from a cover flight.
    fn without_change_cipher_spec(flight: &[u8]) -> Vec<u8> {
        let mut out = Vec::new();
        let mut offset = 0;
        while offset + 5 <= flight.len() {
            let len = u16::from_be_bytes([flight[offset + 3], flight[offset + 4]]) as usize;
            let end = offset + 5 + len;
            assert!(end <= flight.len(), "truncated cover flight");
            if flight[offset] != 0x14 {
                out.extend_from_slice(&flight[offset..end]);
            }
            offset = end;
        }
        assert!(
            out.len() < flight.len(),
            "the cover flight must contain a CCS record to remove"
        );
        out
    }

    /// A cover flight that authenticates one client hello and answers with the
    /// given TLS records. The client connection is kept so the test can finish
    /// the REALITY handshake with the same keys the hello used.
    struct CoverFlight {
        client: CryptoConnection,
        hello: Vec<u8>,
        response: Vec<u8>,
        listener: tokio::net::TcpListener,
    }

    async fn cover_flight() -> CoverFlight {
        use aws_lc_rs::agreement;

        let private_key = [1; 32];
        let public_key = agreement::PrivateKey::from_private_key(&agreement::X25519, &private_key)
            .unwrap()
            .compute_public_key()
            .unwrap();
        let mut public_key_bytes = [0u8; 32];
        public_key_bytes.copy_from_slice(public_key.as_ref());

        let mut client = CryptoConnection::new_reality_client(
            crate::reality::RealityClientConnection::new(crate::reality::RealityClientConfig {
                public_key: public_key_bytes,
                short_id: [0; 8],
                server_name: "localhost".into(),
                cipher_suites: Vec::new(),
            })
            .unwrap(),
        );
        let mut hello = Vec::new();
        client.write_tls(&mut hello).unwrap();

        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        // A large certificate keeps the first encrypted record well above the
        // 512-byte combined-mode threshold, so the cover layout is deterministic.
        let cert = rcgen::generate_simple_self_signed(
            (0..100)
                .map(|index| format!("host-{index}.example.test"))
                .collect::<Vec<_>>(),
        )
        .unwrap();
        let mut config =
            rustls::ServerConfig::builder_with_protocol_versions(&[&rustls::version::TLS13])
                .with_no_client_auth()
                .with_single_cert(
                    vec![cert.cert.der().clone()],
                    rustls::pki_types::PrivatePkcs8KeyDer::from(cert.signing_key.serialize_der())
                        .into(),
                )
                .unwrap();
        config.send_tls13_tickets = 0;
        let mut decoy = CryptoConnection::new_rustls_server(
            rustls::ServerConnection::new(Arc::new(config)).unwrap(),
        );
        crate::crypto::feed_crypto_connection(&mut decoy, &hello).unwrap();
        decoy.process_new_packets().unwrap();
        let mut response = Vec::new();
        decoy.write_tls(&mut response).unwrap();

        CoverFlight {
            client,
            hello,
            response,
            listener,
        }
    }

    /// Bound for the cover helper to accept and read the client hello.
    const COVER_SETUP_BOUND: Duration = Duration::from_secs(5);
    /// Bound for the server side of one handshake.
    const SERVER_SETUP_BOUND: Duration = Duration::from_secs(10);
    /// Bound for the client side of one handshake and its request.
    const CLIENT_EXCHANGE_BOUND: Duration = Duration::from_secs(8);

    /// What the cover helper observed. It is recorded in shared state so the test
    /// still sees it when the helper is cancelled.
    #[derive(Clone, Debug, Default)]
    struct CoverObservations {
        accepted: bool,
        received_hello: usize,
        hello_matched: bool,
        bytes_written: usize,
        error: Option<String>,
    }

    impl CoverObservations {
        fn record_error(&mut self, error: impl std::fmt::Display) {
            if self.error.is_none() {
                self.error = Some(error.to_string());
            }
        }
    }

    /// Run one server handshake against a cover that answers with `response`,
    /// delivered in `chunks` separate writes.
    ///
    /// With `complete_client` the test also drives the REALITY client to the end
    /// of its handshake. Without it the client side only stays open, which is
    /// what the unauthenticated forwarding path needs.
    ///
    /// `expected_cover_hello` is what the cover must receive; `None` means the
    /// client hello of this flight. A test uses the parameter to prove that a
    /// cover mismatch reaches the test result.
    async fn run_cover_handshake(
        flight: CoverFlight,
        response: Vec<u8>,
        chunks: usize,
        complete_client: bool,
        expected_cover_hello: Option<Vec<u8>>,
    ) -> std::io::Result<TcpServerSetupResult> {
        let address = flight.listener.local_addr().unwrap();
        let dest = NetLocation::new(
            crate::address::Address::Hostname("localhost".into()),
            address.port(),
        );
        let resolver: Arc<dyn Resolver> = Arc::new(crate::resolver::NativeResolver::new());
        let target = local_cover_target(dest, [1; 32], Arc::clone(&resolver));

        let (io, mut peer) = tokio::io::duplex(65540);
        peer.write_all(&flight.hello).await.unwrap();
        let mut io: Box<dyn AsyncStream> = Box::new(io);
        let parsed = crate::tls_hello::read_client_hello(&mut io).await.unwrap();

        // The inner VLESS handler connects to this address after the REALITY
        // handshake, so the setup can finish with a session.
        let inner_listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let inner_target = inner_listener.local_addr().unwrap();
        let inner_observations = Arc::new(std::sync::Mutex::new(CoverObservations::default()));
        let inner_log = Arc::clone(&inner_observations);
        let inner_accept = tokio::spawn(async move {
            match tokio::time::timeout(COVER_SETUP_BOUND, inner_listener.accept()).await {
                Ok(Ok((_socket, _))) => {
                    inner_log.lock().unwrap().accepted = true;
                    // Keep the accepted session open for the inner handler.
                    std::future::pending::<()>().await;
                }
                Ok(Err(error)) => inner_log.lock().unwrap().record_error(error),
                Err(_) => inner_log
                    .lock()
                    .unwrap()
                    .record_error("the inner handler did not connect"),
            }
        });

        let listener = flight.listener;
        let expected = expected_cover_hello.unwrap_or_else(|| flight.hello.clone());
        let cover_observations = Arc::new(std::sync::Mutex::new(CoverObservations::default()));
        let cover_log = Arc::clone(&cover_observations);
        let cover = tokio::spawn(async move {
            let mut socket = match tokio::time::timeout(COVER_SETUP_BOUND, listener.accept()).await
            {
                Ok(Ok((socket, _))) => {
                    cover_log.lock().unwrap().accepted = true;
                    socket
                }
                Ok(Err(error)) => {
                    cover_log.lock().unwrap().record_error(error);
                    return;
                }
                Err(_) => {
                    cover_log
                        .lock()
                        .unwrap()
                        .record_error("the client did not connect");
                    return;
                }
            };

            let mut forwarded = vec![0; expected.len()];
            match tokio::time::timeout(COVER_SETUP_BOUND, socket.read_exact(&mut forwarded)).await {
                Ok(Ok(_)) => {
                    let mut log = cover_log.lock().unwrap();
                    log.received_hello = forwarded.len();
                    log.hello_matched = forwarded == expected;
                }
                Ok(Err(error)) => {
                    cover_log
                        .lock()
                        .unwrap()
                        .record_error(format!("reading the client hello failed: {error}"));
                    return;
                }
                Err(_) => {
                    cover_log
                        .lock()
                        .unwrap()
                        .record_error("reading the client hello timed out");
                    return;
                }
            }

            // An empty response is a cover that answers with nothing.
            let part = response.len().div_ceil(chunks).max(1);
            for chunk in response.chunks(part) {
                match socket.write_all(chunk).await {
                    Ok(()) => cover_log.lock().unwrap().bytes_written += chunk.len(),
                    Err(error) => {
                        cover_log
                            .lock()
                            .unwrap()
                            .record_error(format!("writing the cover flight failed: {error}"));
                        return;
                    }
                }
                if let Err(error) = socket.flush().await {
                    cover_log
                        .lock()
                        .unwrap()
                        .record_error(format!("flushing the cover flight failed: {error}"));
                    return;
                }
                tokio::time::sleep(Duration::from_millis(20)).await;
            }
            // A healthy cover waits for the client Finished, so keep the
            // connection open until the test cancels this task.
            std::future::pending::<()>().await;
        });

        let request = vless_tcp_request(inner_target);
        let server = tokio::time::timeout(
            SERVER_SETUP_BOUND,
            setup_reality_server_stream(io, &target, parsed, &resolver),
        );
        let client = async {
            if !complete_client {
                // Keep the client side open; the unauthenticated path only
                // forwards the cover flight to it.
                let _keep_open = peer;
                return Ok::<(), std::io::Error>(());
            }
            let mut stream =
                CryptoTlsStream::handshake(peer, flight.client, TlsReadMode::Stream, &[]).await?;
            stream.write_all(&request).await?;
            stream.flush().await?;
            // Hold the connection until the server has the request.
            tokio::time::sleep(Duration::from_millis(500)).await;
            Ok::<(), std::io::Error>(())
        };
        // Capture the exchange outcome first: a timeout must still reach the
        // helper cleanup below instead of detaching the owned tasks.
        let exchange = tokio::time::timeout(CLIENT_EXCHANGE_BOUND, async {
            tokio::join!(server, client)
        })
        .await;
        let exchange_outcome = match exchange {
            Ok(results) => Ok(results),
            Err(_) => Err(std::io::Error::new(
                std::io::ErrorKind::TimedOut,
                "the client exchange did not finish",
            )),
        };

        // Always cancel and await the owned helpers, whatever the outcome was.
        cover.abort();
        let cover_join = cover.await;
        inner_accept.abort();
        let inner_join = inner_accept.await;

        let cover_observations = cover_observations.lock().unwrap().clone();
        let inner_observations = inner_observations.lock().unwrap().clone();

        if let Err(error) = cover_join
            && !error.is_cancelled()
        {
            return Err(std::io::Error::other(format!(
                "the cover helper failed: {error}"
            )));
        }
        if let Err(error) = inner_join
            && !error.is_cancelled()
        {
            return Err(std::io::Error::other(format!(
                "the inner accept helper failed: {error}"
            )));
        }
        // The server outcome is the primary result, so it is reported before the
        // helper observations: a helper that merely waited while the setup failed
        // is not a separate failure.
        let (server_result, client_result) = exchange_outcome?;
        let server_result = match server_result {
            Ok(Ok(result)) => result,
            Ok(Err(error)) => return Err(error),
            Err(_) => {
                return Err(std::io::Error::new(
                    std::io::ErrorKind::TimedOut,
                    "the server setup did not finish",
                ));
            }
        };

        if let Some(error) = cover_observations.error {
            return Err(std::io::Error::other(format!("the cover helper: {error}")));
        }
        if let Some(error) = inner_observations.error {
            return Err(std::io::Error::other(format!("the inner helper: {error}")));
        }

        if !cover_observations.accepted || !cover_observations.hello_matched {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                format!(
                    "the cover did not receive the client hello: {:?}",
                    cover_observations
                ),
            ));
        }

        client_result?;
        Ok(server_result)
    }

    /// A VLESS TCP request for `target`, which the inner handler needs before it
    /// can finish the setup.
    fn vless_tcp_request(target: std::net::SocketAddr) -> Vec<u8> {
        let mut request = vec![0x00];
        request.extend_from_slice(&[
            0x55, 0x0e, 0x84, 0x00, 0xe2, 0x9b, 0x41, 0xd4, 0xa7, 0x16, 0x44, 0x66, 0x55, 0x44,
            0x00, 0x00,
        ]);
        request.push(0x00); // no addons
        request.push(0x01); // TCP
        request.extend_from_slice(&target.port().to_be_bytes());
        match target.ip() {
            std::net::IpAddr::V4(v4) => {
                request.push(0x01);
                request.extend_from_slice(&v4.octets());
            }
            std::net::IpAddr::V6(v6) => {
                request.push(0x03);
                request.extend_from_slice(&v6.octets());
            }
        }
        request
    }

    /// Assert that the setup finished with a usable result and report the error
    /// otherwise. An authenticated TCP request forwards to the destination and an
    /// unauthenticated one continues as a fallback session.
    fn expect_setup_ok(result: std::io::Result<TcpServerSetupResult>, context: &str) {
        match result {
            Ok(TcpServerSetupResult::TcpForward { .. }) | Ok(TcpServerSetupResult::Session(_)) => {}
            Ok(_) => panic!("{context}: unexpected setup result variant"),
            Err(error) => panic!("{context}: {error}"),
        }
    }

    #[tokio::test]
    async fn a_cover_that_sends_nothing_times_out_and_is_cleaned_up() {
        // The cover accepts the hello and answers with nothing, so the server
        // deadline closes the connection. The fixture must report the timeout
        // and still cancel and await both helpers.
        let flight = cover_flight().await;
        let error = run_cover_handshake(flight, Vec::new(), 1, true, None)
            .await
            .err()
            .expect("a cover that sends nothing must not complete the handshake");
        assert_eq!(error.kind(), std::io::ErrorKind::TimedOut, "{error}");
    }

    #[tokio::test]
    async fn a_cover_that_receives_a_different_hello_fails_the_test() {
        // The helper records what it received; the fixture turns a mismatch into
        // an error so a broken helper cannot look like a pass.
        let flight = cover_flight().await;
        let response = flight.response.clone();
        let error = run_cover_handshake(flight, response, 1, true, Some(vec![0u8; 8]))
            .await
            .err()
            .expect("a cover mismatch must fail the handshake");
        assert_eq!(error.kind(), std::io::ErrorKind::InvalidData, "{error}");
    }

    #[tokio::test]
    async fn a_cover_flight_with_change_cipher_spec_completes_the_handshake() {
        let flight = cover_flight().await;
        let response = flight.response.clone();
        let result = run_cover_handshake(flight, response, 1, true, None).await;
        expect_setup_ok(
            result,
            "a cover flight with CCS must complete the REALITY handshake",
        );
    }

    #[tokio::test]
    async fn a_cover_flight_without_change_cipher_spec_completes_the_handshake() {
        let flight = cover_flight().await;
        let response = without_change_cipher_spec(&flight.response);
        let result = run_cover_handshake(flight, response, 1, true, None).await;
        expect_setup_ok(result, "a TLS 1.3 cover may omit CCS");
    }

    #[tokio::test]
    async fn a_cover_flight_without_change_cipher_spec_split_across_reads_completes() {
        let flight = cover_flight().await;
        let response = without_change_cipher_spec(&flight.response);
        let result = run_cover_handshake(flight, response, 3, true, None).await;
        expect_setup_ok(
            result,
            "records arriving in separate reads must still complete",
        );
    }

    #[tokio::test]
    async fn an_unauthenticated_client_keeps_a_cover_flight_without_change_cipher_spec() {
        let mut flight = cover_flight().await;
        // Corrupt the REALITY envelope: the credentials no longer authenticate,
        // but the hello stays a valid TLS client hello for the cover.
        let session_id = 5 + 39;
        flight.hello[session_id] ^= 0xff;
        let response = without_change_cipher_spec(&flight.response);
        let result = run_cover_handshake(flight, response, 1, false, None).await;
        expect_setup_ok(
            result,
            "unauthenticated traffic must be forwarded through the cover",
        );
    }

    #[tokio::test]
    async fn authenticated_non_vision_oversized_preread_returns_error_without_panicking() {
        use crate::client_proxy_chain::InitialHopEntry;
        use crate::reality::{RealityClientConfig, RealityClientConnection};
        use crate::tcp::socket_connector_impl::SocketConnectorImpl;
        use crate::vless::vless_server_handler::VlessTcpServerHandler;
        use aws_lc_rs::agreement;

        let private_key = [1; 32];
        let public_key = agreement::PrivateKey::from_private_key(&agreement::X25519, &private_key)
            .unwrap()
            .compute_public_key()
            .unwrap();
        let mut client = RealityClientConnection::new(RealityClientConfig {
            public_key: public_key.as_ref().try_into().unwrap(),
            short_id: [0; 8],
            server_name: "localhost".into(),
            cipher_suites: Vec::new(),
        })
        .unwrap();
        let mut hello = Vec::new();
        client.write_tls(&mut hello).unwrap();

        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let dest = NetLocation::new(
            crate::address::Address::Hostname("localhost".into()),
            address.port(),
        );
        let cert = rcgen::generate_simple_self_signed(
            (0..100)
                .map(|index| format!("host-{index}.example.test"))
                .collect::<Vec<_>>(),
        )
        .unwrap();
        let decoy_config =
            rustls::ServerConfig::builder_with_protocol_versions(&[&rustls::version::TLS13])
                .with_no_client_auth()
                .with_single_cert(
                    vec![cert.cert.der().clone()],
                    rustls::pki_types::PrivatePkcs8KeyDer::from(cert.signing_key.serialize_der())
                        .into(),
                )
                .unwrap();
        let mut decoy = CryptoConnection::new_rustls_server(
            rustls::ServerConnection::new(Arc::new(decoy_config)).unwrap(),
        );
        crate::crypto::feed_crypto_connection(&mut decoy, &hello).unwrap();
        decoy.process_new_packets().unwrap();
        let mut response = Vec::new();
        decoy.write_tls(&mut response).unwrap();
        let selector = Arc::new(ClientProxySelector::new(Vec::new()));
        let resolver: Arc<dyn Resolver> = Arc::new(crate::resolver::NativeResolver::new());
        let target = RealityServerTarget {
            limit_fallback_upload: Some(FallbackRateLimit {
                bytes_per_sec: 1,
                ..Default::default()
            }),
            limit_fallback_download: Some(FallbackRateLimit {
                bytes_per_sec: 1,
                ..Default::default()
            }),
            private_key,
            short_ids: vec![[0; 8]],
            dest: dest.clone(),
            max_time_diff: None,
            min_client_version: None,
            max_client_version: None,
            cipher_suites: Vec::new(),
            effective_selector: selector.clone(),
            inner_protocol: InnerProtocol::Normal(Box::new(VlessTcpServerHandler::new(
                "550e8400-e29b-41d4-a716-446655440000",
                true,
                selector,
                resolver.clone(),
                None,
            ))),
            dest_client_chain: ClientProxyChain::new(
                vec![InitialHopEntry::Direct(Box::new(
                    SocketConnectorImpl::from_config(&crate::config::ClientConfig::default()),
                ))],
                Vec::new(),
            ),
        };
        let (io, mut peer) = tokio::io::duplex(65540);
        let mut flight = hello.clone();
        flight.extend_from_slice(&[0x17, 0x03, 0x03, 0, 1, 0].repeat(6000));
        peer.write_all(&flight).await.unwrap();
        let mut io: Box<dyn AsyncStream> = Box::new(io);
        let parsed = crate::tls_hello::read_client_hello(&mut io).await.unwrap();
        assert!(parsed.client_reader.unparsed_data().len() > 33290);

        let decoy_io = async {
            let (mut socket, _) = listener.accept().await.unwrap();
            let mut forwarded_hello = vec![0; hello.len()];
            socket.read_exact(&mut forwarded_hello).await.unwrap();
            assert_eq!(forwarded_hello, hello);
            socket.write_all(&response).await.unwrap();
        };
        let (result, ()) = tokio::time::timeout(Duration::from_secs(2), async {
            tokio::join!(
                setup_reality_server_stream(io, &target, parsed, &resolver),
                decoy_io,
            )
        })
        .await
        .unwrap();
        let error = result.err().expect("invalid encrypted records must fail");
        assert_eq!(error.kind(), io::ErrorKind::InvalidData, "{error}");
    }

    #[tokio::test]
    async fn fallback_preread_backpressure_does_not_block_server_response() {
        let (client, mut client_peer) = tokio::io::duplex(64);
        let (dest, mut dest_peer) = tokio::io::duplex(64);
        let pending = vec![42; 128];
        let TcpServerSetupResult::Session(session) = start_forward_to_dest(
            Box::new(client),
            Box::new(dest),
            vec![Bytes::from_static(b"server")],
            Bytes::new(),
            Bytes::copy_from_slice(&pending),
            FallbackLimits::default(),
        ) else {
            panic!("expected fallback session");
        };
        tokio::time::timeout(Duration::from_secs(1), async {
            tokio::join!(session, async {
                let mut response = [0; 6];
                client_peer.read_exact(&mut response).await.unwrap();
                assert_eq!(&response, b"server");
                let mut received = vec![0; pending.len()];
                dest_peer.read_exact(&mut received).await.unwrap();
                assert_eq!(received, pending);
                client_peer.shutdown().await.unwrap();
                dest_peer.shutdown().await.unwrap();
            });
        })
        .await
        .unwrap();
    }

    #[tokio::test]
    async fn fallback_preserves_preread_client_bytes() {
        let (client, mut client_peer) = tokio::io::duplex(64);
        let (dest, mut dest_peer) = tokio::io::duplex(64);
        let TcpServerSetupResult::Session(session) = start_forward_to_dest(
            Box::new(client),
            Box::new(dest),
            vec![Bytes::from_static(b"server")],
            Bytes::from_static(b"-tail"),
            Bytes::from_static(b"client"),
            FallbackLimits::default(),
        ) else {
            panic!("expected fallback session");
        };
        tokio::time::timeout(Duration::from_secs(1), async {
            tokio::join!(session, async {
                client_peer.write_all(b"-live").await.unwrap();
                client_peer.shutdown().await.unwrap();
                let mut received = [0; 11];
                dest_peer.read_exact(&mut received).await.unwrap();
                assert_eq!(&received, b"client-live");
                dest_peer.write_all(b"-reply").await.unwrap();
                dest_peer.shutdown().await.unwrap();
                let mut response = Vec::new();
                client_peer.read_to_end(&mut response).await.unwrap();
                assert_eq!(response, b"server-tail-reply");
            });
        })
        .await
        .unwrap();
    }

    #[tokio::test(start_paused = true)]
    async fn fallback_limits_include_preread_and_allow_reverse_progress() {
        let (client, mut client_peer) = tokio::io::duplex(64);
        let (dest, mut dest_peer) = tokio::io::duplex(64);
        let TcpServerSetupResult::Session(session) = start_forward_to_dest(
            Box::new(client),
            Box::new(dest),
            vec![Bytes::from_static(b"server")],
            Bytes::from_static(b"-tail"),
            Bytes::from_static(b"abc"),
            FallbackLimits {
                upload: Some(FallbackRateLimit {
                    after_bytes: 6,
                    bytes_per_sec: 1,
                    burst_bytes_per_sec: 1,
                }),
                download: Some(FallbackRateLimit {
                    after_bytes: 2,
                    bytes_per_sec: 2,
                    burst_bytes_per_sec: 1,
                }),
                initial_upload: 4,
            },
        ) else {
            panic!()
        };
        let task = tokio::spawn(session);
        let mut upload = [0; 3];
        dest_peer.read_exact(&mut upload).await.unwrap();
        assert_eq!(&upload, b"abc");
        let mut download = [0; 3];
        client_peer.read_exact(&mut download).await.unwrap();
        assert_eq!(&download, b"ser");
        client_peer.write_all(b"d").await.unwrap();
        let mut byte = [0];
        let mut read_upload = Box::pin(dest_peer.read_exact(&mut byte));
        assert!(futures::poll!(read_upload.as_mut()).is_pending());
        tokio::time::advance(Duration::from_millis(500)).await;
        client_peer.read_exact(&mut download[..1]).await.unwrap();
        assert_eq!(download[0], b'v');
        assert!(futures::poll!(read_upload.as_mut()).is_pending());
        tokio::time::advance(Duration::from_millis(500)).await;
        read_upload.await.unwrap();
        assert_eq!(byte[0], b'd');
        // Cancellation drops both streams and the outstanding token timer.
        task.abort();
        assert!(task.await.unwrap_err().is_cancelled());
    }

    #[tokio::test(start_paused = true)]
    async fn fallback_cancellation_releases_throttled_streams() {
        let marker = Arc::new(());
        let stream = || {
            Box::new(StalledShutdown {
                fail_write: false,
                fail_flush: false,
                _marker: marker.clone(),
            }) as Box<dyn AsyncStream>
        };
        let TcpServerSetupResult::Session(mut session) = start_forward_to_dest(
            stream(),
            stream(),
            vec![Bytes::from_static(b"response")],
            Bytes::new(),
            Bytes::new(),
            FallbackLimits {
                download: Some(FallbackRateLimit {
                    bytes_per_sec: 1,
                    ..Default::default()
                }),
                ..Default::default()
            },
        ) else {
            panic!()
        };
        assert!(futures::poll!(session.as_mut()).is_pending());
        assert_eq!(Arc::strong_count(&marker), 3);
        drop(session);
        assert_eq!(Arc::strong_count(&marker), 1);
    }

    struct StalledShutdown {
        fail_write: bool,
        fail_flush: bool,
        _marker: Arc<()>,
    }

    impl AsyncRead for StalledShutdown {
        fn poll_read(
            self: Pin<&mut Self>,
            _: &mut Context<'_>,
            _: &mut ReadBuf<'_>,
        ) -> Poll<io::Result<()>> {
            Poll::Ready(Ok(()))
        }
    }

    impl AsyncWrite for StalledShutdown {
        fn poll_write(
            self: Pin<&mut Self>,
            _: &mut Context<'_>,
            bytes: &[u8],
        ) -> Poll<io::Result<usize>> {
            if self.fail_write {
                Poll::Ready(Err(io::ErrorKind::BrokenPipe.into()))
            } else {
                Poll::Ready(Ok(bytes.len()))
            }
        }
        fn poll_flush(self: Pin<&mut Self>, _: &mut Context<'_>) -> Poll<io::Result<()>> {
            if self.fail_flush {
                Poll::Ready(Err(io::ErrorKind::BrokenPipe.into()))
            } else {
                Poll::Ready(Ok(()))
            }
        }
        fn poll_shutdown(self: Pin<&mut Self>, _: &mut Context<'_>) -> Poll<io::Result<()>> {
            Poll::Pending
        }
    }

    impl crate::async_stream::AsyncPing for StalledShutdown {
        fn supports_ping(&self) -> bool {
            false
        }
        fn poll_write_ping(self: Pin<&mut Self>, _: &mut Context<'_>) -> Poll<io::Result<bool>> {
            Poll::Ready(Ok(false))
        }
    }

    impl AsyncStream for StalledShutdown {}

    /// The queued thirteen-byte reply needs twelve seconds at one byte per second.
    /// Cleanup must wait for that drain, then stay bounded, and release both streams.
    #[tokio::test(start_paused = true)]
    async fn fallback_throttled_half_close_cleanup_is_bounded() {
        let marker = Arc::new(());
        let stream = || {
            Box::new(StalledShutdown {
                fail_write: false,
                fail_flush: false,
                _marker: marker.clone(),
            }) as Box<dyn AsyncStream>
        };
        let TcpServerSetupResult::Session(session) = start_forward_to_dest(
            stream(),
            stream(),
            vec![Bytes::from_static(b"long response")],
            Bytes::new(),
            Bytes::new(),
            FallbackLimits {
                download: Some(FallbackRateLimit {
                    bytes_per_sec: 1,
                    ..Default::default()
                }),
                ..Default::default()
            },
        ) else {
            panic!()
        };
        let started = Instant::now();
        tokio::time::timeout(Duration::from_secs(60), session)
            .await
            .unwrap();
        assert_eq!(started.elapsed(), Duration::from_secs(22));
        assert_eq!(Arc::strong_count(&marker), 1);
    }

    /// Client side of a fallback session that counts bytes it accepts.
    struct CapturingStalled {
        captured: Arc<AtomicUsize>,
        accept_limit: usize,
        shutdown_ready: bool,
    }

    impl AsyncRead for CapturingStalled {
        fn poll_read(
            self: Pin<&mut Self>,
            _: &mut Context<'_>,
            _: &mut ReadBuf<'_>,
        ) -> Poll<io::Result<()>> {
            Poll::Ready(Ok(()))
        }
    }

    impl AsyncWrite for CapturingStalled {
        fn poll_write(
            self: Pin<&mut Self>,
            _: &mut Context<'_>,
            bytes: &[u8],
        ) -> Poll<io::Result<usize>> {
            let accepted = bytes.len().min(self.accept_limit);
            self.captured.fetch_add(accepted, Ordering::Relaxed);
            Poll::Ready(Ok(accepted))
        }

        fn poll_flush(self: Pin<&mut Self>, _: &mut Context<'_>) -> Poll<io::Result<()>> {
            Poll::Ready(Ok(()))
        }

        fn poll_shutdown(self: Pin<&mut Self>, _: &mut Context<'_>) -> Poll<io::Result<()>> {
            if self.shutdown_ready {
                Poll::Ready(Ok(()))
            } else {
                Poll::Pending
            }
        }
    }

    impl crate::async_stream::AsyncPing for CapturingStalled {
        fn supports_ping(&self) -> bool {
            false
        }

        fn poll_write_ping(self: Pin<&mut Self>, _: &mut Context<'_>) -> Poll<io::Result<bool>> {
            Poll::Ready(Ok(false))
        }
    }

    impl AsyncStream for CapturingStalled {}

    fn capturing_stalled(captured: &Arc<AtomicUsize>, accept_limit: usize) -> Box<dyn AsyncStream> {
        Box::new(CapturingStalled {
            captured: captured.clone(),
            accept_limit,
            shutdown_ready: false,
        })
    }

    /// A queued, rate-limited reply must survive a stalled half-close.
    /// Before the progress-aware policy this test lost queued bytes at ten seconds.
    #[tokio::test(start_paused = true)]
    async fn fallback_limited_reply_drains_before_shutdown_cleanup() {
        const REPLY_BYTES: usize = 13;
        for (limited, expected_seconds) in [(false, 10), (true, 22)] {
            let delivered = Arc::new(AtomicUsize::new(0));
            let limits = FallbackLimits {
                download: limited.then_some(FallbackRateLimit {
                    bytes_per_sec: 1,
                    ..Default::default()
                }),
                ..Default::default()
            };
            let TcpServerSetupResult::Session(session) = start_forward_to_dest(
                capturing_stalled(&delivered, usize::MAX),
                capturing_stalled(&Arc::new(AtomicUsize::new(0)), usize::MAX),
                vec![Bytes::from(vec![0u8; REPLY_BYTES])],
                Bytes::new(),
                Bytes::new(),
                limits,
            ) else {
                panic!("expected fallback session")
            };
            let started = Instant::now();
            tokio::time::timeout(Duration::from_secs(60), session)
                .await
                .expect("fallback cleanup must stay bounded");
            assert_eq!(
                delivered.load(Ordering::Relaxed),
                REPLY_BYTES,
                "limited={limited} lost queued reply bytes"
            );
            assert_eq!(
                started.elapsed(),
                Duration::from_secs(expected_seconds),
                "limited={limited}"
            );
        }
    }

    /// Partial writer acceptance must not truncate a rate-limited reply.
    #[tokio::test(start_paused = true)]
    async fn fallback_partial_writes_do_not_truncate_limited_reply() {
        const REPLY_BYTES: usize = 11;
        let delivered = Arc::new(AtomicUsize::new(0));
        let TcpServerSetupResult::Session(session) = start_forward_to_dest(
            capturing_stalled(&delivered, 2),
            capturing_stalled(&Arc::new(AtomicUsize::new(0)), usize::MAX),
            vec![Bytes::from(vec![0u8; REPLY_BYTES])],
            Bytes::new(),
            Bytes::new(),
            FallbackLimits {
                download: Some(FallbackRateLimit {
                    bytes_per_sec: 1,
                    burst_bytes_per_sec: 3,
                    ..Default::default()
                }),
                ..Default::default()
            },
        ) else {
            panic!("expected fallback session")
        };
        let started = Instant::now();
        tokio::time::timeout(Duration::from_secs(60), session)
            .await
            .expect("fallback cleanup must stay bounded");
        assert_eq!(delivered.load(Ordering::Relaxed), REPLY_BYTES);
        assert_eq!(started.elapsed(), Duration::from_secs(18));
    }

    /// Control: a normal slow half-close delivers the whole reply and ends cleanly.
    #[tokio::test(start_paused = true)]
    async fn fallback_slow_half_close_delivers_whole_reply() {
        let (client, mut client_peer) = tokio::io::duplex(64);
        let (dest, mut dest_peer) = tokio::io::duplex(64);
        let TcpServerSetupResult::Session(session) = start_forward_to_dest(
            Box::new(client),
            Box::new(dest),
            vec![Bytes::from_static(b"hello")],
            Bytes::new(),
            Bytes::new(),
            FallbackLimits {
                download: Some(FallbackRateLimit {
                    bytes_per_sec: 1,
                    ..Default::default()
                }),
                ..Default::default()
            },
        ) else {
            panic!("expected fallback session")
        };
        let started = Instant::now();
        tokio::time::timeout(Duration::from_secs(15), async {
            tokio::join!(session, async {
                client_peer.write_all(b"request").await.unwrap();
                client_peer.shutdown().await.unwrap();
                let mut request = Vec::new();
                dest_peer.read_to_end(&mut request).await.unwrap();
                assert_eq!(request, b"request");
                dest_peer.shutdown().await.unwrap();
                let mut reply = Vec::new();
                client_peer.read_to_end(&mut reply).await.unwrap();
                assert_eq!(reply, b"hello");
            });
        })
        .await
        .unwrap();
        assert!(started.elapsed() <= Duration::from_secs(10));
    }

    #[tokio::test(start_paused = true)]
    async fn fallback_cleanup_is_bounded_after_copy_and_early_errors() {
        let bytes = Bytes::from_static(b"handshake");
        let cases = [
            (false, false, vec![], Bytes::new(), 10),
            (true, false, vec![bytes.clone()], Bytes::new(), 5),
            (false, true, vec![bytes.clone()], Bytes::new(), 5),
            (true, false, vec![], bytes, 5),
        ];
        for (fail_write, fail_flush, records, remaining, seconds) in cases {
            let marker = Arc::new(());
            let client = StalledShutdown {
                fail_write,
                fail_flush,
                _marker: marker.clone(),
            };
            let dest = StalledShutdown {
                fail_write: false,
                fail_flush: false,
                _marker: marker.clone(),
            };
            let TcpServerSetupResult::Session(session) = start_forward_to_dest(
                Box::new(client),
                Box::new(dest),
                records,
                remaining,
                Bytes::new(),
                FallbackLimits::default(),
            ) else {
                panic!("fallback must remain owned by its caller")
            };
            let started = tokio::time::Instant::now();
            tokio::time::timeout(std::time::Duration::from_secs(11), session)
                .await
                .expect("fallback cleanup retained the streams indefinitely");
            assert_eq!(started.elapsed(), std::time::Duration::from_secs(seconds));
            assert_eq!(Arc::strong_count(&marker), 1);
        }
    }

    // ---- Synthetic covers for the REALITY cover-failure decision ------------------

    const SYNTHETIC_UUID: &str = "550e8400-e29b-41d4-a716-446655440000";
    /// The owner-mandated REALITY destination and SNI. Fixtures use this exact name
    /// as the ClientHello SNI and as the target destination hostname.
    const SYNTHETIC_COVER_HOSTNAME: &str = "whatsapp.net";

    /// Maps exactly the synthetic cover name to one loopback endpoint. Every other
    /// name fails, so no fixture can reach real DNS or a real cover.
    #[derive(Debug)]
    struct SyntheticCoverResolver {
        address: std::net::IpAddr,
        calls: Arc<AtomicUsize>,
    }

    impl Resolver for SyntheticCoverResolver {
        fn resolve_location(
            &self,
            location: &NetLocation,
        ) -> std::pin::Pin<
            Box<
                dyn std::future::Future<Output = std::io::Result<Vec<std::net::SocketAddr>>> + Send,
            >,
        > {
            self.calls.fetch_add(1, Ordering::SeqCst);
            let resolved = match location.address() {
                crate::address::Address::Hostname(name) if name == SYNTHETIC_COVER_HOSTNAME => Ok(
                    vec![std::net::SocketAddr::new(self.address, location.port())],
                ),
                other => Err(std::io::Error::new(
                    std::io::ErrorKind::NotFound,
                    format!("synthetic resolver has no mapping for {other:?}"),
                )),
            };
            Box::pin(async move { resolved })
        }
    }

    fn synthetic_cover_resolver() -> (Arc<dyn Resolver>, Arc<AtomicUsize>) {
        let calls = Arc::new(AtomicUsize::new(0));
        (
            Arc::new(SyntheticCoverResolver {
                address: std::net::IpAddr::V4(std::net::Ipv4Addr::LOCALHOST),
                calls: calls.clone(),
            }),
            calls,
        )
    }

    fn synthetic_public_key(private_key: [u8; 32]) -> [u8; 32] {
        aws_lc_rs::agreement::PrivateKey::from_private_key(
            &aws_lc_rs::agreement::X25519,
            &private_key,
        )
        .unwrap()
        .compute_public_key()
        .unwrap()
        .as_ref()
        .try_into()
        .unwrap()
    }

    fn synthetic_reality_client_hello(short_id: [u8; 8], public_key: [u8; 32]) -> Vec<u8> {
        use crate::reality::{RealityClientConfig, RealityClientConnection};

        let mut client = RealityClientConnection::new(RealityClientConfig {
            public_key,
            short_id,
            server_name: SYNTHETIC_COVER_HOSTNAME.into(),
            cipher_suites: Vec::new(),
        })
        .unwrap();
        let mut hello = Vec::new();
        client.write_tls(&mut hello).unwrap();
        hello
    }

    fn tls_record(content_type: u8, payload: &[u8]) -> Vec<u8> {
        let mut record = vec![content_type, 0x03, 0x03];
        record.extend_from_slice(&(payload.len() as u16).to_be_bytes());
        record.extend_from_slice(payload);
        record
    }

    /// One structurally valid ServerHello. TLS 1.3 is signalled only by the
    /// supported_versions extension, exactly as `parse_server_hello` expects.
    fn synthetic_server_hello(tls13: bool) -> Vec<u8> {
        let mut body = vec![0x03, 0x03];
        body.extend_from_slice(&[0x5a; 32]);
        body.push(0);
        body.extend_from_slice(&[0x13, 0x01]);
        body.push(0);
        if tls13 {
            body.extend_from_slice(&[0x00, 0x06, 0x00, 0x2b, 0x00, 0x02, 0x03, 0x04]);
        } else {
            body.extend_from_slice(&[0x00, 0x00]);
        }
        let mut payload = vec![0x02];
        let length = body.len();
        payload.extend_from_slice(&[(length >> 16) as u8, (length >> 8) as u8, length as u8]);
        payload.extend_from_slice(&body);
        tls_record(0x16, &payload)
    }

    /// ServerHello, ChangeCipherSpec, and one encrypted record over 512 bytes.
    /// That is the combined-mode shape the cover mirroring heuristic expects.
    fn synthetic_tls13_flight() -> Vec<u8> {
        let mut flight = synthetic_server_hello(true);
        flight.extend_from_slice(&tls_record(0x14, &[0x01]));
        flight.extend_from_slice(&tls_record(0x17, &[0x5c; 600]));
        flight
    }

    /// Readable cover answers that are not a usable ServerHello: a fatal alert
    /// (content type 0x15) and a HelloRetryRequest. A healthy cover may answer an
    /// unauthenticated hello with either one, so neither is a cover failure.
    fn synthetic_non_server_hello_cover_responses() -> [Vec<u8>; 2] {
        // RFC 8446 section 4.1.3: a HelloRetryRequest is a ServerHello whose
        // random is this fixed value.
        const HELLO_RETRY_REQUEST_RANDOM: [u8; 32] = [
            0xCF, 0x21, 0xAD, 0x74, 0xE5, 0x9A, 0x61, 0x11, 0xBE, 0x1D, 0x8C, 0x02, 0x1E, 0x65,
            0xB8, 0x91, 0xC2, 0xA2, 0x11, 0x16, 0x7A, 0xBB, 0x8C, 0x5E, 0x07, 0x9E, 0x09, 0xE2,
            0xC8, 0xA8, 0x33, 0x9C,
        ];
        // The random starts after the 5-byte record header, the 4-byte handshake
        // header, and the 2-byte legacy version of `synthetic_server_hello`.
        let mut retry = synthetic_server_hello(true);
        retry[11..43].copy_from_slice(&HELLO_RETRY_REQUEST_RANDOM);
        [tls_record(0x15, &[0x02, 0x28]), retry]
    }

    /// A genuine TLS 1.2 ClientHello and the flight a TLS 1.2 cover answers it with.
    /// Both sides are driven by the production crypto backend, so the records are
    /// real TLS 1.2 records rather than hand-flipped version flags.
    fn synthetic_tls12_handshake() -> (Vec<u8>, Vec<u8>) {
        let cert =
            rcgen::generate_simple_self_signed(vec![SYNTHETIC_COVER_HOSTNAME.into()]).unwrap();
        let server_config =
            rustls::ServerConfig::builder_with_protocol_versions(&[&rustls::version::TLS12])
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
            rustls::ClientConfig::builder_with_protocol_versions(&[&rustls::version::TLS12])
                .with_root_certificates(roots)
                .with_no_client_auth();
        let mut client = CryptoConnection::new_rustls_client(
            rustls::ClientConnection::new(
                Arc::new(client_config),
                SYNTHETIC_COVER_HOSTNAME.try_into().unwrap(),
            )
            .unwrap(),
        );
        let mut server = CryptoConnection::new_rustls_server(
            rustls::ServerConnection::new(Arc::new(server_config)).unwrap(),
        );
        let mut hello = Vec::new();
        client.write_tls(&mut hello).unwrap();
        crate::crypto::feed_crypto_connection(&mut server, &hello).unwrap();
        server.process_new_packets().unwrap();
        let mut flight = Vec::new();
        server.write_tls(&mut flight).unwrap();
        (hello, flight)
    }

    enum SyntheticCover {
        /// Accepts the connection and sends nothing.
        Silent,
        /// Sends these bytes once the forwarded ClientHello arrives.
        Respond(Vec<u8>),
        /// Closes the connection once the forwarded ClientHello arrives.
        Close,
        /// Resets the connection without reading the forwarded ClientHello.
        Reset,
    }

    /// A loopback cover. The task holds the accepted socket until the test aborts it.
    async fn synthetic_cover(behavior: SyntheticCover) -> (u16, tokio::task::JoinHandle<()>) {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        let task = tokio::spawn(async move {
            let Ok((mut socket, _)) = listener.accept().await else {
                return;
            };
            match behavior {
                SyntheticCover::Reset => {
                    socket2::SockRef::from(&socket)
                        .set_linger(Some(Duration::ZERO))
                        .unwrap();
                }
                SyntheticCover::Close => {
                    drain_forwarded_hello(&mut socket).await;
                }
                SyntheticCover::Silent => {
                    drain_forwarded_hello(&mut socket).await;
                    tokio::time::sleep(Duration::from_secs(300)).await;
                }
                SyntheticCover::Respond(response) => {
                    drain_forwarded_hello(&mut socket).await;
                    let _ = socket.write_all(&response).await;
                    tokio::time::sleep(Duration::from_secs(300)).await;
                }
            }
        });
        (port, task)
    }

    /// Reads the forwarded ClientHello without waiting for further cover input.
    async fn drain_forwarded_hello(socket: &mut tokio::net::TcpStream) {
        let mut scratch = [0u8; 4096];
        while let Ok(Ok(read)) =
            tokio::time::timeout(Duration::from_millis(200), socket.read(&mut scratch)).await
        {
            if read == 0 {
                return;
            }
        }
    }

    /// A cover that hands its accepted socket to the test.
    struct InteractiveCover {
        port: u16,
        socket: tokio::sync::oneshot::Receiver<tokio::net::TcpStream>,
    }

    async fn interactive_cover() -> InteractiveCover {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        let (sender, socket) = tokio::sync::oneshot::channel();
        tokio::spawn(async move {
            if let Ok((socket, _)) = listener.accept().await {
                let _ = sender.send(socket);
            }
        });
        InteractiveCover { port, socket }
    }

    fn synthetic_reality_target(
        private_key: [u8; 32],
        cover_port: u16,
        resolver: Arc<dyn Resolver>,
    ) -> RealityServerTarget {
        use crate::client_proxy_chain::InitialHopEntry;
        use crate::tcp::socket_connector_impl::SocketConnectorImpl;
        use crate::vless::vless_server_handler::VlessTcpServerHandler;

        let selector = Arc::new(ClientProxySelector::new(Vec::new()));
        RealityServerTarget {
            limit_fallback_upload: None,
            limit_fallback_download: None,
            private_key,
            short_ids: vec![[0; 8]],
            dest: NetLocation::new(
                crate::address::Address::Hostname(SYNTHETIC_COVER_HOSTNAME.into()),
                cover_port,
            ),
            max_time_diff: None,
            min_client_version: None,
            max_client_version: None,
            cipher_suites: Vec::new(),
            effective_selector: selector.clone(),
            inner_protocol: InnerProtocol::Normal(Box::new(VlessTcpServerHandler::new(
                SYNTHETIC_UUID,
                true,
                selector,
                resolver.clone(),
                None,
            ))),
            dest_client_chain: ClientProxyChain::new(
                vec![InitialHopEntry::Direct(Box::new(
                    SocketConnectorImpl::from_config(&crate::config::ClientConfig::default()),
                ))],
                Vec::new(),
            ),
        }
    }

    /// Runs the production setup against one synthetic cover.
    async fn setup_against_cover(
        hello: Vec<u8>,
        target: &RealityServerTarget,
        resolver: &Arc<dyn Resolver>,
    ) -> (
        std::io::Result<TcpServerSetupResult>,
        tokio::io::DuplexStream,
    ) {
        let (io, mut peer) = tokio::io::duplex(65540);
        peer.write_all(&hello).await.unwrap();
        let mut io: Box<dyn AsyncStream> = Box::new(io);
        let parsed = crate::tls_hello::read_client_hello(&mut io).await.unwrap();
        let result = setup_reality_server_stream(io, target, parsed, resolver).await;
        (result, peer)
    }

    fn synthetic_vless_tcp_request() -> Vec<u8> {
        let mut request = vec![0u8];
        request.extend_from_slice(&crate::uuid_util::parse_uuid(SYNTHETIC_UUID).unwrap());
        request.push(0);
        request.push(1); // COMMAND_TCP
        request.extend_from_slice(&443u16.to_be_bytes());
        request.push(1);
        request.extend_from_slice(&[127, 0, 0, 1]);
        request
    }

    #[tokio::test(start_paused = true)]
    async fn authenticated_client_stops_on_silent_cover() {
        let private_key = [1u8; 32];
        let (resolver, cover_calls) = synthetic_cover_resolver();
        let (cover_port, cover) = synthetic_cover(SyntheticCover::Silent).await;
        let target = synthetic_reality_target(private_key, cover_port, resolver.clone());
        let hello = synthetic_reality_client_hello([0; 8], synthetic_public_key(private_key));

        let (result, _peer) = setup_against_cover(hello, &target, &resolver).await;
        cover.abort();
        assert_eq!(
            cover_calls.load(Ordering::SeqCst),
            1,
            "the cover name must resolve only through the injected synthetic resolver"
        );

        let error = result
            .err()
            .expect("an authenticated client behind a silent cover must be stopped");
        assert_eq!(error.kind(), io::ErrorKind::TimedOut, "{error}");
    }

    #[tokio::test(start_paused = true)]
    async fn unauthenticated_client_stops_on_silent_cover() {
        let private_key = [1u8; 32];
        let (resolver, cover_calls) = synthetic_cover_resolver();
        let (cover_port, cover) = synthetic_cover(SyntheticCover::Silent).await;
        let target = synthetic_reality_target(private_key, cover_port, resolver.clone());
        // A wrong short id fails authentication but the cover decision must not change.
        let hello = synthetic_reality_client_hello([9; 8], synthetic_public_key(private_key));

        let (result, _peer) = setup_against_cover(hello, &target, &resolver).await;
        cover.abort();
        assert_eq!(cover_calls.load(Ordering::SeqCst), 1);

        let error = result
            .err()
            .expect("unauthenticated traffic behind a silent cover must be stopped");
        assert_eq!(error.kind(), io::ErrorKind::TimedOut, "{error}");
    }

    #[tokio::test(start_paused = true)]
    async fn tls12_client_stops_on_silent_cover() {
        let private_key = [1u8; 32];
        let (resolver, _cover_calls) = synthetic_cover_resolver();
        let (cover_port, cover) = synthetic_cover(SyntheticCover::Silent).await;
        let target = synthetic_reality_target(private_key, cover_port, resolver.clone());
        let (hello, _cover_flight) = synthetic_tls12_handshake();

        let (result, _peer) = setup_against_cover(hello, &target, &resolver).await;
        cover.abort();

        let error = result
            .err()
            .expect("a non-TLS1.3 client is no excuse for a silent cover");
        assert_eq!(error.kind(), io::ErrorKind::TimedOut, "{error}");
    }

    #[tokio::test]
    async fn client_stops_on_malformed_cover_records() {
        let private_key = [1u8; 32];
        let (resolver, _cover_calls) = synthetic_cover_resolver();
        let (cover_port, cover) = synthetic_cover(SyntheticCover::Respond(
            b"this is not a tls record".to_vec(),
        ))
        .await;
        let target = synthetic_reality_target(private_key, cover_port, resolver.clone());
        let hello = synthetic_reality_client_hello([0; 8], synthetic_public_key(private_key));

        let (result, _peer) = setup_against_cover(hello, &target, &resolver).await;
        cover.abort();

        let error = result
            .err()
            .expect("malformed cover records must stop the connection");
        assert_eq!(error.kind(), io::ErrorKind::InvalidData, "{error}");
    }

    #[tokio::test]
    async fn client_stops_on_invalid_cover_server_hello() {
        let private_key = [1u8; 32];
        let (resolver, _cover_calls) = synthetic_cover_resolver();
        // A handshake record that is not a ServerHello.
        let mut payload = vec![0x0b];
        payload.extend_from_slice(&[0x00, 0x00, 45]);
        payload.extend_from_slice(&[0x11; 45]);
        let (cover_port, cover) =
            synthetic_cover(SyntheticCover::Respond(tls_record(0x16, &payload))).await;
        let target = synthetic_reality_target(private_key, cover_port, resolver.clone());
        let hello = synthetic_reality_client_hello([0; 8], synthetic_public_key(private_key));

        let (result, _peer) = setup_against_cover(hello, &target, &resolver).await;
        cover.abort();

        let error = result
            .err()
            .expect("an invalid cover ServerHello must stop the connection");
        assert_eq!(error.kind(), io::ErrorKind::InvalidData, "{error}");
    }

    /// A healthy cover may answer an unauthenticated hello with a readable record
    /// that is not a ServerHello. That is a cover response, not a cover failure:
    /// the alert or HelloRetryRequest bytes must reach the client unchanged.
    #[tokio::test]
    async fn unauthenticated_client_keeps_a_non_server_hello_cover_response() {
        let private_key = [5u8; 32];
        for response in synthetic_non_server_hello_cover_responses() {
            let (resolver, cover_calls) = synthetic_cover_resolver();
            let (cover_port, cover) =
                synthetic_cover(SyntheticCover::Respond(response.clone())).await;
            let target = synthetic_reality_target(private_key, cover_port, resolver.clone());
            let hello = synthetic_reality_client_hello([9; 8], synthetic_public_key(private_key));

            let (result, mut peer) = setup_against_cover(hello, &target, &resolver).await;
            assert_eq!(cover_calls.load(Ordering::SeqCst), 1);

            let TcpServerSetupResult::Session(session) = result.expect(
                "a readable non-ServerHello cover response must forward unauthenticated traffic",
            ) else {
                panic!("unauthenticated traffic must return a forwarding session");
            };
            let relay = tokio::spawn(session);
            let mut preread = vec![0u8; response.len()];
            tokio::time::timeout(Duration::from_secs(5), peer.read_exact(&mut preread))
                .await
                .expect("the cover response must reach the client")
                .unwrap();
            assert_eq!(
                preread, response,
                "the cover response must reach the client unchanged"
            );
            // Nothing follows the relayed cover response: it must not be duplicated.
            let mut extra = [0u8; 1];
            let mut read_extra = Box::pin(peer.read(&mut extra));
            assert!(
                futures::poll!(read_extra.as_mut()).is_pending(),
                "the cover response must be relayed exactly once"
            );
            relay.abort();
            cover.abort();
        }
    }

    /// The authenticated counterpart: the same readable cover answers cannot back a
    /// REALITY handshake, so an authenticated request still stops.
    #[tokio::test]
    async fn authenticated_client_stops_on_a_non_server_hello_cover_response() {
        let private_key = [7u8; 32];
        for response in synthetic_non_server_hello_cover_responses() {
            let (resolver, _cover_calls) = synthetic_cover_resolver();
            let (cover_port, cover) = synthetic_cover(SyntheticCover::Respond(response)).await;
            let target = synthetic_reality_target(private_key, cover_port, resolver.clone());
            let hello = synthetic_reality_client_hello([0; 8], synthetic_public_key(private_key));

            let (result, _peer) = setup_against_cover(hello, &target, &resolver).await;
            cover.abort();

            let error = result
                .err()
                .expect("an authenticated request needs a usable TLS 1.3 cover response");
            assert_eq!(error.kind(), io::ErrorKind::InvalidData, "{error}");
        }
    }

    #[tokio::test]
    async fn client_stops_on_cover_eof() {
        let private_key = [1u8; 32];
        let (resolver, cover_calls) = synthetic_cover_resolver();
        let (cover_port, cover) = synthetic_cover(SyntheticCover::Close).await;
        let target = synthetic_reality_target(private_key, cover_port, resolver.clone());
        let hello = synthetic_reality_client_hello([0; 8], synthetic_public_key(private_key));

        let (result, _peer) = setup_against_cover(hello, &target, &resolver).await;
        cover.abort();
        assert_eq!(cover_calls.load(Ordering::SeqCst), 1);

        let error = result
            .err()
            .expect("a closed cover must stop the connection");
        assert_eq!(error.kind(), io::ErrorKind::UnexpectedEof, "{error}");
    }

    #[tokio::test]
    async fn client_stops_on_cover_read_error() {
        let private_key = [1u8; 32];
        let (resolver, _cover_calls) = synthetic_cover_resolver();
        let (cover_port, cover) = synthetic_cover(SyntheticCover::Reset).await;
        let target = synthetic_reality_target(private_key, cover_port, resolver.clone());
        let hello = synthetic_reality_client_hello([0; 8], synthetic_public_key(private_key));

        let (result, _peer) = setup_against_cover(hello, &target, &resolver).await;
        cover.abort();

        let error = result
            .err()
            .expect("a reset cover must stop the connection");
        assert_eq!(error.kind(), io::ErrorKind::ConnectionReset, "{error}");
    }

    #[tokio::test]
    async fn client_stops_on_refused_cover() {
        let private_key = [1u8; 32];
        let (resolver, _cover_calls) = synthetic_cover_resolver();
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let refused_port = listener.local_addr().unwrap().port();
        drop(listener);
        let target = synthetic_reality_target(private_key, refused_port, resolver.clone());
        let hello = synthetic_reality_client_hello([0; 8], synthetic_public_key(private_key));

        let (result, _peer) = setup_against_cover(hello, &target, &resolver).await;

        let error = result
            .err()
            .expect("a refused cover must stop the connection");
        assert_eq!(error.kind(), io::ErrorKind::ConnectionRefused, "{error}");
    }

    #[tokio::test]
    async fn authenticated_client_stops_on_incomplete_cover_flight() {
        let private_key = [1u8; 32];
        let (resolver, _cover_calls) = synthetic_cover_resolver();
        let mut incomplete = synthetic_server_hello(true);
        incomplete.extend_from_slice(&tls_record(0x14, &[0x01]));
        let (cover_port, cover) = synthetic_cover(SyntheticCover::Respond(incomplete)).await;
        let target = synthetic_reality_target(private_key, cover_port, resolver.clone());
        let hello = synthetic_reality_client_hello([0; 8], synthetic_public_key(private_key));

        let started = Instant::now();
        let (result, _peer) = setup_against_cover(hello, &target, &resolver).await;
        let elapsed = started.elapsed();
        cover.abort();

        let error = result
            .err()
            .expect("a cover flight that stops before the handshake must stop the connection");
        assert_eq!(error.kind(), io::ErrorKind::TimedOut, "{error}");
        assert!(
            elapsed >= Duration::from_secs(4),
            "the cover deadline must expire before the connection stops, elapsed {elapsed:?}"
        );
    }

    #[tokio::test]
    async fn authenticated_client_stops_on_tls12_cover() {
        let private_key = [1u8; 32];
        let (resolver, _cover_calls) = synthetic_cover_resolver();
        let (cover_port, cover) =
            synthetic_cover(SyntheticCover::Respond(synthetic_server_hello(false))).await;
        let target = synthetic_reality_target(private_key, cover_port, resolver.clone());
        let hello = synthetic_reality_client_hello([0; 8], synthetic_public_key(private_key));

        let (result, _peer) = setup_against_cover(hello, &target, &resolver).await;
        cover.abort();

        let error = result
            .err()
            .expect("an authenticated request needs a usable TLS 1.3 cover");
        assert_eq!(error.kind(), io::ErrorKind::InvalidData, "{error}");
    }

    /// Control: a valid authenticated REALITY handshake still reaches the inner VLESS
    /// handler through a healthy synthetic cover.
    #[tokio::test]
    async fn healthy_authenticated_reality_handshake_still_succeeds() {
        for vision in [false, true] {
            for id in [SYNTHETIC_UUID, "550e8400-e29b-41d4-a716-446655440001"] {
                authenticated_reality_user(id, vision).await;
            }
        }
    }

    async fn authenticated_reality_user(id: &str, vision: bool) {
        use crate::option_util::OneOrSome;
        use crate::reality::{RealityClientConfig, RealityClientConnection};
        use crate::vless::vless_server_handler::{VlessTcpServerHandler, VlessUsers};

        let private_key = [3u8; 32];
        let public_key = synthetic_public_key(private_key);
        let (resolver, cover_calls) = synthetic_cover_resolver();
        let cover = interactive_cover().await;
        let mut target = synthetic_reality_target(private_key, cover.port, resolver.clone());
        let ids = OneOrSome::Some(vec![
            SYNTHETIC_UUID.into(),
            "550e8400-e29b-41d4-a716-446655440001".into(),
        ]);
        target.inner_protocol = if vision {
            InnerProtocol::VisionVless(crate::tls_server_handler::VisionVlessConfig {
                user_id: VlessUsers::new(&ids),
                udp_enabled: true,
                fallback: None,
            })
        } else {
            InnerProtocol::Normal(Box::new(VlessTcpServerHandler::new_users(
                &ids,
                true,
                target.effective_selector.clone(),
                resolver.clone(),
                None,
            )))
        };
        let mut request = synthetic_vless_tcp_request();
        request[1..17].copy_from_slice(&crate::uuid_util::parse_uuid(id).unwrap());
        if vision {
            let flow = b"xtls-rprx-vision";
            let mut addons = vec![(flow.len() + 2) as u8, 0x0a, flow.len() as u8];
            addons.extend_from_slice(flow);
            request.splice(17..18, addons);
        }

        let mut client = RealityClientConnection::new(RealityClientConfig {
            public_key,
            short_id: [0; 8],
            server_name: SYNTHETIC_COVER_HOSTNAME.into(),
            cipher_suites: Vec::new(),
        })
        .unwrap();
        let mut hello = Vec::new();
        client.write_tls(&mut hello).unwrap();

        let (server_io, mut client_io) = tokio::io::duplex(65540);
        // `write_tls` already drained the ClientHello and the handshake helper writes
        // only what is still queued, so the test sends the drained flight itself.
        client_io.write_all(&hello).await.unwrap();
        let client_task = tokio::spawn(async move {
            let connection = CryptoConnection::new_reality_client(client);
            let mut stream =
                CryptoTlsStream::handshake(client_io, connection, TlsReadMode::Stream, &[])
                    .await
                    .expect("client REALITY handshake");
            stream.write_all(&request).await.unwrap();
            stream.flush().await.unwrap();
            stream
        });

        let mut server_io: Box<dyn AsyncStream> = Box::new(server_io);
        let scenario = tokio::time::timeout(Duration::from_secs(15), async {
            let parsed = crate::tls_hello::read_client_hello(&mut server_io)
                .await
                .unwrap();
            assert!(parsed.supports_tls13);
            let setup = tokio::spawn(async move {
                setup_reality_server_stream(server_io, &target, parsed, &resolver).await
            });

            let mut cover_socket = cover.socket.await.unwrap();
            let mut forwarded = vec![0u8; hello.len()];
            cover_socket.read_exact(&mut forwarded).await.unwrap();
            assert_eq!(
                forwarded, hello,
                "the cover must receive the exact ClientHello"
            );
            cover_socket
                .write_all(&synthetic_tls13_flight())
                .await
                .unwrap();

            let result = setup.await.unwrap();
            let client_stream = client_task.await.unwrap();
            (result, client_stream)
        })
        .await
        .expect("the healthy authenticated scenario must finish inside 15 s");
        assert_eq!(
            cover_calls.load(Ordering::SeqCst),
            1,
            "the cover name must resolve only through the injected synthetic resolver"
        );
        match scenario.0 {
            Ok(TcpServerSetupResult::TcpForward {
                remote_location, ..
            }) => {
                assert_eq!(
                    remote_location,
                    NetLocation::new(
                        crate::address::Address::Ipv4(std::net::Ipv4Addr::LOCALHOST),
                        443
                    )
                );
            }
            Ok(_) => panic!("authenticated REALITY setup returned an unexpected variant"),
            Err(error) => panic!("authenticated REALITY setup failed: {error}"),
        }
        drop(scenario.1);
    }

    /// Control: unauthenticated traffic still returns a forwarding session that relays
    /// both directions through that same synthetic cover.
    #[tokio::test]
    async fn healthy_unauthenticated_forwarding_still_relays_through_cover() {
        let private_key = [2u8; 32];
        let (resolver, cover_calls) = synthetic_cover_resolver();
        let cover = interactive_cover().await;
        let target = synthetic_reality_target(private_key, cover.port, resolver.clone());
        let hello = synthetic_reality_client_hello([9; 8], synthetic_public_key(private_key));
        let flight = synthetic_tls13_flight();

        let (io, mut peer) = tokio::io::duplex(65540);
        peer.write_all(&hello).await.unwrap();
        let mut io: Box<dyn AsyncStream> = Box::new(io);
        let parsed = crate::tls_hello::read_client_hello(&mut io).await.unwrap();
        assert!(parsed.supports_tls13);
        let setup = tokio::spawn(async move {
            setup_reality_server_stream(io, &target, parsed, &resolver).await
        });

        tokio::time::timeout(Duration::from_secs(15), async {
            let mut cover_socket = cover.socket.await.unwrap();
            let mut forwarded = vec![0u8; hello.len()];
            cover_socket.read_exact(&mut forwarded).await.unwrap();
            assert_eq!(forwarded, hello);
            cover_socket.write_all(&flight).await.unwrap();

            let result = setup.await.unwrap();
            let TcpServerSetupResult::Session(session) =
                result.expect("a healthy cover must still forward unauthenticated traffic")
            else {
                panic!("unauthenticated traffic must return a forwarding session");
            };

            let relay = tokio::spawn(session);
            let mut preread = vec![0u8; flight.len()];
            peer.read_exact(&mut preread).await.unwrap();
            assert_eq!(
                preread, flight,
                "the cover flight must reach the client first"
            );
            peer.write_all(b"client-live").await.unwrap();
            let mut uploaded = [0u8; 11];
            cover_socket.read_exact(&mut uploaded).await.unwrap();
            assert_eq!(&uploaded, b"client-live");
            cover_socket.write_all(b"cover-live").await.unwrap();
            let mut downloaded = [0u8; 10];
            peer.read_exact(&mut downloaded).await.unwrap();
            assert_eq!(&downloaded, b"cover-live");
            relay.abort();
        })
        .await
        .expect("the healthy forwarding scenario must finish inside 15 s");
        assert_eq!(
            cover_calls.load(Ordering::SeqCst),
            1,
            "the cover name must resolve only through the injected synthetic resolver"
        );
    }

    /// Control: a TLS 1.2 client with a TLS 1.2 cover keeps ordinary forwarding.
    #[tokio::test]
    async fn healthy_tls12_cover_still_forwards_tls12_client() {
        let private_key = [4u8; 32];
        let (resolver, cover_calls) = synthetic_cover_resolver();
        let cover = interactive_cover().await;
        let target = synthetic_reality_target(private_key, cover.port, resolver.clone());
        let (hello, cover_flight) = synthetic_tls12_handshake();

        let (io, mut peer) = tokio::io::duplex(65540);
        peer.write_all(&hello).await.unwrap();
        let mut io: Box<dyn AsyncStream> = Box::new(io);
        let parsed = crate::tls_hello::read_client_hello(&mut io).await.unwrap();
        assert!(!parsed.supports_tls13);
        let setup = tokio::spawn(async move {
            setup_reality_server_stream(io, &target, parsed, &resolver).await
        });

        tokio::time::timeout(Duration::from_secs(15), async {
            let mut cover_socket = cover.socket.await.unwrap();
            let mut forwarded = vec![0u8; hello.len()];
            cover_socket.read_exact(&mut forwarded).await.unwrap();
            assert_eq!(forwarded, hello);
            cover_socket.write_all(&cover_flight).await.unwrap();

            let result = setup.await.unwrap();
            let TcpServerSetupResult::Session(session) =
                result.expect("a TLS 1.2 cover must still forward unauthenticated traffic")
            else {
                panic!("a TLS 1.2 client must return a forwarding session");
            };

            let relay = tokio::spawn(session);
            let mut preread = vec![0u8; cover_flight.len()];
            peer.read_exact(&mut preread).await.unwrap();
            assert_eq!(
                preread, cover_flight,
                "the TLS 1.2 cover flight must reach the client first"
            );
            peer.write_all(b"tls12-live").await.unwrap();
            let mut uploaded = [0u8; 10];
            cover_socket.read_exact(&mut uploaded).await.unwrap();
            assert_eq!(&uploaded, b"tls12-live");
            relay.abort();
        })
        .await
        .expect("the healthy TLS 1.2 scenario must finish inside 15 s");
        assert_eq!(
            cover_calls.load(Ordering::SeqCst),
            1,
            "the cover name must resolve only through the injected synthetic resolver"
        );
    }
}
