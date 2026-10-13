mod address;
mod async_stream;
mod buf_reader;
mod client_proxy_chain;
mod client_proxy_selector;
mod config;
mod copy_bidirectional;
mod copy_bidirectional_message;
mod crypto;
mod dns;
mod h2mux;
#[cfg(target_os = "linux")]
mod linux_splice;
mod listener_tasks;
mod logging;
mod memory;
mod option_util;
mod prepend_stream;
mod quic_endpoint;
mod reality;
mod reality_client_handler;
mod resolver;
mod resources;
mod routing;
mod rustls_config_util;
mod rustls_connection_util;
mod server_runtime;
mod slide_buffer;
mod socket_util;
mod stream_reader;
mod sync_adapter;
mod tcp;
mod thread_util;
mod tls_client_handler;
mod tls_hello;
mod tls_server_handler;
#[cfg(unix)]
mod tun;
mod uot;
mod util;
mod uuid_util;
mod vless;
mod xudp;

#[cfg(not(any(target_env = "msvc", target_os = "ios", target_os = "android")))]
use tikv_jemallocator::Jemalloc;

#[cfg(not(any(target_env = "msvc", target_os = "ios", target_os = "android")))]
#[global_allocator]
static GLOBAL: Jemalloc = Jemalloc;

use std::path::Path;

use log::debug;
use notify::{Event, EventKind, RecommendedWatcher, RecursiveMode, Watcher};
use tokio::runtime::Builder;
use tokio::sync::mpsc::{Receiver, channel};

use crate::reality::generate_keypair;
use crate::thread_util::set_num_threads;

#[derive(Debug)]
struct ConfigChanged;

fn start_notify_thread(
    config_paths: Vec<String>,
) -> std::io::Result<(RecommendedWatcher, Receiver<ConfigChanged>)> {
    let (tx, rx) = channel(1);
    let cwd = std::env::current_dir()?;
    let mut paths: Vec<_> = config_paths
        .iter()
        .map(|p| {
            let path = Path::new(p);
            if path.is_absolute() {
                path.to_path_buf()
            } else {
                cwd.join(path)
            }
        })
        .collect();
    // Keep the configured path to notice an atomic symlink replacement, while
    // also watching an existing target in a different directory.
    for path in paths.clone() {
        if let Ok(target) = std::fs::canonicalize(path)
            && !paths.contains(&target)
        {
            paths.push(target);
        }
    }
    let watched_paths = paths.clone();

    let mut watcher = notify::recommended_watcher(move |res: notify::Result<Event>| match res {
        Ok(event) => {
            if matches!(
                event.kind,
                EventKind::Modify(..) | EventKind::Create(..) | EventKind::Remove(..)
            ) && event.paths.iter().any(|path| watched_paths.contains(path))
            {
                let _ = tx.try_send(ConfigChanged {});
            }
        }
        Err(e) => log::error!("Configuration watch error: {e}"),
    })
    .map_err(std::io::Error::other)?;

    let mut parents = std::collections::HashSet::new();
    for config_path in paths {
        let parent = config_path
            .parent()
            .ok_or_else(|| std::io::Error::other("config has no parent directory"))?;
        if !parents.insert(parent.to_path_buf()) {
            continue;
        }
        watcher
            .watch(parent, RecursiveMode::NonRecursive)
            .map_err(std::io::Error::other)?;
    }

    Ok((watcher, rx))
}

fn print_usage_and_exit(arg0: String) {
    eprintln!("{arg0} [OPTIONS] <config.yaml> [config.yaml...]");
    eprintln!();
    eprintln!("OPTIONS:");
    eprintln!("    -t, --threads NUM    Set the number of worker threads (default: CPU count)");
    eprintln!(
        "    -l, --log-file PATH  Log to file (repeatable; \"-\" means stderr; default: stderr)"
    );
    eprintln!("    -d, --dry-run        Parse the config and exit");
    eprintln!("    --no-reload          Disable automatic config reloading on file changes");
    eprintln!(
        "    --production-profile Enforce the public IPv4:443 whatsapp.net REALITY/Vision profile"
    );
    eprintln!(
        "    global_limits.reload_grace_secs sets the TCP reload drain deadline (default: 300)"
    );
    eprintln!("    -V, --version        Print version information and exit");
    eprintln!();
    eprintln!("RESOURCE LIMITS:");
    eprintln!("    Optional YAML entry: - global_limits: {{ max_connections: 1024 }}");
    eprintln!(
        "    Admission is unlimited unless configured. Only one global_limits entry is allowed."
    );
    eprintln!();
    eprintln!("COMMANDS:");
    eprintln!(
        "    generate-reality-keypair                       Generate a new Reality X25519 keypair"
    );
    eprintln!(
        "    generate-vless-user-id                         Generate a random VLESS user ID (UUID v4)"
    );
    std::process::exit(1);
}

fn main() {
    let mut args: Vec<String> = std::env::args().collect();
    let arg0 = args.remove(0);
    let mut num_threads = 0usize;
    let mut dry_run = false;
    let mut no_reload = false;
    let mut production_profile = false;
    let mut log_files: Vec<String> = Vec::new();

    while !args.is_empty() && args[0].starts_with("-") {
        if args[0] == "--threads" || args[0] == "-t" {
            args.remove(0);
            if args.is_empty() {
                eprintln!("Missing threads argument.");
                print_usage_and_exit(arg0);
                return;
            }
            num_threads = match args.remove(0).parse::<usize>() {
                Ok(n) => n,
                Err(e) => {
                    eprintln!("Invalid thread count: {e}");
                    print_usage_and_exit(arg0);
                    return;
                }
            };
        } else if args[0] == "--log-file" || args[0] == "-l" {
            args.remove(0);
            if args.is_empty() {
                eprintln!("Missing log-file argument.");
                print_usage_and_exit(arg0);
                return;
            }
            log_files.push(args.remove(0));
        } else if args[0] == "--dry-run" || args[0] == "-d" {
            args.remove(0);
            dry_run = true;
        } else if args[0] == "--no-reload" {
            args.remove(0);
            no_reload = true;
        } else if args[0] == "--production-profile" {
            args.remove(0);
            production_profile = true;
        } else if args[0] == "--version" || args[0] == "-V" {
            println!("shoes {}", env!("CARGO_PKG_VERSION"));
            return;
        } else {
            eprintln!("Invalid argument: {}", args[0]);
            print_usage_and_exit(arg0);
            return;
        }
    }

    let directives = logging::resolve_directives();
    let mut writers: Vec<Box<dyn logging::LogWriter>> = Vec::new();

    if log_files.is_empty() || log_files.iter().any(|p| p == "-") {
        writers.push(Box::new(logging::StderrWriter));
    }
    for path in &log_files {
        if path == "-" {
            continue;
        }
        match logging::FileLogWriter::new(path) {
            Ok(w) => writers.push(Box::new(w)),
            Err(e) => {
                eprintln!("Failed to open log file {path}: {e}");
                std::process::exit(1);
            }
        }
    }

    logging::init_multi_logger(writers, directives);

    if args.iter().any(|s| s == "generate-reality-keypair") {
        let (private_key, public_key) = generate_keypair().unwrap();
        println!(
            "--------------------------------------------------------------------------------"
        );
        println!("REALITY private key: {}", private_key);
        println!("REALITY public key: {}", public_key);
        println!(
            "--------------------------------------------------------------------------------"
        );
        return;
    }

    if args.iter().any(|s| s == "generate-vless-user-id") {
        let uuid = uuid_util::generate_uuid();
        println!(
            "--------------------------------------------------------------------------------"
        );
        println!("VLESS User ID: {}", uuid);
        println!(
            "--------------------------------------------------------------------------------"
        );
        return;
    }

    if args.is_empty() {
        println!("No config specified, assuming loading from file config.shoes.yaml");
        args.push("config.shoes.yaml".to_string())
    }

    if dry_run {
        println!("Starting dry run.");
    }

    if num_threads == 0 {
        num_threads = std::cmp::max(
            2,
            std::thread::available_parallelism()
                .map(|n| n.get())
                .unwrap_or(1),
        );
        debug!("Runtime threads: {num_threads}");
    } else {
        println!("Using custom thread count ({num_threads})");
    }

    // Shared with the library so DNS and socket helpers agree on the thread budget.
    set_num_threads(num_threads);

    let mut builder = if num_threads == 1 {
        Builder::new_current_thread()
    } else {
        let mut mt = Builder::new_multi_thread();
        mt.worker_threads(num_threads);
        mt
    };

    let runtime = builder
        .enable_io()
        .enable_time()
        .build()
        .expect("Could not build tokio runtime");

    if let Err(error) = runtime.block_on(server_runtime::run(
        args,
        dry_run,
        no_reload,
        production_profile,
    )) {
        eprintln!("Server configuration/startup failed: {error}");
        std::process::exit(1);
    }
}
