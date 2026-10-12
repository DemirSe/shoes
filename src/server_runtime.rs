//! Prepare a complete replacement before stopping the current accept loops.
use std::io;
use std::sync::Arc;

use futures::stream::{FuturesUnordered, StreamExt};
use tokio::task::JoinHandle;

use crate::config::{self, Config, GlobalLimits, TunConfig, ValidatedConfigs};
use crate::resolver::Resolver;
use crate::tcp::tcp_server::{
    ListenerRegistry, PreparedTcpServers, prepare_tcp_servers, start_servers,
};

type Tasks = FuturesUnordered<JoinHandle<()>>;

async fn load(paths: &Vec<String>, production: bool) -> io::Result<ValidatedConfigs> {
    let configs = config::load_configs(paths).await?;
    let (configs, _) = config::convert_cert_paths(configs).await?;
    let validated = config::create_server_configs(configs)?;
    if production {
        config::validate_production_profile(&validated)?;
    }
    Ok(validated)
}

struct PreparedGeneration {
    registry: ListenerRegistry,
    tcp: Vec<PreparedTcpServers>,
    tun: Vec<(TunConfig, Arc<dyn Resolver>)>,
    limits: GlobalLimits,
}

impl PreparedGeneration {
    async fn prepare(
        configs: ValidatedConfigs,
        previous: &ListenerRegistry,
        reload: bool,
        previous_has_tun: bool,
    ) -> io::Result<Self> {
        if reload
            && (previous_has_tun
                || configs
                    .configs
                    .iter()
                    .any(|c| matches!(c, Config::TunServer(_))))
        {
            return Err(io::Error::new(
                io::ErrorKind::Unsupported,
                "TUN live replacement is not supported; current generation retained",
            ));
        }
        let mut dns = crate::dns::build_dns_registry(configs.dns_groups).await?;
        let mut next = Self {
            registry: ListenerRegistry::default(),
            tcp: Vec::new(),
            tun: Vec::new(),
            limits: configs.global_limits,
        };
        for config in configs.configs {
            match config {
                Config::Server(server) => {
                    let resolver = dns.get_for_server(server.dns.as_ref());
                    next.tcp.push(prepare_tcp_servers(
                        server,
                        resolver,
                        previous,
                        &mut next.registry,
                    )?);
                }
                Config::TunServer(tun) => {
                    let resolver = dns.get_for_server(tun.dns.as_ref());
                    next.tun.push((tun, resolver));
                }
                _ => unreachable!("validated listener configs"),
            }
        }
        if next.tcp.is_empty() && next.tun.is_empty() {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "no listeners configured",
            ));
        }
        Ok(next)
    }

    async fn start(self) -> io::Result<(ListenerRegistry, Tasks, bool)> {
        let has_tun = !self.tun.is_empty();
        let mut tasks = Tasks::new();
        for (tun, resolver) in self.tun {
            match start_servers(Config::TunServer(tun), resolver).await {
                Ok(handles) => tasks.extend(handles),
                Err(error) => {
                    stop(&mut tasks).await;
                    return Err(error);
                }
            }
        }
        for tcp in self.tcp {
            tasks.extend(tcp.start());
        }
        Ok((self.registry, tasks, has_tun))
    }
}

async fn stop(tasks: &mut Tasks) {
    for handle in tasks.iter() {
        handle.abort();
    }
    while tasks.next().await.is_some() {}
}

fn listener_exit_error(ended: Option<Result<(), tokio::task::JoinError>>) -> io::Error {
    io::Error::other(match ended {
        Some(Err(error)) if error.is_panic() => "listener task panicked",
        Some(Err(_)) => "listener task was cancelled",
        _ => "listener task stopped unexpectedly",
    })
}

pub async fn run(
    paths: Vec<String>,
    dry_run: bool,
    no_reload: bool,
    production: bool,
) -> io::Result<()> {
    let validated = load(&paths, production).await?;
    if dry_run {
        println!("Finishing dry run, config parsed successfully.");
        return Ok(());
    }
    let prepared =
        PreparedGeneration::prepare(validated, &ListenerRegistry::default(), false, false).await?;
    let mut watcher = if no_reload {
        None
    } else {
        Some(crate::start_notify_thread(paths.clone())?)
    };
    crate::resources::configure(prepared.limits)?;
    let _reporter = crate::resources::ResourceReporter::start();
    let (mut registry, mut tasks, mut has_tun) = prepared.start().await?;
    let mut reload_pending = false;
    loop {
        tokio::select! {
            ended = tasks.next() => {
                stop(&mut tasks).await;
                return Err(listener_exit_error(ended));
            }
            event = async {
                if reload_pending { return Some(crate::ConfigChanged); }
                match watcher.as_mut() {
                    Some((_, rx)) => rx.recv().await,
                    None => std::future::pending().await,
                }
            } => {
                reload_pending = false;
                if event.is_none() {
                    stop(&mut tasks).await;
                    return Err(io::Error::other("configuration watcher stopped"));
                }
                // Keep current listeners serving throughout debounce and preparation.
                tokio::time::sleep(std::time::Duration::from_millis(100)).await;
                if let Some((_, rx)) = watcher.as_mut() { while rx.try_recv().is_ok() {} }
                let replacement = match load(&paths, production).await {
                    Ok(validated) => PreparedGeneration::prepare(validated, &registry, true, has_tun).await,
                    Err(error) => Err(error),
                };
                match replacement {
                    Err(error) => log::error!("Reload rejected; current generation retained: {error}"),
                    Ok(next) => {
                        // Retargeted symlinks may point into a different directory.
                        // Prepare the new watch before retiring the current one.
                        let next_watcher = if watcher.is_some() {
                            match crate::start_notify_thread(paths.clone()) {
                                Ok(next_watcher) => Some(next_watcher),
                                Err(error) => {
                                    log::error!("Reload rejected; current generation retained: cannot refresh configuration watch: {error}");
                                    continue;
                                }
                            }
                        } else { None };
                        // All TCP binds succeeded. Unchanged addresses reuse live sockets.
                        crate::resources::configure(next.limits)?;
                        stop(&mut tasks).await;
                        (registry, tasks, has_tun) = next.start().await?;
                        // Do not discard a change queued while preparing the replacement.
                        // The new watcher may not have existed when that edit occurred.
                        reload_pending = watcher.as_mut().is_some_and(|(_, rx)| rx.try_recv().is_ok());
                        watcher = next_watcher;
                        log::info!("Configuration reloaded successfully");
                    }
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn listener_failures_distinguish_panics_cancellation_and_normal_exit() {
        let panicked = tokio::spawn(async { panic!("synthetic listener failure") }).await;
        assert_eq!(
            listener_exit_error(Some(panicked)).to_string(),
            "listener task panicked"
        );
        let cancelled = tokio::spawn(std::future::pending::<()>());
        cancelled.abort();
        assert_eq!(
            listener_exit_error(Some(cancelled.await)).to_string(),
            "listener task was cancelled"
        );
        assert_eq!(
            listener_exit_error(Some(Ok(()))).to_string(),
            "listener task stopped unexpectedly"
        );
        assert_eq!(
            listener_exit_error(None).to_string(),
            "listener task stopped unexpectedly"
        );
    }
}
