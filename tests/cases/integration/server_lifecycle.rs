//! Process-level regressions. All listeners and destinations are loopback.
use std::io::Write;
use std::path::PathBuf;
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use shoes_test_support::process::ProcessGuard;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};

const ID: &str = "550e8400-e29b-41d4-a716-446655440000";
const SECOND: &str = "550e8400-e29b-41d4-a716-446655440001";

fn binary() -> std::ffi::OsString {
    std::env::var_os("SHOES_TEST_SHOES_BIN").unwrap_or_else(|| env!("CARGO_BIN_EXE_shoes").into())
}

fn profile(port: u16, user: &str) -> String {
    format!("- address: '127.0.0.1:{port}'\n  protocol:\n    type: vless\n    user_id: '{user}'\n")
}

fn exits(config: &str, no_reload: bool, dry_run: bool) -> (std::process::ExitStatus, String) {
    let mut file = tempfile::NamedTempFile::new().unwrap();
    file.write_all(config.as_bytes()).unwrap();
    let log = tempfile::NamedTempFile::new().unwrap();
    let mut command = Command::new(binary());
    command
        .args(["--threads", "1"])
        .env("RUST_LOG", "off")
        .stdout(Stdio::null())
        .stderr(Stdio::from(log.reopen().unwrap()));
    if no_reload {
        command.arg("--no-reload");
    }
    if dry_run {
        command.arg("--dry-run");
    }
    let mut child = command.arg(file.path()).spawn().unwrap();
    let deadline = Instant::now() + Duration::from_secs(5);
    let status = loop {
        if let Some(status) = child.try_wait().unwrap() {
            break status;
        }
        if Instant::now() >= deadline {
            child.kill().unwrap();
            child.wait().unwrap();
            panic!("server failed to exit within deadline");
        }
        std::thread::sleep(Duration::from_millis(20));
    };
    (status, std::fs::read_to_string(log.path()).unwrap())
}

#[test]
fn dry_run_invalid_uuid_and_udp_transport_fail_without_panic() {
    for config in [
        profile(18443, "invalid"),
        profile(18443, ID).replace("  protocol:", "  transport: udp\n  protocol:"),
    ] {
        for dry_run in [false, true] {
            let (status, log) = exits(&config, true, dry_run);
            assert!(!status.success());
            assert!(!log.contains("panicked"));
        }
    }
    assert!(exits(&profile(18443, ID), false, true).0.success());
}

#[test]
fn occupied_port_exits_in_both_reload_modes_without_panic() {
    let occupied = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    for no_reload in [true, false] {
        let (status, log) = exits(
            &profile(occupied.local_addr().unwrap().port(), ID),
            no_reload,
            false,
        );
        assert!(!status.success());
        assert!(!log.contains("panicked"));
    }
}

#[test]
fn partial_startup_failure_releases_new_listeners() {
    let occupied = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let reservation = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let free_port = reservation.local_addr().unwrap().port();
    drop(reservation);
    let config = format!(
        "{}{}",
        profile(free_port, ID),
        profile(occupied.local_addr().unwrap().port(), SECOND)
    );
    assert!(!exits(&config, true, false).0.success());
    assert!(std::net::TcpListener::bind(("127.0.0.1", free_port)).is_ok());
}

#[cfg(unix)]
#[test]
fn existing_file_at_unix_socket_path_is_not_deleted() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("do-not-overwrite");
    std::fs::write(&path, b"keep").unwrap();
    let config = format!(
        "- path: '{}'\n  protocol:\n    type: vless\n    user_id: '{ID}'\n",
        path.display()
    );
    assert!(!exits(&config, true, false).0.success());
    assert_eq!(std::fs::read(path).unwrap(), b"keep");
}

#[test]
fn missing_config_is_a_reported_error_not_a_watcher_panic() {
    let directory = tempfile::tempdir().unwrap();
    let status = Command::new(binary())
        .arg(directory.path().join("missing.yaml"))
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .unwrap();
    assert_eq!(status.code(), Some(1));
}

struct Proxy {
    _guard: ProcessGuard,
    _directory: tempfile::TempDir,
    config: PathBuf,
    log: PathBuf,
    port: u16,
}

impl Proxy {
    async fn start() -> Self {
        Self::start_with_symlink(false).await
    }

    async fn start_with_symlink(symlink_mode: bool) -> Self {
        let directory = tempfile::tempdir().unwrap();
        let config = directory.path().join("server.yaml");
        let log = directory.path().join("server.log");
        let reservation = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let port = reservation.local_addr().unwrap().port();
        drop(reservation);
        std::fs::write(&config, profile(port, ID)).unwrap();
        #[cfg(unix)]
        if symlink_mode {
            let target_directory = directory.path().join("target-a");
            std::fs::create_dir(&target_directory).unwrap();
            let target = target_directory.join("server.yaml");
            std::fs::rename(&config, &target).unwrap();
            std::os::unix::fs::symlink(&target, &config).unwrap();
        }
        #[cfg(not(unix))]
        assert!(!symlink_mode, "symlink fixture requires Unix");
        let child = Command::new(binary())
            .args(["--threads", "1", "--log-file"])
            .arg(&log)
            .arg(&config)
            .env("RUST_LOG", "info")
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .unwrap();
        let proxy = Self {
            _guard: ProcessGuard::new(child, "lifecycle-shoes"),
            _directory: directory,
            config,
            log,
            port,
        };
        tokio::time::timeout(Duration::from_secs(5), async {
            loop {
                if TcpStream::connect(("127.0.0.1", port)).await.is_ok() {
                    break;
                }
                tokio::time::sleep(Duration::from_millis(20)).await;
            }
        })
        .await
        .unwrap();
        proxy
    }

    async fn replace(&self, config: &str, accepted: bool, atomic: bool) {
        std::fs::write(&self.log, "").unwrap();
        if atomic {
            let replacement = self.config.with_extension("new");
            std::fs::write(&replacement, config).unwrap();
            std::fs::rename(replacement, &self.config).unwrap();
        } else {
            std::fs::write(&self.config, config).unwrap();
        }
        self.wait_for_reload(accepted).await;
    }

    async fn wait_for_reload(&self, accepted: bool) {
        let marker = if accepted {
            "Configuration reloaded successfully"
        } else {
            "Reload rejected; current generation retained"
        };
        tokio::time::timeout(Duration::from_secs(8), async {
            loop {
                if std::fs::read_to_string(&self.log)
                    .unwrap_or_default()
                    .contains(marker)
                {
                    break;
                }
                tokio::time::sleep(Duration::from_millis(20)).await;
            }
        })
        .await
        .expect("reload result must be reported");
    }

    async fn connect(&self, destination: u16, id: &str) -> TcpStream {
        let mut stream = TcpStream::connect(("127.0.0.1", self.port)).await.unwrap();
        let mut request = vec![0];
        request.extend_from_slice(&shoes_test_support::vless::parse_uuid(id).unwrap());
        request.extend_from_slice(&[0, 1]);
        request.extend_from_slice(&destination.to_be_bytes());
        request.extend_from_slice(&[1, 127, 0, 0, 1]);
        stream.write_all(&request).await.unwrap();
        let mut response = [0; 2];
        tokio::time::timeout(Duration::from_secs(3), stream.read_exact(&mut response))
            .await
            .unwrap()
            .unwrap();
        assert_eq!(response, [0, 0]);
        stream
    }
}

#[cfg(unix)]
#[tokio::test]
async fn symlink_targets_and_atomic_retargeting_remain_watched() {
    let proxy = Proxy::start_with_symlink(true).await;
    let (port, echo_task) = echo().await;
    exchange(&mut proxy.connect(port, ID).await).await;

    // The first target is outside the configured path's directory.
    proxy
        .replace(&profile(proxy.port, "invalid"), false, false)
        .await;
    exchange(&mut proxy.connect(port, ID).await).await;
    proxy
        .replace(&profile(proxy.port, SECOND), true, false)
        .await;
    exchange(&mut proxy.connect(port, SECOND).await).await;

    let target_directory = proxy.config.parent().unwrap().join("target-b");
    std::fs::create_dir(&target_directory).unwrap();
    let target = target_directory.join("server.yaml");
    std::fs::write(&target, profile(proxy.port, ID)).unwrap();
    let replacement = proxy.config.with_extension("link");
    std::os::unix::fs::symlink(&target, &replacement).unwrap();
    std::fs::write(&proxy.log, "").unwrap();
    std::fs::rename(&replacement, &proxy.config).unwrap();
    proxy.wait_for_reload(true).await;
    exchange(&mut proxy.connect(port, ID).await).await;

    // A subsequent edit must be detected in the new target directory too.
    proxy
        .replace(&profile(proxy.port, SECOND), true, false)
        .await;
    exchange(&mut proxy.connect(port, SECOND).await).await;
    echo_task.abort();
}

async fn echo() -> (u16, tokio::task::JoinHandle<()>) {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    let task = tokio::spawn(async move {
        while let Ok((mut stream, _)) = listener.accept().await {
            tokio::spawn(async move {
                let mut buf = [0; 8192];
                while let Ok(n) = stream.read(&mut buf).await {
                    if n == 0 || stream.write_all(&buf[..n]).await.is_err() {
                        break;
                    }
                }
            });
        }
    });
    (port, task)
}

async fn exchange(stream: &mut TcpStream) {
    stream.write_all(b"local-regression").await.unwrap();
    let mut reply = [0; 16];
    tokio::time::timeout(Duration::from_secs(3), stream.read_exact(&mut reply))
        .await
        .unwrap()
        .unwrap();
    assert_eq!(&reply, b"local-regression");
}

#[tokio::test]
async fn rejected_reload_retains_listener_and_active_session_then_recovers() {
    let proxy = Proxy::start().await;
    let (port, echo_task) = echo().await;
    let mut active = proxy.connect(port, ID).await;
    let occupied = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let occupied_port = occupied.local_addr().unwrap().port();
    for invalid in [
        profile(proxy.port, "invalid"),
        "not: [valid".into(),
        format!(
            "{}{}",
            profile(proxy.port, SECOND),
            profile(occupied_port, SECOND)
        ),
    ] {
        proxy.replace(&invalid, false, false).await;
        exchange(&mut active).await;
        exchange(&mut proxy.connect(port, ID).await).await;
    }
    // Same socket, new handler; an already authenticated session keeps transferring.
    proxy
        .replace(&profile(proxy.port, SECOND), true, true)
        .await;
    exchange(&mut active).await;
    exchange(&mut proxy.connect(port, SECOND).await).await;
    // The parent-directory watch must also notice the next atomic replacement.
    proxy.replace(&profile(proxy.port, ID), true, true).await;
    exchange(&mut proxy.connect(port, ID).await).await;
    echo_task.abort();
}

#[tokio::test]
async fn duplicate_addresses_do_not_partially_start_or_replace_a_generation() {
    let proxy = Proxy::start().await;
    let duplicate = format!("{}{}", profile(proxy.port, ID), profile(proxy.port, SECOND));
    proxy.replace(&duplicate, false, false).await;
    let (port, echo_task) = echo().await;
    exchange(&mut proxy.connect(port, ID).await).await;
    echo_task.abort();
}

#[cfg(unix)]
#[tokio::test]
async fn failed_preparation_rolls_back_only_owned_unix_socket_paths() {
    let proxy = Proxy::start().await;
    let socket = proxy.config.with_extension("socket");
    let unix = format!(
        "- path: '{}'\n  protocol:\n    type: vless\n    user_id: '{ID}'\n",
        socket.display()
    );
    let occupied = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let rejected = format!(
        "{}{}{}",
        profile(proxy.port, ID),
        unix,
        profile(occupied.local_addr().unwrap().port(), SECOND)
    );
    proxy.replace(&rejected, false, false).await;
    assert!(
        !socket.exists(),
        "failed preparation must not leave a stale socket path"
    );
    proxy
        .replace(&format!("{}{}", profile(proxy.port, ID), unix), true, false)
        .await;
    assert!(tokio::net::UnixStream::connect(&socket).await.is_ok());
    proxy.replace(&profile(proxy.port, ID), true, false).await;
    assert!(
        !socket.exists(),
        "retiring a listener must release its own socket path"
    );
}

#[cfg(unix)]
#[tokio::test]
async fn signals_stop_listeners_drain_active_transfers_and_exit_zero() {
    struct Child(std::process::Child);
    impl Drop for Child {
        fn drop(&mut self) {
            let _ = self.0.kill();
            let _ = self.0.wait();
        }
    }

    // Cover both signals, runtime flavors, watcher modes and deadline expiry.
    for (signal, threads, no_reload, stalled, grace) in [
        (libc::SIGTERM, "1", true, false, 3),
        (libc::SIGINT, "2", false, false, 3),
        (libc::SIGTERM, "2", false, true, 1),
        (libc::SIGINT, "1", true, true, 0),
    ] {
        let directory = tempfile::tempdir().unwrap();
        let socket = directory.path().join("listener.socket");
        let config = directory.path().join("server.yaml");
        let log = directory.path().join("server.log");
        std::fs::write(
            &config,
            format!(
                "- global_limits: {{ reload_grace_secs: {grace} }}\n- path: '{}'\n  protocol:\n    type: vless\n    user_id: '{ID}'\n",
                socket.display()
            ),
        )
        .unwrap();
        let mut command = Command::new(binary());
        command
            .args(["--threads", threads, "--log-file"])
            .arg(&log)
            .env("RUST_LOG", "info")
            .stdout(Stdio::null())
            .stderr(Stdio::null());
        if no_reload {
            command.arg("--no-reload");
        }
        let mut child = Child(command.arg(&config).spawn().unwrap());
        let mut stream = tokio::time::timeout(Duration::from_secs(5), async {
            loop {
                if let Ok(stream) = tokio::net::UnixStream::connect(&socket).await {
                    break stream;
                }
                assert!(child.0.try_wait().unwrap().is_none());
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .unwrap();
        let (destination, echo_task) = echo().await;
        let mut request = vec![0];
        request.extend_from_slice(&shoes_test_support::vless::parse_uuid(ID).unwrap());
        request.extend_from_slice(&[0, 1]);
        request.extend_from_slice(&destination.to_be_bytes());
        request.extend_from_slice(&[1, 127, 0, 0, 1]);
        stream.write_all(&request).await.unwrap();
        let mut response = [0; 2];
        tokio::time::timeout(Duration::from_secs(3), stream.read_exact(&mut response))
            .await
            .unwrap()
            .unwrap();
        assert_eq!(response, [0, 0]);

        if !no_reload && !stalled {
            // The active connection now belongs to a retired generation. Signal
            // shutdown must wait for that drain worker, not only current listeners.
            let replacement = config.with_extension("new");
            std::fs::copy(&config, &replacement).unwrap();
            std::fs::rename(replacement, &config).unwrap();
            tokio::time::timeout(Duration::from_secs(5), async {
                while !std::fs::read_to_string(&log)
                    .unwrap_or_default()
                    .contains("Configuration reloaded successfully")
                {
                    tokio::time::sleep(Duration::from_millis(10)).await;
                }
            })
            .await
            .unwrap();
        }

        let started = Instant::now();
        // SAFETY: signal only our owned live child; its PID fits pid_t.
        assert_eq!(
            unsafe { libc::kill(child.0.id() as libc::pid_t, signal) },
            0
        );
        tokio::time::timeout(Duration::from_secs(2), async {
            while socket.exists() {
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .expect("shutdown must remove the owned listener before draining");
        assert!(tokio::net::UnixStream::connect(&socket).await.is_err());

        if !stalled {
            assert!(child.0.try_wait().unwrap().is_none());
            // Transfer after listener removal proves accepted work is still alive.
            let data = vec![0x5a; 128 * 1024];
            let (mut reader, mut writer) = stream.split();
            let send = async {
                writer.write_all(&data).await.unwrap();
                writer.shutdown().await.unwrap();
            };
            let receive = async {
                let mut reply = Vec::new();
                reader.read_to_end(&mut reply).await.unwrap();
                assert_eq!(reply, data);
            };
            tokio::time::timeout(Duration::from_secs(2), async {
                tokio::join!(send, receive);
            })
            .await
            .unwrap();
            drop(stream);
        }
        let status = tokio::time::timeout(Duration::from_secs(5), async {
            loop {
                if let Some(status) = child.0.try_wait().unwrap() {
                    break status;
                }
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .expect("signal shutdown must be bounded");
        assert_eq!(status.code(), Some(0));
        if stalled && grace > 0 {
            assert!(started.elapsed() >= Duration::from_secs(grace));
            assert!(
                std::fs::read_to_string(&log)
                    .unwrap()
                    .contains("grace expired")
            );
        }
        assert!(!socket.exists());
        echo_task.abort();
    }
}
