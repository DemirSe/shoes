use std::net::{Ipv4Addr, Ipv6Addr};
use std::sync::LazyLock;

use tokio::io::AsyncReadExt;

use crate::address::{Address, NetLocation};
use crate::stream_reader::StreamReader;

// VLESS protocol command types
pub const COMMAND_TCP: u8 = 1;
pub const COMMAND_UDP: u8 = 2;
pub const COMMAND_MUX: u8 = 3; // Also known as XUDP - multiplexes UDP over single TCP connection

pub const XTLS_VISION_FLOW: &str = "xtls-rprx-vision";

pub async fn parse_addons_from_reader<S: AsyncReadExt + Unpin>(
    stream_reader: &mut StreamReader,
    stream: &mut S,
    addon_length: u8,
) -> std::io::Result<String> {
    let addon_bytes = stream_reader
        .read_slice(stream, addon_length as usize)
        .await?;

    log::debug!("Parsing addons: length={}", addon_length);

    // Parse protobuf-encoded addons
    // Format: field_tag length data [field_tag length data ...]
    // Field 1 = flow (string)
    // Field 2 = seed (bytes)

    let mut addon_cursor = 0;
    let mut flow_string = String::new();

    while addon_cursor < addon_bytes.len() {
        // Read field tag
        let field_tag = addon_bytes[addon_cursor];
        addon_cursor += 1;

        let field_number = field_tag >> 3;
        let wire_type = field_tag & 0x07;

        if wire_type != 2 {
            return Err(std::io::Error::other(format!(
                "Unexpected wire type {} for field {}",
                wire_type, field_number
            )));
        }

        // Read length
        if addon_cursor >= addon_bytes.len() {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                "Unexpected end of addon data reading length",
            ));
        }
        let (field_length, bytes_used) = read_varint(&addon_bytes[addon_cursor..])?;
        let field_length = usize::try_from(field_length).map_err(|_| {
            std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                "Addon field length does not fit in a usize",
            )
        })?;

        // The length varint consumes at most MAX_VARINT_BYTES, so `field_start` cannot
        // wrap; the field length itself still needs checked arithmetic.
        let field_start = addon_cursor + bytes_used;
        let field_end = field_start
            .checked_add(field_length)
            .filter(|field_end| *field_end <= addon_bytes.len())
            .ok_or_else(|| {
                std::io::Error::new(
                    std::io::ErrorKind::InvalidData,
                    format!(
                        "Field {} length {} exceeds remaining addon bytes (cursor: {}, total: {})",
                        field_number,
                        field_length,
                        field_start,
                        addon_bytes.len()
                    ),
                )
            })?;

        // Read field data
        let field_data = &addon_bytes[field_start..field_end];
        addon_cursor = field_end;

        match field_number {
            1 => {
                // Flow field
                flow_string = std::str::from_utf8(field_data)
                    .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e))?
                    .to_string();
                log::debug!("Parsed flow: {}", flow_string);
            }
            2 => {
                // Seed field (ignored for now)
                log::debug!("Parsed seed: {} bytes", field_data.len());
            }
            _ => {
                log::debug!(
                    "Unknown field {}, skipping {} bytes",
                    field_number,
                    field_data.len()
                );
            }
        }
    }

    Ok(flow_string)
}

/// Parse VLESS remote location from stream
pub async fn parse_remote_location_from_reader<S: AsyncReadExt + Unpin>(
    stream_reader: &mut StreamReader,
    stream: &mut S,
) -> std::io::Result<NetLocation> {
    let port = stream_reader.read_u16_be(stream).await?;
    let address_type = stream_reader.read_u8(stream).await?;

    match address_type {
        1 => {
            // 4 byte ipv4 address
            let address_bytes = stream_reader.read_slice(stream, 4).await?;
            let v4addr = Ipv4Addr::new(
                address_bytes[0],
                address_bytes[1],
                address_bytes[2],
                address_bytes[3],
            );
            Ok(NetLocation::new(Address::Ipv4(v4addr), port))
        }
        2 => {
            // domain name
            let domain_name_len = stream_reader.read_u8(stream).await?;
            let domain_name_bytes = stream_reader
                .read_slice(stream, domain_name_len as usize)
                .await?;

            let address_str = std::str::from_utf8(domain_name_bytes).map_err(|e| {
                std::io::Error::new(
                    std::io::ErrorKind::InvalidData,
                    format!("Failed to decode address: {e}"),
                )
            })?;

            Ok(NetLocation::new(Address::from(address_str)?, port))
        }
        3 => {
            // 16 byte ipv6 address
            let address_bytes = stream_reader.read_slice(stream, 16).await?;
            let v6addr = Ipv6Addr::new(
                ((address_bytes[0] as u16) << 8) | (address_bytes[1] as u16),
                ((address_bytes[2] as u16) << 8) | (address_bytes[3] as u16),
                ((address_bytes[4] as u16) << 8) | (address_bytes[5] as u16),
                ((address_bytes[6] as u16) << 8) | (address_bytes[7] as u16),
                ((address_bytes[8] as u16) << 8) | (address_bytes[9] as u16),
                ((address_bytes[10] as u16) << 8) | (address_bytes[11] as u16),
                ((address_bytes[12] as u16) << 8) | (address_bytes[13] as u16),
                ((address_bytes[14] as u16) << 8) | (address_bytes[15] as u16),
            );

            Ok(NetLocation::new(Address::Ipv6(v6addr), port))
        }
        invalid_type => Err(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            format!("Invalid address type: {invalid_type}"),
        )),
    }
}

pub fn vision_flow_addon_data() -> &'static [u8] {
    static INSTANCE: LazyLock<Vec<u8>> = LazyLock::new(|| {
        encode_flow_addon(XTLS_VISION_FLOW)
            .expect("Failed to encode vision flow addon at initialization")
    });
    &INSTANCE
}

/// A protobuf varint holds a u64 in at most ten continuation-grouped bytes.
const MAX_VARINT_BYTES: usize = 10;

/// Decode a protobuf base-128 varint into a u64.
///
/// Returns the decoded value and the number of consumed bytes. Empty,
/// unterminated, over-long, and overflowing inputs are rejected with
/// `InvalidData` instead of panicking or wrapping.
fn read_varint(data: &[u8]) -> std::io::Result<(u64, usize)> {
    let mut value = 0u64;
    let mut cursor = 0usize;
    while cursor < data.len().min(MAX_VARINT_BYTES) {
        let byte = data[cursor];
        if cursor == MAX_VARINT_BYTES - 1 && byte > 0x01 {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                "Varint value does not fit in u64",
            ));
        }
        value |= u64::from(byte & 0x7f) << (7 * cursor);
        cursor += 1;
        if byte & 0x80 == 0 {
            return Ok((value, cursor));
        }
    }

    Err(std::io::Error::new(
        std::io::ErrorKind::InvalidData,
        if cursor == MAX_VARINT_BYTES {
            "Varint is longer than ten bytes"
        } else {
            "Varint is unterminated"
        },
    ))
}

/// Encode a flow string as protobuf addon data
/// Format: field_tag(0x0a) + length + data
/// Field 1 = flow (string), wire type 2 (length-delimited)
fn encode_flow_addon(flow: &str) -> std::io::Result<Vec<u8>> {
    let flow_bytes = flow.as_bytes();
    let flow_len = flow_bytes.len();

    if flow_len > 127 {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            "Flow string too long for simple varint encoding",
        ));
    }

    let mut result = Vec::new();

    // Field 1, wire type 2 (0x0a = (1 << 3) | 2)
    result.push(0x0a);

    // Length as varint (simple case: < 128)
    result.push(flow_len as u8);

    // Flow string data
    result.extend_from_slice(flow_bytes);

    Ok(result)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The VLESS handler reads addons through `StreamReader::new_with_buffer_size(800)`.
    const ADDON_BUFFER_SIZE: usize = 800;
    /// A protobuf varint carries at most ten bytes of a u64.
    const MAX_VARINT_BYTES: usize = 10;

    async fn parse_addons(addon_bytes: &[u8]) -> std::io::Result<String> {
        let mut stream_reader = StreamReader::new_with_buffer_size(ADDON_BUFFER_SIZE);
        let mut stream = addon_bytes;
        parse_addons_from_reader(&mut stream_reader, &mut stream, addon_bytes.len() as u8).await
    }

    fn encode_varint(mut value: u64) -> Vec<u8> {
        let mut encoded = Vec::new();
        loop {
            let byte = (value & 0x7f) as u8;
            value >>= 7;
            if value == 0 {
                encoded.push(byte);
                return encoded;
            }
            encoded.push(byte | 0x80);
        }
    }

    fn length_delimited_field(field_number: u8, payload: &[u8]) -> Vec<u8> {
        let mut field = vec![(field_number << 3) | 2];
        field.extend_from_slice(&encode_varint(payload.len() as u64));
        field.extend_from_slice(payload);
        field
    }

    #[test]
    fn parser_boundary_reads_single_byte_varint() {
        assert_eq!(read_varint(&[0x7f]).expect("one byte varint"), (127, 1));
    }

    #[test]
    fn parser_boundary_reads_little_endian_seven_bit_groups() {
        assert_eq!(read_varint(&[0x80, 0x01]).expect("128"), (128, 2));
        assert_eq!(read_varint(&[0xff, 0x7f]).expect("16383"), (16_383, 2));
    }

    #[test]
    fn parser_boundary_accepts_max_u64_varint() {
        let mut max_varint = [0xffu8; MAX_VARINT_BYTES];
        max_varint[MAX_VARINT_BYTES - 1] = 0x01;
        assert_eq!(
            read_varint(&max_varint).expect("u64::MAX is a valid varint"),
            (u64::MAX, MAX_VARINT_BYTES)
        );
    }

    #[test]
    fn parser_boundary_rejects_empty_varint() {
        let error = read_varint(&[]).expect_err("empty input holds no varint");
        assert_eq!(error.kind(), std::io::ErrorKind::InvalidData);
    }

    #[test]
    fn parser_boundary_rejects_continuation_only_varints() {
        for bytes in [[0x80u8].as_slice(), [0x80, 0x80, 0x80].as_slice()] {
            let error = read_varint(bytes).expect_err("unterminated varint must be rejected");
            assert_eq!(error.kind(), std::io::ErrorKind::InvalidData);
        }

        let all_continuation = [0x80u8; MAX_VARINT_BYTES];
        let error =
            read_varint(&all_continuation).expect_err("ten continuation bytes cannot end a varint");
        assert_eq!(error.kind(), std::io::ErrorKind::InvalidData);
    }

    #[test]
    fn parser_boundary_rejects_overflowing_varints() {
        let mut overflow = [0xffu8; MAX_VARINT_BYTES];
        overflow[MAX_VARINT_BYTES - 1] = 0x02;
        let error = read_varint(&overflow).expect_err("a tenth byte above 0x01 overflows u64");
        assert_eq!(error.kind(), std::io::ErrorKind::InvalidData);

        let too_long = [0xffu8; MAX_VARINT_BYTES + 1];
        let error = read_varint(&too_long).expect_err("eleven bytes cannot encode u64");
        assert_eq!(error.kind(), std::io::ErrorKind::InvalidData);
    }

    #[tokio::test]
    async fn parser_boundary_reads_complete_vision_flow_addon() {
        let addon = vision_flow_addon_data();
        assert_eq!(addon, b"\x0a\x10xtls-rprx-vision");
        assert_eq!(
            parse_addons(addon).await.expect("vision addon must parse"),
            XTLS_VISION_FLOW
        );
    }

    #[tokio::test]
    async fn parser_boundary_reads_complete_fields_at_127_and_128_bytes() {
        let short_field = length_delimited_field(2, &[0x5au8; 127]);
        assert_eq!(&short_field[..2], &[0x12, 0x7f]);
        assert_eq!(
            parse_addons(&short_field).await.expect("127-byte field"),
            ""
        );

        let long_field = length_delimited_field(2, &[0xa5u8; 128]);
        assert_eq!(&long_field[..3], &[0x12, 0x80, 0x01]);
        assert_eq!(parse_addons(&long_field).await.expect("128-byte field"), "");
    }

    #[tokio::test]
    async fn parser_boundary_rejects_truncated_addon_length() {
        let error = parse_addons(&[0x0a, 0x80])
            .await
            .expect_err("a truncated continuation length must be rejected");
        assert_eq!(error.kind(), std::io::ErrorKind::InvalidData);
    }

    #[tokio::test]
    async fn parser_boundary_rejects_continuation_only_addon_lengths() {
        let cases: [&[u8]; 2] = [&[0x0a, 0x80, 0x80], &[0x0a, 0x80, 0x80, 0x80]];
        for addon in cases {
            let error = parse_addons(addon)
                .await
                .expect_err("a continuation-only field length must be rejected");
            assert_eq!(error.kind(), std::io::ErrorKind::InvalidData);
        }
    }

    #[tokio::test]
    async fn parser_boundary_rejects_field_length_past_the_addon_window() {
        let mut addon = vec![0x0a, 0x80, 0x01];
        addon.extend_from_slice(&[0x61u8; 10]);
        let error = parse_addons(&addon)
            .await
            .expect_err("a field longer than the addon window must be rejected");
        assert_eq!(error.kind(), std::io::ErrorKind::InvalidData);
    }

    #[tokio::test]
    async fn parser_boundary_rejects_field_length_that_overflows_the_addon_cursor() {
        let mut addon = vec![0x0a];
        addon.extend_from_slice(&[0xffu8; MAX_VARINT_BYTES - 1]);
        addon.push(0x01);
        assert_eq!(addon.len(), 1 + MAX_VARINT_BYTES);
        let error = parse_addons(&addon)
            .await
            .expect_err("a u64::MAX field length must be rejected");
        assert_eq!(error.kind(), std::io::ErrorKind::InvalidData);
    }

    #[tokio::test]
    async fn parser_boundary_keeps_unknown_length_delimited_fields() {
        let unknown = length_delimited_field(7, &[0xde, 0xad, 0xbe, 0xef]);
        assert_eq!(parse_addons(&unknown).await.expect("unknown field"), "");

        let mut flow_with_unknown_tail = length_delimited_field(1, XTLS_VISION_FLOW.as_bytes());
        flow_with_unknown_tail.extend_from_slice(&length_delimited_field(9, &[0x00; 3]));
        assert_eq!(
            parse_addons(&flow_with_unknown_tail)
                .await
                .expect("flow field with an unknown tail"),
            XTLS_VISION_FLOW
        );
    }
}
