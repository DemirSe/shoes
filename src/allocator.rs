//! GNU/Linux CLI-only defaults for the already-linked jemalloc allocator.
//!
//! Configure once after blocking shutdown signals and before starting Tokio.
//! No allocation, packet or connection path calls these controls. A nonempty
//! `_RJEM_MALLOC_CONF` remains an explicit override of the built-in defaults.
use std::ffi::{CStr, CString, c_char, c_int, c_void};
use std::io;

const DIRTY_DECAY_MS: isize = 60_000;
const MUZZY_DECAY_MS: isize = 0;

unsafe extern "C" {
    // Public C control API, with the prefix used by the locked tikv-jemallocator
    // build. The CLI already links this allocator; no second allocator is added.
    #[link_name = "_rjem_mallctl"]
    fn mallctl(
        name: *const c_char,
        old: *mut c_void,
        old_len: *mut usize,
        new: *mut c_void,
        new_len: usize,
    ) -> c_int;
}

fn control_error(name: &CStr, error: c_int) -> io::Error {
    let error = io::Error::from_raw_os_error(error);
    io::Error::new(
        error.kind(),
        format!("jemalloc {}: {error}", name.to_string_lossy()),
    )
}

// Private callers must match each control's documented C value type:
// bool, ssize_t (isize), unsigned (u32), or uint64_t (u64).
fn read<T: Default>(name: &CStr) -> io::Result<T> {
    let mut value = T::default();
    let mut length = std::mem::size_of::<T>();
    // SAFETY: callers use the control's C type; output is initialized, aligned
    // and writable. A null new pointer requests no mutation.
    let error = unsafe {
        mallctl(
            name.as_ptr(),
            (&mut value as *mut T).cast(),
            &mut length,
            std::ptr::null_mut(),
            0,
        )
    };
    if error != 0 {
        return Err(control_error(name, error));
    }
    if length != std::mem::size_of::<T>() {
        return Err(io::Error::other("unexpected jemalloc control value size"));
    }
    Ok(value)
}

fn write<T>(name: &CStr, mut value: T) -> io::Result<()> {
    // SAFETY: callers use the control's C type; jemalloc reads the correctly
    // sized, aligned input during this call and does not retain its pointer.
    let error = unsafe {
        mallctl(
            name.as_ptr(),
            std::ptr::null_mut(),
            std::ptr::null_mut(),
            (&mut value as *mut T).cast(),
            std::mem::size_of::<T>(),
        )
    };
    if error != 0 {
        return Err(control_error(name, error));
    }
    Ok(())
}

fn arena_control(arena: u32, property: &str) -> CString {
    CString::new(format!("arena.{arena}.{property}")).expect("static arena control name")
}

pub fn configure_reclamation() -> io::Result<()> {
    if std::env::var_os("_RJEM_MALLOC_CONF").is_some_and(|value| !value.is_empty()) {
        log::debug!("Explicit jemalloc configuration retained");
        return Ok(());
    }

    // Set defaults first so any arena created during startup inherits them.
    write(c"arenas.dirty_decay_ms", DIRTY_DECAY_MS)?;
    write(c"arenas.muzzy_decay_ms", MUZZY_DECAY_MS)?;

    // Existing arenas (including allocations before main) keep separate decay
    // values. Refresh initialization metadata and configure those arenas too.
    write(c"epoch", 1u64)?;
    let arenas: u32 = read(c"arenas.narenas")?;
    for arena in 0..arenas {
        if read::<bool>(&arena_control(arena, "initialized"))? {
            write(&arena_control(arena, "dirty_decay_ms"), DIRTY_DECAY_MS)?;
            write(&arena_control(arena, "muzzy_decay_ms"), MUZZY_DECAY_MS)?;
        }
    }
    write(c"background_thread", true)?;
    log::info!("Jemalloc background reclamation enabled: dirty_decay_ms=60000, muzzy_decay_ms=0");
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reclamation_defaults_and_explicit_overrides_are_process_isolated() {
        let Some(case) = std::env::var_os("SHOES_ALLOCATOR_TEST_CASE") else {
            for (case, profile) in [
                ("defaults", None),
                ("empty", Some("")),
                (
                    "override",
                    Some(
                        "background_thread:false,dirty_decay_ms:1000,muzzy_decay_ms:0,abort_conf:true",
                    ),
                ),
            ] {
                let mut child = std::process::Command::new(std::env::current_exe().unwrap());
                child
                    .args([
                        "--exact",
                        "allocator::tests::reclamation_defaults_and_explicit_overrides_are_process_isolated",
                        "--quiet",
                    ])
                    .env("SHOES_ALLOCATOR_TEST_CASE", case)
                    .env_remove("_RJEM_MALLOC_CONF");
                if let Some(profile) = profile {
                    child.env("_RJEM_MALLOC_CONF", profile);
                }
                assert!(child.status().unwrap().success());
            }
            return;
        };

        // An existing worker arena must also receive the policy, not only
        // arenas created after startup. Keep its live allocation until checked.
        let ready = std::sync::Arc::new(std::sync::Barrier::new(2));
        let worker_ready = ready.clone();
        let (send, receive) = std::sync::mpsc::channel();
        let worker = std::thread::spawn(move || {
            let bytes = vec![0x5a; 16 * 1024];
            std::hint::black_box(&bytes);
            let arena: u32 = read(c"thread.arena").unwrap();
            send.send(arena).unwrap();
            worker_ready.wait();
            assert!(bytes.iter().all(|byte| *byte == 0x5a));
        });
        let worker_arena = receive.recv().unwrap();
        configure_reclamation().unwrap();

        let overridden = case == "override";
        let expected_dirty = if overridden {
            1_000isize
        } else {
            DIRTY_DECAY_MS
        };
        assert_eq!(
            read::<isize>(c"arenas.dirty_decay_ms").unwrap(),
            expected_dirty
        );
        assert_eq!(read::<isize>(c"arenas.muzzy_decay_ms").unwrap(), 0);
        assert_eq!(read::<bool>(c"background_thread").unwrap(), !overridden);
        assert_eq!(
            read::<isize>(&arena_control(worker_arena, "dirty_decay_ms")).unwrap(),
            expected_dirty
        );
        let main_arena: u32 = read(c"thread.arena").unwrap();
        assert_eq!(
            read::<isize>(&arena_control(main_arena, "dirty_decay_ms")).unwrap(),
            expected_dirty
        );
        for arena in [main_arena, worker_arena] {
            assert_eq!(
                read::<isize>(&arena_control(arena, "muzzy_decay_ms")).unwrap(),
                0
            );
        }
        // A fresh arena must inherit the new defaults as well. This empty arena
        // is confined to the short-lived child process and owns no test buffers.
        let future_arena: u32 = read(c"arenas.create").unwrap();
        assert_eq!(
            read::<isize>(&arena_control(future_arena, "dirty_decay_ms")).unwrap(),
            expected_dirty
        );
        assert_eq!(
            read::<isize>(&arena_control(future_arena, "muzzy_decay_ms")).unwrap(),
            0
        );
        ready.wait();
        worker.join().unwrap();
    }

    #[test]
    fn unsupported_controls_return_errors_instead_of_silent_success() {
        let error = write(c"shoes.nonexistent_allocator_control", true).unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::NotFound);
    }

    #[test]
    #[ignore = "manual 256 MiB allocator reclamation probe; up to 180 seconds"]
    fn background_reclaims_unused_buffers_with_workers_alive() {
        const NAME: &str =
            "allocator::tests::background_reclaims_unused_buffers_with_workers_alive";
        if std::env::var_os("SHOES_ALLOCATOR_RECLAIM_TEST_CHILD").is_none() {
            let status = std::process::Command::new(std::env::current_exe().unwrap())
                .args([
                    "--exact",
                    NAME,
                    "--ignored",
                    "--nocapture",
                    "--test-threads=1",
                ])
                .env("SHOES_ALLOCATOR_RECLAIM_TEST_CHILD", "1")
                .env_remove("_RJEM_MALLOC_CONF")
                .status()
                .unwrap();
            assert!(status.success());
            return;
        }

        fn rss_kib() -> u64 {
            std::fs::read_to_string("/proc/self/smaps_rollup")
                .unwrap()
                .lines()
                .find_map(|line| {
                    line.strip_prefix("Rss:")?
                        .split_whitespace()
                        .next()?
                        .parse()
                        .ok()
                })
                .unwrap()
        }

        configure_reclamation().unwrap();
        let barrier = std::sync::Arc::new(std::sync::Barrier::new(5));
        let workers: Vec<_> = (0..4)
            .map(|_| {
                let barrier = barrier.clone();
                std::thread::spawn(move || {
                    let blocks: Vec<_> = (0..4096).map(|_| vec![0x5au8; 16384]).collect();
                    std::hint::black_box(&blocks);
                    barrier.wait();
                    barrier.wait();
                    let kept: Vec<_> = blocks
                        .into_iter()
                        .enumerate()
                        .filter_map(|(i, block)| (i % 32 == 0).then_some(block))
                        .collect();
                    assert_eq!(kept.len(), 128);
                    barrier.wait();
                    barrier.wait();
                    assert!(
                        kept.iter()
                            .all(|block| block.iter().all(|byte| *byte == 0x5a))
                    );
                })
            })
            .collect();
        barrier.wait();
        let peak = rss_kib();
        assert!(peak > 256 * 1024);
        barrier.wait();
        barrier.wait();
        let start = std::time::Instant::now();
        let deadline = start + std::time::Duration::from_secs(180);
        let mut rss = rss_kib();
        while rss >= 64 * 1024 && std::time::Instant::now() < deadline {
            std::thread::sleep(std::time::Duration::from_secs(1));
            rss = rss_kib();
        }
        assert!(workers.iter().all(|worker| !worker.is_finished()));
        assert!(
            rss < 64 * 1024,
            "RSS remained {rss} KiB after reclamation deadline"
        );
        println!(
            "jemalloc: peak={peak} KiB, reclaimed={rss} KiB, elapsed={:.1}s",
            start.elapsed().as_secs_f64()
        );
        barrier.wait();
        for worker in workers {
            worker.join().unwrap();
        }
    }
}
