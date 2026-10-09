//! Shared TLS handshake parsers for TLS, REALITY, and Vision.
//!
//! These parsers inspect the first ClientHello of an incoming TLS connection
//! and the ServerHello sent by a decoy destination. They are protocol-generic.

use crate::async_stream::AsyncStream;
use crate::buf_reader::BufReader;
use crate::stream_reader::StreamReader;

const TLS_HEADER_LEN: usize = 5;

// the limit should be 5 (header) + 2^14 + 256 (AEAD encryption overhead) = 16640,
// although draft-mattsson-tls-super-jumbo-record-limit-01 would increase that.
// we set the limit to 5 + u16::MAX to allow for the maximum possible record size.
const TLS_FRAME_MAX_LEN: usize = TLS_HEADER_LEN + 65535;

const CONTENT_TYPE_HANDSHAKE: u8 = 0x16;

const HANDSHAKE_TYPE_SERVER_HELLO: u8 = 0x02;
const HANDSHAKE_TYPE_CLIENT_HELLO: u8 = 0x01;

const TLS_EXT_SUPPORTED_VERSIONS: u16 = 0x002b;

// retry request random value, see https://datatracker.ietf.org/doc/html/rfc8446#section-4.1.3
// TODO: should we also check to disallow TLS1.2/TLS1.1 client downgrade requests?
const RETRY_REQUEST_RANDOM_BYTES: [u8; 32] = [
    0xCF, 0x21, 0xAD, 0x74, 0xE5, 0x9A, 0x61, 0x11, 0xBE, 0x1D, 0x8C, 0x02, 0x1E, 0x65, 0xB8, 0x91,
    0xC2, 0xA2, 0x11, 0x16, 0x7A, 0xBB, 0x8C, 0x5E, 0x07, 0x9E, 0x09, 0xE2, 0xC8, 0xA8, 0x33, 0x9C,
];

pub struct ParsedClientHello {
    pub client_hello_frame: Vec<u8>,
    pub client_reader: StreamReader,
    pub requested_server_name: Option<String>,
    pub supports_tls13: bool,
}

#[inline]
pub async fn read_client_hello(
    server_stream: &mut Box<dyn AsyncStream>,
) -> std::io::Result<ParsedClientHello> {
    let mut client_reader = StreamReader::new_with_buffer_size(TLS_FRAME_MAX_LEN);

    // Allocates to allow borrowing the payload below.
    let client_tls_header_bytes = client_reader
        .read_slice(server_stream, TLS_HEADER_LEN)
        .await?
        .to_vec();

    let client_content_type = client_tls_header_bytes[0];
    if client_content_type != CONTENT_TYPE_HANDSHAKE {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            "expected client handshake",
        ));
    }

    let client_payload_len =
        u16::from_be_bytes([client_tls_header_bytes[3], client_tls_header_bytes[4]]) as usize;
    let client_payload_bytes = client_reader
        .read_slice(server_stream, client_payload_len)
        .await?;

    let mut client_hello = BufReader::new(client_payload_bytes);
    if client_hello.read_u8()? != HANDSHAKE_TYPE_CLIENT_HELLO {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            "expected ClientHello",
        ));
    }

    let client_hello_message_len = client_hello.read_u24_be()? as usize;
    // this should be 4 bytes less than the payload length (handshake type + 3 bytes length)
    if client_hello_message_len + 4 != client_payload_len {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            "client hello message length mismatch",
        ));
    }

    let client_version_major = client_hello.read_u8()?;
    let client_version_minor = client_hello.read_u8()?;
    let record_protocol_version_ok = client_version_major == 0x03
        && (client_version_minor == 0x01 || client_version_minor == 0x03);
    if !record_protocol_version_ok {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            format!(
                "unexpected ClientHello TLS version {client_version_major}.{client_version_minor}"
            ),
        ));
    }

    client_hello.skip(32)?; // client random

    let client_session_id_len = client_hello.read_u8()?;
    if client_session_id_len > 0 {
        client_hello.skip(client_session_id_len as usize)?;
    }

    let client_cipher_suite_len = client_hello.read_u16_be()?;
    client_hello.skip(client_cipher_suite_len as usize)?;

    let client_compression_method_len = client_hello.read_u8()?;
    client_hello.skip(client_compression_method_len as usize)?;

    let client_extensions_len = client_hello.read_u16_be()?;
    let client_extension_bytes = client_hello.read_slice(client_extensions_len as usize)?;

    let mut client_extensions = BufReader::new(client_extension_bytes);

    let mut requested_server_name: Option<String> = None;
    let mut client_supports_tls13 = false;

    while !client_extensions.is_consumed() {
        let extension_type = client_extensions.read_u16_be()?;
        let extension_len = client_extensions.read_u16_be()? as usize;

        if extension_type == 0x0000 {
            // server_name
            if requested_server_name.is_some() {
                return Err(std::io::Error::new(
                    std::io::ErrorKind::InvalidData,
                    "multiple server names",
                ));
            }
            // TODO: assert lengths
            let _server_name_list_len = client_extensions.read_u16_be()?;
            let server_name_type = client_extensions.read_u8()?;
            if server_name_type != 0 {
                return Err(std::io::Error::new(
                    std::io::ErrorKind::InvalidData,
                    "expected server name type to be hostname (0)",
                ));
            }
            let server_name_len = client_extensions.read_u16_be()?;
            let server_name_str = client_extensions.read_str(server_name_len as usize)?;
            requested_server_name = Some(server_name_str.to_string());
        } else if extension_type == TLS_EXT_SUPPORTED_VERSIONS {
            // supported_versions
            let version_list_len = client_extensions.read_u8()?;
            if version_list_len % 2 != 0 {
                return Err(std::io::Error::new(
                    std::io::ErrorKind::InvalidData,
                    format!("invalid odd version list length: 0x{version_list_len:02x}"),
                ));
            }
            let version_list_bytes = client_extensions.read_slice(version_list_len as usize)?;
            for i in (0..version_list_bytes.len()).step_by(2) {
                let version_major = version_list_bytes[i];
                let version_minor = version_list_bytes[i + 1];
                if version_major == 3 && version_minor == 4 {
                    client_supports_tls13 = true;
                    break;
                }
            }
        } else {
            client_extensions.skip(extension_len)?;
        }
    }

    let mut client_hello_frame =
        Vec::with_capacity(client_tls_header_bytes.len() + client_payload_bytes.len());
    client_hello_frame.extend_from_slice(&client_tls_header_bytes);
    client_hello_frame.extend_from_slice(client_payload_bytes);

    Ok(ParsedClientHello {
        client_hello_frame,
        client_reader,
        requested_server_name,
        supports_tls13: client_supports_tls13,
    })
}

pub struct ParsedServerHello {
    pub cipher_suite: u16,
    pub is_tls13: bool,
}

/// Parses a ServerHello frame and extracts relevant fields.
/// This is a generic parser that can be used by multiple protocols (TLS, REALITY, Vision).
/// It performs strict validation on structure but is lenient on TLS version requirements.
pub fn parse_server_hello(server_hello_frame: &[u8]) -> std::io::Result<ParsedServerHello> {
    // Minimum size when session_id_len=0 and no extensions:
    // 5 (record header) + 4 (handshake header) + 2 (version) + 32 (random)
    // + 1 (session_id_len byte) + 2 (cipher) + 1 (compression) = 47
    if server_hello_frame.len() < 47 {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            "ServerHello frame too short",
        ));
    }

    let content_type = server_hello_frame[0];
    if content_type != CONTENT_TYPE_HANDSHAKE {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            "expected handshake content type",
        ));
    }

    let record_version_major = server_hello_frame[1];
    let record_version_minor = server_hello_frame[2];
    if record_version_major != 3 || record_version_minor != 3 {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            format!("unexpected record TLS version {record_version_major}.{record_version_minor}"),
        ));
    }

    let mut reader = BufReader::new(&server_hello_frame[TLS_HEADER_LEN..]);

    let handshake_type = reader.read_u8()?;
    if handshake_type != HANDSHAKE_TYPE_SERVER_HELLO {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            "expected ServerHello handshake type",
        ));
    }

    let message_len = reader.read_u24_be()? as usize;
    if reader.remaining() < message_len {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            "ServerHello message length exceeds frame",
        ));
    }

    // Legacy version (should be 0x0303 for TLS 1.2/1.3)
    let version_major = reader.read_u8()?;
    let version_minor = reader.read_u8()?;
    if version_major != 3 || version_minor != 3 {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            format!("expected TLS version 3.3, got {version_major}.{version_minor}"),
        ));
    }

    let server_random = reader.read_slice(32)?;
    if server_random == RETRY_REQUEST_RANDOM_BYTES {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            "server sent a HelloRetryRequest",
        ));
    }

    // Session ID (variable length, 0-32 bytes)
    let session_id_len = reader.read_u8()?;
    if session_id_len > 32 {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            format!("invalid session_id_len {session_id_len}, max is 32"),
        ));
    }
    reader.skip(session_id_len as usize)?;

    let cipher_suite = reader.read_u16_be()?;
    reader.skip(1)?; // compression method
    let mut is_tls13 = false;
    if !reader.is_consumed() {
        let extensions_len = reader.read_u16_be()? as usize;
        if reader.remaining() < extensions_len {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                "extensions length exceeds remaining data",
            ));
        }

        let extensions_data = reader.read_slice(extensions_len)?;
        let mut ext_reader = BufReader::new(extensions_data);

        while !ext_reader.is_consumed() {
            let ext_type = ext_reader.read_u16_be()?;
            let ext_len = ext_reader.read_u16_be()?;

            if ext_type == TLS_EXT_SUPPORTED_VERSIONS {
                // In ServerHello, supported_versions is exactly 2 bytes (single selected version).
                if ext_len != 2 {
                    return Err(std::io::Error::new(
                        std::io::ErrorKind::InvalidData,
                        format!("supported_versions extension should be 2 bytes, got {ext_len}"),
                    ));
                }
                let version_bytes = ext_reader.read_slice(2)?;
                is_tls13 = version_bytes[0] == 0x03 && version_bytes[1] == 0x04; // TLS 1.3
            } else {
                ext_reader.skip(ext_len as usize)?;
            }
        }
    }

    Ok(ParsedServerHello {
        cipher_suite,
        is_tls13,
    })
}
