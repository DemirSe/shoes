// This library shares code with the shoes binary. Server-side code appears "unused"
// in lib builds but is used by: (1) the binary for server mode, (2) FFI for mobile.
// The client/server code is intermingled within modules - a proper fix would require
// splitting into separate client/server modules or using feature flags.
#![allow(dead_code)]

//! shoes - A high-performance multi-protocol proxy server.
//!
//! This library provides the core functionality for shoes, enabling it to be
//! embedded in mobile applications (Android/iOS) as a VPN backend.
//!
//! # Features
//!
//! - **VLESS support**: VLESS over TLS, REALITY, and Vision, with H2MUX and XUDP
//! - **TUN device support**: Virtual network interface for VPN mode
//! - **Proxy chaining**: Connect through multiple proxies
//! - **Flexible routing**: Rule-based traffic routing
//!
//! # Mobile Integration
//!
//! For Android, use the FFI module:
//!
//! ```kotlin
//! // Load native library
//! System.loadLibrary("shoes")
//!
//! // Initialize
//! ShoesNative.init("info")
//!
//! // Start VPN with TUN fd from VpnService
//! val handle = ShoesNative.startTun(tunFd, configYaml, protectCallback)
//!
//! // Stop VPN
//! ShoesNative.stop(handle)
//! ```
//!
//! For iOS, use the C FFI module from Swift:
//!
//! ```swift
//! // Initialize
//! shoes_init("info")
//!
//! // Start VPN with packet tunnel fd
//! let handle = shoes_start(configYaml, protectCallback)
//!
//! // Stop VPN
//! shoes_stop(handle)
//! ```
//!
//! # Platform Support
//!
//! - Linux (x86_64, aarch64)
//! - Android (arm64-v8a, armeabi-v7a, x86_64)
//! - iOS (arm64)

// Modules are declared here (mirroring main.rs) so the library crate can
// expose them for FFI/mobile integration.
mod address;
mod async_stream;
mod buf_reader;
mod client_proxy_chain;
mod client_proxy_selector;
mod copy_bidirectional;
mod copy_bidirectional_message;
mod crypto;
pub mod dns;
mod h2mux;
#[cfg(target_os = "linux")]
mod linux_splice;
mod listener_tasks;
mod memory;
mod option_util;
mod prepend_stream;
mod quic_endpoint;
mod reality;
mod reality_client_handler;
pub mod resolver;
pub mod resources;
mod routing;
mod rustls_config_util;
mod rustls_connection_util;
mod slide_buffer;
mod socket_util;
mod stream_reader;
mod sync_adapter;
mod tcp;
mod thread_util;
mod tls_client_handler;
mod tls_hello;
mod tls_server_handler;
mod uot;
mod util;
mod uuid_util;
mod vless;
mod xudp;

/// Configuration types.
pub mod config;

/// Multi-output logging infrastructure.
pub mod logging;

/// TUN device support for VPN mode.
#[cfg(unix)]
pub mod tun;

/// FFI bindings for mobile platforms.
#[cfg(any(target_os = "android", target_os = "ios", feature = "ffi"))]
pub mod ffi;
