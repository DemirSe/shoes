//! Server configuration types.

use std::collections::HashMap;

use serde::{Deserialize, Serialize};

use crate::address::NetLocation;
use crate::option_util::{NoneOrSome, OneOrSome};

use super::common::{default_reality_server_short_ids, default_reality_time_diff, default_true};
use super::dns::DnsConfig;
use super::rules::{ClientChainHop, RuleConfig};
use super::selection::ConfigSelection;
use super::transport::{BindLocation, TcpConfig, Transport};

pub fn direct_allow_rule() -> NoneOrSome<ConfigSelection<RuleConfig>> {
    NoneOrSome::One(ConfigSelection::Config(RuleConfig::default()))
}

/// Rejects the removed `shadowtls_targets` map in TLS profiles.
///
/// ShadowTLS was removed from this build. Without this check a profile that
/// combines valid `tls_targets` or `reality_targets` with `shadowtls_targets`
/// would deserialize successfully and silently discard the retired targets.
fn reject_removed_shadowtls_targets(value: &serde_yaml::Value) -> Result<(), String> {
    let Some(map) = value.as_mapping() else {
        return Ok(());
    };
    let is_tls = map
        .get(serde_yaml::Value::String("type".to_string()))
        .and_then(|tag| tag.as_str())
        == Some("tls");
    if is_tls && map.contains_key(serde_yaml::Value::String("shadowtls_targets".to_string())) {
        return Err(
            "the `shadowtls_targets` field is not supported: ShadowTLS was removed. \
             Remove the field from the TLS profile."
                .to_string(),
        );
    }
    Ok(())
}

/// Deserializer for `ServerProxyConfig` fields that rejects retired ShadowTLS targets
/// before the protocol variant is built.
fn deserialize_server_proxy_config<'de, D>(deserializer: D) -> Result<ServerProxyConfig, D::Error>
where
    D: serde::Deserializer<'de>,
{
    use serde::de::Error;

    let value = serde_yaml::Value::deserialize(deserializer)?;
    reject_removed_shadowtls_targets(&value).map_err(Error::custom)?;
    serde_yaml::from_value(value).map_err(Error::custom)
}

#[derive(Debug, Clone, Serialize)]
pub struct ServerConfig {
    #[serde(flatten)]
    pub bind_location: BindLocation,
    pub protocol: ServerProxyConfig,
    #[serde(
        alias = "transport",
        default,
        skip_serializing_if = "Transport::is_default"
    )]
    pub transport: Transport,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tcp_settings: Option<TcpConfig>,
    #[serde(
        alias = "rule",
        default = "direct_allow_rule",
        skip_serializing_if = "NoneOrSome::is_unspecified"
    )]
    pub rules: NoneOrSome<ConfigSelection<RuleConfig>>,
    /// DNS configuration for this server (optional).
    /// Can reference a dns_group by name or specify inline DNS servers.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub dns: Option<DnsConfig>,
}

impl<'de> serde::de::Deserialize<'de> for ServerConfig {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::de::Deserializer<'de>,
    {
        use serde::de::Error;

        let value = serde_yaml::Value::deserialize(deserializer)?;
        let map = value
            .as_mapping()
            .ok_or_else(|| Error::custom("ServerConfig must be a YAML mapping"))?;

        const VALID_FIELDS: &[&str] = &[
            "address",
            "addresses",
            "path", // BindLocation (flattened)
            "protocol",
            "transport",
            "tcp_settings",
            "rules",
            "rule",
            "dns",
        ];

        // Check for unknown fields
        for key in map.keys() {
            if let Some(key_str) = key.as_str()
                && !VALID_FIELDS.contains(&key_str)
            {
                return Err(Error::custom(format!(
                    "unknown field `{}` in server config. Expected one of: {}",
                    key_str,
                    VALID_FIELDS.join(", ")
                )));
            }
        }

        // Parse bind_location (flattened - either address or path)
        let bind_location = if let Some(v) = map.get("address").or_else(|| map.get("addresses")) {
            serde_yaml::from_value(v.clone())
                .map(BindLocation::Address)
                .map_err(|e| Error::custom(format!("invalid address: {e}")))?
        } else if let Some(v) = map.get("path") {
            serde_yaml::from_value(v.clone())
                .map(BindLocation::Path)
                .map_err(|e| Error::custom(format!("invalid path: {e}")))?
        } else {
            return Err(Error::custom(
                "server config must have 'address', 'addresses', or 'path' field",
            ));
        };

        // Parse protocol (required)
        let protocol: ServerProxyConfig = map
            .get("protocol")
            .ok_or_else(|| Error::custom("missing 'protocol' field in server config"))
            .and_then(|v| {
                reject_removed_shadowtls_targets(v).map_err(Error::custom)?;
                serde_yaml::from_value(v.clone())
                    .map_err(|e| Error::custom(format!("invalid protocol: {e}")))
            })?;

        // Parse transport (optional, default)
        let transport: Transport = map
            .get("transport")
            .map(|v| serde_yaml::from_value(v.clone()))
            .transpose()
            .map_err(|e| Error::custom(format!("invalid transport: {e}")))?
            .unwrap_or_default();

        // Parse tcp_settings (optional, skip if null)
        let tcp_settings: Option<TcpConfig> = map
            .get("tcp_settings")
            .filter(|v| !v.is_null())
            .map(|v| serde_yaml::from_value(v.clone()))
            .transpose()
            .map_err(|e| Error::custom(format!("invalid tcp_settings: {e}")))?;

        // Parse rules (optional, with alias "rule", default to direct_allow_rule, skip if null)
        let rules: NoneOrSome<ConfigSelection<RuleConfig>> = map
            .get("rules")
            .or_else(|| map.get("rule"))
            .filter(|v| !v.is_null())
            .map(|v| serde_yaml::from_value(v.clone()))
            .transpose()
            .map_err(|e| Error::custom(format!("invalid rules: {e}")))?
            .unwrap_or_else(direct_allow_rule);

        // Parse dns (optional)
        let dns: Option<DnsConfig> = map
            .get("dns")
            .filter(|v| !v.is_null())
            .map(|v| serde_yaml::from_value(v.clone()))
            .transpose()
            .map_err(|e| Error::custom(format!("invalid dns: {e}")))?;

        Ok(ServerConfig {
            bind_location,
            protocol,
            transport,
            tcp_settings,
            rules,
            dns,
        })
    }
}

/// Optional rate control for one direction of a transparent REALITY connection.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct FallbackRateLimit {
    #[serde(default)]
    pub after_bytes: u64,
    #[serde(default)]
    pub bytes_per_sec: u64,
    /// Zero or omitted uses one second of bytes_per_sec as the bucket capacity.
    #[serde(default)]
    pub burst_bytes_per_sec: u64,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct RealityServerConfig {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub limit_fallback_upload: Option<FallbackRateLimit>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub limit_fallback_download: Option<FallbackRateLimit>,
    /// X25519 private key (32 bytes, base64url encoded)
    pub private_key: String,

    /// List of valid short IDs (hex strings, 0-16 chars each)
    #[serde(alias = "short_id", default = "default_reality_server_short_ids")]
    pub short_ids: OneOrSome<String>,

    /// Fallback destination (e.g., "example.com:443")
    pub dest: NetLocation,

    /// Maximum timestamp difference in milliseconds (optional)
    #[serde(default = "default_reality_time_diff")]
    pub max_time_diff: Option<u64>,

    /// Minimum client version [major, minor, patch] (optional)
    #[serde(default)]
    pub min_client_version: Option<[u8; 3]>,

    /// Maximum client version [major, minor, patch] (optional)
    #[serde(default)]
    pub max_client_version: Option<[u8; 3]>,

    /// TLS 1.3 cipher suites to support (optional)
    /// Valid values: "TLS_AES_128_GCM_SHA256", "TLS_AES_256_GCM_SHA384", "TLS_CHACHA20_POLY1305_SHA256"
    /// If empty or not specified, the default set is used.
    #[serde(alias = "cipher_suite", default)]
    pub cipher_suites: NoneOrSome<crate::reality::CipherSuite>,

    /// Enable XTLS-Vision protocol for TLS-in-TLS optimization.
    /// When enabled, the inner protocol MUST be VLESS.
    /// Vision detects TLS-in-TLS scenarios and switches to Direct mode for zero-copy performance.
    /// Reality provides censorship resistance while Vision provides performance optimization.
    #[serde(default)]
    pub vision: bool,
    /// Inner protocol (VLESS)
    #[serde(deserialize_with = "deserialize_server_proxy_config")]
    pub protocol: ServerProxyConfig,

    /// Client chain for connecting to dest server (for fallback connections).
    /// If not specified, connects directly to dest.
    #[serde(default)]
    pub dest_client_chain: NoneOrSome<ClientChainHop>,

    /// Override rules
    #[serde(alias = "override_rule", default)]
    pub override_rules: NoneOrSome<ConfigSelection<RuleConfig>>,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct TlsServerConfig {
    pub cert: String,
    pub key: String,
    #[serde(alias = "alpn_protocol", default)]
    pub alpn_protocols: NoneOrSome<String>,

    // trusted CA certs that client certs must chain to.
    // note that if a client cert chains back to a cert in this field,
    // it is validated even if the leaf certificate is not in `client_fingerprints`
    // below.
    #[serde(alias = "client_ca_cert", default)]
    pub client_ca_certs: NoneOrSome<String>,

    // sha256 fingerprint of allowed client certificates
    //
    // To generate a new ECDSA client certificate:
    // 1. Generate private key with P-256 curve:
    //    openssl ecparam -genkey -name prime256v1 -out client.key
    // 2. Create self-signed certificate:
    //    openssl req -new -x509 -nodes -key client.key -out client.crt -days 365 -subj "/CN=Client"
    //
    // Get the certificate's SHA256 fingerprint:
    //    openssl x509 -in client.crt -noout -fingerprint -sha256
    //
    // Each generated key pair will be unique. Multiple fingerprints can be specified
    // to allow multiple client certificates.
    #[serde(alias = "client_fingerprint", default)]
    pub client_fingerprints: NoneOrSome<String>,

    /// Enable XTLS-Vision protocol for TLS-in-TLS optimization.
    /// When enabled, the inner protocol MUST be VLESS.
    /// Vision detects TLS-in-TLS scenarios and switches to Direct mode for zero-copy performance.
    /// Requires TLS 1.3.
    #[serde(default)]
    pub vision: bool,
    #[serde(deserialize_with = "deserialize_server_proxy_config")]
    pub protocol: ServerProxyConfig,

    #[serde(alias = "override_rule", default)]
    pub override_rules: NoneOrSome<ConfigSelection<RuleConfig>>,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(tag = "type", rename_all = "lowercase")]
pub enum ServerProxyConfig {
    Vless {
        #[serde(alias = "user_ids")]
        user_id: OneOrSome<String>,
        #[serde(default = "default_true")]
        udp_enabled: bool,
        /// Fallback destination for failed authentication (optional)
        /// When set, failed auth attempts are proxied here instead of rejected
        #[serde(default, skip_serializing_if = "Option::is_none")]
        fallback: Option<NetLocation>,
    },
    Tls {
        // sni_targets is the previous field name
        #[serde(default, alias = "sni_targets", alias = "targets")]
        tls_targets: HashMap<String, TlsServerConfig>,
        // default_target is the previous field name
        #[serde(
            default,
            alias = "default_target",
            skip_serializing_if = "Option::is_none"
        )]
        default_tls_target: Option<Box<TlsServerConfig>>,
        #[serde(default)]
        reality_targets: HashMap<String, RealityServerConfig>,

        #[serde(default, skip_serializing_if = "Option::is_none")]
        tls_buffer_size: Option<usize>,
    },
}

impl std::fmt::Display for ServerProxyConfig {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Vless { .. } => write!(f, "Vless"),
            Self::Tls {
                tls_targets,
                default_tls_target,
                reality_targets,
                ..
            } => {
                let mut parts = vec![];

                if !tls_targets.is_empty() {
                    parts.push("TLS");
                }

                if !reality_targets.is_empty() {
                    parts.push("REALITY");
                }
                if tls_targets.values().any(|cfg| cfg.vision)
                    || default_tls_target.as_ref().is_some_and(|cfg| cfg.vision)
                    || reality_targets.values().any(|cfg| cfg.vision)
                {
                    parts.push("Vision");
                }

                write!(f, "{}", parts.join("+"))
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::net::{IpAddr, Ipv4Addr, Ipv6Addr};

    const TEST_UUID: &str = "550e8400-e29b-41d4-a716-446655440000";

    fn vless_protocol() -> ServerProxyConfig {
        ServerProxyConfig::Vless {
            user_id: OneOrSome::One(TEST_UUID.to_string()),
            udp_enabled: true,
            fallback: None,
        }
    }

    fn create_test_server_config_vless() -> ServerConfig {
        ServerConfig {
            bind_location: NetLocation::from_ip_addr(
                IpAddr::V6(Ipv6Addr::new(0, 0, 0, 0, 0, 0, 0, 1)),
                443,
            )
            .into(),
            protocol: vless_protocol(),
            transport: Transport::Tcp,
            tcp_settings: None,
            rules: NoneOrSome::None,
            dns: None,
        }
    }

    fn create_test_server_config_tls() -> ServerConfig {
        let mut tls_targets = HashMap::new();
        tls_targets.insert(
            "example.com".to_string(),
            TlsServerConfig {
                cert: "example.crt".to_string(),
                key: "example.key".to_string(),
                alpn_protocols: NoneOrSome::Some(vec!["h2".to_string(), "http/1.1".to_string()]),
                client_ca_certs: NoneOrSome::One("ca.crt".to_string()),
                client_fingerprints: NoneOrSome::One("abc123".to_string()),
                vision: true,
                protocol: vless_protocol(),
                override_rules: NoneOrSome::None,
            },
        );

        let mut reality_targets = HashMap::new();
        reality_targets.insert(
            "www.example.com".to_string(),
            RealityServerConfig {
                limit_fallback_upload: None,
                limit_fallback_download: None,
                private_key: "AAECAwQFBgcICQoLDA0ODxAREhMUFRYXGBkaGxwdHh8".to_string(),
                short_ids: OneOrSome::One("0123456789abcdef".to_string()),
                dest: NetLocation::from_ip_addr(IpAddr::V4(Ipv4Addr::new(93, 184, 216, 34)), 443),
                max_time_diff: None,
                min_client_version: None,
                max_client_version: None,
                cipher_suites: NoneOrSome::None,
                vision: true,
                protocol: vless_protocol(),
                dest_client_chain: NoneOrSome::None,
                override_rules: NoneOrSome::None,
            },
        );

        ServerConfig {
            bind_location: NetLocation::from_ip_addr(IpAddr::V4(Ipv4Addr::new(10, 0, 0, 1)), 8443)
                .into(),
            protocol: ServerProxyConfig::Tls {
                tls_targets,
                default_tls_target: Some(Box::new(TlsServerConfig {
                    cert: "default.crt".to_string(),
                    key: "default.key".to_string(),
                    alpn_protocols: NoneOrSome::None,
                    client_ca_certs: NoneOrSome::None,
                    client_fingerprints: NoneOrSome::None,
                    vision: false,
                    protocol: vless_protocol(),
                    override_rules: NoneOrSome::None,
                })),
                reality_targets,
                tls_buffer_size: Some(8192),
            },
            transport: Transport::Tcp,
            tcp_settings: None,
            rules: NoneOrSome::None,
            dns: None,
        }
    }

    // Test individual server config variants
    #[test]
    fn fallback_rate_limit_round_trip_and_invalid_fields() {
        let yaml = "after_bytes: 18446744073709551615\nbytes_per_sec: 4\nburst_bytes_per_sec: 2\n";
        let limit: FallbackRateLimit = serde_yaml::from_str(yaml).unwrap();
        assert_eq!(limit.after_bytes, u64::MAX);
        assert_eq!(
            serde_yaml::from_str::<FallbackRateLimit>(&serde_yaml::to_string(&limit).unwrap())
                .unwrap(),
            limit
        );
        assert_eq!(
            serde_yaml::from_str::<FallbackRateLimit>("{}").unwrap(),
            FallbackRateLimit::default()
        );
        for invalid in [
            "bytes_per_sec: -1",
            "bytes_per_sec: 1.5",
            "bytes_per_sec: nope",
            "after_bytes: 18446744073709551616",
            "burst_bytes_per_sec: -1",
            "bytes_per_second: 1",
        ] {
            assert!(
                serde_yaml::from_str::<FallbackRateLimit>(invalid).is_err(),
                "{invalid}"
            );
        }
        let mut server = create_test_server_config_tls();
        let ServerProxyConfig::Tls {
            ref mut reality_targets,
            ..
        } = server.protocol
        else {
            panic!()
        };
        let target = reality_targets.get_mut("www.example.com").unwrap();
        target.limit_fallback_upload = Some(limit);
        target.limit_fallback_download = Some(FallbackRateLimit::default());
        let decoded: ServerConfig =
            serde_yaml::from_str(&serde_yaml::to_string(&server).unwrap()).unwrap();
        let ServerProxyConfig::Tls {
            reality_targets, ..
        } = decoded.protocol
        else {
            panic!()
        };
        assert_eq!(
            reality_targets["www.example.com"].limit_fallback_upload,
            Some(limit)
        );
        assert_eq!(
            reality_targets["www.example.com"].limit_fallback_download,
            Some(FallbackRateLimit::default())
        );
    }

    #[test]
    fn test_server_config_vless() {
        let original = create_test_server_config_vless();
        let yaml_str = serde_yaml::to_string(&original).expect("Failed to serialize");
        let deserialized: ServerConfig =
            serde_yaml::from_str(&yaml_str).expect("Failed to deserialize");
        assert!(matches!(
            deserialized.protocol,
            ServerProxyConfig::Vless { .. }
        ));
    }

    #[test]
    fn test_server_config_tls_with_reality_and_vision() {
        let original = create_test_server_config_tls();
        let yaml_str = serde_yaml::to_string(&original).expect("Failed to serialize");
        let deserialized: ServerConfig =
            serde_yaml::from_str(&yaml_str).expect("Failed to deserialize");
        let ServerProxyConfig::Tls {
            tls_targets,
            default_tls_target,
            reality_targets,
            tls_buffer_size,
        } = deserialized.protocol
        else {
            panic!("expected TLS protocol");
        };
        assert!(tls_targets.contains_key("example.com"));
        assert!(tls_targets["example.com"].vision);
        assert!(matches!(
            tls_targets["example.com"].protocol,
            ServerProxyConfig::Vless { .. }
        ));
        assert!(default_tls_target.is_some());
        assert!(reality_targets.contains_key("www.example.com"));
        assert!(reality_targets["www.example.com"].vision);
        assert_eq!(tls_buffer_size, Some(8192));
    }

    #[test]
    fn test_rejects_retired_protocol_types() {
        for protocol_type in [
            "http",
            "socks",
            "socks5",
            "shadowsocks",
            "ss",
            "snell",
            "trojan",
            "vmess",
            "websocket",
            "ws",
            "portforward",
            "forward",
            "hysteria2",
            "tuic",
            "mixed",
            "http+socks",
            "anytls",
            "naiveproxy",
            "naive",
            "shadowtls",
        ] {
            let yaml = format!("address: \"127.0.0.1:8080\"\nprotocol:\n  type: {protocol_type}\n");
            let result: Result<ServerConfig, _> = serde_yaml::from_str(&yaml);
            let err = result
                .expect_err(&format!(
                    "retired protocol {protocol_type} must be rejected"
                ))
                .to_string();
            assert!(
                err.contains("unknown variant"),
                "error for {protocol_type} should name the unknown variant, got: {err}"
            );
        }
    }

    #[test]
    fn test_rejects_removed_shadowtls_targets_mixed_with_retained_targets() {
        let mixed_tls = r#"
address: "127.0.0.1:8443"
protocol:
  type: tls
  tls_targets:
    "example.com":
      cert: "example.crt"
      key: "example.key"
      protocol:
        type: vless
        user_id: "550e8400-e29b-41d4-a716-446655440000"
  shadowtls_targets:
    "shadow.example.com":
      password: "secret"
      handshake:
        address: "example.com:443"
      protocol:
        type: vless
        user_id: "550e8400-e29b-41d4-a716-446655440000"
"#;
        let err = serde_yaml::from_str::<ServerConfig>(mixed_tls)
            .expect_err("TLS profile with shadowtls_targets must be rejected")
            .to_string();
        assert!(
            err.contains("shadowtls_targets"),
            "error should name the removed field: {err}"
        );

        let mixed_reality = r#"
address: "127.0.0.1:8443"
protocol:
  type: tls
  reality_targets:
    "www.example.com":
      private_key: "AAECAwQFBgcICQoLDA0ODxAREhMUFRYXGBkaGxwdHh8"
      dest: "www.example.com:443"
      protocol:
        type: vless
        user_id: "550e8400-e29b-41d4-a716-446655440000"
  shadowtls_targets: {}
"#;
        let err = serde_yaml::from_str::<ServerConfig>(mixed_reality)
            .expect_err("REALITY profile with shadowtls_targets must be rejected")
            .to_string();
        assert!(
            err.contains("shadowtls_targets"),
            "error should name the removed field: {err}"
        );

        // Retained aliases and profiles without the removed field still parse.
        let retained = r#"
address: "127.0.0.1:8443"
protocol:
  type: tls
  sni_targets:
    "example.com":
      cert: "example.crt"
      key: "example.key"
      protocol:
        type: vless
        user_id: "550e8400-e29b-41d4-a716-446655440000"
"#;
        let config: ServerConfig =
            serde_yaml::from_str(retained).expect("retained TLS profile must parse");
        assert!(matches!(config.protocol, ServerProxyConfig::Tls { .. }));
    }

    #[test]
    fn test_rejects_nested_shadowtls_targets_in_retained_profile() {
        let nested = r#"
address: "127.0.0.1:8443"
protocol:
  type: tls
  tls_targets:
    "outer.example.com":
      cert: "outer.crt"
      key: "outer.key"
      protocol:
        type: tls
        tls_targets:
          "inner.example.com":
            cert: "inner.crt"
            key: "inner.key"
            protocol:
              type: vless
              user_id: "550e8400-e29b-41d4-a716-446655440000"
        shadowtls_targets: {}
"#;
        let err = serde_yaml::from_str::<ServerConfig>(nested)
            .expect_err("nested TLS profile with shadowtls_targets must be rejected")
            .to_string();
        assert!(
            err.contains("shadowtls_targets"),
            "error should name the removed field: {err}"
        );
    }

    #[test]
    fn test_rejects_quic_transport_and_quic_settings() {
        let yaml = r#"
address: "127.0.0.1:8080"
transport: quic
protocol:
  type: vless
  user_id: "550e8400-e29b-41d4-a716-446655440000"
"#;
        let err = serde_yaml::from_str::<ServerConfig>(yaml)
            .expect_err("quic transport must be rejected")
            .to_string();
        assert!(err.contains("quic"), "error should name quic: {err}");

        let yaml = r#"
address: "127.0.0.1:8080"
quic_settings:
  cert: "server.crt"
  key: "server.key"
protocol:
  type: vless
  user_id: "550e8400-e29b-41d4-a716-446655440000"
"#;
        let err = serde_yaml::from_str::<ServerConfig>(yaml)
            .expect_err("quic_settings must be rejected")
            .to_string();
        assert!(
            err.contains("quic_settings"),
            "error should name the unknown field: {err}"
        );
    }

    #[test]
    fn test_rejects_invalid_upstream_field() {
        let yaml = r#"
address: "127.0.0.1:8080"
protocol:
  type: vless
  user_id: "550e8400-e29b-41d4-a716-446655440000"
upstream:
  address: "127.0.0.1:443"
  protocol:
    type: vless
    user_id: "550e8400-e29b-41d4-a716-446655440000"
"#;

        let result: Result<ServerConfig, _> = serde_yaml::from_str(yaml);
        assert!(
            result.is_err(),
            "Should reject config with invalid 'upstream' field"
        );
        let err = result.unwrap_err().to_string();
        assert!(
            err.contains("unknown field") && err.contains("upstream"),
            "Error should mention 'upstream' as unknown field, got: {err}"
        );
    }

    #[test]
    fn test_rejects_typo_in_field_name() {
        let yaml = r#"
address: "127.0.0.1:8080"
protocl:
  type: vless
"#;

        let result: Result<ServerConfig, _> = serde_yaml::from_str(yaml);
        assert!(result.is_err(), "Should reject config with typo 'protocl'");
        let err = result.unwrap_err().to_string();
        assert!(
            err.contains("unknown field") && err.contains("protocl"),
            "Error should mention 'protocl' as unknown field, got: {err}"
        );
    }

    #[test]
    fn test_accepts_valid_server_config() {
        let yaml = r#"
address: "127.0.0.1:8080"
protocol:
  type: vless
  user_id: "550e8400-e29b-41d4-a716-446655440000"
rules:
  - mask: 0.0.0.0/0
    action: allow
"#;

        let result: Result<ServerConfig, _> = serde_yaml::from_str(yaml);
        assert!(
            result.is_ok(),
            "Should accept valid server config: {:?}",
            result.err()
        );
    }

    #[test]
    fn test_scalar_bind_address_remains_scalar() {
        let yaml = r#"
address: "127.0.0.1:8080"
protocol:
  type: vless
  user_id: "550e8400-e29b-41d4-a716-446655440000"
"#;

        let config: ServerConfig = serde_yaml::from_str(yaml).unwrap();
        assert!(matches!(
            &config.bind_location,
            BindLocation::Address(OneOrSome::One(_))
        ));

        let value = serde_yaml::to_value(&config).unwrap();
        let address = value
            .as_mapping()
            .unwrap()
            .get(serde_yaml::Value::String("address".to_string()))
            .unwrap();
        assert_eq!(address.as_str(), Some("127.0.0.1:8080"));
    }

    #[test]
    fn test_accepts_multiple_bind_addresses() {
        let yaml = r#"
address:
  - "127.0.0.1:22223"
  - "172.17.0.1:22223-22225"
protocol:
  type: vless
  user_id: "550e8400-e29b-41d4-a716-446655440000"
"#;

        let config: ServerConfig = serde_yaml::from_str(yaml).unwrap();
        let BindLocation::Address(addresses) = &config.bind_location else {
            panic!("expected address bind location");
        };
        let socket_addrs: Vec<String> = addresses
            .iter()
            .flat_map(|address| address.to_socket_addrs().unwrap())
            .map(|address| address.to_string())
            .collect();
        assert_eq!(
            socket_addrs,
            vec![
                "127.0.0.1:22223",
                "172.17.0.1:22223",
                "172.17.0.1:22224",
                "172.17.0.1:22225",
            ]
        );

        let serialized = serde_yaml::to_string(&config).unwrap();
        let round_tripped: ServerConfig = serde_yaml::from_str(&serialized).unwrap();
        assert_eq!(round_tripped.bind_location, config.bind_location);
    }

    #[test]
    fn test_address_precedes_addresses_alias() {
        let yaml = r#"
address: "127.0.0.1:8080"
addresses:
  - "172.17.0.1:8080"
protocol:
  type: vless
  user_id: "550e8400-e29b-41d4-a716-446655440000"
"#;

        let config: ServerConfig = serde_yaml::from_str(yaml).unwrap();
        assert_eq!(config.bind_location.to_string(), "127.0.0.1:8080");
    }

    #[test]
    fn test_rejects_empty_bind_address_list() {
        let yaml = r#"
address: []
protocol:
  type: vless
  user_id: "550e8400-e29b-41d4-a716-446655440000"
"#;

        assert!(serde_yaml::from_str::<ServerConfig>(yaml).is_err());
    }

    #[test]
    fn test_accepts_valid_server_config_with_all_fields() {
        let yaml = r#"
address: "127.0.0.1:8080"
protocol:
  type: vless
  user_id: "550e8400-e29b-41d4-a716-446655440000"
transport: tcp
tcp_settings:
  no_delay: true
rules:
  - mask: 0.0.0.0/0
    action: allow
"#;

        let result: Result<ServerConfig, _> = serde_yaml::from_str(yaml);
        assert!(
            result.is_ok(),
            "Should accept valid server config with all fields: {:?}",
            result.err()
        );
    }

    #[test]
    fn test_vless_fallback_profile_parses() {
        let yaml = r#"
address: "127.0.0.1:8080"
protocol:
  type: vless
  user_id: "550e8400-e29b-41d4-a716-446655440000"
  udp_enabled: false
  fallback: "whatsapp.net:443"
"#;

        let config: ServerConfig = serde_yaml::from_str(yaml).unwrap();
        let ServerProxyConfig::Vless {
            udp_enabled,
            fallback,
            ..
        } = config.protocol
        else {
            panic!("expected VLESS protocol");
        };
        assert!(!udp_enabled);
        assert_eq!(fallback.unwrap().to_string(), "whatsapp.net:443");
    }
}
