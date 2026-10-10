#[path = "cases/integration/h2mux_integration.rs"]
mod h2mux;
#[cfg(target_os = "linux")]
#[path = "cases/integration/linux_splice_integration.rs"]
mod linux_splice;
#[path = "cases/integration/reality_client_config_test.rs"]
mod reality_client_config;
#[path = "cases/integration/resource_lifecycle_test.rs"]
mod resource_lifecycle;
#[path = "cases/integration/vless_integration.rs"]
mod vless;
#[path = "cases/integration/vless_cross_protocol_udp_test.rs"]
mod vless_cross_protocol_udp;
#[path = "cases/integration/vless_xudp_integration.rs"]
mod vless_xudp;
#[path = "cases/integration/vless_xudp_edge_cases.rs"]
mod vless_xudp_edge_cases;
#[path = "cases/integration/vless_xudp_hostname_test.rs"]
mod vless_xudp_hostname;
#[path = "cases/integration/vless_xudp_multiip_response_test.rs"]
mod vless_xudp_multiip_response;
#[path = "cases/integration/vless_xudp_realistic_mosh_test.rs"]
mod vless_xudp_realistic_mosh;
#[path = "cases/integration/vless_xudp_true_multiplexing.rs"]
mod vless_xudp_true_multiplexing;
