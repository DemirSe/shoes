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
    if client_hello.remaining() != client_extensions_len as usize {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            "ClientHello has trailing bytes after the extension block",
        ));
    }
    let client_extension_bytes = client_hello.read_slice(client_extensions_len as usize)?;

    let mut client_extensions = BufReader::new(client_extension_bytes);

    let mut requested_server_name: Option<String> = None;
    let mut client_supports_tls13 = false;

    while !client_extensions.is_consumed() {
        let extension_type = client_extensions.read_u16_be()?;
        let extension_len = client_extensions.read_u16_be()? as usize;
        // Each extension body is read from its own declared slice, so bytes
        // from a neighboring extension can never satisfy a nested length.
        let extension_bytes = client_extensions.read_slice(extension_len)?;

        if extension_type == 0x0000 {
            // server_name
            if requested_server_name.is_some() {
                return Err(std::io::Error::new(
                    std::io::ErrorKind::InvalidData,
                    "multiple server names",
                ));
            }
            let mut server_name_list = BufReader::new(extension_bytes);
            let server_name_list_len = server_name_list.read_u16_be()? as usize;
            if server_name_list_len == 0 || server_name_list_len + 2 != extension_len {
                return Err(std::io::Error::new(
                    std::io::ErrorKind::InvalidData,
                    format!(
                        "server_name list length {server_name_list_len} does not match extension length {extension_len}"
                    ),
                ));
            }
            let server_name_type = server_name_list.read_u8()?;
            if server_name_type != 0 {
                return Err(std::io::Error::new(
                    std::io::ErrorKind::InvalidData,
                    "expected server name type to be hostname (0)",
                ));
            }
            let server_name_len = server_name_list.read_u16_be()? as usize;
            if server_name_len == 0 || server_name_len + 3 != server_name_list_len {
                return Err(std::io::Error::new(
                    std::io::ErrorKind::InvalidData,
                    format!(
                        "server_name entry length {server_name_len} does not match list length {server_name_list_len}"
                    ),
                ));
            }
            let server_name_str = server_name_list.read_str(server_name_len)?;
            requested_server_name = Some(server_name_str.to_string());
        } else if extension_type == TLS_EXT_SUPPORTED_VERSIONS {
            // supported_versions
            let mut supported_versions = BufReader::new(extension_bytes);
            let version_list_len = supported_versions.read_u8()? as usize;
            if version_list_len == 0
                || !version_list_len.is_multiple_of(2)
                || version_list_len + 1 != extension_len
            {
                return Err(std::io::Error::new(
                    std::io::ErrorKind::InvalidData,
                    format!(
                        "invalid version list length {version_list_len} for extension length {extension_len}"
                    ),
                ));
            }
            let version_list_bytes = supported_versions.read_slice(version_list_len)?;
            for i in (0..version_list_bytes.len()).step_by(2) {
                let version_major = version_list_bytes[i];
                let version_minor = version_list_bytes[i + 1];
                if version_major == 3 && version_minor == 4 {
                    client_supports_tls13 = true;
                    break;
                }
            }
        }
        // Unknown extensions stay safely skippable; their body was already
        // bounded by the slice above and is dropped here.
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

#[cfg(test)]
mod parser_boundary_tests {
    use std::time::Duration;

    use tokio::io::AsyncWriteExt;

    use super::*;

    const SCENARIO_TIMEOUT: Duration = Duration::from_secs(5);
    const PRODUCER_TIMEOUT: Duration = Duration::from_secs(5);
    const DUPLEX_CAPACITY: usize = 256 * 1024;
    const HEALTHY_HOSTNAME: &str = "whatsapp.net";
    const SNI_EXTENSION_TYPE: u16 = 0x0000;
    const GREASE_EXTENSION_TYPE: u16 = 0x0a0a;
    const ZERO_LENGTH_UNKNOWN_EXTENSION_TYPE: u16 = 0x0033;
    const UNKNOWN_EXTENSION_TYPE: u16 = 0x1a1a;

    enum HelloOutcome {
        Parsed(ParsedClientHello),
        Rejected(std::io::Error),
    }

    impl HelloOutcome {
        fn expect_parsed(self) -> ParsedClientHello {
            match self {
                HelloOutcome::Parsed(parsed) => parsed,
                HelloOutcome::Rejected(error) => {
                    panic!("expected parsed ClientHello, got rejection: {error}")
                }
            }
        }

        fn expect_rejected(self) -> std::io::Error {
            match self {
                HelloOutcome::Parsed(_) => {
                    panic!("expected ClientHello rejection, but the hello parsed")
                }
                HelloOutcome::Rejected(error) => {
                    println!("parser_boundary rejected as expected: {error}");
                    error
                }
            }
        }
    }

    fn push_u16(buffer: &mut Vec<u8>, value: u16) {
        buffer.extend_from_slice(&value.to_be_bytes());
    }

    fn push_extension(buffer: &mut Vec<u8>, extension_type: u16, body: &[u8]) {
        push_u16(buffer, extension_type);
        push_u16(buffer, body.len() as u16);
        buffer.extend_from_slice(body);
    }

    fn sni_extension_body(hostname: &str) -> Vec<u8> {
        let mut body = Vec::new();
        push_u16(&mut body, (1 + 2 + hostname.len()) as u16);
        body.push(0); // host_name
        push_u16(&mut body, hostname.len() as u16);
        body.extend_from_slice(hostname.as_bytes());
        body
    }

    fn sni_extension_header(declared_len: u16) -> Vec<u8> {
        let mut header = Vec::new();
        push_u16(&mut header, SNI_EXTENSION_TYPE);
        push_u16(&mut header, declared_len);
        header
    }

    fn supported_versions_extension_body(versions: &[(u8, u8)]) -> Vec<u8> {
        let mut body = Vec::new();
        body.push((versions.len() * 2) as u8);
        for (major, minor) in versions {
            body.push(*major);
            body.push(*minor);
        }
        body
    }

    fn client_hello_frame_with_declared_extensions_len(
        extensions: &[u8],
        declared_extensions_len: u16,
    ) -> Vec<u8> {
        let mut body = Vec::new();
        body.push(HANDSHAKE_TYPE_CLIENT_HELLO);
        body.extend_from_slice(&[0, 0, 0]); // handshake length placeholder
        push_u16(&mut body, 0x0303); // legacy client version
        body.extend_from_slice(&[0x42; 32]); // client random
        body.push(0); // session id length
        push_u16(&mut body, 2); // cipher suite length
        push_u16(&mut body, 0x1301); // TLS_AES_128_GCM_SHA256
        body.push(1); // compression method length
        body.push(0); // null compression
        push_u16(&mut body, declared_extensions_len);
        body.extend_from_slice(extensions);
        let handshake_len = (body.len() - 4) as u32;
        body[1..4].copy_from_slice(&handshake_len.to_be_bytes()[1..]);

        let mut frame = Vec::new();
        frame.push(CONTENT_TYPE_HANDSHAKE);
        frame.extend_from_slice(&[0x03, 0x01]); // legacy record version
        push_u16(&mut frame, body.len() as u16);
        frame.extend_from_slice(&body);
        frame
    }

    fn client_hello_frame(extensions: &[u8]) -> Vec<u8> {
        client_hello_frame_with_declared_extensions_len(extensions, extensions.len() as u16)
    }

    /// Feeds one frame through `read_client_hello` with a bounded producer task.
    ///
    /// The producer timeout, the JoinHandle result, and the write error all
    /// propagate as scenario errors so a hang or a failed write cannot be
    /// mistaken for a parser rejection.
    async fn parse_frame(frame: Vec<u8>) -> Result<HelloOutcome, String> {
        let (mut producer_stream, server_stream) = tokio::io::duplex(DUPLEX_CAPACITY);
        let producer = tokio::spawn(async move {
            match tokio::time::timeout(PRODUCER_TIMEOUT, producer_stream.write_all(&frame)).await {
                Ok(Ok(())) => Ok(()),
                Ok(Err(error)) => Err(format!("producer write failed: {error}")),
                Err(_) => Err("producer write timed out".to_string()),
            }
        });

        let mut server_stream: Box<dyn AsyncStream> = Box::new(server_stream);
        let parsed =
            tokio::time::timeout(SCENARIO_TIMEOUT, read_client_hello(&mut server_stream)).await;

        // Join the producer and propagate producer failures before judging the parser.
        producer
            .await
            .map_err(|error| format!("producer task failed: {error}"))??;

        match parsed {
            Ok(Ok(parsed)) => Ok(HelloOutcome::Parsed(parsed)),
            Ok(Err(error)) => Ok(HelloOutcome::Rejected(error)),
            Err(_) => Err("ClientHello parse timed out".to_string()),
        }
    }

    #[tokio::test]
    async fn parser_boundary_accepts_tls13_hello_with_unknown_extensions() {
        let mut extensions = Vec::new();
        push_extension(&mut extensions, GREASE_EXTENSION_TYPE, &[0x00; 4]);
        push_extension(&mut extensions, ZERO_LENGTH_UNKNOWN_EXTENSION_TYPE, &[]);
        push_extension(
            &mut extensions,
            SNI_EXTENSION_TYPE,
            &sni_extension_body(HEALTHY_HOSTNAME),
        );
        push_extension(
            &mut extensions,
            TLS_EXT_SUPPORTED_VERSIONS,
            &supported_versions_extension_body(&[(3, 4), (3, 3)]),
        );
        push_extension(&mut extensions, UNKNOWN_EXTENSION_TYPE, &[1, 2, 3]);
        let frame = client_hello_frame(&extensions);

        let parsed = parse_frame(frame.clone())
            .await
            .expect("scenario failed")
            .expect_parsed();
        assert_eq!(
            parsed.requested_server_name.as_deref(),
            Some(HEALTHY_HOSTNAME)
        );
        assert!(parsed.supports_tls13);
        assert_eq!(parsed.client_hello_frame, frame);
    }

    #[tokio::test]
    async fn parser_boundary_accepts_tls12_hello_without_supported_versions() {
        let mut extensions = Vec::new();
        push_extension(
            &mut extensions,
            SNI_EXTENSION_TYPE,
            &sni_extension_body(HEALTHY_HOSTNAME),
        );
        push_extension(&mut extensions, UNKNOWN_EXTENSION_TYPE, &[]);
        let frame = client_hello_frame(&extensions);

        let parsed = parse_frame(frame.clone())
            .await
            .expect("scenario failed")
            .expect_parsed();
        assert_eq!(
            parsed.requested_server_name.as_deref(),
            Some(HEALTHY_HOSTNAME)
        );
        assert!(!parsed.supports_tls13);
        assert_eq!(parsed.client_hello_frame, frame);
    }

    #[tokio::test]
    async fn parser_boundary_accepts_tls12_supported_versions_list() {
        let mut extensions = Vec::new();
        push_extension(
            &mut extensions,
            SNI_EXTENSION_TYPE,
            &sni_extension_body(HEALTHY_HOSTNAME),
        );
        push_extension(
            &mut extensions,
            TLS_EXT_SUPPORTED_VERSIONS,
            &supported_versions_extension_body(&[(3, 3), (3, 2)]),
        );
        let frame = client_hello_frame(&extensions);

        let parsed = parse_frame(frame.clone())
            .await
            .expect("scenario failed")
            .expect_parsed();
        assert_eq!(
            parsed.requested_server_name.as_deref(),
            Some(HEALTHY_HOSTNAME)
        );
        assert!(!parsed.supports_tls13);
        assert_eq!(parsed.client_hello_frame, frame);
    }

    #[tokio::test]
    async fn parser_boundary_rejects_sni_declared_length_zero_with_longer_body() {
        let mut extensions = Vec::new();
        extensions.extend_from_slice(&sni_extension_header(0));
        extensions.extend_from_slice(&sni_extension_body(HEALTHY_HOSTNAME));
        push_extension(
            &mut extensions,
            TLS_EXT_SUPPORTED_VERSIONS,
            &supported_versions_extension_body(&[(3, 4)]),
        );
        let frame = client_hello_frame(&extensions);

        parse_frame(frame)
            .await
            .expect("scenario failed")
            .expect_rejected();
    }

    #[tokio::test]
    async fn parser_boundary_rejects_sni_declared_length_two_with_longer_body() {
        let mut extensions = Vec::new();
        extensions.extend_from_slice(&sni_extension_header(2));
        extensions.extend_from_slice(&sni_extension_body(HEALTHY_HOSTNAME));
        push_extension(
            &mut extensions,
            TLS_EXT_SUPPORTED_VERSIONS,
            &supported_versions_extension_body(&[(3, 4)]),
        );
        let frame = client_hello_frame(&extensions);

        parse_frame(frame)
            .await
            .expect("scenario failed")
            .expect_rejected();
    }

    #[tokio::test]
    async fn parser_boundary_rejects_sni_borrowed_hostname_from_next_extension() {
        let mut extensions = Vec::new();
        extensions.extend_from_slice(&sni_extension_header(2));
        // The declared SNI body holds only the list length field. The remaining
        // SNI fields are physically the following extension's header and body.
        extensions.extend_from_slice(&sni_extension_body(HEALTHY_HOSTNAME));
        let frame = client_hello_frame(&extensions);

        parse_frame(frame)
            .await
            .expect("scenario failed")
            .expect_rejected();
    }

    #[tokio::test]
    async fn parser_boundary_rejects_sni_list_length_smaller_than_body() {
        let mut body = Vec::new();
        push_u16(&mut body, (1 + 2 + HEALTHY_HOSTNAME.len() - 1) as u16);
        body.push(0);
        push_u16(&mut body, HEALTHY_HOSTNAME.len() as u16);
        body.extend_from_slice(HEALTHY_HOSTNAME.as_bytes());
        let mut extensions = Vec::new();
        push_extension(&mut extensions, SNI_EXTENSION_TYPE, &body);
        push_extension(&mut extensions, UNKNOWN_EXTENSION_TYPE, &[0xaa, 0xbb]);
        let frame = client_hello_frame(&extensions);

        parse_frame(frame)
            .await
            .expect("scenario failed")
            .expect_rejected();
    }

    #[tokio::test]
    async fn parser_boundary_rejects_sni_list_length_larger_than_body() {
        let mut body = Vec::new();
        push_u16(&mut body, (1 + 2 + HEALTHY_HOSTNAME.len() + 4) as u16);
        body.push(0);
        push_u16(&mut body, HEALTHY_HOSTNAME.len() as u16);
        body.extend_from_slice(HEALTHY_HOSTNAME.as_bytes());
        let mut extensions = Vec::new();
        push_extension(&mut extensions, SNI_EXTENSION_TYPE, &body);
        push_extension(&mut extensions, UNKNOWN_EXTENSION_TYPE, &[0xaa, 0xbb]);
        let frame = client_hello_frame(&extensions);

        parse_frame(frame)
            .await
            .expect("scenario failed")
            .expect_rejected();
    }

    #[tokio::test]
    async fn parser_boundary_rejects_sni_trailing_bytes_in_extension() {
        let mut body = sni_extension_body(HEALTHY_HOSTNAME);
        body.push(0x00);
        let mut extensions = Vec::new();
        push_extension(&mut extensions, SNI_EXTENSION_TYPE, &body);
        push_extension(&mut extensions, UNKNOWN_EXTENSION_TYPE, &[0xaa, 0xbb]);
        let frame = client_hello_frame(&extensions);

        parse_frame(frame)
            .await
            .expect("scenario failed")
            .expect_rejected();
    }

    #[tokio::test]
    async fn parser_boundary_rejects_empty_sni_hostname() {
        let mut body = Vec::new();
        push_u16(&mut body, 3); // list length: entry header only
        body.push(0); // host_name
        push_u16(&mut body, 0); // empty hostname
        let mut extensions = Vec::new();
        push_extension(&mut extensions, SNI_EXTENSION_TYPE, &body);
        push_extension(&mut extensions, UNKNOWN_EXTENSION_TYPE, &[0xaa, 0xbb]);
        let frame = client_hello_frame(&extensions);

        parse_frame(frame)
            .await
            .expect("scenario failed")
            .expect_rejected();
    }

    #[tokio::test]
    async fn parser_boundary_rejects_sni_name_length_overrunning_extension() {
        let mut body = Vec::new();
        push_u16(&mut body, (1 + 2 + 12) as u16);
        body.push(0);
        push_u16(&mut body, 12);
        body.extend_from_slice(b"whats"); // only 5 of the declared 12 bytes
        let mut extensions = Vec::new();
        push_extension(&mut extensions, SNI_EXTENSION_TYPE, &body);
        // Bytes after the SNI container must not complete the hostname read.
        extensions.extend_from_slice(&[0x41, 0x41, 0x00, 0x07, 0x41, 0x41, 0x41]);
        let frame = client_hello_frame(&extensions);

        parse_frame(frame)
            .await
            .expect("scenario failed")
            .expect_rejected();
    }

    #[tokio::test]
    async fn parser_boundary_rejects_duplicate_sni_extensions() {
        let mut extensions = Vec::new();
        push_extension(
            &mut extensions,
            SNI_EXTENSION_TYPE,
            &sni_extension_body(HEALTHY_HOSTNAME),
        );
        push_extension(
            &mut extensions,
            SNI_EXTENSION_TYPE,
            &sni_extension_body("whatsapp.com"),
        );
        let frame = client_hello_frame(&extensions);

        parse_frame(frame)
            .await
            .expect("scenario failed")
            .expect_rejected();
    }

    #[tokio::test]
    async fn parser_boundary_rejects_duplicate_hostname_entries_in_sni_list() {
        let first = HEALTHY_HOSTNAME.as_bytes();
        let second = b"whatsapp.com";
        let mut body = Vec::new();
        push_u16(
            &mut body,
            (1 + 2 + first.len() + 1 + 2 + second.len()) as u16,
        );
        body.push(0);
        push_u16(&mut body, first.len() as u16);
        body.extend_from_slice(first);
        body.push(0);
        push_u16(&mut body, second.len() as u16);
        body.extend_from_slice(second);
        let mut extensions = Vec::new();
        push_extension(&mut extensions, SNI_EXTENSION_TYPE, &body);
        let frame = client_hello_frame(&extensions);

        parse_frame(frame)
            .await
            .expect("scenario failed")
            .expect_rejected();
    }

    #[tokio::test]
    async fn parser_boundary_rejects_supported_versions_zero_length_body() {
        let mut extensions = Vec::new();
        push_extension(&mut extensions, TLS_EXT_SUPPORTED_VERSIONS, &[]);
        // Bytes physically after the empty container must not become the list.
        extensions.extend_from_slice(&[0x02, 0x03, 0x04]);
        let frame = client_hello_frame(&extensions);

        parse_frame(frame)
            .await
            .expect("scenario failed")
            .expect_rejected();
    }

    #[tokio::test]
    async fn parser_boundary_rejects_supported_versions_empty_version_list() {
        let mut extensions = Vec::new();
        push_extension(&mut extensions, TLS_EXT_SUPPORTED_VERSIONS, &[0x00]);
        push_extension(&mut extensions, UNKNOWN_EXTENSION_TYPE, &[0xaa, 0xbb]);
        let frame = client_hello_frame(&extensions);

        parse_frame(frame)
            .await
            .expect("scenario failed")
            .expect_rejected();
    }

    #[tokio::test]
    async fn parser_boundary_rejects_supported_versions_odd_version_list() {
        let mut extensions = Vec::new();
        push_extension(
            &mut extensions,
            TLS_EXT_SUPPORTED_VERSIONS,
            &[0x03, 0x03, 0x04, 0x03],
        );
        push_extension(&mut extensions, UNKNOWN_EXTENSION_TYPE, &[0xaa, 0xbb]);
        let frame = client_hello_frame(&extensions);

        parse_frame(frame)
            .await
            .expect("scenario failed")
            .expect_rejected();
    }

    #[tokio::test]
    async fn parser_boundary_rejects_supported_versions_short_body() {
        let mut extensions = Vec::new();
        push_extension(&mut extensions, TLS_EXT_SUPPORTED_VERSIONS, &[0x02, 0x03]);
        push_extension(&mut extensions, UNKNOWN_EXTENSION_TYPE, &[0x04, 0x00]);
        let frame = client_hello_frame(&extensions);

        parse_frame(frame)
            .await
            .expect("scenario failed")
            .expect_rejected();
    }

    #[tokio::test]
    async fn parser_boundary_rejects_supported_versions_trailing_bytes() {
        let mut extensions = Vec::new();
        push_extension(
            &mut extensions,
            TLS_EXT_SUPPORTED_VERSIONS,
            &[0x02, 0x03, 0x04, 0xff, 0xff],
        );
        push_extension(&mut extensions, UNKNOWN_EXTENSION_TYPE, &[0xaa, 0xbb]);
        let frame = client_hello_frame(&extensions);

        parse_frame(frame)
            .await
            .expect("scenario failed")
            .expect_rejected();
    }

    #[tokio::test]
    async fn parser_boundary_rejects_supported_versions_borrowed_from_next_extension() {
        let mut extensions = Vec::new();
        push_extension(&mut extensions, TLS_EXT_SUPPORTED_VERSIONS, &[0x02]);
        // The declared container ends after the list length byte; these bytes
        // must not be read as the version list.
        extensions.extend_from_slice(&[0x03, 0x04]);
        let frame = client_hello_frame(&extensions);

        parse_frame(frame)
            .await
            .expect("scenario failed")
            .expect_rejected();
    }

    #[tokio::test]
    async fn parser_boundary_rejects_trailing_bytes_after_extension_block() {
        let mut extensions = Vec::new();
        push_extension(
            &mut extensions,
            SNI_EXTENSION_TYPE,
            &sni_extension_body(HEALTHY_HOSTNAME),
        );
        extensions.extend_from_slice(&[0xde, 0xad]);
        let declared_extensions_len = (extensions.len() - 2) as u16;
        let frame =
            client_hello_frame_with_declared_extensions_len(&extensions, declared_extensions_len);

        parse_frame(frame)
            .await
            .expect("scenario failed")
            .expect_rejected();
    }

    #[tokio::test]
    async fn parser_boundary_rejects_truncated_extension_block() {
        let mut extensions = Vec::new();
        push_extension(
            &mut extensions,
            SNI_EXTENSION_TYPE,
            &sni_extension_body(HEALTHY_HOSTNAME),
        );
        let declared_extensions_len = (extensions.len() + 2) as u16;
        let frame =
            client_hello_frame_with_declared_extensions_len(&extensions, declared_extensions_len);

        parse_frame(frame)
            .await
            .expect("scenario failed")
            .expect_rejected();
    }

    #[tokio::test]
    async fn parser_boundary_rejects_partial_extension_header() {
        let mut extensions = Vec::new();
        push_extension(
            &mut extensions,
            SNI_EXTENSION_TYPE,
            &sni_extension_body(HEALTHY_HOSTNAME),
        );
        extensions.extend_from_slice(&[0x00, 0x11]);
        let frame = client_hello_frame(&extensions);

        parse_frame(frame)
            .await
            .expect("scenario failed")
            .expect_rejected();
    }

    #[tokio::test]
    async fn parser_boundary_rejects_unknown_extension_body_past_block_end() {
        let mut extensions = Vec::new();
        push_extension(
            &mut extensions,
            SNI_EXTENSION_TYPE,
            &sni_extension_body(HEALTHY_HOSTNAME),
        );
        push_u16(&mut extensions, UNKNOWN_EXTENSION_TYPE);
        push_u16(&mut extensions, 16); // body extends past the declared block
        extensions.extend_from_slice(&[0xaa, 0xbb, 0xcc, 0xdd]);
        let frame = client_hello_frame(&extensions);

        parse_frame(frame)
            .await
            .expect("scenario failed")
            .expect_rejected();
    }
}
