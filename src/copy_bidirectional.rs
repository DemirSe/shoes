// Forked from tokio's copy.rs and copy_bidirectional.rs.
//
// Changes:
// - Customizable buffer size
// - Read and write whenever there's a space
// - Circular buffer
// - Cooperative yielding via tokio's coop budget to prevent task starvation

use futures::ready;
use tokio::io::ReadBuf;

use std::future::Future;
use std::io;
use std::pin::Pin;
use std::task::{Context, Poll};

use crate::async_stream::AsyncStream;
use crate::util::allocate_vec;

const DEFAULT_BUF_SIZE: usize = 16384;
const MAX_COPY_BYTES_PER_POLL: usize = 1024 * 1024;

// The two shutdown policies are separate entry points, so each one is a separate
// compile-time instantiation. The ordinary instantiation carries no progress
// counters and no drain checks.
//
// - `copy_bidirectional` fails a stalled half-close shutdown after
//   `SHUTDOWN_TIMEOUT`.
// - `copy_bidirectional_with_progress_aware_shutdown` extends that deadline while
//   either direction still moves bytes, up to `SHUTDOWN_DRAIN_LIMIT` from the first
//   half-close. Fallback REALITY sessions use it so queued, rate-limited reply data
//   drains.

/// Total time a progress-aware transfer may wait on a stalled shutdown. Progress
/// cannot extend the wait past this limit, so a peer that never reaches EOF cannot
/// defer cleanup forever.
const SHUTDOWN_DRAIN_LIMIT: std::time::Duration = std::time::Duration::from_secs(60);

#[derive(Debug)]
struct CopyBuffer<const TRACK_PROGRESS: bool> {
    read_done: bool,
    need_flush: bool,
    need_write_ping: bool,
    start_index: usize,
    cache_length: usize,
    size: usize,
    buf: Box<[u8]>,
    /// Counts completed reads, writes, flushes, and pings. Only
    /// `TRACK_PROGRESS` transfers read the count. The `false` instantiation
    /// never writes it and never compares it.
    progress: u64,
}

impl<const TRACK_PROGRESS: bool> CopyBuffer<TRACK_PROGRESS> {
    pub fn new(size: usize, need_initial_flush: bool) -> Self {
        let buf = allocate_vec(size);
        Self {
            read_done: false,
            need_flush: need_initial_flush,
            need_write_ping: false,
            start_index: 0,
            cache_length: 0,
            size,
            buf: buf.into_boxed_slice(),
            progress: 0,
        }
    }

    /// Records one completed read, write, flush, or ping. The check disappears
    /// from the ordinary instantiation at compile time.
    #[inline(always)]
    fn note_progress(&mut self) {
        if TRACK_PROGRESS {
            self.progress += 1;
        }
    }

    pub fn poll_copy<R, W>(
        &mut self,
        cx: &mut Context<'_>,
        mut reader: Pin<&mut R>,
        mut writer: Pin<&mut W>,
    ) -> Poll<io::Result<()>>
    where
        R: AsyncStream + ?Sized,
        W: AsyncStream + ?Sized,
    {
        let coop = ready!(tokio::task::coop::poll_proceed(cx));
        // Initial output and an unfinished flush remain barriers across polls.
        if self.need_flush {
            ready!(writer.as_mut().poll_flush(cx))?;
            self.need_flush = false;
            coop.made_progress();
            self.note_progress();
        }
        let mut copied = 0;

        loop {
            let mut read_pending = false;
            let mut write_pending = false;

            // Read as much as possible before writing. Some AsyncStream implementations
            // packetize each poll_write call individually, so this reduces the overhead.
            while !self.read_done && self.cache_length < self.size {
                let unused_start_index = (self.start_index + self.cache_length) % self.size;
                let unused_end_index_exclusive = if unused_start_index < self.start_index {
                    self.start_index
                } else {
                    self.size
                };

                let me = &mut *self;
                let mut buf =
                    ReadBuf::new(&mut me.buf[unused_start_index..unused_end_index_exclusive]);
                match reader.as_mut().poll_read(cx, &mut buf) {
                    Poll::Ready(val) => {
                        val?;
                        let n = buf.filled().len();
                        if n == 0 {
                            self.read_done = true;
                        } else {
                            self.cache_length += n;
                            coop.made_progress();
                            self.note_progress();
                        }
                    }
                    Poll::Pending => {
                        read_pending = true;
                        break;
                    }
                }
            }

            if self.need_write_ping {
                // if we just read data and we are going to write anyway, no need for a ping
                if self.cache_length == 0 {
                    match writer.as_mut().poll_write_ping(cx) {
                        Poll::Ready(val) => {
                            let written = val?;
                            self.need_write_ping = false;
                            if written {
                                self.need_flush = true;
                                coop.made_progress();
                                self.note_progress();
                            }
                        }
                        Poll::Pending => {
                            write_pending = true;
                        }
                    }
                } else {
                    self.need_write_ping = false;
                }
            }

            // If our buffer has some data, let's write it out!
            // Loop and try to write out as much as possible to minimize forwarding
            // latency, and so that we increase the chance we have an optimal read
            // with start_index at zero.
            while self.cache_length > 0 {
                let used_start_index = self.start_index;
                let used_end_index_exclusive =
                    (self.start_index + self.cache_length).min(self.size);

                let me = &mut *self;
                match writer
                    .as_mut()
                    .poll_write(cx, &me.buf[used_start_index..used_end_index_exclusive])
                {
                    Poll::Ready(val) => {
                        let written = val?;
                        if written == 0 {
                            return Poll::Ready(Err(io::Error::new(
                                io::ErrorKind::WriteZero,
                                "write zero byte into writer",
                            )));
                        } else {
                            copied += written;
                            self.cache_length -= written;
                            if self.cache_length == 0 {
                                self.start_index = 0;
                            } else {
                                self.start_index = (self.start_index + written) % self.size;
                            }
                            self.need_flush = true;
                            coop.made_progress();
                            self.note_progress();
                        }
                    }
                    Poll::Pending => {
                        write_pending = true;
                        break;
                    }
                }
            }

            // Complete the current chunk rather than splitting a protocol-sized record.
            let quota_reached = copied >= MAX_COPY_BYTES_PER_POLL;
            if self.need_flush && (read_pending || write_pending || self.read_done || quota_reached)
            {
                ready!(writer.as_mut().poll_flush(cx))?;
                self.need_flush = false;
                coop.made_progress();
                self.note_progress();
            }

            // If we've written all the data and we've seen EOF, finish the transfer.
            if self.read_done && self.cache_length == 0 {
                return Poll::Ready(Ok(()));
            }

            // Return Pending to prevent task starvation
            if read_pending || write_pending {
                return Poll::Pending;
            }
            if quota_reached {
                cx.waker().wake_by_ref();
                return Poll::Pending;
            }
        }
    }
}

enum TransferState {
    Running,
    ShuttingDown(Pin<Box<tokio::time::Sleep>>),
    Done,
}

struct CopyBidirectional<'a, A: ?Sized, B: ?Sized, const TRACK_PROGRESS: bool> {
    a: &'a mut A,
    b: &'a mut B,
    a_buf: CopyBuffer<TRACK_PROGRESS>,
    b_buf: CopyBuffer<TRACK_PROGRESS>,
    a_to_b: TransferState,
    b_to_a: TransferState,
    sleep_future: Option<Pin<Box<tokio::time::Sleep>>>,
    /// When the first direction stopped reading and requested a shutdown.
    shutdown_started: Option<tokio::time::Instant>,
}

fn shutdown_timeout_error() -> io::Error {
    io::Error::new(io::ErrorKind::TimedOut, "stream shutdown timed out")
}

fn transfer_one_direction<A, B, const TRACK_PROGRESS: bool>(
    cx: &mut Context<'_>,
    state: &mut TransferState,
    buf: &mut CopyBuffer<TRACK_PROGRESS>,
    r: &mut A,
    w: &mut B,
) -> Poll<io::Result<()>>
where
    A: AsyncStream + ?Sized,
    B: AsyncStream + ?Sized,
{
    let mut r = Pin::new(r);
    let mut w = Pin::new(w);

    loop {
        match state {
            TransferState::Running => {
                ready!(buf.poll_copy(cx, r.as_mut(), w.as_mut()))?;
                *state = TransferState::ShuttingDown(Box::pin(tokio::time::sleep(
                    crate::util::SHUTDOWN_TIMEOUT,
                )));
            }
            TransferState::ShuttingDown(deadline) => match w.as_mut().poll_shutdown(cx) {
                Poll::Ready(result) => {
                    result?;
                    *state = TransferState::Done;
                }
                Poll::Pending => {
                    if TRACK_PROGRESS {
                        // The caller reconciles peer progress first, then polls or
                        // resets this deadline. Deciding here would cancel data that
                        // the peer direction moves in the same poll.
                        return Poll::Pending;
                    }
                    ready!(deadline.as_mut().poll(cx));
                    return Poll::Ready(Err(shutdown_timeout_error()));
                }
            },
            TransferState::Done => return Poll::Ready(Ok(())),
        }
    }
}

impl<A, B, const TRACK_PROGRESS: bool> Future for CopyBidirectional<'_, A, B, TRACK_PROGRESS>
where
    A: AsyncStream + ?Sized,
    B: AsyncStream + ?Sized,
{
    type Output = io::Result<()>;

    fn poll(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        let CopyBidirectional {
            a,
            b,
            a_buf,
            b_buf,
            a_to_b,
            b_to_a,
            sleep_future,
            shutdown_started,
        } = &mut *self;

        if let Some(sleep) = sleep_future {
            let ping_fired = sleep.as_mut().poll(cx).is_ready();
            if ping_fired {
                // a_buf writes to b - so we need to check if b supports ping, and similarly
                // for b_buf.
                a_buf.need_write_ping = b.supports_ping();
                b_buf.need_write_ping = a.supports_ping();
                sleep
                    .as_mut()
                    .reset(tokio::time::Instant::now() + std::time::Duration::from_secs(60));
            }
        }

        // The counter reads exist only in the opt-in instantiation.
        let progress_before = if TRACK_PROGRESS {
            (a_buf.progress, b_buf.progress)
        } else {
            (0, 0)
        };
        let a_to_b_result = transfer_one_direction(cx, a_to_b, &mut *a_buf, &mut *a, &mut *b);
        let b_to_a_result = transfer_one_direction(cx, b_to_a, &mut *b_buf, &mut *b, &mut *a);

        if TRACK_PROGRESS {
            // A genuine I/O error ends the transfer at once; it is never deferred.
            let failed = matches!(a_to_b_result, Poll::Ready(Err(_)))
                || matches!(b_to_a_result, Poll::Ready(Err(_)));
            if !failed {
                let now = tokio::time::Instant::now();
                let stalled = matches!(*a_to_b, TransferState::ShuttingDown(_))
                    || matches!(*b_to_a, TransferState::ShuttingDown(_));
                let progressed = (a_buf.progress, b_buf.progress) != progress_before;
                match (stalled, *shutdown_started) {
                    // The drain limit covers one shutdown phase, so a later half-close
                    // after a completed shutdown starts a fresh limit.
                    (true, None) => *shutdown_started = Some(now),
                    (false, _) => *shutdown_started = None,
                    (true, Some(_)) => {}
                }
                if stalled {
                    let drain_limit =
                        shutdown_started.map(|started| started + SHUTDOWN_DRAIN_LIMIT);
                    // A pending shutdown keeps its deadline while the copy still moves
                    // bytes. Every timer, including one created after the last progress
                    // event, stays inside the phase cap.
                    for state in [&mut *a_to_b, &mut *b_to_a] {
                        if let TransferState::ShuttingDown(deadline) = state {
                            let current = deadline.deadline();
                            let keep = if progressed {
                                now + crate::util::SHUTDOWN_TIMEOUT
                            } else {
                                current
                            };
                            let bounded = drain_limit.map_or(keep, |limit| keep.min(limit));
                            if bounded != current {
                                deadline.as_mut().reset(bounded);
                            }
                        }
                    }
                    // The no-progress timeout and the phase cap are decided only after
                    // both directions and this reconciliation have run. Polling the
                    // timers here also registers their wakers.
                    let mut expired = false;
                    for state in [&mut *a_to_b, &mut *b_to_a] {
                        if let TransferState::ShuttingDown(deadline) = state {
                            expired |= deadline.as_mut().poll(cx).is_ready();
                        }
                    }
                    let capped = drain_limit.is_some_and(|limit| now >= limit);
                    if capped || (!progressed && expired) {
                        return Poll::Ready(Err(shutdown_timeout_error()));
                    }
                }
            }
        }

        match (a_to_b_result, b_to_a_result) {
            (Poll::Ready(Err(e)), _) | (_, Poll::Ready(Err(e))) => Poll::Ready(Err(e)),
            (Poll::Ready(Ok(())), Poll::Ready(Ok(()))) => Poll::Ready(Ok(())),
            _ => Poll::Pending,
        }
    }
}

/// Copies data in both directions between `a` and `b`.
///
/// This function returns a future that will read from both streams,
/// writing any data read to the opposing stream.
/// This happens in both directions concurrently.
///
/// If an EOF is observed on one stream, [`shutdown()`] will be invoked on
/// the other, and reading from that stream will stop. Copying of data in
/// the other direction will continue.
///
/// The future will complete successfully once both directions of communication has been shut down.
/// A direction is shut down when the reader reports EOF,
/// at which point [`shutdown()`] is called on the corresponding writer.
/// Each shutdown has a five-second deadline; an active read direction has no lifetime limit.
///
/// [`shutdown()`]: tokio::io::AsyncWriteExt::shutdown
///
/// # Errors
///
/// The future will immediately return an error if any IO operation on `a`
/// or `b` returns an error. Some data read from either stream may be lost (not
/// written to the other stream) in this case.
///
pub async fn copy_bidirectional<A, B>(
    a: &mut A,
    b: &mut B,
    a_need_initial_flush: bool,
    b_need_initial_flush: bool,
) -> io::Result<()>
where
    A: AsyncStream + ?Sized,
    B: AsyncStream + ?Sized,
{
    copy_bidirectional_with_sizes(
        a,
        b,
        a_need_initial_flush,
        b_need_initial_flush,
        DEFAULT_BUF_SIZE,
        DEFAULT_BUF_SIZE,
    )
    .await
}

/// Copies data in both directions between `a` and `b` and lets a stalled half-close
/// wait for the peer direction to drain.
///
/// The ordinary [`copy_bidirectional()`] fails a stalled shutdown after
/// `SHUTDOWN_TIMEOUT`. This variant extends that deadline while either direction
/// still moves bytes, up to `SHUTDOWN_DRAIN_LIMIT` from the first half-close.
/// Fallback REALITY sessions use it, so a rate-limited reply can drain before
/// cleanup.
///
pub(crate) async fn copy_bidirectional_with_progress_aware_shutdown<A, B>(
    a: &mut A,
    b: &mut B,
    a_need_initial_flush: bool,
    b_need_initial_flush: bool,
) -> io::Result<()>
where
    A: AsyncStream + ?Sized,
    B: AsyncStream + ?Sized,
{
    copy_bidirectional_with_options::<A, B, true>(
        a,
        b,
        a_need_initial_flush,
        b_need_initial_flush,
        DEFAULT_BUF_SIZE,
        DEFAULT_BUF_SIZE,
    )
    .await
}

/// Copies data in both directions between `a` and `b` using buffers of the specified size.
///
/// This method is the same as the [`copy_bidirectional()`], except that it allows you to set the
/// size of the internal buffers used when copying data.
pub async fn copy_bidirectional_with_sizes<A, B>(
    a: &mut A,
    b: &mut B,
    a_need_initial_flush: bool,
    b_need_initial_flush: bool,
    a_to_b_buf_size: usize,
    b_to_a_buf_size: usize,
) -> io::Result<()>
where
    A: AsyncStream + ?Sized,
    B: AsyncStream + ?Sized,
{
    copy_bidirectional_with_options::<A, B, false>(
        a,
        b,
        a_need_initial_flush,
        b_need_initial_flush,
        a_to_b_buf_size,
        b_to_a_buf_size,
    )
    .await
}

async fn copy_bidirectional_with_options<A, B, const TRACK_PROGRESS: bool>(
    a: &mut A,
    b: &mut B,
    a_need_initial_flush: bool,
    b_need_initial_flush: bool,
    a_to_b_buf_size: usize,
    b_to_a_buf_size: usize,
) -> io::Result<()>
where
    A: AsyncStream + ?Sized,
    B: AsyncStream + ?Sized,
{
    let sleep_future = if a.supports_ping() || b.supports_ping() {
        Some(Box::pin(tokio::time::sleep(
            std::time::Duration::from_secs(60),
        )))
    } else {
        None
    };

    let a_to_b_buf_size = copy_buffer_size(a_to_b_buf_size, b);
    let b_to_a_buf_size = copy_buffer_size(b_to_a_buf_size, a);
    CopyBidirectional {
        a,
        b,
        // this is correctly reversed - CopyBuffer will copy from a (reader) to b (writer) using
        // a_buf, which means that the need_flush signal is for the writer (b), and vice versa for
        // b_buf.
        a_buf: CopyBuffer::<TRACK_PROGRESS>::new(a_to_b_buf_size, b_need_initial_flush),
        b_buf: CopyBuffer::<TRACK_PROGRESS>::new(b_to_a_buf_size, a_need_initial_flush),
        a_to_b: TransferState::Running,
        b_to_a: TransferState::Running,
        sleep_future,
        shutdown_started: None,
    }
    .await
}

fn copy_buffer_size(requested: usize, writer: &(impl AsyncStream + ?Sized)) -> usize {
    match writer.preferred_write_size() {
        Some(preferred) => requested.min(preferred.get()),
        None => requested,
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use crate::async_stream::AsyncPing;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::{Arc, Mutex};
    use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};

    #[derive(Default)]
    pub(crate) struct Capture {
        pub(crate) data: Vec<u8>,
        pub(crate) first_write_offer: Vec<u8>,
        writes: Vec<usize>,
        flushes: usize,
        write_release: Option<tokio::sync::oneshot::Receiver<()>>,
        flush_release: Option<tokio::sync::oneshot::Receiver<()>>,
    }

    #[derive(Default)]
    struct MemoryStream {
        input: std::io::Cursor<Vec<u8>>,
        output: Arc<Mutex<Capture>>,
        block_flush: bool,
        pending_at_eof: bool,
        write_limit: Option<usize>,
        preferred_write_size: Option<std::num::NonZeroUsize>,
        write_flush_release: Option<tokio::sync::oneshot::Receiver<()>>,
    }

    impl AsyncRead for MemoryStream {
        fn poll_read(
            mut self: Pin<&mut Self>,
            cx: &mut Context<'_>,
            buf: &mut ReadBuf<'_>,
        ) -> Poll<io::Result<()>> {
            if self.pending_at_eof && self.input.position() as usize == self.input.get_ref().len() {
                return Poll::Pending;
            }
            Pin::new(&mut self.input).poll_read(cx, buf)
        }
    }

    impl AsyncWrite for MemoryStream {
        fn poll_write(
            mut self: Pin<&mut Self>,
            cx: &mut Context<'_>,
            data: &[u8],
        ) -> Poll<io::Result<usize>> {
            let already_written = !self.output.lock().unwrap().data.is_empty();
            if already_written && let Some(release) = &mut self.write_flush_release {
                ready!(Pin::new(release).poll(cx)).unwrap();
                self.write_flush_release = None;
            }
            let mut output = self.output.lock().unwrap();
            if already_written && let Some(release) = &mut output.write_release {
                ready!(Pin::new(release).poll(cx)).unwrap();
                output.write_release = None;
            }
            if output.first_write_offer.is_empty() {
                output.first_write_offer.extend_from_slice(data);
            }
            let data = &data[..data.len().min(self.write_limit.unwrap_or(usize::MAX))];
            output.data.extend_from_slice(data);
            output.writes.push(data.len());
            Poll::Ready(Ok(data.len()))
        }

        fn poll_flush(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
            if let Some(release) = &mut self.write_flush_release {
                ready!(Pin::new(release).poll(cx)).unwrap();
                self.write_flush_release = None;
            }
            if self.block_flush {
                return Poll::Pending;
            }
            let mut output = self.output.lock().unwrap();
            if let Some(release) = &mut output.flush_release {
                ready!(Pin::new(release).poll(cx)).unwrap();
                output.flush_release = None;
            }
            output.flushes += 1;
            Poll::Ready(Ok(()))
        }

        fn poll_shutdown(self: Pin<&mut Self>, _: &mut Context<'_>) -> Poll<io::Result<()>> {
            Poll::Ready(Ok(()))
        }
    }

    impl AsyncPing for MemoryStream {
        fn supports_ping(&self) -> bool {
            false
        }

        fn poll_write_ping(self: Pin<&mut Self>, _: &mut Context<'_>) -> Poll<io::Result<bool>> {
            Poll::Ready(Ok(false))
        }
    }

    impl AsyncStream for MemoryStream {
        fn preferred_write_size(&self) -> Option<std::num::NonZeroUsize> {
            self.preferred_write_size
        }
    }

    struct WakeCount(AtomicUsize);

    impl std::task::Wake for WakeCount {
        fn wake(self: Arc<Self>) {
            self.0.fetch_add(1, Ordering::Relaxed);
        }
    }

    #[tokio::test]
    async fn ready_copy_batches_flushes_and_yields_at_a_byte_quota() {
        let wakes = Arc::new(WakeCount(AtomicUsize::new(0)));
        let waker = std::task::Waker::from(wakes.clone());
        let mut cx = Context::from_waker(&waker);
        let payload = vec![42; MAX_COPY_BYTES_PER_POLL * 4];
        let mut reader = MemoryStream {
            input: std::io::Cursor::new(payload.clone()),
            ..Default::default()
        };
        let mut writer = MemoryStream {
            write_limit: Some(1023),
            ..Default::default()
        };
        let mut buffer = CopyBuffer::<false>::new(DEFAULT_BUF_SIZE, false);
        assert!(
            buffer
                .poll_copy(&mut cx, Pin::new(&mut reader), Pin::new(&mut writer))
                .is_pending()
        );
        assert_eq!(
            writer.output.lock().unwrap().data.len(),
            MAX_COPY_BYTES_PER_POLL
        );
        assert_eq!(writer.output.lock().unwrap().flushes, 1);
        assert_eq!(wakes.0.load(Ordering::Relaxed), 1);
        std::future::poll_fn(|cx| {
            buffer.poll_copy(cx, Pin::new(&mut reader), Pin::new(&mut writer))
        })
        .await
        .unwrap();
        let output = writer.output.lock().unwrap();
        assert_eq!(output.data, payload);
        assert_eq!(output.flushes, 4);
    }

    #[test]
    fn ordinary_copy_leaves_the_progress_counter_untouched() {
        let mut reader = MemoryStream {
            input: std::io::Cursor::new(vec![7; 64]),
            ..Default::default()
        };
        let mut writer = MemoryStream::default();
        let mut buffer = CopyBuffer::<false>::new(DEFAULT_BUF_SIZE, false);
        let mut cx = Context::from_waker(futures::task::noop_waker_ref());
        assert!(matches!(
            buffer.poll_copy(&mut cx, Pin::new(&mut reader), Pin::new(&mut writer)),
            Poll::Ready(Ok(()))
        ));
        assert_eq!(
            buffer.progress, 0,
            "ordinary copies must not update the progress counter"
        );
    }

    #[test]
    fn progress_aware_copy_counts_progress() {
        let mut reader = MemoryStream {
            input: std::io::Cursor::new(vec![7; 64]),
            ..Default::default()
        };
        let mut writer = MemoryStream::default();
        let mut buffer = CopyBuffer::<true>::new(DEFAULT_BUF_SIZE, false);
        let mut cx = Context::from_waker(futures::task::noop_waker_ref());
        assert!(matches!(
            buffer.poll_copy(&mut cx, Pin::new(&mut reader), Pin::new(&mut writer)),
            Poll::Ready(Ok(()))
        ));
        assert!(
            buffer.progress > 0,
            "the progress-aware instantiation must count its progress"
        );
    }

    #[tokio::test]
    async fn write_pressure_resumes_after_a_pending_flush_wakes() {
        let payload: Vec<u8> = (0..128).collect();
        let mut reader = MemoryStream {
            input: std::io::Cursor::new(payload.clone()),
            ..Default::default()
        };
        let (release, blocked) = tokio::sync::oneshot::channel();
        let mut writer = MemoryStream {
            write_limit: Some(7),
            write_flush_release: Some(blocked),
            ..Default::default()
        };
        let mut buffer = CopyBuffer::<false>::new(32, false);
        let wakes = Arc::new(WakeCount(AtomicUsize::new(0)));
        let waker = std::task::Waker::from(wakes.clone());
        let mut cx = Context::from_waker(&waker);
        for _ in 0..2 {
            assert!(
                buffer
                    .poll_copy(&mut cx, Pin::new(&mut reader), Pin::new(&mut writer))
                    .is_pending()
            );
            assert_eq!(reader.input.position(), 32);
            assert_eq!(writer.output.lock().unwrap().data, payload[..7]);
            assert!(buffer.need_flush);
        }
        assert_eq!(wakes.0.load(Ordering::Relaxed), 0);
        release.send(()).unwrap();
        assert_eq!(wakes.0.load(Ordering::Relaxed), 1);

        std::future::poll_fn(|cx| {
            buffer.poll_copy(cx, Pin::new(&mut reader), Pin::new(&mut writer))
        })
        .await
        .unwrap();
        let output = writer.output.lock().unwrap();
        assert_eq!(output.data, payload);
        assert_eq!(output.flushes, 2);
        assert!(!buffer.need_flush);
    }

    #[test]
    fn initial_and_pending_flushes_are_write_barriers() {
        let mut reader = MemoryStream {
            input: std::io::Cursor::new(vec![1; MAX_COPY_BYTES_PER_POLL * 2]),
            ..Default::default()
        };
        let mut writer = MemoryStream {
            block_flush: true,
            ..Default::default()
        };
        let mut buffer = CopyBuffer::<false>::new(DEFAULT_BUF_SIZE, true);
        let mut cx = Context::from_waker(futures::task::noop_waker_ref());
        assert!(
            buffer
                .poll_copy(&mut cx, Pin::new(&mut reader), Pin::new(&mut writer))
                .is_pending()
        );
        assert_eq!(reader.input.position(), 0);
        writer.block_flush = false;
        assert!(
            buffer
                .poll_copy(&mut cx, Pin::new(&mut reader), Pin::new(&mut writer))
                .is_pending()
        );
        assert_eq!(reader.input.position() as usize, MAX_COPY_BYTES_PER_POLL);
        writer.block_flush = true;
        assert!(
            buffer
                .poll_copy(&mut cx, Pin::new(&mut reader), Pin::new(&mut writer))
                .is_pending()
        );
        let position = reader.input.position();
        assert!(
            buffer
                .poll_copy(&mut cx, Pin::new(&mut reader), Pin::new(&mut writer))
                .is_pending()
        );
        assert_eq!(reader.input.position(), position);
        writer.block_flush = false;
        assert!(matches!(
            buffer.poll_copy(&mut cx, Pin::new(&mut reader), Pin::new(&mut writer)),
            Poll::Ready(Ok(()))
        ));
        assert_eq!(
            writer.output.lock().unwrap().data.len(),
            MAX_COPY_BYTES_PER_POLL * 2
        );
    }

    #[test]
    fn sparse_input_is_flushed_without_waiting_for_a_full_batch() {
        let mut reader = MemoryStream {
            input: std::io::Cursor::new(b"interactive".to_vec()),
            pending_at_eof: true,
            ..Default::default()
        };
        let mut writer = MemoryStream::default();
        let mut buffer = CopyBuffer::<false>::new(DEFAULT_BUF_SIZE, false);
        let mut cx = Context::from_waker(futures::task::noop_waker_ref());
        assert!(
            buffer
                .poll_copy(&mut cx, Pin::new(&mut reader), Pin::new(&mut writer))
                .is_pending()
        );
        let output = writer.output.lock().unwrap();
        assert_eq!(output.data, b"interactive");
        assert_eq!(output.flushes, 1);
    }

    #[tokio::test]
    async fn copy_respects_preferred_write_size_and_avoids_tiny_writes() {
        let preferred = std::num::NonZeroUsize::new(16383).unwrap();
        let sink = MemoryStream {
            preferred_write_size: Some(preferred),
            ..Default::default()
        };
        let capture = sink.output.clone();
        let mut writer: Box<dyn AsyncStream> = Box::new(sink);
        writer.write_all(b"warm").await.unwrap();
        writer.flush().await.unwrap();
        capture.lock().unwrap().writes.clear();
        let mut writer = crate::prepend_stream::PrependStream::new(&mut writer, None);
        assert_eq!(copy_buffer_size(16384, &writer), 16383);
        assert_eq!(copy_buffer_size(1024, &writer), 1024);
        assert_eq!(copy_buffer_size(16384, &MemoryStream::default()), 16384);

        let payload: Vec<u8> = (0..1024 * 1024).map(|i| (i ^ (i >> 8)) as u8).collect();
        let mut source = MemoryStream {
            input: std::io::Cursor::new(payload.clone()),
            ..Default::default()
        };
        copy_bidirectional(&mut source, &mut writer, false, false)
            .await
            .unwrap();
        let wire = {
            let captured = capture.lock().unwrap();
            assert_eq!(captured.writes.len(), 65);
            assert!(!captured.writes.contains(&35));
            captured.data.clone()
        };
        assert_eq!(&wire[..4], b"warm");
        assert_eq!(&wire[4..], payload);
    }

    struct StalledShutdown;

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
            data: &[u8],
        ) -> Poll<io::Result<usize>> {
            Poll::Ready(Ok(data.len()))
        }

        fn poll_flush(self: Pin<&mut Self>, _: &mut Context<'_>) -> Poll<io::Result<()>> {
            Poll::Ready(Ok(()))
        }

        fn poll_shutdown(self: Pin<&mut Self>, _: &mut Context<'_>) -> Poll<io::Result<()>> {
            Poll::Pending
        }
    }

    impl AsyncPing for StalledShutdown {
        fn supports_ping(&self) -> bool {
            false
        }

        fn poll_write_ping(self: Pin<&mut Self>, _: &mut Context<'_>) -> Poll<io::Result<bool>> {
            Poll::Ready(Ok(false))
        }
    }

    impl AsyncStream for StalledShutdown {}

    #[tokio::test(start_paused = true)]
    async fn stalled_shutdown_and_error_cleanup_have_deadlines() {
        let start = tokio::time::Instant::now();
        let error = copy_bidirectional(&mut StalledShutdown, &mut StalledShutdown, false, false)
            .await
            .unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::TimedOut);
        assert_eq!(start.elapsed(), crate::util::SHUTDOWN_TIMEOUT);
        let start = tokio::time::Instant::now();
        crate::util::shutdown_stream(&mut StalledShutdown).await;
        assert_eq!(start.elapsed(), crate::util::SHUTDOWN_TIMEOUT);
    }

    #[tokio::test(start_paused = true)]
    async fn half_close_does_not_limit_response_lifetime() {
        let (mut client, mut downstream) = tokio::io::duplex(64);
        let (mut upstream, mut server) = tokio::io::duplex(64);
        let copy = tokio::spawn(async move {
            copy_bidirectional(&mut downstream, &mut upstream, false, false).await
        });
        client.write_all(b"request").await.unwrap();
        client.shutdown().await.unwrap();
        let mut request = Vec::new();
        server.read_to_end(&mut request).await.unwrap();
        assert_eq!(request, b"request");
        tokio::time::advance(std::time::Duration::from_secs(300)).await;
        assert!(!copy.is_finished());
        server.write_all(b"response").await.unwrap();
        server.shutdown().await.unwrap();
        let mut response = Vec::new();
        client.read_to_end(&mut response).await.unwrap();
        assert_eq!(response, b"response");
        copy.await.unwrap().unwrap();
    }

    /// Reader behavior for the shutdown-policy probes.
    #[derive(Clone, Copy)]
    enum PeerReads {
        /// Report EOF immediately.
        Eof,
        /// Release one byte per interval, then report EOF.
        Drip {
            remaining: usize,
            interval: std::time::Duration,
        },
        /// Release one byte per interval, then wait one more interval before EOF.
        DripThenPause {
            remaining: usize,
            interval: std::time::Duration,
        },
        /// Always release another kilobyte, and never report EOF.
        Endless,
    }

    /// Writer behavior for the shutdown-policy probes.
    #[derive(Clone, Copy)]
    enum PeerWrites {
        /// Accept every offered byte, up to `limit` bytes per call.
        Accept { limit: usize },
        /// Never accept data.
        Pending,
    }

    /// Shutdown behavior for the shutdown-policy probes.
    #[derive(Clone, Copy)]
    enum PeerShutdown {
        /// Complete the shutdown at once.
        Ready,
        /// Never complete the shutdown.
        Pending,
        /// Stay pending until the given instant, then complete.
        At(tokio::time::Instant),
    }

    struct ProbePeer {
        reads: PeerReads,
        writes: PeerWrites,
        sleep: Option<Pin<Box<tokio::time::Sleep>>>,
        shutdown: PeerShutdown,
        shutdown_sleep: Option<Pin<Box<tokio::time::Sleep>>>,
        captured: Arc<AtomicUsize>,
        read_calls: Arc<AtomicUsize>,
    }

    impl ProbePeer {
        fn new(reads: PeerReads, writes: PeerWrites, shutdown: PeerShutdown) -> Self {
            Self {
                reads,
                writes,
                sleep: None,
                shutdown,
                shutdown_sleep: None,
                captured: Arc::new(AtomicUsize::new(0)),
                read_calls: Arc::new(AtomicUsize::new(0)),
            }
        }
    }

    impl AsyncRead for ProbePeer {
        fn poll_read(
            self: Pin<&mut Self>,
            cx: &mut Context<'_>,
            buf: &mut ReadBuf<'_>,
        ) -> Poll<io::Result<()>> {
            let me = self.get_mut();
            me.read_calls.fetch_add(1, Ordering::Relaxed);
            match &mut me.reads {
                PeerReads::Eof => Poll::Ready(Ok(())),
                PeerReads::Endless => {
                    let chunk = buf.remaining().min(1024);
                    buf.put_slice(&[0; 1024][..chunk]);
                    Poll::Ready(Ok(()))
                }
                PeerReads::Drip {
                    remaining,
                    interval,
                } => {
                    if *remaining == 0 {
                        return Poll::Ready(Ok(()));
                    }
                    if let Some(sleep) = me.sleep.as_mut() {
                        ready!(sleep.as_mut().poll(cx));
                    }
                    me.sleep = None;
                    buf.put_slice(&[0]);
                    *remaining -= 1;
                    let interval = *interval;
                    me.sleep = Some(Box::pin(tokio::time::sleep(interval)));
                    Poll::Ready(Ok(()))
                }
                PeerReads::DripThenPause {
                    remaining,
                    interval,
                } => {
                    if *remaining == 0 {
                        if let Some(sleep) = me.sleep.as_mut() {
                            ready!(sleep.as_mut().poll(cx));
                        }
                        me.sleep = None;
                        return Poll::Ready(Ok(()));
                    }
                    if let Some(sleep) = me.sleep.as_mut() {
                        ready!(sleep.as_mut().poll(cx));
                    }
                    me.sleep = None;
                    buf.put_slice(&[0]);
                    *remaining -= 1;
                    let interval = *interval;
                    me.sleep = Some(Box::pin(tokio::time::sleep(interval)));
                    Poll::Ready(Ok(()))
                }
            }
        }
    }

    impl AsyncWrite for ProbePeer {
        fn poll_write(
            self: Pin<&mut Self>,
            _: &mut Context<'_>,
            data: &[u8],
        ) -> Poll<io::Result<usize>> {
            let me = self.get_mut();
            match me.writes {
                PeerWrites::Accept { limit } => {
                    let accepted = data.len().min(limit);
                    me.captured.fetch_add(accepted, Ordering::Relaxed);
                    Poll::Ready(Ok(accepted))
                }
                PeerWrites::Pending => Poll::Pending,
            }
        }

        fn poll_flush(self: Pin<&mut Self>, _: &mut Context<'_>) -> Poll<io::Result<()>> {
            Poll::Ready(Ok(()))
        }

        fn poll_shutdown(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
            let me = self.get_mut();
            match me.shutdown {
                PeerShutdown::Ready => Poll::Ready(Ok(())),
                PeerShutdown::Pending => Poll::Pending,
                PeerShutdown::At(at) => {
                    if tokio::time::Instant::now() >= at {
                        return Poll::Ready(Ok(()));
                    }
                    if me.shutdown_sleep.is_none() {
                        me.shutdown_sleep = Some(Box::pin(tokio::time::sleep_until(at)));
                    }
                    if let Some(sleep) = me.shutdown_sleep.as_mut() {
                        ready!(sleep.as_mut().poll(cx));
                    }
                    Poll::Ready(Ok(()))
                }
            }
        }
    }

    impl AsyncPing for ProbePeer {
        fn supports_ping(&self) -> bool {
            false
        }

        fn poll_write_ping(self: Pin<&mut Self>, _: &mut Context<'_>) -> Poll<io::Result<bool>> {
            Poll::Ready(Ok(false))
        }
    }

    impl AsyncStream for ProbePeer {}

    #[tokio::test(start_paused = true)]
    async fn progress_aware_shutdown_waits_for_a_dripping_peer() {
        const REPLY_BYTES: usize = 8;
        let run = |progress_aware: bool| async move {
            let mut stalled = ProbePeer::new(
                PeerReads::Eof,
                PeerWrites::Accept { limit: usize::MAX },
                PeerShutdown::Pending,
            );
            let captured = stalled.captured.clone();
            let mut drip = ProbePeer::new(
                PeerReads::Drip {
                    remaining: REPLY_BYTES,
                    interval: std::time::Duration::from_secs(1),
                },
                PeerWrites::Accept { limit: usize::MAX },
                PeerShutdown::Pending,
            );
            let started = tokio::time::Instant::now();
            let error = if progress_aware {
                copy_bidirectional_with_progress_aware_shutdown(
                    &mut stalled,
                    &mut drip,
                    false,
                    false,
                )
                .await
            } else {
                copy_bidirectional(&mut stalled, &mut drip, false, false).await
            }
            .unwrap_err();
            (
                error.kind(),
                captured.load(Ordering::Relaxed),
                started.elapsed(),
            )
        };

        let (kind, delivered, elapsed) = run(false).await;
        assert_eq!(kind, io::ErrorKind::TimedOut);
        assert_eq!(elapsed, crate::util::SHUTDOWN_TIMEOUT);
        assert!(
            delivered < REPLY_BYTES,
            "the fixed deadline must abandon the queued reply, delivered {delivered}/{REPLY_BYTES}"
        );

        let (kind, delivered, elapsed) = run(true).await;
        assert_eq!(kind, io::ErrorKind::TimedOut);
        assert_eq!(
            delivered, REPLY_BYTES,
            "the progress-aware policy must deliver the queued reply first"
        );
        assert_eq!(elapsed, std::time::Duration::from_secs(12));
    }

    /// Boundary: the stalled direction's deadline expires in the same poll that
    /// moves a reply byte. The queued third byte must survive that poll.
    #[tokio::test(start_paused = true)]
    async fn progress_aware_deferred_timeout_keeps_a_slow_reply_stalled_first() {
        const REPLY_BYTES: usize = 3;
        let mut stalled = ProbePeer::new(
            PeerReads::Eof,
            PeerWrites::Accept { limit: usize::MAX },
            PeerShutdown::Pending,
        );
        let captured = stalled.captured.clone();
        let mut drip = ProbePeer::new(
            PeerReads::Drip {
                remaining: REPLY_BYTES,
                interval: std::time::Duration::from_secs(5),
            },
            PeerWrites::Accept { limit: usize::MAX },
            PeerShutdown::Pending,
        );
        let started = tokio::time::Instant::now();
        let error =
            copy_bidirectional_with_progress_aware_shutdown(&mut stalled, &mut drip, false, false)
                .await
                .unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::TimedOut);
        assert_eq!(captured.load(Ordering::Relaxed), REPLY_BYTES);
        assert_eq!(started.elapsed(), std::time::Duration::from_secs(15));
    }

    /// Boundary: the same expiry with the directions swapped, so the stalled
    /// direction is polled second.
    #[tokio::test(start_paused = true)]
    async fn progress_aware_deferred_timeout_keeps_a_slow_reply_stalled_second() {
        const REPLY_BYTES: usize = 3;
        let mut drip = ProbePeer::new(
            PeerReads::Drip {
                remaining: REPLY_BYTES,
                interval: std::time::Duration::from_secs(5),
            },
            PeerWrites::Accept { limit: usize::MAX },
            PeerShutdown::Pending,
        );
        let mut stalled = ProbePeer::new(
            PeerReads::Eof,
            PeerWrites::Accept { limit: usize::MAX },
            PeerShutdown::Pending,
        );
        let captured = stalled.captured.clone();
        let started = tokio::time::Instant::now();
        let error =
            copy_bidirectional_with_progress_aware_shutdown(&mut drip, &mut stalled, false, false)
                .await
                .unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::TimedOut);
        assert_eq!(captured.load(Ordering::Relaxed), REPLY_BYTES);
        assert_eq!(started.elapsed(), std::time::Duration::from_secs(15));
    }

    /// Boundary: a pending shutdown created after the last progress event must not
    /// outlive the phase cap, even when the earlier shutdown completes at the cap.
    #[tokio::test(start_paused = true)]
    async fn progress_aware_phase_cap_bounds_a_later_pending_shutdown() {
        const REPLY_BYTES: usize = 59;
        // The first pending shutdown has waited since t=0 and completes at the cap.
        let mut drip = ProbePeer::new(
            PeerReads::DripThenPause {
                remaining: REPLY_BYTES,
                interval: std::time::Duration::from_secs(1),
            },
            PeerWrites::Accept { limit: usize::MAX },
            PeerShutdown::At(tokio::time::Instant::now() + std::time::Duration::from_secs(60)),
        );
        // The second pending shutdown appears at t=59, when the drip reaches EOF.
        let mut eof = ProbePeer::new(
            PeerReads::Eof,
            PeerWrites::Accept { limit: usize::MAX },
            PeerShutdown::Pending,
        );
        let captured = eof.captured.clone();
        let started = tokio::time::Instant::now();
        let error =
            copy_bidirectional_with_progress_aware_shutdown(&mut drip, &mut eof, false, false)
                .await
                .unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::TimedOut);
        assert_eq!(captured.load(Ordering::Relaxed), REPLY_BYTES);
        assert_eq!(started.elapsed(), std::time::Duration::from_secs(60));
    }

    #[tokio::test(start_paused = true)]
    async fn progress_aware_drain_stops_at_the_progress_limit() {
        let mut stalled = ProbePeer::new(
            PeerReads::Eof,
            PeerWrites::Accept { limit: usize::MAX },
            PeerShutdown::Pending,
        );
        let mut endless = ProbePeer::new(
            PeerReads::Endless,
            PeerWrites::Accept { limit: usize::MAX },
            PeerShutdown::Pending,
        );
        let captured = stalled.captured.clone();
        let mut copy = Box::pin(copy_bidirectional_with_progress_aware_shutdown(
            &mut stalled,
            &mut endless,
            false,
            false,
        ));
        let started = tokio::time::Instant::now();
        let mut polls = 0u32;
        let error = loop {
            if let Poll::Ready(result) = futures::poll!(copy.as_mut()) {
                break result.unwrap_err();
            }
            polls += 1;
            assert!(polls <= 65, "the shutdown drain outlived its limit");
            tokio::time::advance(std::time::Duration::from_secs(1)).await;
        };
        assert_eq!(error.kind(), io::ErrorKind::TimedOut);
        assert_eq!(started.elapsed(), std::time::Duration::from_secs(60));
        assert!(
            captured.load(Ordering::Relaxed) > 0,
            "the endless peer must make progress for this case to apply"
        );
    }

    #[tokio::test(start_paused = true)]
    async fn progress_aware_drain_limit_restarts_for_a_later_half_close() {
        let mut stalled = ProbePeer::new(
            PeerReads::Eof,
            PeerWrites::Accept { limit: usize::MAX },
            PeerShutdown::Pending,
        );
        let captured = stalled.captured.clone();
        let mut drip = ProbePeer::new(
            PeerReads::Drip {
                remaining: 3,
                interval: std::time::Duration::from_secs(31),
            },
            PeerWrites::Accept { limit: usize::MAX },
            PeerShutdown::Ready,
        );
        let started = tokio::time::Instant::now();
        let error =
            copy_bidirectional_with_progress_aware_shutdown(&mut stalled, &mut drip, false, false)
                .await
                .unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::TimedOut);
        assert_eq!(captured.load(Ordering::Relaxed), 3);
        assert_eq!(started.elapsed(), std::time::Duration::from_secs(67));
    }

    #[tokio::test(start_paused = true)]
    async fn progress_aware_shutdown_needs_progress_to_extend_the_deadline() {
        let mut stalled =
            ProbePeer::new(PeerReads::Eof, PeerWrites::Pending, PeerShutdown::Pending);
        let mut endless = ProbePeer::new(
            PeerReads::Endless,
            PeerWrites::Accept { limit: usize::MAX },
            PeerShutdown::Pending,
        );
        let read_calls = endless.read_calls.clone();
        let mut copy = Box::pin(copy_bidirectional_with_progress_aware_shutdown(
            &mut stalled,
            &mut endless,
            false,
            false,
        ));
        let started = tokio::time::Instant::now();
        let mut cx = Context::from_waker(futures::task::noop_waker_ref());
        for _ in 0..4 {
            assert!(copy.as_mut().poll(&mut cx).is_pending());
        }
        assert!(
            read_calls.load(Ordering::Relaxed) <= 32,
            "a stalled shutdown kept reading an endless peer"
        );
        let error = tokio::time::timeout(std::time::Duration::from_secs(10), copy.as_mut())
            .await
            .expect("the stalled shutdown had no deadline")
            .unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::TimedOut);
        assert_eq!(started.elapsed(), crate::util::SHUTDOWN_TIMEOUT);
        assert!(
            read_calls.load(Ordering::Relaxed) <= 32,
            "reaching the deadline must not spin on the endless peer"
        );
    }
}
