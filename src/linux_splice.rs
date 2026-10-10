//! Linux splice fast path for eligible raw TCP relay directions.
//!
//! `splice(2)` moves payload between a socket and a pipe inside the kernel, so
//! those bytes never pass through a userspace buffer. This module owns the
//! syscall plumbing and the pipe descriptors. `copy_bidirectional` owns the
//! decision to use them, the pipe byte count, and the relay byte order.

use std::io;
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd, RawFd};

use tokio::io::Interest;
use tokio::net::TcpStream;

/// Smallest kernel-reported socket buffer that keeps the fast path eligible.
///
/// The symmetric constrained-window measurement shows a material local loss
/// below this value, so such a direction keeps ordinary copying. The value is an
/// evidence-based conservative exclusion of the measured local case, not a
/// universal no-regression proof.
pub const MIN_SOCKET_BUFFER_BYTES: usize = 32 * 1024;

/// True when both relay sockets report at least [`MIN_SOCKET_BUFFER_BYTES`] for
/// both the send and the receive direction.
///
/// The values are read-only queries of the kernel-reported effective sizes, so
/// Linux doubling of a requested size is already included. A failed query keeps
/// ordinary copying. The check covers these local sockets at this moment only:
/// it does not cover later socket changes, and it cannot see an unobservable
/// remote receive window.
pub fn local_socket_buffers_admissible(read: &TcpStream, write: &TcpStream) -> bool {
    socket_buffers_admissible(read) && socket_buffers_admissible(write)
}

/// Exact threshold test, so the boundary stays visible in one place.
fn buffers_admissible(send: usize, receive: usize) -> bool {
    send >= MIN_SOCKET_BUFFER_BYTES && receive >= MIN_SOCKET_BUFFER_BYTES
}

fn socket_buffers_admissible(stream: &TcpStream) -> bool {
    #[cfg(test)]
    if test_hooks::socket_buffer_query_fails() {
        return false;
    }
    let socket = socket2::SockRef::from(stream);
    match (socket.send_buffer_size(), socket.recv_buffer_size()) {
        (Ok(send), Ok(receive)) => buffers_admissible(send, receive),
        _ => false,
    }
}

/// Attempts allowed after `EINTR` inside one syscall wrapper.
///
/// A signal can interrupt a splice or a pipe read. Retrying inside the wrapper
/// keeps the connection alive without touching the socket readiness or the pipe
/// byte count. The cap keeps a signal storm from spinning; after it the wrapper
/// reports `Interrupted` and the copy loop yields to the scheduler.
const MAX_INTERRUPTED_RETRIES: usize = 8;

/// Capacity assumed when `F_GETPIPE_SZ` fails.
///
/// Every Linux pipe holds at least one page, so a splice request of this size
/// into an empty pipe cannot block on pipe space.
const FALLBACK_PIPE_CAPACITY: usize = 4096;

/// A nonblocking, close-on-exec pipe that buffers one relay direction.
///
/// The descriptor pair is owned here and closed on drop, which covers normal
/// completion, errors, cancellation, and fallback. Every splice call also passes
/// `SPLICE_F_NONBLOCK`, so a full pipe or a full send buffer reports
/// `WouldBlock` instead of parking the relay task.
#[derive(Debug)]
pub struct SplicePipe {
    read: OwnedFd,
    write: OwnedFd,
    capacity: usize,
}

impl SplicePipe {
    /// Allocates the pipe. A failure keeps buffered copying for the direction.
    pub fn new() -> io::Result<Self> {
        #[cfg(test)]
        test_hooks::maybe_fail_pipe_allocation()?;

        let mut descriptors = [0 as RawFd; 2];
        // SAFETY: `descriptors` is a valid array of two descriptors for `pipe2`
        // to fill, and the flags are valid open flags.
        let created =
            unsafe { libc::pipe2(descriptors.as_mut_ptr(), libc::O_CLOEXEC | libc::O_NONBLOCK) };
        if created != 0 {
            return Err(io::Error::last_os_error());
        }
        // SAFETY: `pipe2` returned success, so both descriptors are open and
        // this function owns them. Moving each one into an `OwnedFd` makes the
        // ownership explicit and closes it on drop.
        let read = unsafe { OwnedFd::from_raw_fd(descriptors[0]) };
        // SAFETY: as above, for the write end of the same new pipe.
        let write = unsafe { OwnedFd::from_raw_fd(descriptors[1]) };

        let capacity = pipe_capacity(&read);
        #[cfg(test)]
        test_hooks::record_pipe_allocation();

        Ok(Self {
            read,
            write,
            capacity,
        })
    }

    /// Bytes the pipe can hold. One socket-to-pipe splice is bounded by this
    /// value, so the request cannot block on a full pipe.
    pub fn capacity(&self) -> usize {
        self.capacity
    }

    /// Moves up to `len` bytes from `source` into the pipe.
    ///
    /// `try_io` clears the read readiness of `source` only when the syscall
    /// reports `WouldBlock`, which is the readiness that the caller must wait
    /// for again. A full pipe reports `WouldBlock` too, so callers must keep
    /// `len` within the free pipe space.
    pub fn splice_socket_in(&self, source: &TcpStream, len: usize) -> io::Result<usize> {
        let pipe_write = self.write.as_raw_fd();
        source.try_io(Interest::READABLE, || {
            splice_descriptors(source.as_raw_fd(), pipe_write, len)
        })
    }

    /// Moves up to `len` bytes from the pipe into `destination`.
    ///
    /// `try_io` clears the write readiness of `destination` only when the
    /// syscall reports `WouldBlock`, which happens when the send buffer is full.
    pub fn splice_socket_out(&self, destination: &TcpStream, len: usize) -> io::Result<usize> {
        #[cfg(test)]
        if test_hooks::out_splice_blocked() {
            return Err(io::Error::from(io::ErrorKind::WouldBlock));
        }
        let pipe_read = self.read.as_raw_fd();
        destination.try_io(Interest::WRITABLE, || {
            splice_descriptors(pipe_read, destination.as_raw_fd(), len)
        })
    }

    /// Reads bytes that the pipe already holds with `read(2)`.
    ///
    /// Only the fallback path uses this. Once splice is denied, the bytes in
    /// the pipe must still reach the writer in order. An interrupted call is
    /// retried here, so the pipe byte count only changes when bytes moved.
    pub fn read_into(&self, destination: &mut [u8]) -> io::Result<usize> {
        let mut attempts = 0;
        loop {
            attempts += 1;
            #[cfg(test)]
            if test_hooks::take_pipe_read_interruption() {
                if attempts < MAX_INTERRUPTED_RETRIES {
                    continue;
                }
                return Err(io::Error::from(io::ErrorKind::Interrupted));
            }

            // SAFETY: `destination` is a valid writable slice for its own
            // length, and the descriptor stays open and owned by `self`.
            let read = unsafe {
                libc::read(
                    self.read.as_raw_fd(),
                    destination.as_mut_ptr().cast(),
                    destination.len(),
                )
            };
            if read >= 0 {
                return Ok(read as usize);
            }
            let error = io::Error::last_os_error();
            if error.kind() != io::ErrorKind::Interrupted || attempts >= MAX_INTERRUPTED_RETRIES {
                return Err(error);
            }
        }
    }
}

/// Per-direction splice state for one relay direction.
///
/// The pipe is allocated on the first splice attempt, so an ineligible or idle
/// direction holds no descriptors. `pending` counts the bytes in the pipe
/// exactly, which is what keeps the byte order across a fallback.
#[derive(Debug, Default)]
pub struct SpliceState {
    pipe: Option<SplicePipe>,
    pending: usize,
    disabled: bool,
    /// True once the local socket buffers were checked for this direction.
    checked: bool,
}

impl SpliceState {
    /// Bytes in the pipe that must reach the writer before any new socket read.
    pub fn pending(&self) -> usize {
        self.pending
    }

    /// True when splice must not run again for this direction.
    pub fn disabled(&self) -> bool {
        self.disabled
    }

    /// True while the local socket buffers still need their one-time check.
    ///
    /// The decision is taken when the direction first becomes natively eligible,
    /// so no descriptor is cached and no query repeats per chunk.
    pub fn needs_buffer_check(&self) -> bool {
        !self.checked
    }

    /// Records the one-time local socket buffer decision. An inadmissible or
    /// unreadable result keeps ordinary copying for the rest of the direction.
    pub fn record_buffer_check(&mut self, admissible: bool) {
        self.checked = true;
        if !admissible {
            self.disable();
        }
    }

    /// Stops the fast path for this direction.
    ///
    /// The pipe stays until it is empty, so bytes that a denied splice already
    /// moved still reach the writer in order.
    pub fn disable(&mut self) {
        self.disabled = true;
        self.release_when_empty();
    }

    /// Appends up to `limit` bytes from `socket` into the empty pipe.
    ///
    /// The caller waits for read readiness first and only uses this while the
    /// pipe is empty. An empty pipe always has a free slot, so a `WouldBlock`
    /// belongs to that readiness and `try_io` may clear it. `None` means the
    /// pipe could not be allocated, which keeps buffered copying.
    pub fn splice_in_ready(
        &mut self,
        socket: &TcpStream,
        limit: usize,
    ) -> Option<io::Result<usize>> {
        if self.pipe.is_none() {
            self.pipe = Some(SplicePipe::new().ok()?);
        }
        let pipe = self.pipe.as_ref()?;
        let result = pipe.splice_socket_in(socket, pipe.capacity().min(limit));
        if let Ok(moved) = result.as_ref() {
            self.pending += *moved;
        }
        Some(result)
    }

    /// Moves up to `limit` bytes from the pipe into `socket`.
    pub fn splice_out(&mut self, socket: &TcpStream, limit: usize) -> io::Result<usize> {
        let Some(pipe) = self.pipe.as_ref() else {
            return Err(io::Error::other(
                "splice pipe is missing while it holds bytes",
            ));
        };
        let result = pipe.splice_socket_out(socket, self.pending.min(limit));
        if let Ok(moved) = result.as_ref() {
            self.pending -= *moved;
        }
        result
    }

    /// Reads pipe bytes into `destination` for the buffered fallback.
    pub fn drain_into(&mut self, destination: &mut [u8]) -> io::Result<usize> {
        let Some(pipe) = self.pipe.as_ref() else {
            return Err(io::Error::other(
                "splice pipe is missing while it holds bytes",
            ));
        };
        let result = pipe.read_into(destination);
        if let Ok(moved) = result.as_ref() {
            self.pending -= *moved;
        }
        self.release_when_empty();
        result
    }

    /// Closes the pipe once the fast path is disabled and the pipe is empty.
    fn release_when_empty(&mut self) {
        if self.disabled && self.pending == 0 {
            self.pipe = None;
        }
    }
}

/// True when a splice failure means the kernel or the sandbox denies splice.
///
/// Every other failure is a real I/O error for the connection.
pub fn splice_unsupported(error: &io::Error) -> bool {
    matches!(
        error.raw_os_error(),
        Some(libc::ENOSYS | libc::EINVAL | libc::EPERM | libc::EXDEV | libc::EOPNOTSUPP)
    )
}

/// Moves up to `len` bytes between two descriptors without file offsets.
///
/// `SPLICE_F_NONBLOCK` keeps the call from sleeping. Both descriptors are
/// nonblocking as well, so the call reports `WouldBlock` instead of parking. An
/// interrupted call is retried here, so a signal never reaches the copy state
/// machine that owns the readiness and the pipe byte count.
fn splice_descriptors(source: RawFd, destination: RawFd, len: usize) -> io::Result<usize> {
    #[cfg(test)]
    test_hooks::maybe_fail_splice()?;

    let mut attempts = 0;
    loop {
        attempts += 1;
        #[cfg(test)]
        if test_hooks::take_splice_interruption() {
            if attempts < MAX_INTERRUPTED_RETRIES {
                continue;
            }
            return Err(io::Error::from(io::ErrorKind::Interrupted));
        }

        // SAFETY: `splice` only reads the two descriptors and the length. Both
        // descriptors stay open and owned by the caller for the whole call, and
        // the null offsets ask for each descriptor's current position.
        let moved = unsafe {
            libc::splice(
                source,
                std::ptr::null_mut(),
                destination,
                std::ptr::null_mut(),
                len,
                libc::SPLICE_F_NONBLOCK,
            )
        };
        if moved >= 0 {
            let moved = moved as usize;
            #[cfg(test)]
            test_hooks::record_splice(moved);
            return Ok(moved);
        }
        let error = io::Error::last_os_error();
        if error.kind() != io::ErrorKind::Interrupted || attempts >= MAX_INTERRUPTED_RETRIES {
            return Err(error);
        }
    }
}

/// Reads the pipe capacity, which bounds one socket-to-pipe splice.
fn pipe_capacity(read: &OwnedFd) -> usize {
    // SAFETY: `F_GETPIPE_SZ` only reads one integer from an open descriptor.
    let capacity = unsafe { libc::fcntl(read.as_raw_fd(), libc::F_GETPIPE_SZ) };
    if capacity > 0 {
        capacity as usize
    } else {
        FALLBACK_PIPE_CAPACITY
    }
}

/// Fault injection and syscall counting for the splice tests.
///
/// Nothing here exists in a production build, so the fast path keeps no
/// instrumentation and no test-only branch.
#[cfg(test)]
pub(crate) mod test_hooks {
    use std::io;
    use std::sync::atomic::{AtomicBool, AtomicI64, AtomicU64, Ordering};

    /// Splice calls that moved bytes.
    static SPLICE_CALLS: AtomicU64 = AtomicU64::new(0);
    /// Bytes moved by those calls.
    static SPLICE_BYTES: AtomicU64 = AtomicU64::new(0);
    /// Pipes allocated so far. Monotonic, so a transient allocation is visible
    /// even after the pipe closed again.
    static PIPE_ALLOCATIONS: AtomicU64 = AtomicU64::new(0);
    /// When set, pipe allocation fails like an exhausted descriptor table.
    static FAIL_PIPE_ALLOCATION: AtomicBool = AtomicBool::new(false);
    /// Remaining splice calls before a simulated `ENOSYS`. Negative disables it.
    static SPLICE_FAIL_AFTER: AtomicI64 = AtomicI64::new(-1);
    /// Splice attempt index that reports `EINTR`. Zero disables it.
    static SPLICE_INTERRUPT_AT: AtomicI64 = AtomicI64::new(0);
    /// When set, every splice attempt reports `EINTR`.
    static SPLICE_INTERRUPT_ALWAYS: AtomicBool = AtomicBool::new(false);
    /// Splice attempts seen by the injection hook.
    static SPLICE_ATTEMPTS: AtomicU64 = AtomicU64::new(0);
    /// Pipe read attempt index that reports `EINTR`. Zero disables it.
    static PIPE_READ_INTERRUPT_AT: AtomicI64 = AtomicI64::new(0);
    /// When set, every fallback pipe read reports `EINTR`.
    static PIPE_READ_INTERRUPT_ALWAYS: AtomicBool = AtomicBool::new(false);
    /// When set, every pipe-to-socket splice reports `WouldBlock`.
    static OUT_SPLICE_BLOCKED: AtomicBool = AtomicBool::new(false);
    /// When set, the socket buffer query fails.
    static SOCKET_BUFFER_QUERY_FAILS: AtomicBool = AtomicBool::new(false);
    /// One-shot interruption of the next splice attempt.
    static SPLICE_INTERRUPT_ONCE: AtomicBool = AtomicBool::new(false);
    /// One-shot `ENOSYS` for the next splice call.
    static SPLICE_FAIL_ONCE: AtomicBool = AtomicBool::new(false);
    /// Fallback pipe reads seen by the injection hook.
    static PIPE_READ_ATTEMPTS: AtomicU64 = AtomicU64::new(0);

    /// Clears every hook. The unit tests run on a single thread.
    pub(crate) fn reset() {
        SPLICE_CALLS.store(0, Ordering::Relaxed);
        SPLICE_BYTES.store(0, Ordering::Relaxed);
        PIPE_ALLOCATIONS.store(0, Ordering::Relaxed);
        FAIL_PIPE_ALLOCATION.store(false, Ordering::Relaxed);
        SPLICE_FAIL_AFTER.store(-1, Ordering::Relaxed);
        SPLICE_INTERRUPT_AT.store(0, Ordering::Relaxed);
        SPLICE_INTERRUPT_ALWAYS.store(false, Ordering::Relaxed);
        SPLICE_ATTEMPTS.store(0, Ordering::Relaxed);
        PIPE_READ_INTERRUPT_AT.store(0, Ordering::Relaxed);
        PIPE_READ_INTERRUPT_ALWAYS.store(false, Ordering::Relaxed);
        PIPE_READ_ATTEMPTS.store(0, Ordering::Relaxed);
        OUT_SPLICE_BLOCKED.store(false, Ordering::Relaxed);
        SOCKET_BUFFER_QUERY_FAILS.store(false, Ordering::Relaxed);
        SPLICE_INTERRUPT_ONCE.store(false, Ordering::Relaxed);
        SPLICE_FAIL_ONCE.store(false, Ordering::Relaxed);
    }

    /// Makes every socket buffer query fail while set.
    pub(crate) fn fail_socket_buffer_query(fails: bool) {
        SOCKET_BUFFER_QUERY_FAILS.store(fails, Ordering::Relaxed);
    }

    /// Interrupts the next splice attempt once.
    pub(crate) fn interrupt_next_splice() {
        SPLICE_INTERRUPT_ONCE.store(true, Ordering::Relaxed);
    }

    /// Reports `ENOSYS` for the next splice call once.
    pub(crate) fn fail_next_splice() {
        SPLICE_FAIL_ONCE.store(true, Ordering::Relaxed);
    }

    pub(super) fn socket_buffer_query_fails() -> bool {
        SOCKET_BUFFER_QUERY_FAILS.load(Ordering::Relaxed)
    }

    /// Reports `WouldBlock` for every pipe-to-socket splice while set.
    pub(crate) fn block_out_splice(blocked: bool) {
        OUT_SPLICE_BLOCKED.store(blocked, Ordering::Relaxed);
    }

    pub(super) fn out_splice_blocked() -> bool {
        OUT_SPLICE_BLOCKED.load(Ordering::Relaxed)
    }

    /// Reports `EINTR` for the splice attempt with this 1-based index.
    pub(crate) fn interrupt_splice_at(attempt: i64) {
        SPLICE_INTERRUPT_AT.store(attempt, Ordering::Relaxed);
    }

    /// Reports `EINTR` for every splice attempt while `always` is set.
    pub(crate) fn interrupt_splice_always(always: bool) {
        SPLICE_INTERRUPT_ALWAYS.store(always, Ordering::Relaxed);
    }

    /// Reports `EINTR` for the fallback pipe read with this 1-based index.
    pub(crate) fn interrupt_pipe_read_at(attempt: i64) {
        PIPE_READ_INTERRUPT_AT.store(attempt, Ordering::Relaxed);
    }

    /// Reports `EINTR` for every fallback pipe read while `always` is set.
    pub(crate) fn interrupt_pipe_read_always(always: bool) {
        PIPE_READ_INTERRUPT_ALWAYS.store(always, Ordering::Relaxed);
    }

    /// Splice attempts seen so far, which indexes the injected interruption.
    pub(crate) fn splice_attempts() -> u64 {
        SPLICE_ATTEMPTS.load(Ordering::Relaxed)
    }

    pub(super) fn take_splice_interruption() -> bool {
        let attempt = SPLICE_ATTEMPTS.fetch_add(1, Ordering::Relaxed) as i64 + 1;
        SPLICE_INTERRUPT_ALWAYS.load(Ordering::Relaxed)
            || SPLICE_INTERRUPT_AT.load(Ordering::Relaxed) == attempt
            || SPLICE_INTERRUPT_ONCE.swap(false, Ordering::Relaxed)
    }

    pub(super) fn take_pipe_read_interruption() -> bool {
        let attempt = PIPE_READ_ATTEMPTS.fetch_add(1, Ordering::Relaxed) as i64 + 1;
        PIPE_READ_INTERRUPT_ALWAYS.load(Ordering::Relaxed)
            || PIPE_READ_INTERRUPT_AT.load(Ordering::Relaxed) == attempt
    }

    pub(crate) fn fail_pipe_allocation() {
        FAIL_PIPE_ALLOCATION.store(true, Ordering::Relaxed);
    }

    /// Allows `calls` splice calls, then reports `ENOSYS` for every later call.
    pub(crate) fn fail_splice_after(calls: i64) {
        SPLICE_FAIL_AFTER.store(calls, Ordering::Relaxed);
    }

    pub(crate) fn splice_calls() -> u64 {
        SPLICE_CALLS.load(Ordering::Relaxed)
    }

    pub(crate) fn splice_bytes() -> u64 {
        SPLICE_BYTES.load(Ordering::Relaxed)
    }

    /// Pipes allocated so far, including pipes that already closed.
    pub(crate) fn pipe_allocations() -> u64 {
        PIPE_ALLOCATIONS.load(Ordering::Relaxed)
    }

    pub(super) fn record_pipe_allocation() {
        PIPE_ALLOCATIONS.fetch_add(1, Ordering::Relaxed);
    }

    pub(super) fn maybe_fail_pipe_allocation() -> io::Result<()> {
        if FAIL_PIPE_ALLOCATION.load(Ordering::Relaxed) {
            return Err(io::Error::from_raw_os_error(libc::EMFILE));
        }
        Ok(())
    }

    pub(super) fn maybe_fail_splice() -> io::Result<()> {
        if SPLICE_FAIL_ONCE.swap(false, Ordering::Relaxed) {
            return Err(io::Error::from_raw_os_error(libc::ENOSYS));
        }
        let remaining = SPLICE_FAIL_AFTER.load(Ordering::Relaxed);
        if remaining < 0 {
            return Ok(());
        }
        if remaining == 0 {
            return Err(io::Error::from_raw_os_error(libc::ENOSYS));
        }
        SPLICE_FAIL_AFTER.store(remaining - 1, Ordering::Relaxed);
        Ok(())
    }

    pub(super) fn record_splice(bytes: usize) {
        SPLICE_CALLS.fetch_add(1, Ordering::Relaxed);
        SPLICE_BYTES.fetch_add(bytes as u64, Ordering::Relaxed);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;
    use std::net::{TcpListener, TcpStream as StdTcpStream};

    fn tcp_pair() -> io::Result<(StdTcpStream, StdTcpStream)> {
        let listener = TcpListener::bind("127.0.0.1:0")?;
        let address = listener.local_addr()?;
        let connector = StdTcpStream::connect(address)?;
        let (accepted, _) = listener.accept()?;
        Ok((connector, accepted))
    }

    #[ignore = "process-wide counters; runs in an isolated process"]
    #[tokio::test]
    async fn pipe_is_nonblocking_and_close_on_exec() -> io::Result<()> {
        // The fault-injection hooks are shared, so each test starts clean.
        test_hooks::reset();
        let pipe = SplicePipe::new()?;
        // The capacity comes from the kernel query, so any positive value the
        // running kernel reports is valid here.
        // SAFETY: `F_GETPIPE_SZ` only reads one integer from an open descriptor.
        let queried = unsafe { libc::fcntl(pipe.read.as_raw_fd(), libc::F_GETPIPE_SZ) };
        assert!(queried > 0, "the kernel must report a pipe capacity");
        assert_eq!(pipe.capacity(), queried as usize);
        assert!(pipe.capacity() > 0);
        // SAFETY: `F_GETFD` only reads the descriptor flags of an open fd.
        let descriptor_flags = unsafe { libc::fcntl(pipe.read.as_raw_fd(), libc::F_GETFD) };
        assert_ne!(descriptor_flags & libc::FD_CLOEXEC, 0);
        // SAFETY: `F_GETFL` only reads the status flags of an open fd.
        let status_flags = unsafe { libc::fcntl(pipe.read.as_raw_fd(), libc::F_GETFL) };
        assert_ne!(status_flags & libc::O_NONBLOCK, 0);
        assert_eq!(
            pipe.read_into(&mut [0; 8]).unwrap_err().kind(),
            io::ErrorKind::WouldBlock
        );
        Ok(())
    }

    #[ignore = "process-wide counters; runs in an isolated process"]
    #[tokio::test]
    async fn splice_moves_exact_bytes_through_the_pipe() -> io::Result<()> {
        test_hooks::reset();
        let (mut source_peer, source_local) = tcp_pair()?;
        let (sink_local, mut sink_peer) = tcp_pair()?;
        // Tokio only registers nonblocking sockets, which is also what the
        // splice path needs.
        source_local.set_nonblocking(true)?;
        sink_local.set_nonblocking(true)?;
        let source = TcpStream::from_std(source_local)?;
        let sink = TcpStream::from_std(sink_local)?;
        let payload: Vec<u8> = (0..4096)
            .map(|index| (index ^ (index >> 3)) as u8)
            .collect();
        source_peer.write_all(&payload).unwrap();
        let pipe = SplicePipe::new().unwrap();

        source.readable().await.unwrap();
        let moved = pipe.splice_socket_in(&source, pipe.capacity()).unwrap();
        assert_eq!(moved, payload.len());
        sink.writable().await.unwrap();
        assert_eq!(pipe.splice_socket_out(&sink, moved).unwrap(), moved);

        let mut received = vec![0; payload.len()];
        std::io::Read::read_exact(&mut sink_peer, &mut received).unwrap();
        assert_eq!(received, payload);
        Ok(())
    }

    /// Tests that read the process-global fault injection, the syscall counters,
    /// or `/proc/self/fd`. The default parallel unit run skips them, and the
    /// driver below runs each one in its own single-threaded process, because
    /// those counters and the descriptor table belong to the whole process.
    ///
    /// The CI workflow runs `cargo test --workspace --lib --bins` without
    /// `--test-threads=1`, so this list must stay complete.
    const PROCESS_WIDE_TESTS: &[&str] = &[
        "copy_bidirectional::splice_tests::a_blocked_destination_resumes_after_it_drains",
        "copy_bidirectional::splice_tests::a_failed_socket_buffer_query_keeps_buffered_copying",
        "copy_bidirectional::splice_tests::a_peer_reset_stays_a_real_io_error",
        "copy_bidirectional::splice_tests::an_interrupted_fallback_pipe_read_retries_without_loss",
        "copy_bidirectional::splice_tests::an_interrupted_fallback_pipe_read_yields_instead_of_failing",
        "copy_bidirectional::splice_tests::an_interrupted_pipe_to_socket_splice_after_partial_progress_keeps_accounting",
        "copy_bidirectional::splice_tests::an_interrupted_pipe_to_socket_splice_retries_without_loss",
        "copy_bidirectional::splice_tests::an_interrupted_socket_to_pipe_splice_retries_without_loss",
        "copy_bidirectional::splice_tests::descriptors_return_to_baseline_after_sessions_and_cancellation",
        "copy_bidirectional::splice_tests::eof_waits_for_the_pipe_to_drain_before_completion",
        "copy_bidirectional::splice_tests::fallback_after_partial_pipe_progress_keeps_order",
        "copy_bidirectional::splice_tests::fallback_with_pipe_bytes_keeps_order_and_payload",
        "copy_bidirectional::splice_tests::flush_barrier_precedes_the_fast_path",
        "copy_bidirectional::splice_tests::ineligible_streams_keep_buffered_copying",
        "copy_bidirectional::splice_tests::interrupted_splices_yield_instead_of_failing_the_relay",
        "copy_bidirectional::splice_tests::pipe_allocation_failure_keeps_buffered_copying",
        "copy_bidirectional::splice_tests::prepend_prefix_precedes_socket_bytes_and_defers_splice",
        "copy_bidirectional::splice_tests::progress_aware_copy_keeps_buffered_copying",
        "copy_bidirectional::splice_tests::short_fragmented_input_stays_byte_exact",
        "copy_bidirectional::splice_tests::small_local_socket_buffers_keep_buffered_copying",
        "copy_bidirectional::splice_tests::splice_keeps_the_client_first_half_close_order",
        "copy_bidirectional::splice_tests::splice_keeps_the_destination_first_half_close_order",
        "copy_bidirectional::splice_tests::splice_moves_exact_bytes_in_both_directions",
        "copy_bidirectional::splice_tests::splice_moves_exact_bytes_over_ipv6_loopback",
        "copy_bidirectional::splice_tests::stalled_output_does_not_spin",
        "copy_bidirectional::splice_tests::the_reverse_direction_progresses_while_the_output_stalls",
        "copy_bidirectional::splice_tests::tiny_buffer_bidirectional_exchange_stays_byte_exact",
        "copy_bidirectional::splice_tests::unsupported_splice_keeps_buffered_copying",
        "linux_splice::tests::pipe_is_nonblocking_and_close_on_exec",
        "linux_splice::tests::socket_buffer_guard_reads_the_kernel_reported_size",
        "linux_splice::tests::splice_moves_exact_bytes_through_the_pipe",
        "vless::vision_stream::tests::a_direct_vision_stream_moves_payload_through_splice",
        "vless::vless_response_stream::tests::the_response_header_gates_the_fast_path_without_reaching_the_payload",
    ];

    #[test]
    fn process_wide_tests_pass_in_isolated_processes() {
        let executable = std::env::current_exe().expect("test executable path");
        for name in PROCESS_WIDE_TESTS {
            run_isolated_test(&executable, name);
        }
    }

    /// Runs one named test in its own process with one test thread, and proves
    /// that exactly that test ran and passed. Output goes to a file, so no pipe
    /// can fill and no unrelated test can touch the shared state.
    fn run_isolated_test(executable: &std::path::Path, name: &str) {
        let output = tempfile::NamedTempFile::new().expect("isolated output file");
        let mut child = std::process::Command::new(executable)
            .args(["--exact", name, "--ignored", "--test-threads=1"])
            .stdout(std::process::Stdio::from(
                output.reopen().expect("reopen isolated stdout"),
            ))
            .stderr(std::process::Stdio::from(
                output.reopen().expect("reopen isolated stderr"),
            ))
            .spawn()
            .expect("spawn the isolated test");
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(120);
        let status = loop {
            match child.try_wait().expect("poll the isolated test") {
                Some(status) => break status,
                None if std::time::Instant::now() >= deadline => {
                    let _ = child.kill();
                    let _ = child.wait();
                    panic!("isolated test {name} exceeded its 120 s deadline");
                }
                None => std::thread::sleep(std::time::Duration::from_millis(10)),
            }
        };
        let captured = std::fs::read_to_string(output.path()).unwrap_or_default();
        assert!(
            status.success(),
            "isolated test {name} failed, output:\n{captured}"
        );
        assert!(
            captured.contains("1 passed"),
            "isolated test {name} must run exactly one passing test, output:\n{captured}"
        );
        assert!(
            captured.contains(name),
            "isolated test {name} must report the requested test, output:\n{captured}"
        );
    }

    #[test]
    fn socket_buffer_guard_boundary_is_exact() {
        assert!(buffers_admissible(
            MIN_SOCKET_BUFFER_BYTES,
            MIN_SOCKET_BUFFER_BYTES
        ));
        assert!(!buffers_admissible(
            MIN_SOCKET_BUFFER_BYTES - 1,
            MIN_SOCKET_BUFFER_BYTES
        ));
        assert!(!buffers_admissible(
            MIN_SOCKET_BUFFER_BYTES,
            MIN_SOCKET_BUFFER_BYTES - 1
        ));
        assert!(!buffers_admissible(0, MIN_SOCKET_BUFFER_BYTES));
    }

    #[ignore = "process-wide counters; runs in an isolated process"]
    #[tokio::test]
    async fn socket_buffer_guard_reads_the_kernel_reported_size() -> io::Result<()> {
        test_hooks::reset();
        let (connector, accepted) = tcp_pair()?;
        connector.set_nonblocking(true)?;
        accepted.set_nonblocking(true)?;
        let read = TcpStream::from_std(accepted)?;
        let write = TcpStream::from_std(connector)?;

        // A 16 KiB request is reported as 32 KiB, the smallest admissible size.
        for stream in [&read, &write] {
            let socket = socket2::SockRef::from(stream);
            socket.set_send_buffer_size(16 * 1024)?;
            socket.set_recv_buffer_size(16 * 1024)?;
        }
        let queried = socket2::SockRef::from(&read);
        assert!(queried.send_buffer_size()? >= MIN_SOCKET_BUFFER_BYTES);
        assert!(queried.recv_buffer_size()? >= MIN_SOCKET_BUFFER_BYTES);
        assert!(local_socket_buffers_admissible(&read, &write));

        // A smaller request stays below the threshold, and the guard follows the
        // queried values rather than the requested ones.
        for stream in [&read, &write] {
            let socket = socket2::SockRef::from(stream);
            socket.set_send_buffer_size(8 * 1024)?;
            socket.set_recv_buffer_size(8 * 1024)?;
        }
        let queried = socket2::SockRef::from(&read);
        let send = queried.send_buffer_size()?;
        let receive = queried.recv_buffer_size()?;
        assert!(send < MIN_SOCKET_BUFFER_BYTES);
        assert!(receive < MIN_SOCKET_BUFFER_BYTES);
        assert_eq!(
            local_socket_buffers_admissible(&read, &write),
            buffers_admissible(send, receive),
        );
        assert!(!local_socket_buffers_admissible(&read, &write));

        // A failed query keeps ordinary copying.
        test_hooks::fail_socket_buffer_query(true);
        assert!(!local_socket_buffers_admissible(&read, &write));
        test_hooks::fail_socket_buffer_query(false);
        Ok(())
    }

    #[test]
    fn unsupported_errors_are_classified() {
        assert!(splice_unsupported(&io::Error::from_raw_os_error(
            libc::ENOSYS
        )));
        assert!(splice_unsupported(&io::Error::from_raw_os_error(
            libc::EPERM
        )));
        assert!(!splice_unsupported(&io::Error::from_raw_os_error(
            libc::ECONNRESET
        )));
        assert!(!splice_unsupported(&io::Error::from_raw_os_error(
            libc::EPIPE
        )));
    }
}
