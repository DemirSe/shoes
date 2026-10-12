use bytes::{BufMut, Bytes, BytesMut};
use rand::RngExt;
use std::io;

/// Largest content one padded packet can carry.
const MAX_CONTENT_LEN: usize = u16::MAX as usize;

// Padding parameters
const LONG_PADDING_MIN: usize = 900;
const LONG_PADDING_RANDOM_MAX: usize = 500;
const SHORT_PADDING_RANDOM_MAX: usize = 256;
const MAX_PADDING_SIZE: usize = 8171; // buf.Size - 21

pub fn pad_with_uuid_and_command(
    data: &[u8],
    uuid: &[u8; 16],
    command: u8,
    is_tls: bool,
) -> io::Result<Bytes> {
    pad(data, Some(uuid), command, is_tls)
}

pub fn pad_with_command(data: &[u8], command: u8, is_tls: bool) -> io::Result<Bytes> {
    pad(data, None, command, is_tls)
}

fn pad(data: &[u8], uuid: Option<&[u8; 16]>, command: u8, is_tls: bool) -> io::Result<Bytes> {
    // The packet header stores the content length in two bytes. A longer content
    // cannot be framed, so it is refused instead of silently truncated.
    let content_len = u16::try_from(data.len()).map_err(|_| {
        io::Error::new(
            io::ErrorKind::InvalidData,
            format!(
                "Vision padding content is {} bytes, the maximum is {MAX_CONTENT_LEN}",
                data.len()
            ),
        )
    })?;
    let padding_len = calculate_padding_length(content_len as usize, is_tls);

    let uuid_len = if uuid.is_some() { 16 } else { 0 };
    let total_size = uuid_len + 1 + 2 + 2 + data.len() + padding_len;

    let mut output = BytesMut::with_capacity(total_size);

    if let Some(uuid) = uuid {
        output.put_slice(uuid);
    }

    output.put_u8(command);
    output.put_u16(content_len);
    output.put_u16(padding_len as u16);
    output.put_slice(data);

    // Write random padding
    if padding_len > 0 {
        let padding_start = output.len();
        output.resize(padding_start + padding_len, 0);
        rand::rng().fill(&mut output[padding_start..]);
    }

    Ok(output.freeze())
}

/// Calculate padding length based on content size and TLS detection
fn calculate_padding_length(content_len: usize, is_tls: bool) -> usize {
    let mut rng = rand::rng();

    // Calculate maximum allowable padding (avoid overflow)
    // Matches Xray-core logic: buf.Size - 21 - contentLen
    // MAX_PADDING_SIZE is already (buf.Size - 21), so we only subtract content_len
    let max_allowable = MAX_PADDING_SIZE.saturating_sub(content_len);

    if is_tls && content_len < LONG_PADDING_MIN {
        // Long padding for TLS handshake phase
        let random_part = rng.random_range(0..LONG_PADDING_RANDOM_MAX);
        let padding = LONG_PADDING_MIN
            .saturating_sub(content_len)
            .saturating_add(random_part);
        std::cmp::min(padding, max_allowable)
    } else {
        // Short random padding
        let padding = rng.random_range(0..SHORT_PADDING_RANDOM_MAX);
        std::cmp::min(padding, max_allowable)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Largest content the Vision write path can offer: the inner TLS deframer
    /// rejects ciphertext above 18432 bytes and the byte-copy buffer is 16384
    /// bytes, so no caller reaches the u16 content length limit.
    const LARGEST_REACHABLE_CONTENT: usize = 18_437;

    #[test]
    fn the_largest_reachable_message_is_encoded_exactly() {
        let data = vec![0x5a; LARGEST_REACHABLE_CONTENT];
        let padded = pad_with_command(&data, 0x01, true).unwrap();

        assert_eq!(
            &padded[1..3],
            &(LARGEST_REACHABLE_CONTENT as u16).to_be_bytes()
        );
        assert_eq!(&padded[5..5 + data.len()], data.as_slice());
    }

    #[test]
    fn the_largest_framable_message_is_encoded_exactly() {
        let data = vec![0x5a; MAX_CONTENT_LEN];
        let padded = pad_with_command(&data, 0x00, false).unwrap();

        assert_eq!(&padded[1..3], &(MAX_CONTENT_LEN as u16).to_be_bytes());
        assert_eq!(&padded[5..5 + data.len()], data.as_slice());
    }

    #[test]
    fn content_above_the_framable_maximum_is_refused() {
        let data = vec![0x5a; MAX_CONTENT_LEN + 1];
        let error = pad_with_command(&data, 0x00, false)
            .expect_err("content above the u16 length must be refused");
        assert_eq!(error.kind(), io::ErrorKind::InvalidData);

        let uuid = [0x11; 16];
        let error = pad_with_uuid_and_command(&data, &uuid, 0x00, false)
            .expect_err("content above the u16 length must be refused");
        assert_eq!(error.kind(), io::ErrorKind::InvalidData);
    }

    #[test]
    fn padding_never_exceeds_the_configured_maximum() {
        for content_len in [
            0usize,
            1,
            900,
            8171,
            16_384,
            LARGEST_REACHABLE_CONTENT,
            MAX_CONTENT_LEN,
        ] {
            let data = vec![0x11; content_len];
            let padded = pad_with_command(&data, 0x00, false).unwrap();
            let padding_len = u16::from_be_bytes([padded[3], padded[4]]) as usize;

            assert!(
                padding_len <= MAX_PADDING_SIZE.saturating_sub(content_len),
                "padding for {content_len} bytes is {padding_len}"
            );
            assert_eq!(padded.len(), 5 + content_len + padding_len);
        }
    }
}
