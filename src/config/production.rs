//! Opt-in deployment policy; general protocol fixtures remain unrestricted.
use std::io;
use std::net::Ipv4Addr;

use super::{BindLocation, Config, ServerProxyConfig, Transport, ValidatedConfigs};
use crate::address::Address;

fn invalid(message: &str) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidInput, message)
}

fn public_ipv4(ip: Ipv4Addr) -> bool {
    // Conservative deployment policy: exclude private/local, documentation,
    // CGNAT, benchmark, multicast/reserved and legacy special-use ranges.
    // This is deliberately not a claim that every remaining address is routed.
    let [a, b, c, _] = ip.octets();
    !(ip.is_private()
        || ip.is_loopback()
        || ip.is_link_local()
        || ip.is_documentation()
        || a == 0
        || a >= 224
        || (a == 100 && (64..=127).contains(&b))
        || (a == 198 && (b == 18 || b == 19))
        || (a == 192 && b == 0 && (c == 0 || c == 2))
        || (a == 192 && b == 88 && c == 99))
}

pub fn validate_production_profile(configs: &ValidatedConfigs) -> io::Result<()> {
    let [Config::Server(server)] = configs.configs.as_slice() else {
        return Err(invalid(
            "production profile requires exactly one REALITY TCP server",
        ));
    };
    if server.transport != Transport::Tcp {
        return Err(invalid("production profile requires TCP transport"));
    }
    let BindLocation::Address(addresses) = &server.bind_location else {
        return Err(invalid(
            "production profile requires an explicit public IPv4 address",
        ));
    };
    let addresses: Vec<_> = addresses.iter().collect();
    if addresses.len() != 1
        || addresses[0].ports() != [443]
        || !matches!(addresses[0].address(), Address::Ipv4(ip) if public_ipv4(*ip))
    {
        return Err(invalid(
            "production profile requires one explicit public IPv4 TCP 443 listener",
        ));
    }
    let ServerProxyConfig::Tls {
        tls_targets,
        default_tls_target,
        reality_targets,
        ..
    } = &server.protocol
    else {
        return Err(invalid(
            "production profile requires REALITY with Vision and VLESS",
        ));
    };
    if !tls_targets.is_empty() || default_tls_target.is_some() || reality_targets.len() != 1 {
        return Err(invalid(
            "production profile permits only the whatsapp.net REALITY target",
        ));
    }
    let cover = reality_targets
        .get("whatsapp.net")
        .ok_or_else(|| invalid("production REALITY SNI must be exactly whatsapp.net"))?;
    if cover.dest.address().hostname() != Some("whatsapp.net") || cover.dest.port() != 443 {
        return Err(invalid(
            "production REALITY destination must be exactly whatsapp.net:443",
        ));
    }
    if !cover.vision
        || !matches!(
            &cover.protocol,
            ServerProxyConfig::Vless {
                udp_enabled: true,
                fallback: None,
                ..
            }
        )
    {
        return Err(invalid(
            "production profile requires Vision, VLESS UDP/XUDP, and no VLESS fallback",
        ));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn profile() -> serde_yaml::Value {
        use base64::Engine;
        let key = base64::engine::general_purpose::URL_SAFE_NO_PAD.encode([1; 32]);
        serde_yaml::from_str(&format!(
            r#"
- address: "93.184.216.34:443"
  protocol:
    type: tls
    reality_targets:
      whatsapp.net:
        private_key: "{key}"
        short_ids: ["01"]
        dest: "whatsapp.net:443"
        vision: true
        protocol:
          type: vless
          user_id: "550e8400-e29b-41d4-a716-446655440000"
          udp_enabled: true
"#
        ))
        .unwrap()
    }

    fn check(value: serde_yaml::Value) -> io::Result<()> {
        let configs = super::super::create_server_configs(serde_yaml::from_value(value).unwrap())?;
        validate_production_profile(&configs)
    }

    #[test]
    fn production_policy_is_exact_and_offline() {
        assert!(check(profile()).is_ok());
        for address in [
            "0.0.0.0:443",
            "127.0.0.1:443",
            "[::1]:443",
            "93.184.216.34:8443",
            "whatsapp.net:443",
        ] {
            let mut value = profile();
            value[0]["address"] = address.into();
            assert!(check(value).is_err());
        }
        for (field, data) in [
            ("vision", serde_yaml::Value::Bool(false)),
            (
                "dest",
                serde_yaml::Value::String("whatsapp.net:8443".into()),
            ),
        ] {
            let mut value = profile();
            value[0]["protocol"]["reality_targets"]["whatsapp.net"][field] = data;
            assert!(check(value).is_err());
        }
        let mut value = profile();
        value[0]["protocol"]["reality_targets"]["whatsapp.net"]["protocol"]["fallback"] =
            "whatsapp.net:443".into();
        assert!(check(value).is_err());
        let mut value = profile();
        value[0]["protocol"]["reality_targets"]["whatsapp.net"]["protocol"]["udp_enabled"] =
            false.into();
        assert!(check(value).is_err());
        let mut value = profile();
        let targets = value[0]["protocol"]["reality_targets"]
            .as_mapping_mut()
            .unwrap();
        let target = targets
            .remove(serde_yaml::Value::String("whatsapp.net".into()))
            .unwrap();
        targets.insert("WHATSAPP.NET".into(), target);
        assert!(check(value).is_err());
    }

    #[test]
    fn single_and_multiple_user_profiles_validate_but_empty_invalid_and_duplicate_lists_do_not() {
        for ids in [
            "user_id: '550e8400-e29b-41d4-a716-446655440000'",
            "user_ids: ['550e8400-e29b-41d4-a716-446655440000', '550e8400-e29b-41d4-a716-446655440001']",
        ] {
            let yaml =
                format!("- address: '127.0.0.1:443'\n  protocol:\n    type: vless\n    {ids}\n");
            assert!(
                super::super::create_server_configs(serde_yaml::from_str(&yaml).unwrap()).is_ok()
            );
        }
        for ids in [
            "[]",
            "['invalid']",
            "['550e8400-e29b-41d4-a716-446655440000', '550E8400-E29B-41D4-A716-446655440000']",
        ] {
            let yaml = format!(
                "- address: '127.0.0.1:443'\n  protocol:\n    type: vless\n    user_ids: {ids}\n"
            );
            let result = serde_yaml::from_str(&yaml)
                .map_err(io::Error::other)
                .and_then(super::super::create_server_configs);
            assert!(result.is_err());
        }
    }

    #[test]
    fn public_address_filter_excludes_non_public_ranges() {
        for ip in [
            "0.0.0.0",
            "10.0.0.1",
            "127.0.0.1",
            "100.64.0.1",
            "169.254.1.1",
            "192.0.0.1",
            "192.0.2.1",
            "198.18.0.1",
            "203.0.113.1",
            "224.0.0.1",
            "255.255.255.255",
        ] {
            assert!(!public_ipv4(ip.parse().unwrap()));
        }
        assert!(public_ipv4("93.184.216.34".parse().unwrap()));
    }
}
