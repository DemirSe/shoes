use std::io::Cursor;

macro_rules! feed_rustls_connection {
    ($name:ident, $connection:ty, $failure:literal, $unconsumed:literal) => {
        #[inline]
        pub fn $name(connection: &mut $connection, data: &[u8]) -> std::io::Result<()> {
            let mut cursor = Cursor::new(data);
            let mut i = 0;
            while i < data.len() {
                let n = connection.read_tls(&mut cursor).map_err(|e| {
                    std::io::Error::new(std::io::ErrorKind::InvalidData, format!($failure, e))
                })?;
                if n == 0 {
                    return Err(std::io::Error::new(
                        std::io::ErrorKind::InvalidData,
                        format!($unconsumed, i, data.len()),
                    ));
                }
                i += n;
            }
            Ok(())
        }
    };
}

feed_rustls_connection!(
    feed_rustls_server_connection,
    rustls::ServerConnection,
    "failed to feed rustls server connection: {}",
    "rustls server connection did not consume all bytes: fed {}/{} bytes"
);
feed_rustls_connection!(
    feed_rustls_client_connection,
    rustls::ClientConnection,
    "failed to feed rustls client connection: {}",
    "rustls client connection did not consume all bytes: fed {}/{} bytes"
);

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::ErrorKind;
    use std::sync::Arc;

    fn connected_pair() -> (rustls::ClientConnection, rustls::ServerConnection) {
        let cert = rcgen::generate_simple_self_signed(vec!["localhost".into()]).unwrap();
        let provider = Arc::new(rustls::crypto::aws_lc_rs::default_provider());
        let mut roots = rustls::RootCertStore::empty();
        roots.add(cert.cert.der().clone()).unwrap();
        let client_config = rustls::ClientConfig::builder_with_provider(provider.clone())
            .with_protocol_versions(&[&rustls::version::TLS13])
            .unwrap()
            .with_root_certificates(roots)
            .with_no_client_auth();
        let mut server_config = rustls::ServerConfig::builder_with_provider(provider)
            .with_protocol_versions(&[&rustls::version::TLS13])
            .unwrap()
            .with_no_client_auth()
            .with_single_cert(
                vec![cert.cert.der().clone()],
                rustls::pki_types::PrivatePkcs8KeyDer::from(cert.signing_key.serialize_der())
                    .into(),
            )
            .unwrap();
        server_config.send_tls13_tickets = 0;
        let mut client =
            rustls::ClientConnection::new(Arc::new(client_config), "localhost".try_into().unwrap())
                .unwrap();
        let mut server = rustls::ServerConnection::new(Arc::new(server_config)).unwrap();
        for _ in 0..10 {
            let mut wire = Vec::new();
            while client.wants_write() {
                client.write_tls(&mut wire).unwrap();
            }
            feed_rustls_server_connection(&mut server, &wire).unwrap();
            server.process_new_packets().unwrap();
            wire.clear();
            while server.wants_write() {
                server.write_tls(&mut wire).unwrap();
            }
            feed_rustls_client_connection(&mut client, &wire).unwrap();
            client.process_new_packets().unwrap();
            if !client.is_handshaking()
                && !server.is_handshaking()
                && !client.wants_write()
                && !server.wants_write()
            {
                return (client, server);
            }
        }
        panic!("TLS handshake did not finish");
    }

    #[test]
    fn empty_input_does_not_signal_eof() {
        let (mut client, mut server) = connected_pair();
        feed_rustls_server_connection(&mut server, &[]).unwrap();
        feed_rustls_client_connection(&mut client, &[]).unwrap();
        let mut byte = [0];
        assert_eq!(
            std::io::Read::read(&mut server.reader(), &mut byte)
                .unwrap_err()
                .kind(),
            ErrorKind::WouldBlock
        );
        assert_eq!(
            std::io::Read::read(&mut client.reader(), &mut byte)
                .unwrap_err()
                .kind(),
            ErrorKind::WouldBlock
        );
    }

    #[test]
    fn nonempty_input_after_close_notify_reports_unconsumed_bytes() {
        let (mut client, mut server) = connected_pair();
        client.send_close_notify();
        let mut wire = Vec::new();
        client.write_tls(&mut wire).unwrap();
        feed_rustls_server_connection(&mut server, &wire).unwrap();
        assert!(server.process_new_packets().unwrap().peer_has_closed());
        feed_rustls_server_connection(&mut server, &[]).unwrap();
        let error = feed_rustls_server_connection(&mut server, &[0, 1]).unwrap_err();
        assert_eq!(error.kind(), ErrorKind::InvalidData);
        assert_eq!(
            error.to_string(),
            "rustls server connection did not consume all bytes: fed 0/2 bytes"
        );

        server.send_close_notify();
        wire.clear();
        server.write_tls(&mut wire).unwrap();
        feed_rustls_client_connection(&mut client, &wire).unwrap();
        assert!(client.process_new_packets().unwrap().peer_has_closed());
        feed_rustls_client_connection(&mut client, &[]).unwrap();
        let error = feed_rustls_client_connection(&mut client, &[0, 1]).unwrap_err();
        assert_eq!(error.kind(), ErrorKind::InvalidData);
        assert_eq!(
            error.to_string(),
            "rustls client connection did not consume all bytes: fed 0/2 bytes"
        );
    }

    #[test]
    fn read_tls_errors_preserve_connection_label_and_message() {
        let (mut client, mut server) = connected_pair();
        // Without processing packets, this exceeds rustls's incoming record buffer.
        let data = [0; 65536];
        let error = feed_rustls_server_connection(&mut server, &data).unwrap_err();
        assert_eq!(error.kind(), ErrorKind::InvalidData);
        assert_eq!(
            error.to_string(),
            "failed to feed rustls server connection: message buffer full"
        );
        let error = feed_rustls_client_connection(&mut client, &data).unwrap_err();
        assert_eq!(error.kind(), ErrorKind::InvalidData);
        assert_eq!(
            error.to_string(),
            "failed to feed rustls client connection: message buffer full"
        );
        feed_rustls_server_connection(&mut server, &[]).unwrap();
        feed_rustls_client_connection(&mut client, &[]).unwrap();
    }
}
