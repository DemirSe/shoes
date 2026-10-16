// XUDP message stream - protocol-agnostic UDP session multiplexing
// Wraps any AsyncStream and provides XUDP frame encoding/decoding with session management
// Used by VLESS in this build

use bytes::{Buf, BufMut, Bytes, BytesMut};
use futures::ready;
use std::collections::{HashMap, VecDeque};
use std::net::SocketAddr;
use std::pin::Pin;
use std::sync::Arc;
use std::task::{Context, Poll};
use tokio::io::{AsyncRead, AsyncWrite, ReadBuf};

use crate::address::{Address, NetLocation};
use crate::async_stream::{
    AsyncFlushMessage, AsyncPing, AsyncReadSessionMessage, AsyncSessionMessageStream,
    AsyncShutdownMessage, AsyncStream, AsyncWriteSessionMessage, MessageSessionId,
};
use crate::resolver::{NativeResolver, ResolverCache};

use super::frame::{FrameMetadata, FrameOption, SessionStatus, TargetNetwork};

pub(crate) const MAX_XUDP_ROUTES: usize = 1024;

/// Hard cap on live wire sessions in one XUDP stream.
///
/// Each session is a small heap record (a map plus a destination, roughly
/// 150 bytes) and holds no file descriptors, so even the cap stays far below
/// any memory limit while exceeding what a legitimate client keeps open. The
/// map still needs a bound: a peer can send `New` frames for fresh session IDs
/// without ever ending them.
pub(crate) const MAX_XUDP_SESSIONS: usize = 16_384;

pub struct XudpMessageStream {
    /// Underlying byte stream (e.g. a VLESS VisionStream or TLS stream) carrying raw XUDP frame bytes
    inner_stream: Box<dyn AsyncStream>,

    /// Read buffer for incoming XUDP frames. The first `read_filled` bytes are
    /// frame data; the rest is initialized spare capacity for reads.
    read_buffer: BytesMut,

    /// Logical length of the frame data at the front of `read_buffer`.
    read_filled: usize,

    /// Write buffer for outgoing XUDP frames
    write_buffer: BytesMut,

    next_route_id: MessageSessionId,

    /// Active protocol sessions and their independently routed destinations.
    wire_sessions: HashMap<u16, WireSession>,

    /// Insertion order of wire sessions with their generation, used to evict
    /// the oldest live session once `MAX_XUDP_SESSIONS` is reached. Entries for
    /// replaced or closed sessions are skipped lazily when they surface and are
    /// compacted once the queue outgrows the cap.
    wire_session_order: VecDeque<(u16, u64)>,

    /// Generation handed to the next wire session. Refreshing a session under
    /// the same ID gives it a new generation, which marks older queue entries
    /// stale so they cannot evict the replacement.
    next_wire_session_generation: u64,

    /// Translates router identities back to protocol session IDs and destinations.
    routes: HashMap<MessageSessionId, RoutedSession>,

    /// Route identities whose protocol session has ended.
    closed_routes: VecDeque<MessageSessionId>,

    /// Resolver cache for hostname resolution
    /// Resolves hostnames to IPs before storing in session maps
    resolver_cache: ResolverCache,

    /// Buffered incoming message waiting for destination resolution.
    incoming_message: Option<(Bytes, NetLocation, u16)>,

    /// EOF flag
    is_eof: bool,
}

struct WireSession {
    default_destination: NetLocation,
    routes: HashMap<NetLocation, MessageSessionId>,
    /// Identifies this exact session instance in `wire_session_order`, so
    /// stale order entries cannot evict a replacement that reused the ID.
    generation: u64,
}

struct RoutedSession {
    wire_session_id: u16,
    original_destination: NetLocation,
}

impl XudpMessageStream {
    #[allow(
        dead_code,
        reason = "native-resolver default remains available to library and test callers; server XUDP paths use the configured resolver"
    )]
    pub fn new(inner_stream: Box<dyn AsyncStream>) -> Self {
        Self::new_with_resolver(inner_stream, Arc::new(NativeResolver::new()))
    }

    pub(crate) fn new_with_resolver(
        inner_stream: Box<dyn AsyncStream>,
        resolver: Arc<dyn crate::resolver::Resolver>,
    ) -> Self {
        Self {
            inner_stream,
            read_buffer: BytesMut::with_capacity(65536),
            read_filled: 0,
            write_buffer: BytesMut::with_capacity(65536),
            next_route_id: 1,
            wire_sessions: HashMap::new(),
            wire_session_order: VecDeque::new(),
            next_wire_session_generation: 0,
            routes: HashMap::new(),
            closed_routes: VecDeque::new(),
            resolver_cache: ResolverCache::new(resolver),
            incoming_message: None,
            is_eof: false,
        }
    }

    /// Feed initial unparsed data into the read buffer
    /// Used when VLESS header parsing read ahead into XUDP frame data
    pub fn feed_initial_read_data(&mut self, data: &[u8]) -> std::io::Result<()> {
        if data.is_empty() {
            return Ok(());
        }

        log::debug!(
            "[XUDP] Feeding {} bytes of initial data to read buffer",
            data.len()
        );
        let filled = self.read_filled;
        if self.read_buffer.len() - filled < data.len() {
            self.read_buffer.resize(filled + data.len(), 0);
        }
        self.read_buffer[filled..filled + data.len()].copy_from_slice(data);
        self.read_filled = filled + data.len();
        Ok(())
    }

    fn allocate_route_id(&mut self) -> std::io::Result<MessageSessionId> {
        let id = self.next_route_id;
        self.next_route_id = self.next_route_id.checked_add(1).ok_or_else(|| {
            std::io::Error::other("XUDP internal session identifier space exhausted")
        })?;
        Ok(id)
    }

    fn close_wire_session(&mut self, wire_session_id: u16) {
        let Some(session) = self.wire_sessions.remove(&wire_session_id) else {
            return;
        };
        for route_id in session.routes.into_values() {
            self.routes.remove(&route_id);
            self.closed_routes.push_back(route_id);
        }
    }

    fn start_wire_session(&mut self, wire_session_id: u16, destination: NetLocation) {
        self.close_wire_session(wire_session_id);

        // Evict the oldest live session while the cap is reached. The queue can
        // hold stale entries (replaced or closed sessions); a stale entry is
        // skipped without closing a replacement that reused the same ID.
        while self.wire_sessions.len() >= MAX_XUDP_SESSIONS {
            let Some((oldest_id, generation)) = self.wire_session_order.pop_front() else {
                log::warn!(
                    "[XUDP SESSION] Wire session limit reached but no live session can be evicted; dropping new session {}",
                    wire_session_id
                );
                return;
            };
            let is_live = self
                .wire_sessions
                .get(&oldest_id)
                .is_some_and(|session| session.generation == generation);
            if is_live {
                log::debug!(
                    "[XUDP SESSION] Evicting oldest wire session {} at the {}-session limit",
                    oldest_id,
                    MAX_XUDP_SESSIONS
                );
                self.close_wire_session(oldest_id);
            }
        }

        let generation = self.next_wire_session_generation;
        self.next_wire_session_generation = generation.wrapping_add(1);
        // Stale entries are normally skipped by eviction, but a peer that keeps
        // reopening sessions below the cap would grow the queue forever.
        // Compacting once the queue outgrows the cap keeps it bounded
        // (amortized O(1) per session start) without changing eviction order.
        if self.wire_session_order.len() > MAX_XUDP_SESSIONS {
            self.wire_session_order.retain(|(id, generation)| {
                self.wire_sessions
                    .get(id)
                    .is_some_and(|session| session.generation == *generation)
            });
        }
        self.wire_sessions.insert(
            wire_session_id,
            WireSession {
                default_destination: destination,
                routes: HashMap::new(),
                generation,
            },
        );
        self.wire_session_order
            .push_back((wire_session_id, generation));
    }

    fn get_or_create_route(
        &mut self,
        wire_session_id: u16,
        original_destination: &NetLocation,
    ) -> std::io::Result<Option<MessageSessionId>> {
        let Some(session) = self.wire_sessions.get(&wire_session_id) else {
            return Ok(None);
        };
        if let Some(route_id) = session.routes.get(original_destination) {
            return Ok(Some(*route_id));
        }
        if self.routes.len() >= MAX_XUDP_ROUTES {
            log::warn!(
                "[XUDP SESSION READ] Dropping packet for new destination {} after reaching the {}-route limit",
                original_destination,
                MAX_XUDP_ROUTES
            );
            return Ok(None);
        }

        let route_id = self.allocate_route_id()?;
        self.wire_sessions
            .get_mut(&wire_session_id)
            .expect("wire session disappeared during route allocation")
            .routes
            .insert(original_destination.clone(), route_id);
        self.routes.insert(
            route_id,
            RoutedSession {
                wire_session_id,
                original_destination: original_destination.clone(),
            },
        );
        Ok(Some(route_id))
    }

    /// Try to decode one complete XUDP frame from the read buffer.
    ///
    /// This function must NOT consume any bytes from the buffer unless
    /// it successfully decodes a complete frame. Otherwise, partial frames would
    /// be lost when the function is called again with more data.
    ///
    /// Returns:
    ///   Ok(Some((data, destination, session_id))) - Successfully decoded a complete frame
    ///   Ok(None) - Buffer doesn't contain a complete frame yet (need more data)
    ///   Err(e) - Error during decoding
    fn try_decode_one_frame(&mut self) -> std::io::Result<Option<(Bytes, NetLocation, u16)>> {
        loop {
            log::debug!(
                "[XUDP READ] Attempting to decode frame, buffer len: {}",
                self.read_filled
            );

            // First, peek at the buffer to determine total frame size WITHOUT consuming anything.
            // We need to check: metadata_len (2 bytes) + metadata + data_len (2 bytes) + data

            // Need at least 2 bytes for metadata length
            if self.read_filled < 2 {
                log::debug!("[XUDP READ] Buffer too short for metadata length field");
                return Ok(None);
            }

            let metadata_len =
                u16::from_be_bytes([self.read_buffer[0], self.read_buffer[1]]) as usize;

            // Check if we have complete metadata
            if self.read_filled < 2 + metadata_len {
                log::debug!("[XUDP READ] Buffer too short for complete metadata");
                return Ok(None);
            }

            // Peek at metadata to check if frame has data
            if metadata_len < 4 {
                return Err(std::io::Error::new(
                    std::io::ErrorKind::InvalidData,
                    format!("metadata too short: {}", metadata_len),
                ));
            }

            let option_byte = self.read_buffer[2 + 3];
            let has_data = option_byte & FrameOption::DATA != 0;
            let data_len_offset = 2 + metadata_len;
            if has_data && self.read_filled < data_len_offset + 2 {
                log::debug!("[XUDP READ] Buffer too short for data length field");
                return Ok(None);
            }

            let data_len = if has_data {
                u16::from_be_bytes([
                    self.read_buffer[data_len_offset],
                    self.read_buffer[data_len_offset + 1],
                ]) as usize
            } else {
                0
            };

            let total_frame_len = data_len_offset + usize::from(has_data) * (2 + data_len);
            if self.read_filled < total_frame_len {
                log::debug!(
                    "[XUDP READ] Buffer too short for complete frame: have {}, need {}",
                    self.read_filled,
                    total_frame_len
                );
                return Ok(None);
            }

            let metadata = FrameMetadata::decode(&mut self.read_buffer)?
                .expect("metadata decode should succeed after length check");
            // Decode consumed exactly the declared metadata envelope.
            self.read_filled -= 2 + metadata_len;

            log::debug!(
                "[XUDP READ] Decoded frame: session_id={}, status={:?}, has_data={}, target={:?}, network={:?}",
                metadata.session_id,
                metadata.status,
                metadata.option.has_data(),
                metadata.target,
                metadata.network
            );

            if has_data {
                self.read_buffer.advance(2);
                self.read_filled -= 2;
            }
            let data = self.read_buffer.split_to(data_len).freeze();
            self.read_filled -= data_len;

            if metadata.status == SessionStatus::End {
                self.close_wire_session(metadata.session_id);
                continue;
            }

            if metadata.status == SessionStatus::KeepAlive {
                continue;
            }

            if let Some(TargetNetwork::Tcp) = metadata.network {
                return Err(std::io::Error::new(
                    std::io::ErrorKind::Unsupported,
                    "XUDP with TCP destinations is not supported. Only UDP destinations are supported.",
                ));
            }

            // Check for ERROR option bit - remote side is signaling an error
            if metadata.option.has_error() {
                log::error!(
                    "[XUDP READ] Received frame with ERROR option set for session {}",
                    metadata.session_id
                );
                return Err(std::io::Error::new(
                    std::io::ErrorKind::ConnectionReset,
                    "XUDP session closed by remote with error",
                ));
            }

            let destination = match metadata.status {
                SessionStatus::New => {
                    let target = metadata.target.ok_or_else(|| {
                        std::io::Error::new(
                            std::io::ErrorKind::InvalidData,
                            "XUDP New frame is missing its destination",
                        )
                    })?;
                    self.start_wire_session(metadata.session_id, target.clone());
                    target
                }
                SessionStatus::Keep => {
                    let Some(session) = self.wire_sessions.get(&metadata.session_id) else {
                        log::warn!(
                            "[XUDP READ] Ignoring data for unknown session {}",
                            metadata.session_id
                        );
                        continue;
                    };
                    metadata
                        .target
                        .unwrap_or_else(|| session.default_destination.clone())
                }
                SessionStatus::End | SessionStatus::KeepAlive => unreachable!(),
            };

            // A data section of length zero is an empty datagram, not a control
            // frame: only a frame without the DATA option carries no message.
            if !has_data {
                continue;
            }

            log::debug!(
                "[XUDP READ] Decoded complete frame with {} bytes for destination {}",
                data.len(),
                destination
            );
            return Ok(Some((data, destination, metadata.session_id)));
        }
    }
}

impl AsyncFlushMessage for XudpMessageStream {
    fn poll_flush_message(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<std::io::Result<()>> {
        let this = self.get_mut();

        while !this.write_buffer.is_empty() {
            let n = ready!(Pin::new(&mut this.inner_stream).poll_write(cx, &this.write_buffer))?;
            if n == 0 {
                return Poll::Ready(Err(std::io::Error::new(
                    std::io::ErrorKind::WriteZero,
                    "XUDP inner stream wrote zero bytes for non-empty buffered output",
                )));
            }
            this.write_buffer.advance(n);
        }

        Pin::new(&mut this.inner_stream).poll_flush(cx)
    }
}

impl AsyncShutdownMessage for XudpMessageStream {
    fn poll_shutdown_message(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
    ) -> Poll<std::io::Result<()>> {
        ready!(self.as_mut().poll_flush_message(cx))?;
        Pin::new(&mut self.get_mut().inner_stream).poll_shutdown(cx)
    }
}

impl AsyncPing for XudpMessageStream {
    fn supports_ping(&self) -> bool {
        self.inner_stream.supports_ping()
    }

    fn poll_write_ping(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<std::io::Result<bool>> {
        Pin::new(&mut self.get_mut().inner_stream).poll_write_ping(cx)
    }
}

impl AsyncReadSessionMessage for XudpMessageStream {
    fn poll_read_session_message(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut ReadBuf<'_>,
    ) -> Poll<std::io::Result<(MessageSessionId, SocketAddr)>> {
        let this = self.get_mut();

        if this.is_eof && this.incoming_message.is_none() {
            return Poll::Ready(Err(std::io::Error::new(
                std::io::ErrorKind::UnexpectedEof,
                "EOF reached",
            )));
        }

        loop {
            // Resume DNS without a separate delivery path: immediate and delayed
            // resolution failures must have the same datagram-local semantics.
            let message = if let Some(message) = this.incoming_message.take() {
                Some(message)
            } else {
                this.try_decode_one_frame()?
            };
            match message {
                Some((data, destination, wire_session_id)) => {
                    // Do not resolve new destinations once route admission is
                    // exhausted. Existing routes remain usable at the limit.
                    if this.routes.len() >= MAX_XUDP_ROUTES
                        && !this
                            .wire_sessions
                            .get(&wire_session_id)
                            .is_some_and(|session| session.routes.contains_key(&destination))
                    {
                        log::warn!("[XUDP SESSION READ] Dropping datagram at the route limit");
                        ready!(tokio::task::coop::poll_proceed(cx)).made_progress();
                        continue;
                    }

                    let socket_addr =
                        match this.resolver_cache.poll_resolve_location(cx, &destination) {
                            Poll::Ready(Ok(addr)) => addr,
                            Poll::Ready(Err(error)) => {
                                // DNS failure is not transport EOF. Drop only
                                // this datagram; keep all wire sessions/routes
                                // so later packets may retry or reach other peers.
                                log::debug!(
                                    "[XUDP SESSION READ] Dropping datagram after DNS failure: {:?}",
                                    error.kind()
                                );
                                // A burst of immediately rejected names must
                                // yield rather than monopolize the runtime.
                                ready!(tokio::task::coop::poll_proceed(cx)).made_progress();
                                continue;
                            }
                            Poll::Pending => {
                                this.incoming_message = Some((data, destination, wire_session_id));
                                return Poll::Pending;
                            }
                        };

                    // Reserve routes only for resolved, deliverable packets.
                    // Failed names must not consume the bounded route table.
                    if data.len() > buf.remaining() {
                        this.incoming_message = Some((data, destination, wire_session_id));
                        return Poll::Ready(Err(std::io::Error::new(
                            std::io::ErrorKind::InvalidData,
                            "buffer too small for incoming message",
                        )));
                    }
                    let Some(session_id) =
                        this.get_or_create_route(wire_session_id, &destination)?
                    else {
                        log::warn!(
                            "[XUDP SESSION READ] Dropping data without an admitted route for session {}",
                            wire_session_id
                        );
                        continue;
                    };

                    let resolved_destination = match socket_addr {
                        SocketAddr::V4(addr) => {
                            NetLocation::new(Address::Ipv4(*addr.ip()), addr.port())
                        }
                        SocketAddr::V6(addr) => {
                            NetLocation::new(Address::Ipv6(*addr.ip()), addr.port())
                        }
                    };
                    log::debug!(
                        "[XUDP SESSION READ] Resolved {} -> {}",
                        destination,
                        resolved_destination
                    );

                    log::debug!(
                        "[XUDP SESSION READ] Session {} mapped to {}",
                        session_id,
                        resolved_destination
                    );

                    buf.put_slice(&data);
                    log::debug!(
                        "[XUDP SESSION READ] Returning {} bytes for session {} to {}",
                        data.len(),
                        session_id,
                        socket_addr
                    );
                    return Poll::Ready(Ok((session_id, socket_addr)));
                }
                None => {
                    // Buffer doesn't have a complete frame, need to read more data
                }
            }

            // Read more data from the inner stream into the initialized spare
            // capacity. The buffer is zero-filled once when it grows, not on
            // every read.
            let filled = this.read_filled;
            if this.read_buffer.len() - filled < 8192 {
                this.read_buffer.resize(filled + 8192, 0);
            }
            let mut temp_buf = ReadBuf::new(&mut this.read_buffer[filled..filled + 8192]);

            log::debug!(
                "[XUDP SESSION READ] Reading from inner stream, current buffer has {} bytes",
                filled
            );
            let poll_result = Pin::new(&mut this.inner_stream).poll_read(cx, &mut temp_buf);

            let n = temp_buf.filled().len();
            this.read_filled = filled + n;

            match ready!(poll_result) {
                Ok(()) => {
                    log::debug!(
                        "[XUDP SESSION READ] Got {} bytes from inner stream (total buffer: {})",
                        n,
                        this.read_filled
                    );

                    if n == 0 {
                        this.is_eof = true;
                        return Poll::Ready(Err(std::io::Error::new(
                            std::io::ErrorKind::UnexpectedEof,
                            "EOF reached",
                        )));
                    }

                    // Got new data, continue loop to try decoding again
                    continue;
                }
                Err(e) => {
                    log::error!("[XUDP SESSION READ] Error reading from inner stream: {}", e);
                    return Poll::Ready(Err(e));
                }
            }
        }
    }

    fn take_closed_session(&mut self) -> Option<MessageSessionId> {
        self.closed_routes.pop_front()
    }
}

impl AsyncWriteSessionMessage for XudpMessageStream {
    fn poll_write_session_message(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        session_id: MessageSessionId,
        buf: &[u8],
        target: &SocketAddr,
    ) -> Poll<std::io::Result<()>> {
        // This is the reverse direction: UDP response from internet → XUDP client
        // Use original destination (may be hostname) in response frame, NOT resolved IP

        log::debug!(
            "[XUDP SESSION WRITE] Writing {} bytes for session {} from source {}",
            buf.len(),
            session_id,
            target
        );

        // Flush any pending write buffer first
        if !self.write_buffer.is_empty() {
            ready!(self.as_mut().poll_flush_message(cx))?;
        }

        let Some(route) = self.routes.get(&session_id) else {
            log::debug!(
                "[XUDP SESSION WRITE] Discarding response for closed route {}",
                session_id
            );
            return Poll::Ready(Ok(()));
        };
        let wire_session_id = route.wire_session_id;
        log::debug!(
            "[XUDP SESSION WRITE] Using original destination {} for session {} (response came from {})",
            route.original_destination,
            wire_session_id,
            target
        );
        // The borrow of `route` ends here; a single clone carries the
        // destination into the frame metadata below.
        let target_location = route.original_destination.clone();

        log::debug!(
            "[XUDP SESSION WRITE] Encoding {:?} frame: session_id={}, target={}, data_len={}",
            SessionStatus::Keep,
            wire_session_id,
            target_location,
            buf.len()
        );

        let metadata = FrameMetadata {
            session_id: wire_session_id,
            status: SessionStatus::Keep,
            option: FrameOption::new().with_data(),
            target: Some(target_location),
            network: Some(TargetNetwork::Udp),
        };

        // Encode metadata
        metadata.encode(&mut self.write_buffer)?;

        // Write data length
        self.write_buffer.put_u16(buf.len() as u16);

        // Write data
        self.write_buffer.extend_from_slice(buf);

        Poll::Ready(Ok(()))
    }
}

impl AsyncSessionMessageStream for XudpMessageStream {}

#[cfg(test)]
mod tests {
    use super::*;
    use futures::task::noop_waker;
    use std::collections::HashSet;
    use std::io;
    use std::sync::Mutex;

    struct PendingOnceWriter {
        output: Arc<Mutex<Vec<u8>>>,
        pending: bool,
    }

    impl AsyncRead for PendingOnceWriter {
        fn poll_read(
            self: Pin<&mut Self>,
            _cx: &mut Context<'_>,
            _buf: &mut ReadBuf<'_>,
        ) -> Poll<io::Result<()>> {
            Poll::Pending
        }
    }

    impl AsyncWrite for PendingOnceWriter {
        fn poll_write(
            mut self: Pin<&mut Self>,
            cx: &mut Context<'_>,
            buf: &[u8],
        ) -> Poll<io::Result<usize>> {
            if self.pending {
                self.pending = false;
                cx.waker().wake_by_ref();
                return Poll::Pending;
            }
            self.output.lock().unwrap().extend_from_slice(buf);
            Poll::Ready(Ok(buf.len()))
        }

        fn poll_flush(self: Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<io::Result<()>> {
            Poll::Ready(Ok(()))
        }

        fn poll_shutdown(self: Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<io::Result<()>> {
            Poll::Ready(Ok(()))
        }
    }

    impl AsyncPing for PendingOnceWriter {
        fn supports_ping(&self) -> bool {
            false
        }

        fn poll_write_ping(self: Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<io::Result<bool>> {
            Poll::Ready(Ok(false))
        }
    }

    impl AsyncStream for PendingOnceWriter {}

    /// Mock writer with a fixed write-call budget.
    ///
    /// After the scripted steps are exhausted, the next call returns an injected
    /// error. This keeps the zero-progress regression tests from looping forever
    /// when the WriteZero guard is absent.
    struct BudgetedWriter {
        output: Arc<Mutex<Vec<u8>>>,
        calls: Arc<Mutex<WriterCalls>>,
        write_budget: usize,
        steps: VecDeque<WriteStep>,
    }

    /// Test-side views into a `BudgetedWriter` that stays inside the stream.
    struct BudgetedWriterHandles {
        output: Arc<Mutex<Vec<u8>>>,
        calls: Arc<Mutex<WriterCalls>>,
    }

    enum WriteStep {
        /// Accept at most this many bytes from the offered buffer.
        WriteBytes(usize),
        /// Report Ok(0), the zero-progress behavior under test.
        WriteZero,
    }

    #[derive(Default)]
    struct WriterCalls {
        writes: usize,
        flushes: usize,
    }

    impl AsyncRead for BudgetedWriter {
        fn poll_read(
            self: Pin<&mut Self>,
            _cx: &mut Context<'_>,
            _buf: &mut ReadBuf<'_>,
        ) -> Poll<io::Result<()>> {
            Poll::Pending
        }
    }

    impl AsyncWrite for BudgetedWriter {
        fn poll_write(
            mut self: Pin<&mut Self>,
            _cx: &mut Context<'_>,
            buf: &[u8],
        ) -> Poll<io::Result<usize>> {
            let mut calls = self.calls.lock().unwrap();
            calls.writes += 1;
            let call_index = calls.writes;
            drop(calls);

            match self.steps.pop_front() {
                Some(WriteStep::WriteBytes(count)) => {
                    let n = count.min(buf.len());
                    self.output.lock().unwrap().extend_from_slice(&buf[..n]);
                    return Poll::Ready(Ok(n));
                }
                Some(WriteStep::WriteZero) => return Poll::Ready(Ok(0)),
                None => {}
            }

            if call_index > self.write_budget {
                return Poll::Ready(Err(io::Error::other(
                    "BudgetedWriter write-call budget exceeded",
                )));
            }

            self.output.lock().unwrap().extend_from_slice(buf);
            Poll::Ready(Ok(buf.len()))
        }

        fn poll_flush(self: Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<io::Result<()>> {
            self.calls.lock().unwrap().flushes += 1;
            Poll::Ready(Ok(()))
        }

        fn poll_shutdown(self: Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<io::Result<()>> {
            Poll::Ready(Ok(()))
        }
    }

    impl AsyncPing for BudgetedWriter {
        fn supports_ping(&self) -> bool {
            false
        }

        fn poll_write_ping(self: Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<io::Result<bool>> {
            Poll::Ready(Ok(false))
        }
    }

    impl AsyncStream for BudgetedWriter {}

    /// Inner stream that serves scripted XUDP frame bytes to the reader.
    struct FrameReader {
        bytes: bytes::Bytes,
        /// Maximum bytes handed out per poll; `usize::MAX` means no limit.
        max_chunk: usize,
    }

    impl AsyncRead for FrameReader {
        fn poll_read(
            self: Pin<&mut Self>,
            _cx: &mut Context<'_>,
            buf: &mut ReadBuf<'_>,
        ) -> Poll<io::Result<()>> {
            let this = self.get_mut();
            if this.bytes.is_empty() {
                // A stream with nothing left to send stays open.
                return Poll::Pending;
            }
            let len = buf.remaining().min(this.bytes.len()).min(this.max_chunk);
            buf.put_slice(&this.bytes[..len]);
            this.bytes.advance(len);
            Poll::Ready(Ok(()))
        }
    }

    impl AsyncWrite for FrameReader {
        fn poll_write(
            self: Pin<&mut Self>,
            _cx: &mut Context<'_>,
            buf: &[u8],
        ) -> Poll<io::Result<usize>> {
            Poll::Ready(Ok(buf.len()))
        }

        fn poll_flush(self: Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<io::Result<()>> {
            Poll::Ready(Ok(()))
        }

        fn poll_shutdown(self: Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<io::Result<()>> {
            Poll::Ready(Ok(()))
        }
    }

    impl AsyncPing for FrameReader {
        fn supports_ping(&self) -> bool {
            false
        }

        fn poll_write_ping(self: Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<io::Result<bool>> {
            Poll::Ready(Ok(false))
        }
    }

    impl AsyncStream for FrameReader {}

    /// Build one XUDP frame: metadata plus an optional data section.
    fn xudp_frame(
        session_id: u16,
        status: SessionStatus,
        option: FrameOption,
        target: Option<NetLocation>,
        data: Option<&[u8]>,
    ) -> bytes::BytesMut {
        let network = target.as_ref().map(|_| TargetNetwork::Udp);
        let metadata = FrameMetadata {
            session_id,
            status,
            option,
            target,
            network,
        };
        let mut frame = bytes::BytesMut::new();
        metadata.encode(&mut frame).unwrap();
        if option.has_data() {
            let data = data.unwrap_or(&[]);
            frame.put_u16(data.len() as u16);
            frame.extend_from_slice(data);
        }
        frame
    }

    #[derive(Debug)]
    struct FailingResolver {
        calls: std::sync::atomic::AtomicUsize,
        failures: usize,
        pending_once: bool,
        error_kind: io::ErrorKind,
    }

    impl crate::resolver::Resolver for FailingResolver {
        fn resolve_location(
            &self,
            location: &NetLocation,
        ) -> Pin<Box<dyn std::future::Future<Output = io::Result<Vec<SocketAddr>>> + Send>>
        {
            let fail = self
                .calls
                .fetch_add(1, std::sync::atomic::Ordering::Relaxed)
                < self.failures;
            let error_kind = self.error_kind;
            let address = SocketAddr::from(([127, 0, 0, 1], location.port()));
            let mut pending = self.pending_once;
            Box::pin(std::future::poll_fn(move |cx| {
                if pending {
                    pending = false;
                    cx.waker().wake_by_ref();
                    return Poll::Pending;
                }
                Poll::Ready(if fail {
                    Err(io::Error::new(error_kind, "injected DNS failure"))
                } else {
                    Ok(vec![address])
                })
            }))
        }
    }

    fn dns_failure_stream(
        frames: BytesMut,
        failures: usize,
        pending_once: bool,
        error_kind: io::ErrorKind,
    ) -> (XudpMessageStream, Arc<FailingResolver>) {
        let resolver = Arc::new(FailingResolver {
            calls: std::sync::atomic::AtomicUsize::new(0),
            failures,
            pending_once,
            error_kind,
        });
        let mut stream = XudpMessageStream::new_with_resolver(
            Box::new(FrameReader {
                bytes: Bytes::new(),
                max_chunk: usize::MAX,
            }),
            resolver.clone(),
        );
        stream.feed_initial_read_data(&frames).unwrap();
        (stream, resolver)
    }

    #[test]
    fn dns_failure_drops_only_the_datagram_not_existing_routes() {
        let healthy = NetLocation::new(Address::Ipv4("127.0.0.1".parse().unwrap()), 1234);
        let bad = NetLocation::new(Address::Hostname("failure.test".into()), 53);
        let mut frames = xudp_frame(
            11,
            SessionStatus::New,
            FrameOption::new().with_data(),
            Some(healthy.clone()),
            Some(b"before"),
        );
        frames.extend_from_slice(&xudp_frame(
            11,
            SessionStatus::Keep,
            FrameOption::new().with_data(),
            Some(bad.clone()),
            Some(b"drop"),
        ));
        frames.extend_from_slice(&xudp_frame(
            11,
            SessionStatus::Keep,
            FrameOption::new().with_data(),
            None,
            Some(b"after"),
        ));
        frames.extend_from_slice(&xudp_frame(
            22,
            SessionStatus::New,
            FrameOption::new().with_data(),
            Some(NetLocation::new(
                Address::Ipv6("::1".parse().unwrap()),
                4321,
            )),
            Some(b"other"),
        ));
        let (mut stream, _) = dns_failure_stream(frames, 1, false, io::ErrorKind::NotFound);
        let waker = noop_waker();
        let mut cx = Context::from_waker(&waker);
        let mut bytes = [0; 64];
        let mut buf = ReadBuf::new(&mut bytes);
        let first = Pin::new(&mut stream).poll_read_session_message(&mut cx, &mut buf);
        let Poll::Ready(Ok((route, _))) = first else {
            panic!("expected first datagram");
        };
        buf.clear();
        assert!(matches!(
            Pin::new(&mut stream).poll_read_session_message(&mut cx, &mut buf),
            Poll::Ready(Ok((id, _))) if id == route
        ));
        assert_eq!(buf.filled(), b"after");
        assert_eq!(
            stream.routes.len(),
            1,
            "failed DNS must not reserve a route"
        );
        assert!(!stream.wire_sessions[&11].routes.contains_key(&bad));
        assert_eq!(stream.take_closed_session(), None);
        buf.clear();
        assert!(matches!(
            Pin::new(&mut stream).poll_read_session_message(&mut cx, &mut buf),
            Poll::Ready(Ok((_, address))) if address == "[::1]:4321".parse::<SocketAddr>().unwrap()
        ));
        assert_eq!(buf.filled(), b"other");
    }

    #[test]
    fn pending_dns_failure_and_timeout_preserve_the_following_datagram() {
        for error_kind in [io::ErrorKind::NotFound, io::ErrorKind::TimedOut] {
            let mut frames = xudp_frame(
                33,
                SessionStatus::New,
                FrameOption::new().with_data(),
                Some(NetLocation::new(
                    Address::Hostname("failure.test".into()),
                    53,
                )),
                Some(b"drop"),
            );
            frames.extend_from_slice(&xudp_frame(
                11,
                SessionStatus::New,
                FrameOption::new().with_data(),
                Some(NetLocation::new(
                    Address::Ipv4("127.0.0.1".parse().unwrap()),
                    1234,
                )),
                Some(b"after"),
            ));
            let (mut stream, _) = dns_failure_stream(frames, 1, true, error_kind);
            let waker = noop_waker();
            let mut cx = Context::from_waker(&waker);
            let mut bytes = [0; 64];
            let mut buf = ReadBuf::new(&mut bytes);
            assert!(
                Pin::new(&mut stream)
                    .poll_read_session_message(&mut cx, &mut buf)
                    .is_pending()
            );
            assert!(stream.incoming_message.is_some());
            assert!(
                stream.routes.is_empty(),
                "pending DNS must not reserve a route"
            );
            assert!(matches!(
                Pin::new(&mut stream).poll_read_session_message(&mut cx, &mut buf),
                Poll::Ready(Ok(_))
            ));
            assert_eq!(buf.filled(), b"after");
            assert!(stream.wire_sessions[&33].routes.is_empty());
            assert_eq!(stream.routes.len(), 1);
            assert_eq!(stream.take_closed_session(), None);
        }
    }

    #[test]
    fn dns_failure_can_retry_on_the_same_wire_session() {
        let target = NetLocation::new(Address::Hostname("retry.test".into()), 1234);
        let mut frames = xudp_frame(
            11,
            SessionStatus::New,
            FrameOption::new().with_data(),
            Some(target),
            Some(b"drop"),
        );
        frames.extend_from_slice(&xudp_frame(
            11,
            SessionStatus::Keep,
            FrameOption::new().with_data(),
            None,
            Some(b"retry"),
        ));
        let (mut stream, resolver) = dns_failure_stream(frames, 1, false, io::ErrorKind::NotFound);
        let waker = noop_waker();
        let mut cx = Context::from_waker(&waker);
        let mut bytes = [0; 64];
        let mut buf = ReadBuf::new(&mut bytes);
        assert!(matches!(
            Pin::new(&mut stream).poll_read_session_message(&mut cx, &mut buf),
            Poll::Ready(Ok(_))
        ));
        assert_eq!(buf.filled(), b"retry");
        assert_eq!(resolver.calls.load(std::sync::atomic::Ordering::Relaxed), 2);
        assert_eq!(stream.wire_sessions.len(), 1);
        assert_eq!(stream.routes.len(), 1);
    }

    #[test]
    fn full_route_table_skips_new_dns_work_but_keeps_existing_routes() {
        let healthy = NetLocation::new(Address::Ipv4("127.0.0.1".parse().unwrap()), 1);
        let mut frames = xudp_frame(
            22,
            SessionStatus::New,
            FrameOption::new().with_data(),
            Some(NetLocation::new(
                Address::Hostname("failure.test".into()),
                53,
            )),
            Some(b"drop"),
        );
        frames.extend_from_slice(&xudp_frame(
            11,
            SessionStatus::Keep,
            FrameOption::new().with_data(),
            None,
            Some(b"after"),
        ));
        let (mut stream, resolver) = dns_failure_stream(frames, 1, false, io::ErrorKind::NotFound);
        stream.start_wire_session(11, healthy);
        let mut first_route = None;
        for port in 1..=MAX_XUDP_ROUTES as u16 {
            let target = NetLocation::new(Address::Ipv4("127.0.0.1".parse().unwrap()), port);
            let route = stream.get_or_create_route(11, &target).unwrap();
            if port == 1 {
                first_route = route;
            }
        }
        let waker = noop_waker();
        let mut cx = Context::from_waker(&waker);
        let mut bytes = [0; 64];
        let mut buf = ReadBuf::new(&mut bytes);
        assert!(matches!(
            Pin::new(&mut stream).poll_read_session_message(&mut cx, &mut buf),
            Poll::Ready(Ok((id, _))) if Some(id) == first_route
        ));
        assert_eq!(buf.filled(), b"after");
        assert_eq!(resolver.calls.load(std::sync::atomic::Ordering::Relaxed), 0);
        assert_eq!(stream.routes.len(), MAX_XUDP_ROUTES);
    }

    #[tokio::test]
    async fn repeated_dns_failures_yield_without_exhausting_routes() {
        tokio::spawn(async {
            let failures = MAX_XUDP_ROUTES + 32;
            let mut frames = BytesMut::new();
            for index in 0..failures {
                frames.extend_from_slice(&xudp_frame(
                    11,
                    if index == 0 {
                        SessionStatus::New
                    } else {
                        SessionStatus::Keep
                    },
                    FrameOption::new().with_data(),
                    Some(NetLocation::new(
                        Address::Hostname(format!("failure-{index}.test")),
                        53,
                    )),
                    Some(b"drop"),
                ));
            }
            frames.extend_from_slice(&xudp_frame(
                11,
                SessionStatus::Keep,
                FrameOption::new().with_data(),
                Some(NetLocation::new(
                    Address::Ipv4("127.0.0.1".parse().unwrap()),
                    1234,
                )),
                Some(b"after"),
            ));
            let (mut stream, resolver) =
                dns_failure_stream(frames, failures, false, io::ErrorKind::NotFound);
            let mut bytes = [0; 64];
            let mut buf = ReadBuf::new(&mut bytes);
            {
                let waker = noop_waker();
                let mut cx = Context::from_waker(&waker);
                assert!(
                    Pin::new(&mut stream)
                        .poll_read_session_message(&mut cx, &mut buf)
                        .is_pending()
                );
            }
            assert!(resolver.calls.load(std::sync::atomic::Ordering::Relaxed) < failures);
            let result = tokio::time::timeout(
                std::time::Duration::from_secs(1),
                std::future::poll_fn(|cx| {
                    Pin::new(&mut stream).poll_read_session_message(cx, &mut buf)
                }),
            )
            .await
            .unwrap()
            .unwrap();
            assert_eq!(result.1, "127.0.0.1:1234".parse::<SocketAddr>().unwrap());
            assert_eq!(buf.filled(), b"after");
            assert_eq!(
                resolver.calls.load(std::sync::atomic::Ordering::Relaxed),
                failures
            );
            assert_eq!(stream.routes.len(), 1);
            assert_eq!(stream.take_closed_session(), None);
        })
        .await
        .unwrap();
    }

    #[test]
    fn malformed_metadata_is_not_treated_as_a_dns_failure() {
        let (mut stream, resolver) = dns_failure_stream(
            BytesMut::from(&[0, 4, 0, 11, 0xff, 0][..]),
            0,
            false,
            io::ErrorKind::NotFound,
        );
        let waker = noop_waker();
        let mut cx = Context::from_waker(&waker);
        let mut bytes = [0; 64];
        let mut buf = ReadBuf::new(&mut bytes);
        assert!(matches!(
            Pin::new(&mut stream).poll_read_session_message(&mut cx, &mut buf),
            Poll::Ready(Err(error)) if error.kind() == io::ErrorKind::InvalidData
        ));
        assert_eq!(resolver.calls.load(std::sync::atomic::Ordering::Relaxed), 0);
    }

    #[test]
    fn end_with_error_closes_only_its_wire_session() {
        let target = NetLocation::new(Address::Ipv4("127.0.0.1".parse().unwrap()), 1234);
        let mut frames = BytesMut::new();
        for id in [11, 22] {
            frames.extend_from_slice(&xudp_frame(
                id,
                SessionStatus::New,
                FrameOption::new().with_data(),
                Some(target.clone()),
                Some(b"before"),
            ));
        }
        frames.extend_from_slice(&xudp_frame(
            22,
            SessionStatus::End,
            FrameOption::from(FrameOption::ERROR),
            None,
            None,
        ));
        frames.extend_from_slice(&xudp_frame(
            11,
            SessionStatus::Keep,
            FrameOption::new().with_data(),
            None,
            Some(b"after"),
        ));
        let (mut stream, _) = dns_failure_stream(frames, 0, false, io::ErrorKind::NotFound);
        let waker = noop_waker();
        let mut cx = Context::from_waker(&waker);
        let mut bytes = [0; 64];
        let mut buf = ReadBuf::new(&mut bytes);
        let mut routes = Vec::new();
        for _ in 0..2 {
            let Poll::Ready(Ok((route, _))) =
                Pin::new(&mut stream).poll_read_session_message(&mut cx, &mut buf)
            else {
                panic!("expected initial packet");
            };
            routes.push(route);
            buf.clear();
        }
        assert!(matches!(
            Pin::new(&mut stream).poll_read_session_message(&mut cx, &mut buf),
            Poll::Ready(Ok((id, _))) if id == routes[0]
        ));
        assert_eq!(buf.filled(), b"after");
        assert_eq!(stream.take_closed_session(), Some(routes[1]));
        assert_eq!(stream.take_closed_session(), None);
    }

    #[test]
    fn non_end_error_frames_keep_their_fatal_error_behavior() {
        let frames = xudp_frame(
            11,
            SessionStatus::Keep,
            FrameOption::from(FrameOption::ERROR),
            None,
            None,
        );
        let (mut stream, _) = dns_failure_stream(frames, 0, false, io::ErrorKind::NotFound);
        let waker = noop_waker();
        let mut cx = Context::from_waker(&waker);
        let mut bytes = [0; 64];
        let mut buf = ReadBuf::new(&mut bytes);
        assert!(matches!(
            Pin::new(&mut stream).poll_read_session_message(&mut cx, &mut buf),
            Poll::Ready(Err(error)) if error.kind() == io::ErrorKind::ConnectionReset
        ));
    }

    #[tokio::test]
    async fn transport_eof_is_not_treated_as_a_dns_failure() {
        let (inner, peer) = tokio::io::duplex(64);
        drop(peer);
        let mut stream = XudpMessageStream::new(Box::new(inner));
        let mut bytes = [0; 64];
        let mut buf = ReadBuf::new(&mut bytes);
        let error = std::future::poll_fn(|cx| {
            Pin::new(&mut stream).poll_read_session_message(cx, &mut buf)
        })
        .await
        .unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::UnexpectedEof);
    }

    #[test]
    fn a_zero_length_data_frame_is_delivered_as_an_empty_message() {
        let destination = NetLocation::new(Address::Ipv4("127.0.0.1".parse().unwrap()), 53);
        // HAS_DATA with a zero-length data section is an empty datagram, which is
        // not the same as a control frame without a data section.
        let frame = xudp_frame(
            10,
            SessionStatus::New,
            FrameOption::new().with_data(),
            Some(destination.clone()),
            Some(&[]),
        );
        let mut stream = XudpMessageStream::new(Box::new(FrameReader {
            bytes: frame.freeze(),
            max_chunk: usize::MAX,
        }));
        let waker = noop_waker();
        let mut cx = Context::from_waker(&waker);
        let mut bytes = [0u8; 64];
        let mut buf = ReadBuf::new(&mut bytes);

        match Pin::new(&mut stream).poll_read_session_message(&mut cx, &mut buf) {
            Poll::Ready(Ok((session_id, address))) => {
                assert!(
                    buf.filled().is_empty(),
                    "the empty datagram carries no bytes"
                );
                assert_eq!(address, "127.0.0.1:53".parse().unwrap());
                assert!(session_id > 0, "the frame must map to a routed session");
            }
            other => panic!("an empty data frame must be delivered, got {other:?}"),
        }
    }

    #[test]
    fn a_control_frame_without_data_is_still_skipped() {
        let destination = NetLocation::new(Address::Ipv4("127.0.0.1".parse().unwrap()), 53);
        // KeepAlive has no data section and stays a control frame.
        let frame = xudp_frame(
            10,
            SessionStatus::KeepAlive,
            FrameOption::new(),
            Some(destination),
            None,
        );
        let mut stream = XudpMessageStream::new(Box::new(FrameReader {
            bytes: frame.freeze(),
            max_chunk: usize::MAX,
        }));
        let waker = noop_waker();
        let mut cx = Context::from_waker(&waker);
        let mut bytes = [0u8; 64];
        let mut buf = ReadBuf::new(&mut bytes);

        assert!(
            Pin::new(&mut stream)
                .poll_read_session_message(&mut cx, &mut buf)
                .is_pending(),
            "a control frame must not be delivered as a message"
        );
    }

    #[test]
    fn a_frame_split_across_single_byte_reads_still_decodes() {
        // One byte per poll exercises partial-frame accumulation and the
        // logical buffer length that tracks it.
        let destination = NetLocation::new(Address::Ipv4("127.0.0.1".parse().unwrap()), 53);
        let frame = xudp_frame(
            10,
            SessionStatus::New,
            FrameOption::new().with_data(),
            Some(destination),
            Some(b"split"),
        );
        let mut stream = XudpMessageStream::new(Box::new(FrameReader {
            bytes: frame.freeze(),
            max_chunk: 1,
        }));
        let waker = noop_waker();
        let mut cx = Context::from_waker(&waker);
        let mut bytes = [0u8; 64];
        let mut buf = ReadBuf::new(&mut bytes);

        match Pin::new(&mut stream).poll_read_session_message(&mut cx, &mut buf) {
            Poll::Ready(Ok((session_id, address))) => {
                assert_eq!(buf.filled(), b"split");
                assert_eq!(address, "127.0.0.1:53".parse().unwrap());
                assert!(session_id > 0, "the frame must map to a routed session");
            }
            other => panic!("a trickled frame must decode, got {other:?}"),
        }
    }

    #[test]
    fn a_frame_larger_than_one_read_chunk_decodes_after_buffer_growth() {
        // A payload beyond the 8192-byte read chunk forces the read buffer to
        // grow while the frame is still incomplete.
        let destination = NetLocation::new(Address::Ipv4("127.0.0.1".parse().unwrap()), 53);
        let payload = vec![0x5Au8; 20_000];
        let frame = xudp_frame(
            11,
            SessionStatus::New,
            FrameOption::new().with_data(),
            Some(destination),
            Some(&payload),
        );
        let mut stream = XudpMessageStream::new(Box::new(FrameReader {
            bytes: frame.freeze(),
            max_chunk: usize::MAX,
        }));
        let waker = noop_waker();
        let mut cx = Context::from_waker(&waker);
        let mut bytes = vec![0u8; payload.len()];
        let mut buf = ReadBuf::new(&mut bytes);

        match Pin::new(&mut stream).poll_read_session_message(&mut cx, &mut buf) {
            Poll::Ready(Ok((session_id, address))) => {
                assert_eq!(buf.filled(), &payload[..]);
                assert_eq!(address, "127.0.0.1:53".parse().unwrap());
                assert!(session_id > 0, "the frame must map to a routed session");
            }
            other => panic!("a large frame must decode, got {other:?}"),
        }
    }

    fn budgeted_writer(
        write_budget: usize,
        steps: Vec<WriteStep>,
    ) -> (BudgetedWriter, BudgetedWriterHandles) {
        let handles = BudgetedWriterHandles {
            output: Arc::new(Mutex::new(Vec::new())),
            calls: Arc::new(Mutex::new(WriterCalls::default())),
        };
        let writer = BudgetedWriter {
            output: Arc::clone(&handles.output),
            calls: Arc::clone(&handles.calls),
            write_budget,
            steps: VecDeque::from(steps),
        };
        (writer, handles)
    }

    fn stream_with_buffered_response(writer: BudgetedWriter) -> (XudpMessageStream, Vec<u8>) {
        let mut stream = XudpMessageStream::new(Box::new(writer));
        let destination = NetLocation::new(Address::Ipv4("127.0.0.1".parse().unwrap()), 53);
        stream.start_wire_session(10, destination.clone());
        let route_id = stream
            .get_or_create_route(10, &destination)
            .unwrap()
            .unwrap();
        let source = "127.0.0.1:53".parse().unwrap();
        let waker = noop_waker();
        let mut cx = Context::from_waker(&waker);
        assert!(matches!(
            Pin::new(&mut stream).poll_write_session_message(&mut cx, route_id, b"x", &source),
            Poll::Ready(Ok(()))
        ));
        let frame = stream.write_buffer.to_vec();
        (stream, frame)
    }

    #[test]
    fn buffered_response_is_accepted_once_when_output_blocks() {
        let output = Arc::new(Mutex::new(Vec::new()));
        let inner = PendingOnceWriter {
            output: Arc::clone(&output),
            pending: true,
        };
        let mut stream = XudpMessageStream::new(Box::new(inner));
        let destination = NetLocation::new(Address::Ipv4("127.0.0.1".parse().unwrap()), 53);
        stream.start_wire_session(10, destination.clone());
        let route_id = stream
            .get_or_create_route(10, &destination)
            .unwrap()
            .unwrap();
        let source = "127.0.0.1:53".parse().unwrap();
        let waker = noop_waker();
        let mut cx = Context::from_waker(&waker);

        assert!(matches!(
            Pin::new(&mut stream).poll_write_session_message(&mut cx, route_id, b"x", &source),
            Poll::Ready(Ok(()))
        ));
        assert!(matches!(
            Pin::new(&mut stream).poll_flush_message(&mut cx),
            Poll::Pending
        ));
        assert!(matches!(
            Pin::new(&mut stream).poll_flush_message(&mut cx),
            Poll::Ready(Ok(()))
        ));
        assert_eq!(
            *output.lock().unwrap(),
            [0, 12, 0, 10, 2, 1, 2, 0, 53, 1, 127, 0, 0, 1, 0, 1, b'x']
        );
    }

    #[test]
    fn hostname_response_frame_preserves_original_destination_bytes() {
        let output = Arc::new(Mutex::new(Vec::new()));
        let inner = PendingOnceWriter {
            output: Arc::clone(&output),
            pending: false,
        };
        let mut stream = XudpMessageStream::new(Box::new(inner));
        let destination = NetLocation::new(Address::Hostname("example.com".to_string()), 443);
        stream.start_wire_session(10, destination.clone());
        let route_id = stream
            .get_or_create_route(10, &destination)
            .unwrap()
            .unwrap();
        let source = "127.0.0.1:53".parse().unwrap();
        let waker = noop_waker();
        let mut cx = Context::from_waker(&waker);

        assert!(matches!(
            Pin::new(&mut stream).poll_write_session_message(&mut cx, route_id, b"hi", &source),
            Poll::Ready(Ok(()))
        ));
        assert!(matches!(
            Pin::new(&mut stream).poll_flush_message(&mut cx),
            Poll::Ready(Ok(()))
        ));

        // The response must carry the original hostname destination, not the
        // resolved source address. The expected frame is written out by hand
        // so the check does not depend on the encoder under test.
        let mut expected: Vec<u8> = vec![
            0, 20, // metadata length
            0, 10, // wire session id
            2,  // SessionStatus::Keep
            1,  // FrameOption::DATA
            2,  // TargetNetwork::Udp
            1, 187, // port 443
            2,   // address type: hostname
            11,  // hostname length
        ];
        expected.extend_from_slice(b"example.com");
        expected.extend_from_slice(&[0, 2]); // data length
        expected.extend_from_slice(b"hi");

        let written = output.lock().unwrap().clone();
        assert_eq!(written, expected);
    }

    #[test]
    fn closed_routes_cannot_emit_or_reuse_internal_ids() {
        let output = Arc::new(Mutex::new(Vec::new()));
        let inner = PendingOnceWriter {
            output: Arc::clone(&output),
            pending: false,
        };
        let mut stream = XudpMessageStream::new(Box::new(inner));
        let destination = NetLocation::new(Address::Ipv4("127.0.0.1".parse().unwrap()), 53);
        stream.start_wire_session(0, destination.clone());
        let old_route = stream
            .get_or_create_route(0, &destination)
            .unwrap()
            .unwrap();
        stream.close_wire_session(0);
        assert_eq!(stream.take_closed_session(), Some(old_route));
        assert_eq!(stream.take_closed_session(), None);

        let source = "127.0.0.1:53".parse().unwrap();
        let waker = noop_waker();
        let mut cx = Context::from_waker(&waker);
        assert!(matches!(
            Pin::new(&mut stream).poll_write_session_message(&mut cx, old_route, b"late", &source),
            Poll::Ready(Ok(()))
        ));
        assert!(output.lock().unwrap().is_empty());

        stream.start_wire_session(0, destination.clone());
        let new_route = stream
            .get_or_create_route(0, &destination)
            .unwrap()
            .unwrap();
        assert_ne!(new_route, old_route);
    }

    #[test]
    fn route_limit_preserves_existing_routes_and_releases_closed_routes() {
        let output = Arc::new(Mutex::new(Vec::new()));
        let inner = PendingOnceWriter {
            output,
            pending: false,
        };
        let mut stream = XudpMessageStream::new(Box::new(inner));
        let address = Address::Ipv4("127.0.0.1".parse().unwrap());
        let first_destination = NetLocation::new(address.clone(), 1);
        stream.start_wire_session(10, first_destination.clone());
        let preserved_destination = NetLocation::new(address.clone(), u16::MAX);
        stream.start_wire_session(20, preserved_destination.clone());

        let first_route = stream
            .get_or_create_route(10, &first_destination)
            .unwrap()
            .unwrap();
        let preserved_route = stream
            .get_or_create_route(20, &preserved_destination)
            .unwrap()
            .unwrap();
        assert_eq!(
            stream.get_or_create_route(10, &first_destination).unwrap(),
            Some(first_route)
        );

        for port in 2..MAX_XUDP_ROUTES as u16 {
            let destination = NetLocation::new(address.clone(), port);
            assert!(
                stream
                    .get_or_create_route(10, &destination)
                    .unwrap()
                    .is_some()
            );
        }
        assert_eq!(stream.routes.len(), MAX_XUDP_ROUTES);
        assert_eq!(
            stream.get_or_create_route(10, &first_destination).unwrap(),
            Some(first_route)
        );
        assert_eq!(
            stream
                .get_or_create_route(20, &preserved_destination)
                .unwrap(),
            Some(preserved_route)
        );

        let rejected_destination = NetLocation::new(address.clone(), u16::MAX - 1);
        let error = stream
            .get_or_create_route(20, &rejected_destination)
            .unwrap();
        assert_eq!(error, None);
        assert_eq!(stream.routes.len(), MAX_XUDP_ROUTES);

        stream.close_wire_session(10);
        assert_eq!(stream.routes.len(), 1);
        assert!(stream.routes.contains_key(&preserved_route));
        let closed_routes: HashSet<_> =
            std::iter::from_fn(|| stream.take_closed_session()).collect();
        assert_eq!(closed_routes.len(), MAX_XUDP_ROUTES - 1);
        assert!(closed_routes.contains(&first_route));
        assert!(!closed_routes.contains(&preserved_route));
        assert_eq!(stream.take_closed_session(), None);

        assert!(
            stream
                .get_or_create_route(20, &rejected_destination)
                .unwrap()
                .is_some()
        );
    }

    #[test]
    fn wire_session_cap_evicts_the_oldest_session() {
        let destination = NetLocation::new(Address::Ipv4("127.0.0.1".parse().unwrap()), 53);
        // One more New frame than the cap allows, all control-only so the
        // stream processes every session without needing message delivery.
        let mut frames = bytes::BytesMut::new();
        for session_id in 0..=MAX_XUDP_SESSIONS as u16 {
            let frame = xudp_frame(
                session_id,
                SessionStatus::New,
                FrameOption::new(),
                Some(destination.clone()),
                None,
            );
            frames.extend_from_slice(&frame);
        }
        let mut stream = XudpMessageStream::new(Box::new(FrameReader {
            bytes: frames.freeze(),
            max_chunk: usize::MAX,
        }));
        let waker = noop_waker();
        let mut cx = Context::from_waker(&waker);
        let mut bytes = [0u8; 64];
        let mut buf = ReadBuf::new(&mut bytes);

        assert!(
            Pin::new(&mut stream)
                .poll_read_session_message(&mut cx, &mut buf)
                .is_pending(),
            "control-only New frames leave the stream waiting for data"
        );
        assert_eq!(stream.wire_sessions.len(), MAX_XUDP_SESSIONS);
        assert!(
            !stream.wire_sessions.contains_key(&0),
            "the oldest session must be evicted at the cap"
        );
        assert!(
            stream
                .wire_sessions
                .contains_key(&(MAX_XUDP_SESSIONS as u16)),
            "the newest session must be admitted"
        );
    }

    #[test]
    fn wire_session_eviction_closes_the_oldest_sessions_routes() {
        let mut stream = XudpMessageStream::new(Box::new(PendingOnceWriter {
            output: Arc::new(Mutex::new(Vec::new())),
            pending: false,
        }));
        let address = Address::Ipv4("127.0.0.1".parse().unwrap());
        let oldest_destination = NetLocation::new(address.clone(), 1);
        stream.start_wire_session(0, oldest_destination.clone());
        let oldest_route = stream
            .get_or_create_route(0, &oldest_destination)
            .unwrap()
            .unwrap();
        for session_id in 1..MAX_XUDP_SESSIONS as u16 {
            let destination = NetLocation::new(address.clone(), session_id + 1);
            stream.start_wire_session(session_id, destination);
        }
        assert_eq!(stream.wire_sessions.len(), MAX_XUDP_SESSIONS);
        assert_eq!(stream.take_closed_session(), None);

        let new_destination = NetLocation::new(address.clone(), u16::MAX);
        stream.start_wire_session(MAX_XUDP_SESSIONS as u16, new_destination);

        assert_eq!(stream.wire_sessions.len(), MAX_XUDP_SESSIONS);
        assert!(!stream.wire_sessions.contains_key(&0));
        assert_eq!(
            stream.take_closed_session(),
            Some(oldest_route),
            "the evicted session's routes must surface as closed"
        );
        assert_eq!(stream.take_closed_session(), None);
    }

    #[test]
    fn refreshing_a_wire_session_id_does_not_evict_other_sessions() {
        let mut stream = XudpMessageStream::new(Box::new(PendingOnceWriter {
            output: Arc::new(Mutex::new(Vec::new())),
            pending: false,
        }));
        let address = Address::Ipv4("127.0.0.1".parse().unwrap());
        for session_id in 0..MAX_XUDP_SESSIONS as u16 {
            let destination = NetLocation::new(address.clone(), session_id + 1);
            stream.start_wire_session(session_id, destination);
        }
        assert_eq!(stream.wire_sessions.len(), MAX_XUDP_SESSIONS);

        // Refreshing session 0 under the same ID replaces its old entry, so the
        // cap must not force another session out and the stale order entry must
        // not close the replacement.
        let replacement_destination = NetLocation::new(address.clone(), 7);
        stream.start_wire_session(0, replacement_destination.clone());
        assert_eq!(stream.wire_sessions.len(), MAX_XUDP_SESSIONS);
        assert_eq!(
            stream.wire_sessions.get(&0).unwrap().default_destination,
            replacement_destination
        );
        for session_id in 1..MAX_XUDP_SESSIONS as u16 {
            assert!(
                stream.wire_sessions.contains_key(&session_id),
                "session {session_id} must survive the refresh of session 0"
            );
        }
    }

    #[test]
    fn repeated_session_refresh_keeps_the_order_queue_bounded() {
        let mut stream = XudpMessageStream::new(Box::new(PendingOnceWriter {
            output: Arc::new(Mutex::new(Vec::new())),
            pending: false,
        }));
        let destination = NetLocation::new(Address::Ipv4("127.0.0.1".parse().unwrap()), 53);
        stream.start_wire_session(0, destination.clone());
        for _ in 0..(MAX_XUDP_SESSIONS * 3) {
            stream.start_wire_session(0, destination.clone());
        }
        assert_eq!(stream.wire_sessions.len(), 1);
        assert!(
            stream.wire_session_order.len() <= MAX_XUDP_SESSIONS + 1,
            "reopening one session must not grow the order queue without bound"
        );
    }

    #[test]
    fn flush_reports_write_zero_when_first_write_makes_no_progress() {
        let (writer, handles) = budgeted_writer(1, vec![WriteStep::WriteZero]);
        let (mut stream, frame) = stream_with_buffered_response(writer);
        let waker = noop_waker();
        let mut cx = Context::from_waker(&waker);

        let error = match Pin::new(&mut stream).poll_flush_message(&mut cx) {
            Poll::Ready(Err(error)) => error,
            other => panic!("expected WriteZero error, got {:?}", other),
        };

        assert_eq!(error.kind(), io::ErrorKind::WriteZero);
        assert_eq!(handles.calls.lock().unwrap().writes, 1);
        assert_eq!(handles.calls.lock().unwrap().flushes, 0);
        assert!(handles.output.lock().unwrap().is_empty());
        assert_eq!(&stream.write_buffer[..], &frame[..]);
    }

    #[test]
    fn flush_reports_write_zero_after_partial_progress_and_keeps_remaining_buffer() {
        let partial = 5;
        let (writer, handles) = budgeted_writer(
            2,
            vec![WriteStep::WriteBytes(partial), WriteStep::WriteZero],
        );
        let (mut stream, frame) = stream_with_buffered_response(writer);
        assert!(
            frame.len() > partial,
            "fixture frame must exceed partial write"
        );
        let waker = noop_waker();
        let mut cx = Context::from_waker(&waker);

        let error = match Pin::new(&mut stream).poll_flush_message(&mut cx) {
            Poll::Ready(Err(error)) => error,
            other => panic!("expected WriteZero error, got {:?}", other),
        };

        assert_eq!(error.kind(), io::ErrorKind::WriteZero);
        assert_eq!(handles.calls.lock().unwrap().writes, 2);
        assert_eq!(handles.calls.lock().unwrap().flushes, 0);
        assert_eq!(&handles.output.lock().unwrap()[..], &frame[..partial]);
        assert_eq!(&stream.write_buffer[..], &frame[partial..]);
    }

    #[test]
    fn flush_with_empty_buffer_skips_writes_and_flushes_inner() {
        let (writer, handles) = budgeted_writer(0, Vec::new());
        let mut stream = XudpMessageStream::new(Box::new(writer));
        let waker = noop_waker();
        let mut cx = Context::from_waker(&waker);

        assert!(matches!(
            Pin::new(&mut stream).poll_flush_message(&mut cx),
            Poll::Ready(Ok(()))
        ));
        assert_eq!(handles.calls.lock().unwrap().writes, 0);
        assert_eq!(handles.calls.lock().unwrap().flushes, 1);
        assert!(handles.output.lock().unwrap().is_empty());
    }
}
