//! SagerNet packet-address UDP-over-TCP framing shared with h2mux `packet_addr` mode.
//!
//! h2mux `packet_addr` streams carry per-packet destination addresses using the
//! sing-mux SOCKS address serializer.
//!
//! ## Address Format (SOCKS-style)
//! - 0x01: IPv4 (4 bytes)
//! - 0x03: Domain (1 byte length + domain)
//! - 0x04: IPv6 (16 bytes)
//!
//! Frame format: `[SOCKS5 address][length:u16be][data]`

mod socks_addr;
mod uot_v1_server_stream;

pub use uot_v1_server_stream::SocksPacketAddrStream;
