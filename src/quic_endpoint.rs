use std::cell::RefCell;
use std::io::{self, IoSliceMut};
use std::net::SocketAddr;
use std::pin::Pin;
use std::sync::Arc;
use std::task::{Context, Poll};

use crate::resources::BudgetPermit;
use quinn::{AsyncUdpSocket, Runtime, UdpPoller};

tokio::task_local! {
    static CONNECTION_MEMORY: RefCell<Option<BudgetPermit>>;
}

/// For a dedicated single-connection endpoint, such as Hickory's H3 client.
/// The socket holds its memory reservation until the last clone is dropped.
pub(crate) fn socket_with_memory(
    socket: std::net::UdpSocket,
    memory: BudgetPermit,
) -> io::Result<Arc<dyn AsyncUdpSocket>> {
    Ok(Arc::new(MemorySocket {
        inner: quinn::TokioRuntime.wrap_udp_socket(socket)?,
        endpoint_memory: Some(memory),
    }))
}

#[derive(Debug)]
struct MemorySocket {
    inner: Arc<dyn AsyncUdpSocket>,
    endpoint_memory: Option<BudgetPermit>,
}

impl AsyncUdpSocket for MemorySocket {
    fn create_io_poller(self: Arc<Self>) -> Pin<Box<dyn UdpPoller>> {
        if self.endpoint_memory.is_some() {
            return self.inner.clone().create_io_poller();
        }
        // Quinn creates one poller synchronously inside connect/accept. Its State
        // drops protocol stream/datagram buffers before dropping this poller,
        // including when application handles survive driver completion.
        let memory = CONNECTION_MEMORY.with(|memory| {
            memory
                .borrow_mut()
                .take()
                .expect("QUIC memory reserved once per connection")
        });
        Box::pin(MemoryPoller {
            inner: self.inner.clone().create_io_poller(),
            _memory: memory,
        })
    }

    fn try_send(&self, transmit: &quinn::udp::Transmit<'_>) -> io::Result<()> {
        self.inner.try_send(transmit)
    }

    fn poll_recv(
        &self,
        cx: &mut Context<'_>,
        bufs: &mut [IoSliceMut<'_>],
        meta: &mut [quinn::udp::RecvMeta],
    ) -> Poll<io::Result<usize>> {
        self.inner.poll_recv(cx, bufs, meta)
    }

    fn local_addr(&self) -> io::Result<SocketAddr> {
        self.inner.local_addr()
    }

    fn max_transmit_segments(&self) -> usize {
        self.inner.max_transmit_segments()
    }

    fn max_receive_segments(&self) -> usize {
        self.inner.max_receive_segments()
    }

    fn may_fragment(&self) -> bool {
        self.inner.may_fragment()
    }
}

#[derive(Debug)]
struct MemoryPoller {
    inner: Pin<Box<dyn UdpPoller>>,
    _memory: BudgetPermit,
}

impl UdpPoller for MemoryPoller {
    fn poll_writable(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        self.inner.as_mut().poll_writable(cx)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::time::Duration;

    use crate::resources::Budget;
    use bytes::Bytes;
    use tokio::time::timeout;

    async fn wait_for_slot(budget: &Arc<Budget>) -> BudgetPermit {
        loop {
            if let Some(permit) = budget.acquire(1) {
                return permit;
            }
            tokio::time::sleep(Duration::from_millis(1)).await;
        }
    }

    fn configs() -> (quinn::ClientConfig, quinn::ServerConfig) {
        let cert = rcgen::generate_simple_self_signed(vec!["localhost".into()]).unwrap();
        let mut roots = rustls::RootCertStore::empty();
        roots.add(cert.cert.der().clone()).unwrap();
        let server_config = quinn::ServerConfig::with_single_cert(
            vec![cert.cert.der().clone()],
            rustls::pki_types::PrivatePkcs8KeyDer::from(cert.signing_key.serialize_der()).into(),
        )
        .unwrap();
        let client_config = quinn::ClientConfig::with_root_certificates(Arc::new(roots)).unwrap();
        (client_config, server_config)
    }

    struct Payload {
        bytes: Vec<u8>,
        live: Arc<AtomicUsize>,
        budget: Arc<Budget>,
    }

    impl AsRef<[u8]> for Payload {
        fn as_ref(&self) -> &[u8] {
            &self.bytes
        }
    }

    impl Drop for Payload {
        fn drop(&mut self) {
            assert_eq!(
                self.budget.available_permits(),
                0,
                "payload outlived reservation"
            );
            self.live.fetch_sub(1, Ordering::SeqCst);
        }
    }

    fn payload(live: &Arc<AtomicUsize>, budget: &Arc<Budget>) -> Bytes {
        live.fetch_add(1, Ordering::SeqCst);
        Bytes::from_owner(Payload {
            bytes: vec![0; 1 << 20],
            live: live.clone(),
            budget: budget.clone(),
        })
    }

    /// The retained DNS QUIC socket (`socket_with_memory`) must keep its budget
    /// reservation while a live payload and a receive handle outlive the client
    /// endpoint, and must release it only after the last stream handle drops.
    #[tokio::test]
    async fn socket_reservation_survives_draining_and_retained_streams() {
        let (client_config, server_config) = configs();
        let budget = Arc::new(Budget::new(Some(1)));
        let socket = socket_with_memory(
            std::net::UdpSocket::bind("0.0.0.0:0").unwrap(),
            budget.acquire(1).unwrap(),
        )
        .unwrap();
        let mut client = quinn::Endpoint::new_with_abstract_socket(
            quinn::EndpointConfig::default(),
            None,
            socket,
            Arc::new(quinn::TokioRuntime),
        )
        .unwrap();
        client.set_default_client_config(client_config);
        // The removed pooled-endpoint fixture is replaced by an ordinary server endpoint.
        let server =
            quinn::Endpoint::server(server_config, SocketAddr::from(([0, 0, 0, 0], 0))).unwrap();
        let address = SocketAddr::from(([127, 0, 0, 1], server.local_addr().unwrap().port()));
        let (connection, peer) = timeout(Duration::from_secs(5), async {
            tokio::join!(
                async { client.connect(address, "localhost").unwrap().await.unwrap() },
                async { server.accept().await.unwrap().await.unwrap() },
            )
        })
        .await
        .unwrap();
        let live = Arc::new(AtomicUsize::new(0));
        let (mut send, recv) = connection.open_bi().await.unwrap();
        send.write_chunk(payload(&live, &budget)).await.unwrap();
        connection.close(0u32.into(), b"closed with handles alive");
        assert_eq!(budget.available_permits(), 0);
        assert_eq!(live.load(Ordering::SeqCst), 1);
        timeout(Duration::from_secs(5), client.wait_idle())
            .await
            .unwrap();
        drop((client, connection, send));
        assert_eq!(budget.available_permits(), 0);
        assert_eq!(live.load(Ordering::SeqCst), 1);
        drop(recv);
        let _returned = timeout(Duration::from_secs(5), wait_for_slot(&budget))
            .await
            .unwrap();
        assert_eq!(live.load(Ordering::SeqCst), 0);
        drop(peer);
        timeout(Duration::from_secs(5), server.wait_idle())
            .await
            .unwrap();
    }
}
