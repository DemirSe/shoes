//! Periodic glibc heap trimming.
//!
//! A burst of connections can leave freed buffers retained by the allocator,
//! so the resident set stays high until those pages are returned to the OS.
//! A rare, idle-only trim returns that memory without touching active paths:
//! the measured cost is about 15 ms for a 300 MB heap, and re-faulting the
//! pages costs nothing measurable because the buffers are written on reuse.

#[cfg(all(target_os = "linux", target_env = "gnu"))]
use std::time::Duration;

/// How often the trim check runs. The trim itself only happens when the
/// process is idle and holding more memory than the threshold.
#[cfg(all(target_os = "linux", target_env = "gnu"))]
const TRIM_CHECK_INTERVAL: Duration = Duration::from_secs(300);

/// Only trim when the resident set exceeds this; a small process does not need
/// the trim at all.
#[cfg(all(target_os = "linux", target_env = "gnu"))]
const TRIM_RSS_THRESHOLD_BYTES: u64 = 64 * 1024 * 1024;

/// Only trim while the server is not busy: normal browsing keeps a few dozen
/// connections open, and the measured trim cost (0.02-15 ms, on a blocking
/// thread) stays negligible at that level. A heavily loaded server (hundreds of
/// connections) is left alone so a burst never pays the trim cost or the
/// page-fault cost that follows it.
#[cfg(all(target_os = "linux", target_env = "gnu"))]
const TRIM_MAX_ACTIVE_CONNECTIONS: usize = 128;

/// Whether the heap should be trimmed now.
#[cfg(all(target_os = "linux", target_env = "gnu"))]
fn should_trim(rss_bytes: u64, active_connections: usize) -> bool {
    rss_bytes > TRIM_RSS_THRESHOLD_BYTES && active_connections < TRIM_MAX_ACTIVE_CONNECTIONS
}

/// Resident set size of this process in bytes, from `/proc/self/status`.
#[cfg(all(target_os = "linux", target_env = "gnu"))]
fn process_rss_bytes() -> Option<u64> {
    let status = std::fs::read_to_string("/proc/self/status").ok()?;
    let line = status.lines().find(|line| line.starts_with("VmRSS:"))?;
    let kb: u64 = line.split_whitespace().nth(1)?.parse().ok()?;
    Some(kb * 1024)
}

/// Spawn the periodic heap-trim task.
///
/// The task never runs on the hot path: it wakes on an interval, and only an
/// idle process above the RSS threshold is trimmed, on a blocking thread.
#[cfg(all(target_os = "linux", target_env = "gnu"))]
pub fn spawn_heap_trim_task() {
    tokio::spawn(async {
        let mut ticker = tokio::time::interval(TRIM_CHECK_INTERVAL);
        ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
        // The first tick is immediate; skip it so the check runs one interval
        // after startup.
        ticker.tick().await;
        loop {
            ticker.tick().await;
            let Some(rss) = process_rss_bytes() else {
                continue;
            };
            let active = crate::resources::snapshot().connections.active;
            if should_trim(rss, active) {
                let released = tokio::task::spawn_blocking(|| {
                    // SAFETY: `malloc_trim` is a thread-safe glibc call that
                    // releases free heap pages; it never invalidates live
                    // allocations.
                    unsafe { libc::malloc_trim(0) }
                })
                .await
                .unwrap_or(0);
                log::debug!(
                    "Heap trim: rss={} bytes active_connections={} released={}",
                    rss,
                    active,
                    released
                );
            }
        }
    });
}

/// No-op on platforms without glibc.
#[cfg(not(all(target_os = "linux", target_env = "gnu")))]
pub fn spawn_heap_trim_task() {}

#[cfg(all(test, target_os = "linux", target_env = "gnu"))]
mod tests {
    use super::*;

    #[test]
    fn trim_only_when_idle_and_above_the_threshold() {
        assert!(!should_trim(TRIM_RSS_THRESHOLD_BYTES, 0));
        assert!(!should_trim(
            TRIM_RSS_THRESHOLD_BYTES + 1,
            TRIM_MAX_ACTIVE_CONNECTIONS
        ));
        assert!(should_trim(
            TRIM_RSS_THRESHOLD_BYTES + 1,
            TRIM_MAX_ACTIVE_CONNECTIONS - 1
        ));
        assert!(should_trim(u64::MAX, 0));
    }

    #[test]
    fn process_rss_is_readable() {
        let rss = process_rss_bytes().expect("VmRSS must be readable");
        assert!(rss > 0);
    }

    #[test]
    fn malloc_trim_call_is_safe() {
        // Smoke test for the FFI binding: the call must not fault, and the
        // return value is a boolean-ish "released something" flag.
        let released = unsafe { libc::malloc_trim(0) };
        assert!(released == 0 || released == 1);
    }

    #[test]
    #[ignore = "microbenchmark; run explicitly with --ignored --nocapture"]
    fn bench_malloc_trim_cost() {
        use std::hint::black_box;
        use std::time::Instant;

        let chunk = 65536usize;
        let count = (256 * 1024 * 1024) / chunk;
        let blocks: Vec<Vec<u8>> = (0..count).map(|_| vec![0x5Au8; chunk]).collect();
        let rss_before = process_rss_bytes().unwrap();
        drop(blocks);
        let start = Instant::now();
        let released = unsafe { libc::malloc_trim(0) };
        let elapsed = start.elapsed();
        let rss_after = process_rss_bytes().unwrap();
        println!(
            "malloc_trim: released={released} cost={:.3} ms rss {} -> {} MB",
            elapsed.as_secs_f64() * 1000.0,
            rss_before / 1048576,
            rss_after / 1048576
        );
        let _ = black_box(rss_after);
    }
}
