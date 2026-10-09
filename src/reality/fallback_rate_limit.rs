use std::future::Future;
use std::io;
use std::pin::Pin;
use std::task::{Context, Poll};
use std::time::Duration;

use tokio::io::{AsyncRead, AsyncWrite, ReadBuf};
use tokio::time::{Instant, Sleep};

use crate::async_stream::{AsyncPing, AsyncStream};
use crate::config::server::FallbackRateLimit;

const NANOS_PER_BYTE: u128 = 1_000_000_000;

#[derive(Default)]
pub(super) struct FallbackLimits {
    pub upload: Option<FallbackRateLimit>,
    pub download: Option<FallbackRateLimit>,
    pub initial_upload: u64,
}

struct TokenBucket {
    free_remaining: u64,
    rate: u64,
    capacity: u128,
    tokens: u128,
    updated: Instant,
    sleep: Option<Pin<Box<Sleep>>>,
}

impl TokenBucket {
    fn new(config: FallbackRateLimit, already_forwarded: u64) -> Self {
        let burst = if config.burst_bytes_per_sec == 0 {
            config.bytes_per_sec
        } else {
            config.burst_bytes_per_sec
        };
        let capacity = u128::from(burst) * NANOS_PER_BYTE;
        Self {
            free_remaining: config.after_bytes.saturating_sub(already_forwarded),
            rate: config.bytes_per_sec,
            capacity,
            tokens: capacity,
            updated: Instant::now(),
            sleep: None,
        }
    }

    fn allowance(&mut self, cx: &mut Context<'_>, requested: usize) -> Poll<usize> {
        if self.free_remaining != 0 {
            return Poll::Ready(requested.min(self.free_remaining.min(usize::MAX as u64) as usize));
        }
        let now = Instant::now();
        let refill = now
            .duration_since(self.updated)
            .as_nanos()
            .saturating_mul(u128::from(self.rate));
        self.tokens = self.tokens.saturating_add(refill).min(self.capacity);
        self.updated = now;
        let available = (self.tokens / NANOS_PER_BYTE).min(usize::MAX as u128) as usize;
        if available != 0 {
            self.sleep = None;
            return Poll::Ready(requested.min(available));
        }
        // At most one timer per writer. Fractional tokens retain subsecond precision.
        let nanos = (NANOS_PER_BYTE - self.tokens).div_ceil(u128::from(self.rate));
        let deadline = now + Duration::from_nanos(nanos as u64);
        let sleep = self
            .sleep
            .get_or_insert_with(|| Box::pin(tokio::time::sleep_until(deadline)));
        sleep.as_mut().reset(deadline);
        let _ = sleep.as_mut().poll(cx);
        Poll::Pending
    }

    fn consume(&mut self, written: usize) {
        if self.free_remaining != 0 {
            self.free_remaining -= written as u64;
            // The full bucket starts at the threshold, not at connection creation.
            if self.free_remaining == 0 {
                self.updated = Instant::now();
            }
        } else {
            self.tokens -= written as u128 * NANOS_PER_BYTE;
        }
    }
}

/// Only transparent fallback writers use this wrapper. Reads never wait for tokens.
struct RateLimitedStream {
    inner: Box<dyn AsyncStream>,
    bucket: TokenBucket,
}

pub(super) fn limit_writer(
    inner: Box<dyn AsyncStream>,
    config: Option<FallbackRateLimit>,
    already_forwarded: u64,
) -> Box<dyn AsyncStream> {
    match config.filter(|config| config.bytes_per_sec != 0) {
        Some(config) => Box::new(RateLimitedStream {
            inner,
            bucket: TokenBucket::new(config, already_forwarded),
        }),
        None => inner,
    }
}

impl AsyncRead for RateLimitedStream {
    fn poll_read(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut ReadBuf<'_>,
    ) -> Poll<io::Result<()>> {
        Pin::new(&mut self.inner).poll_read(cx, buf)
    }
}

impl AsyncWrite for RateLimitedStream {
    fn poll_write(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &[u8],
    ) -> Poll<io::Result<usize>> {
        if buf.is_empty() {
            return Pin::new(&mut self.inner).poll_write(cx, buf);
        }
        let count = std::task::ready!(self.bucket.allowance(cx, buf.len()));
        match Pin::new(&mut self.inner).poll_write(cx, &buf[..count]) {
            Poll::Ready(Ok(written)) => {
                self.bucket.consume(written);
                Poll::Ready(Ok(written))
            }
            result => result,
        }
    }

    fn poll_flush(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        Pin::new(&mut self.inner).poll_flush(cx)
    }

    fn poll_shutdown(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        self.bucket.sleep = None;
        Pin::new(&mut self.inner).poll_shutdown(cx)
    }
}

impl AsyncPing for RateLimitedStream {
    fn supports_ping(&self) -> bool {
        self.inner.supports_ping()
    }

    fn poll_write_ping(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<bool>> {
        Pin::new(&mut self.inner).poll_write_ping(cx)
    }
}

impl AsyncStream for RateLimitedStream {
    fn preferred_write_size(&self) -> Option<std::num::NonZeroUsize> {
        self.inner.preferred_write_size()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    fn config(after_bytes: u64, rate: u64, burst: u64) -> FallbackRateLimit {
        FallbackRateLimit {
            after_bytes,
            bytes_per_sec: rate,
            burst_bytes_per_sec: burst,
        }
    }

    fn allowance(bucket: &mut TokenBucket, count: usize) -> Poll<usize> {
        bucket.allowance(
            &mut Context::from_waker(futures::task::noop_waker_ref()),
            count,
        )
    }

    #[tokio::test(start_paused = true)]
    async fn initial_bytes_threshold_burst_and_fractional_refill() {
        let mut bucket = TokenBucket::new(config(10, 4, 2), 7);
        assert_eq!(allowance(&mut bucket, 100), Poll::Ready(3));
        bucket.consume(3);
        assert_eq!(allowance(&mut bucket, 100), Poll::Ready(2));
        bucket.consume(2);
        assert!(allowance(&mut bucket, 100).is_pending());
        tokio::time::advance(Duration::from_millis(125)).await;
        assert!(allowance(&mut bucket, 100).is_pending());
        tokio::time::advance(Duration::from_millis(125)).await;
        assert_eq!(allowance(&mut bucket, 100), Poll::Ready(1));
        bucket.consume(1);
        tokio::time::advance(Duration::from_secs(100)).await;
        assert_eq!(allowance(&mut bucket, 100), Poll::Ready(2));
        let mut bootstrap = TokenBucket::new(config(3, 1, 0), 100);
        assert_eq!(allowance(&mut bootstrap, 100), Poll::Ready(1));
        bootstrap.consume(1);
        assert!(allowance(&mut bootstrap, 1).is_pending());
    }

    #[tokio::test(start_paused = true)]
    async fn default_burst_low_rate_and_u64_edges() {
        let mut slow = TokenBucket::new(config(0, 1, 0), 0);
        assert_eq!(allowance(&mut slow, 10), Poll::Ready(1));
        slow.consume(1);
        assert!(allowance(&mut slow, 1).is_pending());
        tokio::time::advance(Duration::from_millis(999)).await;
        assert!(allowance(&mut slow, 1).is_pending());
        tokio::time::advance(Duration::from_millis(1)).await;
        assert_eq!(allowance(&mut slow, 1), Poll::Ready(1));
        let mut huge = TokenBucket::new(config(u64::MAX, u64::MAX, u64::MAX), u64::MAX - 1);
        assert_eq!(allowance(&mut huge, 100), Poll::Ready(1));
        huge.consume(1);
        assert_eq!(allowance(&mut huge, usize::MAX), Poll::Ready(usize::MAX));
        huge.consume(usize::MAX);
        tokio::time::advance(Duration::from_secs(2)).await;
        assert_eq!(allowance(&mut huge, usize::MAX), Poll::Ready(usize::MAX));
    }

    struct Scripted {
        pending: bool,
        fail: bool,
        zero: bool,
        _marker: Arc<()>,
    }

    impl AsyncRead for Scripted {
        fn poll_read(
            self: Pin<&mut Self>,
            _: &mut Context<'_>,
            buf: &mut ReadBuf<'_>,
        ) -> Poll<io::Result<()>> {
            if buf.remaining() != 0 {
                buf.put_slice(b"r");
            }
            Poll::Ready(Ok(()))
        }
    }
    impl AsyncWrite for Scripted {
        fn poll_write(
            mut self: Pin<&mut Self>,
            _: &mut Context<'_>,
            bytes: &[u8],
        ) -> Poll<io::Result<usize>> {
            if self.pending {
                self.pending = false;
                return Poll::Pending;
            }
            if self.fail {
                return Poll::Ready(Err(io::ErrorKind::BrokenPipe.into()));
            }
            Poll::Ready(Ok(if self.zero { 0 } else { bytes.len().min(2) }))
        }
        fn poll_flush(self: Pin<&mut Self>, _: &mut Context<'_>) -> Poll<io::Result<()>> {
            Poll::Ready(Ok(()))
        }
        fn poll_shutdown(self: Pin<&mut Self>, _: &mut Context<'_>) -> Poll<io::Result<()>> {
            Poll::Ready(Ok(()))
        }
    }
    impl AsyncPing for Scripted {
        fn supports_ping(&self) -> bool {
            false
        }
        fn poll_write_ping(self: Pin<&mut Self>, _: &mut Context<'_>) -> Poll<io::Result<bool>> {
            Poll::Ready(Ok(false))
        }
    }
    impl AsyncStream for Scripted {}

    fn scripted(pending: bool, fail: bool, zero: bool, marker: Arc<()>) -> Box<dyn AsyncStream> {
        Box::new(Scripted {
            pending,
            fail,
            zero,
            _marker: marker,
        })
    }

    #[tokio::test(start_paused = true)]
    async fn partial_pending_error_zero_reads_flush_shutdown_and_cancellation() {
        let marker = Arc::new(());
        let mut stream = RateLimitedStream {
            inner: scripted(true, false, false, marker.clone()),
            bucket: TokenBucket::new(config(0, 2, 0), 0),
        };
        let mut cx = Context::from_waker(futures::task::noop_waker_ref());
        assert!(
            Pin::new(&mut stream)
                .poll_write(&mut cx, b"abc")
                .is_pending()
        );
        assert_eq!(stream.bucket.tokens, 2 * NANOS_PER_BYTE);
        assert!(matches!(
            Pin::new(&mut stream).poll_write(&mut cx, b"abc"),
            Poll::Ready(Ok(2))
        ));
        assert!(Pin::new(&mut stream).poll_write(&mut cx, b"c").is_pending());
        let mut byte = [0];
        assert!(
            Pin::new(&mut stream)
                .poll_read(&mut cx, &mut ReadBuf::new(&mut byte))
                .is_ready()
        );
        assert_eq!(&byte, b"r");
        assert!(Pin::new(&mut stream).poll_flush(&mut cx).is_ready());
        assert!(stream.bucket.sleep.is_some());
        assert!(Pin::new(&mut stream).poll_shutdown(&mut cx).is_ready());
        assert!(stream.bucket.sleep.is_none());
        assert!(Pin::new(&mut stream).poll_write(&mut cx, b"c").is_pending());
        drop(stream);
        assert_eq!(Arc::strong_count(&marker), 1);
        for (fail, zero) in [(true, false), (false, true)] {
            let mut stream = limit_writer(
                scripted(false, fail, zero, marker.clone()),
                Some(config(0, 1, 0)),
                0,
            );
            let error = stream.write_all(b"abc").await.unwrap_err();
            assert_eq!(
                error.kind(),
                if fail {
                    io::ErrorKind::BrokenPipe
                } else {
                    io::ErrorKind::WriteZero
                }
            );
        }
    }

    #[tokio::test(start_paused = true)]
    async fn missing_and_zero_rates_preserve_passthrough() {
        for cfg in [None, Some(config(u64::MAX, 0, 1))] {
            let marker = Arc::new(());
            let started = Instant::now();
            let mut stream = limit_writer(scripted(false, false, false, marker), cfg, 0);
            stream.write_all(&[0; 1000]).await.unwrap();
            assert_eq!(started.elapsed(), Duration::ZERO);
        }
    }

    #[tokio::test(start_paused = true)]
    async fn independent_directional_limits_and_connection_buckets() {
        let (up, mut up_peer) = tokio::io::duplex(64);
        let (down, mut down_peer) = tokio::io::duplex(64);
        let mut up = limit_writer(Box::new(up), Some(config(0, 1, 0)), 0);
        let mut down = limit_writer(Box::new(down), Some(config(0, 4, 0)), 0);
        up.write_all(b"u").await.unwrap();
        down.write_all(b"down").await.unwrap();
        let mut next_upload = Box::pin(up.write_all(b"p"));
        assert!(futures::poll!(next_upload.as_mut()).is_pending());
        tokio::time::advance(Duration::from_millis(250)).await;
        down.write_all(b"!").await.unwrap();
        assert!(futures::poll!(next_upload.as_mut()).is_pending());
        tokio::time::advance(Duration::from_millis(750)).await;
        next_upload.await.unwrap();
        let mut upload = [0; 2];
        up_peer.read_exact(&mut upload).await.unwrap();
        assert_eq!(&upload, b"up");
        let mut download = [0; 5];
        down_peer.read_exact(&mut download).await.unwrap();
        assert_eq!(&download, b"down!");
        let marker = Arc::new(());
        let mut other_connection = limit_writer(
            scripted(false, false, false, marker),
            Some(config(0, 1, 0)),
            0,
        );
        let started = Instant::now();
        other_connection.write_all(b"x").await.unwrap();
        assert_eq!(started.elapsed(), Duration::ZERO);
    }
}
