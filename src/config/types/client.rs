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

/// Custom deserializer for TlsClientConfig.
fn deserialize_tls_client_config<'de, D>(deserializer: D) -> Result<TlsClientConfig, D::Error>
where
    D: serde::Deserializer<'de>,
{
    #[derive(Deserialize)]
    #[serde(deny_unknown_fields)]
    struct TlsClientConfigTemp {
        #[serde(default = "default_true")]
        verify: bool,
        #[serde(alias = "server_fingerprint", default)]
        server_fingerprints: NoneOrSome<String>,
        #[serde(default)]
        sni_hostname: NoneOrOne<String>,
        #[serde(alias = "alpn_protocol", default)]
        alpn_protocols: NoneOrSome<String>,
        #[serde(default)]
        tls_buffer_size: Option<usize>,
        #[serde(default)]
        key: Option<String>,
        #[serde(default)]
        cert: Option<String>,
        #[serde(default)]
        vision: bool,
        protocol: Box<ClientProxyConfig>,
    }

    let temp = TlsClientConfigTemp::deserialize(deserializer)?;

    Ok(TlsClientConfig {
        verify: temp.verify,
        server_fingerprints: temp.server_fingerprints,
        sni_hostname: temp.sni_hostname,
        alpn_protocols: temp.alpn_protocols,
        tls_buffer_size: temp.tls_buffer_size,
        key: temp.key,
        cert: temp.cert,
        vision: temp.vision,
        protocol: temp.protocol,
    })
}

/// Variant deserializer for Tls in ClientProxyConfig enum
fn deserialize_tls_variant<'de, D>(deserializer: D) -> Result<TlsClientConfig, D::Error>
where
    D: serde::Deserializer<'de>,
{
    deserialize_tls_client_config(deserializer)
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
    #[serde(deserialize_with = "deserialize_tls_variant")]
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

#[derive(Debug, Clone, Serialize)]
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
