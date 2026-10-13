// XUDP frame protocol implementation
// Protocol-agnostic UDP multiplexing used by VLESS in this build

use bytes::{Buf, BufMut, BytesMut};
use std::net::{Ipv4Addr, Ipv6Addr};

use crate::address::{Address, NetLocation};

/// XUDP session status
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
pub enum SessionStatus {
    New = 0x01,       // Create new session
    Keep = 0x02,      // Continue session
    End = 0x03,       // Close session
    KeepAlive = 0x04, // Heartbeat
}

impl TryFrom<u8> for SessionStatus {
    type Error = std::io::Error;

    fn try_from(value: u8) -> Result<Self, Self::Error> {
        match value {
            0x01 => Ok(SessionStatus::New),
            0x02 => Ok(SessionStatus::Keep),
            0x03 => Ok(SessionStatus::End),
            0x04 => Ok(SessionStatus::KeepAlive),
            other => Err(std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                format!("invalid session status: {}", other),
            )),
        }
    }
}

/// XUDP frame options (bitmask)
#[derive(Debug, Clone, Copy)]
pub struct FrameOption(u8);

impl FrameOption {
    pub const DATA: u8 = 0x01;
    pub const ERROR: u8 = 0x02;

    pub fn new() -> Self {
        Self(0)
    }

    pub fn with_data(mut self) -> Self {
        self.0 |= Self::DATA;
        self
    }

    pub fn has_data(&self) -> bool {
        (self.0 & Self::DATA) != 0
    }

    pub fn has_error(&self) -> bool {
        (self.0 & Self::ERROR) != 0
    }

    pub fn raw(&self) -> u8 {
        self.0
    }
}

impl Default for FrameOption {
    fn default() -> Self {
        Self::new()
    }
}

impl From<u8> for FrameOption {
    fn from(value: u8) -> Self {
        Self(value)
    }
}

/// Target network type
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
pub enum TargetNetwork {
    Tcp = 0x01,
    Udp = 0x02,
}

impl TryFrom<u8> for TargetNetwork {
    type Error = std::io::Error;

    fn try_from(value: u8) -> Result<Self, Self::Error> {
        match value {
            0x01 => Ok(TargetNetwork::Tcp),
            0x02 => Ok(TargetNetwork::Udp),
            other => Err(std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                format!("invalid network type: {}", other),
            )),
        }
    }
}

/// XUDP frame metadata
#[derive(Debug)]
pub struct FrameMetadata {
    pub session_id: u16,
    pub status: SessionStatus,
    pub option: FrameOption,
    pub target: Option<NetLocation>,
    pub network: Option<TargetNetwork>,
}

impl FrameMetadata {
    /// Encode frame metadata to bytes
    pub fn encode(&self, buf: &mut BytesMut) -> std::io::Result<()> {
        // Reserve space for length (will fill in at end)
        let length_pos = buf.len();
        buf.put_u16(0);

        let metadata_start = buf.len();

        // Session ID
        buf.put_u16(self.session_id);

        // Status and Option
        buf.put_u8(self.status as u8);
        buf.put_u8(self.option.raw());

        // For New status or Keep+UDP: write destination
        if self.status == SessionStatus::New {
            let network = self.network.ok_or_else(|| {
                std::io::Error::new(
                    std::io::ErrorKind::InvalidInput,
                    "network required for SessionStatusNew",
                )
            })?;
            buf.put_u8(network as u8);

            let target = self.target.as_ref().ok_or_else(|| {
                std::io::Error::new(
                    std::io::ErrorKind::InvalidInput,
                    "target required for SessionStatusNew",
                )
            })?;

            // Port
            buf.put_u16(target.port());

            // Address
            encode_address(buf, target.address())?;
        } else if matches!(self.status, SessionStatus::Keep)
            && matches!(self.network, Some(TargetNetwork::Udp))
        {
            // For Keep frames with UDP, include destination
            buf.put_u8(TargetNetwork::Udp as u8);

            let target = self.target.as_ref().ok_or_else(|| {
                std::io::Error::new(
                    std::io::ErrorKind::InvalidInput,
                    "target required for Keep+UDP",
                )
            })?;

            buf.put_u16(target.port());
            encode_address(buf, target.address())?;
        }

        // Calculate and write length
        let metadata_len = buf.len() - metadata_start;
        let length_bytes = &mut buf[length_pos..length_pos + 2];
        length_bytes.copy_from_slice(&(metadata_len as u16).to_be_bytes());

        Ok(())
    }

    /// Decode frame metadata from bytes
    pub fn decode(buf: &mut BytesMut) -> std::io::Result<Option<Self>> {
        // Need at least length field
        if buf.len() < 2 {
            log::debug!(
                "[XUDP DECODE] Buffer too short for length field: {} bytes",
                buf.len()
            );
            return Ok(None);
        }

        let metadata_len = u16::from_be_bytes([buf[0], buf[1]]) as usize;
        log::debug!(
            "[XUDP DECODE] metadata_len={}, buf.len()={}, need={}",
            metadata_len,
            buf.len(),
            2 + metadata_len
        );

        // Need complete metadata
        if buf.len() < 2 + metadata_len {
            log::debug!("[XUDP DECODE] Incomplete metadata, need more data");
            return Ok(None);
        }

        if metadata_len < 4 {
            log::error!(
                "[XUDP DECODE] Metadata too short: {} (buffer was {} bytes, first 8 metadata bytes: {:?})",
                metadata_len,
                buf.len(),
                &buf[2..std::cmp::min(2 + 8, buf.len())]
            );
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                format!("metadata too short: {}", metadata_len),
            ));
        }

        // Parse strictly inside the declared metadata envelope. The buffer is
        // consumed only after the whole envelope parses, so payload and
        // following frame bytes can never satisfy a missing destination field.
        let metadata = &buf[2..2 + metadata_len];
        let session_id = u16::from_be_bytes([metadata[0], metadata[1]]);
        let status = SessionStatus::try_from(metadata[2])?;
        let option = FrameOption::from(metadata[3]);
        let mut fields = &metadata[4..];

        let mut network = None;
        let mut target = None;

        // New always carries a destination; Keep carries one only for UDP.
        let has_destination = match status {
            SessionStatus::New => true,
            SessionStatus::Keep => fields.first() == Some(&(TargetNetwork::Udp as u8)),
            SessionStatus::End | SessionStatus::KeepAlive => false,
        };

        if has_destination {
            let net_byte = take_metadata_u8(&mut fields, "network type")?;
            network = Some(TargetNetwork::try_from(net_byte)?);

            let port = take_metadata_u16(&mut fields, "port")?;
            let address = decode_address(&mut fields)?;
            target = Some(NetLocation::new(address, port));
        }

        // Consume exactly the declared envelope. Any remaining metadata bytes
        // (GlobalID or padding) are skipped; payload and following frames stay.
        let unconsumed = fields.len();
        if unconsumed > 0 {
            log::debug!(
                "[XUDP DECODE] Skipping {} unconsumed metadata bytes (GlobalID or padding)",
                unconsumed
            );
        }
        buf.advance(2 + metadata_len);

        Ok(Some(FrameMetadata {
            session_id,
            status,
            option,
            target,
            network,
        }))
    }
}

/// Read one metadata byte, bounded by the declared envelope.
fn take_metadata_u8(fields: &mut &[u8], name: &str) -> std::io::Result<u8> {
    let Some((first, rest)) = fields.split_first() else {
        return Err(std::io::Error::new(
            std::io::ErrorKind::UnexpectedEof,
            format!("incomplete XUDP metadata: missing {}", name),
        ));
    };
    *fields = rest;
    Ok(*first)
}

/// Read a big-endian u16 field, bounded by the declared envelope.
fn take_metadata_u16(fields: &mut &[u8], name: &str) -> std::io::Result<u16> {
    if fields.len() < 2 {
        return Err(std::io::Error::new(
            std::io::ErrorKind::UnexpectedEof,
            format!("incomplete XUDP metadata: truncated {}", name),
        ));
    }
    let value = u16::from_be_bytes([fields[0], fields[1]]);
    *fields = &fields[2..];
    Ok(value)
}

/// Encode address to buffer (VLESS address format)
fn encode_address(buf: &mut BytesMut, address: &Address) -> std::io::Result<()> {
    match address {
        Address::Ipv4(v4) => {
            buf.put_u8(0x01);
            buf.put_slice(&v4.octets());
        }
        Address::Hostname(hostname) => {
            if hostname.len() > 255 {
                return Err(std::io::Error::new(
                    std::io::ErrorKind::InvalidInput,
                    "hostname too long",
                ));
            }
            buf.put_u8(0x02);
            buf.put_u8(hostname.len() as u8);
            buf.put_slice(hostname.as_bytes());
        }
        Address::Ipv6(v6) => {
            buf.put_u8(0x03);
            buf.put_slice(&v6.octets());
        }
    }
    Ok(())
}

/// Decode address from a bounded metadata slice (VLESS address format)
fn decode_address(fields: &mut &[u8]) -> std::io::Result<Address> {
    let addr_type = take_metadata_u8(fields, "address type")?;
    match addr_type {
        0x01 => {
            // IPv4
            if fields.len() < 4 {
                return Err(std::io::Error::new(
                    std::io::ErrorKind::UnexpectedEof,
                    "incomplete IPv4 address",
                ));
            }
            let mut octets = [0u8; 4];
            octets.copy_from_slice(&fields[..4]);
            *fields = &fields[4..];
            Ok(Address::Ipv4(Ipv4Addr::from(octets)))
        }
        0x02 => {
            // Hostname
            let len = take_metadata_u8(fields, "hostname length")? as usize;
            if fields.len() < len {
                return Err(std::io::Error::new(
                    std::io::ErrorKind::UnexpectedEof,
                    "incomplete hostname",
                ));
            }
            let hostname = std::str::from_utf8(&fields[..len])
                .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e))?;
            let address = Address::from(hostname)?;
            *fields = &fields[len..];
            Ok(address)
        }
        0x03 => {
            // IPv6
            if fields.len() < 16 {
                return Err(std::io::Error::new(
                    std::io::ErrorKind::UnexpectedEof,
                    "incomplete IPv6 address",
                ));
            }
            let mut octets = [0u8; 16];
            octets.copy_from_slice(&fields[..16]);
            *fields = &fields[16..];
            Ok(Address::Ipv6(Ipv6Addr::from(octets)))
        }
        other => Err(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            format!("invalid address type: {}", other),
        )),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // ==========================================================================
    // Basic encode/decode tests
    // ==========================================================================

    #[test]
    fn test_encode_decode_new_frame_udp() {
        let metadata = FrameMetadata {
            session_id: 42,
            status: SessionStatus::New,
            option: FrameOption::new().with_data(),
            target: Some(NetLocation::new(
                Address::Ipv4("1.1.1.1".parse().unwrap()),
                53,
            )),
            network: Some(TargetNetwork::Udp),
        };

        let mut buf = BytesMut::new();
        metadata.encode(&mut buf).unwrap();

        let decoded = FrameMetadata::decode(&mut buf).unwrap().unwrap();
        assert_eq!(decoded.session_id, 42);
        assert_eq!(decoded.status, SessionStatus::New);
        assert!(decoded.option.has_data());
        assert_eq!(decoded.network, Some(TargetNetwork::Udp));
        assert!(decoded.target.is_some());
        let target = decoded.target.unwrap();
        assert_eq!(target.port(), 53);
    }

    #[test]
    fn test_encode_decode_keep_frame_udp() {
        let metadata = FrameMetadata {
            session_id: 100,
            status: SessionStatus::Keep,
            option: FrameOption::new().with_data(),
            target: Some(NetLocation::new(
                Address::Ipv4("8.8.8.8".parse().unwrap()),
                53,
            )),
            network: Some(TargetNetwork::Udp),
        };

        let mut buf = BytesMut::new();
        metadata.encode(&mut buf).unwrap();

        let decoded = FrameMetadata::decode(&mut buf).unwrap().unwrap();
        assert_eq!(decoded.session_id, 100);
        assert_eq!(decoded.status, SessionStatus::Keep);
        assert!(decoded.option.has_data());
        assert_eq!(decoded.network, Some(TargetNetwork::Udp));
    }

    #[test]
    fn test_encode_decode_end_frame() {
        let metadata = FrameMetadata {
            session_id: 50,
            status: SessionStatus::End,
            option: FrameOption::new(),
            target: None,
            network: None,
        };

        let mut buf = BytesMut::new();
        metadata.encode(&mut buf).unwrap();

        let decoded = FrameMetadata::decode(&mut buf).unwrap().unwrap();
        assert_eq!(decoded.session_id, 50);
        assert_eq!(decoded.status, SessionStatus::End);
        assert!(!decoded.option.has_data());
    }

    #[test]
    fn test_encode_decode_hostname() {
        let metadata = FrameMetadata {
            session_id: 1,
            status: SessionStatus::New,
            option: FrameOption::new(),
            target: Some(NetLocation::new(
                Address::Hostname("example.com".to_string()),
                443,
            )),
            network: Some(TargetNetwork::Tcp),
        };

        let mut buf = BytesMut::new();
        metadata.encode(&mut buf).unwrap();

        let decoded = FrameMetadata::decode(&mut buf).unwrap().unwrap();
        assert_eq!(decoded.session_id, 1);
        let target = decoded.target.unwrap();
        assert_eq!(target.port(), 443);
        match target.address() {
            Address::Hostname(h) => assert_eq!(h, "example.com"),
            _ => panic!("expected hostname"),
        }
    }

    /// Test decoding a frame with extra metadata bytes (GlobalID simulation)
    /// Xray-core appends 8-byte GlobalID for UDP session continuity.
    /// We skip these bytes like sing-box
    #[test]
    fn test_decode_frame_with_globalid_extra_bytes() {
        let mut buf = BytesMut::new();

        // Manually construct a frame with GlobalID (8 extra bytes)
        // Format: length(2) + session_id(2) + status(1) + option(1) + network(1) + port(2) + addr_type(1) + ipv4(4) + GlobalID(8)
        let metadata_len: u16 = 4 + 1 + 2 + 1 + 4 + 8; // 20 bytes
        buf.put_u16(metadata_len);
        buf.put_u16(123); // session_id
        buf.put_u8(0x01); // SessionStatus::New
        buf.put_u8(0x01); // FrameOption::DATA
        buf.put_u8(0x02); // TargetNetwork::Udp
        buf.put_u16(53); // port
        buf.put_u8(0x01); // address type: IPv4
        buf.put_slice(&[8, 8, 8, 8]); // 8.8.8.8
        buf.put_slice(&[0x01, 0x02, 0x03, 0x04, 0x05, 0x06, 0x07, 0x08]); // GlobalID (8 bytes)

        let decoded = FrameMetadata::decode(&mut buf).unwrap().unwrap();
        assert_eq!(decoded.session_id, 123);
        assert_eq!(decoded.status, SessionStatus::New);
        assert!(decoded.option.has_data());
        assert_eq!(decoded.network, Some(TargetNetwork::Udp));
        let target = decoded.target.unwrap();
        assert_eq!(target.port(), 53);
        assert_eq!(target.address(), &Address::Ipv4("8.8.8.8".parse().unwrap()));

        // Buffer should be fully consumed
        assert_eq!(buf.len(), 0, "Buffer should be fully consumed after decode");
    }

    /// Test Keep frame without destination (TCP keep)
    /// This is a minimal Keep frame with just session_id + status + option
    #[test]
    fn test_decode_keep_frame_tcp_no_destination() {
        let mut buf = BytesMut::new();

        // Minimal Keep frame: length(2) + session_id(2) + status(1) + option(1) = 4 bytes metadata
        buf.put_u16(4); // metadata length
        buf.put_u16(42); // session_id
        buf.put_u8(0x02); // SessionStatus::Keep
        buf.put_u8(0x01); // FrameOption::DATA

        let decoded = FrameMetadata::decode(&mut buf).unwrap().unwrap();
        assert_eq!(decoded.session_id, 42);
        assert_eq!(decoded.status, SessionStatus::Keep);
        assert!(decoded.option.has_data());
        assert!(decoded.network.is_none(), "TCP Keep should have no network");
        assert!(
            decoded.target.is_none(),
            "TCP Keep should have no destination"
        );
        assert_eq!(buf.len(), 0);
    }

    /// Test Keep frame with UDP destination change
    /// Per Xray-core: Keep frames CAN have destination if network byte is 0x02 (UDP)
    #[test]
    fn test_decode_keep_frame_udp_with_destination() {
        let mut buf = BytesMut::new();

        // Keep+UDP frame with destination
        // Format: length(2) + session_id(2) + status(1) + option(1) + network(1) + port(2) + addr_type(1) + ipv4(4)
        let metadata_len: u16 = 4 + 1 + 2 + 1 + 4; // 12 bytes
        buf.put_u16(metadata_len);
        buf.put_u16(99); // session_id
        buf.put_u8(0x02); // SessionStatus::Keep
        buf.put_u8(0x01); // FrameOption::DATA
        buf.put_u8(0x02); // TargetNetwork::Udp - this triggers destination parsing
        buf.put_u16(1234); // port
        buf.put_u8(0x01); // address type: IPv4
        buf.put_slice(&[192, 168, 1, 1]); // 192.168.1.1

        let decoded = FrameMetadata::decode(&mut buf).unwrap().unwrap();
        assert_eq!(decoded.session_id, 99);
        assert_eq!(decoded.status, SessionStatus::Keep);
        assert_eq!(decoded.network, Some(TargetNetwork::Udp));
        let target = decoded.target.unwrap();
        assert_eq!(target.port(), 1234);
        assert_eq!(buf.len(), 0);
    }

    /// Test Keep frame where first byte after header is NOT 0x02 (not UDP)
    /// Should NOT parse destination even if there's extra data
    #[test]
    fn test_decode_keep_frame_non_udp_extra_bytes() {
        let mut buf = BytesMut::new();

        // Keep frame where network byte is 0x01 (TCP) - should NOT parse destination
        let metadata_len: u16 = 4 + 1 + 2 + 1 + 4; // 12 bytes total
        buf.put_u16(metadata_len);
        buf.put_u16(50); // session_id
        buf.put_u8(0x02); // SessionStatus::Keep
        buf.put_u8(0x01); // FrameOption::DATA
        buf.put_u8(0x01); // TargetNetwork::Tcp (NOT UDP!)
        buf.put_u16(443); // port (should be skipped)
        buf.put_u8(0x01); // address type
        buf.put_slice(&[1, 2, 3, 4]); // address (should be skipped)

        let decoded = FrameMetadata::decode(&mut buf).unwrap().unwrap();
        assert_eq!(decoded.session_id, 50);
        assert_eq!(decoded.status, SessionStatus::Keep);
        // Network and target should be None because first byte wasn't 0x02
        assert!(
            decoded.network.is_none(),
            "Keep+TCP should not parse network"
        );
        assert!(
            decoded.target.is_none(),
            "Keep+TCP should not parse destination"
        );
        // Extra bytes should be consumed/skipped
        assert_eq!(buf.len(), 0, "Extra bytes should be consumed");
    }

    /// Test KeepAlive frame (status 0x04)
    #[test]
    fn test_decode_keepalive_frame() {
        let mut buf = BytesMut::new();

        buf.put_u16(4); // metadata length
        buf.put_u16(1); // session_id
        buf.put_u8(0x04); // SessionStatus::KeepAlive
        buf.put_u8(0x00); // no options

        let decoded = FrameMetadata::decode(&mut buf).unwrap().unwrap();
        assert_eq!(decoded.session_id, 1);
        assert_eq!(decoded.status, SessionStatus::KeepAlive);
        assert!(!decoded.option.has_data());
        assert!(!decoded.option.has_error());
        assert_eq!(buf.len(), 0);
    }

    /// Test End frame with ERROR option
    #[test]
    fn test_decode_end_frame_with_error() {
        let mut buf = BytesMut::new();

        buf.put_u16(4); // metadata length
        buf.put_u16(77); // session_id
        buf.put_u8(0x03); // SessionStatus::End
        buf.put_u8(0x02); // FrameOption::ERROR

        let decoded = FrameMetadata::decode(&mut buf).unwrap().unwrap();
        assert_eq!(decoded.session_id, 77);
        assert_eq!(decoded.status, SessionStatus::End);
        assert!(!decoded.option.has_data());
        assert!(decoded.option.has_error());
        assert_eq!(buf.len(), 0);
    }

    /// Test IPv6 destination
    #[test]
    fn test_decode_new_frame_ipv6() {
        let mut buf = BytesMut::new();

        // New frame with IPv6 destination
        let metadata_len: u16 = 4 + 1 + 2 + 1 + 16; // 24 bytes
        buf.put_u16(metadata_len);
        buf.put_u16(200); // session_id
        buf.put_u8(0x01); // SessionStatus::New
        buf.put_u8(0x01); // FrameOption::DATA
        buf.put_u8(0x02); // TargetNetwork::Udp
        buf.put_u16(53); // port
        buf.put_u8(0x03); // address type: IPv6
        buf.put_slice(&[
            0x20, 0x01, 0x48, 0x60, 0x48, 0x60, 0, 0, 0, 0, 0, 0, 0, 0, 0x88, 0x88,
        ]); // 2001:4860:4860::8888

        let decoded = FrameMetadata::decode(&mut buf).unwrap().unwrap();
        assert_eq!(decoded.session_id, 200);
        let target = decoded.target.unwrap();
        assert_eq!(target.port(), 53);
        match target.address() {
            Address::Ipv6(v6) => {
                assert_eq!(v6.to_string(), "2001:4860:4860::8888");
            }
            _ => panic!("expected IPv6"),
        }
        assert_eq!(buf.len(), 0);
    }

    // ==========================================================================
    // Error handling tests
    // ==========================================================================

    /// Test incomplete buffer (not enough for length field)
    #[test]
    fn test_decode_incomplete_length() {
        let mut buf = BytesMut::new();
        buf.put_u8(0x00); // Only 1 byte, need 2 for length

        let result = FrameMetadata::decode(&mut buf).unwrap();
        assert!(result.is_none(), "Should return None for incomplete length");
        assert_eq!(buf.len(), 1, "Buffer should not be consumed");
    }

    /// Test incomplete metadata (length says more than available)
    #[test]
    fn test_decode_incomplete_metadata() {
        let mut buf = BytesMut::new();
        buf.put_u16(10); // metadata length = 10
        buf.put_u16(1); // session_id
        // Only 2 bytes of metadata, but length says 10

        let result = FrameMetadata::decode(&mut buf).unwrap();
        assert!(
            result.is_none(),
            "Should return None for incomplete metadata"
        );
    }

    /// Test metadata too short (less than 4 bytes)
    #[test]
    fn test_decode_metadata_too_short() {
        let mut buf = BytesMut::new();
        buf.put_u16(2); // metadata length = 2 (too short, need at least 4)
        buf.put_u16(1); // only 2 bytes of metadata

        let result = FrameMetadata::decode(&mut buf);
        assert!(result.is_err(), "Should error on metadata too short");
    }

    /// Test invalid session status
    #[test]
    fn test_decode_invalid_session_status() {
        let mut buf = BytesMut::new();
        buf.put_u16(4); // metadata length
        buf.put_u16(1); // session_id
        buf.put_u8(0x99); // Invalid status!
        buf.put_u8(0x00); // option

        let result = FrameMetadata::decode(&mut buf);
        assert!(result.is_err(), "Should error on invalid session status");
    }

    /// Test invalid network type in New frame
    #[test]
    fn test_decode_invalid_network_type() {
        let mut buf = BytesMut::new();
        let metadata_len: u16 = 4 + 1 + 2 + 1 + 4;
        buf.put_u16(metadata_len);
        buf.put_u16(1); // session_id
        buf.put_u8(0x01); // SessionStatus::New
        buf.put_u8(0x01); // FrameOption::DATA
        buf.put_u8(0x99); // Invalid network type!
        buf.put_u16(53); // port
        buf.put_u8(0x01); // address type
        buf.put_slice(&[1, 2, 3, 4]);

        let result = FrameMetadata::decode(&mut buf);
        assert!(result.is_err(), "Should error on invalid network type");
    }

    /// Test invalid address type
    #[test]
    fn test_decode_invalid_address_type() {
        let mut buf = BytesMut::new();
        let metadata_len: u16 = 4 + 1 + 2 + 1 + 4;
        buf.put_u16(metadata_len);
        buf.put_u16(1); // session_id
        buf.put_u8(0x01); // SessionStatus::New
        buf.put_u8(0x01); // FrameOption::DATA
        buf.put_u8(0x02); // TargetNetwork::Udp
        buf.put_u16(53); // port
        buf.put_u8(0x99); // Invalid address type!
        buf.put_slice(&[1, 2, 3, 4]);

        let result = FrameMetadata::decode(&mut buf);
        assert!(result.is_err(), "Should error on invalid address type");
    }

    #[test]
    fn test_frame_format_compatibility() {
        let mut buf = BytesMut::new();

        // Xray-core format for New+UDP:
        // 2 bytes - length (of metadata after this field)
        // 2 bytes - session id
        // 1 byte - status
        // 1 byte - option
        // 1 byte - network
        // 2 bytes - port
        // n bytes - address (1 byte type + address bytes)

        let metadata_len: u16 = 2 + 1 + 1 + 1 + 2 + 1 + 4; // = 12
        buf.put_u16(metadata_len);
        buf.put_u16(0x0001); // session_id = 1
        buf.put_u8(0x01); // SessionStatusNew
        buf.put_u8(0x01); // OptionData
        buf.put_u8(0x02); // TargetNetworkUDP
        buf.put_u16(443); // port (big-endian)
        buf.put_u8(0x01); // AddressTypeIPv4
        buf.put_slice(&[93, 184, 216, 34]); // 93.184.216.34 (example.com)

        let decoded = FrameMetadata::decode(&mut buf).unwrap().unwrap();

        assert_eq!(decoded.session_id, 1);
        assert_eq!(decoded.status, SessionStatus::New);
        assert!(decoded.option.has_data());
        assert!(!decoded.option.has_error());
        assert_eq!(decoded.network, Some(TargetNetwork::Udp));

        let target = decoded.target.unwrap();
        assert_eq!(target.port(), 443);
        assert_eq!(
            target.address(),
            &Address::Ipv4("93.184.216.34".parse().unwrap())
        );
    }

    /// Test domain name encoding
    #[test]
    fn test_domain_format() {
        let mut buf = BytesMut::new();

        let domain = "dns.google";
        let metadata_len: u16 = 4 + 1 + 2 + 1 + 1 + domain.len() as u16; // 19
        buf.put_u16(metadata_len);
        buf.put_u16(5); // session_id
        buf.put_u8(0x01); // SessionStatusNew
        buf.put_u8(0x01); // OptionData
        buf.put_u8(0x02); // TargetNetworkUDP
        buf.put_u16(853); // DNS over TLS port
        buf.put_u8(0x02); // AddressTypeDomain
        buf.put_u8(domain.len() as u8);
        buf.put_slice(domain.as_bytes());

        let decoded = FrameMetadata::decode(&mut buf).unwrap().unwrap();
        let target = decoded.target.unwrap();
        assert_eq!(target.port(), 853);
        match target.address() {
            Address::Hostname(h) => assert_eq!(h, "dns.google"),
            _ => panic!("expected hostname"),
        }
    }

    /// Test that our encoder produces Xray-compatible output
    #[test]
    fn test_encode_xray_compatible() {
        let metadata = FrameMetadata {
            session_id: 100,
            status: SessionStatus::New,
            option: FrameOption::new().with_data(),
            target: Some(NetLocation::new(
                Address::Ipv4("1.1.1.1".parse().unwrap()),
                53,
            )),
            network: Some(TargetNetwork::Udp),
        };

        let mut buf = BytesMut::new();
        metadata.encode(&mut buf).unwrap();

        // Verify the wire format
        assert_eq!(buf[0..2], [0x00, 0x0c]); // length = 12 (big-endian)
        assert_eq!(buf[2..4], [0x00, 0x64]); // session_id = 100 (big-endian)
        assert_eq!(buf[4], 0x01); // SessionStatusNew
        assert_eq!(buf[5], 0x01); // OptionData
        assert_eq!(buf[6], 0x02); // TargetNetworkUDP
        assert_eq!(buf[7..9], [0x00, 0x35]); // port = 53 (big-endian)
        assert_eq!(buf[9], 0x01); // AddressTypeIPv4
        assert_eq!(buf[10..14], [1, 1, 1, 1]); // 1.1.1.1
    }

    /// Test maximum session ID (u16::MAX)
    #[test]
    fn test_max_session_id() {
        let mut buf = BytesMut::new();
        buf.put_u16(4);
        buf.put_u16(0xFFFF); // max session_id
        buf.put_u8(0x02); // Keep
        buf.put_u8(0x00);

        let decoded = FrameMetadata::decode(&mut buf).unwrap().unwrap();
        assert_eq!(decoded.session_id, 65535);
    }

    /// Test both DATA and ERROR options set
    #[test]
    fn test_data_and_error_options() {
        let mut buf = BytesMut::new();
        buf.put_u16(4);
        buf.put_u16(1);
        buf.put_u8(0x03); // End
        buf.put_u8(0x03); // DATA | ERROR

        let decoded = FrameMetadata::decode(&mut buf).unwrap().unwrap();
        assert!(decoded.option.has_data());
        assert!(decoded.option.has_error());
    }

    // ==========================================================================
    // parser_boundary tests: metadata envelope bounds
    // ==========================================================================

    /// Build a complete XUDP buffer from an explicit metadata envelope and a tail.
    fn parser_boundary_complete(metadata: &[u8], tail: &[u8]) -> BytesMut {
        assert!(metadata.len() <= u16::MAX as usize);
        let mut buf = BytesMut::new();
        buf.put_u16(metadata.len() as u16);
        buf.put_slice(metadata);
        buf.put_slice(tail);
        buf
    }

    /// Encode one New+UDP IPv4 frame with a DATA payload.
    fn parser_boundary_new_ipv4_frame(
        session_id: u16,
        address: &str,
        port: u16,
        payload: &[u8],
    ) -> BytesMut {
        let metadata = FrameMetadata {
            session_id,
            status: SessionStatus::New,
            option: FrameOption::new().with_data(),
            target: Some(NetLocation::new(
                Address::Ipv4(address.parse().unwrap()),
                port,
            )),
            network: Some(TargetNetwork::Udp),
        };
        let mut buf = BytesMut::new();
        metadata.encode(&mut buf).unwrap();
        buf.put_u16(payload.len() as u16);
        buf.put_slice(payload);
        buf
    }

    /// Metadata lengths 0..=3 are shorter than the fixed XUDP header.
    #[test]
    fn parser_boundary_metadata_length_zero_to_three_rejected() {
        for metadata_len in 0u16..=3 {
            let mut buf = parser_boundary_complete(&vec![0u8; metadata_len as usize], &[]);
            let result = FrameMetadata::decode(&mut buf);
            let error = result.expect_err("short metadata must be rejected");
            assert_eq!(
                error.kind(),
                std::io::ErrorKind::InvalidData,
                "metadata_len {metadata_len}"
            );
        }
    }

    /// The reported panic input: New status declaring a network byte but no port.
    #[test]
    fn parser_boundary_exact_panic_input_is_rejected() {
        let mut buf = BytesMut::from(&[0x00u8, 0x05, 0x00, 0x01, 0x01, 0x00, 0x02][..]);
        let original = buf.clone();
        let result = FrameMetadata::decode(&mut buf);
        assert!(
            result.is_err(),
            "expected a controlled error, got {result:?}"
        );
        assert_eq!(buf, original, "input bytes must stay untouched");
    }

    /// New status with no destination bytes at all is malformed.
    #[test]
    fn parser_boundary_metadata_length_four_new_is_rejected() {
        let crafted_tail = [0x02u8, 0x00, 0x35, 0x01, 1, 1, 1, 1];
        let mut buf = parser_boundary_complete(&[0x00, 0x01, 0x01, 0x00], &crafted_tail);
        let original = buf.clone();
        let result = FrameMetadata::decode(&mut buf);
        assert!(
            result.is_err(),
            "New without a destination must be rejected: {result:?}"
        );
        assert_eq!(buf, original, "crafted tail bytes must stay in the buffer");
    }

    /// Keep without a UDP tail is a valid 4-byte envelope.
    #[test]
    fn parser_boundary_metadata_length_four_keep_without_destination() {
        let mut buf = parser_boundary_complete(&[0x00, 0x2a, 0x02, 0x01], &[]);
        let decoded = FrameMetadata::decode(&mut buf)
            .expect("complete Keep metadata decodes")
            .expect("complete Keep metadata is available");
        assert_eq!(decoded.session_id, 0x2a);
        assert_eq!(decoded.status, SessionStatus::Keep);
        assert!(decoded.network.is_none());
        assert!(decoded.target.is_none());
        assert!(buf.is_empty(), "Keep envelope is consumed exactly");
    }

    /// New status with a network byte but no port byte is malformed.
    #[test]
    fn parser_boundary_metadata_length_five_new_network_only_rejected() {
        let mut buf = parser_boundary_complete(&[0x00, 0x01, 0x01, 0x00, 0x02], &[]);
        let original = buf.clone();
        let result = FrameMetadata::decode(&mut buf);
        assert!(
            result.is_err(),
            "network without port must be rejected: {result:?}"
        );
        assert_eq!(buf, original);
    }

    /// Keep+UDP with a network byte but no port byte is malformed.
    #[test]
    fn parser_boundary_metadata_length_five_keep_udp_network_only_rejected() {
        let mut buf = parser_boundary_complete(&[0x00, 0x01, 0x02, 0x01, 0x02], &[]);
        let original = buf.clone();
        let result = FrameMetadata::decode(&mut buf);
        assert!(
            result.is_err(),
            "Keep+UDP without port must be rejected: {result:?}"
        );
        assert_eq!(buf, original);
    }

    /// New status with a network byte and one port byte is malformed.
    #[test]
    fn parser_boundary_metadata_length_six_new_partial_port_rejected() {
        let crafted_tail = [0x35u8, 0x01, 1, 1, 1, 1];
        let mut buf =
            parser_boundary_complete(&[0x00, 0x01, 0x01, 0x00, 0x02, 0x00], &crafted_tail);
        let original = buf.clone();
        let result = FrameMetadata::decode(&mut buf);
        assert!(result.is_err(), "partial port must be rejected: {result:?}");
        assert_eq!(buf, original, "crafted port bytes must stay in the buffer");
    }

    /// A payload port must not complete a missing metadata port.
    #[test]
    fn parser_boundary_truncated_port_field_rejected() {
        let crafted_tail = [0x00u8, 0x35, 0x01, 1, 1, 1, 1];
        let mut buf = parser_boundary_complete(&[0x00, 0x01, 0x01, 0x00, 0x02], &crafted_tail);
        let original = buf.clone();
        let result = FrameMetadata::decode(&mut buf);
        assert!(
            result.is_err(),
            "port must fit inside the metadata: {result:?}"
        );
        assert_eq!(buf, original, "crafted bytes must stay in the buffer");
    }

    /// A payload address type must not complete a missing metadata address type.
    #[test]
    fn parser_boundary_truncated_address_type_rejected() {
        let crafted_tail = [0x01u8, 1, 1, 1, 1];
        let mut buf =
            parser_boundary_complete(&[0x00, 0x01, 0x01, 0x00, 0x02, 0x00, 0x35], &crafted_tail);
        let original = buf.clone();
        let result = FrameMetadata::decode(&mut buf);
        assert!(
            result.is_err(),
            "address type must fit inside the metadata: {result:?}"
        );
        assert_eq!(buf, original, "crafted bytes must stay in the buffer");
    }

    /// A payload IPv4 tail must not complete a truncated metadata IPv4 address.
    #[test]
    fn parser_boundary_truncated_ipv4_address_rejected() {
        let crafted_tail = [2u8, 3, 4];
        let mut buf = parser_boundary_complete(
            &[0x00, 0x01, 0x01, 0x00, 0x02, 0x00, 0x35, 0x01, 1],
            &crafted_tail,
        );
        let original = buf.clone();
        let result = FrameMetadata::decode(&mut buf);
        assert!(
            result.is_err(),
            "IPv4 address must fit inside the metadata: {result:?}"
        );
        assert_eq!(buf, original, "crafted bytes must stay in the buffer");
    }

    /// A payload IPv6 tail must not complete a truncated metadata IPv6 address.
    #[test]
    fn parser_boundary_truncated_ipv6_address_rejected() {
        let mut metadata = vec![0x00u8, 0x01, 0x01, 0x00, 0x02, 0x00, 0x35, 0x03];
        metadata.extend_from_slice(&[0x20, 0x01, 0x48]);
        let crafted_tail = [0u8; 13];
        let mut buf = parser_boundary_complete(&metadata, &crafted_tail);
        let original = buf.clone();
        let result = FrameMetadata::decode(&mut buf);
        assert!(
            result.is_err(),
            "IPv6 address must fit inside the metadata: {result:?}"
        );
        assert_eq!(buf, original, "crafted bytes must stay in the buffer");
    }

    /// A payload hostname length byte must not complete a missing metadata byte.
    #[test]
    fn parser_boundary_truncated_hostname_length_rejected() {
        let crafted_tail = [0x05u8, b'h', b'e', b'l', b'l', b'o'];
        let mut buf = parser_boundary_complete(
            &[0x00, 0x01, 0x01, 0x00, 0x02, 0x00, 0x35, 0x02],
            &crafted_tail,
        );
        let original = buf.clone();
        let result = FrameMetadata::decode(&mut buf);
        assert!(
            result.is_err(),
            "hostname length must fit inside the metadata: {result:?}"
        );
        assert_eq!(buf, original, "crafted bytes must stay in the buffer");
    }

    /// A payload hostname must not complete a hostname declared by metadata length.
    #[test]
    fn parser_boundary_truncated_hostname_bytes_rejected() {
        let mut buf = parser_boundary_complete(
            &[0x00, 0x01, 0x01, 0x00, 0x02, 0x00, 0x35, 0x02, 0x05],
            b"hello",
        );
        let original = buf.clone();
        let result = FrameMetadata::decode(&mut buf);
        assert!(
            result.is_err(),
            "hostname bytes must fit inside the metadata: {result:?}"
        );
        assert_eq!(buf, original, "crafted bytes must stay in the buffer");
    }

    /// Bytes of a following complete frame must not complete earlier metadata.
    #[test]
    fn parser_boundary_following_frame_bytes_cannot_complete_metadata() {
        // Metadata declares a hostname but omits the length byte. The next byte
        // is the length field of the next frame, which is zero and would read as
        // an empty hostname if payload bytes were allowed to complete metadata.
        let following = parser_boundary_new_ipv4_frame(9, "9.9.9.9", 53, b"next");
        let mut buf = parser_boundary_complete(
            &[0x00, 0x01, 0x01, 0x00, 0x02, 0x00, 0x35, 0x02],
            &following,
        );
        let original = buf.clone();
        let result = FrameMetadata::decode(&mut buf);
        assert!(
            result.is_err(),
            "following frame bytes must not complete metadata: {result:?}"
        );
        assert_eq!(
            buf, original,
            "following frame bytes must stay in the buffer"
        );
    }

    /// Every split prefix of a valid frame stays buffered until it is complete.
    #[test]
    fn parser_boundary_incomplete_split_input_is_kept() {
        let full = parser_boundary_new_ipv4_frame(7, "1.2.3.4", 5353, b"ping");
        let metadata_len = u16::from_be_bytes([full[0], full[1]]) as usize;
        for split in 0..(2 + metadata_len) {
            let mut buf = BytesMut::from(&full[..split]);
            let before = buf.clone();
            let result = FrameMetadata::decode(&mut buf)
                .expect("incomplete metadata is not an error before the envelope is complete");
            assert!(result.is_none(), "split {split} must be incomplete");
            assert_eq!(buf, before, "split {split} must remain buffered");
        }
    }

    /// Valid metadata consumes exactly its declared envelope and keeps the tail.
    #[test]
    fn parser_boundary_valid_metadata_consumes_exactly_envelope() {
        let metadata = FrameMetadata {
            session_id: 21,
            status: SessionStatus::New,
            option: FrameOption::new().with_data(),
            target: Some(NetLocation::new(
                Address::Ipv4("10.1.2.3".parse().unwrap()),
                443,
            )),
            network: Some(TargetNetwork::Udp),
        };
        let mut full = BytesMut::new();
        metadata.encode(&mut full).unwrap();
        let tail = b"payload-and-next-frame-bytes";
        full.put_slice(tail);

        let mut buf = full.clone();
        let decoded = FrameMetadata::decode(&mut buf).unwrap().unwrap();
        assert_eq!(decoded.session_id, 21);
        assert_eq!(decoded.target.unwrap().port(), 443);
        assert_eq!(buf, BytesMut::from(&tail[..]));
    }

    /// Coalesced frames decode one at a time without eating each other.
    #[test]
    fn parser_boundary_coalesced_frames_stay_intact() {
        let first = parser_boundary_new_ipv4_frame(11, "10.0.0.1", 53, b"one");
        let second = parser_boundary_new_ipv4_frame(12, "10.0.0.2", 54, b"two");
        let mut full = first.clone();
        full.extend_from_slice(&second);
        let first_metadata_len = u16::from_be_bytes([full[0], full[1]]) as usize;

        let mut buf = full.clone();
        let decoded_first = FrameMetadata::decode(&mut buf).unwrap().unwrap();
        assert_eq!(decoded_first.session_id, 11);
        assert_eq!(&buf[..], &full[2 + first_metadata_len..]);

        // Consume the first frame tail the way the message stream does.
        let first_data_len = u16::from_be_bytes([buf[0], buf[1]]) as usize;
        buf.advance(2 + first_data_len);

        let decoded_second = FrameMetadata::decode(&mut buf).unwrap().unwrap();
        assert_eq!(decoded_second.session_id, 12);
        let second_data_len = u16::from_be_bytes([buf[0], buf[1]]) as usize;
        assert_eq!(second_data_len, 3);
        assert_eq!(&buf[2..2 + second_data_len], b"two");
        buf.advance(2 + second_data_len);
        assert!(buf.is_empty());
    }

    /// Valid New+UDP IPv4 metadata is preserved.
    #[test]
    fn parser_boundary_new_ipv4_control() {
        let metadata = FrameMetadata {
            session_id: 42,
            status: SessionStatus::New,
            option: FrameOption::new().with_data(),
            target: Some(NetLocation::new(
                Address::Ipv4("1.1.1.1".parse().unwrap()),
                53,
            )),
            network: Some(TargetNetwork::Udp),
        };
        let mut buf = BytesMut::new();
        metadata.encode(&mut buf).unwrap();

        let decoded = FrameMetadata::decode(&mut buf).unwrap().unwrap();
        assert_eq!(decoded.session_id, 42);
        assert_eq!(decoded.status, SessionStatus::New);
        assert!(decoded.option.has_data());
        assert_eq!(decoded.network, Some(TargetNetwork::Udp));
        assert_eq!(decoded.target.unwrap().port(), 53);
        assert!(buf.is_empty());
    }

    /// Valid New+UDP IPv6 metadata is preserved.
    #[test]
    fn parser_boundary_new_ipv6_control() {
        let metadata = FrameMetadata {
            session_id: 43,
            status: SessionStatus::New,
            option: FrameOption::new(),
            target: Some(NetLocation::new(
                Address::Ipv6("2001:4860:4860::8888".parse().unwrap()),
                53,
            )),
            network: Some(TargetNetwork::Udp),
        };
        let mut buf = BytesMut::new();
        metadata.encode(&mut buf).unwrap();

        let decoded = FrameMetadata::decode(&mut buf).unwrap().unwrap();
        assert_eq!(decoded.session_id, 43);
        let target = decoded.target.unwrap();
        assert_eq!(target.port(), 53);
        assert_eq!(
            target.address(),
            &Address::Ipv6("2001:4860:4860::8888".parse().unwrap())
        );
        assert!(buf.is_empty());
    }

    /// Valid New+UDP hostname metadata is preserved.
    #[test]
    fn parser_boundary_new_hostname_control() {
        let metadata = FrameMetadata {
            session_id: 44,
            status: SessionStatus::New,
            option: FrameOption::new(),
            target: Some(NetLocation::new(
                Address::Hostname("example.com".to_string()),
                443,
            )),
            network: Some(TargetNetwork::Udp),
        };
        let mut buf = BytesMut::new();
        metadata.encode(&mut buf).unwrap();

        let decoded = FrameMetadata::decode(&mut buf).unwrap().unwrap();
        assert_eq!(decoded.session_id, 44);
        let target = decoded.target.unwrap();
        assert_eq!(target.port(), 443);
        match target.address() {
            Address::Hostname(hostname) => assert_eq!(hostname, "example.com"),
            other => panic!("expected hostname, got {other:?}"),
        }
        assert!(buf.is_empty());
    }

    /// Valid Keep+UDP IPv4 metadata is preserved.
    #[test]
    fn parser_boundary_keep_udp_ipv4_control() {
        let metadata = FrameMetadata {
            session_id: 45,
            status: SessionStatus::Keep,
            option: FrameOption::new().with_data(),
            target: Some(NetLocation::new(
                Address::Ipv4("8.8.8.8".parse().unwrap()),
                53,
            )),
            network: Some(TargetNetwork::Udp),
        };
        let mut buf = BytesMut::new();
        metadata.encode(&mut buf).unwrap();

        let decoded = FrameMetadata::decode(&mut buf).unwrap().unwrap();
        assert_eq!(decoded.status, SessionStatus::Keep);
        assert_eq!(decoded.network, Some(TargetNetwork::Udp));
        let target = decoded.target.unwrap();
        assert_eq!(target.port(), 53);
        assert_eq!(target.address(), &Address::Ipv4("8.8.8.8".parse().unwrap()));
        assert!(buf.is_empty());
    }

    /// Valid Keep+UDP IPv6 metadata is preserved.
    #[test]
    fn parser_boundary_keep_udp_ipv6_control() {
        let metadata = FrameMetadata {
            session_id: 46,
            status: SessionStatus::Keep,
            option: FrameOption::new().with_data(),
            target: Some(NetLocation::new(
                Address::Ipv6("2001:4860:4860::8844".parse().unwrap()),
                53,
            )),
            network: Some(TargetNetwork::Udp),
        };
        let mut buf = BytesMut::new();
        metadata.encode(&mut buf).unwrap();

        let decoded = FrameMetadata::decode(&mut buf).unwrap().unwrap();
        assert_eq!(decoded.status, SessionStatus::Keep);
        assert_eq!(decoded.network, Some(TargetNetwork::Udp));
        assert_eq!(decoded.target.unwrap().port(), 53);
        assert!(buf.is_empty());
    }

    /// Valid Keep+UDP hostname metadata is preserved.
    #[test]
    fn parser_boundary_keep_udp_hostname_control() {
        let metadata = FrameMetadata {
            session_id: 47,
            status: SessionStatus::Keep,
            option: FrameOption::new().with_data(),
            target: Some(NetLocation::new(
                Address::Hostname("dns.google".to_string()),
                853,
            )),
            network: Some(TargetNetwork::Udp),
        };
        let mut buf = BytesMut::new();
        metadata.encode(&mut buf).unwrap();

        let decoded = FrameMetadata::decode(&mut buf).unwrap().unwrap();
        assert_eq!(decoded.status, SessionStatus::Keep);
        let target = decoded.target.unwrap();
        assert_eq!(target.port(), 853);
        match target.address() {
            Address::Hostname(hostname) => assert_eq!(hostname, "dns.google"),
            other => panic!("expected hostname, got {other:?}"),
        }
        assert!(buf.is_empty());
    }

    /// Keep+UDP whose IPv4 address misses its last octet is malformed.
    #[test]
    fn parser_boundary_keep_udp_truncated_tail_rejected() {
        let crafted_tail = [0x04u8];
        let mut buf = parser_boundary_complete(
            &[0x00, 0x01, 0x02, 0x01, 0x02, 0x00, 0x35, 0x01, 8, 8, 8],
            &crafted_tail,
        );
        let original = buf.clone();
        let result = FrameMetadata::decode(&mut buf);
        assert!(
            result.is_err(),
            "truncated Keep+UDP IPv4 must be rejected: {result:?}"
        );
        assert_eq!(buf, original, "crafted bytes must stay in the buffer");
    }

    /// Keep with a non-UDP tail keeps the tail and parses no destination.
    #[test]
    fn parser_boundary_keep_tcp_tail_control() {
        let metadata = [0x00, 0x2a, 0x02, 0x01, 0x01, 0x00, 0x35, 0x01, 1, 2, 3, 4];
        let mut buf = parser_boundary_complete(&metadata, &[]);

        let decoded = FrameMetadata::decode(&mut buf).unwrap().unwrap();
        assert_eq!(decoded.status, SessionStatus::Keep);
        assert!(decoded.network.is_none());
        assert!(decoded.target.is_none());
        assert!(buf.is_empty());
    }

    /// Optional metadata tails (GlobalID style) are skipped, not parsed.
    #[test]
    fn parser_boundary_optional_metadata_tail_control() {
        let mut metadata = vec![0x00u8, 0x01, 0x01, 0x00, 0x02, 0x00, 0x35, 0x01, 8, 8, 8, 8];
        metadata.extend_from_slice(&[0xaa; 8]);
        let mut buf = parser_boundary_complete(&metadata, &[]);

        let decoded = FrameMetadata::decode(&mut buf).unwrap().unwrap();
        assert_eq!(decoded.status, SessionStatus::New);
        let target = decoded.target.unwrap();
        assert_eq!(target.port(), 53);
        assert_eq!(target.address(), &Address::Ipv4("8.8.8.8".parse().unwrap()));
        assert!(buf.is_empty());
    }

    /// KeepAlive and End control frames have no destination.
    #[test]
    fn parser_boundary_keepalive_and_end_controls() {
        for (status, name) in [(0x04u8, "KeepAlive"), (0x03u8, "End")] {
            let mut buf = parser_boundary_complete(&[0x00, 0x09, status, 0x00], &[]);
            let decoded = FrameMetadata::decode(&mut buf).unwrap().unwrap();
            assert_eq!(decoded.session_id, 9, "{name}");
            assert!(decoded.target.is_none(), "{name}");
            assert!(decoded.network.is_none(), "{name}");
            assert!(buf.is_empty(), "{name} envelope must be consumed");
        }
    }
}
