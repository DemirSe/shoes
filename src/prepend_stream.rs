use std::io;
use std::pin::Pin;
use std::task::{Context, Poll};

use tokio::io::{AsyncRead, AsyncWrite, ReadBuf};
use tokio::net::TcpStream;

use crate::async_stream::{AsyncPing, AsyncStream};

/// A stream wrapper that prepends initial data to reads.
///
/// Used when the protocol detection leaves some data buffered that needs
/// to be read first before continuing with the underlying stream.
pub struct PrependStream<S> {
    inner: S,
    initial_data: Option<Box<[u8]>>,
    offset: usize,
}

impl<S> PrependStream<S> {
    pub fn new(inner: S, initial_data: Option<Box<[u8]>>) -> Self {
        Self {
            inner,
            initial_data,
            offset: 0,
        }
    }

    /// True while prepended bytes still precede the inner stream's own bytes.
    fn pending_read_bytes(&self) -> bool {
        self.initial_data
            .as_ref()
            .is_some_and(|data| self.offset < data.len())
    }
}

impl<S: AsyncRead + Unpin> AsyncRead for PrependStream<S> {
    fn poll_read(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut ReadBuf<'_>,
    ) -> Poll<io::Result<()>> {
        let this = &mut *self;
        // First, drain any initial data
        if let Some(data) = this.initial_data.take() {
            let remaining = &data[this.offset..];
            if !remaining.is_empty() {
                let to_copy = std::cmp::min(remaining.len(), buf.remaining());
                buf.put_slice(&remaining[..to_copy]);
                this.offset += to_copy;
                // Put back if not fully consumed
                if this.offset < data.len() {
                    this.initial_data = Some(data);
                } else {
                    this.offset = 0;
                }
                return Poll::Ready(Ok(()));
            }
            this.offset = 0;
        }
        // Read from inner stream
        Pin::new(&mut this.inner).poll_read(cx, buf)
    }
}

impl<S: AsyncWrite + Unpin> AsyncWrite for PrependStream<S> {
    fn poll_write(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &[u8],
    ) -> Poll<io::Result<usize>> {
        Pin::new(&mut self.inner).poll_write(cx, buf)
    }

    fn poll_flush(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        Pin::new(&mut self.inner).poll_flush(cx)
    }

    fn poll_write_vectored(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        bufs: &[io::IoSlice<'_>],
    ) -> Poll<io::Result<usize>> {
        Pin::new(&mut self.inner).poll_write_vectored(cx, bufs)
    }

    fn is_write_vectored(&self) -> bool {
        self.inner.is_write_vectored()
    }

    fn poll_shutdown(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        Pin::new(&mut self.inner).poll_shutdown(cx)
    }
}

impl<S: AsyncPing + Unpin> AsyncPing for PrependStream<S> {
    fn supports_ping(&self) -> bool {
        self.inner.supports_ping()
    }

    fn poll_write_ping(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<bool>> {
        Pin::new(&mut self.get_mut().inner).poll_write_ping(cx)
    }
}

impl<S: AsyncStream> AsyncStream for PrependStream<S> {
    fn preferred_write_size(&self) -> Option<std::num::NonZeroUsize> {
        self.inner.preferred_write_size()
    }

    /// Reads reach the inner socket only after the prepended bytes are drained.
    fn raw_read_socket(&self) -> Option<&TcpStream> {
        if self.pending_read_bytes() {
            return None;
        }
        self.inner.raw_read_socket()
    }

    /// Writes always reach the inner stream, so the inner gate decides.
    fn raw_write_socket(&self) -> Option<&TcpStream> {
        self.inner.raw_write_socket()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::async_stream::AsyncStream;
    use tokio::io::AsyncReadExt;
    use tokio::net::{TcpListener, TcpStream};

    #[tokio::test]
    async fn prefix_bytes_gate_the_inner_read_socket() -> io::Result<()> {
        let listener = TcpListener::bind("127.0.0.1:0").await?;
        let address = listener.local_addr()?;
        let (connector, accepted) = tokio::join!(TcpStream::connect(address), listener.accept());
        let (accepted, _) = accepted?;
        let mut stream = PrependStream::new(accepted, Some(b"prefix".to_vec().into_boxed_slice()));
        assert!(
            stream.raw_read_socket().is_none(),
            "prepended bytes must keep the buffered path"
        );
        assert!(
            stream.raw_write_socket().is_some(),
            "writes always reach the inner stream"
        );

        let mut drained = [0; 6];
        stream.read_exact(&mut drained).await?;
        assert_eq!(&drained, b"prefix");
        assert!(
            stream.raw_read_socket().is_some(),
            "the read socket must open once the prefix drains"
        );
        drop(connector);
        Ok(())
    }
}
