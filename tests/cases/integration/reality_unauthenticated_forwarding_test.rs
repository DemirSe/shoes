//! Wire regressions for unauthenticated REALITY forwarding.
//!
//! A valid TLS client whose ClientHello cannot carry a REALITY auth envelope is
//! unauthenticated traffic. These tests drive the real shoes process and a real
//! local TLS 1.3 cover, and prove bidirectional application forwarding:
//!
//! * an empty legacy session id,
//! * a ClientHello that offers only a valid P-256 key share,
//! * a silent cover, which must stop the connection instead of forwarding.
//!
//! The client is a pinned local Python TLS client (`helpers/unauthenticated_tls_client.py`)
//! that shapes the ClientHello with OpenSSL. It runs offline against loopback
//! addresses only.

use std::error::Error;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};
use std::time::Duration;

use shoes_test_support as common;

use common::port_helper::PortHelper;
use common::process::ProcessGuard;
use common::test_fixture::start_shoes_server;
use common::test_servers::{TlsVersion, start_tls_stream_echo_server};
use tempfile::NamedTempFile;
use tokio::net::TcpListener;
use tokio::time::Instant;

type TestResult<T = ()> = Result<T, Box<dyn Error>>;

/// A fixed REALITY private key for the fixture; the client never authenticates.
const FIXTURE_PRIVATE_KEY: &str = "AQEBAQEBAQEBAQEBAQEBAQEBAQEBAQEBAQEBAQEBAQE";

fn helper_script() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests/cases/integration/helpers/unauthenticated_tls_client.py")
}

/// Write a certificate with many subject names, so the cover's first encrypted
/// record stays above the handler's 512-byte combined-mode threshold.
fn write_large_cover_cert() -> TestResult<(NamedTempFile, NamedTempFile)> {
    let cert = rcgen::generate_simple_self_signed(
        (0..100)
            .map(|index| format!("host-{index}.example.test"))
            .collect::<Vec<_>>(),
    )?;
    let mut cert_file = NamedTempFile::new()?;
    std::io::Write::write_all(&mut cert_file, cert.cert.pem().as_bytes())?;
    let mut key_file = NamedTempFile::new()?;
    std::io::Write::write_all(&mut key_file, cert.signing_key.serialize_pem().as_bytes())?;
    Ok((cert_file, key_file))
}

/// Start a REALITY listener whose cover is the given address.
async fn start_reality_server(cover_port: u16) -> TestResult<(ProcessGuard, NamedTempFile, u16)> {
    let mut ports = PortHelper::new();
    // The Python helper connects to 127.0.0.1, so the listener uses that address.
    let (ip, port) = ports.get_localhost_listener_port();
    let config = format!(
        r#"
- address: "{ip}:{port}"
  protocol:
    type: tls
    reality_targets:
      localhost:
        private_key: "{FIXTURE_PRIVATE_KEY}"
        short_ids: ["0123456789abcdef"]
        dest: "localhost:{cover_port}"
        vision: false
        protocol:
          type: vless
          user_id: "550e8400-e29b-41d4-a716-446655440000"
"#
    );
    let (guard, config_file) = start_shoes_server(&config)?;
    ports.wait_for_all_ports().await?;
    Ok((guard, config_file, port))
}

fn run_client(port: u16, mode: &str) -> std::io::Result<Output> {
    Command::new("python3")
        .arg(helper_script())
        .args(["--port", &port.to_string()])
        .args(["--mode", mode])
        .args(["--payload", "reality-forwarding"])
        .args(["--timeout", "8"])
        .output()
}

fn assert_forwarded(output: &Output, mode: &str) {
    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        output.status.success(),
        "{mode}: the client did not complete a forwarded handshake (status {:?})\nstdout: {stdout}\nstderr: {stderr}",
        output.status.code()
    );
    assert!(
        stdout.contains("OK reality-forwarding"),
        "{mode}: unexpected client output: {stdout}"
    );
}

#[tokio::test]
async fn an_empty_session_id_client_is_forwarded_to_the_cover() -> TestResult {
    let (cert_file, key_file) = write_large_cover_cert()?;
    let cover = start_tls_stream_echo_server(
        "127.0.0.1",
        0,
        cert_file.path(),
        key_file.path(),
        TlsVersion::Tls13Only,
    )
    .await?;
    let (_guard, _config, port) = start_reality_server(cover.local_addr().port()).await?;

    let output =
        tokio::task::spawn_blocking(move || run_client(port, "empty-session-id")).await??;
    assert_forwarded(&output, "empty-session-id");
    Ok(())
}

#[tokio::test]
async fn a_p256_only_client_is_forwarded_to_the_cover() -> TestResult {
    let (cert_file, key_file) = write_large_cover_cert()?;
    let cover = start_tls_stream_echo_server(
        "127.0.0.1",
        0,
        cert_file.path(),
        key_file.path(),
        TlsVersion::Tls13Only,
    )
    .await?;
    let (_guard, _config, port) = start_reality_server(cover.local_addr().port()).await?;

    let output = tokio::task::spawn_blocking(move || run_client(port, "p256-only")).await??;
    assert_forwarded(&output, "p256-only");
    Ok(())
}

/// Run one scenario against a silent cover (a listener that accepts and never
/// answers).
///
/// The scenario, including its preparation step, runs in its own task, so every
/// outcome - preparation failure, body failure or assertion failure - still
/// reaches the awaited helper cleanup. The helper is always cancelled and
/// awaited; an unexpected helper panic or I/O error is propagated separately
/// from the expected cancellation.
async fn with_silent_cover<F, Fut>(scenario: F) -> TestResult
where
    F: FnOnce(u16) -> Fut + Send + 'static,
    Fut: std::future::Future<Output = Result<(), String>> + Send + 'static,
{
    with_silent_cover_inner(scenario, None).await
}

/// Run the same scenario with an injected helper I/O error, so the propagation
/// branch is exercised after the awaited cleanup.
async fn with_silent_cover_injecting_helper_error<F, Fut>(scenario: F) -> TestResult
where
    F: FnOnce(u16) -> Fut + Send + 'static,
    Fut: std::future::Future<Output = Result<(), String>> + Send + 'static,
{
    with_silent_cover_inner(
        scenario,
        Some(std::io::Error::other("injected helper I/O error")),
    )
    .await
}

async fn with_silent_cover_inner<F, Fut>(
    scenario: F,
    injected: Option<std::io::Error>,
) -> TestResult
where
    F: FnOnce(u16) -> Fut + Send + 'static,
    Fut: std::future::Future<Output = Result<(), String>> + Send + 'static,
{
    let silent = TcpListener::bind("127.0.0.1:0").await?;
    let silent_port = silent.local_addr()?.port();
    let silent_task = tokio::spawn(async move {
        if let Some(error) = injected {
            return Err(error);
        }
        let mut held = Vec::new();
        loop {
            match silent.accept().await {
                Ok((socket, _)) => held.push(socket),
                Err(error) if error.kind() == std::io::ErrorKind::Interrupted => continue,
                // A real accept error is an unexpected helper failure: it must
                // reach the test instead of looking like a clean end.
                Err(error) => return Err(error),
            }
        }
    });

    let outcome = tokio::spawn(scenario(silent_port)).await;

    // Always cancel and await the owned helper before propagating anything.
    silent_task.abort();
    let helper = silent_task.await;

    let scenario_outcome = match outcome {
        Ok(result) => result,
        Err(error) => panic!("the silent-cover scenario failed: {error}"),
    };
    match helper {
        Ok(Ok(())) => {}
        Ok(Err(error)) => return Err(error.into()),
        Err(error) if error.is_cancelled() => {}
        Err(error) => panic!("the silent cover helper failed: {error}"),
    }
    scenario_outcome.map_err(|message| message.into())
}

#[tokio::test]
async fn a_silent_cover_stops_the_unauthenticated_connection() -> TestResult {
    with_silent_cover(|silent_port| async move {
        let (_guard, _config, port) = start_reality_server(silent_port)
            .await
            .map_err(|error| error.to_string())?;

        let started = Instant::now();
        let output = tokio::task::spawn_blocking(move || run_client(port, "empty-session-id"))
            .await
            .map_err(|error| error.to_string())?
            .map_err(|error| error.to_string())?;
        let elapsed = started.elapsed();

        let stdout = String::from_utf8_lossy(&output.stdout);
        assert!(
            !output.status.success(),
            "a silent cover must stop the connection instead of fabricating a response, got: {stdout}"
        );
        assert!(
            stdout.contains("FAILED"),
            "the client must report the failed handshake: {stdout}"
        );
        // The proxy stops the connection on its own deadline, before the client's
        // own timeout expires.
        assert!(
            elapsed < Duration::from_secs(7),
            "the proxy kept the connection open for {elapsed:?}"
        );
        Ok(())
    })
    .await
}

#[tokio::test]
async fn an_injected_helper_error_is_propagated_after_cleanup() -> TestResult {
    // The helper fails with an I/O error while the scenario succeeds. The fixture
    // must cancel and await the helper, then propagate that error instead of
    // reporting success.
    let result =
        with_silent_cover_injecting_helper_error(
            |_silent_port| async move { Ok::<(), String>(()) },
        )
        .await;

    let error = result.expect_err("the injected helper error must be propagated");
    assert!(
        error.to_string().contains("injected helper I/O error"),
        "unexpected error: {error}"
    );
    Ok(())
}

#[tokio::test]
async fn an_injected_preparation_failure_is_cleaned_up_and_reported() -> TestResult {
    // The preparation step fails before any client runs. The injected error
    // stands for any startup or readiness error of the same step, and the helper
    // must still be cancelled and awaited before the failure is propagated.
    let result = with_silent_cover(|_silent_port| async move {
        Err::<(), String>("injected preparation failure".into())
    })
    .await;

    let error = result.expect_err("the injected preparation failure must be reported");
    assert!(
        error.to_string().contains("injected preparation failure"),
        "unexpected error: {error}"
    );
    Ok(())
}
