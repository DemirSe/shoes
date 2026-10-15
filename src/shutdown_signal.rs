//! CLI-only synchronous signal waiting; no async signal handler or Tokio signal feature.
use std::io;

/// Call on the main thread before creating the runtime or configuration watcher.
/// Threads created afterwards inherit the blocked mask. The waiter is intentionally
/// detached: process exit also terminates it on dry-run and startup-error paths.
#[cfg(unix)]
pub fn listen() -> io::Result<impl Future<Output = io::Result<()>>> {
    let mut signals = std::mem::MaybeUninit::<libc::sigset_t>::uninit();
    // SAFETY: sigemptyset initializes the writable set before sigaddset uses it.
    let signals = unsafe {
        if libc::sigemptyset(signals.as_mut_ptr()) != 0 {
            return Err(io::Error::last_os_error());
        }
        let mut signals = signals.assume_init();
        if libc::sigaddset(&mut signals, libc::SIGTERM) != 0
            || libc::sigaddset(&mut signals, libc::SIGINT) != 0
        {
            return Err(io::Error::last_os_error());
        }
        signals
    };
    // SAFETY: the initialized mask is valid. No previous mask is requested.
    let error = unsafe { libc::pthread_sigmask(libc::SIG_BLOCK, &signals, std::ptr::null_mut()) };
    if error != 0 {
        return Err(io::Error::from_raw_os_error(error));
    }
    let (sender, receiver) = tokio::sync::oneshot::channel();
    std::thread::Builder::new()
        .name("shutdown-sigwait".into())
        .spawn(move || {
            let mut signal = 0;
            // SAFETY: the set is initialized and blocked in this thread, and the
            // output points to a live integer. sigwait runs in a normal thread.
            let error = unsafe { libc::sigwait(&signals, &mut signal) };
            let result = if error == 0 {
                Ok(())
            } else {
                Err(io::Error::from_raw_os_error(error))
            };
            let _ = sender.send(result);
        })?;
    Ok(async move {
        receiver
            .await
            .map_err(|_| io::Error::other("signal waiter stopped"))?
    })
}

#[cfg(not(unix))]
pub fn listen() -> io::Result<impl Future<Output = io::Result<()>>> {
    Ok(std::future::pending())
}
