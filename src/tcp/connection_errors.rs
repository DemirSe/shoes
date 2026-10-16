//! Process-wide, bounded reporting shared by TCP/Unix listeners and reloads.
//! No peer identity or error message is retained, even at debug level.
use std::io::ErrorKind;
use std::sync::Mutex;
use std::time::{Duration, Instant};

const INTERVAL: Duration = Duration::from_secs(30);
const CATEGORIES: usize = 8;
static ERRORS: Mutex<ErrorSummary> = Mutex::new(ErrorSummary::new());

struct ErrorSummary {
    counts: [u64; CATEGORIES],
    last_report: Option<Instant>,
}

impl ErrorSummary {
    const fn new() -> Self {
        Self {
            counts: [0; CATEGORIES],
            last_report: None,
        }
    }

    fn record(&mut self, kind: ErrorKind, now: Instant) -> Option<[u64; CATEGORIES]> {
        let index = match kind {
            ErrorKind::InvalidData => 0,
            ErrorKind::UnexpectedEof => 1,
            ErrorKind::ConnectionReset => 2,
            ErrorKind::ConnectionAborted => 3,
            ErrorKind::TimedOut => 4,
            ErrorKind::ConnectionRefused => 5,
            ErrorKind::BrokenPipe => 6,
            _ => 7,
        };
        self.counts[index] = self.counts[index].saturating_add(1);
        if self
            .last_report
            .is_some_and(|previous| now.saturating_duration_since(previous) < INTERVAL)
        {
            return None;
        }
        self.last_report = Some(now);
        Some(std::mem::replace(&mut self.counts, [0; CATEGORIES]))
    }
}

pub(super) fn record(kind: ErrorKind) {
    log::debug!("Connection closed with error kind={kind:?}");
    // Hold through emission so concurrent listeners cannot reorder summaries.
    // This lock is never held across an await; formatting happens at most once/30s.
    let mut summary = ERRORS.lock().unwrap_or_else(|error| error.into_inner());
    if let Some(counts) = summary.record(kind, Instant::now()) {
        log::error!(
            target: "shoes::connection_errors",
            "Connection close error counts (not service failures): InvalidData={} UnexpectedEof={} ConnectionReset={} ConnectionAborted={} TimedOut={} ConnectionRefused={} BrokenPipe={} Other={}",
            counts[0], counts[1], counts[2], counts[3], counts[4], counts[5], counts[6], counts[7]
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn first_error_reports_then_counts_are_gated_and_reset() {
        let mut summary = ErrorSummary::new();
        let start = Instant::now();
        assert_eq!(
            summary.record(ErrorKind::InvalidData, start),
            Some([1, 0, 0, 0, 0, 0, 0, 0])
        );
        for kind in [
            ErrorKind::InvalidData,
            ErrorKind::UnexpectedEof,
            ErrorKind::ConnectionReset,
            ErrorKind::ConnectionAborted,
            ErrorKind::TimedOut,
            ErrorKind::ConnectionRefused,
            ErrorKind::BrokenPipe,
            ErrorKind::PermissionDenied,
            ErrorKind::Other,
        ] {
            assert_eq!(
                summary.record(kind, start + INTERVAL - Duration::from_nanos(1)),
                None
            );
        }
        assert_eq!(
            summary.record(ErrorKind::InvalidData, start + INTERVAL),
            Some([2, 1, 1, 1, 1, 1, 1, 2])
        );
        assert_eq!(summary.counts, [0; CATEGORIES]);
        assert_eq!(summary.record(ErrorKind::TimedOut, start + INTERVAL), None);
        // Idle intervals do not schedule background work. Pending counts are
        // retained until another failure, rather than lost during reload.
        assert_eq!(
            summary.record(ErrorKind::BrokenPipe, start + INTERVAL * 10),
            Some([0, 0, 0, 0, 1, 0, 1, 0])
        );
    }

    #[test]
    fn counts_saturate_and_unknown_errors_use_a_fixed_bucket() {
        let mut summary = ErrorSummary::new();
        let now = Instant::now();
        summary.last_report = Some(now);
        summary.counts[7] = u64::MAX;
        assert_eq!(summary.record(ErrorKind::OutOfMemory, now), None);
        assert_eq!(summary.counts[7], u64::MAX);
    }

    #[test]
    fn concurrent_generations_share_one_gate_without_losing_counts() {
        let summary = Mutex::new(ErrorSummary::new());
        let now = Instant::now();
        std::thread::scope(|scope| {
            for _ in 0..8 {
                let summary = &summary;
                scope.spawn(move || {
                    for _ in 0..100 {
                        let emitted = summary.lock().unwrap().record(ErrorKind::InvalidData, now);
                        if let Some(counts) = emitted {
                            assert_eq!(counts, [1, 0, 0, 0, 0, 0, 0, 0]);
                        }
                    }
                });
            }
        });
        let mut summary = summary.lock().unwrap();
        assert_eq!(
            summary.record(ErrorKind::InvalidData, now + INTERVAL),
            Some([800, 0, 0, 0, 0, 0, 0, 0])
        );
    }
}
