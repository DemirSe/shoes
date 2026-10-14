//! Client configuration types.

use serde::{Deserialize, Serialize};

use crate::address::NetLocation;
use crate::h2mux::{H2MuxOptions, MuxProtocol};
use crate::option_util::{NoneOrOne, NoneOrSome};

use super::common::{
    default_reality_client_short_id, default_true, is_false, is_true, unspecified_address,
};
use super::transport::{TcpConfig, Transport};

/// Configuration for h2mux (HTTP/2 multiplexing) on protocols that support it.
///
/// H2MUX multiplexes multiple proxy streams over a single HTTP/2 connection,
/// reducing connection overhead and improving performance for many concurrent streams.
#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct H2MuxConfig {
    /// Maximum number of connections to maintain
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_connections: Option<u32>,

    /// Minimum number of streams before opening a new connection
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub min_streams: Option<u32>,

    /// Maximum number of streams per connection (0 = unlimited)
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_streams: Option<u32>,

    /// Enable padding for traffic obfuscation
    #[serde(default, skip_serializing_if = "is_false")]
    pub padding: bool,
}

impl H2MuxConfig {
    /// Converts this config to H2MuxOptions for the client handler.
    pub fn to_options(&self) -> H2MuxOptions {
        H2MuxOptions {
            protocol: MuxProtocol::H2Mux,
            max_connections: self.max_connections.unwrap_or(4),
            min_streams: self.min_streams.unwrap_or(4),
            max_streams: self.max_streams.unwrap_or(0),
            padding: self.padding,
        }
    }
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct ClientConfig {
    #[serde(default, skip_serializing_if = "NoneOrOne::is_unspecified")]
    pub bind_interface: NoneOrOne<String>,
    #[serde(
        default = "unspecified_address",
        skip_serializing_if = "NetLocation::is_unspecified"
    )]
    pub address: NetLocation,
    pub protocol: ClientProxyConfig,
    #[serde(default, skip_serializing_if = "Transport::is_default")]
    pub transport: Transport,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tcp_settings: Option<TcpConfig>,
}

impl Default for ClientConfig {
    fn default() -> Self {
        Self {
            bind_interface: NoneOrOne::None,
            address: unspecified_address(),
            protocol: ClientProxyConfig::Direct,
            transport: Transport::default(),
            tcp_settings: None,
        }
    }
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(tag = "type", rename_all = "lowercase")]
pub enum ClientProxyConfig {
    Direct,
    Vless {
        user_id: String,
        #[serde(default = "default_true", skip_serializing_if = "is_true")]
        udp_enabled: bool,
        /// H2MUX multiplexing configuration
        #[serde(default, skip_serializing_if = "Option::is_none")]
        h2mux: Option<H2MuxConfig>,
    },
    Reality {
        public_key: String,
        #[serde(default = "default_reality_client_short_id")]
        short_id: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        sni_hostname: Option<String>,

        /// TLS 1.3 cipher suites to use (optional)
        /// Valid values: "TLS_AES_128_GCM_SHA256", "TLS_AES_256_GCM_SHA384", "TLS_CHACHA20_POLY1305_SHA256"
        /// If empty or not specified, all three cipher suites are offered.
        #[serde(
            alias = "cipher_suite",
            default,
            skip_serializing_if = "NoneOrSome::is_unspecified"
        )]
        cipher_suites: NoneOrSome<crate::reality::CipherSuite>,

        /// Enable XTLS-Vision protocol for TLS-in-TLS optimization.
        /// When enabled, the inner protocol MUST be VLESS.
        #[serde(default, skip_serializing_if = "is_false")]
        vision: bool,

        protocol: Box<ClientProxyConfig>,
    },
    Tls(TlsClientConfig),
}

impl ClientProxyConfig {
    pub fn is_direct(&self) -> bool {
        matches!(self, ClientProxyConfig::Direct)
    }

    /// Returns the protocol name for display/error messages
    pub fn protocol_name(&self) -> &str {
        match self {
            ClientProxyConfig::Direct => "Direct",
            ClientProxyConfig::Vless { .. } => "VLESS",
            ClientProxyConfig::Reality { .. } => "Reality",
            ClientProxyConfig::Tls(..) => "TLS",
        }
    }
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct TlsClientConfig {
    #[serde(default = "default_true", skip_serializing_if = "is_true")]
    pub verify: bool,
    #[serde(
        alias = "server_fingerprint",
        default,
        skip_serializing_if = "NoneOrSome::is_unspecified"
    )]
    pub server_fingerprints: NoneOrSome<String>,
    #[serde(default, skip_serializing_if = "NoneOrOne::is_unspecified")]
    pub sni_hostname: NoneOrOne<String>,
    #[serde(
        alias = "alpn_protocol",
        default,
        skip_serializing_if = "NoneOrSome::is_unspecified"
    )]
    pub alpn_protocols: NoneOrSome<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tls_buffer_size: Option<usize>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub key: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cert: Option<String>,

    /// Enable XTLS-Vision protocol for TLS-in-TLS optimization.
    /// When enabled, the inner protocol MUST be VLESS.
    /// Requires TLS 1.3.
    #[serde(default, skip_serializing_if = "is_false")]
    pub vision: bool,

    pub protocol: Box<ClientProxyConfig>,
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::net::{IpAddr, Ipv4Addr};

    const TEST_UUID: &str = "550e8400-e29b-41d4-a716-446655440000";

    fn assert_tls_deserialization_paths(yaml: &str) -> TlsClientConfig {
        let direct: TlsClientConfig = serde_yaml::from_str(yaml).expect("untagged TLS config");
        let tagged: ClientProxyConfig =
            serde_yaml::from_str(&format!("type: tls\n{yaml}")).expect("tagged TLS config");
        let ClientProxyConfig::Tls(tls) = tagged else {
            panic!("expected TLS");
        };
        assert_eq!(format!("{direct:?}"), format!("{tls:?}"));
        let serialized = serde_yaml::to_string(&ClientProxyConfig::Tls(tls.clone())).unwrap();
        let roundtrip: ClientProxyConfig = serde_yaml::from_str(&serialized).unwrap();
        assert_eq!(
            format!("{:?}", ClientProxyConfig::Tls(tls.clone())),
            format!("{roundtrip:?}")
        );
        tls
    }

    #[test]
    fn test_tls_deserializer_defaults_and_explicit_fields() {
        let tls = assert_tls_deserialization_paths(&format!(
            "protocol:\n  type: vless\n  user_id: {TEST_UUID}\n"
        ));
        assert!(tls.verify);
        assert!(!tls.vision);
        assert!(matches!(tls.server_fingerprints, NoneOrSome::Unspecified));
        assert!(matches!(tls.alpn_protocols, NoneOrSome::Unspecified));
        assert!(matches!(tls.sni_hostname, NoneOrOne::Unspecified));
        assert!(tls.tls_buffer_size.is_none());
        assert!(tls.key.is_none());
        assert!(tls.cert.is_none());
        let ClientProxyConfig::Vless {
            user_id,
            udp_enabled,
            h2mux,
        } = *tls.protocol
        else {
            panic!("expected VLESS");
        };
        assert_eq!(user_id, TEST_UUID);
        assert!(udp_enabled);
        assert!(h2mux.is_none());

        let yaml = format!(
            "verify: false\nvision: true\nserver_fingerprints: [fingerprint]\n\
             sni_hostname: test.local\nalpn_protocols: [h2, http/1.1]\n\
             tls_buffer_size: 4096\nkey: test-key\ncert: test-cert\n\
             protocol:\n  type: vless\n  user_id: {TEST_UUID}\n  udp_enabled: false\n"
        );
        let tls = assert_tls_deserialization_paths(&yaml);
        // Nondefault fields must serialize unchanged, including the nested protocol.
        assert_eq!(
            serde_yaml::to_value(&tls).unwrap(),
            serde_yaml::from_str::<serde_yaml::Value>(&yaml).unwrap()
        );
    }

    #[test]
    fn test_tls_deserializer_alias_null_and_list_matrix() {
        for (canonical, alias) in [
            ("server_fingerprints", "server_fingerprint"),
            ("alpn_protocols", "alpn_protocol"),
        ] {
            for value in ["null", "h2", "[h2]", "[h2, http/1.1]", "[]"] {
                let canonical_tls = assert_tls_deserialization_paths(&format!(
                    "{canonical}: {value}\nprotocol:\n  type: direct\n"
                ));
                let alias_tls = assert_tls_deserialization_paths(&format!(
                    "{alias}: {value}\nprotocol:\n  type: direct\n"
                ));
                assert_eq!(format!("{canonical_tls:?}"), format!("{alias_tls:?}"));
                let field = if canonical == "server_fingerprints" {
                    &canonical_tls.server_fingerprints
                } else {
                    &canonical_tls.alpn_protocols
                };
                match (value, field) {
                    ("null", NoneOrSome::None) => (),
                    ("h2", NoneOrSome::One(v)) => assert_eq!(v, "h2"),
                    ("[h2]", NoneOrSome::Some(v)) => assert_eq!(v, &["h2"]),
                    ("[h2, http/1.1]", NoneOrSome::Some(v)) => {
                        assert_eq!(v, &["h2", "http/1.1"]);
                    }
                    ("[]", NoneOrSome::Some(v)) => assert!(v.is_empty()),
                    _ => panic!("unexpected {canonical}: {value} => {field:?}"),
                }
                let serialized = serde_yaml::to_value(&alias_tls).unwrap();
                assert!(serialized.get(alias).is_none());
                assert_eq!(
                    serialized.get(canonical).unwrap(),
                    &serde_yaml::from_str::<serde_yaml::Value>(value).unwrap()
                );
            }
        }
        let tls = assert_tls_deserialization_paths(
            "sni_hostname: null\ntls_buffer_size: null\nkey: null\ncert: null\nprotocol:\n  type: direct\n",
        );
        assert!(matches!(tls.sni_hostname, NoneOrOne::None));
        assert!(tls.tls_buffer_size.is_none());
        assert!(tls.key.is_none());
        assert!(tls.cert.is_none());
        let tls = assert_tls_deserialization_paths(
            "sni_hostname: test.local\nprotocol:\n  type: direct\n",
        );
        assert!(matches!(tls.sni_hostname, NoneOrOne::One(ref sni) if sni == "test.local"));
    }

    #[test]
    fn test_tls_deserializer_rejection_matrix() {
        for (fields, error) in [
            ("wrong_field: true\n", "unknown field `wrong_field`"),
            (
                "shadowtls_password: secret\n",
                "unknown field `shadowtls_password`",
            ),
            ("verify: null\n", "invalid type"),
            ("verify: []\n", "invalid type"),
            ("vision: null\n", "invalid type"),
            ("vision: []\n", "invalid type"),
            ("tls_buffer_size: -1\n", "invalid value"),
            ("tls_buffer_size: []\n", "invalid type"),
            (
                "server_fingerprints: h2\nserver_fingerprint: h2\n",
                "duplicate field `server_fingerprints`",
            ),
            (
                "alpn_protocols: h2\nalpn_protocol: h2\n",
                "duplicate field `alpn_protocols`",
            ),
            ("verify: true\nverify: false\n", "duplicate field `verify`"),
        ] {
            let yaml = format!("{fields}protocol:\n  type: direct\n");
            let direct = serde_yaml::from_str::<TlsClientConfig>(&yaml)
                .unwrap_err()
                .to_string();
            // YAML's direct and internally tagged deserializers classify negative usize differently.
            let direct_error = if fields == "tls_buffer_size: -1\n" {
                "invalid type"
            } else {
                error
            };
            assert!(direct.contains(direct_error), "{yaml}: {direct}");
            let tagged = format!("type: tls\n{yaml}");
            let err = serde_yaml::from_str::<ClientProxyConfig>(&tagged)
                .unwrap_err()
                .to_string();
            assert!(err.contains(error), "{tagged}: {err}");
            let client = format!(
                "protocol:\n{}",
                tagged
                    .lines()
                    .map(|line| format!("  {line}\n"))
                    .collect::<String>()
            );
            let err = serde_yaml::from_str::<ClientConfig>(&client)
                .unwrap_err()
                .to_string();
            assert!(err.contains(error), "{client}: {err}");
        }
        for (yaml, error) in [
            ("type: tls\n", "missing field `protocol`"),
            ("type: tls\nprotocol: null\n", "invalid type"),
            (
                "type: tls\nprotocol:\n  type: vless\n",
                "missing field `user_id`",
            ),
            (
                "type: tls\nprotocol:\n  type: tls\n  typo: true\n  protocol:\n    type: direct\n",
                "unknown field `typo`",
            ),
        ] {
            let err = serde_yaml::from_str::<ClientProxyConfig>(yaml)
                .unwrap_err()
                .to_string();
            assert!(err.contains(error), "{yaml}: {err}");
        }
    }

    #[test]
    fn test_tls_deserializer_nested_reality_matrix() {
        let vless = format!("type: vless\nuser_id: {TEST_UUID}\n");
        let indent = |yaml: &str| {
            yaml.lines()
                .map(|line| format!("  {line}\n"))
                .collect::<String>()
        };
        for fields in [
            "",
            "cipher_suite: TLS_AES_128_GCM_SHA256\nsni_hostname: null\nignored_field: true\n",
            "cipher_suites: [TLS_AES_128_GCM_SHA256]\nshort_id: 0123456789abcdef\nsni_hostname: test.local\nvision: true\n",
        ] {
            for inner in [
                vless.clone(),
                format!(
                    "type: tls\nserver_fingerprint: null\nalpn_protocol: []\nprotocol:\n{}",
                    indent(&vless)
                ),
            ] {
                let reality = format!(
                    "type: reality\npublic_key: test-public-key\n{fields}protocol:\n{}",
                    indent(&inner)
                );
                let parsed: ClientProxyConfig = serde_yaml::from_str(&reality).unwrap();
                let ClientProxyConfig::Reality {
                    short_id,
                    cipher_suites,
                    sni_hostname,
                    ..
                } = &parsed
                else {
                    panic!("expected REALITY");
                };
                assert_eq!(
                    short_id,
                    if fields.contains("short_id:") {
                        "0123456789abcdef"
                    } else {
                        "0000000000000000"
                    }
                );
                assert_eq!(
                    sni_hostname.as_deref(),
                    if fields.contains("test.local") {
                        Some("test.local")
                    } else {
                        None
                    }
                );
                assert_eq!(cipher_suites.is_unspecified(), fields.is_empty());
                let serialized = serde_yaml::to_string(&parsed).unwrap();
                assert!(!serialized.contains("ignored_field"));
                let roundtrip: ClientProxyConfig = serde_yaml::from_str(&serialized).unwrap();
                assert_eq!(format!("{parsed:?}"), format!("{roundtrip:?}"));
                let tls =
                    assert_tls_deserialization_paths(&format!("protocol:\n{}", indent(&reality)));
                assert_eq!(format!("{:?}", tls.protocol), format!("{parsed:?}"));
            }
        }
    }

    fn create_test_client_config() -> ClientConfig {
        ClientConfig {
            bind_interface: NoneOrOne::One("eth0".to_string()),
            address: NetLocation::from_ip_addr(IpAddr::V4(Ipv4Addr::new(1, 1, 1, 1)), 1080),
            protocol: ClientProxyConfig::Vless {
                user_id: TEST_UUID.to_string(),
                udp_enabled: true,
                h2mux: None,
            },
            transport: Transport::Tcp,
            tcp_settings: None,
        }
    }

    #[test]
    fn w1_tls_deserializer_rejects_string_bools_float_and_overflow_buffer() {
        let overflow = (usize::MAX as u128) + 1;
        for fields in [
            "verify: 'true'\n".to_string(),
            "vision: 'false'\n".to_string(),
            "tls_buffer_size: 1.5\n".to_string(),
            format!("tls_buffer_size: {overflow}\n"),
        ] {
            let yaml = format!("{fields}protocol:\n  type: direct\n");
            let direct = serde_yaml::from_str::<TlsClientConfig>(&yaml)
                .unwrap_err()
                .to_string();
            assert!(direct.contains("invalid"), "{yaml}: {direct}");
            let tagged = format!("type: tls\n{yaml}");
            let error = serde_yaml::from_str::<ClientProxyConfig>(&tagged)
                .unwrap_err()
                .to_string();
            assert!(error.contains("invalid"), "{tagged}: {error}");
            let client = format!(
                "protocol:\n{}",
                tagged
                    .lines()
                    .map(|line| format!("  {line}\n"))
                    .collect::<String>()
            );
            let error = serde_yaml::from_str::<ClientConfig>(&client)
                .unwrap_err()
                .to_string();
            assert!(error.contains("invalid"), "{client}: {error}");
        }
    }

    #[test]
    fn w1_tls_deserializer_rejects_alias_first_duplicates() {
        for (canonical, alias) in [
            ("server_fingerprints", "server_fingerprint"),
            ("alpn_protocols", "alpn_protocol"),
        ] {
            let yaml = format!("{alias}: h2\n{canonical}: [http/1.1]\nprotocol:\n  type: direct\n");
            let expected = format!("duplicate field `{canonical}`");
            let direct = serde_yaml::from_str::<TlsClientConfig>(&yaml)
                .unwrap_err()
                .to_string();
            assert!(direct.contains(&expected), "{yaml}: {direct}");
            let error = serde_yaml::from_str::<ClientProxyConfig>(&format!("type: tls\n{yaml}"))
                .unwrap_err()
                .to_string();
            assert!(error.contains(&expected), "{yaml}: {error}");
        }
    }

    #[test]
    fn w1_nested_reality_cipher_alias_scalar_and_canonical_list_exact_values() {
        use crate::reality::CipherSuite;
        for (fields, expected) in [
            (
                "cipher_suite: TLS_AES_128_GCM_SHA256\n",
                vec![CipherSuite::AES_128_GCM_SHA256],
            ),
            (
                "cipher_suites: [TLS_CHACHA20_POLY1305_SHA256, TLS_AES_256_GCM_SHA384, TLS_AES_128_GCM_SHA256]\n",
                vec![
                    CipherSuite::CHACHA20_POLY1305_SHA256,
                    CipherSuite::AES_256_GCM_SHA384,
                    CipherSuite::AES_128_GCM_SHA256,
                ],
            ),
        ] {
            let reality = format!(
                "type: reality\npublic_key: test-public-key\n{fields}protocol:\n  type: vless\n  user_id: {TEST_UUID}\n"
            );
            let nested = reality
                .lines()
                .map(|line| format!("  {line}\n"))
                .collect::<String>();
            let tls = assert_tls_deserialization_paths(&format!("protocol:\n{nested}"));
            let ClientProxyConfig::Reality { cipher_suites, .. } = *tls.protocol else {
                panic!("expected REALITY");
            };
            if fields.starts_with("cipher_suite:") {
                assert!(matches!(cipher_suites, NoneOrSome::One(_)));
            } else {
                assert!(matches!(cipher_suites, NoneOrSome::Some(_)));
            }
            assert_eq!(cipher_suites.into_vec(), expected);
        }
    }

    #[test]
    fn test_client_config_serialization() {
        let original = create_test_client_config();
        let yaml_str = serde_yaml::to_string(&original).expect("Failed to serialize");
        println!("Client config YAML:\n{yaml_str}");
        let deserialized: ClientConfig =
            serde_yaml::from_str(&yaml_str).expect("Failed to deserialize");
        assert!(matches!(
            deserialized.protocol,
            ClientProxyConfig::Vless { .. }
        ));
    }

    #[test]
    fn test_rejects_retired_client_protocol_types() {
        for protocol_type in [
            "http",
            "socks",
            "socks5",
            "shadowsocks",
            "ss",
            "snell",
            "trojan",
            "shadowtls",
            "vmess",
            "websocket",
            "ws",
            "portforward",
            "noop",
            "anytls",
            "naiveproxy",
            "naive",
        ] {
            let yaml = format!("type: {protocol_type}\n");
            let result: Result<ClientProxyConfig, _> = serde_yaml::from_str(&yaml);
            let err = result
                .expect_err(&format!(
                    "retired client protocol {protocol_type} must be rejected"
                ))
                .to_string();
            assert!(
                err.contains("unknown variant"),
                "error for {protocol_type} should name the unknown variant, got: {err}"
            );
        }
    }

    #[test]
    fn test_rejects_shadowtls_password_in_tls_client() {
        let yaml = r#"
type: tls
shadowtls_password: "secret"
protocol:
  type: vless
  user_id: "550e8400-e29b-41d4-a716-446655440000"
"#;
        let result: Result<ClientProxyConfig, _> = serde_yaml::from_str(yaml);
        let err = result
            .expect_err("deprecated shadowtls_password must be rejected")
            .to_string();
        assert!(
            err.contains("shadowtls_password"),
            "Error should mention the removed field: {err}"
        );
    }

    #[test]
    fn test_rejects_unknown_field_in_tls_client_config() {
        // Test ClientProxyConfig::Tls directly
        let yaml = r#"
type: tls
verify: true
wrong_field: "oops"
protocol:
  type: vless
  user_id: "550e8400-e29b-41d4-a716-446655440000"
"#;
        let result: Result<ClientProxyConfig, _> = serde_yaml::from_str(yaml);
        assert!(result.is_err());
        let err = result.unwrap_err().to_string();
        assert!(
            err.contains("unknown field `wrong_field`"),
            "Error should mention unknown field: {err}"
        );
    }

    #[test]
    fn test_rejects_unknown_field_in_client_config() {
        // Test ClientConfig directly
        let yaml = r#"
address: "127.0.0.1:9090"
protocol:
  type: direct
invalid_client_field: "bad"
"#;
        let result: Result<ClientConfig, _> = serde_yaml::from_str(yaml);
        assert!(result.is_err());
        let err = result.unwrap_err().to_string();
        assert!(
            err.contains("unknown field `invalid_client_field`"),
            "Error should mention unknown field: {err}"
        );
    }

    #[test]
    fn test_client_proxy_config_direct() {
        let yaml = r#"
type: direct
"#;
        let result: Result<ClientProxyConfig, _> = serde_yaml::from_str(yaml);
        assert!(result.is_ok());
        assert!(result.unwrap().is_direct());
    }

    #[test]
    fn test_client_proxy_config_vless_with_h2mux() {
        let yaml = r#"
type: vless
user_id: "550e8400-e29b-41d4-a716-446655440000"
h2mux:
  max_connections: 4
  min_streams: 1
"#;
        let result: Result<ClientProxyConfig, _> = serde_yaml::from_str(yaml);
        assert!(result.is_ok());
        let ClientProxyConfig::Vless { h2mux, .. } = result.unwrap() else {
            panic!("expected VLESS");
        };
        let options = h2mux.expect("h2mux config").to_options();
        assert_eq!(options.max_connections, 4);
        assert_eq!(options.min_streams, 1);
    }

    #[test]
    fn test_client_proxy_config_tls_vision_vless() {
        let yaml = r#"
type: tls
sni_hostname: "test.local"
verify: false
vision: true
protocol:
  type: vless
  user_id: "550e8400-e29b-41d4-a716-446655440000"
"#;
        let result: Result<ClientProxyConfig, _> = serde_yaml::from_str(yaml);
        assert!(result.is_ok());
        let ClientProxyConfig::Tls(tls) = result.unwrap() else {
            panic!("expected TLS");
        };
        assert!(tls.vision);
        assert!(matches!(
            tls.protocol.as_ref(),
            ClientProxyConfig::Vless { .. }
        ));
    }

    #[test]
    fn test_client_proxy_config_reality_vision_vless() {
        let yaml = r#"
type: reality
public_key: "AAECAwQFBgcICQoLDA0ODxAREhMUFRYXGBkaGxwdHh8"
short_id: "0123456789abcdef"
sni_hostname: "example.com"
vision: true
protocol:
  type: vless
  user_id: "550e8400-e29b-41d4-a716-446655440000"
"#;
        let result: Result<ClientProxyConfig, _> = serde_yaml::from_str(yaml);
        assert!(result.is_ok());
        let ClientProxyConfig::Reality {
            vision, protocol, ..
        } = result.unwrap()
        else {
            panic!("expected Reality");
        };
        assert!(vision);
        assert!(matches!(protocol.as_ref(), ClientProxyConfig::Vless { .. }));
    }
}
