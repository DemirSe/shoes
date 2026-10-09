use common::test_fixture::ProxyTestFixture;
/// Benchmark tests for retained proxy protocols
///
/// These tests stream 2GB through various proxy configurations to measure throughput.
/// Tests are separated into:
/// - Non-verified: Uses curl with output discarded, fastest for pure throughput testing
/// - Verified: Computes SHA256 on both ends to verify data integrity
use shoes_test_support as common;

#[tokio::test]
async fn test_reality_vless_singbox_client_shoes_server_2gb()
-> Result<(), Box<dyn std::error::Error>> {
    let fixture = ProxyTestFixture::new()
        .with_singbox_reality_vless_client()
        .with_shoes_reality_vless_server()
        .with_local_http_server()
        .build()
        .await?;

    // 2GB = 2147483648 bytes, timeout 300 seconds
    let elapsed = fixture
        .test_local_server_streaming("/bytes/2147483648", false, 300)
        .await?;

    let mb_per_sec = 2048.0 / elapsed.as_secs_f64();
    eprintln!(
        "[BENCHMARK] REALITY+VLESS 2GB transfer completed in {:.2}s ({:.2} MB/s)",
        elapsed.as_secs_f64(),
        mb_per_sec
    );
    Ok(())
}

#[tokio::test]
async fn test_reality_vless_singbox_client_shoes_server_2gb_verified()
-> Result<(), Box<dyn std::error::Error>> {
    let fixture = ProxyTestFixture::new()
        .with_singbox_reality_vless_client()
        .with_shoes_reality_vless_server()
        .with_local_http_server()
        .build()
        .await?;

    // 2GB = 2147483648 bytes, timeout 600 seconds (longer for verification overhead)
    let elapsed = fixture
        .test_local_server_streaming_verified(2147483648, 600)
        .await?;

    let mb_per_sec = 2048.0 / elapsed.as_secs_f64();
    eprintln!(
        "[BENCHMARK] REALITY+VLESS 2GB verified transfer completed in {:.2}s ({:.2} MB/s)",
        elapsed.as_secs_f64(),
        mb_per_sec
    );
    Ok(())
}

#[tokio::test]
async fn test_reality_vision_https_singbox_client_shoes_server_2gb()
-> Result<(), Box<dyn std::error::Error>> {
    let fixture = ProxyTestFixture::new()
        .with_singbox_reality_vision_client()
        .with_shoes_reality_vision_server()
        .with_local_https_tls13_server()
        .build()
        .await?;

    // 2GB = 2147483648 bytes, timeout 300 seconds
    let elapsed = fixture
        .test_local_server_streaming("/bytes/2147483648", true, 300)
        .await?;

    let mb_per_sec = 2048.0 / elapsed.as_secs_f64();
    eprintln!(
        "[BENCHMARK] REALITY Vision HTTPS 2GB transfer completed in {:.2}s ({:.2} MB/s)",
        elapsed.as_secs_f64(),
        mb_per_sec
    );
    Ok(())
}

#[tokio::test]
async fn test_reality_vision_https_singbox_client_shoes_server_2gb_verified()
-> Result<(), Box<dyn std::error::Error>> {
    let fixture = ProxyTestFixture::new()
        .with_singbox_reality_vision_client()
        .with_shoes_reality_vision_server()
        .with_local_https_tls13_server()
        .build()
        .await?;

    // 2GB = 2147483648 bytes, timeout 600 seconds (longer for verification overhead)
    let elapsed = fixture
        .test_local_server_streaming_verified(2147483648, 600)
        .await?;

    let mb_per_sec = 2048.0 / elapsed.as_secs_f64();
    eprintln!(
        "[BENCHMARK] REALITY Vision HTTPS 2GB verified transfer completed in {:.2}s ({:.2} MB/s)",
        elapsed.as_secs_f64(),
        mb_per_sec
    );
    Ok(())
}

#[tokio::test]
async fn test_tun_streaming_2gb() -> Result<(), Box<dyn std::error::Error>> {
    let fixture = ProxyTestFixture::new()
        .with_tun_entry()
        .with_local_http_server()
        .build()
        .await?;

    // 2GB = 2147483648 bytes, timeout 600 seconds (TUN may be slower)
    let elapsed = fixture
        .test_local_server_streaming_via_tun("/bytes/2147483648", 600)
        .await?;

    let mb_per_sec = 2048.0 / elapsed.as_secs_f64();
    eprintln!(
        "[BENCHMARK] TUN + netstack 2GB transfer completed in {:.2}s ({:.2} MB/s)",
        elapsed.as_secs_f64(),
        mb_per_sec
    );
    Ok(())
}

#[tokio::test]
async fn test_tun_streaming_2gb_verified() -> Result<(), Box<dyn std::error::Error>> {
    let fixture = ProxyTestFixture::new()
        .with_tun_entry()
        .with_local_http_server()
        .build()
        .await?;

    // 2GB = 2147483648 bytes, timeout 900 seconds (TUN + verification overhead)
    let elapsed = fixture
        .test_local_server_streaming_verified_via_tun(2147483648, 900)
        .await?;

    let mb_per_sec = 2048.0 / elapsed.as_secs_f64();
    eprintln!(
        "[BENCHMARK] TUN + netstack 2GB verified transfer completed in {:.2}s ({:.2} MB/s)",
        elapsed.as_secs_f64(),
        mb_per_sec
    );
    Ok(())
}

#[tokio::test]
async fn test_h2mux_vless_singbox_client_shoes_server_2gb() -> Result<(), Box<dyn std::error::Error>>
{
    let fixture = ProxyTestFixture::new()
        .with_singbox_h2mux_vless_client()
        .with_shoes_vless_tls_server()
        .with_local_http_server()
        .build()
        .await?;

    // 2GB = 2147483648 bytes, timeout 600 seconds
    let elapsed = fixture
        .test_local_server_streaming("/bytes/2147483648", false, 600)
        .await?;

    let mb_per_sec = 2048.0 / elapsed.as_secs_f64();
    eprintln!(
        "[BENCHMARK] H2MUX VLESS 2GB transfer completed in {:.2}s ({:.2} MB/s)",
        elapsed.as_secs_f64(),
        mb_per_sec
    );
    Ok(())
}

#[tokio::test]
async fn test_h2mux_vless_singbox_client_shoes_server_2gb_verified()
-> Result<(), Box<dyn std::error::Error>> {
    let fixture = ProxyTestFixture::new()
        .with_singbox_h2mux_vless_client()
        .with_shoes_vless_tls_server()
        .with_local_http_server()
        .build()
        .await?;

    // 2GB = 2147483648 bytes, timeout 600 seconds
    let elapsed = fixture
        .test_local_server_streaming_verified(2147483648, 600)
        .await?;

    let mb_per_sec = 2048.0 / elapsed.as_secs_f64();
    eprintln!(
        "[BENCHMARK] H2MUX VLESS 2GB verified transfer completed in {:.2}s ({:.2} MB/s)",
        elapsed.as_secs_f64(),
        mb_per_sec
    );
    Ok(())
}

// These benchmarks use a Shoes client and server behind a stock sing-box entry.
// The entry is part of the measured path.

/// 2GB streaming test for REALITY+VLESS (shoes client -> shoes server)
#[tokio::test]
async fn test_reality_vless_shoes_client_shoes_server_2gb() -> Result<(), Box<dyn std::error::Error>>
{
    let fixture = ProxyTestFixture::new()
        .with_shoes_reality_client()
        .with_shoes_reality_vless_server()
        .with_local_http_server()
        .build()
        .await?;

    let elapsed = fixture
        .test_local_server_streaming("/bytes/2147483648", false, 300)
        .await?;

    let mb_per_sec = 2048.0 / elapsed.as_secs_f64();
    eprintln!(
        "[BENCHMARK] Shoes-to-Shoes REALITY+VLESS 2GB transfer completed in {:.2}s ({:.2} MB/s)",
        elapsed.as_secs_f64(),
        mb_per_sec
    );
    Ok(())
}

/// 2GB streaming test for REALITY+VLESS with verification (shoes client -> shoes server)
#[tokio::test]
async fn test_reality_vless_shoes_client_shoes_server_2gb_verified()
-> Result<(), Box<dyn std::error::Error>> {
    let fixture = ProxyTestFixture::new()
        .with_shoes_reality_client()
        .with_shoes_reality_vless_server()
        .with_local_http_server()
        .build()
        .await?;

    let elapsed = fixture
        .test_local_server_streaming_verified(2147483648, 600)
        .await?;

    let mb_per_sec = 2048.0 / elapsed.as_secs_f64();
    eprintln!(
        "[BENCHMARK] Shoes-to-Shoes REALITY+VLESS 2GB verified transfer completed in {:.2}s ({:.2} MB/s)",
        elapsed.as_secs_f64(),
        mb_per_sec
    );
    Ok(())
}

/// 2GB streaming test for REALITY Vision over HTTPS (shoes client -> shoes server)
#[tokio::test]
async fn test_reality_vision_https_shoes_client_shoes_server_2gb()
-> Result<(), Box<dyn std::error::Error>> {
    let fixture = ProxyTestFixture::new()
        .with_shoes_reality_vision_client()
        .with_shoes_reality_vision_server()
        .with_local_https_tls13_server()
        .build()
        .await?;

    let elapsed = fixture
        .test_local_server_streaming("/bytes/2147483648", true, 300)
        .await?;

    let mb_per_sec = 2048.0 / elapsed.as_secs_f64();
    eprintln!(
        "[BENCHMARK] Shoes-to-Shoes REALITY Vision HTTPS 2GB transfer completed in {:.2}s ({:.2} MB/s)",
        elapsed.as_secs_f64(),
        mb_per_sec
    );
    Ok(())
}

/// 2GB streaming test for REALITY Vision over HTTPS with verification (shoes client -> shoes server)
#[tokio::test]
async fn test_reality_vision_https_shoes_client_shoes_server_2gb_verified()
-> Result<(), Box<dyn std::error::Error>> {
    let fixture = ProxyTestFixture::new()
        .with_shoes_reality_vision_client()
        .with_shoes_reality_vision_server()
        .with_local_https_tls13_server()
        .build()
        .await?;

    let elapsed = fixture
        .test_local_server_streaming_verified(2147483648, 600)
        .await?;

    let mb_per_sec = 2048.0 / elapsed.as_secs_f64();
    eprintln!(
        "[BENCHMARK] Shoes-to-Shoes REALITY Vision HTTPS 2GB verified transfer completed in {:.2}s ({:.2} MB/s)",
        elapsed.as_secs_f64(),
        mb_per_sec
    );
    Ok(())
}

/// 2GB streaming test for VLESS+h2mux (shoes client -> shoes server)
#[tokio::test]
async fn test_h2mux_vless_shoes_client_shoes_server_2gb() -> Result<(), Box<dyn std::error::Error>>
{
    let fixture = ProxyTestFixture::new()
        .with_shoes_h2mux_vless_client()
        .with_shoes_vless_tls_server()
        .with_local_http_server()
        .build()
        .await?;

    // 2GB = 2147483648 bytes, timeout 600 seconds
    let elapsed = fixture
        .test_local_server_streaming("/bytes/2147483648", false, 600)
        .await?;

    let mb_per_sec = 2048.0 / elapsed.as_secs_f64();
    eprintln!(
        "[BENCHMARK] H2MUX VLESS (shoes->shoes) 2GB transfer completed in {:.2}s ({:.2} MB/s)",
        elapsed.as_secs_f64(),
        mb_per_sec
    );
    Ok(())
}

/// 2GB streaming test for VLESS+h2mux with verification (shoes client -> shoes server)
#[tokio::test]
async fn test_h2mux_vless_shoes_client_shoes_server_2gb_verified()
-> Result<(), Box<dyn std::error::Error>> {
    let fixture = ProxyTestFixture::new()
        .with_shoes_h2mux_vless_client()
        .with_shoes_vless_tls_server()
        .with_local_http_server()
        .build()
        .await?;

    // 2GB = 2147483648 bytes, timeout 600 seconds
    let elapsed = fixture
        .test_local_server_streaming_verified(2147483648, 600)
        .await?;

    let mb_per_sec = 2048.0 / elapsed.as_secs_f64();
    eprintln!(
        "[BENCHMARK] H2MUX VLESS (shoes->shoes) 2GB verified transfer completed in {:.2}s ({:.2} MB/s)",
        elapsed.as_secs_f64(),
        mb_per_sec
    );
    Ok(())
}
