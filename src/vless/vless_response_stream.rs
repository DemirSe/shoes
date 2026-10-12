use std::pin::Pin;
use std::task::{Context, Poll};

use futures::ready;
use log::debug;
use tokio::io::{AsyncRead, AsyncWrite, ReadBuf};
use tokio::net::TcpStream;

use crate::async_stream::{AsyncPing, AsyncStream};

/// A wrapper stream that reads VLESS response header on first read, similar
/// to VLESS clients.
pub struct VlessResponseStream<IO> {
    inner: IO,
    response_pending: bool,
    response_buffer: Vec<u8>,
    pending_read: Vec<u8>,
}

impl<IO> VlessResponseStream<IO>
where
    IO: AsyncStream,
{
    pub fn new(inner: IO) -> Self {
        Self {
            inner,
            response_pending: true,
            response_buffer: Vec::new(),
            pending_read: Vec::new(),
        }
    }

    fn poll_read_response(&mut self, cx: &mut Context<'_>) -> Poll<std::io::Result<()>> {
        // Need to read at least 2 bytes for the response header
        while self.response_buffer.len() < 2 {
            let remaining = 2 - self.response_buffer.len();
            let mut buf = [0u8; 2];
            let mut read_buf = ReadBuf::new(&mut buf[..remaining]);
            ready!(Pin::new(&mut self.inner).poll_read(cx, &mut read_buf))?;

            let filled = read_buf.filled();
            if filled.is_empty() {
                return Poll::Ready(Err(std::io::Error::new(
                    std::io::ErrorKind::UnexpectedEof,
                    "Connection closed while reading VLESS response",
                )));
            }
            self.response_buffer.extend_from_slice(filled);
        }

        // Validate version
        let version = self.response_buffer[0];
        if version != 0 {
            return Poll::Ready(Err(std::io::Error::other(format!(
                "invalid server protocol version, expected 0, got {version}"
            ))));
        }

        // Check if we need to read addons
        let addon_length = self.response_buffer[1] as usize;
        let total_response_len = 2 + addon_length;

        // Read addon data if needed
        while self.response_buffer.len() < total_response_len {
            let remaining = total_response_len - self.response_buffer.len();
            let mut buf = vec![0u8; remaining];
            let mut read_buf = ReadBuf::new(&mut buf);
            ready!(Pin::new(&mut self.inner).poll_read(cx, &mut read_buf))?;

            let filled = read_buf.filled();
            if filled.is_empty() {
                return Poll::Ready(Err(std::io::Error::new(
                    std::io::ErrorKind::UnexpectedEof,
                    "Connection closed while reading VLESS response addons",
                )));
            }
            self.response_buffer.extend_from_slice(filled);
        }

        debug!(
            "VLESS: Successfully read and consumed {} byte response header (version={}, addon_length={})",
            total_response_len, version, addon_length
        );

        // Keep any bytes beyond the header so a payload byte coalesced with
        // the header is not lost. The capped reads above make this empty in
        // practice; it mirrors VisionStream's remainder handling.
        if self.response_buffer.len() > total_response_len {
            self.pending_read
                .extend_from_slice(&self.response_buffer[total_response_len..]);
            self.response_buffer.truncate(total_response_len);
        }

        Poll::Ready(Ok(()))
    }
}

impl<IO> AsyncRead for VlessResponseStream<IO>
where
    IO: AsyncStream,
{
    fn poll_read(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut ReadBuf<'_>,
    ) -> Poll<std::io::Result<()>> {
        // A read with no capacity is a no-op, like the other streams.
        if buf.remaining() == 0 {
            return Poll::Ready(Ok(()));
        }

        // Read VLESS response header on first read
        if self.response_pending {
            match self.poll_read_response(cx) {
                Poll::Ready(Ok(())) => {
                    self.response_pending = false;
                    // Response is consumed, now read actual data
                }
                Poll::Ready(Err(e)) => return Poll::Ready(Err(e)),
                Poll::Pending => return Poll::Pending,
            }
        }

        // Serve a payload byte coalesced with the header before touching the
        // inner stream, so the byte order is preserved.
        if !self.pending_read.is_empty() {
            let len = buf.remaining().min(self.pending_read.len());
            buf.put_slice(&self.pending_read[..len]);
            self.pending_read.drain(..len);
            return Poll::Ready(Ok(()));
        }

        // Pass through to inner stream
        Pin::new(&mut self.inner).poll_read(cx, buf)
    }
}

impl<IO> AsyncWrite for VlessResponseStream<IO>
where
    IO: AsyncStream,
{
    fn poll_write(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &[u8],
    ) -> Poll<std::io::Result<usize>> {
        Pin::new(&mut self.inner).poll_write(cx, buf)
    }

    fn poll_flush(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<std::io::Result<()>> {
        Pin::new(&mut self.inner).poll_flush(cx)
    }

    fn poll_shutdown(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<std::io::Result<()>> {
        Pin::new(&mut self.inner).poll_shutdown(cx)
    }
}

impl<IO> AsyncPing for VlessResponseStream<IO>
where
    IO: AsyncStream,
{
    fn supports_ping(&self) -> bool {
        self.inner.supports_ping()
    }

    fn poll_write_ping(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
    ) -> Poll<std::io::Result<bool>> {
        Pin::new(&mut self.inner).poll_write_ping(cx)
    }
}

impl<IO> AsyncStream for VlessResponseStream<IO>
where
    IO: AsyncStream,
{
    /// Reads reach the inner socket only after the response header is consumed.
    fn raw_read_socket(&self) -> Option<&TcpStream> {
        if self.response_pending || !self.pending_read.is_empty() {
            return None;
        }
        self.inner.raw_read_socket()
    }

    /// Writes always reach the inner stream, so the inner gate decides.
    fn raw_write_socket(&self) -> Option<&TcpStream> {
        self.inner.raw_write_socket()
    }
}

/// The response header gate, checked at the copy seam.
#[cfg(all(test, target_os = "linux"))]
mod tests {
    use super::*;
    use crate::linux_splice::test_hooks;
    use std::collections::VecDeque;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::net::{TcpListener, TcpStream};
    use tokio::time::{Duration, timeout};

    async fn tcp_pair() -> std::io::Result<(TcpStream, TcpStream)> {
        let listener = TcpListener::bind("127.0.0.1:0").await?;
        let address = listener.local_addr()?;
        let (connector, accepted) = tokio::join!(TcpStream::connect(address), listener.accept());
        let (accepted, _) = accepted?;
        Ok((connector?, accepted))
    }

    /// Scripted inner stream that releases one queued chunk per read, capped
    /// by the offered buffer like a socket. It reproduces a peer that splits
    /// the 2-byte response as 1 byte, then coalesces the second header byte
    /// with the first payload bytes.
    struct SplitReadIo {
        chunks: VecDeque<Vec<u8>>,
    }

    impl AsyncRead for SplitReadIo {
        fn poll_read(
            mut self: Pin<&mut Self>,
            _cx: &mut Context<'_>,
            buf: &mut ReadBuf<'_>,
        ) -> Poll<std::io::Result<()>> {
            let Some(front) = self.chunks.front_mut() else {
                return Poll::Ready(Ok(()));
            };
            if front.is_empty() {
                self.chunks.pop_front();
                return Poll::Ready(Ok(()));
            }
            let len = buf.remaining().min(front.len());
            if len == 0 {
                return Poll::Ready(Ok(()));
            }
            buf.put_slice(&front[..len]);
            front.drain(..len);
            if front.is_empty() {
                self.chunks.pop_front();
            }
            Poll::Ready(Ok(()))
        }
    }

    impl AsyncWrite for SplitReadIo {
        fn poll_write(
            self: Pin<&mut Self>,
            _cx: &mut Context<'_>,
            buf: &[u8],
        ) -> Poll<std::io::Result<usize>> {
            Poll::Ready(Ok(buf.len()))
        }

        fn poll_flush(self: Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<std::io::Result<()>> {
            Poll::Ready(Ok(()))
        }

        fn poll_shutdown(self: Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<std::io::Result<()>> {
            Poll::Ready(Ok(()))
        }
    }

    impl AsyncPing for SplitReadIo {
        fn supports_ping(&self) -> bool {
            false
        }

        fn poll_write_ping(
            self: Pin<&mut Self>,
            _cx: &mut Context<'_>,
        ) -> Poll<std::io::Result<bool>> {
            Poll::Ready(Ok(false))
        }
    }

    impl AsyncStream for SplitReadIo {}

    #[tokio::test]
    async fn split_header_read_preserves_coalesced_payload_byte() -> std::io::Result<()> {
        let payload = vec![0x11u8, 0x22, 0x33, 0x44];
        let mut second = vec![0u8];
        second.extend_from_slice(&payload);
        let inner = SplitReadIo {
            chunks: VecDeque::from([vec![0u8], second]),
        };
        let mut stream = VlessResponseStream::new(inner);
        let mut received = vec![0u8; payload.len()];
        timeout(Duration::from_secs(5), stream.read_exact(&mut received)).await??;
        assert_eq!(
            received, payload,
            "a payload byte coalesced with a split header must survive"
        );
        assert!(
            stream.pending_read.is_empty(),
            "no coalesced payload byte may be left behind"
        );
        Ok(())
    }

    #[ignore = "process-wide counters; runs in an isolated process"]
    #[tokio::test]
    async fn the_response_header_gates_the_fast_path_without_reaching_the_payload()
    -> std::io::Result<()> {
        test_hooks::reset();
        let (mut peer, reader) = tcp_pair().await?;
        let (relay_b, mut destination) = tcp_pair().await?;
        let mut reader = VlessResponseStream::new(reader);
        assert!(
            reader.raw_read_socket().is_none(),
            "the response header must be consumed before the socket is usable"
        );

        let mut relay_b = relay_b;
        let copy = tokio::spawn(async move {
            crate::copy_bidirectional::copy_bidirectional(&mut reader, &mut relay_b, false, false)
                .await
        });

        // The header is consumed silently; only the payload may reach the peer.
        peer.write_all(&[0, 0]).await?;
        let first = vec![0x11u8; 4096];
        peer.write_all(&first).await?;
        let mut received = vec![0; first.len()];
        timeout(
            Duration::from_secs(5),
            destination.read_exact(&mut received),
        )
        .await??;
        assert_eq!(
            received, first,
            "the response header must not reach the payload"
        );

        // Once the header is gone, later payload must take the fast path.
        let later = vec![0x22u8; 32 * 1024];
        peer.write_all(&later).await?;
        let mut received_later = vec![0; later.len()];
        timeout(
            Duration::from_secs(5),
            destination.read_exact(&mut received_later),
        )
        .await??;
        assert_eq!(received_later, later);
        assert!(
            test_hooks::splice_bytes() >= 2 * later.len() as u64,
            "a consumed response header must let the payload use splice: {} bytes",
            test_hooks::splice_bytes()
        );

        peer.shutdown().await?;
        timeout(Duration::from_secs(5), destination.shutdown()).await??;
        timeout(Duration::from_secs(10), copy)
            .await??
            .map_err(std::io::Error::other)?;
        Ok(())
    }
}
