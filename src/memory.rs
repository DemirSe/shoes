//! Periodic glibc heap trimming.
//!
//! This only attempts to return free pages owned by glibc to the OS. The Linux
//! CLI uses jemalloc for Rust allocations, which `malloc_trim` cannot purge;
//! native dependencies may still allocate through glibc. Process RSS includes
//! both allocators and is only a trigger, not a measure of reclaimable memory.
//! A successful call does not establish which workload caused the allocation.

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

/// Avoid trimming above this connection count. The snapshot is a heuristic:
/// fewer connections do not prove low traffic, and activity can change before
/// the blocking task runs. This is not a latency or page-fault guarantee.
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
/// The task wakes on an interval and attempts glibc-only reclamation on a
/// blocking thread when the RSS and connection-count heuristics permit it.
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
                    // releases free glibc heap pages; it never invalidates live
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

        // Vec uses the enclosing target's global allocator: glibc in the library
        // test executable, jemalloc in the Linux CLI test executable. Results
        // from one target must not be attributed to the other allocator.
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
