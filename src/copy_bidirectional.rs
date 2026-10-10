// Forked from tokio's copy.rs and copy_bidirectional.rs.
//
// Changes:
// - Customizable buffer size
// - Read and write whenever there's a space
// - Circular buffer
// - Cooperative yielding via tokio's coop budget to prevent task starvation
// - Linux splice fast path for raw TCP directions, with a lossless buffered
//   fallback (see `SpliceState` and `poll_splice_copy`)

use futures::ready;
use tokio::io::ReadBuf;

use std::future::Future;
use std::io;
use std::pin::Pin;
use std::task::{Context, Poll};

use crate::async_stream::AsyncStream;
use crate::util::allocate_vec;

#[cfg(target_os = "linux")]
use crate::linux_splice::{SpliceState, local_socket_buffers_admissible, splice_unsupported};

const DEFAULT_BUF_SIZE: usize = 16384;
const MAX_COPY_BYTES_PER_POLL: usize = 1024 * 1024;

/// Splice syscalls allowed in one poll.
///
/// Byte progress is bounded separately, and a syscall that reports `WouldBlock`
/// moves no bytes, so the count needs its own cap.
#[cfg(target_os = "linux")]
const MAX_SPLICE_SYSCALLS_PER_POLL: usize = 64;

/// Readiness waits allowed after one splice reports `WouldBlock`.
///
/// A `WouldBlock` clears the readiness that produced it, so the loop must wait
/// for that direction again to register its waker. Tokio does the same in its
/// own read and write loops. The cap bounds a spurious readiness report.
#[cfg(target_os = "linux")]
const MAX_SPLICE_READINESS_RETRIES: usize = 4;

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
    /// Linux splice state for this direction. Other targets keep the buffered
    /// path only.
    #[cfg(target_os = "linux")]
    splice: SpliceState,
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
            #[cfg(target_os = "linux")]
            splice: SpliceState::default(),
        }
    }

    /// True while this direction may still yield bytes, including bytes that a
    /// denied splice already moved into the pipe.
    ///
    /// The buffered path uses it so pipe bytes are never abandoned at EOF, and
    /// the fast path uses it to keep draining after EOF.
    fn has_pending_input(&self) -> bool {
        #[cfg(target_os = "linux")]
        {
            !self.read_done || self.splice.pending() > 0
        }
        #[cfg(not(target_os = "linux"))]
        {
            !self.read_done
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

        #[cfg(target_os = "linux")]
        {
            // The fast path may only start from a clean byte boundary, so the
            // pipe can never overtake bytes the ring buffer already holds. It is
            // re-entered after EOF to finish draining the pipe. The flush barrier
            // above has already run for this poll.
            if self.cache_length == 0 && self.has_pending_input() {
                match self.poll_splice_copy(cx, reader.as_mut(), writer.as_mut()) {
                    Poll::Ready(Ok(SpliceAttempt::Complete)) => return Poll::Ready(Ok(())),
                    Poll::Ready(Ok(SpliceAttempt::Pending)) => return Poll::Pending,
                    Poll::Ready(Ok(SpliceAttempt::Buffered)) => {}
                    Poll::Ready(Err(error)) => return Poll::Ready(Err(error)),
                    Poll::Pending => return Poll::Pending,
                }
            }
        }

        loop {
            let mut read_pending = false;
            let mut write_pending = false;

            // Read as much as possible before writing. Some AsyncStream implementations
            // packetize each poll_write call individually, so this reduces the overhead.
            // A denied splice may leave bytes in the pipe after EOF, so the loop
            // keeps running until both the reader and the pipe are empty.
            while self.has_pending_input() && self.cache_length < self.size {
                let unused_start_index = (self.start_index + self.cache_length) % self.size;
                let unused_end_index_exclusive = if unused_start_index < self.start_index {
                    self.start_index
                } else {
                    self.size
                };

                #[cfg(target_os = "linux")]
                if self.splice.pending() > 0 {
                    // A denied splice may already have moved bytes into the pipe.
                    // Those bytes precede every later socket read.
                    let drained = {
                        let Self { buf, splice, .. } = &mut *self;
                        match splice
                            .drain_into(&mut buf[unused_start_index..unused_end_index_exclusive])
                        {
                            Ok(0) => {
                                return Poll::Ready(Err(io::Error::other(
                                    "splice pipe ended while it still held bytes",
                                )));
                            }
                            Ok(drained) => Some(drained),
                            Err(error) if error.kind() == io::ErrorKind::Interrupted => {
                                // A signal interrupted the pipe read. The pipe
                                // still holds the same bytes, so yield and retry.
                                cx.waker().wake_by_ref();
                                None
                            }
                            Err(error) => return Poll::Ready(Err(error)),
                        }
                    };
                    let Some(drained) = drained else {
                        read_pending = true;
                        break;
                    };
                    self.cache_length += drained;
                    coop.made_progress();
                    self.note_progress();
                    continue;
                }

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

            // If we've written all the data, seen EOF, and emptied the pipe,
            // finish the transfer.
            if self.read_done && self.cache_length == 0 && !self.has_pending_input() {
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

    /// Moves payload with `splice(2)` while both ends of this direction are raw
    /// TCP sockets and the ring buffer is empty.
    ///
    /// The pipe keeps the byte order: bytes enter it only while it is empty, and
    /// they leave it before any new socket read. An empty pipe always has a free
    /// slot, so a socket-to-pipe `WouldBlock` belongs to the source readiness
    /// that `try_io` cleared. A nonempty pipe is never refilled, which keeps a
    /// slot-exhausted `WouldBlock` from being mistaken for an idle source.
    ///
    /// A denied or unavailable splice returns `Buffered`, and any bytes already
    /// in the pipe are then drained by the ordinary read loop before the
    /// direction may complete.
    ///
    /// The progress-aware REALITY fallback instantiation keeps its buffered
    /// accounting, so it never reaches this path.
    #[cfg(target_os = "linux")]
    fn poll_splice_copy<R, W>(
        &mut self,
        cx: &mut Context<'_>,
        reader: Pin<&mut R>,
        writer: Pin<&mut W>,
    ) -> Poll<io::Result<SpliceAttempt>>
    where
        R: AsyncStream + ?Sized,
        W: AsyncStream + ?Sized,
    {
        if TRACK_PROGRESS || self.splice.disabled() {
            return Poll::Ready(Ok(SpliceAttempt::Buffered));
        }
        let (Some(read_socket), Some(write_socket)) = (
            reader.as_ref().get_ref().raw_read_socket(),
            writer.as_ref().get_ref().raw_write_socket(),
        ) else {
            return Poll::Ready(Ok(SpliceAttempt::Buffered));
        };
        // This path carries no protocol message of its own, so a writer that
        // owns a ping protocol keeps the buffered path.
        if writer.as_ref().get_ref().supports_ping() {
            return Poll::Ready(Ok(SpliceAttempt::Buffered));
        }
        if self.splice.needs_buffer_check() {
            // Decide once, from read-only queries of the kernel-reported buffer
            // sizes. This covers the two local sockets at this moment only: later
            // changes and an unobservable remote receive window are not covered.
            self.splice
                .record_buffer_check(local_socket_buffers_admissible(read_socket, write_socket));
            if self.splice.disabled() {
                return Poll::Ready(Ok(SpliceAttempt::Buffered));
            }
        }
        debug_assert!(
            !self.need_write_ping,
            "a writer with a pending ping must use the buffered path"
        );

        let mut moved = 0usize;
        let mut syscalls = 0usize;
        let mut readiness_retries = 0usize;
        loop {
            let coop = ready!(tokio::task::coop::poll_proceed(cx));
            if moved >= MAX_COPY_BYTES_PER_POLL || syscalls >= MAX_SPLICE_SYSCALLS_PER_POLL {
                cx.waker().wake_by_ref();
                return Poll::Ready(Ok(SpliceAttempt::Pending));
            }
            let limit = MAX_COPY_BYTES_PER_POLL - moved;

            if self.splice.pending() > 0 {
                // Drain the pipe into the destination before any new socket read.
                match write_socket.poll_write_ready(cx) {
                    Poll::Ready(Ok(())) => {}
                    Poll::Ready(Err(error)) => return Poll::Ready(Err(error)),
                    Poll::Pending => return Poll::Ready(Ok(SpliceAttempt::Pending)),
                }
                syscalls += 1;
                match self.splice.splice_out(write_socket, limit) {
                    Ok(0) => {
                        return Poll::Ready(Err(io::Error::new(
                            io::ErrorKind::WriteZero,
                            "splice wrote zero bytes into writer",
                        )));
                    }
                    Ok(written) => {
                        moved += written;
                        readiness_retries = 0;
                        coop.made_progress();
                        self.note_progress();
                    }
                    Err(error) if error.kind() == io::ErrorKind::WouldBlock => {
                        if readiness_retries >= MAX_SPLICE_READINESS_RETRIES {
                            cx.waker().wake_by_ref();
                            return Poll::Ready(Ok(SpliceAttempt::Pending));
                        }
                        readiness_retries += 1;
                        continue;
                    }
                    Err(error) if error.kind() == io::ErrorKind::Interrupted => {
                        // A signal interrupted the syscall after the wrapper's own
                        // retries. `try_io` only clears readiness for `WouldBlock`,
                        // and no byte counter changed, so this direction needs
                        // another poll.
                        cx.waker().wake_by_ref();
                        return Poll::Ready(Ok(SpliceAttempt::Pending));
                    }
                    Err(error) if splice_unsupported(&error) => {
                        self.splice.disable();
                        return Poll::Ready(Ok(SpliceAttempt::Buffered));
                    }
                    Err(error) => return Poll::Ready(Err(error)),
                }
            } else if !self.read_done {
                match read_socket.poll_read_ready(cx) {
                    Poll::Ready(Ok(())) => {}
                    Poll::Ready(Err(error)) => return Poll::Ready(Err(error)),
                    Poll::Pending => return Poll::Ready(Ok(SpliceAttempt::Pending)),
                }
                syscalls += 1;
                // The request stays within the free pipe space of an empty pipe,
                // so a `WouldBlock` belongs to the socket readiness that
                // `try_io` has just cleared.
                let Some(result) = self.splice.splice_in_ready(read_socket, limit) else {
                    self.splice.disable();
                    return Poll::Ready(Ok(SpliceAttempt::Buffered));
                };
                match result {
                    Ok(0) => {
                        // The peer closed its write side, and the pipe was empty,
                        // so every earlier byte has already reached the writer.
                        self.read_done = true;
                    }
                    Ok(read) => {
                        moved += read;
                        readiness_retries = 0;
                        coop.made_progress();
                        self.note_progress();
                    }
                    Err(error) if error.kind() == io::ErrorKind::WouldBlock => {
                        if readiness_retries >= MAX_SPLICE_READINESS_RETRIES {
                            cx.waker().wake_by_ref();
                            return Poll::Ready(Ok(SpliceAttempt::Pending));
                        }
                        readiness_retries += 1;
                        continue;
                    }
                    Err(error) if error.kind() == io::ErrorKind::Interrupted => {
                        cx.waker().wake_by_ref();
                        return Poll::Ready(Ok(SpliceAttempt::Pending));
                    }
                    Err(error) if splice_unsupported(&error) => {
                        self.splice.disable();
                        return Poll::Ready(Ok(SpliceAttempt::Buffered));
                    }
                    Err(error) => return Poll::Ready(Err(error)),
                }
            }

            // Complete only after EOF and an empty pipe, so no byte is abandoned.
            if self.read_done && self.splice.pending() == 0 {
                return Poll::Ready(Ok(SpliceAttempt::Complete));
            }
        }
    }
}

/// Outcome of one splice attempt for a single direction.
#[cfg(target_os = "linux")]
enum SpliceAttempt {
    /// The reader reported EOF and every byte reached the writer.
    Complete,
    /// This direction must return `Pending`; the blocking readiness is
    /// registered with the task.
    Pending,
    /// This direction continues with buffered copying.
    Buffered,
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

/// Linux splice fast path tests.
///
/// The module mirrors the production gate, so other targets compile without the
/// fast path. Every test resets the fault-injection hooks, because the unit
/// tests run on a single thread.
#[cfg(all(test, target_os = "linux"))]
mod splice_tests {
    use super::{copy_bidirectional, copy_bidirectional_with_progress_aware_shutdown};
    use crate::async_stream::{AsyncPing, AsyncStream};
    use crate::linux_splice::test_hooks;
    use crate::prepend_stream::PrependStream;
    use std::io;
    use std::pin::Pin;
    use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
    use std::sync::{Arc, Mutex};
    use std::task::{Context, Poll};
    use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt, ReadBuf};
    use tokio::net::tcp::{OwnedReadHalf, OwnedWriteHalf};
    use tokio::net::{TcpListener, TcpStream};
    use tokio::time::{Duration, timeout};

    /// One loopback TCP connection, split into its two endpoints.
    async fn tcp_pair(ip: &str) -> io::Result<(TcpStream, TcpStream)> {
        let listener = TcpListener::bind((ip, 0)).await?;
        let address = listener.local_addr()?;
        let (accepted, connector) = tokio::join!(listener.accept(), TcpStream::connect(address));
        let (accepted, _) = accepted?;
        Ok((connector?, accepted))
    }

    /// Runs the ordinary copy between two loopback connections and returns the
    /// client and destination ends.
    async fn start_relay(
        ip: &str,
    ) -> io::Result<(
        TcpStream,
        TcpStream,
        tokio::task::JoinHandle<io::Result<()>>,
    )> {
        let (client, relay_a) = tcp_pair(ip).await?;
        let (relay_b, upstream) = tcp_pair(ip).await?;
        let copy = tokio::spawn(async move {
            let mut relay_a = relay_a;
            let mut relay_b = relay_b;
            copy_bidirectional(&mut relay_a, &mut relay_b, false, false).await
        });
        Ok((client, upstream, copy))
    }

    /// A payload that is not compressible or repetitive.
    fn pattern(len: usize, seed: u8) -> Vec<u8> {
        (0..len)
            .map(|index| (index as u8).wrapping_mul(31).wrapping_add(seed))
            .collect()
    }

    /// Narrows the socket buffers, so a stalled direction stays stalled.
    fn shrink_socket_buffers(stream: &TcpStream, size: usize) {
        let socket = socket2::SockRef::from(stream);
        socket.set_recv_buffer_size(size).unwrap();
        socket.set_send_buffer_size(size).unwrap();
    }

    /// Waits for the relay to finish, so a stalled relay fails visibly.
    async fn finish(copy: tokio::task::JoinHandle<io::Result<()>>) -> io::Result<()> {
        timeout(Duration::from_secs(10), copy)
            .await
            .map_err(|_| io::Error::new(io::ErrorKind::TimedOut, "the relay did not finish"))?
            .map_err(io::Error::other)?
    }

    /// Open descriptors of this process, which the pipe checks compare.
    fn open_descriptor_count() -> usize {
        std::fs::read_dir("/proc/self/fd")
            .map(|entries| entries.count())
            .unwrap_or_default()
    }

    /// Pipe descriptors of this process. The relay fast path is the only part of
    /// these tests that allocates one, so the count is the pipe-leak check.
    fn pipe_descriptor_count() -> usize {
        let Ok(entries) = std::fs::read_dir("/proc/self/fd") else {
            return 0;
        };
        entries
            .filter_map(|entry| entry.ok())
            .filter(|entry| {
                std::fs::read_link(entry.path())
                    .map(|target| target.to_string_lossy().starts_with("pipe:"))
                    .unwrap_or(false)
            })
            .count()
    }

    /// Both ends of one relay, split so a full-duplex exchange can run.
    struct Ends {
        client_reader: OwnedReadHalf,
        client_writer: OwnedWriteHalf,
        upstream_reader: OwnedReadHalf,
        upstream_writer: OwnedWriteHalf,
    }

    impl Ends {
        fn new(client: TcpStream, upstream: TcpStream) -> Self {
            let (client_reader, client_writer) = client.into_split();
            let (upstream_reader, upstream_writer) = upstream.into_split();
            Self {
                client_reader,
                client_writer,
                upstream_reader,
                upstream_writer,
            }
        }

        /// Copies one payload in each direction and returns what each end got.
        ///
        /// All four operations run as independent futures, so a writer that
        /// blocks on a small socket buffer cannot stop the matching reader. The
        /// outer timeout keeps a broken fixture from hanging the suite.
        async fn exchange(
            &mut self,
            request: &[u8],
            response: &[u8],
        ) -> io::Result<(Vec<u8>, Vec<u8>)> {
            let Self {
                client_reader,
                client_writer,
                upstream_reader,
                upstream_writer,
            } = self;
            let mut received_request = vec![0; request.len()];
            let mut received_response = vec![0; response.len()];
            let (wrote_request, read_request, wrote_response, read_response) =
                timeout(Duration::from_secs(30), async {
                    tokio::join!(
                        client_writer.write_all(request),
                        upstream_reader.read_exact(&mut received_request),
                        upstream_writer.write_all(response),
                        client_reader.read_exact(&mut received_response),
                    )
                })
                .await
                .map_err(|_| io::Error::new(io::ErrorKind::TimedOut, "the exchange stalled"))?;
            wrote_request?;
            read_request?;
            wrote_response?;
            read_response?;
            Ok((received_request, received_response))
        }

        /// Half-closes the client first and the destination second.
        async fn close(&mut self) -> io::Result<()> {
            self.client_writer.shutdown().await?;
            self.upstream_writer.shutdown().await?;
            Ok(())
        }
    }

    /// A socket pass-through that never advertises its socket, so the copy has
    /// to keep the buffered path.
    struct HiddenSocket(TcpStream);

    impl AsyncRead for HiddenSocket {
        fn poll_read(
            mut self: Pin<&mut Self>,
            cx: &mut Context<'_>,
            buf: &mut ReadBuf<'_>,
        ) -> Poll<io::Result<()>> {
            Pin::new(&mut self.0).poll_read(cx, buf)
        }
    }

    impl AsyncWrite for HiddenSocket {
        fn poll_write(
            mut self: Pin<&mut Self>,
            cx: &mut Context<'_>,
            buf: &[u8],
        ) -> Poll<io::Result<usize>> {
            Pin::new(&mut self.0).poll_write(cx, buf)
        }

        fn poll_flush(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
            Pin::new(&mut self.0).poll_flush(cx)
        }

        fn poll_shutdown(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
            Pin::new(&mut self.0).poll_shutdown(cx)
        }
    }

    impl AsyncPing for HiddenSocket {
        fn supports_ping(&self) -> bool {
            false
        }

        fn poll_write_ping(self: Pin<&mut Self>, _: &mut Context<'_>) -> Poll<io::Result<bool>> {
            Poll::Ready(Ok(false))
        }
    }

    impl AsyncStream for HiddenSocket {}

    /// A socket pass-through whose flush can be held, so the initial output
    /// barrier of the fast path is observable.
    struct FlushGatedSocket {
        socket: TcpStream,
        flush_blocked: Arc<AtomicBool>,
        flush_waker: Arc<Mutex<Option<std::task::Waker>>>,
        write_calls: Arc<AtomicUsize>,
    }

    struct FlushGate {
        flush_blocked: Arc<AtomicBool>,
        flush_waker: Arc<Mutex<Option<std::task::Waker>>>,
        write_calls: Arc<AtomicUsize>,
    }

    impl FlushGatedSocket {
        fn new(socket: TcpStream) -> (Self, FlushGate) {
            let flush_blocked = Arc::new(AtomicBool::new(true));
            let flush_waker = Arc::new(Mutex::new(None));
            let write_calls = Arc::new(AtomicUsize::new(0));
            let gate = FlushGate {
                flush_blocked: flush_blocked.clone(),
                flush_waker: flush_waker.clone(),
                write_calls: write_calls.clone(),
            };
            (
                Self {
                    socket,
                    flush_blocked,
                    flush_waker,
                    write_calls,
                },
                gate,
            )
        }
    }

    impl FlushGate {
        fn release(&self) {
            self.flush_blocked.store(false, Ordering::Relaxed);
            if let Some(waker) = self.flush_waker.lock().unwrap().take() {
                waker.wake();
            }
        }

        fn write_calls(&self) -> usize {
            self.write_calls.load(Ordering::Relaxed)
        }
    }

    impl AsyncRead for FlushGatedSocket {
        fn poll_read(
            mut self: Pin<&mut Self>,
            cx: &mut Context<'_>,
            buf: &mut ReadBuf<'_>,
        ) -> Poll<io::Result<()>> {
            Pin::new(&mut self.socket).poll_read(cx, buf)
        }
    }

    impl AsyncWrite for FlushGatedSocket {
        fn poll_write(
            mut self: Pin<&mut Self>,
            cx: &mut Context<'_>,
            buf: &[u8],
        ) -> Poll<io::Result<usize>> {
            self.write_calls.fetch_add(1, Ordering::Relaxed);
            Pin::new(&mut self.socket).poll_write(cx, buf)
        }

        fn poll_flush(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
            if self.flush_blocked.load(Ordering::Relaxed) {
                *self.flush_waker.lock().unwrap() = Some(cx.waker().clone());
                return Poll::Pending;
            }
            Pin::new(&mut self.socket).poll_flush(cx)
        }

        fn poll_shutdown(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
            Pin::new(&mut self.socket).poll_shutdown(cx)
        }
    }

    impl AsyncPing for FlushGatedSocket {
        fn supports_ping(&self) -> bool {
            false
        }

        fn poll_write_ping(self: Pin<&mut Self>, _: &mut Context<'_>) -> Poll<io::Result<bool>> {
            Poll::Ready(Ok(false))
        }
    }

    impl AsyncStream for FlushGatedSocket {
        fn raw_read_socket(&self) -> Option<&TcpStream> {
            Some(&self.socket)
        }

        fn raw_write_socket(&self) -> Option<&TcpStream> {
            Some(&self.socket)
        }
    }

    #[ignore = "process-wide counters; runs in an isolated process"]
    #[tokio::test]
    async fn splice_moves_exact_bytes_in_both_directions() -> io::Result<()> {
        test_hooks::reset();
        let (client, upstream, copy) = start_relay("127.0.0.1").await?;
        let mut ends = Ends::new(client, upstream);
        let request = pattern(200_000, 7);
        let response = pattern(300_000, 13);

        let (received_request, received_response) = ends.exchange(&request, &response).await?;
        assert_eq!(received_request, request);
        assert_eq!(received_response, response);
        ends.close().await?;
        finish(copy).await?;

        assert!(
            test_hooks::splice_calls() > 0,
            "the fast path must run on raw loopback sockets"
        );
        assert_eq!(
            test_hooks::splice_bytes(),
            (request.len() + response.len()) as u64 * 2,
            "every payload byte must move through one socket-to-pipe and one pipe-to-socket splice"
        );
        Ok(())
    }

    #[ignore = "process-wide counters; runs in an isolated process"]
    #[tokio::test]
    async fn splice_moves_exact_bytes_over_ipv6_loopback() -> io::Result<()> {
        test_hooks::reset();
        let Ok((client, upstream, copy)) = start_relay("::1").await else {
            eprintln!("SKIP: IPv6 loopback is unavailable");
            return Ok(());
        };
        let mut ends = Ends::new(client, upstream);
        let request = pattern(120_000, 3);
        let response = pattern(90_000, 5);

        let (received_request, received_response) = ends.exchange(&request, &response).await?;
        assert_eq!(received_request, request);
        assert_eq!(received_response, response);
        ends.close().await?;
        finish(copy).await?;
        assert_eq!(
            test_hooks::splice_bytes(),
            (request.len() + response.len()) as u64 * 2,
            "IPv6 payload must move through splice as well"
        );
        Ok(())
    }

    #[ignore = "process-wide counters; runs in an isolated process"]
    #[tokio::test]
    async fn splice_keeps_the_client_first_half_close_order() -> io::Result<()> {
        test_hooks::reset();
        let (mut client, mut upstream, copy) = start_relay("127.0.0.1").await?;

        client.write_all(b"request").await?;
        client.shutdown().await?;
        let mut request = Vec::new();
        upstream.read_to_end(&mut request).await?;
        assert_eq!(request, b"request");

        upstream.write_all(b"reply").await?;
        upstream.shutdown().await?;
        let mut reply = Vec::new();
        client.read_to_end(&mut reply).await?;
        assert_eq!(reply, b"reply");

        finish(copy).await?;
        assert!(test_hooks::splice_calls() > 0);
        Ok(())
    }

    #[ignore = "process-wide counters; runs in an isolated process"]
    #[tokio::test]
    async fn splice_keeps_the_destination_first_half_close_order() -> io::Result<()> {
        test_hooks::reset();
        let (mut client, mut upstream, copy) = start_relay("127.0.0.1").await?;

        upstream.write_all(b"reply").await?;
        upstream.shutdown().await?;
        let mut reply = Vec::new();
        client.read_to_end(&mut reply).await?;
        assert_eq!(reply, b"reply");

        client.write_all(b"request").await?;
        client.shutdown().await?;
        let mut request = Vec::new();
        upstream.read_to_end(&mut request).await?;
        assert_eq!(request, b"request");

        finish(copy).await?;
        assert!(test_hooks::splice_calls() > 0);
        Ok(())
    }

    #[ignore = "process-wide counters; runs in an isolated process"]
    #[tokio::test]
    async fn prepend_prefix_precedes_socket_bytes_and_defers_splice() -> io::Result<()> {
        test_hooks::reset();
        let (mut client, relay_a) = tcp_pair("127.0.0.1").await?;
        let (relay_b, mut upstream) = tcp_pair("127.0.0.1").await?;
        let prefix = b"VISION-PREFIX".to_vec();
        let mut relay_a = PrependStream::new(relay_a, Some(prefix.clone().into_boxed_slice()));
        assert!(
            relay_a.raw_read_socket().is_none(),
            "prepended bytes must keep the buffered path"
        );

        let mut relay_b = relay_b;
        let copy = tokio::spawn(async move {
            copy_bidirectional(&mut relay_a, &mut relay_b, false, false).await
        });

        // The prefix alone must arrive through the buffered path.
        let mut received_prefix = vec![0; prefix.len()];
        upstream.read_exact(&mut received_prefix).await?;
        assert_eq!(received_prefix, prefix);
        assert_eq!(
            test_hooks::splice_calls(),
            0,
            "the buffered path owns the prefixed bytes"
        );

        let first = pattern(4096, 3);
        let mut received_first = vec![0; first.len()];
        client.write_all(&first).await?;
        upstream.read_exact(&mut received_first).await?;
        assert_eq!(received_first, first, "socket bytes must follow the prefix");

        let later = pattern(32 * 1024, 45);
        let mut received_later = vec![0; later.len()];
        client.write_all(&later).await?;
        upstream.read_exact(&mut received_later).await?;
        assert_eq!(received_later, later);
        let spliced = test_hooks::splice_bytes();
        assert!(
            spliced >= 2 * later.len() as u64,
            "splice must take over after the prefix drains: {spliced} bytes"
        );
        assert!(
            spliced < 2 * (prefix.len() + first.len() + later.len()) as u64,
            "the prefix must not move through splice: {spliced} bytes"
        );

        client.shutdown().await?;
        upstream.shutdown().await?;
        finish(copy).await?;
        Ok(())
    }

    #[ignore = "process-wide counters; runs in an isolated process"]
    #[tokio::test]
    async fn ineligible_streams_keep_buffered_copying() -> io::Result<()> {
        test_hooks::reset();
        let (client, relay_a) = tcp_pair("127.0.0.1").await?;
        let (relay_b, upstream) = tcp_pair("127.0.0.1").await?;
        let mut relay_a = relay_a;
        let mut relay_b = HiddenSocket(relay_b);
        let copy = tokio::spawn(async move {
            copy_bidirectional(&mut relay_a, &mut relay_b, false, false).await
        });

        let mut ends = Ends::new(client, upstream);
        let request = pattern(100_000, 5);
        let response = pattern(50_000, 9);
        let (received_request, received_response) = ends.exchange(&request, &response).await?;
        assert_eq!(received_request, request);
        assert_eq!(received_response, response);
        assert_eq!(
            test_hooks::splice_calls(),
            0,
            "a stream that hides its socket must not splice"
        );

        ends.close().await?;
        finish(copy).await?;
        Ok(())
    }

    #[ignore = "process-wide counters; runs in an isolated process"]
    #[tokio::test]
    async fn unsupported_splice_keeps_buffered_copying() -> io::Result<()> {
        test_hooks::reset();
        test_hooks::fail_splice_after(0);
        let (client, upstream, copy) = start_relay("127.0.0.1").await?;
        let mut ends = Ends::new(client, upstream);
        let request = pattern(70_000, 15);
        let response = pattern(90_000, 19);

        let (received_request, received_response) = ends.exchange(&request, &response).await?;
        assert_eq!(received_request, request);
        assert_eq!(received_response, response);
        assert_eq!(test_hooks::splice_calls(), 0);
        ends.close().await?;
        finish(copy).await?;
        Ok(())
    }

    #[ignore = "process-wide counters; runs in an isolated process"]
    #[tokio::test]
    async fn pipe_allocation_failure_keeps_buffered_copying() -> io::Result<()> {
        test_hooks::reset();
        test_hooks::fail_pipe_allocation();
        let (client, upstream, copy) = start_relay("127.0.0.1").await?;
        let mut ends = Ends::new(client, upstream);
        let request = pattern(70_000, 23);
        let response = pattern(90_000, 29);

        let (received_request, received_response) = ends.exchange(&request, &response).await?;
        assert_eq!(received_request, request);
        assert_eq!(received_response, response);
        assert_eq!(test_hooks::splice_calls(), 0);
        ends.close().await?;
        finish(copy).await?;
        Ok(())
    }

    #[ignore = "process-wide counters; runs in an isolated process"]
    #[tokio::test]
    async fn fallback_with_pipe_bytes_keeps_order_and_payload() -> io::Result<()> {
        test_hooks::reset();
        let (mut client, relay_a) = tcp_pair("127.0.0.1").await?;
        let (relay_b, mut upstream) = tcp_pair("127.0.0.1").await?;
        // The request is already in the socket, so the first splice fills the
        // pipe and the second one, the pipe-to-socket splice, is denied.
        let request = pattern(8192, 11);
        client.write_all(&request).await?;
        test_hooks::fail_splice_after(1);

        let mut relay_a = relay_a;
        let mut relay_b = relay_b;
        let copy = tokio::spawn(async move {
            copy_bidirectional(&mut relay_a, &mut relay_b, false, false).await
        });

        let mut received_request = vec![0; request.len()];
        upstream.read_exact(&mut received_request).await?;
        assert_eq!(received_request, request, "pipe bytes must arrive first");

        let rest = pattern(64 * 1024 + 7, 21);
        let response = pattern(4096, 27);
        let mut received_rest = vec![0; rest.len()];
        let mut received_response = vec![0; response.len()];
        client.write_all(&rest).await?;
        upstream.read_exact(&mut received_rest).await?;
        assert_eq!(received_rest, rest);
        upstream.write_all(&response).await?;
        client.read_exact(&mut received_response).await?;
        assert_eq!(received_response, response);

        client.shutdown().await?;
        upstream.shutdown().await?;
        finish(copy).await?;
        assert_eq!(
            test_hooks::splice_calls(),
            1,
            "only the socket-to-pipe splice may succeed before the denial"
        );
        assert!(
            test_hooks::splice_bytes() > 0,
            "the pipe must have held bytes when the fallback began"
        );
        Ok(())
    }

    #[ignore = "process-wide counters; runs in an isolated process"]
    #[tokio::test]
    async fn flush_barrier_precedes_the_fast_path() -> io::Result<()> {
        test_hooks::reset();
        let (mut client, reader) = tcp_pair("127.0.0.1").await?;
        let (gated_socket, mut peer) = tcp_pair("127.0.0.1").await?;
        let (gated, gate) = FlushGatedSocket::new(gated_socket);
        let payload = pattern(4096, 17);
        client.write_all(&payload).await?;

        let mut reader = reader;
        let mut gated = gated;
        let copy =
            tokio::spawn(
                async move { copy_bidirectional(&mut reader, &mut gated, false, true).await },
            );

        for _ in 0..8 {
            tokio::task::yield_now().await;
        }
        assert_eq!(
            test_hooks::splice_calls(),
            0,
            "the initial output flush is a barrier"
        );
        assert_eq!(gate.write_calls(), 0);

        gate.release();
        let mut received = vec![0; payload.len()];
        timeout(Duration::from_secs(5), peer.read_exact(&mut received)).await??;
        assert_eq!(received, payload);
        assert_eq!(
            gate.write_calls(),
            0,
            "the fast path must move payload without poll_write"
        );
        assert!(test_hooks::splice_calls() > 0);

        client.shutdown().await?;
        peer.shutdown().await?;
        finish(copy).await?;
        Ok(())
    }

    #[ignore = "process-wide counters; runs in an isolated process"]
    #[tokio::test]
    async fn stalled_output_does_not_spin() -> io::Result<()> {
        test_hooks::reset();
        let (mut client, relay_a) = tcp_pair("127.0.0.1").await?;
        let (relay_b, upstream) = tcp_pair("127.0.0.1").await?;
        // This end never reads, and both buffers are small, so the destination
        // stays blocked once the pipe fills. The sizes stay above the guard
        // threshold, so the fast path is still exercised.
        shrink_socket_buffers(&relay_b, 32 * 1024);
        shrink_socket_buffers(&upstream, 32 * 1024);
        let payload = pattern(4 * 1024 * 1024, 33);
        let upload = tokio::spawn(async move { client.write_all(&payload).await });
        // Let the uploader fill every socket buffer before the manual polls, so
        // the pipe and the destination are already stalled.
        tokio::time::sleep(Duration::from_millis(200)).await;

        let mut relay_a = relay_a;
        let mut relay_b = relay_b;
        let mut copy = Box::pin(copy_bidirectional(&mut relay_a, &mut relay_b, false, false));
        let mut cx = Context::from_waker(futures::task::noop_waker_ref());
        for _ in 0..8 {
            assert!(copy.as_mut().poll(&mut cx).is_pending());
        }
        assert!(
            test_hooks::splice_bytes() >= 4096,
            "the stalled case needs bytes in flight"
        );

        let calls_before = test_hooks::splice_calls();
        let started = std::time::Instant::now();
        for _ in 0..64 {
            assert!(
                copy.as_mut().poll(&mut cx).is_pending(),
                "a blocked writer must yield instead of finishing"
            );
        }
        let calls = test_hooks::splice_calls() - calls_before;
        assert!(
            calls <= 128,
            "a stalled output must not spin: {calls} splice calls in 64 polls"
        );
        assert!(
            started.elapsed() < Duration::from_secs(5),
            "64 polls of a stalled output took {:?}",
            started.elapsed()
        );

        drop(copy);
        upload.abort();
        Ok(())
    }

    #[ignore = "process-wide counters; runs in an isolated process"]
    #[tokio::test]
    async fn the_reverse_direction_progresses_while_the_output_stalls() -> io::Result<()> {
        test_hooks::reset();
        let (client, relay_a) = tcp_pair("127.0.0.1").await?;
        let (relay_b, mut upstream) = tcp_pair("127.0.0.1").await?;
        // Only the destination side is narrow, so the upload stalls with a full
        // pipe while the reverse direction keeps its normal buffers. The sizes
        // stay above the guard threshold, so the fast path is still exercised.
        shrink_socket_buffers(&relay_b, 32 * 1024);
        shrink_socket_buffers(&upstream, 32 * 1024);
        let (mut client_reader, mut client_writer) = client.into_split();

        let upload_len = 256 * 1024;
        let upload = tokio::spawn(async move {
            let payload = pattern(upload_len, 37);
            client_writer
                .write_all(&payload)
                .await
                .map(|()| client_writer)
        });

        let mut relay_a = relay_a;
        let mut relay_b = relay_b;
        let copy = tokio::spawn(async move {
            copy_bidirectional(&mut relay_a, &mut relay_b, false, false).await
        });

        // This end does not read the upload yet, so the client-to-destination
        // direction stays backpressured while the reply travels the other way.
        timeout(Duration::from_secs(5), upstream.write_all(b"pong")).await??;
        let mut pong = [0; 4];
        timeout(Duration::from_secs(5), client_reader.read_exact(&mut pong)).await??;
        assert_eq!(&pong, b"pong");
        assert!(
            test_hooks::splice_bytes() >= 2 * pong.len() as u64,
            "the reply must move through the fast path while the other direction stalls"
        );

        let mut received = vec![0; upload_len];
        timeout(Duration::from_secs(30), upstream.read_exact(&mut received)).await??;
        assert_eq!(received, pattern(upload_len, 37));
        let mut client_writer = timeout(Duration::from_secs(30), upload).await???;
        timeout(Duration::from_secs(5), client_writer.shutdown()).await??;
        timeout(Duration::from_secs(5), upstream.shutdown()).await??;
        let copy_result = timeout(Duration::from_secs(10), copy).await??;
        copy_result?;
        Ok(())
    }

    #[ignore = "process-wide counters; runs in an isolated process"]
    #[tokio::test]
    async fn descriptors_return_to_baseline_after_sessions_and_cancellation() -> io::Result<()> {
        test_hooks::reset();
        let payload = pattern(4096, 43);

        // Warm the runtime's own descriptors before the baseline.
        let (client, upstream, copy) = start_relay("127.0.0.1").await?;
        let mut ends = Ends::new(client, upstream);
        ends.exchange(&payload, &payload).await?;
        ends.close().await?;
        finish(copy).await?;
        // Every test socket is dropped at this point, so this is a like-for-like
        // lifecycle baseline for the leak checks below.
        let baseline = open_descriptor_count();
        let pipe_baseline = pipe_descriptor_count();
        let allocations_baseline = test_hooks::pipe_allocations();
        assert!(
            allocations_baseline > 0,
            "the warm session must have allocated a splice pipe"
        );

        for _ in 0..12 {
            let (client, upstream, copy) = start_relay("127.0.0.1").await?;
            let mut ends = Ends::new(client, upstream);
            ends.exchange(&payload, &payload).await?;
            ends.close().await?;
            finish(copy).await?;
        }
        assert_eq!(
            pipe_descriptor_count(),
            pipe_baseline,
            "completed sessions must release every pipe"
        );
        assert!(
            open_descriptor_count() <= baseline,
            "completed sessions must not add descriptors"
        );

        for _ in 0..12 {
            let (mut client, relay_a) = tcp_pair("127.0.0.1").await?;
            let (relay_b, _upstream) = tcp_pair("127.0.0.1").await?;
            client.write_all(&payload).await?;
            // Wait for the payload, so the first poll reaches the fast path.
            timeout(Duration::from_secs(5), relay_a.readable()).await??;
            let mut relay_a = relay_a;
            let mut relay_b = relay_b;
            let mut copy = Box::pin(copy_bidirectional(&mut relay_a, &mut relay_b, false, false));
            let mut cx = Context::from_waker(futures::task::noop_waker_ref());
            for _ in 0..4 {
                assert!(copy.as_mut().poll(&mut cx).is_pending());
            }
            assert!(
                test_hooks::splice_calls() > 0,
                "the cancelled session must reach the fast path"
            );
            drop(copy);
            drop(relay_a);
            drop(relay_b);
            drop(client);
        }
        assert_eq!(
            pipe_descriptor_count(),
            pipe_baseline,
            "cancelled sessions must release every pipe"
        );
        assert!(
            test_hooks::pipe_allocations() > allocations_baseline,
            "the cancelled sessions must have reached the fast path"
        );
        assert!(
            open_descriptor_count() <= baseline,
            "cancelled sessions must not add descriptors"
        );
        Ok(())
    }

    /// User plus system CPU time of this process.
    fn process_cpu_time() -> Duration {
        // SAFETY: `getrusage` only writes into the provided struct.
        let mut usage: libc::rusage = unsafe { std::mem::zeroed() };
        // SAFETY: as above, with a valid pointer to a `rusage`.
        unsafe { libc::getrusage(libc::RUSAGE_SELF, &mut usage) };
        Duration::from_secs(usage.ru_utime.tv_sec as u64 + usage.ru_stime.tv_sec as u64)
            + Duration::from_micros(usage.ru_utime.tv_usec as u64 + usage.ru_stime.tv_usec as u64)
    }

    /// One loopback TCP connection, with the connector and acceptor roles either
    /// as usual or swapped.
    async fn tcp_pair_with_roles(ip: &str, swap: bool) -> io::Result<(TcpStream, TcpStream)> {
        let listener = TcpListener::bind((ip, 0)).await?;
        let address = listener.local_addr()?;
        let (connector, accepted) = tokio::join!(TcpStream::connect(address), listener.accept());
        let (accepted, _) = accepted?;
        let (connector, accepted) = (connector?, accepted);
        Ok(if swap {
            (accepted, connector)
        } else {
            (connector, accepted)
        })
    }

    /// Queried socket settings, so a diagnostic records what the kernel applied.
    fn socket_settings(stream: &TcpStream) -> String {
        let socket = socket2::SockRef::from(stream);
        format!(
            "sndbuf={:?} rcvbuf={:?} nodelay={:?}",
            socket.send_buffer_size(),
            socket.recv_buffer_size(),
            socket.tcp_nodelay(),
        )
    }

    /// One diagnostic sample with exact byte verification.
    async fn diagnostic_sample(
        payload: &Arc<Vec<u8>>,
        forward: bool,
        swap_roles: bool,
        nodelay: bool,
        buffer: usize,
        splice: bool,
    ) -> io::Result<(Duration, u64, String)> {
        test_hooks::reset();
        let (client, relay_a) = tcp_pair_with_roles("127.0.0.1", swap_roles).await?;
        let (relay_b, destination) = tcp_pair_with_roles("127.0.0.1", swap_roles).await?;
        for socket in [&client, &relay_a, &relay_b, &destination] {
            shrink_socket_buffers(socket, buffer);
            if nodelay {
                socket.set_nodelay(true)?;
            }
        }
        let settings = format!(
            "client[{}] relay_a[{}] relay_b[{}] destination[{}]",
            socket_settings(&client),
            socket_settings(&relay_a),
            socket_settings(&relay_b),
            socket_settings(&destination),
        );
        let mut reader: Box<dyn AsyncStream> = if splice {
            Box::new(relay_a)
        } else {
            Box::new(HiddenSocket(relay_a))
        };
        let mut writer: Box<dyn AsyncStream> = if splice {
            Box::new(relay_b)
        } else {
            Box::new(HiddenSocket(relay_b))
        };
        let (client_reader, client_writer) = client.into_split();
        let (destination_reader, destination_writer) = destination.into_split();
        let (mut source_writer, mut sink_reader, unused_writer, unused_reader) = if forward {
            (
                client_writer,
                destination_reader,
                destination_writer,
                client_reader,
            )
        } else {
            (
                destination_writer,
                client_reader,
                client_writer,
                destination_reader,
            )
        };
        drop(unused_writer);
        drop(unused_reader);

        let started = std::time::Instant::now();
        let copy = tokio::spawn(async move {
            copy_bidirectional(&mut reader, &mut writer, false, false).await
        });
        let uploading = payload.clone();
        let upload = tokio::spawn(async move {
            source_writer.write_all(&uploading).await.unwrap();
            source_writer.shutdown().await.unwrap();
        });
        let expected = payload.clone();
        let download = tokio::spawn(async move {
            let mut received = vec![0; expected.len()];
            sink_reader.read_exact(&mut received).await.unwrap();
            assert_eq!(&received[..], &expected[..], "payload must stay byte exact");
            let mut trailing = [0u8; 1];
            assert_eq!(sink_reader.read(&mut trailing).await.unwrap(), 0);
        });
        timeout(Duration::from_secs(120), upload).await??;
        timeout(Duration::from_secs(120), download).await??;
        finish(copy).await?;
        let elapsed = started.elapsed();
        Ok((elapsed, test_hooks::splice_calls(), settings))
    }

    /// Bounded diagnostic for the constrained window case.
    ///
    /// It swaps the connector and acceptor roles and repeats the production
    /// `TCP_NODELAY` choice, so the paired buffered/splice samples can be
    /// compared under two narrow fixture variants.
    ///
    /// Ignored by default, because it is a measurement, not a correctness check.
    /// Run it with `--release -- --ignored --nocapture`.
    #[tokio::test]
    #[ignore = "local measurement"]
    async fn diagnose_constrained_window_case() -> io::Result<()> {
        const BUFFER: usize = 8 * 1024;
        const PAYLOAD: usize = 128 * 1024;
        const ROUNDS: usize = 4;
        let payload = Arc::new(pattern(PAYLOAD, 5));
        for (label, swap_roles, nodelay) in [
            ("baseline", false, false),
            ("roles_swapped", true, false),
            ("nodelay", false, true),
        ] {
            for forward in [true, false] {
                for round in 0..ROUNDS {
                    for splice in [true, false] {
                        let (elapsed, calls, settings) = diagnostic_sample(
                            &payload, forward, swap_roles, nodelay, BUFFER, splice,
                        )
                        .await?;
                        let rate_kib_s = PAYLOAD as f64 / elapsed.as_secs_f64() / 1024.0;
                        eprintln!(
                            "DIAG config={label} forward={forward} round={round} path={} elapsed_ms={} rate_kib_s={rate_kib_s:.1} splice_calls={calls}",
                            if splice { "splice" } else { "buffered" },
                            elapsed.as_millis(),
                        );
                        if round == 0 && splice {
                            eprintln!("DIAG settings config={label} forward={forward} {settings}");
                        }
                    }
                }
            }
        }
        Ok(())
    }

    /// Finite, symmetric comparison of both relay paths on this source.
    ///
    /// Both variants run over the same sockets, the same payload, the same peer
    /// code, and the same build. Only the relay's eligibility differs: the
    /// buffered variant hides the socket from the fast path, while the stream's
    /// own I/O path is unchanged. Each case measures both directions, with and
    /// without constrained socket buffers.
    ///
    /// Ignored by default, because it is a measurement, not a correctness check.
    /// Run it with `--release -- --ignored --nocapture`.
    #[tokio::test]
    #[ignore = "local measurement"]
    async fn measure_buffered_and_splice_throughput() -> io::Result<()> {
        const SAMPLES: usize = 5;
        // The sweep shows whether a loss is confined to artificially tiny
        // windows or persists as the buffers grow. `None` keeps the OS defaults.
        for buffer_size in [None, Some(8 * 1024), Some(32 * 1024), Some(128 * 1024)] {
            let constrained = buffer_size.is_some();
            let payload_len = if constrained {
                256 * 1024
            } else {
                32 * 1024 * 1024
            };
            let payload = Arc::new(pattern(payload_len, 3));
            for splice in [true, false] {
                for forward in [true, false] {
                    let mut rates = Vec::new();
                    let mut cpu_ms = Vec::new();
                    let mut admissible = false;
                    for _ in 0..SAMPLES {
                        test_hooks::reset();
                        let (client, relay_a) = tcp_pair("127.0.0.1").await?;
                        let (relay_b, destination) = tcp_pair("127.0.0.1").await?;
                        if let Some(buffer_size) = buffer_size {
                            // Every socket of the path gets the same size, so the
                            // two ends stay symmetric.
                            for socket in [&client, &relay_a, &relay_b, &destination] {
                                shrink_socket_buffers(socket, buffer_size);
                            }
                        }
                        // The guard decides from the kernel-reported sizes of the
                        // two relay sockets, so the expectation follows them.
                        admissible = [&relay_a, &relay_b].iter().all(|stream| {
                            let socket = socket2::SockRef::from(*stream);
                            let send = socket.send_buffer_size().unwrap_or_default();
                            let receive = socket.recv_buffer_size().unwrap_or_default();
                            send >= crate::linux_splice::MIN_SOCKET_BUFFER_BYTES
                                && receive >= crate::linux_splice::MIN_SOCKET_BUFFER_BYTES
                        });
                        let pipes_before = pipe_descriptor_count();
                        let allocations_before = test_hooks::pipe_allocations();
                        let mut reader: Box<dyn AsyncStream> = if splice {
                            Box::new(relay_a)
                        } else {
                            Box::new(HiddenSocket(relay_a))
                        };
                        let mut writer: Box<dyn AsyncStream> = if splice {
                            Box::new(relay_b)
                        } else {
                            Box::new(HiddenSocket(relay_b))
                        };
                        let (client_reader, client_writer) = client.into_split();
                        let (destination_reader, destination_writer) = destination.into_split();
                        // The same peer code runs in both directions. The unused
                        // halves are dropped, which half-closes their side, so the
                        // relay sees EOF in both directions.
                        let (mut source_writer, mut sink_reader, unused_writer, unused_reader) =
                            if forward {
                                (
                                    client_writer,
                                    destination_reader,
                                    destination_writer,
                                    client_reader,
                                )
                            } else {
                                (
                                    destination_writer,
                                    client_reader,
                                    client_writer,
                                    destination_reader,
                                )
                            };
                        drop(unused_writer);
                        drop(unused_reader);

                        let cpu_before = process_cpu_time();
                        let started = std::time::Instant::now();
                        let copy = tokio::spawn(async move {
                            copy_bidirectional(&mut reader, &mut writer, false, false).await
                        });
                        let uploading = payload.clone();
                        let upload = tokio::spawn(async move {
                            source_writer.write_all(&uploading).await.unwrap();
                            source_writer.shutdown().await.unwrap();
                        });
                        let expected = payload.clone();
                        let download = tokio::spawn(async move {
                            let mut received = vec![0; expected.len()];
                            sink_reader.read_exact(&mut received).await.unwrap();
                            assert_eq!(
                                &received[..],
                                &expected[..],
                                "payload must stay byte exact"
                            );
                            let mut trailing = [0u8; 1];
                            assert_eq!(sink_reader.read(&mut trailing).await.unwrap(), 0);
                        });
                        timeout(Duration::from_secs(300), upload).await??;
                        timeout(Duration::from_secs(300), download).await??;
                        finish(copy).await?;
                        let elapsed = started.elapsed();
                        let cpu = process_cpu_time() - cpu_before;
                        let spliced = test_hooks::splice_bytes();
                        let pipes_after = pipe_descriptor_count();
                        if splice && admissible {
                            // The favourable cases must prove real kernel splice.
                            assert_eq!(
                                spliced,
                                2 * payload_len as u64,
                                "every byte must move through splice"
                            );
                            assert!(
                                test_hooks::pipe_allocations() > allocations_before,
                                "the fast path must have allocated a pipe"
                            );
                        } else {
                            // A guarded or buffered case must prove ordinary
                            // copying, with no pipe at all.
                            assert_eq!(
                                spliced, 0,
                                "a guarded or buffered direction must not splice"
                            );
                            assert_eq!(
                                pipes_after, pipes_before,
                                "a guarded or buffered direction must not hold a pipe"
                            );
                            assert_eq!(
                                test_hooks::pipe_allocations(),
                                allocations_before,
                                "a guarded or buffered direction must not allocate a pipe"
                            );
                        }
                        rates.push(payload_len as f64 / elapsed.as_secs_f64() / (1024.0 * 1024.0));
                        cpu_ms.push(cpu.as_secs_f64() * 1e3);
                    }
                    rates.sort_by(|left, right| left.total_cmp(right));
                    cpu_ms.sort_by(|left, right| left.total_cmp(right));
                    eprintln!(
                        "MEASURE buffer={buffer_size:?} admissible={admissible} splice={splice} forward={forward} payload_kib={} samples_mib_per_s={rates:?} median_mib_per_s={:.3} samples_cpu_ms={cpu_ms:?} median_cpu_ms={:.2}",
                        payload_len / 1024,
                        rates[rates.len() / 2],
                        cpu_ms[cpu_ms.len() / 2],
                    );
                }
            }
        }
        Ok(())
    }

    /// Many tiny segments fill the pipe slots long before they fill its bytes,
    /// so a socket-to-pipe splice can report `WouldBlock` while the pipe still
    /// reports free space. The relay must keep the source edge and stay exact.
    #[ignore = "process-wide counters; runs in an isolated process"]
    #[tokio::test]
    async fn short_fragmented_input_stays_byte_exact() -> io::Result<()> {
        test_hooks::reset();
        let (mut client, relay_a) = tcp_pair("127.0.0.1").await?;
        let (relay_b, mut upstream) = tcp_pair("127.0.0.1").await?;
        // A narrow destination keeps the pipe from draining immediately. The
        // sizes stay above the guard threshold, so the fast path is exercised.
        shrink_socket_buffers(&relay_b, 32 * 1024);
        shrink_socket_buffers(&upstream, 32 * 1024);
        client.set_nodelay(true)?;
        let mut relay_a = relay_a;
        let mut relay_b = relay_b;
        let copy = tokio::spawn(async move {
            copy_bidirectional(&mut relay_a, &mut relay_b, false, false).await
        });

        // One byte per segment, with a gap, so each write arrives separately.
        const FRAGMENTS: usize = 1024;
        let sender = tokio::spawn(async move {
            for index in 0..FRAGMENTS {
                client.write_all(&[index as u8]).await.unwrap();
                tokio::time::sleep(Duration::from_micros(200)).await;
            }
            client.shutdown().await.unwrap();
            client
        });
        let mut received = Vec::with_capacity(FRAGMENTS);
        let mut buffer = [0u8; 256];
        timeout(Duration::from_secs(30), async {
            while received.len() < FRAGMENTS {
                let read = upstream.read(&mut buffer).await.unwrap();
                if read == 0 {
                    break;
                }
                received.extend_from_slice(&buffer[..read]);
            }
        })
        .await?;
        let expected: Vec<u8> = (0..FRAGMENTS).map(|index| index as u8).collect();
        assert_eq!(received, expected, "fragmented input must stay byte exact");

        let mut client = sender.await.unwrap();
        client.shutdown().await?;
        upstream.shutdown().await?;
        finish(copy).await?;
        Ok(())
    }

    /// EOF must not complete the direction while the pipe still holds bytes.
    #[ignore = "process-wide counters; runs in an isolated process"]
    #[tokio::test]
    async fn eof_waits_for_the_pipe_to_drain_before_completion() -> io::Result<()> {
        test_hooks::reset();
        let (mut client, relay_a) = tcp_pair("127.0.0.1").await?;
        let (relay_b, mut upstream) = tcp_pair("127.0.0.1").await?;
        // The request fits in the pipe, so EOF is read while the pipe holds it.
        let request = pattern(8192, 91);
        client.write_all(&request).await?;
        client.shutdown().await?;
        // Hold every pipe-to-socket splice, so only EOF and the pipe fill run.
        test_hooks::block_out_splice(true);

        let mut relay_a = relay_a;
        let mut relay_b = relay_b;
        let copy = tokio::spawn(async move {
            copy_bidirectional(&mut relay_a, &mut relay_b, false, false).await
        });
        tokio::time::sleep(Duration::from_millis(200)).await;
        assert!(
            !copy.is_finished(),
            "EOF must not complete the direction while the pipe holds bytes"
        );
        assert_eq!(
            test_hooks::splice_bytes(),
            request.len() as u64,
            "only the socket-to-pipe splice may have succeeded"
        );

        test_hooks::block_out_splice(false);
        let mut received = vec![0; request.len()];
        timeout(Duration::from_secs(10), upstream.read_exact(&mut received)).await??;
        assert_eq!(received, request, "the pipe bytes must survive EOF");
        let mut trailing = [0u8; 1];
        assert_eq!(
            timeout(Duration::from_secs(10), upstream.read(&mut trailing)).await??,
            0,
            "the relay must half-close only after the pipe drains"
        );

        upstream.shutdown().await?;
        finish(copy).await?;
        Ok(())
    }

    /// A denied splice after partial pipe progress must fall back without loss.
    ///
    /// The destination stalls first, so the pipe fills and a later partial drain
    /// happens before the denial.
    #[ignore = "process-wide counters; runs in an isolated process"]
    #[tokio::test]
    async fn fallback_after_partial_pipe_progress_keeps_order() -> io::Result<()> {
        test_hooks::reset();
        let (mut client, relay_a) = tcp_pair("127.0.0.1").await?;
        let (relay_b, mut upstream) = tcp_pair("127.0.0.1").await?;
        shrink_socket_buffers(&relay_b, 32 * 1024);
        shrink_socket_buffers(&upstream, 32 * 1024);
        let request = pattern(512 * 1024, 97);
        client.write_all(&request).await?;

        let mut relay_a = relay_a;
        let mut relay_b = relay_b;
        let copy = tokio::spawn(async move {
            copy_bidirectional(&mut relay_a, &mut relay_b, false, false).await
        });
        // Let the relay fill the pipe while this end does not read.
        tokio::time::sleep(Duration::from_millis(200)).await;
        let mut partial = vec![0; 16 * 1024];
        timeout(Duration::from_secs(10), upstream.read_exact(&mut partial)).await??;
        assert!(
            test_hooks::splice_bytes() > 0,
            "the fixture must reach the fast path before the denial"
        );
        // Deny the next splice call, so the fallback starts with pipe bytes left.
        test_hooks::fail_next_splice();
        let calls_at_denial = test_hooks::splice_calls();

        let mut rest = vec![0; request.len() - partial.len()];
        timeout(Duration::from_secs(30), upstream.read_exact(&mut rest)).await??;
        assert_eq!(&partial[..], &request[..partial.len()]);
        assert_eq!(
            &rest[..],
            &request[partial.len()..],
            "the fallback must stay in order"
        );
        assert_eq!(
            test_hooks::splice_calls(),
            calls_at_denial,
            "the denial must stop further splices"
        );

        client.shutdown().await?;
        upstream.shutdown().await?;
        finish(copy).await?;
        Ok(())
    }

    /// Regression for the tiny-buffer fixture itself: both directions must move
    /// their payloads when every socket buffer is far smaller than them.
    #[ignore = "process-wide counters; runs in an isolated process"]
    #[tokio::test]
    async fn tiny_buffer_bidirectional_exchange_stays_byte_exact() -> io::Result<()> {
        test_hooks::reset();
        let (client, relay_a) = tcp_pair("127.0.0.1").await?;
        let (relay_b, upstream) = tcp_pair("127.0.0.1").await?;
        for socket in [&client, &relay_a, &relay_b, &upstream] {
            shrink_socket_buffers(socket, 8 * 1024);
        }
        let mut relay_a = relay_a;
        let mut relay_b = relay_b;
        let copy = tokio::spawn(async move {
            copy_bidirectional(&mut relay_a, &mut relay_b, false, false).await
        });
        let mut ends = Ends::new(client, upstream);
        let request = pattern(128 * 1024, 113);
        let response = pattern(64 * 1024, 127);
        let (received_request, received_response) = ends.exchange(&request, &response).await?;
        assert_eq!(received_request, request);
        assert_eq!(received_response, response);

        ends.close().await?;
        finish(copy).await?;
        Ok(())
    }

    /// Local socket buffers below the guard threshold keep ordinary copying,
    /// without an error and without allocating a pipe.
    #[ignore = "process-wide counters; runs in an isolated process"]
    #[tokio::test]
    async fn small_local_socket_buffers_keep_buffered_copying() -> io::Result<()> {
        test_hooks::reset();
        let (client, relay_a) = tcp_pair("127.0.0.1").await?;
        let (relay_b, upstream) = tcp_pair("127.0.0.1").await?;
        for socket in [&client, &relay_a, &relay_b, &upstream] {
            shrink_socket_buffers(socket, 8 * 1024);
        }
        assert!(
            !crate::linux_splice::local_socket_buffers_admissible(&relay_a, &relay_b),
            "the fixture must sit below the guard: {}",
            socket_settings(&relay_a),
        );

        let descriptors_before = open_descriptor_count();
        let pipes_before = pipe_descriptor_count();
        let allocations_before = test_hooks::pipe_allocations();
        let mut relay_a = relay_a;
        let mut relay_b = relay_b;
        let copy = tokio::spawn(async move {
            copy_bidirectional(&mut relay_a, &mut relay_b, false, false).await
        });
        let mut ends = Ends::new(client, upstream);
        let request = pattern(128 * 1024, 101);
        let response = pattern(64 * 1024, 103);
        // Sample while the transfer is in flight and every socket is still owned,
        // so a transient pipe or descriptor cannot hide. The exchange runs
        // concurrently with the sampler.
        let exchange = ends.exchange(&request, &response);
        let sampler = async {
            tokio::time::sleep(Duration::from_millis(50)).await;
            (pipe_descriptor_count(), open_descriptor_count())
        };
        let (exchange_result, (pipes_in_flight, descriptors_in_flight)) =
            timeout(Duration::from_secs(30), async {
                tokio::join!(exchange, sampler)
            })
            .await
            .map_err(|_| io::Error::new(io::ErrorKind::TimedOut, "the guarded exchange stalled"))?;
        let (received_request, received_response) = exchange_result?;
        assert_eq!(
            pipes_in_flight, pipes_before,
            "a denied guard must not hold a pipe while the transfer runs"
        );
        assert!(
            descriptors_in_flight <= descriptors_before,
            "a denied guard must not add descriptors while the transfer runs"
        );
        assert_eq!(received_request, request);
        assert_eq!(received_response, response);
        assert_eq!(
            test_hooks::splice_calls(),
            0,
            "a denied guard must keep buffered copying"
        );
        assert_eq!(
            test_hooks::pipe_allocations(),
            allocations_before,
            "a denied guard must not allocate a pipe at any point"
        );

        ends.close().await?;
        finish(copy).await?;
        assert_eq!(
            pipe_descriptor_count(),
            pipes_before,
            "a denied guard must not leak a pipe"
        );
        Ok(())
    }

    /// A failed socket buffer query keeps ordinary copying.
    #[ignore = "process-wide counters; runs in an isolated process"]
    #[tokio::test]
    async fn a_failed_socket_buffer_query_keeps_buffered_copying() -> io::Result<()> {
        test_hooks::reset();
        test_hooks::fail_socket_buffer_query(true);
        let (client, upstream, copy) = start_relay("127.0.0.1").await?;
        let mut ends = Ends::new(client, upstream);
        let request = pattern(128 * 1024, 107);
        let response = pattern(64 * 1024, 109);
        let (received_request, received_response) = ends.exchange(&request, &response).await?;
        assert_eq!(received_request, request);
        assert_eq!(received_response, response);
        assert_eq!(
            test_hooks::splice_calls(),
            0,
            "a failed query must keep buffered copying"
        );

        ends.close().await?;
        finish(copy).await?;
        Ok(())
    }

    /// Interrupts the socket-to-pipe splice before it moves any byte. The relay
    /// must retry and stay byte exact.
    #[ignore = "process-wide counters; runs in an isolated process"]
    #[tokio::test]
    async fn an_interrupted_socket_to_pipe_splice_retries_without_loss() -> io::Result<()> {
        test_hooks::reset();
        let (mut client, relay_a) = tcp_pair("127.0.0.1").await?;
        let (relay_b, mut upstream) = tcp_pair("127.0.0.1").await?;
        // The request is already in the socket, so the first splice attempt is
        // the socket-to-pipe direction.
        let request = pattern(64 * 1024, 59);
        client.write_all(&request).await?;
        test_hooks::interrupt_splice_at(1);

        let mut relay_a = relay_a;
        let mut relay_b = relay_b;
        let copy = tokio::spawn(async move {
            copy_bidirectional(&mut relay_a, &mut relay_b, false, false).await
        });

        let mut received_request = vec![0; request.len()];
        timeout(
            Duration::from_secs(10),
            upstream.read_exact(&mut received_request),
        )
        .await??;
        assert_eq!(received_request, request);
        let response = pattern(4096, 61);
        upstream.write_all(&response).await?;
        let mut received_response = vec![0; response.len()];
        timeout(
            Duration::from_secs(10),
            client.read_exact(&mut received_response),
        )
        .await??;
        assert_eq!(received_response, response);
        assert!(
            test_hooks::splice_attempts() >= 2,
            "the interrupted attempt must be retried"
        );
        assert_eq!(
            test_hooks::splice_bytes(),
            (request.len() + response.len()) as u64 * 2,
            "an interruption must not change the byte accounting"
        );

        client.shutdown().await?;
        upstream.shutdown().await?;
        finish(copy).await?;
        Ok(())
    }

    /// Interrupts the pipe-to-socket splice before it moves any byte, right after
    /// the pipe was filled.
    #[ignore = "process-wide counters; runs in an isolated process"]
    #[tokio::test]
    async fn an_interrupted_pipe_to_socket_splice_retries_without_loss() -> io::Result<()> {
        test_hooks::reset();
        let (mut client, relay_a) = tcp_pair("127.0.0.1").await?;
        let (relay_b, mut upstream) = tcp_pair("127.0.0.1").await?;
        let request = pattern(32 * 1024, 63);
        client.write_all(&request).await?;
        test_hooks::interrupt_splice_at(2);

        let mut relay_a = relay_a;
        let mut relay_b = relay_b;
        let copy = tokio::spawn(async move {
            copy_bidirectional(&mut relay_a, &mut relay_b, false, false).await
        });

        let mut received_request = vec![0; request.len()];
        upstream.read_exact(&mut received_request).await?;
        assert_eq!(received_request, request);
        assert!(
            test_hooks::splice_attempts() >= 3,
            "the interrupted pipe-to-socket attempt must be retried"
        );
        assert_eq!(
            test_hooks::splice_bytes(),
            request.len() as u64 * 2,
            "an interruption must not change the byte accounting"
        );

        client.shutdown().await?;
        upstream.shutdown().await?;
        finish(copy).await?;
        Ok(())
    }

    /// Interrupts a splice after the pipe was partially drained, with a
    /// destination that stalls first so the pipe stays occupied.
    #[ignore = "process-wide counters; runs in an isolated process"]
    #[tokio::test]
    async fn an_interrupted_pipe_to_socket_splice_after_partial_progress_keeps_accounting()
    -> io::Result<()> {
        test_hooks::reset();
        let (mut client, relay_a) = tcp_pair("127.0.0.1").await?;
        let (relay_b, mut upstream) = tcp_pair("127.0.0.1").await?;
        shrink_socket_buffers(&relay_b, 32 * 1024);
        shrink_socket_buffers(&upstream, 32 * 1024);
        let request = pattern(512 * 1024, 67);
        client.write_all(&request).await?;

        let mut relay_a = relay_a;
        let mut relay_b = relay_b;
        let copy = tokio::spawn(async move {
            copy_bidirectional(&mut relay_a, &mut relay_b, false, false).await
        });
        // Let the relay fill the pipe while this end does not read.
        tokio::time::sleep(Duration::from_millis(200)).await;
        let mut partial = vec![0; 16 * 1024];
        timeout(Duration::from_secs(10), upstream.read_exact(&mut partial)).await??;
        // The drain that produced those bytes was partial: the pipe still holds
        // data. Interrupt the next attempt.
        let attempts_before = test_hooks::splice_attempts();
        test_hooks::interrupt_next_splice();

        let mut rest = vec![0; request.len() - partial.len()];
        timeout(Duration::from_secs(30), upstream.read_exact(&mut rest)).await??;
        assert_eq!(&partial[..], &request[..partial.len()]);
        assert_eq!(
            &rest[..],
            &request[partial.len()..],
            "the payload must stay byte exact"
        );
        assert!(
            test_hooks::splice_attempts() > attempts_before + 1,
            "the interrupted attempt must be retried"
        );
        assert_eq!(
            test_hooks::splice_bytes(),
            request.len() as u64 * 2,
            "partial progress plus an interruption must not lose or double count bytes"
        );

        client.shutdown().await?;
        upstream.shutdown().await?;
        finish(copy).await?;
        Ok(())
    }

    /// Every splice attempt interrupted must yield to the scheduler instead of
    /// failing the relay, and must resume once the interruptions stop.
    #[ignore = "process-wide counters; runs in an isolated process"]
    #[tokio::test]
    async fn interrupted_splices_yield_instead_of_failing_the_relay() -> io::Result<()> {
        test_hooks::reset();
        let (mut client, relay_a) = tcp_pair("127.0.0.1").await?;
        let (relay_b, mut upstream) = tcp_pair("127.0.0.1").await?;
        let request = pattern(64 * 1024, 71);
        client.write_all(&request).await?;
        test_hooks::interrupt_splice_always(true);

        let mut relay_a = relay_a;
        let mut relay_b = relay_b;
        let copy = tokio::spawn(async move {
            copy_bidirectional(&mut relay_a, &mut relay_b, false, false).await
        });
        tokio::time::sleep(Duration::from_millis(200)).await;
        assert!(
            !copy.is_finished(),
            "an interrupted relay must yield, not fail the connection"
        );
        assert_eq!(
            test_hooks::splice_bytes(),
            0,
            "no byte may move while every splice is interrupted"
        );

        test_hooks::interrupt_splice_always(false);
        let mut received_request = vec![0; request.len()];
        timeout(
            Duration::from_secs(10),
            upstream.read_exact(&mut received_request),
        )
        .await??;
        assert_eq!(received_request, request);

        client.shutdown().await?;
        upstream.shutdown().await?;
        finish(copy).await?;
        Ok(())
    }

    /// Interrupts the fallback pipe read while the pipe already holds the bytes a
    /// denied splice moved into it.
    #[ignore = "process-wide counters; runs in an isolated process"]
    #[tokio::test]
    async fn an_interrupted_fallback_pipe_read_retries_without_loss() -> io::Result<()> {
        test_hooks::reset();
        let (mut client, relay_a) = tcp_pair("127.0.0.1").await?;
        let (relay_b, mut upstream) = tcp_pair("127.0.0.1").await?;
        let request = pattern(8192, 73);
        client.write_all(&request).await?;
        // The first splice fills the pipe; the second one is denied, so the
        // fallback drains an occupied pipe.
        test_hooks::fail_splice_after(1);
        test_hooks::interrupt_pipe_read_at(1);

        let mut relay_a = relay_a;
        let mut relay_b = relay_b;
        let copy = tokio::spawn(async move {
            copy_bidirectional(&mut relay_a, &mut relay_b, false, false).await
        });

        let mut received_request = vec![0; request.len()];
        timeout(
            Duration::from_secs(10),
            upstream.read_exact(&mut received_request),
        )
        .await??;
        assert_eq!(
            received_request, request,
            "the pipe bytes must arrive in order"
        );

        let later = pattern(4096, 77);
        let mut received_later = vec![0; later.len()];
        client.write_all(&later).await?;
        upstream.read_exact(&mut received_later).await?;
        assert_eq!(received_later, later);
        assert_eq!(
            test_hooks::splice_calls(),
            1,
            "only the socket-to-pipe splice may succeed before the denial"
        );

        client.shutdown().await?;
        upstream.shutdown().await?;
        finish(copy).await?;
        Ok(())
    }

    /// Every fallback pipe read interrupted must yield instead of failing, and
    /// must resume once the interruptions stop.
    #[ignore = "process-wide counters; runs in an isolated process"]
    #[tokio::test]
    async fn an_interrupted_fallback_pipe_read_yields_instead_of_failing() -> io::Result<()> {
        test_hooks::reset();
        let (mut client, relay_a) = tcp_pair("127.0.0.1").await?;
        let (relay_b, mut upstream) = tcp_pair("127.0.0.1").await?;
        let request = pattern(8192, 79);
        client.write_all(&request).await?;
        test_hooks::fail_splice_after(1);
        test_hooks::interrupt_pipe_read_always(true);

        let mut relay_a = relay_a;
        let mut relay_b = relay_b;
        let copy = tokio::spawn(async move {
            copy_bidirectional(&mut relay_a, &mut relay_b, false, false).await
        });
        tokio::time::sleep(Duration::from_millis(200)).await;
        assert!(
            !copy.is_finished(),
            "an interrupted drain must yield, not fail the connection"
        );
        assert_eq!(
            test_hooks::splice_bytes(),
            request.len() as u64,
            "the denied splice must have left the request in the pipe"
        );

        test_hooks::interrupt_pipe_read_always(false);
        let mut received_request = vec![0; request.len()];
        timeout(
            Duration::from_secs(10),
            upstream.read_exact(&mut received_request),
        )
        .await??;
        assert_eq!(
            received_request, request,
            "the pipe bytes must survive the yield"
        );

        client.shutdown().await?;
        upstream.shutdown().await?;
        finish(copy).await?;
        Ok(())
    }

    /// Targeted regression: a destination that stalls parks the relay on the
    /// destination write readiness, and the first drain must wake it again.
    ///
    /// A lost wakeup leaves the relay parked, so the drain times out instead of
    /// completing. The counter check proves the parked direction moved bytes
    /// only after the drain.
    #[ignore = "process-wide counters; runs in an isolated process"]
    #[tokio::test]
    async fn a_blocked_destination_resumes_after_it_drains() -> io::Result<()> {
        test_hooks::reset();
        let (mut client, relay_a) = tcp_pair("127.0.0.1").await?;
        let (relay_b, mut destination) = tcp_pair("127.0.0.1").await?;
        // Narrow destination buffers keep the pipe full, so the relay parks.
        // The sizes stay above the guard threshold, and the payload exceeds the
        // buffers, so the fast path is still exercised and still stalls.
        shrink_socket_buffers(&relay_b, 32 * 1024);
        shrink_socket_buffers(&destination, 32 * 1024);
        let payload = pattern(512 * 1024, 51);
        client.write_all(&payload).await?;

        let mut relay_a = relay_a;
        let mut relay_b = relay_b;
        let copy = tokio::spawn(async move {
            copy_bidirectional(&mut relay_a, &mut relay_b, false, false).await
        });

        // Let the relay fill the pipe and park on the blocked destination.
        tokio::time::sleep(Duration::from_millis(200)).await;
        let parked = test_hooks::splice_bytes();
        assert!(parked > 0, "the relay must reach the fast path");

        let mut received = vec![0; payload.len()];
        timeout(
            Duration::from_secs(10),
            destination.read_exact(&mut received),
        )
        .await
        .map_err(|_| {
            io::Error::new(
                io::ErrorKind::TimedOut,
                format!("the parked relay did not resume: {parked} spliced bytes"),
            )
        })??;
        assert_eq!(received, payload, "the payload must stay byte exact");
        assert!(
            test_hooks::splice_bytes() > parked,
            "the parked direction must resume after the drain"
        );

        client.shutdown().await?;
        destination.shutdown().await?;
        finish(copy).await?;
        Ok(())
    }

    #[ignore = "process-wide counters; runs in an isolated process"]
    #[tokio::test]
    async fn a_peer_reset_stays_a_real_io_error() -> io::Result<()> {
        test_hooks::reset();
        let (mut client, relay_a) = tcp_pair("127.0.0.1").await?;
        let (relay_b, upstream) = tcp_pair("127.0.0.1").await?;
        // A zero linger turns the close into a reset, so the fast path sees a
        // real socket error instead of a normal end of file.
        socket2::SockRef::from(&upstream).set_linger(Some(Duration::ZERO))?;
        drop(upstream);
        client.write_all(&pattern(4096, 47)).await?;

        let mut relay_a = relay_a;
        let mut relay_b = relay_b;
        let error = timeout(
            Duration::from_secs(5),
            copy_bidirectional(&mut relay_a, &mut relay_b, false, false),
        )
        .await
        .map_err(|_| io::Error::new(io::ErrorKind::TimedOut, "the relay did not fail"))?
        .unwrap_err();
        assert!(
            matches!(
                error.kind(),
                io::ErrorKind::BrokenPipe | io::ErrorKind::ConnectionReset
            ),
            "a peer reset must stay a real I/O error, got {error}"
        );
        Ok(())
    }

    #[ignore = "process-wide counters; runs in an isolated process"]
    #[tokio::test]
    async fn progress_aware_copy_keeps_buffered_copying() -> io::Result<()> {
        test_hooks::reset();
        let (client, relay_a) = tcp_pair("127.0.0.1").await?;
        let (relay_b, upstream) = tcp_pair("127.0.0.1").await?;
        let mut relay_a = relay_a;
        let mut relay_b = relay_b;
        let copy = tokio::spawn(async move {
            copy_bidirectional_with_progress_aware_shutdown(
                &mut relay_a,
                &mut relay_b,
                false,
                false,
            )
            .await
        });

        let mut ends = Ends::new(client, upstream);
        let request = pattern(64 * 1024, 41);
        let (received_request, received_response) = ends.exchange(&request, &request).await?;
        assert_eq!(received_request, request);
        assert_eq!(received_response, request);
        assert_eq!(
            test_hooks::splice_calls(),
            0,
            "the progress-aware REALITY fallback keeps its buffered accounting"
        );

        ends.close().await?;
        finish(copy).await?;
        Ok(())
    }
}
