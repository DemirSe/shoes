//! Port allocation and readiness helper for tests
//!
//! Provides centralized port allocation with tracking and synchronization.
//! All tests should use PortHelper instead of directly allocating ports.

use std::collections::HashSet;
use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr, TcpListener, UdpSocket};
use std::sync::{Mutex, OnceLock};
use tokio::net::TcpStream;
use tokio::time::{Duration, Instant, sleep, timeout};

/// Address and port pairs this process has already handed out.
///
/// The availability probe binds a listener and then releases it, so two parallel
/// tests in the same process could otherwise be handed the same pair and the
/// second fixture bind would fail with `AddrInUse`. Every allocation reserves
/// its pair here before it is returned.
static ALLOCATED_PORTS: OnceLock<Mutex<HashSet<(IpAddr, u16)>>> = OnceLock::new();

fn allocated_ports() -> &'static Mutex<HashSet<(IpAddr, u16)>> {
    ALLOCATED_PORTS.get_or_init(|| Mutex::new(HashSet::new()))
}

/// Reserve one pair for this process, or report that it is already handed out.
fn reserve_port(ip: IpAddr, port: u16) -> bool {
    allocated_ports()
        .lock()
        .expect("port reservation lock is not poisoned")
        .insert((ip, port))
}

/// True when this process has not handed the pair out yet.
fn port_is_available_to_this_process(ip: IpAddr, port: u16) -> bool {
    !allocated_ports()
        .lock()
        .expect("port reservation lock is not poisoned")
        .contains(&(ip, port))
}

/// True when the operating system can still bind the pair for TCP and UDP.
fn os_accepts_both_protocols(ip: IpAddr, port: u16) -> bool {
    TcpListener::bind(SocketAddr::new(ip, port)).is_ok()
        && UdpSocket::bind(SocketAddr::new(ip, port)).is_ok()
}

/// Helper for allocating random ports and waiting for them to be ready.
/// Tracks all listener ports so we can wait for them to be ready before tests run.
#[derive(Default)]
pub struct PortHelper {
    listener_addrs: Vec<SocketAddr>,
}

impl PortHelper {
    /// Create a new PortHelper
    pub fn new() -> Self {
        Self::default()
    }

    /// Get a random available address (IP, port) for a listening server.
    /// Returns (IP as String, port) where IP is in range 127.0.0.1-127.0.0.255.
    /// This address will be tracked and waited for in wait_for_all_ports().
    pub fn get_listener_port(&mut self) -> (String, u16) {
        let (ip, port) = get_random_addr();
        self.listener_addrs
            .push(format!("{ip}:{port}").parse().unwrap());
        (ip, port)
    }

    /// Get a random available address for a client/connector.
    /// This address will NOT be tracked (clients don't listen).
    pub fn get_port(&mut self) -> (String, u16) {
        get_random_addr()
    }

    /// Get a random available address for a QUIC/UDP listener.
    /// This address will NOT be tracked for TCP readiness checks since QUIC uses UDP.
    /// Use this for Hysteria2, TUIC, and other QUIC-based servers.
    pub fn get_quic_listener_port(&mut self) -> (String, u16) {
        get_random_udp_addr()
    }

    /// Get a random available port on 127.0.0.1 for a listening server.
    ///
    /// IMPORTANT: This is required for tests using libcronet (sing-box naive outbound).
    /// libcronet rejects connections to non-127.0.0.1 loopback addresses (e.g., 127.x.y.z)
    /// with "connection refused" or "invalid argument" errors. This appears to be a
    /// security restriction in Chromium's network stack.
    ///
    /// Returns ("127.0.0.1", port).
    pub fn get_localhost_listener_port(&mut self) -> (String, u16) {
        for _ in 0..64 {
            let listener =
                TcpListener::bind("127.0.0.1:0").expect("failed to allocate a localhost test port");
            let port = listener
                .local_addr()
                .expect("failed to read allocated localhost test port")
                .port();
            if !reserve_port(IpAddr::V4(Ipv4Addr::LOCALHOST), port) {
                continue;
            }
            let addr = ("127.0.0.1".to_string(), port);
            self.listener_addrs
                .push(format!("{}:{}", addr.0, addr.1).parse().unwrap());
            return addr;
        }
        panic!("no localhost test port is available");
    }

    pub fn get_ipv6_listener_port(&mut self) -> (String, u16) {
        for _ in 0..64 {
            let listener = TcpListener::bind((Ipv6Addr::LOCALHOST, 0))
                .expect("failed to allocate an IPv6 test port");
            let address = listener
                .local_addr()
                .expect("failed to read the allocated IPv6 test port");
            if !reserve_port(IpAddr::V6(Ipv6Addr::LOCALHOST), address.port()) {
                continue;
            }
            self.listener_addrs.push(address);
            return (address.ip().to_string(), address.port());
        }
        panic!("no IPv6 test port is available");
    }

    /// Wait for all tracked listener addresses to be ready for connections.
    /// Waits for all addresses in parallel with a 30 second timeout per address.
    /// If any address doesn't become ready, returns an error.
    pub async fn wait_for_all_ports(&mut self) -> std::io::Result<()> {
        use futures::future::join_all;

        let futures: Vec<_> = std::mem::take(&mut self.listener_addrs)
            .into_iter()
            .map(|addr| wait_for_addr(addr, Duration::from_secs(30)))
            .collect();

        let results = join_all(futures).await;

        // Check if any failed
        for result in results {
            result?;
        }

        Ok(())
    }
}

/// Get a random available address (IP, port) by generating random IP in 127.0.0.1-255 range
/// and random port. This dramatically reduces collision probability in parallel test execution
/// by expanding the address space.
fn get_random_addr() -> (String, u16) {
    use rand::RngExt;
    let mut rng = rand::rng();

    for _ in 0..10 {
        // Generate random IP in 127.0.0.1-255 range
        let ip_suffix1: u8 = rng.random_range(0..=255);
        let ip_suffix2: u8 = rng.random_range(0..=255);
        let ip_suffix3: u8 = rng.random_range(1..=255);
        let ip = Ipv4Addr::new(127, ip_suffix1, ip_suffix2, ip_suffix3);
        if ip == Ipv4Addr::LOCALHOST {
            // don't use localhost since it might be used up
            continue;
        }

        if let Ok(listener) = TcpListener::bind((ip, 0)) {
            let port = listener.local_addr().unwrap().port();
            if UdpSocket::bind((ip, port)).is_ok() && reserve_port(IpAddr::V4(ip), port) {
                return (ip.to_string(), port);
            }
        }
    }

    unreachable!();
}

/// Get one port that is free on both loopback families.
///
/// Some fixtures bind the same port on `127.0.0.1` and `::1`, for example to
/// compare an IPv4 and an IPv6 echo server. Both pairs are reserved, so no other
/// test in this process can be handed them.
pub fn get_dual_stack_listener_port() -> u16 {
    use rand::RngExt;
    let mut rng = rand::rng();

    for _ in 0..64 {
        let port: u16 = rng.random_range(20_000..60_000);
        if !port_is_available_to_this_process(IpAddr::V4(Ipv4Addr::LOCALHOST), port) {
            continue;
        }
        if !port_is_available_to_this_process(IpAddr::V6(Ipv6Addr::LOCALHOST), port) {
            continue;
        }
        if !os_accepts_both_protocols(IpAddr::V4(Ipv4Addr::LOCALHOST), port) {
            continue;
        }
        if !os_accepts_both_protocols(IpAddr::V6(Ipv6Addr::LOCALHOST), port) {
            continue;
        }
        if !reserve_port(IpAddr::V4(Ipv4Addr::LOCALHOST), port) {
            continue;
        }
        if !reserve_port(IpAddr::V6(Ipv6Addr::LOCALHOST), port) {
            continue;
        }
        return port;
    }

    panic!("no dual-stack test port is available");
}

fn get_random_udp_addr() -> (String, u16) {
    let (ip, _) = get_random_addr();
    let socket = UdpSocket::bind(format!("{ip}:0")).expect("failed to allocate a UDP test port");
    let port = socket
        .local_addr()
        .expect("failed to read allocated UDP test port")
        .port();
    (ip, port)
}

/// Wait for an address (IP, port) to be ready for connections by attempting TCP connections.
/// Retries every 100ms, capped by the remaining timeout duration.
/// Returns Ok(()) if address becomes available, Err if timeout is reached.
async fn wait_for_addr(addr: SocketAddr, wait_timeout: Duration) -> std::io::Result<()> {
    let deadline = Instant::now() + wait_timeout;

    loop {
        let now = Instant::now();
        if now >= deadline {
            return Err(std::io::Error::new(
                std::io::ErrorKind::TimedOut,
                format!("Address {addr} did not become ready within {wait_timeout:?}"),
            ));
        }

        let remaining = deadline - now;
        if timeout(remaining, TcpStream::connect(addr))
            .await
            .is_ok_and(|result| result.is_ok())
        {
            return Ok(());
        }

        // A failed connect must not add a full retry interval past the deadline.
        sleep(readiness_retry_delay(
            deadline.saturating_duration_since(Instant::now()),
        ))
        .await;
    }
}

fn readiness_retry_delay(remaining: Duration) -> Duration {
    Duration::from_millis(100).min(remaining)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn allocations_are_unique_within_the_process() {
        let mut seen = HashSet::new();
        for _ in 0..200 {
            let (ip, port) = get_random_addr();
            assert!(
                seen.insert((ip.clone(), port)),
                "the allocator handed out {ip}:{port} twice"
            );
        }
    }

    #[test]
    fn a_handed_out_address_is_never_handed_out_again() {
        let (first_ip, first_port) = get_random_addr();
        for _ in 0..300 {
            let (ip, port) = get_random_addr();
            assert!(
                !(ip == first_ip && port == first_port),
                "the allocator repeated {first_ip}:{first_port}"
            );
        }
    }

    #[test]
    fn a_bound_port_is_never_handed_out() {
        // Hold one pair the way a fixture does, then allocate many times.
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let held = listener.local_addr().unwrap();
        for _ in 0..300 {
            let (ip, port) = get_random_addr();
            assert!(
                !(ip == "127.0.0.1" && port == held.port()),
                "the allocator handed out the bound port {held}"
            );
        }
    }

    #[test]
    fn the_dual_stack_helper_reserves_both_families() {
        let port = get_dual_stack_listener_port();
        assert!(!port_is_available_to_this_process(
            IpAddr::V4(Ipv4Addr::LOCALHOST),
            port
        ));
        assert!(!port_is_available_to_this_process(
            IpAddr::V6(Ipv6Addr::LOCALHOST),
            port
        ));
        for _ in 0..200 {
            let (ip, other) = get_random_addr();
            assert!(!(ip == "127.0.0.1" && other == port));
        }
    }

    #[tokio::test]
    async fn readiness_only_waits_for_pending_listeners() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        let mut ports = PortHelper::new();
        ports.listener_addrs.push(addr);

        ports.wait_for_all_ports().await.unwrap();
        drop(listener);

        ports.wait_for_all_ports().await.unwrap();
    }

    #[tokio::test]
    async fn readiness_uses_the_shared_deadline() {
        // Bind without listening: connects are refused while the kernel keeps
        // other tests and processes from taking this address until we are done.
        let socket = tokio::net::TcpSocket::new_v4().unwrap();
        socket.set_reuseaddr(false).unwrap();
        socket.bind("127.0.0.1:0".parse().unwrap()).unwrap();
        let addr = socket.local_addr().unwrap();
        assert_eq!(
            TcpListener::bind(addr).unwrap_err().kind(),
            std::io::ErrorKind::AddrInUse
        );
        assert_eq!(
            TcpStream::connect(addr).await.unwrap_err().kind(),
            std::io::ErrorKind::ConnectionRefused
        );

        let wait_timeout = Duration::from_millis(25);
        let started = Instant::now();
        let error = wait_for_addr(addr, wait_timeout).await.unwrap_err();

        assert_eq!(error.kind(), std::io::ErrorKind::TimedOut);
        let elapsed = started.elapsed();
        assert!(elapsed >= wait_timeout);
        assert!(elapsed < Duration::from_secs(1));
        drop(socket);
    }

    #[test]
    fn readiness_retry_delay_is_capped_by_remaining_budget() {
        // Check the exact budget independently of wall-clock scheduling jitter.
        for millis in [0, 25, 100, 250] {
            assert_eq!(
                readiness_retry_delay(Duration::from_millis(millis)),
                Duration::from_millis(millis.min(100))
            );
        }
    }
}
