//! Shared test fixture infrastructure for integration tests
//!
//! This module provides a builder pattern for constructing integration tests
//! with minimal boilerplate for VLESS, REALITY, Vision, h2mux and TUN.
//! Stock sing-box HTTP entries are test adapters, not Shoes HTTP proxy support.

use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::Duration;

use crate::process::spawn_privileged_process;
pub use crate::process::{
    ProcessGuard, SingBoxCapability, find_singbox_binary, start_singbox_server,
    start_singbox_server_with, start_singbox_server_with_binary,
};

/// Helper to start a shoes proxy server
pub fn start_shoes_server(
    config: &str,
) -> std::io::Result<(ProcessGuard, tempfile::NamedTempFile)> {
    start_shoes_server_internal(config, false)
}

/// Helper to start a shoes proxy server with sudo (for TUN tests)
///
/// This allows running `cargo test` directly without needing to run the
/// entire test binary with sudo. Only the shoes process runs as root.
pub fn start_shoes_server_with_sudo(
    config: &str,
) -> std::io::Result<(ProcessGuard, tempfile::NamedTempFile)> {
    start_shoes_server_internal(config, true)
}

#[must_use = "the route is removed when its guard is dropped"]
pub struct RouteGuard {
    destination: String,
    device: String,
    family: &'static str,
}

pub fn add_route_via_device(destination: &str, device: &str) -> std::io::Result<RouteGuard> {
    let address = destination.split('/').next().unwrap_or(destination);
    let address: std::net::IpAddr = address
        .parse()
        .map_err(|error| std::io::Error::new(std::io::ErrorKind::InvalidInput, error))?;
    let family = if address.is_ipv6() { "-6" } else { "-4" };
    let output = Command::new("sudo")
        .args([
            "-n",
            "ip",
            family,
            "route",
            "add",
            destination,
            "dev",
            device,
        ])
        .output()?;
    if !output.status.success() {
        return Err(std::io::Error::other(format!(
            "failed to add route {destination} via {device}: {}",
            String::from_utf8_lossy(&output.stderr)
        )));
    }
    Ok(RouteGuard {
        destination: destination.to_string(),
        device: device.to_string(),
        family,
    })
}

impl Drop for RouteGuard {
    fn drop(&mut self) {
        match Command::new("sudo")
            .args([
                "-n",
                "ip",
                self.family,
                "route",
                "del",
                &self.destination,
                "dev",
                &self.device,
            ])
            .output()
        {
            Ok(output) if !output.status.success() => eprintln!(
                "[TUN] Failed to remove route {} via {}: {}",
                self.destination,
                self.device,
                String::from_utf8_lossy(&output.stderr)
            ),
            Err(error) => eprintln!(
                "[TUN] Failed to invoke route cleanup for {} via {}: {}",
                self.destination, self.device, error
            ),
            _ => {}
        }
    }
}

fn start_shoes_server_internal(
    config: &str,
    use_sudo: bool,
) -> std::io::Result<(ProcessGuard, tempfile::NamedTempFile)> {
    use std::io::Write;

    let mut config_file = tempfile::NamedTempFile::new()?;

    // Debug: Print config if it contains TUN or other notable keywords
    if config.contains("device_name") || config.contains("reality") || config.contains("vless") {
        eprintln!("========== SHOES CONFIG ==========");
        eprintln!("{}", config);
        eprintln!("========== END CONFIG ==========");
    }

    config_file.write_all(config.as_bytes())?;
    config_file.flush()?;
    let config_path = config_file.path().to_owned();

    let shoes_bin = find_shoes_binary()?;

    let rust_log = std::env::var("RUST_LOG").unwrap_or_else(|_| "debug".to_string());
    let guard = if use_sudo {
        spawn_privileged_process(&shoes_bin, "sudo-shoes", |command| {
            command
                .arg("-t")
                .arg("2")
                .arg("--no-reload")
                .arg(&config_path)
                .env("RUST_LOG", &rust_log)
                .stdout(Stdio::inherit())
                .stderr(Stdio::inherit());
        })?
    } else {
        let child = Command::new(&shoes_bin)
            .arg("-t")
            .arg("2")
            .arg("--no-reload")
            .arg(&config_path)
            .env("RUST_LOG", rust_log)
            .stdout(Stdio::inherit())
            .stderr(Stdio::inherit())
            .spawn()?;
        ProcessGuard::new(child, "shoes")
    };

    Ok((guard, config_file))
}

fn find_shoes_binary() -> std::io::Result<PathBuf> {
    for variable in ["SHOES_TEST_SHOES_BIN", "CARGO_BIN_EXE_shoes"] {
        if let Some(path) = std::env::var_os(variable) {
            return Ok(path.into());
        }
    }

    let mut path = std::env::current_exe()?;
    path.pop();
    if path.ends_with("deps") {
        path.pop();
    }
    path.push(format!("shoes{}", std::env::consts::EXE_SUFFIX));
    if path.is_file() {
        return Ok(path);
    }

    Err(std::io::Error::new(
        std::io::ErrorKind::NotFound,
        "shoes test binary not found; set SHOES_TEST_SHOES_BIN",
    ))
}

// TEST BUILDER INFRASTRUCTURE

use super::certs::generate_test_cert_files as generate_test_cert;
use super::test_servers::{self, TlsVersion};

/// UUID used across all tests for consistency
pub const TEST_UUID: &str = "b85798ef-e9dc-46a4-9a87-8da4499d36d0";

/// Build the stock sing-box HTTP entry that fronts a Shoes VLESS server.
///
/// The removed Shoes HTTP proxy front is replaced by this stock entry, so tests
/// keep dialing an HTTP proxy surface while the retained Shoes client runs
/// behind the VLESS front.
pub fn stock_vless_entry_config(
    entry_ip: &str,
    entry_port: u16,
    front_ip: &str,
    front_port: u16,
) -> String {
    format!(
        r#"{{
  "log": {{ "level": "debug" }},
  "inbounds": [
    {{
      "type": "http",
      "tag": "http-in",
      "listen": "{}",
      "listen_port": {}
    }}
  ],
  "outbounds": [
    {{
      "type": "vless",
      "tag": "vless-out",
      "server": "{}",
      "server_port": {},
      "uuid": "{}"
    }}
  ],
  "route": {{ "final": "vless-out" }}
}}"#,
        entry_ip, entry_port, front_ip, front_port, TEST_UUID
    )
}

/// Proxy role in the test chain
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum ProxyRole {
    /// Entry point - receives requests from curl/test client
    Entry,
    /// Intermediate proxy in the chain
    Intermediate,
    /// Exit proxy - sends requests to final destination
    Exit,
}

/// Inner protocol for REALITY
#[derive(Debug, Clone)]
pub enum RealityInnerProtocol {
    Vless,
    VlessVision,
}

/// REALITY configuration
#[derive(Debug, Clone)]
pub struct RealityConfig {
    pub private_key: String,
    pub public_key: String,
    pub server_name: String,
    pub short_id: String,
    pub dest: String,
    pub inner_protocol: RealityInnerProtocol,
}

/// Type of proxy server
#[derive(Debug, Clone)]
pub enum ProxyType {
    // VLESS
    ShoesTlsVisionServer,
    ShoesTlsVisionClient {
        upstream_ip: String,
        upstream_port: u16,
    },
    ShoesVlessServer,
    ShoesVlessClient {
        upstream_ip: String,
        upstream_port: u16,
    },
    ShoesVlessTlsServer,
    ShoesVlessTlsClient {
        upstream_ip: String,
        upstream_port: u16,
    },

    // REALITY
    ShoesRealityServer {
        config: RealityConfig,
    },
    ShoesRealityClient {
        upstream_ip: String,
        upstream_port: u16,
        config: RealityConfig,
    },
    ShoesRealityVisionServer {
        config: RealityConfig,
    },
    ShoesRealityVisionClient {
        upstream_ip: String,
        upstream_port: u16,
        config: RealityConfig,
    },

    // Sing-box
    SingboxVisionServer,
    SingboxVisionClient {
        upstream_ip: String,
        upstream_port: u16,
    },
    SingboxVlessServer,
    SingboxVlessClient {
        upstream_ip: String,
        upstream_port: u16,
    },
    SingboxVlessTlsServer,
    SingboxVlessTlsClient {
        upstream_ip: String,
        upstream_port: u16,
    },
    SingboxRealityVlessServer {
        config: RealityConfig,
    },

    SingboxRealityVlessClient {
        upstream_ip: String,
        upstream_port: u16,
        config: RealityConfig,
    },

    SingboxRealityVisionServer {
        config: RealityConfig,
    },
    SingboxRealityVisionClient {
        upstream_ip: String,
        upstream_port: u16,
        config: RealityConfig,
    },

    // TUN entry (uses TUN device + netstack-smoltcp as entry point)
    // Traffic flows: curl --interface tun0 -> TUN device -> netstack -> destination (or proxy chain)
    TunEntry {
        /// TUN device name
        tun_name: String,
        /// TUN device IP address (e.g., "10.200.100.1")
        tun_ip: String,
        /// Virtual server IP within TUN subnet (e.g., "10.200.100.2")
        virtual_server_ip: String,
    },

    // H2MUX (HTTP/2 multiplexing over VLESS+TLS)
    // Server side uses ShoesVlessTlsServer - h2mux is auto-detected
    SingboxH2muxVlessClient {
        upstream_ip: String,
        upstream_port: u16,
    },
    ShoesH2muxVlessClient {
        upstream_ip: String,
        upstream_port: u16,
    },
}

/// A proxy server in the test chain
#[derive(Clone)]
pub struct ProxyConfig {
    pub proxy_type: ProxyType,
    pub ip: String,
    pub port: u16,
    pub role: ProxyRole,
}

/// Generate a REALITY keypair for testing
pub fn generate_reality_keypair() -> (String, String) {
    use aws_lc_rs::{
        agreement,
        rand::{SecureRandom, SystemRandom},
    };
    use base64::engine::{Engine as _, general_purpose::URL_SAFE_NO_PAD};

    let rng = SystemRandom::new();

    // Generate raw private key bytes
    let mut private_bytes = [0u8; 32];
    rng.fill(&mut private_bytes)
        .expect("Failed to generate random bytes");

    // Create private key from bytes
    let private_key = agreement::PrivateKey::from_private_key(&agreement::X25519, &private_bytes)
        .expect("Failed to create private key");
    let public_key_bytes = private_key
        .compute_public_key()
        .expect("Failed to compute public key");

    let private_key_b64 = URL_SAFE_NO_PAD.encode(private_bytes);
    let public_key_b64 = URL_SAFE_NO_PAD.encode(public_key_bytes.as_ref());

    (private_key_b64, public_key_b64)
}

/// Builder for constructing test fixtures
pub struct ProxyTestFixture {
    proxies: Vec<ProxyConfig>,
    certs: Vec<(tempfile::TempPath, tempfile::TempPath)>,
    local_server: Option<LocalServerConfig>,
    ports: super::port_helper::PortHelper,
}

/// Configuration for local test HTTP/HTTPS server
pub struct LocalServerConfig {
    pub ip: String,
    pub port: u16,
    pub protocol: LocalServerProtocol,
}

#[derive(Clone, Copy)]
pub enum LocalServerProtocol {
    Http,
    HttpsTls12,
    HttpsTls13,
}

/// Built test fixture ready for testing
pub struct BuiltFixture {
    _tun_route: Option<RouteGuard>,
    _guards: Vec<ProcessGuard>,
    _local_servers: Vec<test_servers::TestServer>,
    _config_files: Vec<tempfile::NamedTempFile>,
    _certs: Vec<(tempfile::TempPath, tempfile::TempPath)>,
    /// Handles for TUN background tasks (keep alive for duration of test)
    _tun_handles: Vec<tokio::task::JoinHandle<()>>,
    entry_ip: String,
    entry_port: u16,
    local_server_ip: Option<String>,
    local_server_port: Option<u16>,
    local_server_protocol: Option<LocalServerProtocol>,
    /// TUN interface name (if TUN entry is used)
    tun_interface: Option<String>,
    /// Virtual server IP within TUN subnet (if TUN entry is used)
    tun_virtual_server_ip: Option<String>,
}

impl ProxyTestFixture {
    /// Create a new test fixture builder
    pub fn new() -> Self {
        Self {
            proxies: Vec::new(),
            certs: Vec::new(),
            local_server: None,
            ports: super::port_helper::PortHelper::new(),
        }
    }

    /// Add a shoes VLESS+Vision server (TLS with vision flag on tls_targets)
    pub fn with_shoes_vision_server(mut self) -> Self {
        let (ip, port) = self.ports.get_listener_port();
        let role = if self.proxies.is_empty() {
            ProxyRole::Entry
        } else {
            ProxyRole::Intermediate
        };
        self.proxies.push(ProxyConfig {
            ip,
            proxy_type: ProxyType::ShoesTlsVisionServer,
            port,
            role,
        });
        self
    }

    /// Add a shoes VLESS+Vision client (connects to next proxy in chain automatically)
    pub fn with_shoes_vision_client(mut self) -> Self {
        let (ip, port) = self.ports.get_listener_port();
        let role = if self.proxies.is_empty() {
            ProxyRole::Entry
        } else {
            ProxyRole::Intermediate
        };
        self.proxies.push(ProxyConfig {
            ip,
            proxy_type: ProxyType::ShoesTlsVisionClient {
                upstream_ip: String::new(),
                upstream_port: 0,
            },
            port,
            role,
        });
        self
    }

    /// Add a shoes VLESS server (no TLS, no Vision)
    pub fn with_shoes_vless_server(mut self) -> Self {
        let (ip, port) = self.ports.get_listener_port();
        let role = if self.proxies.is_empty() {
            ProxyRole::Entry
        } else {
            ProxyRole::Intermediate
        };
        self.proxies.push(ProxyConfig {
            ip,
            proxy_type: ProxyType::ShoesVlessServer,
            port,
            role,
        });
        self
    }

    /// Add a shoes VLESS client (no TLS, no Vision)
    pub fn with_shoes_vless_client(mut self) -> Self {
        let (ip, port) = self.ports.get_listener_port();
        let role = if self.proxies.is_empty() {
            ProxyRole::Entry
        } else {
            ProxyRole::Intermediate
        };
        self.proxies.push(ProxyConfig {
            ip,
            proxy_type: ProxyType::ShoesVlessClient {
                upstream_ip: String::new(),
                upstream_port: 0,
            },
            port,
            role,
        });
        self
    }

    /// Add a sing-box VLESS server (no TLS, no Vision)
    pub fn with_singbox_vless_server(mut self) -> Self {
        let (ip, port) = self.ports.get_listener_port();
        let role = if self.proxies.is_empty() {
            ProxyRole::Entry
        } else {
            ProxyRole::Intermediate
        };
        self.proxies.push(ProxyConfig {
            ip,
            proxy_type: ProxyType::SingboxVlessServer,
            port,
            role,
        });
        self
    }

    /// Add a sing-box VLESS client (no TLS, no Vision)
    pub fn with_singbox_vless_client(mut self) -> Self {
        let (ip, port) = self.ports.get_listener_port();
        let role = if self.proxies.is_empty() {
            ProxyRole::Entry
        } else {
            ProxyRole::Intermediate
        };
        self.proxies.push(ProxyConfig {
            ip,
            proxy_type: ProxyType::SingboxVlessClient {
                upstream_ip: String::new(),
                upstream_port: 0,
            },
            port,
            role,
        });
        self
    }

    /// Add a shoes VLESS+TLS server (TLS with sni_targets, no Vision)
    pub fn with_shoes_vless_tls_server(mut self) -> Self {
        let (ip, port) = self.ports.get_listener_port();
        let role = if self.proxies.is_empty() {
            ProxyRole::Entry
        } else {
            ProxyRole::Intermediate
        };
        self.proxies.push(ProxyConfig {
            ip,
            proxy_type: ProxyType::ShoesVlessTlsServer,
            port,
            role,
        });
        self
    }

    /// Add a shoes VLESS+TLS client (TLS with sni_hostname, no Vision)
    pub fn with_shoes_vless_tls_client(mut self) -> Self {
        let (ip, port) = self.ports.get_listener_port();
        let role = if self.proxies.is_empty() {
            ProxyRole::Entry
        } else {
            ProxyRole::Intermediate
        };
        self.proxies.push(ProxyConfig {
            ip,
            proxy_type: ProxyType::ShoesVlessTlsClient {
                upstream_ip: String::new(),
                upstream_port: 0,
            },
            port,
            role,
        });
        self
    }

    /// Add a sing-box VLESS+TLS server (TLS without Vision)
    pub fn with_singbox_vless_tls_server(mut self) -> Self {
        let (ip, port) = self.ports.get_listener_port();
        let role = if self.proxies.is_empty() {
            ProxyRole::Entry
        } else {
            ProxyRole::Intermediate
        };
        self.proxies.push(ProxyConfig {
            ip,
            proxy_type: ProxyType::SingboxVlessTlsServer,
            port,
            role,
        });
        self
    }

    /// Add a sing-box VLESS+TLS client (TLS without Vision)
    pub fn with_singbox_vless_tls_client(mut self) -> Self {
        let (ip, port) = self.ports.get_listener_port();
        let role = if self.proxies.is_empty() {
            ProxyRole::Entry
        } else {
            ProxyRole::Intermediate
        };
        self.proxies.push(ProxyConfig {
            ip,
            proxy_type: ProxyType::SingboxVlessTlsClient {
                upstream_ip: String::new(),
                upstream_port: 0,
            },
            port,
            role,
        });
        self
    }

    /// Add a sing-box VLESS+TLS client with h2mux multiplexing enabled
    pub fn with_singbox_h2mux_vless_client(mut self) -> Self {
        let (ip, port) = self.ports.get_listener_port();
        let role = if self.proxies.is_empty() {
            ProxyRole::Entry
        } else {
            ProxyRole::Intermediate
        };
        self.proxies.push(ProxyConfig {
            ip,
            proxy_type: ProxyType::SingboxH2muxVlessClient {
                upstream_ip: String::new(),
                upstream_port: 0,
            },
            port,
            role,
        });
        self
    }

    /// Add a shoes VLESS+TLS client with h2mux multiplexing enabled
    pub fn with_shoes_h2mux_vless_client(mut self) -> Self {
        let (ip, port) = self.ports.get_listener_port();
        let role = if self.proxies.is_empty() {
            ProxyRole::Entry
        } else {
            ProxyRole::Intermediate
        };
        self.proxies.push(ProxyConfig {
            ip,
            proxy_type: ProxyType::ShoesH2muxVlessClient {
                upstream_ip: String::new(),
                upstream_port: 0,
            },
            port,
            role,
        });
        self
    }

    /// Add a TUN entry point (uses TUN device + netstack-smoltcp).
    ///
    /// This creates a TUN network interface that captures all IP traffic.
    /// Traffic is processed by netstack-smoltcp and either forwarded directly
    /// to the destination or through subsequent proxies in the chain.
    ///
    /// **Requires root privileges** on Linux to create TUN devices.
    ///
    /// The TUN entry uses a unique subnet (10.200.X.0/24) where X is based on
    /// a counter to avoid conflicts with concurrent tests.
    pub fn with_tun_entry(mut self) -> Self {
        // Generate a unique subnet for this TUN device
        static TUN_COUNTER: std::sync::atomic::AtomicU8 = std::sync::atomic::AtomicU8::new(1);
        let subnet_id = TUN_COUNTER.fetch_add(1, std::sync::atomic::Ordering::SeqCst);

        let tun_name = format!("shoes_tun{}", subnet_id);
        let tun_ip = format!("10.200.{}.1", subnet_id);
        let virtual_server_ip = format!("10.200.{}.2", subnet_id);

        // TUN entry is always the first entry point
        assert!(
            self.proxies.is_empty()
                || !matches!(self.proxies[0].proxy_type, ProxyType::TunEntry { .. }),
            "Only one TUN entry is allowed per fixture"
        );

        // Port is not used for TUN (it's a network interface, not a TCP listener)
        // but we still track it for consistency with the proxy chain
        self.proxies.insert(
            0,
            ProxyConfig {
                ip: tun_ip.clone(),
                proxy_type: ProxyType::TunEntry {
                    tun_name,
                    tun_ip,
                    virtual_server_ip,
                },
                port: 0, // Not applicable for TUN
                role: ProxyRole::Entry,
            },
        );

        self
    }

    // ========================================================================
    // ========================================================================

    /// Add a sing-box VLESS+Vision client (connects to next proxy in chain automatically)
    pub fn with_singbox_vision_client(mut self) -> Self {
        let (ip, port) = self.ports.get_listener_port();
        let role = if self.proxies.is_empty() {
            ProxyRole::Entry
        } else {
            ProxyRole::Intermediate
        };
        self.proxies.push(ProxyConfig {
            ip,
            proxy_type: ProxyType::SingboxVisionClient {
                upstream_ip: String::new(),
                upstream_port: 0,
            },
            port,
            role,
        });
        self
    }

    /// Add a sing-box VLESS+Vision server
    pub fn with_singbox_vision_server(mut self) -> Self {
        let (ip, port) = self.ports.get_listener_port();
        let role = if self.proxies.is_empty() {
            ProxyRole::Entry
        } else {
            ProxyRole::Intermediate
        };
        self.proxies.push(ProxyConfig {
            ip,
            proxy_type: ProxyType::SingboxVisionServer,
            port,
            role,
        });
        self
    }

    /// Add a shoes REALITY+VLESS server
    pub fn with_shoes_reality_vless_server(mut self) -> Self {
        let (ip, port) = self.ports.get_listener_port();
        let (private_key, public_key) = generate_reality_keypair();

        let config = RealityConfig {
            private_key,
            public_key,
            server_name: "www.cloudflare.com".to_string(),
            short_id: "0123456789abcdef".to_string(),
            dest: "www.cloudflare.com:443".to_string(),
            inner_protocol: RealityInnerProtocol::Vless,
        };

        let role = if self.proxies.is_empty() {
            ProxyRole::Entry
        } else {
            ProxyRole::Intermediate
        };

        self.proxies.push(ProxyConfig {
            ip,
            proxy_type: ProxyType::ShoesRealityServer { config },
            port,
            role,
        });
        self
    }

    /// Add a shoes REALITY+VLESS server using a SEPARATE mode dest server
    ///
    /// Uses www.debian.org:443 which sends 4 encrypted records (first one <= 512 bytes)
    /// rather than 1 combined record. This tests the 512-byte heuristic for mode detection.
    pub fn with_shoes_reality_vless_server_separate_mode(mut self) -> Self {
        let (ip, port) = self.ports.get_listener_port();
        let (private_key, public_key) = generate_reality_keypair();

        let config = RealityConfig {
            private_key,
            public_key,
            server_name: "www.debian.org".to_string(),
            short_id: "0123456789abcdef".to_string(),
            dest: "www.debian.org:443".to_string(),
            inner_protocol: RealityInnerProtocol::Vless,
        };

        let role = if self.proxies.is_empty() {
            ProxyRole::Entry
        } else {
            ProxyRole::Intermediate
        };

        self.proxies.push(ProxyConfig {
            ip,
            proxy_type: ProxyType::ShoesRealityServer { config },
            port,
            role,
        });
        self
    }

    /// Add a sing-box REALITY+VLESS client (connects to next proxy in chain automatically)
    pub fn with_singbox_reality_vless_client(mut self) -> Self {
        let (ip, port) = self.ports.get_listener_port();

        let role = if self.proxies.is_empty() {
            ProxyRole::Entry
        } else {
            ProxyRole::Intermediate
        };

        // Placeholder config - will be populated during wire_proxy_chain
        let config = RealityConfig {
            private_key: String::new(),
            public_key: String::new(),
            server_name: String::new(),
            short_id: String::new(),
            dest: String::new(),
            inner_protocol: RealityInnerProtocol::Vless,
        };

        self.proxies.push(ProxyConfig {
            ip,
            proxy_type: ProxyType::SingboxRealityVlessClient {
                upstream_ip: String::new(),
                upstream_port: 0,
                config,
            },
            port,
            role,
        });
        self
    }

    /// Add a sing-box REALITY+VLESS server
    pub fn with_singbox_reality_vless_server(mut self) -> Self {
        let (ip, port) = self.ports.get_listener_port();
        let (private_key, public_key) = generate_reality_keypair();

        let config = RealityConfig {
            private_key,
            public_key,
            server_name: "www.cloudflare.com".to_string(),
            short_id: "0123456789abcdef".to_string(),
            dest: "www.cloudflare.com:443".to_string(),
            inner_protocol: RealityInnerProtocol::Vless,
        };

        let role = if self.proxies.is_empty() {
            ProxyRole::Entry
        } else {
            ProxyRole::Intermediate
        };

        self.proxies.push(ProxyConfig {
            ip,
            proxy_type: ProxyType::SingboxRealityVlessServer { config },
            port,
            role,
        });
        self
    }

    /// Add a shoes REALITY client (connects to next proxy in chain automatically)
    pub fn with_shoes_reality_client(mut self) -> Self {
        let (ip, port) = self.ports.get_listener_port();

        let role = if self.proxies.is_empty() {
            ProxyRole::Entry
        } else {
            ProxyRole::Intermediate
        };

        // Placeholder config - will be populated during wire_proxy_chain
        let config = RealityConfig {
            private_key: String::new(),
            public_key: String::new(),
            server_name: String::new(),
            short_id: String::new(),
            dest: String::new(),
            inner_protocol: RealityInnerProtocol::Vless,
        };

        self.proxies.push(ProxyConfig {
            ip,
            proxy_type: ProxyType::ShoesRealityClient {
                upstream_ip: String::new(),
                upstream_port: 0,
                config,
            },
            port,
            role,
        });
        self
    }

    /// Add a shoes REALITY+Vision server
    pub fn with_shoes_reality_vision_server(mut self) -> Self {
        let (ip, port) = self.ports.get_listener_port();
        let (private_key, public_key) = generate_reality_keypair();

        let config = RealityConfig {
            private_key,
            public_key,
            server_name: "www.cloudflare.com".to_string(),
            short_id: "0123456789abcdef".to_string(),
            dest: "www.cloudflare.com:443".to_string(),
            inner_protocol: RealityInnerProtocol::VlessVision,
        };

        let role = if self.proxies.is_empty() {
            ProxyRole::Entry
        } else {
            ProxyRole::Intermediate
        };

        self.proxies.push(ProxyConfig {
            ip,
            proxy_type: ProxyType::ShoesRealityVisionServer { config },
            port,
            role,
        });
        self
    }

    /// Add a shoes REALITY+Vision client (connects to next proxy in chain automatically)
    pub fn with_shoes_reality_vision_client(mut self) -> Self {
        let (ip, port) = self.ports.get_listener_port();

        let role = if self.proxies.is_empty() {
            ProxyRole::Entry
        } else {
            ProxyRole::Intermediate
        };

        // Placeholder config - will be populated during wire_proxy_chain
        let config = RealityConfig {
            private_key: String::new(),
            public_key: String::new(),
            server_name: String::new(),
            short_id: String::new(),
            dest: String::new(),
            inner_protocol: RealityInnerProtocol::VlessVision,
        };

        self.proxies.push(ProxyConfig {
            ip,
            proxy_type: ProxyType::ShoesRealityVisionClient {
                upstream_ip: String::new(),
                upstream_port: 0,
                config,
            },
            port,
            role,
        });
        self
    }

    /// Add a sing-box REALITY+Vision server
    pub fn with_singbox_reality_vision_server(mut self) -> Self {
        let (ip, port) = self.ports.get_listener_port();
        let (private_key, public_key) = generate_reality_keypair();

        let config = RealityConfig {
            private_key,
            public_key,
            server_name: "www.cloudflare.com".to_string(),
            short_id: "0123456789abcdef".to_string(),
            dest: "www.cloudflare.com:443".to_string(),
            inner_protocol: RealityInnerProtocol::VlessVision,
        };

        let role = if self.proxies.is_empty() {
            ProxyRole::Entry
        } else {
            ProxyRole::Intermediate
        };

        self.proxies.push(ProxyConfig {
            ip,
            proxy_type: ProxyType::SingboxRealityVisionServer { config },
            port,
            role,
        });
        self
    }

    /// Add a sing-box REALITY+Vision client (connects to next proxy in chain automatically)
    pub fn with_singbox_reality_vision_client(mut self) -> Self {
        let (ip, port) = self.ports.get_listener_port();

        let role = if self.proxies.is_empty() {
            ProxyRole::Entry
        } else {
            ProxyRole::Intermediate
        };

        // Placeholder config - will be populated during wire_proxy_chain
        let config = RealityConfig {
            private_key: String::new(),
            public_key: String::new(),
            server_name: String::new(),
            short_id: String::new(),
            dest: String::new(),
            inner_protocol: RealityInnerProtocol::VlessVision,
        };

        self.proxies.push(ProxyConfig {
            ip,
            proxy_type: ProxyType::SingboxRealityVisionClient {
                upstream_ip: String::new(),
                upstream_port: 0,
                config,
            },
            port,
            role,
        });
        self
    }

    // ==================== Local Test Servers ====================

    /// Add a local HTTP test server as final destination
    pub fn with_local_http_server(mut self) -> Self {
        let (ip, port) = self.ports.get_listener_port();
        self.local_server = Some(LocalServerConfig {
            ip,
            port,
            protocol: LocalServerProtocol::Http,
        });
        self
    }

    pub fn with_local_http_server_on_localhost(mut self) -> Self {
        let (ip, port) = self.ports.get_localhost_listener_port();
        self.local_server = Some(LocalServerConfig {
            ip,
            port,
            protocol: LocalServerProtocol::Http,
        });
        self
    }

    pub fn with_local_http_server_ipv6(mut self) -> Self {
        let (ip, port) = self.ports.get_ipv6_listener_port();
        self.local_server = Some(LocalServerConfig {
            ip,
            port,
            protocol: LocalServerProtocol::Http,
        });
        self
    }

    /// Add a local HTTPS/TLS1.3 test server as final destination
    pub fn with_local_https_tls13_server(mut self) -> Self {
        let (ip, port) = self.ports.get_listener_port();
        self.local_server = Some(LocalServerConfig {
            ip,
            port,
            protocol: LocalServerProtocol::HttpsTls13,
        });
        self
    }

    /// Add a local HTTPS/TLS1.2 test server as final destination
    pub fn with_local_https_tls12_server(mut self) -> Self {
        let (ip, port) = self.ports.get_listener_port();
        self.local_server = Some(LocalServerConfig {
            ip,
            port,
            protocol: LocalServerProtocol::HttpsTls12,
        });
        self
    }

    pub async fn test_local_http(
        self,
        response_size: usize,
    ) -> Result<(), Box<dyn std::error::Error>> {
        let fixture = self.with_local_http_server().build().await?;
        let body = fixture
            .test_local_server(&format!("/bytes/{response_size}"), false)
            .await?;
        if body != vec![b'X'; response_size] {
            return Err(format!(
                "local HTTP response did not match {response_size} expected bytes"
            )
            .into());
        }
        Ok(())
    }

    pub async fn test_local_http_hostname(
        self,
        response_size: usize,
    ) -> Result<(), Box<dyn std::error::Error>> {
        let fixture = self.with_local_http_server_on_localhost().build().await?;
        let url = format!(
            "http://localhost:{}/bytes/{response_size}",
            fixture.local_server_port().unwrap()
        );
        let output = fixture
            .test_http_with_options(
                &url,
                super::curl::CurlOptions::new()
                    .timeout(30)
                    .fail_on_http_error(true),
            )
            .await?;
        if !output.status.success() {
            return Err(format!(
                "curl failed with exit code {:?}: {}",
                output.status.code(),
                String::from_utf8_lossy(&output.stderr)
            )
            .into());
        }
        if output.stdout != vec![b'X'; response_size] {
            return Err(format!(
                "local HTTP response did not match {response_size} expected bytes"
            )
            .into());
        }
        Ok(())
    }

    pub async fn test_local_https_tls13(
        self,
        response_size: usize,
    ) -> Result<(), Box<dyn std::error::Error>> {
        let fixture = self.with_local_https_tls13_server().build().await?;
        let body = fixture
            .test_local_server(&format!("/bytes/{response_size}"), true)
            .await?;
        if body != vec![b'X'; response_size] {
            return Err(format!(
                "local HTTPS response did not match {response_size} expected bytes"
            )
            .into());
        }
        Ok(())
    }

    /// Wire up the proxy chain by setting upstream ports for client proxies
    fn wire_proxy_chain(&mut self) {
        for i in 0..self.proxies.len() {
            let needs_upstream = match &self.proxies[i].proxy_type {
                ProxyType::ShoesTlsVisionClient { upstream_port, .. } => *upstream_port == 0,
                ProxyType::ShoesVlessClient { upstream_port, .. } => *upstream_port == 0,
                ProxyType::ShoesVlessTlsClient { upstream_port, .. } => *upstream_port == 0,
                ProxyType::ShoesRealityClient { upstream_port, .. } => *upstream_port == 0,
                ProxyType::ShoesRealityVisionClient { upstream_port, .. } => *upstream_port == 0,
                ProxyType::SingboxVisionClient { upstream_port, .. } => *upstream_port == 0,
                ProxyType::SingboxVlessClient { upstream_port, .. } => *upstream_port == 0,
                ProxyType::SingboxVlessTlsClient { upstream_port, .. } => *upstream_port == 0,
                ProxyType::SingboxH2muxVlessClient { upstream_port, .. } => *upstream_port == 0,
                ProxyType::ShoesH2muxVlessClient { upstream_port, .. } => *upstream_port == 0,
                ProxyType::SingboxRealityVlessClient { upstream_port, .. } => *upstream_port == 0,
                ProxyType::SingboxRealityVisionClient { upstream_port, .. } => *upstream_port == 0,
                _ => false,
            };

            if needs_upstream && i + 1 < self.proxies.len() {
                let next_ip = self.proxies[i + 1].ip.clone();
                let next_port = self.proxies[i + 1].port;

                // For REALITY clients, we also need to copy the server's REALITY config
                if let ProxyType::SingboxRealityVlessClient { .. }
                | ProxyType::SingboxRealityVisionClient { .. }
                | ProxyType::ShoesRealityClient { .. }
                | ProxyType::ShoesRealityVisionClient { .. } = &self.proxies[i].proxy_type
                {
                    // Check if next proxy is a REALITY server (either shoes or sing-box)
                    let server_config = match &self.proxies[i + 1].proxy_type {
                        ProxyType::ShoesRealityServer { config } => Some(config.clone()),
                        ProxyType::ShoesRealityVisionServer { config } => Some(config.clone()),
                        ProxyType::SingboxRealityVlessServer { config } => Some(config.clone()),
                        ProxyType::SingboxRealityVisionServer { config } => Some(config.clone()),
                        _ => None,
                    };

                    if let Some(server_config) = server_config {
                        match &mut self.proxies[i].proxy_type {
                            ProxyType::SingboxRealityVlessClient {
                                upstream_ip,
                                upstream_port,
                                config,
                            } => {
                                *upstream_ip = next_ip.clone();
                                *upstream_port = next_port;
                                *config = server_config;
                            }
                            ProxyType::SingboxRealityVisionClient {
                                upstream_ip,
                                upstream_port,
                                config,
                            } => {
                                *upstream_ip = next_ip.clone();
                                *upstream_port = next_port;
                                *config = server_config;
                            }
                            ProxyType::ShoesRealityClient {
                                upstream_ip,
                                upstream_port,
                                config,
                            } => {
                                *upstream_ip = next_ip.clone();
                                *upstream_port = next_port;
                                *config = server_config;
                            }
                            ProxyType::ShoesRealityVisionClient {
                                upstream_ip,
                                upstream_port,
                                config,
                            } => {
                                *upstream_ip = next_ip.clone();
                                *upstream_port = next_port;
                                *config = server_config;
                            }
                            _ => {}
                        }
                    }
                } else {
                    match &mut self.proxies[i].proxy_type {
                        ProxyType::ShoesTlsVisionClient {
                            upstream_ip,
                            upstream_port,
                        } => {
                            *upstream_ip = next_ip.clone();
                            *upstream_port = next_port
                        }
                        ProxyType::ShoesVlessClient {
                            upstream_ip,
                            upstream_port,
                        } => {
                            *upstream_ip = next_ip.clone();
                            *upstream_port = next_port
                        }
                        ProxyType::ShoesVlessTlsClient {
                            upstream_ip,
                            upstream_port,
                        } => {
                            *upstream_ip = next_ip.clone();
                            *upstream_port = next_port
                        }
                        ProxyType::SingboxVisionClient {
                            upstream_ip,
                            upstream_port,
                        } => {
                            *upstream_ip = next_ip.clone();
                            *upstream_port = next_port
                        }
                        ProxyType::SingboxVlessClient {
                            upstream_ip,
                            upstream_port,
                        } => {
                            *upstream_ip = next_ip.clone();
                            *upstream_port = next_port
                        }
                        ProxyType::SingboxVlessTlsClient {
                            upstream_ip,
                            upstream_port,
                        } => {
                            *upstream_ip = next_ip.clone();
                            *upstream_port = next_port
                        }
                        ProxyType::SingboxH2muxVlessClient {
                            upstream_ip,
                            upstream_port,
                        } => {
                            *upstream_ip = next_ip.clone();
                            *upstream_port = next_port
                        }
                        ProxyType::ShoesH2muxVlessClient {
                            upstream_ip,
                            upstream_port,
                        } => {
                            *upstream_ip = next_ip.clone();
                            *upstream_port = next_port
                        }
                        _ => {}
                    }
                }
            }
        }
    }

    /// Build the fixture and start all servers
    pub async fn build(mut self) -> Result<BuiltFixture, Box<dyn std::error::Error>> {
        // Wire up proxy chain
        self.wire_proxy_chain();

        let mut guards = Vec::new();
        let mut local_servers = Vec::new();
        let mut config_files = Vec::new();
        let tun_handles: Vec<tokio::task::JoinHandle<()>> = Vec::new();
        let mut tun_route = None;
        let mut tun_interface: Option<String> = None;
        let mut tun_virtual_server_ip: Option<String> = None;

        // Start local server if configured
        if let Some(ref local_config) = self.local_server {
            match local_config.protocol {
                LocalServerProtocol::Http => {
                    local_servers.push(
                        test_servers::start_local_http_server(&local_config.ip, local_config.port)
                            .await?,
                    );
                }
                LocalServerProtocol::HttpsTls12 => {
                    let (cert, key) = generate_test_cert()?;
                    local_servers.push(
                        test_servers::start_local_https_server(
                            &local_config.ip,
                            local_config.port,
                            AsRef::<Path>::as_ref(&cert),
                            AsRef::<Path>::as_ref(&key),
                            TlsVersion::Tls12Only,
                        )
                        .await?,
                    );
                    self.certs.push((cert, key));
                }
                LocalServerProtocol::HttpsTls13 => {
                    let (cert, key) = generate_test_cert()?;
                    local_servers.push(
                        test_servers::start_local_https_server(
                            &local_config.ip,
                            local_config.port,
                            AsRef::<Path>::as_ref(&cert),
                            AsRef::<Path>::as_ref(&key),
                            TlsVersion::Tls13Only,
                        )
                        .await?,
                    );
                    self.certs.push((cert, key));
                }
            }
        }

        // Set up TUN entry if configured (must be done before starting proxies)
        // We start shoes with a TUN server config that handles the TUN device
        if let Some(tun_proxy) = self.proxies.first()
            && let ProxyType::TunEntry {
                tun_name,
                tun_ip,
                virtual_server_ip: vip,
            } = &tun_proxy.proxy_type
        {
            eprintln!(
                "[TUN] Setting up TUN via shoes: device={} ip={} vip={}",
                tun_name, tun_ip, vip
            );

            // Get local server info for forwarding
            let local_server_ip = self
                .local_server
                .as_ref()
                .map(|s| s.ip.clone())
                .unwrap_or_else(|| "127.0.0.1".to_string());
            let local_server_port = self.local_server.as_ref().map(|s| s.port).unwrap_or(80);

            // Generate TUN server config for shoes
            // The TUN server will forward all traffic to the local HTTP server
            let tun_config = format!(
                r#"
# TUN server that forwards to local HTTP server
- device_name: "{}"
  address: "{}"
  netmask: 255.255.255.0
  mtu: 1500
  tcp_enabled: true
  udp_enabled: true
  icmp_enabled: true
  rules:
    - masks: "0.0.0.0/0"
      action: allow
      override_address: "{}:{}"
      client_chain:
        - protocol:
            type: direct
"#,
                tun_name, tun_ip, local_server_ip, local_server_port,
            );

            eprintln!("[TUN] Starting shoes with TUN config:\n{}", tun_config);

            // Start shoes with sudo for TUN device creation
            let (tun_guard, tun_config_file) = start_shoes_server_with_sudo(&tun_config)?;
            guards.push(tun_guard);
            config_files.push(tun_config_file);

            // Wait for TUN device to be created
            tokio::time::sleep(std::time::Duration::from_millis(500)).await;

            // Add route for the virtual server IP through the TUN device
            let destination = format!("{vip}/32");
            tun_route = Some(add_route_via_device(&destination, tun_name)?);
            eprintln!("[TUN] Added route for {} via {}", vip, tun_name);

            tun_interface = Some(tun_name.clone());
            tun_virtual_server_ip = Some(vip.clone());

            // Give shoes TUN server time to fully initialize
            tokio::time::sleep(std::time::Duration::from_millis(300)).await;
        }

        // Generate shoes configs - each shoes proxy gets its own process
        let shoes_configs = self.generate_shoes_configs()?;
        for config in shoes_configs {
            let (guard, config_file) = start_shoes_server(&config)?;
            guards.push(guard);
            config_files.push(config_file);
        }

        // Generate and start sing-box configs (each needs separate process)
        let proxies_clone = self.proxies.clone();
        for proxy in &proxies_clone {
            if let Some(config) = self.generate_singbox_config(proxy)? {
                let (guard, config_file) = start_singbox_server(&config)?;
                guards.push(guard);
                config_files.push(config_file);
            }
        }

        // Retained Shoes REALITY clients are fronted by a VLESS server. Start a
        // stock sing-box HTTP entry in front of the chain entry so tests keep
        // dialing an HTTP proxy surface.
        let mut stock_entry = None;
        if let Some(first) = proxies_clone.first()
            && matches!(
                &first.proxy_type,
                ProxyType::ShoesRealityClient { .. }
                    | ProxyType::ShoesRealityVisionClient { .. }
                    | ProxyType::ShoesH2muxVlessClient { .. }
            )
        {
            let (entry_ip, entry_port) = self.ports.get_listener_port();
            let config = stock_vless_entry_config(&entry_ip, entry_port, &first.ip, first.port);
            let (guard, config_file) = start_singbox_server(&config)?;
            guards.push(guard);
            config_files.push(config_file);
            stock_entry = Some((entry_ip, entry_port));
        }

        // Wait for all listener ports to be ready
        self.ports.wait_for_all_ports().await?;

        // Find entry IP and port (first proxy in chain, or its stock HTTP entry)
        let first_proxy = self.proxies.first().ok_or("No proxies configured")?;
        let (entry_ip, entry_port) =
            stock_entry.unwrap_or_else(|| (first_proxy.ip.clone(), first_proxy.port));

        let local_server_ip = self.local_server.as_ref().map(|s| s.ip.clone());
        let local_server_port = self.local_server.as_ref().map(|s| s.port);
        let local_server_protocol = self.local_server.as_ref().map(|s| s.protocol);

        Ok(BuiltFixture {
            _tun_route: tun_route,
            _guards: guards,
            _local_servers: local_servers,
            _config_files: config_files,
            _certs: self.certs,
            _tun_handles: tun_handles,
            entry_ip,
            entry_port,
            local_server_ip,
            local_server_port,
            local_server_protocol,
            tun_interface,
            tun_virtual_server_ip,
        })
    }

    /// Generate the Shoes configs for the configured proxies without starting any
    /// process. Used by offline config regressions for retained fixtures.
    pub fn preview_shoes_configs(&mut self) -> Result<Vec<String>, Box<dyn std::error::Error>> {
        self.wire_proxy_chain();
        self.generate_shoes_configs()
    }

    /// Generate shoes config for all shoes proxies
    fn generate_shoes_configs(&mut self) -> Result<Vec<String>, Box<dyn std::error::Error>> {
        let mut configs = Vec::new();

        for proxy in &self.proxies {
            let config = match &proxy.proxy_type {
                ProxyType::ShoesTlsVisionServer => {
                    let (cert, key) = generate_test_cert()?;
                    let cfg = format!(
                        r#"- address: "{}:{}"
  protocol:
    type: tls
    tls_targets:
      test.local:
        cert: {}
        key: {}
        vision: true
        protocol:
          type: vless
          user_id: "{}"
          udp_enabled: true"#,
                        proxy.ip,
                        proxy.port,
                        AsRef::<Path>::as_ref(&cert).to_str().unwrap(),
                        AsRef::<Path>::as_ref(&key).to_str().unwrap(),
                        TEST_UUID
                    );
                    self.certs.push((cert, key));
                    Some(cfg)
                }
                ProxyType::ShoesTlsVisionClient {
                    upstream_ip,
                    upstream_port,
                } => {
                    let (cert, key) = generate_test_cert()?;
                    let cfg = format!(
                        r#"- client_group: default
  client_proxy:
    - address: "{}:{}"
      protocol:
        type: tls
        sni_hostname: test.local
        verify: false
        vision: true
        protocol:
          type: vless
          user_id: "{}"

- address: "{}:{}"
  protocol:
    type: http
  rule:
    mask: "0.0.0.0/0"
    action: allow
    client_proxy: default"#,
                        upstream_ip, upstream_port, TEST_UUID, proxy.ip, proxy.port
                    );
                    self.certs.push((cert, key));
                    Some(cfg)
                }
                ProxyType::ShoesVlessServer => Some(format!(
                    r#"- address: "{}:{}"
  protocol:
    type: vless
    user_id: "{}""#,
                    proxy.ip, proxy.port, TEST_UUID
                )),
                ProxyType::ShoesVlessClient {
                    upstream_ip,
                    upstream_port,
                } => Some(format!(
                    r#"- client_group: default
  client_proxy:
    - address: "{}:{}"
      protocol:
        type: vless
        user_id: "{}"

- address: "{}:{}"
  protocol:
    type: http
  rule:
    mask: "0.0.0.0/0"
    action: allow
    client_proxy: default"#,
                    upstream_ip, upstream_port, TEST_UUID, proxy.ip, proxy.port
                )),
                ProxyType::ShoesVlessTlsServer => {
                    let (cert, key) = generate_test_cert()?;
                    let cfg = format!(
                        r#"- address: "{}:{}"
  protocol:
    type: tls
    sni_targets:
      test.local:
        cert: {}
        key: {}
        protocol:
          type: vless
          user_id: "{}""#,
                        proxy.ip,
                        proxy.port,
                        AsRef::<Path>::as_ref(&cert).to_str().unwrap(),
                        AsRef::<Path>::as_ref(&key).to_str().unwrap(),
                        TEST_UUID
                    );
                    self.certs.push((cert, key));
                    Some(cfg)
                }
                ProxyType::ShoesVlessTlsClient {
                    upstream_ip,
                    upstream_port,
                } => Some(format!(
                    r#"- client_group: default
  client_proxy:
    - address: "{}:{}"
      protocol:
        type: tls
        sni_hostname: "test.local"
        verify: false
        protocol:
          type: vless
          user_id: "{}"

- address: "{}:{}"
  protocol:
    type: http
  rule:
    mask: "0.0.0.0/0"
    action: allow
    client_proxy: default"#,
                    upstream_ip, upstream_port, TEST_UUID, proxy.ip, proxy.port
                )),
                ProxyType::ShoesH2muxVlessClient {
                    upstream_ip,
                    upstream_port,
                } => Some(format!(
                    r#"- client_group: default
  client_proxy:
    - address: "{}:{}"
      protocol:
        type: tls
        sni_hostname: "test.local"
        verify: false
        protocol:
          type: vless
          user_id: "{}"
          h2mux:
            max_connections: 4
            min_streams: 1

- address: "{}:{}"
  protocol:
    type: vless
    user_id: "{}"
  rule:
    mask: "0.0.0.0/0"
    action: allow
    client_proxy: default"#,
                    upstream_ip, upstream_port, TEST_UUID, proxy.ip, proxy.port, TEST_UUID
                )),
                ProxyType::ShoesRealityServer { config } => {
                    let protocol_config = match &config.inner_protocol {
                        RealityInnerProtocol::Vless => format!(
                            r#"protocol:
          type: vless
          user_id: "{}"
          udp_enabled: false"#,
                            TEST_UUID
                        ),
                        RealityInnerProtocol::VlessVision => format!(
                            r#"protocol:
          type: vless
          user_id: "{}"
          udp_enabled: false"#,
                            TEST_UUID
                        ),
                    };

                    Some(format!(
                        r#"- address: "{}:{}"
  protocol:
    type: tls
    reality_targets:
      "{}":
        private_key: "{}"
        short_ids:
          - "{}"
        dest: "{}"
        max_time_diff: 60000
        {}"#,
                        proxy.ip,
                        proxy.port,
                        config.server_name, // Use server_name (SNI) as the key
                        config.private_key,
                        config.short_id,
                        config.dest,
                        protocol_config
                    ))
                }
                ProxyType::ShoesRealityClient {
                    upstream_ip,
                    upstream_port,
                    config,
                } => {
                    // REALITY client with standalone protocol type
                    let inner_protocol_config = match &config.inner_protocol {
                        RealityInnerProtocol::Vless => format!(
                            r#"type: vless
          user_id: "{}""#,
                            TEST_UUID
                        ),
                        RealityInnerProtocol::VlessVision => format!(
                            r#"type: vless
          user_id: "{}""#,
                            TEST_UUID
                        ),
                    };

                    Some(format!(
                        r#"- client_group: default
  client_proxy:
    - address: "{}:{}"
      protocol:
        type: reality
        public_key: "{}"
        short_id: "{}"
        sni_hostname: "{}"
        protocol:
          {}

- address: "{}:{}"
  protocol:
    type: vless
    user_id: "{}"
  rule:
    mask: "0.0.0.0/0"
    action: allow
    client_proxy: default"#,
                        upstream_ip,
                        upstream_port,
                        config.public_key,
                        config.short_id,
                        config.server_name,
                        inner_protocol_config,
                        proxy.ip,
                        proxy.port,
                        TEST_UUID
                    ))
                }
                ProxyType::ShoesRealityVisionServer { config } => {
                    // REALITY Vision server - always uses VlessVision
                    Some(format!(
                        r#"- address: "{}:{}"
  protocol:
    type: tls
    reality_targets:
      "{}":
        private_key: "{}"
        short_ids:
          - "{}"
        dest: "{}"
        max_time_diff: 60000
        vision: true
        protocol:
          type: vless
          user_id: "{}"
          udp_enabled: false"#,
                        proxy.ip,
                        proxy.port,
                        config.server_name,
                        config.private_key,
                        config.short_id,
                        config.dest,
                        TEST_UUID
                    ))
                }
                ProxyType::ShoesRealityVisionClient {
                    upstream_ip,
                    upstream_port,
                    config,
                } => {
                    // REALITY Vision client - always uses VlessVision
                    Some(format!(
                        r#"- client_group: default
  client_proxy:
    - address: "{}:{}"
      protocol:
        type: reality
        public_key: "{}"
        short_id: "{}"
        sni_hostname: "{}"
        vision: true
        protocol:
          type: vless
          user_id: "{}"

- address: "{}:{}"
  protocol:
    type: vless
    user_id: "{}"
  rule:
    mask: "0.0.0.0/0"
    action: allow
    client_proxy: default"#,
                        upstream_ip,
                        upstream_port,
                        config.public_key,
                        config.short_id,
                        config.server_name,
                        TEST_UUID,
                        proxy.ip,
                        proxy.port,
                        TEST_UUID
                    ))
                }
                _ => None,
            };

            if let Some(cfg) = config {
                configs.push(cfg);
            }
        }

        Ok(configs)
    }

    /// Generate sing-box config for a single sing-box proxy
    fn generate_singbox_config(
        &mut self,
        proxy: &ProxyConfig,
    ) -> Result<Option<String>, Box<dyn std::error::Error>> {
        let config = match &proxy.proxy_type {
            ProxyType::SingboxVisionClient {
                upstream_ip,
                upstream_port,
            } => Some(format!(
                r#"{{
  "log": {{
    "level": "debug"
  }},
  "inbounds": [
    {{
      "type": "http",
      "tag": "http-in",
      "listen": "{}",
      "listen_port": {}
    }}
  ],
  "outbounds": [
    {{
      "type": "vless",
      "tag": "vless-out",
      "server": "{}",
      "server_port": {},
      "uuid": "{}",
      "flow": "xtls-rprx-vision",
      "tls": {{
        "enabled": true,
        "server_name": "test.local",
        "insecure": true
      }}
    }}
  ],
  "route": {{
    "final": "vless-out"
  }}
}}"#,
                proxy.ip, proxy.port, upstream_ip, upstream_port, TEST_UUID
            )),
            ProxyType::SingboxVisionServer => {
                let (cert, key) = generate_test_cert()?;
                let cfg = format!(
                    r#"{{
  "log": {{
    "level": "debug"
  }},
  "inbounds": [
    {{
      "type": "vless",
      "tag": "vless-in",
      "listen": "{}",
      "listen_port": {},
      "users": [
        {{
          "uuid": "{}",
          "flow": "xtls-rprx-vision"
        }}
      ],
      "tls": {{
        "enabled": true,
        "server_name": "test.local",
        "certificate_path": "{}",
        "key_path": "{}"
      }}
    }}
  ],
  "outbounds": [
    {{
      "type": "direct",
      "tag": "direct-out"
    }}
  ],
  "route": {{
    "final": "direct-out"
  }}
}}"#,
                    proxy.ip,
                    proxy.port,
                    TEST_UUID,
                    AsRef::<Path>::as_ref(&cert).to_str().unwrap(),
                    AsRef::<Path>::as_ref(&key).to_str().unwrap()
                );
                self.certs.push((cert, key));
                Some(cfg)
            }
            ProxyType::SingboxVlessServer => Some(format!(
                r#"{{
  "log": {{
    "level": "debug"
  }},
  "inbounds": [
    {{
      "type": "vless",
      "tag": "vless-in",
      "listen": "{}",
      "listen_port": {},
      "users": [
        {{
          "uuid": "{}"
        }}
      ]
    }}
  ],
  "outbounds": [
    {{
      "type": "direct",
      "tag": "direct"
    }}
  ],
  "route": {{
    "final": "direct"
  }}
}}"#,
                proxy.ip, proxy.port, TEST_UUID
            )),
            ProxyType::SingboxVlessClient {
                upstream_ip,
                upstream_port,
            } => Some(format!(
                r#"{{
  "log": {{
    "level": "debug"
  }},
  "inbounds": [
    {{
      "type": "http",
      "tag": "http-in",
      "listen": "{}",
      "listen_port": {}
    }}
  ],
  "outbounds": [
    {{
      "type": "vless",
      "tag": "vless-out",
      "server": "{}",
      "server_port": {},
      "uuid": "{}"
    }}
  ],
  "route": {{
    "final": "vless-out"
  }}
}}"#,
                proxy.ip, proxy.port, upstream_ip, upstream_port, TEST_UUID
            )),
            ProxyType::SingboxVlessTlsServer => {
                let (cert, key) = generate_test_cert()?;
                let cfg = format!(
                    r#"{{
  "log": {{
    "level": "debug"
  }},
  "inbounds": [
    {{
      "type": "vless",
      "tag": "vless-in",
      "listen": "{}",
      "listen_port": {},
      "users": [
        {{
          "uuid": "{}"
        }}
      ],
      "tls": {{
        "enabled": true,
        "server_name": "test.local",
        "certificate_path": "{}",
        "key_path": "{}"
      }}
    }}
  ],
  "outbounds": [
    {{
      "type": "direct",
      "tag": "direct"
    }}
  ],
  "route": {{
    "final": "direct"
  }}
}}"#,
                    proxy.ip,
                    proxy.port,
                    TEST_UUID,
                    AsRef::<Path>::as_ref(&cert).to_str().unwrap(),
                    AsRef::<Path>::as_ref(&key).to_str().unwrap()
                );
                self.certs.push((cert, key));
                Some(cfg)
            }
            ProxyType::SingboxVlessTlsClient {
                upstream_ip,
                upstream_port,
            } => Some(format!(
                r#"{{
  "log": {{
    "level": "debug"
  }},
  "inbounds": [
    {{
      "type": "http",
      "tag": "http-in",
      "listen": "{}",
      "listen_port": {}
    }}
  ],
  "outbounds": [
    {{
      "type": "vless",
      "tag": "vless-out",
      "server": "{}",
      "server_port": {},
      "uuid": "{}",
      "tls": {{
        "enabled": true,
        "server_name": "test.local",
        "insecure": true
      }}
    }}
  ],
  "route": {{
    "final": "vless-out"
  }}
}}"#,
                proxy.ip, proxy.port, upstream_ip, upstream_port, TEST_UUID
            )),
            ProxyType::SingboxH2muxVlessClient {
                upstream_ip,
                upstream_port,
            } => Some(format!(
                r#"{{
  "log": {{
    "level": "warn"
  }},
  "inbounds": [
    {{
      "type": "http",
      "tag": "http-in",
      "listen": "{}",
      "listen_port": {}
    }}
  ],
  "outbounds": [
    {{
      "type": "vless",
      "tag": "vless-out",
      "server": "{}",
      "server_port": {},
      "uuid": "{}",
      "tls": {{
        "enabled": true,
        "server_name": "test.local",
        "insecure": true
      }},
      "multiplex": {{
        "enabled": true,
        "protocol": "h2mux",
        "max_connections": 1,
        "min_streams": 4,
        "max_streams": 0
      }}
    }}
  ],
  "route": {{
    "final": "vless-out"
  }}
}}"#,
                proxy.ip, proxy.port, upstream_ip, upstream_port, TEST_UUID
            )),
            ProxyType::SingboxRealityVlessClient {
                upstream_ip,
                upstream_port,
                config,
            } => Some(format!(
                r#"{{
  "log": {{
    "level": "debug"
  }},
  "inbounds": [
    {{
      "type": "http",
      "tag": "http-in",
      "listen": "{}",
      "listen_port": {}
    }}
  ],
  "outbounds": [
    {{
      "type": "vless",
      "tag": "reality-out",
      "server": "{}",
      "server_port": {},
      "uuid": "{}",
      "flow": "",
      "packet_encoding": "",
      "tls": {{
        "enabled": true,
        "server_name": "{}",
        "insecure": true,
        "utls": {{
          "enabled": true,
          "fingerprint": "chrome"
        }},
        "reality": {{
          "enabled": true,
          "public_key": "{}",
          "short_id": "{}"
        }}
      }}
    }}
  ],
  "route": {{
    "final": "reality-out"
  }}
}}"#,
                proxy.ip,
                proxy.port,
                upstream_ip,
                upstream_port,
                TEST_UUID,
                config.server_name,
                config.public_key,
                config.short_id
            )),
            ProxyType::SingboxRealityVlessServer { config } => Some(format!(
                r#"{{
  "log": {{
    "level": "trace"
  }},
  "inbounds": [
    {{
      "type": "vless",
      "tag": "vless-in",
      "listen": "{}",
      "listen_port": {},
      "users": [
        {{
          "uuid": "{}"
        }}
      ],
      "tls": {{
        "enabled": true,
        "server_name": "{}",
        "reality": {{
          "enabled": true,
          "handshake": {{
            "server": "{}",
            "server_port": 443
          }},
          "private_key": "{}",
          "short_id": [
            "{}"
          ]
        }}
      }}
    }}
  ],
  "outbounds": [
    {{
      "type": "direct",
      "tag": "direct"
    }}
  ],
  "route": {{
    "final": "direct"
  }}
}}"#,
                proxy.ip,
                proxy.port,
                TEST_UUID,
                config.server_name,
                config.dest.split(':').next().unwrap_or(&config.dest),
                config.private_key,
                config.short_id
            )),
            ProxyType::SingboxRealityVisionClient {
                upstream_ip,
                upstream_port,
                config,
            } => Some(format!(
                r#"{{
  "log": {{
    "level": "debug"
  }},
  "inbounds": [
    {{
      "type": "http",
      "tag": "http-in",
      "listen": "{}",
      "listen_port": {}
    }}
  ],
  "outbounds": [
    {{
      "type": "vless",
      "tag": "reality-vision-out",
      "server": "{}",
      "server_port": {},
      "uuid": "{}",
      "flow": "xtls-rprx-vision",
      "packet_encoding": "",
      "tls": {{
        "enabled": true,
        "server_name": "{}",
        "insecure": true,
        "utls": {{
          "enabled": true,
          "fingerprint": "chrome"
        }},
        "reality": {{
          "enabled": true,
          "public_key": "{}",
          "short_id": "{}"
        }}
      }}
    }}
  ],
  "route": {{
    "final": "reality-vision-out"
  }}
}}"#,
                proxy.ip,
                proxy.port,
                upstream_ip,
                upstream_port,
                TEST_UUID,
                config.server_name,
                config.public_key,
                config.short_id
            )),
            ProxyType::SingboxRealityVisionServer { config } => Some(format!(
                r#"{{
  "log": {{
    "level": "trace"
  }},
  "inbounds": [
    {{
      "type": "vless",
      "tag": "vless-vision-in",
      "listen": "{}",
      "listen_port": {},
      "users": [
        {{
          "uuid": "{}",
          "flow": "xtls-rprx-vision"
        }}
      ],
      "tls": {{
        "enabled": true,
        "server_name": "{}",
        "reality": {{
          "enabled": true,
          "handshake": {{
            "server": "{}",
            "server_port": 443
          }},
          "private_key": "{}",
          "short_id": [
            "{}"
          ]
        }}
      }}
    }}
  ],
  "outbounds": [
    {{
      "type": "direct",
      "tag": "direct"
    }}
  ],
  "route": {{
    "final": "direct"
  }}
}}"#,
                proxy.ip,
                proxy.port,
                TEST_UUID,
                config.server_name,
                config.dest.split(':').next().unwrap_or(&config.dest),
                config.private_key,
                config.short_id
            )),
            _ => None,
        };

        Ok(config)
    }
}

impl Default for ProxyTestFixture {
    fn default() -> Self {
        Self::new()
    }
}

impl BuiltFixture {
    /// Test an HTTP request through the proxy chain to the internet
    pub async fn test_http(&self, url: &str) -> Result<(), Box<dyn std::error::Error>> {
        use super::curl::*;

        let output = run_curl(
            url,
            CurlOptions::new()
                .proxy(format!("http://{}:{}", self.entry_ip, self.entry_port))
                .timeout(10),
        )
        .await?;

        assert!(
            output.status.success(),
            "curl failed with exit code: {:?}\nstderr: {}",
            output.status.code(),
            String::from_utf8_lossy(&output.stderr)
        );

        Ok(())
    }

    /// Test an HTTP request with custom curl options
    pub async fn test_http_with_options(
        &self,
        url: &str,
        mut options: super::curl::CurlOptions,
    ) -> Result<std::process::Output, Box<dyn std::error::Error>> {
        options = options.proxy(format!("http://{}:{}", self.entry_ip, self.entry_port));
        Ok(super::curl::run_curl(url, options).await?)
    }

    /// Test a request to the local HTTP/HTTPS server
    pub async fn test_local_server(
        &self,
        path: &str,
        insecure: bool,
    ) -> Result<Vec<u8>, Box<dyn std::error::Error>> {
        let ip = self
            .local_server_ip
            .as_ref()
            .ok_or("No local server configured")?;
        let port = self.local_server_port.ok_or("No local server configured")?;

        let protocol = if insecure { "https" } else { "http" };
        let host = if ip.contains(':') {
            format!("[{ip}]")
        } else {
            ip.clone()
        };
        let url = format!("{protocol}://{host}:{port}{path}");

        let mut options = super::curl::CurlOptions::new()
            .proxy(format!("http://{}:{}", self.entry_ip, self.entry_port))
            .timeout(30);

        if insecure {
            options = options.insecure(true);

            // Force TLS 1.2 if using TLS 1.2 local server
            if let Some(LocalServerProtocol::HttpsTls12) = self.local_server_protocol {
                options = options.tls_version("1.2").tls_max_version("1.2");
            }

            // Force TLS 1.3 if using TLS 1.3 local server
            if let Some(LocalServerProtocol::HttpsTls13) = self.local_server_protocol {
                options = options.tls_version("1.3").tls_max_version("1.3");
            }
        }

        let output = super::curl::run_curl(&url, options).await?;

        assert!(
            output.status.success(),
            "curl failed with exit code: {:?}\nstderr: {}",
            output.status.code(),
            String::from_utf8_lossy(&output.stderr)
        );

        Ok(output.stdout)
    }

    /// Get the entry IP for the proxy chain
    pub fn entry_ip(&self) -> &str {
        &self.entry_ip
    }

    /// Get the entry port for the proxy chain
    pub fn entry_port(&self) -> u16 {
        self.entry_port
    }

    /// Get the local server port if configured
    pub fn local_server_port(&self) -> Option<u16> {
        self.local_server_port
    }

    /// Get the local server IP if configured
    pub fn local_server_ip(&self) -> Option<&str> {
        self.local_server_ip.as_deref()
    }

    /// Get the TUN interface name if configured
    pub fn tun_interface(&self) -> Option<&str> {
        self.tun_interface.as_deref()
    }

    /// Get the TUN virtual server IP if configured
    pub fn tun_virtual_server_ip(&self) -> Option<&str> {
        self.tun_virtual_server_ip.as_deref()
    }

    /// Check if this fixture uses TUN as entry point
    pub fn is_tun_entry(&self) -> bool {
        self.tun_interface.is_some()
    }

    /// Test a request to the local HTTP server via TUN interface.
    ///
    /// This routes traffic through the TUN device instead of using an HTTP proxy.
    /// Requires TUN entry to be configured.
    pub async fn test_local_server_via_tun(
        &self,
        path: &str,
    ) -> Result<Vec<u8>, Box<dyn std::error::Error>> {
        let _tun_iface = self
            .tun_interface
            .as_ref()
            .ok_or("TUN entry not configured")?;
        let virtual_ip = self
            .tun_virtual_server_ip
            .as_ref()
            .ok_or("TUN virtual server IP not configured")?;
        let port = self.local_server_port.ok_or("No local server configured")?;

        // Traffic to virtual_ip goes through TUN, then netstack forwards to local server
        let url = format!("http://{}:{}{}", virtual_ip, port, path);

        let options = super::curl::CurlOptions::new().timeout(30);

        let output = super::curl::run_curl(&url, options).await?;

        assert!(
            output.status.success(),
            "curl via TUN failed with exit code: {:?}\nstderr: {}",
            output.status.code(),
            String::from_utf8_lossy(&output.stderr)
        );

        Ok(output.stdout)
    }

    /// Test a streaming request to the local HTTP server via TUN (discards output).
    /// Returns the elapsed time.
    pub async fn test_local_server_streaming_via_tun(
        &self,
        path: &str,
        timeout_secs: u32,
    ) -> Result<std::time::Duration, Box<dyn std::error::Error>> {
        let tun_iface = self
            .tun_interface
            .as_ref()
            .ok_or("TUN entry not configured")?;
        let virtual_ip = self
            .tun_virtual_server_ip
            .as_ref()
            .ok_or("TUN virtual server IP not configured")?;
        let port = self.local_server_port.ok_or("No local server configured")?;

        let url = format!("http://{}:{}{}", virtual_ip, port, path);

        let options = super::curl::CurlOptions::new()
            .timeout(timeout_secs)
            .discard_output(true)
            .interface(tun_iface);

        let start = std::time::Instant::now();
        let output = super::curl::run_curl(&url, options).await?;
        let elapsed = start.elapsed();

        assert!(
            output.status.success(),
            "curl via TUN failed with exit code: {:?}\nstderr: {}",
            output.status.code(),
            String::from_utf8_lossy(&output.stderr)
        );

        Ok(elapsed)
    }

    /// Test a verified streaming request to the local HTTP server via TUN.
    ///
    /// Uses /bytes_verified/N endpoint which returns N bytes of data followed by
    /// 32-byte SHA256 digest. Verifies data integrity.
    pub async fn test_local_server_streaming_verified_via_tun(
        &self,
        data_size: u64,
        timeout_secs: u64,
    ) -> Result<std::time::Duration, Box<dyn std::error::Error>> {
        use aws_lc_rs::digest::{Context, SHA256};
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        use tokio::net::TcpStream;
        use tokio::time::timeout;

        let _tun_iface = self
            .tun_interface
            .as_ref()
            .ok_or("TUN entry not configured")?;
        let virtual_ip = self
            .tun_virtual_server_ip
            .as_ref()
            .ok_or("TUN virtual server IP not configured")?;
        let port = self.local_server_port.ok_or("No local server configured")?;

        let start = std::time::Instant::now();

        // Connect directly to the virtual IP (goes through TUN -> netstack -> local server)
        let addr = format!("{}:{}", virtual_ip, port);
        let mut stream = timeout(
            std::time::Duration::from_secs(10),
            TcpStream::connect(&addr),
        )
        .await??;

        // Send HTTP request
        let request = format!(
            "GET /bytes_verified/{} HTTP/1.1\r\nHost: {}:{}\r\nConnection: close\r\n\r\n",
            data_size, virtual_ip, port
        );
        stream.write_all(request.as_bytes()).await?;

        // Read headers
        let mut header_buf = Vec::with_capacity(1024);
        loop {
            let mut byte = [0u8; 1];
            timeout(
                std::time::Duration::from_secs(timeout_secs),
                stream.read_exact(&mut byte),
            )
            .await??;
            header_buf.push(byte[0]);
            if header_buf.ends_with(b"\r\n\r\n") {
                break;
            }
        }

        // Parse Content-Length
        let header_str = String::from_utf8_lossy(&header_buf);
        let content_length: u64 = header_str
            .lines()
            .find(|l| l.to_lowercase().starts_with("content-length:"))
            .and_then(|l| l.split(':').nth(1))
            .and_then(|v| v.trim().parse().ok())
            .ok_or("Missing Content-Length header")?;

        let expected_total = data_size + 32; // data + SHA256 digest
        if content_length != expected_total {
            return Err(format!(
                "Unexpected Content-Length: {} (expected {})",
                content_length, expected_total
            )
            .into());
        }

        // Stream body and compute digest
        let mut hasher = Context::new(&SHA256);
        let mut data_remaining = data_size;
        let mut buf = [0u8; 65536];

        while data_remaining > 0 {
            let to_read = std::cmp::min(data_remaining as usize, buf.len());
            let n = timeout(
                std::time::Duration::from_secs(timeout_secs),
                stream.read(&mut buf[..to_read]),
            )
            .await??;
            if n == 0 {
                return Err("Unexpected EOF while reading data".into());
            }
            hasher.update(&buf[..n]);
            data_remaining -= n as u64;
        }

        // Read server's digest
        let mut server_digest = [0u8; 32];
        timeout(
            std::time::Duration::from_secs(timeout_secs),
            stream.read_exact(&mut server_digest),
        )
        .await??;

        // Verify
        let computed = hasher.finish();
        if computed.as_ref() != server_digest {
            return Err("Digest mismatch! Data corruption detected".into());
        }

        Ok(start.elapsed())
    }

    /// Test a streaming request to the local HTTP/HTTPS server (discards output to /dev/null)
    /// This is useful for benchmarking large transfers without memory buffering.
    /// Returns the time taken in milliseconds.
    pub async fn test_local_server_streaming(
        &self,
        path: &str,
        insecure: bool,
        timeout_secs: u32,
    ) -> Result<std::time::Duration, Box<dyn std::error::Error>> {
        let ip = self
            .local_server_ip
            .as_ref()
            .ok_or("No local server configured")?;
        let port = self.local_server_port.ok_or("No local server configured")?;

        let protocol = if insecure { "https" } else { "http" };
        let host = if ip.contains(':') {
            format!("[{ip}]")
        } else {
            ip.clone()
        };
        let url = format!("{protocol}://{host}:{port}{path}");

        let mut options = super::curl::CurlOptions::new()
            .proxy(format!("http://{}:{}", self.entry_ip, self.entry_port))
            .timeout(timeout_secs)
            .discard_output(true);

        if insecure {
            options = options.insecure(true);

            // Force TLS 1.2 if using TLS 1.2 local server
            if let Some(LocalServerProtocol::HttpsTls12) = self.local_server_protocol {
                options = options.tls_version("1.2").tls_max_version("1.2");
            }

            // Force TLS 1.3 if using TLS 1.3 local server
            if let Some(LocalServerProtocol::HttpsTls13) = self.local_server_protocol {
                options = options.tls_version("1.3").tls_max_version("1.3");
            }
        }

        let start = std::time::Instant::now();
        let output = super::curl::run_curl(&url, options).await?;
        let elapsed = start.elapsed();

        assert!(
            output.status.success(),
            "curl failed with exit code: {:?}\nstderr: {}",
            output.status.code(),
            String::from_utf8_lossy(&output.stderr)
        );

        Ok(elapsed)
    }

    /// Test a verified streaming request to the local HTTP server.
    ///
    /// Uses /bytes_verified/N endpoint which returns N bytes of data followed by
    /// 32-byte SHA256 digest. Client computes digest while streaming and verifies
    /// it matches the server's digest at the end.
    ///
    /// Returns the elapsed time on success, or error if verification fails.
    pub async fn test_local_server_streaming_verified(
        &self,
        data_size: u64,
        timeout_secs: u64,
    ) -> Result<std::time::Duration, Box<dyn std::error::Error>> {
        use aws_lc_rs::digest::{Context, SHA256};
        use tokio::io::{AsyncBufReadExt, AsyncRead, AsyncReadExt, AsyncWriteExt, BufReader};
        use tokio::net::TcpStream;
        use tokio::time::timeout;

        let ip = self
            .local_server_ip
            .as_ref()
            .ok_or("No local server configured")?;
        let port = self.local_server_port.ok_or("No local server configured")?;

        let is_https = matches!(
            self.local_server_protocol,
            Some(LocalServerProtocol::HttpsTls13) | Some(LocalServerProtocol::HttpsTls12)
        );

        let start = std::time::Instant::now();

        // Connect to proxy
        let proxy_addr = format!("{}:{}", self.entry_ip, self.entry_port);
        let stream = timeout(Duration::from_secs(10), TcpStream::connect(&proxy_addr)).await??;

        // Helper macro to read headers from any AsyncRead stream
        async fn read_headers<R: AsyncRead + Unpin>(
            stream: &mut R,
            timeout_secs: u64,
        ) -> Result<(String, Option<u64>), Box<dyn std::error::Error>> {
            let mut header_buf = Vec::with_capacity(1024);
            loop {
                let mut byte = [0u8; 1];
                tokio::time::timeout(
                    Duration::from_secs(timeout_secs),
                    stream.read_exact(&mut byte),
                )
                .await??;
                header_buf.push(byte[0]);

                if header_buf.len() >= 4 {
                    let len = header_buf.len();
                    if &header_buf[len - 4..] == b"\r\n\r\n" {
                        break;
                    }
                }

                if header_buf.len() > 16384 {
                    return Err("HTTP headers too large".into());
                }
            }

            let headers_str = String::from_utf8_lossy(&header_buf).to_string();
            let mut content_length = None;
            for line in headers_str.lines() {
                if line.to_lowercase().starts_with("content-length:")
                    && let Some(v) = line.split(':').nth(1).map(str::trim)
                {
                    content_length = v.parse().ok();
                }
            }
            Ok((headers_str, content_length))
        }

        // Helper to stream and verify body from any AsyncRead stream
        async fn stream_and_verify<R: AsyncRead + Unpin>(
            stream: &mut R,
            data_size: u64,
            total_size: u64,
            timeout_secs: u64,
        ) -> Result<(), Box<dyn std::error::Error>> {
            let mut hasher = Context::new(&SHA256);
            let mut bytes_read = 0u64;
            let mut digest_buf = Vec::with_capacity(32);
            let mut read_buf = vec![0u8; 64 * 1024];

            while bytes_read < total_size {
                let to_read =
                    std::cmp::min(read_buf.len() as u64, total_size - bytes_read) as usize;
                let n = tokio::time::timeout(
                    Duration::from_secs(timeout_secs),
                    stream.read(&mut read_buf[..to_read]),
                )
                .await??;

                if n == 0 {
                    return Err(format!(
                        "Unexpected EOF: read {} bytes, expected {}",
                        bytes_read, total_size
                    )
                    .into());
                }

                let chunk = &read_buf[..n];
                let chunk_start = bytes_read;
                let chunk_end = bytes_read + n as u64;

                if chunk_end <= data_size {
                    hasher.update(chunk);
                } else if chunk_start >= data_size {
                    digest_buf.extend_from_slice(chunk);
                } else {
                    let data_part = (data_size - chunk_start) as usize;
                    hasher.update(&chunk[..data_part]);
                    digest_buf.extend_from_slice(&chunk[data_part..]);
                }

                bytes_read = chunk_end;
            }

            if digest_buf.len() != 32 {
                return Err(format!(
                    "Digest buffer wrong size: expected 32, got {}",
                    digest_buf.len()
                )
                .into());
            }

            let computed = hasher.finish();
            let computed_hex: String = computed
                .as_ref()
                .iter()
                .map(|b| format!("{:02x}", b))
                .collect();
            let received_hex: String = digest_buf.iter().map(|b| format!("{:02x}", b)).collect();
            eprintln!("[CLIENT] SHA256 computed: {}", computed_hex);
            eprintln!("[CLIENT] SHA256 received: {}", received_hex);
            if computed.as_ref() != &digest_buf[..] {
                return Err(format!(
                    "SHA256 digest mismatch!\n  Expected: {:02x?}\n  Computed: {:02x?}",
                    &digest_buf[..],
                    computed.as_ref()
                )
                .into());
            }
            Ok(())
        }

        if is_https {
            // For HTTPS: use CONNECT tunnel, then TLS, then HTTP request
            let mut stream = stream;

            // Send CONNECT request to establish tunnel
            let connect_req = format!(
                "CONNECT {}:{} HTTP/1.1\r\n\
                 Host: {}:{}\r\n\
                 \r\n",
                ip, port, ip, port
            );
            stream.write_all(connect_req.as_bytes()).await?;

            // Read CONNECT response
            let mut buf_reader = BufReader::new(stream);
            let mut response_line = String::new();
            buf_reader.read_line(&mut response_line).await?;
            if !response_line.starts_with("HTTP/1.1 200")
                && !response_line.starts_with("HTTP/1.0 200")
            {
                return Err(format!("CONNECT failed: {}", response_line.trim()).into());
            }
            // Read remaining headers until empty line
            loop {
                let mut line = String::new();
                buf_reader.read_line(&mut line).await?;
                if line == "\r\n" || line == "\n" {
                    break;
                }
            }
            let stream = buf_reader.into_inner();

            // TLS handshake through the tunnel (skip cert verification for self-signed test certs)
            use rustls::client::danger::{
                HandshakeSignatureValid, ServerCertVerified, ServerCertVerifier,
            };
            use rustls::pki_types::{CertificateDer, ServerName, UnixTime};
            use rustls::{DigitallySignedStruct, SignatureScheme};

            #[derive(Debug)]
            struct InsecureCertVerifier;

            impl ServerCertVerifier for InsecureCertVerifier {
                fn verify_server_cert(
                    &self,
                    _end_entity: &CertificateDer<'_>,
                    _intermediates: &[CertificateDer<'_>],
                    _server_name: &ServerName<'_>,
                    _ocsp_response: &[u8],
                    _now: UnixTime,
                ) -> Result<ServerCertVerified, rustls::Error> {
                    Ok(ServerCertVerified::assertion())
                }

                fn verify_tls12_signature(
                    &self,
                    _message: &[u8],
                    _cert: &CertificateDer<'_>,
                    _dss: &DigitallySignedStruct,
                ) -> Result<HandshakeSignatureValid, rustls::Error> {
                    Ok(HandshakeSignatureValid::assertion())
                }

                fn verify_tls13_signature(
                    &self,
                    _message: &[u8],
                    _cert: &CertificateDer<'_>,
                    _dss: &DigitallySignedStruct,
                ) -> Result<HandshakeSignatureValid, rustls::Error> {
                    Ok(HandshakeSignatureValid::assertion())
                }

                fn supported_verify_schemes(&self) -> Vec<SignatureScheme> {
                    vec![
                        SignatureScheme::RSA_PKCS1_SHA256,
                        SignatureScheme::RSA_PKCS1_SHA384,
                        SignatureScheme::RSA_PKCS1_SHA512,
                        SignatureScheme::ECDSA_NISTP256_SHA256,
                        SignatureScheme::ECDSA_NISTP384_SHA384,
                        SignatureScheme::ECDSA_NISTP521_SHA512,
                        SignatureScheme::RSA_PSS_SHA256,
                        SignatureScheme::RSA_PSS_SHA384,
                        SignatureScheme::RSA_PSS_SHA512,
                        SignatureScheme::ED25519,
                    ]
                }
            }

            let tls_config = rustls::ClientConfig::builder()
                .dangerous()
                .with_custom_certificate_verifier(std::sync::Arc::new(InsecureCertVerifier))
                .with_no_client_auth();

            let connector = tokio_rustls::TlsConnector::from(std::sync::Arc::new(tls_config));
            let ip_owned = ip.clone();
            let server_name =
                rustls::pki_types::ServerName::try_from(ip_owned).unwrap_or_else(|_| {
                    rustls::pki_types::ServerName::try_from("localhost".to_string()).unwrap()
                });
            let mut tls_stream = connector.connect(server_name, stream).await?;

            // Send HTTP request over TLS (relative URL since we're tunneled)
            let request = format!(
                "GET /bytes_verified/{} HTTP/1.1\r\n\
                 Host: {}:{}\r\n\
                 Connection: close\r\n\
                 \r\n",
                data_size, ip, port
            );
            tls_stream.write_all(request.as_bytes()).await?;

            // Read and verify response
            let (headers_str, content_length) = read_headers(&mut tls_stream, timeout_secs).await?;
            if !headers_str.starts_with("HTTP/1.1 200") && !headers_str.starts_with("HTTP/1.0 200")
            {
                return Err(format!(
                    "HTTP request failed: {}",
                    headers_str.lines().next().unwrap_or("")
                )
                .into());
            }

            let total_size = content_length.ok_or("No Content-Length header")?;
            let expected_total = data_size + 32;
            if total_size != expected_total {
                return Err(format!(
                    "Content-Length mismatch: expected {}, got {}",
                    expected_total, total_size
                )
                .into());
            }

            stream_and_verify(&mut tls_stream, data_size, total_size, timeout_secs).await?;

            let elapsed = start.elapsed();
            eprintln!(
                "[VERIFIED] {} bytes transferred and verified in {:.2}s ({:.2} MB/s)",
                data_size,
                elapsed.as_secs_f64(),
                data_size as f64 / 1024.0 / 1024.0 / elapsed.as_secs_f64()
            );
            Ok(elapsed)
        } else {
            // For HTTP: send GET with absolute URL directly
            let mut stream = stream;
            let request = format!(
                "GET http://{}:{}/bytes_verified/{} HTTP/1.1\r\n\
                 Host: {}:{}\r\n\
                 Connection: close\r\n\
                 \r\n",
                ip, port, data_size, ip, port
            );
            stream.write_all(request.as_bytes()).await?;

            let (headers_str, content_length) = read_headers(&mut stream, timeout_secs).await?;
            if !headers_str.starts_with("HTTP/1.1 200") && !headers_str.starts_with("HTTP/1.0 200")
            {
                return Err(format!(
                    "HTTP request failed: {}",
                    headers_str.lines().next().unwrap_or("")
                )
                .into());
            }

            let total_size = content_length.ok_or("No Content-Length header")?;
            let expected_total = data_size + 32;
            if total_size != expected_total {
                return Err(format!(
                    "Content-Length mismatch: expected {}, got {}",
                    expected_total, total_size
                )
                .into());
            }

            stream_and_verify(&mut stream, data_size, total_size, timeout_secs).await?;

            let elapsed = start.elapsed();
            eprintln!(
                "[VERIFIED] {} bytes transferred and verified in {:.2}s ({:.2} MB/s)",
                data_size,
                elapsed.as_secs_f64(),
                data_size as f64 / 1024.0 / 1024.0 / elapsed.as_secs_f64()
            );
            Ok(elapsed)
        }
    }
}
