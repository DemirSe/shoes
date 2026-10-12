//! SocketConnectorImpl - Implementation of SocketConnector trait.
//!
//! Handles TCP and QUIC transports with bind_interface support.
//! Created from the socket-related fields of any ClientConfig.

use std::net::SocketAddr;
use std::pin::Pin;
use std::sync::Arc;
use std::task::{Context, Poll};

use async_trait::async_trait;
use futures::stream::{FuturesUnordered, StreamExt};
use log::{debug, error};
use tokio::io::ReadBuf;
use tokio::net::UdpSocket;

use crate::address::ResolvedLocation;
use crate::async_stream::AsyncStream;
use crate::config::ClientConfig;
use crate::resolver::{Resolver, resolve_addresses, resolve_location};
use crate::socket_util::{new_tcp_socket, new_udp_socket, set_tcp_keepalive};

use super::socket_connector::SocketConnector;

const TCP_FALLBACK_DELAY: std::time::Duration = std::time::Duration::from_millis(250);
const TCP_ATTEMPT_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(10);
const MAX_TCP_ATTEMPTS: usize = 2;

fn interleave_tcp_addresses(addresses: Vec<SocketAddr>) -> Vec<SocketAddr> {
    let Some(first) = addresses.first() else {
        return addresses;
    };
    let prefer_ipv6 = first.is_ipv6();
    let (preferred, alternate): (Vec<_>, Vec<_>) = addresses
        .into_iter()
        .partition(|address| address.is_ipv6() == prefer_ipv6);
    let mut alternate = alternate.into_iter();
    let mut ordered = Vec::with_capacity(preferred.len() + alternate.len());
    for address in preferred {
        ordered.push(address);
        if let Some(address) = alternate.next() {
            ordered.push(address);
        }
    }
    ordered.extend(alternate);
    ordered
}

async fn connect_tcp_candidates<T, F>(
    addresses: Vec<SocketAddr>,
    mut connect: impl FnMut(SocketAddr) -> std::io::Result<F>,
) -> std::io::Result<T>
where
    F: std::future::Future<Output = std::io::Result<T>>,
{
    if addresses.len() == 1 {
        return connect(addresses[0])?.await;
    }

    let mut candidates = interleave_tcp_addresses(addresses).into_iter().enumerate();
    let mut attempts = FuturesUnordered::new();
    let mut next_launch = tokio::time::Instant::now();
    let mut last_error: Option<(usize, std::io::Error)> = None;

    while candidates.len() != 0 || !attempts.is_empty() {
        tokio::select! {
            biased;
            Some((index, address, result)) = attempts.next(), if !attempts.is_empty() => {
                match result {
                    Ok(stream) => return Ok(stream),
                    Err(error) => {
                        debug!("TCP connect to {address} failed: {error}");
                        if last_error.as_ref().is_none_or(|(previous, _)| index > *previous) {
                            last_error = Some((index, error));
                        }
                        next_launch = tokio::time::Instant::now();
                    }
                }
            }
            _ = tokio::time::sleep_until(next_launch),
                if candidates.len() != 0 && attempts.len() < MAX_TCP_ATTEMPTS =>
            {
                let (index, address) = candidates.next().unwrap();
                // Socket creation, binding and protection still fail closed.
                let attempt = connect(address)?;
                attempts.push(async move {
                    let result = tokio::time::timeout(TCP_ATTEMPT_TIMEOUT, attempt)
                        .await
                        .unwrap_or_else(|_| Err(std::io::Error::new(
                            std::io::ErrorKind::TimedOut, format!("TCP connect to {address} timed out"),
                        )));
                    (index, address, result)
                });
                next_launch = tokio::time::Instant::now() + TCP_FALLBACK_DELAY;
            }
        }
    }
    Err(last_error
        .map(|(_, error)| error)
        .unwrap_or_else(|| std::io::Error::other("no resolved addresses succeeded")))
}

#[derive(Debug)]
enum TransportConfig {
    Tcp { no_delay: bool },
}

/// Implementation of SocketConnector for the TCP transport.
///
/// Created from the socket-related fields of any ClientConfig:
/// - `bind_interface`
/// - `transport`
/// - `tcp_settings`
#[derive(Debug)]
pub struct SocketConnectorImpl {
    bind_interface: Option<String>,
    transport: TransportConfig,
}

impl SocketConnectorImpl {
    /// Create a SocketConnector from a ClientConfig's socket-related fields.
    ///
    /// # Arguments
    /// * `config` - The client config (socket fields are extracted)
    pub fn from_config(config: &ClientConfig) -> Self {
        let bind_interface = config.bind_interface.clone().into_option();

        let no_delay = config
            .tcp_settings
            .as_ref()
            .map(|tc| tc.no_delay)
            .unwrap_or(true);

        Self {
            bind_interface,
            transport: TransportConfig::Tcp { no_delay },
        }
    }

    /// Create a simple TCP SocketConnector for direct connections.
    ///
    /// Used when only TCP is needed (no QUIC).
    #[cfg(test)]
    pub fn new_tcp(bind_interface: Option<String>, no_delay: bool) -> Self {
        Self {
            bind_interface,
            transport: TransportConfig::Tcp { no_delay },
        }
    }
}

#[async_trait]
impl SocketConnector for SocketConnectorImpl {
    async fn connect(
        &self,
        resolver: &Arc<dyn Resolver>,
        address: &ResolvedLocation,
    ) -> std::io::Result<Box<dyn AsyncStream>> {
        let target_addrs = match address.resolved_addr() {
            Some(r) => vec![r],
            None => resolve_addresses(resolver, address.location()).await?,
        };

        match &self.transport {
            TransportConfig::Tcp { no_delay } => {
                let stream = connect_tcp_candidates(target_addrs, |address| {
                    let socket = new_tcp_socket(self.bind_interface.clone(), address.is_ipv6())?;
                    Ok(socket.connect(address))
                })
                .await?;
                if let Err(e) = set_tcp_keepalive(
                    &stream,
                    std::time::Duration::from_secs(120),
                    std::time::Duration::from_secs(30),
                ) {
                    error!("Failed to set TCP keepalive: {e}");
                }
                if *no_delay && let Err(e) = stream.set_nodelay(true) {
                    error!("Failed to set TCP no-delay: {e}");
                }
                Ok(Box::new(stream))
            }
        }
    }

    async fn connect_udp_bidirectional(
        &self,
        resolver: &Arc<dyn Resolver>,
        mut target: ResolvedLocation,
    ) -> std::io::Result<Box<dyn crate::async_stream::AsyncMessageStream>> {
        debug!(
            "[SocketConnector] connect_udp_bidirectional called, target: {}",
            target.location()
        );

        let remote_addr = resolve_location(&mut target, resolver).await?;
        let client_socket = new_udp_socket(remote_addr.is_ipv6(), self.bind_interface.clone())?;

        // Keep the socket unconnected: the wrapper drops datagrams from any
        // source other than this resolved peer while the send path and the
        // bind-interface setup stay unchanged.
        Ok(Box::new(UnconnectedUdpSocket::new(
            client_socket,
            remote_addr,
        )))
    }

    fn bind_interface(&self) -> Option<&str> {
        self.bind_interface.as_deref()
    }
}

/// A UDP socket wrapper that tracks the destination and uses send_to/recv_from.
///
/// Only datagrams sent by the configured peer are delivered. A connected socket
/// would filter foreign sources as well, but this adapter keeps
/// `send_to`/`recv_from` so the bind-interface and hostname resolution behavior
/// of the egress socket stays unchanged.
struct UnconnectedUdpSocket {
    socket: UdpSocket,
    destination: SocketAddr,
}

/// Foreign datagrams are discarded in bounded batches: one poll call must not
/// spend the whole task budget dropping them, and a reply that the configured
/// peer had already queued is still delivered.
const MAX_FOREIGN_DATAGRAMS_PER_POLL: usize = 16;

/// True when `source` is the peer this adapter sends to.
///
/// The port must match: a UDP client only accepts a reply from the address it
/// addressed. An IPv4-mapped IPv6 source describes the same IPv4 peer, and the
/// IPv6 scope id is ignored because the kernel fills it in from the receiving
/// interface instead of from the peer.
fn is_configured_peer(source: &SocketAddr, destination: &SocketAddr) -> bool {
    fn unmap(ip: &std::net::IpAddr) -> std::net::IpAddr {
        match ip {
            std::net::IpAddr::V6(v6) => match v6.to_ipv4_mapped() {
                Some(v4) => std::net::IpAddr::V4(v4),
                None => *ip,
            },
            std::net::IpAddr::V4(_) => *ip,
        }
    }

    source.port() == destination.port() && unmap(&source.ip()) == unmap(&destination.ip())
}

impl UnconnectedUdpSocket {
    fn new(socket: UdpSocket, destination: SocketAddr) -> Self {
        Self {
            socket,
            destination,
        }
    }
}

impl crate::async_stream::AsyncReadMessage for UnconnectedUdpSocket {
    fn read_message_eof_on_empty(&self) -> bool {
        false
    }
    fn poll_read_message(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut ReadBuf<'_>,
    ) -> Poll<std::io::Result<()>> {
        let this = self.get_mut();
        for _ in 0..MAX_FOREIGN_DATAGRAMS_PER_POLL {
            match this.socket.poll_recv_from(cx, buf) {
                Poll::Ready(Ok(addr)) => {
                    if is_configured_peer(&addr, &this.destination) {
                        log::debug!(
                            "[UnconnectedUdp] Received {} bytes from {} (target: {})",
                            buf.filled().len(),
                            addr,
                            this.destination
                        );
                        return Poll::Ready(Ok(()));
                    }
                    // A datagram from any other source is dropped. It is not an
                    // error and must not end the association, so the socket stays
                    // open for the configured peer.
                    log::debug!(
                        "[UnconnectedUdp] Dropped {} bytes from foreign source {} (target: {})",
                        buf.filled().len(),
                        addr,
                        this.destination
                    );
                    buf.clear();
                }
                Poll::Ready(Err(e)) => return Poll::Ready(Err(e)),
                Poll::Pending => return Poll::Pending,
            }
        }

        // The batch budget is spent while the socket may still hold data. Wake
        // this task again so the remaining datagrams are handled later instead
        // of waiting for a new readiness edge that will not come.
        cx.waker().wake_by_ref();
        Poll::Pending
    }
}

impl crate::async_stream::AsyncWriteMessage for UnconnectedUdpSocket {
    fn poll_write_message(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &[u8],
    ) -> Poll<std::io::Result<()>> {
        let this = self.get_mut();
        this.socket
            .poll_send_to(cx, buf, this.destination)
            .map(|r| r.map(|_| ()))
    }
}

impl crate::async_stream::AsyncFlushMessage for UnconnectedUdpSocket {
    fn poll_flush_message(
        self: Pin<&mut Self>,
        _cx: &mut Context<'_>,
    ) -> Poll<std::io::Result<()>> {
        Poll::Ready(Ok(()))
    }
}

impl crate::async_stream::AsyncShutdownMessage for UnconnectedUdpSocket {
    fn poll_shutdown_message(
        self: Pin<&mut Self>,
        _cx: &mut Context<'_>,
    ) -> Poll<std::io::Result<()>> {
        Poll::Ready(Ok(()))
    }
}

impl crate::async_stream::AsyncPing for UnconnectedUdpSocket {
    fn supports_ping(&self) -> bool {
        false
    }

    fn poll_write_ping(self: Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<std::io::Result<bool>> {
        Poll::Ready(Ok(false))
    }
}

impl crate::async_stream::AsyncMessageStream for UnconnectedUdpSocket {}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::address::NetLocation;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::time::Duration;
    use tokio::time::Instant;

    fn addresses(count: u16) -> Vec<SocketAddr> {
        (1..=count)
            .map(|port| SocketAddr::from(([127, 0, 0, 1], port)))
            .collect()
    }

    #[derive(Default)]
    struct Attempts {
        active: AtomicUsize,
        peak: AtomicUsize,
        starts: parking_lot::Mutex<Vec<(SocketAddr, Instant)>>,
    }

    struct ActiveAttempt(Arc<Attempts>);

    impl Attempts {
        fn start(self: &Arc<Self>, address: SocketAddr) -> ActiveAttempt {
            let active = self.active.fetch_add(1, Ordering::Relaxed) + 1;
            self.peak.fetch_max(active, Ordering::Relaxed);
            self.starts.lock().push((address, Instant::now()));
            ActiveAttempt(self.clone())
        }
    }

    impl Drop for ActiveAttempt {
        fn drop(&mut self) {
            self.0.active.fetch_sub(1, Ordering::Relaxed);
        }
    }

    #[test]
    fn address_interleaving_preserves_resolver_family_preference() {
        let v4 = addresses(3);
        let v6: Vec<SocketAddr> = ["[::1]:1", "[::1]:2"].map(|s| s.parse().unwrap()).to_vec();
        assert_eq!(
            interleave_tcp_addresses(vec![v4[0], v4[1], v6[0], v6[1], v4[2]]),
            vec![v4[0], v6[0], v4[1], v6[1], v4[2]]
        );
        assert_eq!(
            interleave_tcp_addresses(vec![v6[0], v6[1], v4[0], v4[1]]),
            vec![v6[0], v4[0], v6[1], v4[1]]
        );
        assert_eq!(interleave_tcp_addresses(v4.clone()), v4);
        assert!(interleave_tcp_addresses(vec![]).is_empty());
    }

    #[tokio::test(start_paused = true)]
    async fn stagger_keeps_slow_viable_attempts_and_cancels_losers() {
        let attempts = Arc::new(Attempts::default());
        let start = Instant::now();
        let winner = connect_tcp_candidates(addresses(3), |address| {
            let guard = attempts.start(address);
            Ok(async move {
                let _guard = guard;
                if address.port() != 1 {
                    std::future::pending::<()>().await;
                }
                tokio::time::sleep(Duration::from_millis(750)).await;
                Ok(address)
            })
        })
        .await
        .unwrap();
        assert_eq!(winner.port(), 1);
        assert_eq!(start.elapsed(), Duration::from_millis(750));
        let starts = attempts.starts.lock();
        assert_eq!(starts.len(), 2);
        assert_eq!(starts[1].1 - starts[0].1, TCP_FALLBACK_DELAY);
        assert_eq!(attempts.peak.load(Ordering::Relaxed), 2);
        assert_eq!(attempts.active.load(Ordering::Relaxed), 0);
    }

    #[tokio::test(start_paused = true)]
    async fn attempt_timeout_reaches_later_candidates_without_exceeding_the_cap() {
        let attempts = Arc::new(Attempts::default());
        let start = Instant::now();
        let winner = connect_tcp_candidates(addresses(4), |address| {
            let guard = attempts.start(address);
            Ok(async move {
                let _guard = guard;
                if address.port() < 3 {
                    std::future::pending::<()>().await;
                }
                Ok(address)
            })
        })
        .await
        .unwrap();
        assert_eq!(winner.port(), 3);
        assert_eq!(start.elapsed(), TCP_ATTEMPT_TIMEOUT);
        assert_eq!(attempts.starts.lock().len(), 3);
        assert_eq!(attempts.peak.load(Ordering::Relaxed), MAX_TCP_ATTEMPTS);
        assert_eq!(attempts.active.load(Ordering::Relaxed), 0);
    }

    #[tokio::test(start_paused = true)]
    async fn failures_advance_immediately_and_singletons_keep_their_deadline() {
        let start = Instant::now();
        let error = connect_tcp_candidates(addresses(3), |address| {
            Ok(async move { Err::<(), _>(std::io::Error::other(address.to_string())) })
        })
        .await
        .unwrap_err();
        assert_eq!(start.elapsed(), Duration::ZERO);
        assert_eq!(error.to_string(), "127.0.0.1:3");
        connect_tcp_candidates(addresses(1), |_| {
            Ok(async {
                tokio::time::sleep(TCP_ATTEMPT_TIMEOUT * 2).await;
                Ok(())
            })
        })
        .await
        .unwrap();
        assert_eq!(start.elapsed(), TCP_ATTEMPT_TIMEOUT * 2);
    }

    #[tokio::test(start_paused = true)]
    async fn outer_cancellation_drops_every_pending_socket() {
        let attempts = Arc::new(Attempts::default());
        let mut connect = Box::pin(connect_tcp_candidates(addresses(4), |address| {
            let guard = attempts.start(address);
            Ok(async move {
                let _guard = guard;
                std::future::pending::<std::io::Result<()>>().await
            })
        }));
        assert!(futures::poll!(&mut connect).is_pending());
        tokio::time::advance(TCP_FALLBACK_DELAY).await;
        assert!(futures::poll!(&mut connect).is_pending());
        assert_eq!(attempts.active.load(Ordering::Relaxed), 2);
        drop(connect);
        assert_eq!(attempts.active.load(Ordering::Relaxed), 0);
    }

    #[tokio::test(start_paused = true)]
    async fn socket_creation_failure_does_not_bypass_protection_or_binding() {
        let attempts = Arc::new(Attempts::default());
        let error = connect_tcp_candidates(addresses(3), |address| {
            if address.port() == 2 {
                return Err(std::io::ErrorKind::PermissionDenied.into());
            }
            let guard = attempts.start(address);
            Ok(async move {
                let _guard = guard;
                std::future::pending::<std::io::Result<()>>().await
            })
        })
        .await
        .unwrap_err();
        assert_eq!(error.kind(), std::io::ErrorKind::PermissionDenied);
        assert_eq!(attempts.starts.lock().len(), 1);
        assert_eq!(attempts.active.load(Ordering::Relaxed), 0);
    }

    #[tokio::test]
    async fn pinned_tcp_target_bypasses_dns() {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        #[derive(Debug)]
        struct NoDns;
        impl Resolver for NoDns {
            fn resolve_location(
                &self,
                _: &NetLocation,
            ) -> Pin<Box<dyn std::future::Future<Output = std::io::Result<Vec<SocketAddr>>> + Send>>
            {
                panic!("a policy-pinned target must not resolve again")
            }
        }
        let listener = tokio::net::TcpListener::bind("0.0.0.0:0").await.unwrap();
        let address = SocketAddr::from(([127, 0, 0, 1], listener.local_addr().unwrap().port()));
        let target = ResolvedLocation::with_resolved(
            NetLocation::from_str("unknown.invalid:443", None).unwrap(),
            address,
        );
        let resolver: Arc<dyn Resolver> = Arc::new(NoDns);
        let mut stream = SocketConnectorImpl::new_tcp(None, true)
            .connect(&resolver, &target)
            .await
            .unwrap();
        let (mut peer, _) = listener.accept().await.unwrap();
        stream.write_all(b"pinned").await.unwrap();
        let mut data = [0; 6];
        peer.read_exact(&mut data).await.unwrap();
        assert_eq!(&data, b"pinned");
    }

    #[test]
    fn test_new_tcp() {
        let connector = SocketConnectorImpl::new_tcp(Some("eth0".to_string()), true);
        assert!(matches!(
            connector.transport,
            TransportConfig::Tcp { no_delay: true }
        ));
        assert_eq!(connector.bind_interface, Some("eth0".to_string()));
    }

    #[test]
    fn test_from_config_direct_protocol() {
        let config = ClientConfig::default(); // default is direct protocol
        let connector = SocketConnectorImpl::from_config(&config);
        assert!(matches!(connector.transport, TransportConfig::Tcp { .. }));
    }

    /// Read one message through the adapter, the way the UDP router does.
    async fn read_message(adapter: &mut UnconnectedUdpSocket) -> std::io::Result<Vec<u8>> {
        use crate::async_stream::AsyncReadMessage;
        std::future::poll_fn(|cx| {
            let mut bytes = [0u8; 64];
            let mut buf = ReadBuf::new(&mut bytes);
            match Pin::new(&mut *adapter).poll_read_message(cx, &mut buf) {
                Poll::Ready(Ok(())) => Poll::Ready(Ok(buf.filled().to_vec())),
                Poll::Ready(Err(error)) => Poll::Ready(Err(error)),
                Poll::Pending => Poll::Pending,
            }
        })
        .await
    }

    /// Bind a loopback UDP socket and return it with its address.
    async fn bound(address: &str) -> (UdpSocket, SocketAddr) {
        let socket = UdpSocket::bind(address).await.unwrap();
        let local = socket.local_addr().unwrap();
        (socket, local)
    }

    #[tokio::test]
    async fn a_foreign_source_cannot_answer_for_the_configured_peer() {
        let (peer, peer_addr) = bound("127.0.0.1:0").await;
        let (egress, egress_addr) = bound("127.0.0.1:0").await;
        let mut adapter = UnconnectedUdpSocket::new(egress, peer_addr);

        // A different local address and a different local port are both foreign.
        let (foreign_ip, _) = bound("127.0.0.2:0").await;
        foreign_ip
            .send_to(b"foreign-ip", egress_addr)
            .await
            .unwrap();
        let (foreign_port, _) = bound("127.0.0.1:0").await;
        foreign_port
            .send_to(b"foreign-port", egress_addr)
            .await
            .unwrap();

        let injected =
            tokio::time::timeout(Duration::from_millis(250), read_message(&mut adapter)).await;
        assert!(
            injected.is_err(),
            "a datagram from a foreign source must not be delivered as the peer's reply: {injected:?}"
        );

        // The configured peer still gets through, including an empty reply.
        peer.send_to(b"legit", egress_addr).await.unwrap();
        assert_eq!(read_message(&mut adapter).await.unwrap(), b"legit");
        peer.send_to(b"", egress_addr).await.unwrap();
        assert!(read_message(&mut adapter).await.unwrap().is_empty());
    }

    #[tokio::test]
    async fn a_mixed_batch_drops_only_foreign_datagrams() {
        let (peer, peer_addr) = bound("127.0.0.1:0").await;
        let (egress, egress_addr) = bound("127.0.0.1:0").await;
        let mut adapter = UnconnectedUdpSocket::new(egress, peer_addr);
        let (foreign, _) = bound("127.0.0.1:0").await;

        foreign.send_to(b"injected", egress_addr).await.unwrap();
        peer.send_to(b"first", egress_addr).await.unwrap();
        foreign.send_to(b"injected", egress_addr).await.unwrap();
        peer.send_to(b"second", egress_addr).await.unwrap();

        assert_eq!(read_message(&mut adapter).await.unwrap(), b"first");
        assert_eq!(read_message(&mut adapter).await.unwrap(), b"second");
    }

    #[tokio::test]
    async fn ipv6_replies_require_the_configured_source_port() {
        let (peer, peer_addr) = bound("[::1]:0").await;
        let (egress, egress_addr) = bound("[::1]:0").await;
        let mut adapter = UnconnectedUdpSocket::new(egress, peer_addr);

        let (foreign_port, _) = bound("[::1]:0").await;
        foreign_port
            .send_to(b"foreign-port", egress_addr)
            .await
            .unwrap();
        let injected =
            tokio::time::timeout(Duration::from_millis(250), read_message(&mut adapter)).await;
        assert!(
            injected.is_err(),
            "a datagram from another port must not be delivered: {injected:?}"
        );

        peer.send_to(b"v6", egress_addr).await.unwrap();
        assert_eq!(read_message(&mut adapter).await.unwrap(), b"v6");
    }

    #[test]
    fn only_the_resolved_peer_address_is_accepted() {
        let destination: SocketAddr = "127.0.0.1:9000".parse().unwrap();
        assert!(is_configured_peer(&destination, &destination));
        assert!(!is_configured_peer(
            &"127.0.0.1:9001".parse().unwrap(),
            &destination
        ));
        assert!(!is_configured_peer(
            &"127.0.0.2:9000".parse().unwrap(),
            &destination
        ));
        // An IPv4-mapped IPv6 source describes the same IPv4 peer.
        assert!(is_configured_peer(
            &"[::ffff:127.0.0.1]:9000".parse().unwrap(),
            &destination
        ));
        // The scope id of an IPv6 source depends on the receiving interface,
        // not on the peer identity.
        assert!(is_configured_peer(
            &"[::1%1]:9000".parse().unwrap(),
            &"[::1]:9000".parse().unwrap()
        ));
    }

    #[tokio::test]
    async fn a_foreign_flood_still_delivers_a_queued_peer_reply() {
        let (peer, peer_addr) = bound("127.0.0.1:0").await;
        let (egress, egress_addr) = bound("127.0.0.1:0").await;
        let mut adapter = UnconnectedUdpSocket::new(egress, peer_addr);
        let (foreign, _) = bound("127.0.0.1:0").await;

        for _ in 0..32 {
            foreign.send_to(b"injected", egress_addr).await.unwrap();
        }
        peer.send_to(b"legit", egress_addr).await.unwrap();

        let delivered = tokio::time::timeout(Duration::from_secs(2), read_message(&mut adapter))
            .await
            .expect("a queued peer reply must not be stranded behind foreign datagrams")
            .unwrap();
        assert_eq!(delivered, b"legit");
    }
}
