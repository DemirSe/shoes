//! Packet-address stream implementation for h2mux `packet_addr` mode.
//!
//! Frame format:
//! ```text
//! | ATYP | address  | port  | length | data     |
//! | u8   | variable | u16be | u16be  | variable |
//! ```
//!
//! Uses the SOCKS-style `SocksaddrSerializer` address encoding.

use std::net::SocketAddr;
use std::pin::Pin;
use std::task::{Context, Poll};

use futures::ready;
use tokio::io::ReadBuf;

use super::socks_addr::{parse_socks_packet_address, write_socks_packet_address};
use crate::address::NetLocation;
use crate::async_stream::{
    AsyncFlushMessage, AsyncPing, AsyncReadTargetedMessage, AsyncShutdownMessage, AsyncStream,
    AsyncTargetedMessageStream, AsyncWriteSourcedMessage,
};
use crate::slide_buffer::SlideBuffer;
use crate::util::allocate_vec;

/// Buffer size for reading and writing packet-address frames.
const BUFFER_SIZE: usize = 65535;

type ParseFn = fn(&[u8]) -> std::io::Result<Option<(NetLocation, usize)>>;

struct AddressCodec {
    parse: ParseFn,
    write: fn(&mut [u8], &SocketAddr) -> usize,
}

const SOCKS_ADDR_CODEC: AddressCodec = AddressCodec {
    parse: parse_socks_packet_address,
    write: write_socks_packet_address,
};

/// The end of a packet-address stream arrives as an error, because a
/// zero-length frame is an empty datagram rather than end-of-stream.
fn packet_addr_stream_closed() -> std::io::Error {
    std::io::Error::new(
        std::io::ErrorKind::UnexpectedEof,
        "packet-address stream closed",
    )
}

/// Packet-address stream for multi-destination UDP transports.
pub struct PacketAddrStream<S> {
    stream: S,
    codec: &'static AddressCodec,

    /// Buffer for reading - accumulates bytes until we have a complete packet
    read_buf: SlideBuffer,

    /// Buffer for writing - holds a complete packet before sending (boxed to avoid stack overflow)
    write_buf: Box<[u8]>,
    write_buf_len: usize,
    write_buf_sent: usize,

    is_eof: bool,

    /// True when the stream ended between frames; false when it ended
    /// mid-frame.
    ended_at_boundary: bool,
}

impl<S: AsyncStream> PacketAddrStream<S> {
    fn new_with_codec(stream: S, codec: &'static AddressCodec) -> Self {
        Self {
            stream,
            codec,
            read_buf: SlideBuffer::new(BUFFER_SIZE),
            write_buf: allocate_vec(BUFFER_SIZE).into_boxed_slice(),
            write_buf_len: 0,
            write_buf_sent: 0,
            is_eof: false,
            ended_at_boundary: false,
        }
    }

    /// Create a stream that uses sing-mux SOCKS packet addresses.
    pub fn new_socks(stream: S) -> Self {
        Self::new_with_codec(stream, &SOCKS_ADDR_CODEC)
    }

    /// Try to parse a complete packet-address frame from the read buffer.
    /// Returns Ok(Some((location, payload_start, payload_len))) if a complete packet is available.
    /// Returns Ok(None) if more data is needed.
    /// Returns Err if the data is malformed (unknown ATYP, invalid UTF-8, etc).
    #[inline]
    fn try_parse_packet(&self) -> std::io::Result<Option<(NetLocation, usize, usize)>> {
        let data = self.read_buf.as_slice();

        // Try to parse the address
        let (location, addr_len) = match (self.codec.parse)(data)? {
            Some(result) => result,
            None => return Ok(None),
        };

        // Need at least address + 2 bytes for length prefix
        if data.len() < addr_len + 2 {
            return Ok(None);
        }

        // Read length prefix
        let payload_len = u16::from_be_bytes([data[addr_len], data[addr_len + 1]]) as usize;
        let payload_start = addr_len + 2;
        let total_len = payload_start + payload_len;

        // Check if we have the complete packet
        if data.len() < total_len {
            return Ok(None);
        }

        Ok(Some((location, payload_start, payload_len)))
    }
}

impl<S: AsyncStream> AsyncReadTargetedMessage for PacketAddrStream<S> {
    /// A zero-length payload frame is an empty datagram, so this stream reports
    /// its end as an error instead of an empty read.
    fn targeted_eof_on_empty(&self) -> bool {
        false
    }

    /// A clean end is reported as an error only so it cannot be mistaken for
    /// an empty datagram; the field still tells the two cases apart.
    fn read_error_is_end_of_stream(&self) -> bool {
        self.ended_at_boundary
    }

    fn poll_read_targeted_message(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut ReadBuf<'_>,
    ) -> Poll<std::io::Result<NetLocation>> {
        let this = self.get_mut();

        log::trace!(
            "PacketAddrStream::poll_read_targeted_message: is_eof={}, buf_len={}",
            this.is_eof,
            this.read_buf.len()
        );

        if this.is_eof {
            return Poll::Ready(Err(packet_addr_stream_closed()));
        }

        loop {
            // Try to parse a complete packet from buffered data
            match this.try_parse_packet()? {
                Some((location, payload_start, payload_len)) => {
                    let data = this.read_buf.as_slice();
                    buf.put_slice(&data[payload_start..payload_start + payload_len]);

                    // Consume the entire packet from the buffer
                    let total_consumed = payload_start + payload_len;
                    this.read_buf.consume(total_consumed);

                    log::trace!(
                        "PacketAddrStream: parsed packet to {}, payload_len={}",
                        location,
                        payload_len
                    );
                    return Poll::Ready(Ok(location));
                }
                None => {
                    if log::log_enabled!(log::Level::Trace) {
                        let data = this.read_buf.as_slice();
                        let preview: Vec<u8> = data.iter().take(20).copied().collect();
                        log::trace!(
                            "PacketAddrStream: incomplete packet, buf_len={}, first_bytes={:02x?}",
                            this.read_buf.len(),
                            preview
                        );
                    }
                    // Need more data - continue below
                }
            }

            // Need more data - compact buffer if needed
            this.read_buf.maybe_compact(4096);

            if this.read_buf.remaining_capacity() == 0 {
                return Poll::Ready(Err(std::io::Error::other(
                    "packet-address read buffer full but no complete packet",
                )));
            }

            // Read more data from the underlying stream
            let write_slice = this.read_buf.write_slice();
            let mut read_buf = ReadBuf::new(write_slice);

            match Pin::new(&mut this.stream).poll_read(cx, &mut read_buf) {
                Poll::Ready(Ok(())) => {
                    let bytes_read = read_buf.filled().len();
                    log::trace!("PacketAddrStream: read {} bytes from stream", bytes_read);
                    if bytes_read == 0 {
                        this.is_eof = true;
                        // A leftover partial frame means the peer stopped mid-frame.
                        this.ended_at_boundary = this.read_buf.is_empty();
                        return Poll::Ready(Err(packet_addr_stream_closed()));
                    }
                    this.read_buf.advance_write(bytes_read);
                    // Loop to try parsing again
                }
                Poll::Ready(Err(e)) => {
                    log::trace!("PacketAddrStream: read error: {}", e);
                    return Poll::Ready(Err(e));
                }
                Poll::Pending => {
                    log::trace!("PacketAddrStream: poll_read pending");
                    return Poll::Pending;
                }
            }
        }
    }
}

impl<S: AsyncStream> AsyncWriteSourcedMessage for PacketAddrStream<S> {
    fn poll_write_sourced_message(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &[u8],
        source: &SocketAddr,
    ) -> Poll<std::io::Result<()>> {
        let this = self.get_mut();

        // If there's pending data to send, flush it first
        while this.write_buf_sent < this.write_buf_len {
            let remaining = &this.write_buf[this.write_buf_sent..this.write_buf_len];
            match Pin::new(&mut this.stream).poll_write(cx, remaining) {
                Poll::Ready(Ok(n)) => {
                    this.write_buf_sent += n;
                }
                Poll::Ready(Err(e)) => return Poll::Ready(Err(e)),
                Poll::Pending => return Poll::Pending,
            }
        }

        // Reset write buffer
        this.write_buf_len = 0;
        this.write_buf_sent = 0;

        // Calculate required space: address + length(2) + payload
        let addr_len = match source {
            SocketAddr::V4(_) => 7,  // ATYP + IPv4(4) + Port(2)
            SocketAddr::V6(_) => 19, // ATYP + IPv6(16) + Port(2)
        };
        let total_len = addr_len + 2 + buf.len();

        if total_len > this.write_buf.len() {
            return Poll::Ready(Err(std::io::Error::other(format!(
                "packet-address frame too large: {total_len} > {}",
                this.write_buf.len()
            ))));
        }

        let offset = (this.codec.write)(&mut this.write_buf, source);

        // Write length prefix (u16be)
        let len_bytes = (buf.len() as u16).to_be_bytes();
        this.write_buf[offset..offset + 2].copy_from_slice(&len_bytes);
        let data_start = offset + 2;

        // Write payload
        this.write_buf[data_start..data_start + buf.len()].copy_from_slice(buf);
        this.write_buf_len = data_start + buf.len();

        Poll::Ready(Ok(()))
    }
}

impl<S: AsyncStream> AsyncFlushMessage for PacketAddrStream<S> {
    fn poll_flush_message(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<std::io::Result<()>> {
        let this = self.get_mut();

        // Flush any pending write data
        while this.write_buf_sent < this.write_buf_len {
            let remaining = &this.write_buf[this.write_buf_sent..this.write_buf_len];
            match Pin::new(&mut this.stream).poll_write(cx, remaining) {
                Poll::Ready(Ok(n)) => {
                    this.write_buf_sent += n;
                }
                Poll::Ready(Err(e)) => return Poll::Ready(Err(e)),
                Poll::Pending => return Poll::Pending,
            }
        }

        this.write_buf_len = 0;
        this.write_buf_sent = 0;

        // Flush the underlying stream
        Pin::new(&mut this.stream).poll_flush(cx)
    }
}

impl<S: AsyncStream> AsyncShutdownMessage for PacketAddrStream<S> {
    fn poll_shutdown_message(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
    ) -> Poll<std::io::Result<()>> {
        let mut this = self.get_mut();
        ready!(Pin::new(&mut this).poll_flush_message(cx))?;
        Pin::new(&mut this.stream).poll_shutdown(cx)
    }
}

impl<S: AsyncStream> AsyncPing for PacketAddrStream<S> {
    fn supports_ping(&self) -> bool {
        false
    }

    fn poll_write_ping(self: Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<std::io::Result<bool>> {
        Poll::Ready(Ok(false))
    }
}

impl<S: AsyncStream> AsyncTargetedMessageStream for PacketAddrStream<S> {}

pub type SocksPacketAddrStream<S> = PacketAddrStream<S>;

#[cfg(test)]
mod tests {
    use std::future::poll_fn;
    use std::net::{Ipv4Addr, SocketAddr};
    use std::pin::Pin;

    use tokio::io::{AsyncReadExt, AsyncWriteExt, ReadBuf};
    use tokio::net::{TcpListener, TcpStream};

    use super::*;

    async fn tcp_pair() -> std::io::Result<(TcpStream, TcpStream)> {
        let listener = TcpListener::bind("127.0.0.1:0").await?;
        let addr = listener.local_addr()?;
        let accept = listener.accept();
        let connect = TcpStream::connect(addr);
        let ((server, _), client) = tokio::try_join!(accept, connect)?;
        Ok((client, server))
    }

    async fn read_packet<S: AsyncStream>(
        stream: &mut PacketAddrStream<S>,
    ) -> std::io::Result<(NetLocation, Vec<u8>)> {
        let mut payload = vec![0u8; 1024];
        let mut read_buf = ReadBuf::new(&mut payload);
        let location =
            poll_fn(|cx| Pin::new(&mut *stream).poll_read_targeted_message(cx, &mut read_buf))
                .await?;
        Ok((location, read_buf.filled().to_vec()))
    }

    async fn write_packet<S: AsyncStream>(
        stream: &mut PacketAddrStream<S>,
        payload: &[u8],
        source: &SocketAddr,
    ) -> std::io::Result<()> {
        poll_fn(|cx| Pin::new(&mut *stream).poll_write_sourced_message(cx, payload, source))
            .await?;
        poll_fn(|cx| Pin::new(&mut *stream).poll_flush_message(cx)).await
    }

    fn encode_packet(
        write_addr: fn(&mut [u8], &SocketAddr) -> usize,
        addr: &SocketAddr,
        payload: &[u8],
    ) -> Vec<u8> {
        let mut frame = vec![0u8; 64 + payload.len()];
        let addr_len = write_addr(&mut frame, addr);
        frame[addr_len..addr_len + 2].copy_from_slice(&(payload.len() as u16).to_be_bytes());
        frame[addr_len + 2..addr_len + 2 + payload.len()].copy_from_slice(payload);
        frame.truncate(addr_len + 2 + payload.len());
        frame
    }

    #[tokio::test]
    async fn test_new_socks_reads_socks_packets() {
        let source = SocketAddr::from((Ipv4Addr::new(203, 0, 113, 20), 5353));
        let payload = b"socks payload";
        let frame = encode_packet(write_socks_packet_address, &source, payload);

        let (client, mut server) = tcp_pair().await.unwrap();
        let mut stream = PacketAddrStream::new_socks(client);

        let writer = tokio::spawn(async move {
            server.write_all(&frame).await.unwrap();
        });

        let (location, read_payload) = read_packet(&mut stream).await.unwrap();
        writer.await.unwrap();

        assert_eq!(location.to_socket_addr_nonblocking(), Some(source));
        assert_eq!(read_payload, payload);
    }

    #[tokio::test]
    async fn test_new_socks_rejects_unknown_address_type() {
        let mut frame = vec![0x00];
        frame.extend_from_slice(&[0, 0, 0, 0]);
        frame.extend_from_slice(&7001u16.to_be_bytes());
        frame.extend_from_slice(&11u16.to_be_bytes());
        frame.extend_from_slice(b"wrong codec");

        let (client, mut server) = tcp_pair().await.unwrap();
        let mut stream = PacketAddrStream::new_socks(client);

        let writer = tokio::spawn(async move {
            server.write_all(&frame).await.unwrap();
        });

        let err = read_packet(&mut stream).await.unwrap_err();
        writer.await.unwrap();

        assert!(err.to_string().contains("unknown SOCKS packet ATYP: 0"));
    }

    #[tokio::test]
    async fn test_new_socks_writes_socks_packets() {
        let source = SocketAddr::from((Ipv4Addr::new(203, 0, 113, 22), 9000));
        let payload = b"write socks";
        let expected = encode_packet(write_socks_packet_address, &source, payload);

        let (client, mut server) = tcp_pair().await.unwrap();
        let mut stream = PacketAddrStream::new_socks(client);

        write_packet(&mut stream, payload, &source).await.unwrap();

        let mut actual = vec![0u8; expected.len()];
        server.read_exact(&mut actual).await.unwrap();
        assert_eq!(actual, expected);
    }

    #[tokio::test]
    async fn test_zero_length_payload_frame_is_an_empty_datagram() {
        let source = SocketAddr::from((Ipv4Addr::new(203, 0, 113, 21), 5353));
        let empty = encode_packet(write_socks_packet_address, &source, b"");
        let frame = encode_packet(write_socks_packet_address, &source, b"after empty");

        let (client, mut server) = tcp_pair().await.unwrap();
        let mut stream = PacketAddrStream::new_socks(client);
        assert!(
            !stream.targeted_eof_on_empty(),
            "a zero-length frame cannot mean end-of-stream"
        );

        let writer = tokio::spawn(async move {
            server.write_all(&empty).await.unwrap();
            server.write_all(&frame).await.unwrap();
        });

        let (location, read_payload) = read_packet(&mut stream).await.unwrap();
        assert_eq!(location.to_socket_addr_nonblocking(), Some(source));
        assert!(
            read_payload.is_empty(),
            "a zero-length payload frame is an empty datagram"
        );

        let (location, read_payload) = read_packet(&mut stream).await.unwrap();
        assert_eq!(location.to_socket_addr_nonblocking(), Some(source));
        assert_eq!(read_payload.as_slice(), b"after empty");
        writer.await.unwrap();
    }

    #[tokio::test]
    async fn test_stream_end_is_reported_as_an_error_not_as_an_empty_frame() {
        let source = SocketAddr::from((Ipv4Addr::new(203, 0, 113, 23), 5354));
        let frame = encode_packet(write_socks_packet_address, &source, b"payload");

        let (client, mut server) = tcp_pair().await.unwrap();
        let mut stream = PacketAddrStream::new_socks(client);

        let writer = tokio::spawn(async move {
            server.write_all(&frame).await.unwrap();
            server.shutdown().await.unwrap();
        });

        let (location, read_payload) = read_packet(&mut stream).await.unwrap();
        assert_eq!(location.to_socket_addr_nonblocking(), Some(source));
        assert_eq!(read_payload.as_slice(), b"payload");

        let error = read_packet(&mut stream).await.unwrap_err();
        writer.await.unwrap();
        assert_eq!(error.kind(), std::io::ErrorKind::UnexpectedEof);
        assert_eq!(error.to_string(), "packet-address stream closed");
        assert!(
            stream.read_error_is_end_of_stream(),
            "a stream end between frames is a clean end"
        );
    }

    #[tokio::test]
    async fn test_truncated_frame_is_not_a_clean_stream_end() {
        let (client, mut server) = tcp_pair().await.unwrap();
        let mut stream = PacketAddrStream::new_socks(client);

        // A partial SOCKS address: the peer stops in the middle of a frame.
        let writer = tokio::spawn(async move {
            server.write_all(&[0x01, 127, 0]).await.unwrap();
            server.shutdown().await.unwrap();
        });

        let error = read_packet(&mut stream).await.unwrap_err();
        writer.await.unwrap();
        assert_eq!(error.kind(), std::io::ErrorKind::UnexpectedEof);
        assert!(
            !stream.read_error_is_end_of_stream(),
            "a mid-frame end must stay a read error"
        );
    }
}
