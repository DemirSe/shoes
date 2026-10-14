//! Offline runtime matrix: real socket/protocol connectors, injected DNS only.

use super::*;
use crate::address::{Address, NetLocation};
use crate::config::{ClientChainHop, ClientConfig, ClientProxyConfig};
use crate::option_util::OneOrSome;
use crate::tcp::chain_builder::build_client_proxy_chain;
use std::future::{Future, poll_fn};
use std::io;
use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr};
use std::pin::Pin;
use std::time::Duration;
use tokio::io::{AsyncReadExt, AsyncWriteExt, ReadBuf};

pub(crate) const USER: &str = "550e8400-e29b-41d4-a716-446655440000";
pub(crate) const DEADLINE: Duration = Duration::from_secs(3);
pub(crate) const LOOPBACKS: [IpAddr; 2] = [
    IpAddr::V4(Ipv4Addr::LOCALHOST),
    IpAddr::V6(Ipv6Addr::LOCALHOST),
];

#[derive(Clone, Debug)]
pub(crate) enum Answer {
    Addresses(Vec<SocketAddr>),
    Fail,
    Pending,
}

#[derive(Debug)]
pub(crate) struct RecordingResolver {
    expected: NetLocation,
    calls: parking_lot::Mutex<Vec<NetLocation>>,
    answer: Answer,
}

impl RecordingResolver {
    pub(crate) fn new(expected: NetLocation, answer: Answer) -> Arc<Self> {
        Arc::new(Self {
            expected,
            calls: parking_lot::Mutex::new(Vec::new()),
            answer,
        })
    }

    pub(crate) fn assert_calls(&self, expected: &[NetLocation]) {
        assert_eq!(*self.calls.lock(), expected);
    }
}

impl Resolver for RecordingResolver {
    fn resolve_location(
        &self,
        location: &NetLocation,
    ) -> Pin<Box<dyn Future<Output = io::Result<Vec<SocketAddr>>> + Send>> {
        // Reject unexpected names rather than falling back to OS DNS.
        assert_eq!(location, &self.expected);
        self.calls.lock().push(location.clone());
        let answer = self.answer.clone();
        Box::pin(async move {
            match answer {
                Answer::Addresses(addresses) => {
                    tokio::task::yield_now().await;
                    Ok(addresses)
                }
                Answer::Fail => Err(io::Error::new(
                    io::ErrorKind::NotFound,
                    "injected resolver failure",
                )),
                Answer::Pending => std::future::pending().await,
            }
        })
    }
}

pub(crate) fn hostname(port: u16) -> NetLocation {
    NetLocation::from_str(&format!("whatsapp.net:{port}"), None).unwrap()
}

pub(crate) fn vless_config(address: NetLocation, udp_enabled: bool) -> ClientConfig {
    ClientConfig {
        address,
        protocol: ClientProxyConfig::Vless {
            user_id: USER.into(),
            udp_enabled,
            h2mux: None,
        },
        ..Default::default()
    }
}

pub(crate) fn chain(config: ClientConfig) -> ClientProxyChain {
    build_client_proxy_chain(OneOrSome::One(ClientChainHop::Single(
        crate::config::ConfigSelection::Config(config),
    )))
}

pub(crate) async fn bounded<T>(future: impl Future<Output = T>) -> T {
    tokio::time::timeout(DEADLINE, future)
        .await
        .expect("loopback fixture stalled")
}

pub(crate) async fn assert_vless_request(
    peer: &mut tokio::net::TcpStream,
    command: u8,
    target: &NetLocation,
) {
    let mut expected = vec![0];
    expected.extend(crate::uuid_util::parse_uuid(USER).unwrap());
    expected.extend([0, command]);
    expected.extend(target.port().to_be_bytes());
    match target.address() {
        Address::Hostname(name) => {
            expected.extend([2, name.len() as u8]);
            expected.extend(name.as_bytes());
        }
        Address::Ipv4(ip) => {
            expected.push(1);
            expected.extend(ip.octets());
        }
        Address::Ipv6(ip) => {
            expected.push(3);
            expected.extend(ip.octets());
        }
    }
    let mut received = vec![0; expected.len()];
    bounded(peer.read_exact(&mut received)).await.unwrap();
    assert_eq!(received, expected);
}

pub(crate) async fn assert_udp_roundtrip(
    stream: &mut Box<dyn AsyncMessageStream>,
    peer: &tokio::net::UdpSocket,
) {
    bounded(async {
        poll_fn(|cx| Pin::new(&mut **stream).poll_write_message(cx, b"fixture"))
            .await
            .unwrap();
        poll_fn(|cx| Pin::new(&mut **stream).poll_flush_message(cx))
            .await
            .unwrap();
        let mut bytes = [0; 32];
        let (length, sender) = peer.recv_from(&mut bytes).await.unwrap();
        assert_eq!(&bytes[..length], b"fixture");
        peer.send_to(b"reply", sender).await.unwrap();
        let mut read = ReadBuf::new(&mut bytes);
        poll_fn(|cx| Pin::new(&mut **stream).poll_read_message(cx, &mut read))
            .await
            .unwrap();
        assert_eq!(read.filled(), b"reply");
    })
    .await;
}

/// Inspect factory-created pools without exposing anything in production builds.
pub(crate) fn assert_hops(chain: &ClientProxyChain, expected: &[Vec<(Option<NetLocation>, bool)>]) {
    assert_eq!(chain.num_hops(), expected.len());
    assert_eq!(chain.initial_hop.len(), expected[0].len());
    for (entry, (location, udp)) in chain.initial_hop.iter().zip(&expected[0]) {
        match entry {
            InitialHopEntry::Direct(socket) => {
                assert!(location.is_none());
                assert!(socket.bind_interface().is_none());
            }
            InitialHopEntry::Proxy { proxy, .. } => {
                assert_eq!(Some(proxy.proxy_location()), location.as_ref())
            }
        }
        assert_eq!(entry.supports_udp(), *udp);
    }
    for (pool, expected_pool) in chain.subsequent_hops.iter().zip(&expected[1..]) {
        assert_eq!(pool.len(), expected_pool.len());
        for (proxy, (location, udp)) in pool.iter().zip(expected_pool) {
            assert_eq!(Some(proxy.proxy_location()), location.as_ref());
            assert_eq!(proxy.supports_udp_over_tcp(), *udp);
        }
    }
    assert_eq!(
        chain.supports_udp(),
        expected.last().unwrap().iter().any(|(_, udp)| *udp)
    );
    assert_eq!(
        chain.is_direct_only(),
        expected.len() == 1 && expected[0].iter().all(|(loc, _)| loc.is_none())
    );
}

pub(crate) fn assert_first_proxy_sni(chain: &ClientProxyChain) {
    let InitialHopEntry::Proxy { proxy, .. } = &chain.initial_hop[0] else {
        panic!("expected wrapped proxy");
    };
    assert!(format!("{proxy:?}").contains("server_name: DnsName(\"whatsapp.net\")"));
}

#[tokio::test]
async fn e02_direct_tcp_hostname_and_bypasses_ipv4_ipv6() {
    for ip in LOOPBACKS {
        let listener = tokio::net::TcpListener::bind(SocketAddr::new(ip, 0))
            .await
            .unwrap();
        let address = listener.local_addr().unwrap();
        let target = hostname(address.port());
        let recording = RecordingResolver::new(target.clone(), Answer::Addresses(vec![address]));
        let resolver: Arc<dyn Resolver> = recording.clone();
        let chain = chain(ClientConfig::default());
        recording.assert_calls(&[]);
        for location in [
            target.clone().into(),
            NetLocation::from_ip_addr(ip, address.port()).into(),
            ResolvedLocation::with_resolved(hostname(443), address),
        ] {
            let mut stream = bounded(chain.connect_tcp(location, &resolver))
                .await
                .unwrap()
                .client_stream;
            let (mut peer, _) = bounded(listener.accept()).await.unwrap();
            assert_eq!(peer.local_addr().unwrap(), address);
            bounded(stream.write_all(b"tcp")).await.unwrap();
            let mut bytes = [0; 3];
            bounded(peer.read_exact(&mut bytes)).await.unwrap();
            assert_eq!(&bytes, b"tcp");
            recording.assert_calls(std::slice::from_ref(&target));
        }
    }
}

#[tokio::test]
async fn e02_direct_udp_hostname_and_bypasses_ipv4_ipv6() {
    for ip in LOOPBACKS {
        let peer = tokio::net::UdpSocket::bind(SocketAddr::new(ip, 0))
            .await
            .unwrap();
        let address = peer.local_addr().unwrap();
        let target = hostname(address.port());
        let recording = RecordingResolver::new(target.clone(), Answer::Addresses(vec![address]));
        let resolver: Arc<dyn Resolver> = recording.clone();
        let chain = chain(ClientConfig::default());
        recording.assert_calls(&[]);
        for location in [
            target.clone().into(),
            NetLocation::from_ip_addr(ip, address.port()).into(),
            ResolvedLocation::with_resolved(hostname(443), address),
        ] {
            let mut stream = bounded(chain.connect_udp_bidirectional(&resolver, location))
                .await
                .unwrap();
            assert_udp_roundtrip(&mut stream, &peer).await;
            recording.assert_calls(std::slice::from_ref(&target));
        }
    }
}

#[tokio::test]
async fn e02_proxy_first_hop_tcp_udp_hostname_and_literal_ipv4_ipv6() {
    for ip in LOOPBACKS {
        let listener = tokio::net::TcpListener::bind(SocketAddr::new(ip, 0))
            .await
            .unwrap();
        let address = listener.local_addr().unwrap();
        let proxy_name = hostname(address.port());
        let final_target = hostname(443);
        for proxy_location in [
            proxy_name.clone(),
            NetLocation::from_ip_addr(ip, address.port()),
        ] {
            let recording =
                RecordingResolver::new(proxy_name.clone(), Answer::Addresses(vec![address]));
            let resolver: Arc<dyn Resolver> = recording.clone();
            let chain = chain(vless_config(proxy_location.clone(), true));
            recording.assert_calls(&[]);
            // Pre-resolution of the final target must not alter its wire hostname.
            let target = ResolvedLocation::with_resolved(final_target.clone(), address);
            let tcp = bounded(chain.connect_tcp(target.clone(), &resolver))
                .await
                .unwrap();
            let (mut peer, _) = bounded(listener.accept()).await.unwrap();
            assert_vless_request(&mut peer, 1, &final_target).await;
            drop((tcp, peer));
            let udp = bounded(chain.connect_udp_bidirectional(&resolver, target))
                .await
                .unwrap();
            let (mut peer, _) = bounded(listener.accept()).await.unwrap();
            assert_vless_request(&mut peer, 2, &final_target).await;
            drop((udp, peer));
            let expected = if proxy_location.address().hostname().is_some() {
                vec![proxy_name.clone(); 2]
            } else {
                vec![]
            };
            recording.assert_calls(&expected);
        }
    }
}

#[tokio::test]
async fn e02_resolution_errors_precede_connect_and_protocol_setup() {
    let target = hostname(443);
    for proxy in [false, true] {
        for answer in [Answer::Fail, Answer::Addresses(vec![])] {
            let recording = RecordingResolver::new(target.clone(), answer.clone());
            let resolver: Arc<dyn Resolver> = recording.clone();
            let chain = chain(if proxy {
                vless_config(target.clone(), true)
            } else {
                ClientConfig::default()
            });
            recording.assert_calls(&[]);
            let tcp = bounded(chain.connect_tcp(target.clone().into(), &resolver))
                .await
                .err()
                .unwrap();
            let udp = bounded(chain.connect_udp_bidirectional(&resolver, target.clone().into()))
                .await
                .err()
                .unwrap();
            for error in [tcp, udp] {
                match answer {
                    Answer::Fail => {
                        assert_eq!(error.kind(), io::ErrorKind::NotFound);
                        assert_eq!(error.to_string(), "injected resolver failure");
                    }
                    Answer::Addresses(_) => {
                        assert_eq!(error.kind(), io::ErrorKind::Other);
                        assert_eq!(
                            error.to_string(),
                            format!("could not resolve location: {target}")
                        );
                    }
                    Answer::Pending => unreachable!(),
                }
            }
            recording.assert_calls(&[target.clone(), target.clone()]);
        }
    }
}

#[tokio::test]
async fn e02_proxy_protocol_error_follows_first_hop_resolution_and_connect() {
    let listener = tokio::net::TcpListener::bind((Ipv4Addr::LOCALHOST, 0))
        .await
        .unwrap();
    let address = listener.local_addr().unwrap();
    let proxy_name = hostname(address.port());
    let recording = RecordingResolver::new(proxy_name.clone(), Answer::Addresses(vec![address]));
    let resolver: Arc<dyn Resolver> = recording.clone();
    let chain = chain(vless_config(proxy_name.clone(), true));
    let too_long = NetLocation::new(Address::Hostname("a".repeat(256)), 443);
    recording.assert_calls(&[]);
    let error = bounded(chain.connect_tcp(too_long.clone().into(), &resolver))
        .await
        .err()
        .unwrap();
    assert_eq!(error.kind(), io::ErrorKind::InvalidData);
    assert!(error.to_string().starts_with("Hostname is too long:"));
    let (_peer, _) = bounded(listener.accept()).await.unwrap();
    let error = bounded(chain.connect_udp_bidirectional(&resolver, too_long.into()))
        .await
        .err()
        .unwrap();
    assert_eq!(error.kind(), io::ErrorKind::InvalidData);
    assert!(error.to_string().starts_with("Hostname is too long:"));
    let (_peer, _) = bounded(listener.accept()).await.unwrap();
    recording.assert_calls(&[proxy_name.clone(), proxy_name]);
}

#[tokio::test]
async fn e02_connect_errors_follow_dns_without_releasing_reserved_port() {
    for ip in LOOPBACKS {
        // Reserve the port for the entire test, but never listen. Unlike binding
        // and dropping a listener, this cannot race another test's port reuse.
        let domain = if ip.is_ipv6() {
            socket2::Domain::IPV6
        } else {
            socket2::Domain::IPV4
        };
        let reservation =
            socket2::Socket::new(domain, socket2::Type::STREAM, Some(socket2::Protocol::TCP))
                .unwrap();
        reservation.bind(&SocketAddr::new(ip, 0).into()).unwrap();
        let address = reservation.local_addr().unwrap().as_socket().unwrap();
        let name = hostname(address.port());
        for proxy in [false, true] {
            let recording = RecordingResolver::new(name.clone(), Answer::Addresses(vec![address]));
            let resolver: Arc<dyn Resolver> = recording.clone();
            let chain = chain(if proxy {
                vless_config(name.clone(), true)
            } else {
                ClientConfig::default()
            });
            recording.assert_calls(&[]);
            let target = if proxy {
                // This would fail at protocol setup if TCP connected first.
                NetLocation::new(Address::Hostname("a".repeat(256)), 443)
            } else {
                name.clone()
            };
            let error = bounded(chain.connect_tcp(target.into(), &resolver))
                .await
                .err()
                .unwrap();
            assert_eq!(error.kind(), io::ErrorKind::ConnectionRefused);
            recording.assert_calls(std::slice::from_ref(&name));
            if proxy {
                let error =
                    bounded(chain.connect_udp_bidirectional(&resolver, hostname(443).into()))
                        .await
                        .err()
                        .unwrap();
                assert_eq!(error.kind(), io::ErrorKind::ConnectionRefused);
                recording.assert_calls(&[name.clone(), name.clone()]);
            }
        }
    }
}
