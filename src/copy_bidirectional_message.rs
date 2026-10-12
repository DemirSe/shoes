use futures::ready;
use tokio::io::ReadBuf;

use std::future::Future;
use std::io;
use std::pin::Pin;
use std::task::{Context, Poll};
use std::time::Duration;
use tokio::time::Instant;

use crate::async_stream::AsyncMessageStream;
use crate::util::allocate_vec;

// Informed by https://stackoverflow.com/questions/14856639/udp-hole-punching-timeout
pub const DEFAULT_ASSOCIATION_TIMEOUT_SECS: u32 = 200;

/// Absolute window for the other direction after the first clean end-of-stream.
///
/// The window starts at that first end and is not extended by traffic. Messages
/// the other direction delivers during it are still copied; a message that
/// arrives after it is discarded, exactly as it was before this window existed.
/// The ordinary idle association timeout stays unchanged for associations whose
/// directions are both open.
const EOF_DRAIN_WINDOW: Duration = Duration::from_secs(2);

struct CopyBuffer {
    read_done: bool,
    /// A zero-length message that still has to reach the writer. A message
    /// stream that does not report end-of-stream on an empty read can carry
    /// empty messages, so message existence is not a byte count.
    pending_empty_message: bool,
    need_flush: bool,
    need_write_ping: bool,
    cache_length: usize,
    buf: Box<[u8]>,
    read_count: usize,
    /// Messages accepted in this direction. A zero-length message is activity
    /// even though it adds no bytes, while ping frames are writes and are not
    /// counted here.
    read_messages: usize,
}

impl CopyBuffer {
    /// True when no message of this direction is waiting to be written.
    fn is_idle(&self) -> bool {
        self.cache_length == 0 && !self.pending_empty_message
    }

    /// True when this direction still owns accepted application data.
    ///
    /// A writer can accept a message and then block while flushing it, so the
    /// pending flush counts as undrained output as well.
    fn has_undrained_output(&self) -> bool {
        !self.is_idle() || self.need_flush
    }

    /// Give the read buffer back. A copy that waits for the other direction does
    /// not need it, and a released buffer is allocated again for the next read
    /// attempt. A cached message keeps its bytes, so the buffer is only released
    /// while nothing of this direction is pending.
    fn release_read_buffer(&mut self) {
        if self.cache_length == 0 && !self.pending_empty_message {
            self.buf = Box::new([]);
        }
    }

    pub fn new(need_flush: bool) -> Self {
        Self {
            read_done: false,
            pending_empty_message: false,
            need_flush,
            need_write_ping: false,
            cache_length: 0,
            buf: Box::new([]),
            read_count: 0,
            read_messages: 0,
        }
    }

    pub fn poll_copy<R, W>(
        &mut self,
        cx: &mut Context<'_>,
        mut reader: Pin<&mut R>,
        mut writer: Pin<&mut W>,
    ) -> Poll<io::Result<()>>
    where
        R: AsyncMessageStream + ?Sized,
        W: AsyncMessageStream + ?Sized,
    {
        // Check tokio's cooperative budget to prevent task starvation
        let coop = ready!(tokio::task::coop::poll_proceed(cx));

        loop {
            let mut did_read = false;
            let mut did_write = false;
            let mut read_pending = false;
            let mut write_pending = false;

            if !self.read_done && self.cache_length == 0 && !self.pending_empty_message {
                let me = &mut *self;
                if me.buf.is_empty() {
                    me.buf = allocate_vec(65535).into_boxed_slice();
                }
                let mut buf = ReadBuf::new(&mut me.buf);
                match reader.as_mut().poll_read_message(cx, &mut buf) {
                    Poll::Ready(Err(error))
                        if error.kind() == io::ErrorKind::UnexpectedEof
                            && reader.read_error_is_end_of_stream() =>
                    {
                        // The stream carries empty messages, so it reports its end
                        // as an error. That end is clean.
                        self.read_done = true;
                    }
                    Poll::Ready(val) => {
                        val?;
                        let n = buf.filled().len();
                        if n == 0 {
                            if reader.read_message_eof_on_empty() {
                                self.read_done = true;
                            } else {
                                // The stream reports empty messages instead of
                                // end-of-stream, so this read is a message that
                                // has to be written even though it carries no
                                // bytes.
                                self.pending_empty_message = true;
                                self.read_messages = self.read_messages.wrapping_add(1);
                                did_read = true;
                                coop.made_progress();
                            }
                        } else {
                            self.cache_length = n;
                            did_read = true;
                            self.read_count = self.read_count.wrapping_add(n);
                            self.read_messages = self.read_messages.wrapping_add(1);
                            coop.made_progress();
                        }
                    }
                    Poll::Pending => {
                        read_pending = true;
                        self.release_read_buffer();
                    }
                }
            }

            if self.cache_length > 0 || self.pending_empty_message {
                let me = &mut *self;
                let message: &[u8] = if me.pending_empty_message {
                    &[]
                } else {
                    &me.buf[0..me.cache_length]
                };
                match writer.as_mut().poll_write_message(cx, message) {
                    Poll::Ready(val) => {
                        val?;
                        self.cache_length = 0;
                        self.pending_empty_message = false;
                        self.need_flush = true;
                        // Don't bother writing ping, since we just wrote.
                        self.need_write_ping = false;
                        did_write = true;
                        coop.made_progress();
                    }
                    Poll::Pending => {
                        write_pending = true;
                    }
                }
            }

            if !write_pending && self.need_write_ping {
                match writer.as_mut().poll_write_ping(cx) {
                    Poll::Ready(val) => {
                        let written = val?;
                        self.need_write_ping = false;
                        if written {
                            self.need_flush = true;
                            coop.made_progress();
                        }
                    }
                    Poll::Pending => {
                        write_pending = true;
                    }
                }
            }

            if did_read && did_write && !read_pending && !write_pending {
                continue;
            }

            if self.need_flush {
                ready!(writer.as_mut().poll_flush_message(cx))?;
                self.need_flush = false;
                coop.made_progress();
                continue;
            }

            // If we've written all the data and we've seen EOF, finish the transfer.
            if self.read_done && self.cache_length == 0 && !self.pending_empty_message {
                return Poll::Ready(Ok(()));
            }

            // Return Pending to prevent task starvation
            if read_pending || write_pending {
                return Poll::Pending;
            }
        }
    }
}

enum TransferState {
    Running,
    ShuttingDown,
    Done,
}

struct CopyBidirectional<'a, A: ?Sized, B: ?Sized> {
    a: &'a mut A,
    b: &'a mut B,
    a_buf: CopyBuffer,
    b_buf: CopyBuffer,
    a_to_b: TransferState,
    b_to_a: TransferState,
    sleep_future: Pin<Box<tokio::time::Sleep>>,
    /// Bounds how long the remaining direction may stay quiet after the first
    /// direction reported end-of-stream.
    drain_future: Pin<Box<tokio::time::Sleep>>,
    drain_started: bool,
    last_active: Instant,
}

/// Outcome of a closed drain window.
///
/// Accepted output that is still undrained must not be reported as a clean end.
fn drain_window_outcome(
    a_buf: &CopyBuffer,
    b_buf: &CopyBuffer,
    a_to_b: &TransferState,
    b_to_a: &TransferState,
) -> Poll<io::Result<()>> {
    let undrained = (a_buf.read_done && !matches!(a_to_b, TransferState::Done))
        || (b_buf.read_done && !matches!(b_to_a, TransferState::Done))
        || a_buf.has_undrained_output()
        || b_buf.has_undrained_output();
    if undrained {
        return Poll::Ready(Err(io::Error::new(
            io::ErrorKind::TimedOut,
            "end-of-stream drain window closed with accepted output undrained",
        )));
    }
    Poll::Ready(Ok(()))
}

fn transfer_one_direction<A, B>(
    cx: &mut Context<'_>,
    state: &mut TransferState,
    buf: &mut CopyBuffer,
    r: &mut A,
    w: &mut B,
) -> Poll<io::Result<()>>
where
    A: AsyncMessageStream + ?Sized,
    B: AsyncMessageStream + ?Sized,
{
    let mut r = Pin::new(r);
    let mut w = Pin::new(w);

    loop {
        match state {
            TransferState::Running => {
                ready!(buf.poll_copy(cx, r.as_mut(), w.as_mut()))?;
                *state = TransferState::ShuttingDown;
            }
            TransferState::ShuttingDown => {
                ready!(w.as_mut().poll_shutdown_message(cx))?;
                *state = TransferState::Done;
            }
            TransferState::Done => {
                // The direction is finished, so its read buffer is not needed
                // while the other direction drains.
                buf.release_read_buffer();
                return Poll::Ready(Ok(()));
            }
        }
    }
}

impl<A, B> Future for CopyBidirectional<'_, A, B>
where
    A: AsyncMessageStream + ?Sized,
    B: AsyncMessageStream + ?Sized,
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
            drain_future,
            drain_started,
            last_active,
        } = &mut *self;

        let ping_fired = sleep_future.as_mut().poll(cx).is_ready();
        if ping_fired {
            // a_buf writes to b - so we need to check if b supports ping, and similarly
            // for b_buf.
            a_buf.need_write_ping = b.supports_ping();
            b_buf.need_write_ping = a.supports_ping();
            sleep_future
                .as_mut()
                .reset(tokio::time::Instant::now() + std::time::Duration::from_secs(60));
        }

        // The idle check shares the timer clock with the sleep above, so a
        // paused test clock can exercise the association timeout. Message counts
        // keep a stream of empty datagrams from looking like inactivity.
        let a_activity = (a_buf.read_count, a_buf.read_messages);
        let b_activity = (b_buf.read_count, b_buf.read_messages);

        // The absolute window starts at the first clean read end-of-stream. Once
        // it has closed, no further message is accepted, and accepted output that
        // is still undrained is reported instead of a clean end.
        if (a_buf.read_done || b_buf.read_done) && !*drain_started {
            *drain_started = true;
            drain_future
                .as_mut()
                .reset(tokio::time::Instant::now() + EOF_DRAIN_WINDOW);
        }
        if *drain_started && drain_future.as_mut().poll(cx).is_ready() {
            return drain_window_outcome(a_buf, b_buf, a_to_b, b_to_a);
        }

        // An error ends the copy at once: it must not be hidden by the other
        // direction finishing at the same time.
        let a_ready = match transfer_one_direction(cx, a_to_b, &mut *a_buf, &mut *a, &mut *b) {
            Poll::Ready(Err(error)) => return Poll::Ready(Err(error)),
            Poll::Ready(Ok(())) => true,
            Poll::Pending => false,
        };
        let b_ready = match transfer_one_direction(cx, b_to_a, &mut *b_buf, &mut *b, &mut *a) {
            Poll::Ready(Err(error)) => return Poll::Ready(Err(error)),
            Poll::Ready(Ok(())) => true,
            Poll::Pending => false,
        };

        let progressed = (a_buf.read_count, a_buf.read_messages) != a_activity
            || (b_buf.read_count, b_buf.read_messages) != b_activity;
        if progressed {
            *last_active = Instant::now();
        } else if last_active.elapsed().as_secs() >= DEFAULT_ASSOCIATION_TIMEOUT_SECS.into() {
            // A timed-out association must not report a clean end while output
            // that was already accepted is still undrained, exactly as when the
            // drain window closes.
            if a_buf.has_undrained_output() || b_buf.has_undrained_output() {
                return Poll::Ready(Err(io::Error::new(
                    io::ErrorKind::TimedOut,
                    "association idle timeout reached with accepted output undrained",
                )));
            }
            return Poll::Ready(Ok(()));
        }

        // A read end-of-stream observed in this poll starts the absolute window,
        // even while that direction is still flushing or shutting down.
        if (a_buf.read_done || b_buf.read_done) && !*drain_started {
            *drain_started = true;
            drain_future
                .as_mut()
                .reset(tokio::time::Instant::now() + EOF_DRAIN_WINDOW);
        }

        // Both directions have to finish before the copy ends cleanly.
        if a_ready && b_ready {
            return Poll::Ready(Ok(()));
        }

        // Keep the window's timer registered while the drain is open.
        if *drain_started && drain_future.as_mut().poll(cx).is_ready() {
            return drain_window_outcome(a_buf, b_buf, a_to_b, b_to_a);
        }

        Poll::Pending
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
/// at which point [`shutdown()`] is called on the corresponding writer. When finished,
/// it will return a tuple of the number of bytes copied from a to b
/// and the number of bytes copied from b to a, in that order.
/// After the first direction ends, the remaining one may still deliver messages
/// for [`EOF_DRAIN_WINDOW`]. Messages that arrive after that absolute window are
/// discarded. An association whose directions are both open still ends after
/// [`DEFAULT_ASSOCIATION_TIMEOUT_SECS`] without activity; if accepted output is
/// still undrained then, the end is reported as a timeout instead of a clean
/// end.
///
/// [`shutdown()`]: crate::io::AsyncWriteExt::shutdown
///
/// # Errors
///
/// The future will immediately return an error if any IO operation on `a`
/// or `b` returns an error. Some data read from either stream may be lost (not
/// written to the other stream) in this case.
///
/// # Return value
///
/// Returns a tuple of bytes copied `a` to `b` and bytes copied `b` to `a`.
pub async fn copy_bidirectional_message<A, B>(
    a: &mut A,
    b: &mut B,
    a_initial_flush: bool,
    b_initial_flush: bool,
) -> Result<(), std::io::Error>
where
    A: AsyncMessageStream + ?Sized,
    B: AsyncMessageStream + ?Sized,
{
    // Unlike tcp copy_bidirectional, we always run a sleep future so that we can expire
    // connections.
    let sleep_future = Box::pin(tokio::time::sleep(std::time::Duration::from_secs(60)));
    let drain_future = Box::pin(tokio::time::sleep(EOF_DRAIN_WINDOW));

    CopyBidirectional {
        a,
        b,
        // this is correctly reversed - CopyBuffer will copy from a (reader) to b (writer) using
        // a_buf, which means that the need_flush signal is for the writer (b), and vice versa for
        // b_buf.
        a_buf: CopyBuffer::new(b_initial_flush),
        b_buf: CopyBuffer::new(a_initial_flush),
        a_to_b: TransferState::Running,
        b_to_a: TransferState::Running,
        sleep_future,
        drain_future,
        drain_started: false,
        last_active: Instant::now(),
    }
    .await
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::async_stream::{
        AsyncFlushMessage, AsyncMessageStream, AsyncPing, AsyncReadMessage, AsyncShutdownMessage,
        AsyncWriteMessage,
    };
    use futures::task::noop_waker;
    use std::collections::VecDeque;
    use std::sync::{Arc, Mutex};
    use std::time::Duration;

    /// One scripted read outcome for [`ScriptedMessageStream`].
    enum ReadStep {
        /// A message with these bytes.
        Message(Vec<u8>),
        /// A zero-length read. The stream contract decides whether it means an
        /// empty message or end of stream.
        Empty,
        /// No message is ready yet; the step stays until it is polled again.
        Idle,
        /// No message is ready on the first poll, then this message arrives.
        Deferred(Vec<u8>),
        /// The stream fails, which is how a stream that never reports
        /// end-of-stream on an empty read signals its end.
        Error(io::ErrorKind),
    }

    #[derive(Default)]
    struct StreamState {
        reads: VecDeque<ReadStep>,
        eof_on_empty: bool,
        error_is_end_of_stream: bool,
        writes: Vec<Vec<u8>>,
        blocked_writes: usize,
        blocked_flushes: usize,
        blocked_shutdowns: usize,
        shutdowns: usize,
    }

    /// Message stream with a scripted read sequence and a recorded write sequence.
    struct ScriptedMessageStream {
        state: Arc<Mutex<StreamState>>,
    }

    impl ScriptedMessageStream {
        fn new(eof_on_empty: bool, reads: Vec<ReadStep>) -> Self {
            let state = StreamState {
                reads: reads.into(),
                eof_on_empty,
                ..Default::default()
            };
            Self {
                state: Arc::new(Mutex::new(state)),
            }
        }

        fn writes(&self) -> Vec<Vec<u8>> {
            self.state.lock().unwrap().writes.clone()
        }
    }

    impl AsyncReadMessage for ScriptedMessageStream {
        fn read_message_eof_on_empty(&self) -> bool {
            self.state.lock().unwrap().eof_on_empty
        }

        fn read_error_is_end_of_stream(&self) -> bool {
            self.state.lock().unwrap().error_is_end_of_stream
        }

        fn poll_read_message(
            self: Pin<&mut Self>,
            _cx: &mut Context<'_>,
            buf: &mut ReadBuf<'_>,
        ) -> Poll<io::Result<()>> {
            let this = self.get_mut();
            let mut state = this.state.lock().unwrap();
            match state.reads.front_mut() {
                None => Poll::Ready(Ok(())),
                Some(ReadStep::Message(data)) => {
                    buf.put_slice(data);
                    state.reads.pop_front();
                    Poll::Ready(Ok(()))
                }
                Some(ReadStep::Empty) => {
                    state.reads.pop_front();
                    Poll::Ready(Ok(()))
                }
                Some(ReadStep::Idle) => Poll::Pending,
                Some(ReadStep::Deferred(data)) => {
                    let data = std::mem::take(data);
                    state.reads.pop_front();
                    state.reads.push_front(ReadStep::Message(data));
                    Poll::Pending
                }
                Some(ReadStep::Error(kind)) => {
                    let kind = *kind;
                    state.reads.pop_front();
                    Poll::Ready(Err(kind.into()))
                }
            }
        }
    }

    impl AsyncWriteMessage for ScriptedMessageStream {
        fn poll_write_message(
            self: Pin<&mut Self>,
            _cx: &mut Context<'_>,
            buf: &[u8],
        ) -> Poll<io::Result<()>> {
            let mut state = self.state.lock().unwrap();
            if state.blocked_writes > 0 {
                state.blocked_writes -= 1;
                return Poll::Pending;
            }
            state.writes.push(buf.to_vec());
            Poll::Ready(Ok(()))
        }
    }

    impl AsyncFlushMessage for ScriptedMessageStream {
        fn poll_flush_message(self: Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<io::Result<()>> {
            let mut state = self.state.lock().unwrap();
            if state.blocked_flushes > 0 {
                state.blocked_flushes -= 1;
                return Poll::Pending;
            }
            Poll::Ready(Ok(()))
        }
    }

    impl AsyncShutdownMessage for ScriptedMessageStream {
        fn poll_shutdown_message(
            self: Pin<&mut Self>,
            _cx: &mut Context<'_>,
        ) -> Poll<io::Result<()>> {
            let mut state = self.state.lock().unwrap();
            state.shutdowns += 1;
            if state.blocked_shutdowns > 0 {
                state.blocked_shutdowns -= 1;
                return Poll::Pending;
            }
            Poll::Ready(Ok(()))
        }
    }

    impl AsyncPing for ScriptedMessageStream {
        fn supports_ping(&self) -> bool {
            false
        }

        fn poll_write_ping(self: Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<io::Result<bool>> {
            Poll::Ready(Ok(false))
        }
    }

    impl AsyncMessageStream for ScriptedMessageStream {}

    /// Poll one direction until it finishes, the way the message copier does.
    fn copy_one_direction(
        reader: &mut ScriptedMessageStream,
        writer: &mut ScriptedMessageStream,
    ) -> io::Result<()> {
        let mut copy = CopyBuffer::new(false);
        let waker = noop_waker();
        let mut cx = Context::from_waker(&waker);
        for _ in 0..16 {
            match copy.poll_copy(&mut cx, Pin::new(&mut *reader), Pin::new(&mut *writer)) {
                Poll::Ready(result) => return result,
                Poll::Pending => {}
            }
        }
        panic!("one direction did not finish");
    }

    #[test]
    fn an_empty_message_is_written_instead_of_ending_the_copy() {
        let mut reader = ScriptedMessageStream::new(
            false,
            vec![
                ReadStep::Empty,
                ReadStep::Message(b"second".to_vec()),
                ReadStep::Error(io::ErrorKind::UnexpectedEof),
            ],
        );
        let mut writer = ScriptedMessageStream::new(true, Vec::new());

        let result = copy_one_direction(&mut reader, &mut writer);

        assert_eq!(
            writer.writes(),
            vec![Vec::new(), b"second".to_vec()],
            "every accepted message, including a zero-length one, must reach the writer"
        );
        assert_eq!(result.unwrap_err().kind(), io::ErrorKind::UnexpectedEof);
    }

    #[test]
    fn an_end_reported_as_an_error_finishes_the_direction_cleanly() {
        let mut reader = ScriptedMessageStream::new(
            false,
            vec![
                ReadStep::Message(b"only".to_vec()),
                ReadStep::Error(io::ErrorKind::UnexpectedEof),
            ],
        );
        reader.state.lock().unwrap().error_is_end_of_stream = true;
        let mut writer = ScriptedMessageStream::new(true, Vec::new());

        copy_one_direction(&mut reader, &mut writer).unwrap();

        assert_eq!(writer.writes(), vec![b"only".to_vec()]);
    }

    #[test]
    fn an_unexpected_end_that_is_not_reported_stays_an_error() {
        let mut reader =
            ScriptedMessageStream::new(false, vec![ReadStep::Error(io::ErrorKind::UnexpectedEof)]);
        let mut writer = ScriptedMessageStream::new(true, Vec::new());

        let error = copy_one_direction(&mut reader, &mut writer).unwrap_err();

        assert_eq!(error.kind(), io::ErrorKind::UnexpectedEof);
    }

    #[test]
    fn an_empty_read_still_ends_a_stream_that_reports_eof_on_empty() {
        let mut reader = ScriptedMessageStream::new(
            true,
            vec![ReadStep::Message(b"only".to_vec()), ReadStep::Empty],
        );
        let mut writer = ScriptedMessageStream::new(true, Vec::new());

        copy_one_direction(&mut reader, &mut writer).unwrap();

        assert_eq!(writer.writes(), vec![b"only".to_vec()]);
        assert_eq!(writer.state.lock().unwrap().shutdowns, 0);
    }

    #[test]
    fn an_empty_message_keeps_its_order_under_write_backpressure() {
        let mut reader = ScriptedMessageStream::new(
            false,
            vec![
                ReadStep::Empty,
                ReadStep::Message(b"later".to_vec()),
                ReadStep::Error(io::ErrorKind::UnexpectedEof),
            ],
        );
        let mut writer = ScriptedMessageStream::new(true, Vec::new());
        writer.state.lock().unwrap().blocked_writes = 1;
        let mut copy = CopyBuffer::new(false);
        let waker = noop_waker();
        let mut cx = Context::from_waker(&waker);

        assert!(
            copy.poll_copy(&mut cx, Pin::new(&mut reader), Pin::new(&mut writer))
                .is_pending()
        );
        assert!(writer.writes().is_empty());
        assert_eq!(
            reader.state.lock().unwrap().reads.len(),
            2,
            "the next message must wait until the empty message is written"
        );

        for _ in 0..4 {
            if copy
                .poll_copy(&mut cx, Pin::new(&mut reader), Pin::new(&mut writer))
                .is_ready()
            {
                break;
            }
        }
        assert_eq!(
            writer.writes(),
            vec![Vec::new(), b"later".to_vec()],
            "the empty message must keep its place in the message order"
        );
    }

    #[tokio::test]
    async fn eof_waits_for_the_last_reply_of_the_other_direction() {
        // The client sends one request, is done writing, and the reply to that
        // request is not ready yet.
        let mut client = ScriptedMessageStream::new(
            true,
            vec![ReadStep::Message(b"request".to_vec()), ReadStep::Empty],
        );
        let mut peer = ScriptedMessageStream::new(
            true,
            vec![ReadStep::Deferred(b"last reply".to_vec()), ReadStep::Empty],
        );
        let client_writes = Arc::clone(&client.state);

        let waker = noop_waker();
        let mut cx = Context::from_waker(&waker);
        let mut copy = Box::pin(copy_bidirectional_message(
            &mut client,
            &mut peer,
            false,
            false,
        ));

        assert!(
            copy.as_mut().poll(&mut cx).is_pending(),
            "the copy must not finish while the other direction still owes a reply"
        );
        assert!(client_writes.lock().unwrap().writes.is_empty());

        let mut polls = 0;
        while copy.as_mut().poll(&mut cx).is_pending() {
            polls += 1;
            assert!(polls < 16, "the copy did not finish");
        }

        assert_eq!(
            client_writes.lock().unwrap().writes,
            vec![b"last reply".to_vec()],
            "the last reply of the other direction must be delivered"
        );
    }

    #[tokio::test(start_paused = true)]
    async fn an_association_is_released_when_the_drain_window_closes() {
        // The reply was already delivered, but the absolute window still runs
        // from the first clean end-of-stream.
        let mut client = ScriptedMessageStream::new(
            true,
            vec![ReadStep::Message(b"request".to_vec()), ReadStep::Empty],
        );
        let mut peer = ScriptedMessageStream::new(
            true,
            vec![ReadStep::Message(b"reply".to_vec()), ReadStep::Idle],
        );
        let mut copy = Box::pin(copy_bidirectional_message(
            &mut client,
            &mut peer,
            false,
            false,
        ));
        let waker = noop_waker();
        let mut cx = Context::from_waker(&waker);

        assert!(copy.as_mut().poll(&mut cx).is_pending());
        tokio::time::advance(EOF_DRAIN_WINDOW + Duration::from_secs(1)).await;
        assert!(
            matches!(copy.as_mut().poll(&mut cx), Poll::Ready(Ok(()))),
            "the window must release the association"
        );
    }

    #[tokio::test(start_paused = true)]
    async fn a_blocked_reply_flush_at_the_deadline_reports_timed_out() {
        // The client is done writing. The peer's reply is accepted by the client
        // stream, but its flush never completes, so the deadline must not report
        // a clean end.
        let mut client = ScriptedMessageStream::new(true, vec![ReadStep::Empty]);
        let mut peer = ScriptedMessageStream::new(
            true,
            vec![ReadStep::Message(b"reply".to_vec()), ReadStep::Idle],
        );
        let client_state = Arc::clone(&client.state);
        client_state.lock().unwrap().blocked_flushes = usize::MAX;
        let mut copy = Box::pin(copy_bidirectional_message(
            &mut client,
            &mut peer,
            false,
            false,
        ));
        let waker = noop_waker();
        let mut cx = Context::from_waker(&waker);

        assert!(copy.as_mut().poll(&mut cx).is_pending());
        assert_eq!(
            client_state.lock().unwrap().writes,
            vec![b"reply".to_vec()],
            "the writer accepted the reply"
        );

        tokio::time::advance(EOF_DRAIN_WINDOW + Duration::from_secs(1)).await;
        let error = match copy.as_mut().poll(&mut cx) {
            Poll::Ready(Err(error)) => error,
            other => panic!("a blocked reply flush must not be reported as delivered: {other:?}"),
        };
        assert_eq!(error.kind(), io::ErrorKind::TimedOut);
    }

    #[tokio::test(start_paused = true)]
    async fn a_blocked_empty_reply_flush_at_the_deadline_reports_timed_out() {
        let mut client = ScriptedMessageStream::new(true, vec![ReadStep::Empty]);
        let mut peer = ScriptedMessageStream::new(false, vec![ReadStep::Empty, ReadStep::Idle]);
        let client_state = Arc::clone(&client.state);
        client_state.lock().unwrap().blocked_flushes = usize::MAX;
        let mut copy = Box::pin(copy_bidirectional_message(
            &mut client,
            &mut peer,
            false,
            false,
        ));
        let waker = noop_waker();
        let mut cx = Context::from_waker(&waker);

        assert!(copy.as_mut().poll(&mut cx).is_pending());
        assert_eq!(
            client_state.lock().unwrap().writes,
            vec![Vec::new()],
            "the writer accepted the empty reply"
        );

        tokio::time::advance(EOF_DRAIN_WINDOW + Duration::from_secs(1)).await;
        let error = match copy.as_mut().poll(&mut cx) {
            Poll::Ready(Err(error)) => error,
            other => {
                panic!("a blocked empty-reply flush must not be reported as delivered: {other:?}")
            }
        };
        assert_eq!(error.kind(), io::ErrorKind::TimedOut);
    }

    #[tokio::test(start_paused = true)]
    async fn a_blocked_forward_flush_at_the_deadline_reports_timed_out() {
        // The peer ends first and the client's message reaches the peer writer,
        // whose flush never completes.
        let mut client = ScriptedMessageStream::new(
            true,
            vec![
                ReadStep::Message(b"client message".to_vec()),
                ReadStep::Idle,
            ],
        );
        let mut peer = ScriptedMessageStream::new(true, vec![ReadStep::Empty]);
        let peer_state = Arc::clone(&peer.state);
        peer_state.lock().unwrap().blocked_flushes = usize::MAX;
        let mut copy = Box::pin(copy_bidirectional_message(
            &mut client,
            &mut peer,
            false,
            false,
        ));
        let waker = noop_waker();
        let mut cx = Context::from_waker(&waker);

        assert!(copy.as_mut().poll(&mut cx).is_pending());
        assert_eq!(
            peer_state.lock().unwrap().writes,
            vec![b"client message".to_vec()],
            "the peer writer accepted the message"
        );

        tokio::time::advance(EOF_DRAIN_WINDOW + Duration::from_secs(1)).await;
        let error = match copy.as_mut().poll(&mut cx) {
            Poll::Ready(Err(error)) => error,
            other => panic!("a blocked forward flush must not be reported as delivered: {other:?}"),
        };
        assert_eq!(error.kind(), io::ErrorKind::TimedOut);
    }

    #[tokio::test(start_paused = true)]
    async fn a_reply_flush_that_completes_before_the_deadline_ends_cleanly() {
        let mut client = ScriptedMessageStream::new(true, vec![ReadStep::Empty]);
        let mut peer = ScriptedMessageStream::new(
            true,
            vec![ReadStep::Message(b"reply".to_vec()), ReadStep::Idle],
        );
        let client_state = Arc::clone(&client.state);
        // The first flush attempt blocks, a later attempt completes inside the
        // window.
        client_state.lock().unwrap().blocked_flushes = 1;
        let mut copy = Box::pin(copy_bidirectional_message(
            &mut client,
            &mut peer,
            false,
            false,
        ));
        let waker = noop_waker();
        let mut cx = Context::from_waker(&waker);

        assert!(copy.as_mut().poll(&mut cx).is_pending());
        assert!(copy.as_mut().poll(&mut cx).is_pending());
        assert_eq!(
            client_state.lock().unwrap().writes,
            vec![b"reply".to_vec()],
            "the reply was delivered"
        );

        tokio::time::advance(EOF_DRAIN_WINDOW + Duration::from_secs(1)).await;
        assert!(
            matches!(copy.as_mut().poll(&mut cx), Poll::Ready(Ok(()))),
            "a drained reply lets the window close cleanly"
        );
    }

    #[tokio::test(start_paused = true)]
    async fn a_message_after_the_drain_window_is_discarded() {
        let mut client = ScriptedMessageStream::new(true, vec![ReadStep::Empty]);
        let mut peer = ScriptedMessageStream::new(true, vec![ReadStep::Idle]);
        let peer_state = Arc::clone(&peer.state);
        let client_state = Arc::clone(&client.state);
        let mut copy = Box::pin(copy_bidirectional_message(
            &mut client,
            &mut peer,
            false,
            false,
        ));
        let waker = noop_waker();
        let mut cx = Context::from_waker(&waker);

        assert!(copy.as_mut().poll(&mut cx).is_pending());

        // A message that is still unread when the window closes is discarded,
        // exactly as it was before the window existed.
        peer_state
            .lock()
            .unwrap()
            .reads
            .push_front(ReadStep::Message(b"too late".to_vec()));
        tokio::time::advance(EOF_DRAIN_WINDOW + Duration::from_secs(1)).await;
        assert!(matches!(copy.as_mut().poll(&mut cx), Poll::Ready(Ok(()))));
        assert!(
            client_state.lock().unwrap().writes.is_empty(),
            "a message after the window must not be delivered"
        );
    }

    #[tokio::test(start_paused = true)]
    async fn several_replies_to_one_request_arrive_inside_the_window() {
        let mut client = ScriptedMessageStream::new(
            true,
            vec![ReadStep::Message(b"request".to_vec()), ReadStep::Empty],
        );
        let mut peer = ScriptedMessageStream::new(
            true,
            vec![
                ReadStep::Deferred(b"first".to_vec()),
                ReadStep::Deferred(b"second".to_vec()),
                ReadStep::Idle,
            ],
        );
        let client_state = Arc::clone(&client.state);
        let mut copy = Box::pin(copy_bidirectional_message(
            &mut client,
            &mut peer,
            false,
            false,
        ));
        let waker = noop_waker();
        let mut cx = Context::from_waker(&waker);

        // Two polls deliver the two deferred replies, then the window closes.
        assert!(copy.as_mut().poll(&mut cx).is_pending());
        assert!(copy.as_mut().poll(&mut cx).is_pending());
        assert_eq!(
            client_state.lock().unwrap().writes,
            vec![b"first".to_vec(), b"second".to_vec()],
            "every reply inside the window must be delivered in order"
        );

        tokio::time::advance(EOF_DRAIN_WINDOW + Duration::from_secs(1)).await;
        assert!(matches!(copy.as_mut().poll(&mut cx), Poll::Ready(Ok(()))));
    }

    #[tokio::test(start_paused = true)]
    async fn unsolicited_traffic_after_the_first_eof_arrives_inside_the_window() {
        // The peer sends without a request, and the client already ended.
        let mut client = ScriptedMessageStream::new(true, vec![ReadStep::Empty]);
        let mut peer = ScriptedMessageStream::new(
            false,
            vec![ReadStep::Deferred(b"unsolicited".to_vec()), ReadStep::Idle],
        );
        let client_state = Arc::clone(&client.state);
        let mut copy = Box::pin(copy_bidirectional_message(
            &mut client,
            &mut peer,
            false,
            false,
        ));
        let waker = noop_waker();
        let mut cx = Context::from_waker(&waker);

        assert!(copy.as_mut().poll(&mut cx).is_pending());
        assert!(copy.as_mut().poll(&mut cx).is_pending());
        assert_eq!(
            client_state.lock().unwrap().writes,
            vec![b"unsolicited".to_vec()],
            "unsolicited traffic inside the window must be delivered"
        );
    }

    #[tokio::test(start_paused = true)]
    async fn a_client_message_after_the_peer_ends_arrives_inside_the_window() {
        // The peer ends first; the client may still deliver its message.
        let mut client = ScriptedMessageStream::new(
            true,
            vec![
                ReadStep::Deferred(b"late client message".to_vec()),
                ReadStep::Idle,
            ],
        );
        let mut peer = ScriptedMessageStream::new(true, vec![ReadStep::Empty]);
        let peer_state = Arc::clone(&peer.state);
        let mut copy = Box::pin(copy_bidirectional_message(
            &mut client,
            &mut peer,
            false,
            false,
        ));
        let waker = noop_waker();
        let mut cx = Context::from_waker(&waker);

        assert!(copy.as_mut().poll(&mut cx).is_pending());
        assert!(copy.as_mut().poll(&mut cx).is_pending());
        assert_eq!(
            peer_state.lock().unwrap().writes,
            vec![b"late client message".to_vec()],
            "a client message inside the window must be delivered"
        );
    }

    #[tokio::test(start_paused = true)]
    async fn the_window_starts_at_the_read_eof_under_shutdown_backpressure() {
        // The ended direction cannot finish its shutdown, so the window has to
        // close on the read end-of-stream alone.
        let mut client = ScriptedMessageStream::new(true, vec![ReadStep::Empty]);
        let mut peer = ScriptedMessageStream::new(true, vec![ReadStep::Idle]);
        peer.state.lock().unwrap().blocked_shutdowns = usize::MAX;
        let mut copy = Box::pin(copy_bidirectional_message(
            &mut client,
            &mut peer,
            false,
            false,
        ));
        let waker = noop_waker();
        let mut cx = Context::from_waker(&waker);

        assert!(copy.as_mut().poll(&mut cx).is_pending());
        tokio::time::advance(EOF_DRAIN_WINDOW + Duration::from_secs(1)).await;
        let error = match copy.as_mut().poll(&mut cx) {
            Poll::Ready(Err(error)) => error,
            other => panic!("undrained accepted output must be reported, got {other:?}"),
        };
        assert_eq!(error.kind(), io::ErrorKind::TimedOut);
    }

    #[tokio::test(start_paused = true)]
    async fn the_window_reports_a_cached_message_that_could_not_be_written() {
        // The reply is accepted but the writer stays blocked, so the window must
        // not report a clean end.
        let mut client = ScriptedMessageStream::new(true, vec![ReadStep::Empty]);
        let mut peer = ScriptedMessageStream::new(
            true,
            vec![ReadStep::Message(b"reply".to_vec()), ReadStep::Idle],
        );
        let client_state = Arc::clone(&client.state);
        client_state.lock().unwrap().blocked_writes = usize::MAX;
        let mut copy = Box::pin(copy_bidirectional_message(
            &mut client,
            &mut peer,
            false,
            false,
        ));
        let waker = noop_waker();
        let mut cx = Context::from_waker(&waker);

        assert!(copy.as_mut().poll(&mut cx).is_pending());
        tokio::time::advance(EOF_DRAIN_WINDOW + Duration::from_secs(1)).await;
        let error = match copy.as_mut().poll(&mut cx) {
            Poll::Ready(Err(error)) => error,
            other => panic!("a cached message must not be reported as delivered: {other:?}"),
        };
        assert_eq!(error.kind(), io::ErrorKind::TimedOut);
    }

    #[tokio::test(start_paused = true)]
    async fn a_blocked_writer_still_delivers_inside_the_window() {
        let mut client = ScriptedMessageStream::new(true, vec![ReadStep::Empty]);
        let mut peer = ScriptedMessageStream::new(
            true,
            vec![ReadStep::Message(b"reply".to_vec()), ReadStep::Idle],
        );
        let client_state = Arc::clone(&client.state);
        client_state.lock().unwrap().blocked_writes = 1;
        let mut copy = Box::pin(copy_bidirectional_message(
            &mut client,
            &mut peer,
            false,
            false,
        ));
        let waker = noop_waker();
        let mut cx = Context::from_waker(&waker);

        assert!(copy.as_mut().poll(&mut cx).is_pending());
        assert!(copy.as_mut().poll(&mut cx).is_pending());
        assert_eq!(
            client_state.lock().unwrap().writes,
            vec![b"reply".to_vec()],
            "backpressure inside the window must not lose the reply"
        );
    }

    #[tokio::test(start_paused = true)]
    async fn cancelling_the_copy_during_the_drain_keeps_the_streams_usable() {
        let mut client = ScriptedMessageStream::new(true, vec![ReadStep::Empty]);
        let mut peer = ScriptedMessageStream::new(true, vec![ReadStep::Idle]);
        {
            let mut copy = Box::pin(copy_bidirectional_message(
                &mut client,
                &mut peer,
                false,
                false,
            ));
            let waker = noop_waker();
            let mut cx = Context::from_waker(&waker);
            assert!(copy.as_mut().poll(&mut cx).is_pending());
            // The task is cancelled while the window is open.
        }

        // The streams are still usable and a new copy finishes on its own.
        let mut copy = Box::pin(copy_bidirectional_message(
            &mut client,
            &mut peer,
            false,
            false,
        ));
        let waker = noop_waker();
        let mut cx = Context::from_waker(&waker);
        assert!(copy.as_mut().poll(&mut cx).is_pending());
        tokio::time::advance(EOF_DRAIN_WINDOW + Duration::from_secs(1)).await;
        assert!(matches!(copy.as_mut().poll(&mut cx), Poll::Ready(Ok(()))));
    }

    #[tokio::test]
    async fn a_direction_error_still_fails_the_copy_immediately() {
        let mut client = ScriptedMessageStream::new(true, vec![ReadStep::Empty]);
        let mut peer =
            ScriptedMessageStream::new(true, vec![ReadStep::Error(io::ErrorKind::ConnectionReset)]);
        let mut copy = Box::pin(copy_bidirectional_message(
            &mut client,
            &mut peer,
            false,
            false,
        ));
        let waker = noop_waker();
        let mut cx = Context::from_waker(&waker);

        let error = match copy.as_mut().poll(&mut cx) {
            Poll::Ready(Err(error)) => error,
            other => panic!("a direction error must not wait for the other direction: {other:?}"),
        };
        assert_eq!(error.kind(), io::ErrorKind::ConnectionReset);
    }

    #[tokio::test(start_paused = true)]
    async fn an_empty_message_refreshes_the_association_activity() {
        // Both directions stay open and only the peer sends, so the association
        // activity check alone decides when the association ends.
        let mut client = ScriptedMessageStream::new(true, vec![ReadStep::Idle]);
        let mut peer = ScriptedMessageStream::new(false, vec![ReadStep::Idle]);
        let peer_state = Arc::clone(&peer.state);
        let client_state = Arc::clone(&client.state);
        let mut copy = Box::pin(copy_bidirectional_message(
            &mut client,
            &mut peer,
            false,
            false,
        ));
        let waker = noop_waker();
        let mut cx = Context::from_waker(&waker);

        assert!(copy.as_mut().poll(&mut cx).is_pending());

        // One empty datagram arrives after the idle boundary would have passed.
        tokio::time::advance(Duration::from_secs(
            u64::from(DEFAULT_ASSOCIATION_TIMEOUT_SECS) + 1,
        ))
        .await;
        peer_state.lock().unwrap().reads.push_front(ReadStep::Empty);
        assert!(
            copy.as_mut().poll(&mut cx).is_pending(),
            "an empty datagram is activity and must not end the association"
        );
        assert_eq!(
            client_state.lock().unwrap().writes,
            vec![Vec::new()],
            "the empty datagram must be delivered"
        );

        // Without any further message the association still expires.
        tokio::time::advance(Duration::from_secs(
            u64::from(DEFAULT_ASSOCIATION_TIMEOUT_SECS) + 1,
        ))
        .await;
        assert!(
            matches!(copy.as_mut().poll(&mut cx), Poll::Ready(Ok(()))),
            "an idle association must still expire"
        );
    }

    #[tokio::test(start_paused = true)]
    async fn an_idle_association_without_eof_still_ends_after_the_association_timeout() {
        let mut client = ScriptedMessageStream::new(true, vec![ReadStep::Idle]);
        let mut peer = ScriptedMessageStream::new(true, vec![ReadStep::Idle]);
        let mut copy = Box::pin(copy_bidirectional_message(
            &mut client,
            &mut peer,
            false,
            false,
        ));
        let waker = noop_waker();
        let mut cx = Context::from_waker(&waker);

        assert!(copy.as_mut().poll(&mut cx).is_pending());
        tokio::time::advance(Duration::from_secs(
            u64::from(DEFAULT_ASSOCIATION_TIMEOUT_SECS) + 1,
        ))
        .await;
        assert!(
            matches!(copy.as_mut().poll(&mut cx), Poll::Ready(Ok(()))),
            "an idle association must still expire"
        );
    }

    #[tokio::test(start_paused = true)]
    async fn an_idle_timeout_reports_a_reply_the_writer_never_accepted() {
        // Neither direction ends. The reply is accepted from the peer, but the
        // idle client's writer stays blocked past the association timeout, so
        // the end must not be reported as clean.
        let mut client = ScriptedMessageStream::new(true, vec![ReadStep::Idle]);
        let mut peer = ScriptedMessageStream::new(
            true,
            vec![ReadStep::Message(b"reply".to_vec()), ReadStep::Idle],
        );
        let client_state = Arc::clone(&client.state);
        client_state.lock().unwrap().blocked_writes = usize::MAX;
        let mut copy = Box::pin(copy_bidirectional_message(
            &mut client,
            &mut peer,
            false,
            false,
        ));
        let waker = noop_waker();
        let mut cx = Context::from_waker(&waker);

        assert!(copy.as_mut().poll(&mut cx).is_pending());
        assert!(
            client_state.lock().unwrap().writes.is_empty(),
            "the stalled writer did not accept the reply"
        );

        tokio::time::advance(Duration::from_secs(
            u64::from(DEFAULT_ASSOCIATION_TIMEOUT_SECS) + 1,
        ))
        .await;
        let error = match copy.as_mut().poll(&mut cx) {
            Poll::Ready(Err(error)) => error,
            other => panic!("a cached reply must not be reported as delivered: {other:?}"),
        };
        assert_eq!(error.kind(), io::ErrorKind::TimedOut);
    }

    #[tokio::test(start_paused = true)]
    async fn an_idle_timeout_reports_a_reply_the_writer_never_flushed() {
        // Neither direction ends. The reply reaches the idle client's writer,
        // but its flush never completes, so the end must not be reported as
        // clean.
        let mut client = ScriptedMessageStream::new(true, vec![ReadStep::Idle]);
        let mut peer = ScriptedMessageStream::new(
            true,
            vec![ReadStep::Message(b"reply".to_vec()), ReadStep::Idle],
        );
        let client_state = Arc::clone(&client.state);
        client_state.lock().unwrap().blocked_flushes = usize::MAX;
        let mut copy = Box::pin(copy_bidirectional_message(
            &mut client,
            &mut peer,
            false,
            false,
        ));
        let waker = noop_waker();
        let mut cx = Context::from_waker(&waker);

        assert!(copy.as_mut().poll(&mut cx).is_pending());
        assert_eq!(
            client_state.lock().unwrap().writes,
            vec![b"reply".to_vec()],
            "the writer accepted the reply"
        );

        tokio::time::advance(Duration::from_secs(
            u64::from(DEFAULT_ASSOCIATION_TIMEOUT_SECS) + 1,
        ))
        .await;
        let error = match copy.as_mut().poll(&mut cx) {
            Poll::Ready(Err(error)) => error,
            other => panic!("a stalled reply flush must not be reported as delivered: {other:?}"),
        };
        assert_eq!(error.kind(), io::ErrorKind::TimedOut);
    }

    #[tokio::test(start_paused = true)]
    async fn an_idle_association_that_drained_still_ends_cleanly() {
        // The reply is written and flushed while the association is idle, so
        // the ordinary timeout still releases it cleanly.
        let mut client = ScriptedMessageStream::new(true, vec![ReadStep::Idle]);
        let mut peer = ScriptedMessageStream::new(
            true,
            vec![ReadStep::Message(b"reply".to_vec()), ReadStep::Idle],
        );
        let client_state = Arc::clone(&client.state);
        let mut copy = Box::pin(copy_bidirectional_message(
            &mut client,
            &mut peer,
            false,
            false,
        ));
        let waker = noop_waker();
        let mut cx = Context::from_waker(&waker);

        assert!(copy.as_mut().poll(&mut cx).is_pending());
        assert_eq!(
            client_state.lock().unwrap().writes,
            vec![b"reply".to_vec()],
            "the reply must be delivered before the timeout"
        );

        tokio::time::advance(Duration::from_secs(
            u64::from(DEFAULT_ASSOCIATION_TIMEOUT_SECS) + 1,
        ))
        .await;
        assert!(
            matches!(copy.as_mut().poll(&mut cx), Poll::Ready(Ok(()))),
            "a drained association must still expire cleanly"
        );
    }
}
