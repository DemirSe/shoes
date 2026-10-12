use std::io::{Error, ErrorKind, Result};
use std::pin::Pin;
use std::task::{Context, Poll};
use tokio::io::ReadBuf;

use crate::async_stream::{
    AsyncFlushMessage, AsyncMessageStream, AsyncPing, AsyncReadMessage, AsyncShutdownMessage,
    AsyncStream, AsyncWriteMessage,
};
use crate::util::allocate_vec;

/// Framing buffer size: one length prefix plus the largest VLESS UDP payload.
const FRAMING_BUFFER_LEN: usize = u16::MAX as usize + 2;

pub struct VlessMessageStream<S> {
    stream: S,
    /// Framing buffer for incoming messages. It is allocated on the first read
    /// and given back while the stream waits with no partial message, so a relay
    /// that is idle between messages does not hold it.
    read_buf: Box<[u8]>,
    read_end_index: usize,
    pending_write: Vec<u8>,
    write_offset: usize,
    is_eof: bool,
    /// Set when the stream ended between messages, which is a clean end even
    /// though it is reported as an error.
    ended_between_messages: bool,
}

impl<S: AsyncStream> VlessMessageStream<S> {
    pub fn new(stream: S) -> Self {
        Self {
            stream,
            read_buf: Box::new([]),
            read_end_index: 0,
            pending_write: Vec::new(),
            write_offset: 0,
            is_eof: false,
            ended_between_messages: false,
        }
    }

    /// Allocate the framing buffer when a message is expected.
    fn reserve_read_buf(&mut self) {
        if self.read_buf.is_empty() {
            self.read_buf = allocate_vec(FRAMING_BUFFER_LEN).into_boxed_slice();
        }
    }

    /// Give the framing buffer back while no partial message is retained.
    fn release_read_buf(&mut self) {
        if self.read_end_index == 0 {
            self.read_buf = Box::new([]);
        }
    }

    pub fn feed_initial_read_data(&mut self, data: &[u8]) -> std::io::Result<()> {
        if data.len() > FRAMING_BUFFER_LEN {
            return Err(std::io::Error::other(
                "feed_initial_read_data called with too much data",
            ));
        }
        self.reserve_read_buf();
        self.read_buf[0..data.len()].copy_from_slice(data);
        self.read_end_index = data.len();
        Ok(())
    }
}

impl<S: AsyncStream> AsyncReadMessage for VlessMessageStream<S> {
    /// A length-prefixed frame with a zero length is an empty datagram, so this
    /// stream reports its end as an error instead of an empty read.
    fn read_message_eof_on_empty(&self) -> bool {
        false
    }

    fn read_error_is_end_of_stream(&self) -> bool {
        self.ended_between_messages
    }

    fn poll_read_message(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        out_buf: &mut ReadBuf<'_>,
    ) -> Poll<Result<()>> {
        let this = self.get_mut();

        if this.is_eof {
            this.release_read_buf();
            return Poll::Ready(Err(Error::new(
                ErrorKind::UnexpectedEof,
                "message stream closed",
            )));
        }

        loop {
            if this.read_end_index >= 2 {
                let payload_len = u16::from_be_bytes([this.read_buf[0], this.read_buf[1]]) as usize;
                let total_len = 2 + payload_len;
                if this.read_end_index >= total_len {
                    if out_buf.remaining() < payload_len {
                        return Poll::Ready(Err(Error::other(
                            "out_buf is too small to hold the message",
                        )));
                    }
                    out_buf.put_slice(&this.read_buf[2..total_len]);
                    if this.read_end_index > total_len {
                        this.read_buf.copy_within(total_len..this.read_end_index, 0);
                        this.read_end_index -= total_len;
                    } else {
                        // this.read_end_index == total_len
                        this.read_end_index = 0;
                        this.release_read_buf();
                    }
                    return Poll::Ready(Ok(()));
                }
            }

            this.reserve_read_buf();
            let read_buf_slice = &mut this.read_buf[this.read_end_index..];
            // this is impossible because our buffer size is u16::MAX + 2, so there should always
            // be space for a full message.
            assert!(!read_buf_slice.is_empty());
            let mut tmp = ReadBuf::new(read_buf_slice);
            match Pin::new(&mut this.stream).poll_read(cx, &mut tmp) {
                Poll::Ready(Ok(())) => {
                    let n = tmp.filled().len();
                    if n == 0 {
                        this.is_eof = true;
                        if this.read_end_index == 0 {
                            this.ended_between_messages = true;
                            // No partial message is retained, so the framing
                            // buffer can go back with the end of the stream.
                            this.release_read_buf();
                            return Poll::Ready(Err(Error::new(
                                ErrorKind::UnexpectedEof,
                                "message stream closed",
                            )));
                        } else {
                            return Poll::Ready(Err(Error::new(
                                ErrorKind::UnexpectedEof,
                                "EOF reached in the middle of a message",
                            )));
                        }
                    }
                    this.read_end_index += n;
                }
                Poll::Ready(Err(e)) => return Poll::Ready(Err(e)),
                Poll::Pending => {
                    // No message is waiting, so the framing buffer can go back
                    // until the stream is readable again.
                    this.release_read_buf();
                    return Poll::Pending;
                }
            }
        }
    }
}

impl<S: AsyncStream> AsyncWriteMessage for VlessMessageStream<S> {
    fn poll_write_message(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &[u8],
    ) -> Poll<Result<()>> {
        let mut this = self.get_mut();

        if !this.pending_write.is_empty() {
            if let Poll::Ready(Err(e)) = Pin::new(&mut this).poll_flush_message(cx) {
                return Poll::Ready(Err(e));
            }
            // previously this checked this.write_offset < this.pending_write.len(), but
            // we want to make sure the message was flushed in the underlying stream.
            if !this.pending_write.is_empty() {
                return Poll::Pending;
            }
        }

        if buf.len() > 65535 {
            return Poll::Ready(Err(Error::new(
                ErrorKind::InvalidInput,
                "message size too large",
            )));
        }

        this.pending_write
            .extend_from_slice(&(buf.len() as u16).to_be_bytes());
        this.pending_write.extend_from_slice(buf);
        this.write_offset = 0;
        Poll::Ready(Ok(()))
    }
}

impl<S: AsyncStream> AsyncFlushMessage for VlessMessageStream<S> {
    fn poll_flush_message(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Result<()>> {
        let this = self.get_mut();
        while this.write_offset < this.pending_write.len() {
            let chunk = &this.pending_write[this.write_offset..];
            match Pin::new(&mut this.stream).poll_write(cx, chunk) {
                Poll::Ready(Ok(n)) => {
                    if n == 0 {
                        return Poll::Ready(Err(Error::new(
                            ErrorKind::WriteZero,
                            "failed to write message",
                        )));
                    }
                    this.write_offset += n;
                }
                Poll::Ready(Err(e)) => return Poll::Ready(Err(e)),
                Poll::Pending => return Poll::Pending,
            }
        }
        // Once complete, flush the underlying stream.
        match Pin::new(&mut this.stream).poll_flush(cx) {
            Poll::Ready(Ok(())) => {
                // The framed output is fully drained, so its storage can go back
                // instead of staying with the association.
                this.pending_write = Vec::new();
                this.write_offset = 0;
                Poll::Ready(Ok(()))
            }
            Poll::Ready(Err(e)) => Poll::Ready(Err(e)),
            Poll::Pending => Poll::Pending,
        }
    }
}

impl<S: AsyncStream> AsyncShutdownMessage for VlessMessageStream<S> {
    fn poll_shutdown_message(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Result<()>> {
        let this = self.get_mut();
        match <Self as AsyncFlushMessage>::poll_flush_message(Pin::new(this), cx) {
            Poll::Ready(Ok(())) => {}
            other => return other,
        }
        Pin::new(&mut this.stream).poll_shutdown(cx)
    }
}

impl<S: AsyncStream> AsyncPing for VlessMessageStream<S> {
    fn supports_ping(&self) -> bool {
        self.stream.supports_ping()
    }

    fn poll_write_ping(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
    ) -> Poll<std::io::Result<bool>> {
        Pin::new(&mut self.stream).poll_write_ping(cx)
    }
}

impl<S: AsyncStream> AsyncMessageStream for VlessMessageStream<S> {}

#[cfg(test)]
mod tests {
    use super::*;
    use bytes::{Buf, BytesMut};
    use std::task::Waker;
    use tokio::io::AsyncRead;

    /// Inner stream that serves scripted bytes and then reports its end.
    struct ScriptedIo {
        bytes: BytesMut,
    }

    impl AsyncRead for ScriptedIo {
        fn poll_read(
            self: Pin<&mut Self>,
            _cx: &mut Context<'_>,
            buf: &mut ReadBuf<'_>,
        ) -> Poll<Result<()>> {
            let this = self.get_mut();
            let len = buf.remaining().min(this.bytes.len());
            buf.put_slice(&this.bytes[..len]);
            this.bytes.advance(len);
            Poll::Ready(Ok(()))
        }
    }

    impl tokio::io::AsyncWrite for ScriptedIo {
        fn poll_write(
            self: Pin<&mut Self>,
            _cx: &mut Context<'_>,
            buf: &[u8],
        ) -> Poll<Result<usize>> {
            Poll::Ready(Ok(buf.len()))
        }

        fn poll_flush(self: Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<Result<()>> {
            Poll::Ready(Ok(()))
        }

        fn poll_shutdown(self: Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<Result<()>> {
            Poll::Ready(Ok(()))
        }
    }

    impl AsyncPing for ScriptedIo {
        fn supports_ping(&self) -> bool {
            false
        }

        fn poll_write_ping(self: Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<Result<bool>> {
            Poll::Ready(Ok(false))
        }
    }

    impl AsyncStream for ScriptedIo {}

    fn read_one(stream: &mut VlessMessageStream<ScriptedIo>) -> Poll<Result<Vec<u8>>> {
        let waker = Waker::noop();
        let mut cx = Context::from_waker(waker);
        let mut bytes = [0u8; 64];
        let mut buf = ReadBuf::new(&mut bytes);
        match Pin::new(stream).poll_read_message(&mut cx, &mut buf) {
            Poll::Ready(Ok(())) => Poll::Ready(Ok(buf.filled().to_vec())),
            Poll::Ready(Err(error)) => Poll::Ready(Err(error)),
            Poll::Pending => Poll::Pending,
        }
    }

    #[test]
    fn an_empty_frame_is_a_message_and_the_end_is_an_error() {
        let mut bytes = BytesMut::new();
        bytes.extend_from_slice(&[0, 0]); // an empty datagram
        bytes.extend_from_slice(&[0, 4, b'p', b'i', b'n', b'g']);
        let mut stream = VlessMessageStream::new(ScriptedIo { bytes });

        assert!(!stream.read_message_eof_on_empty());
        assert!(matches!(read_one(&mut stream), Poll::Ready(Ok(payload)) if payload.is_empty()));
        assert!(
            matches!(read_one(&mut stream), Poll::Ready(Ok(payload)) if payload == b"ping"),
            "the frame after the empty one must still be delivered"
        );

        let error = match read_one(&mut stream) {
            Poll::Ready(Err(error)) => error,
            other => panic!("expected the end of the stream, got {other:?}"),
        };
        assert_eq!(error.kind(), ErrorKind::UnexpectedEof);
        assert!(
            stream.read_error_is_end_of_stream(),
            "the end between messages is a clean end"
        );
    }

    #[test]
    fn a_frame_truncated_by_the_end_is_not_a_clean_end() {
        let mut bytes = BytesMut::new();
        bytes.extend_from_slice(&[0, 4, b'p', b'i']);
        let mut stream = VlessMessageStream::new(ScriptedIo { bytes });

        let error = match read_one(&mut stream) {
            Poll::Ready(Err(error)) => error,
            other => panic!("expected a truncation error, got {other:?}"),
        };
        assert_eq!(error.kind(), ErrorKind::UnexpectedEof);
        assert!(
            !stream.read_error_is_end_of_stream(),
            "a truncated frame is not a clean end"
        );
    }
}
