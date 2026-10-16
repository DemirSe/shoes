//! Multi-output logging infrastructure.
//!
//! Provides `MultiLogger` which dispatches pre-formatted log lines to multiple
//! `LogWriter` destinations. Formats each record once into a thread-local buffer,
//! then passes the resulting `&str` to all writers.

use std::cell::Cell;
use std::fs::{File, OpenOptions};
use std::io::Write;

use log::{Level, LevelFilter, Log, Metadata, Record};

/// Receives pre-formatted log lines. Implementations handle one output destination.
pub trait LogWriter: Send + Sync {
    /// Writes a pre-formatted log line. `formatted` does NOT include a trailing newline.
    fn write_log(&self, record: &Record, formatted: &str);
    fn flush(&self);
}

/// Writes to stderr with ASCII sanitization to prevent terminal escape sequences.
pub struct StderrWriter;

impl LogWriter for StderrWriter {
    fn write_log(&self, record: &Record, formatted: &str) {
        let line = stderr_log_line(record.level(), formatted, stderr_is_journal());
        let _ = std::io::stderr().write_all(line.as_bytes());
    }

    fn flush(&self) {
        let _ = std::io::stderr().flush();
    }
}

/// Match the actual stderr descriptor, not just an inherited environment flag.
/// Plain terminals, redirected files and non-Linux platforms retain their format.
fn stderr_is_journal() -> bool {
    #[cfg(target_os = "linux")]
    {
        use std::os::unix::fs::MetadataExt;
        let Ok(stream) = std::env::var("JOURNAL_STREAM") else {
            return false;
        };
        let Ok(metadata) = std::fs::metadata("/proc/self/fd/2") else {
            return false;
        };
        journal_stream_matches(&stream, metadata.dev(), metadata.ino())
    }
    #[cfg(not(target_os = "linux"))]
    false
}

#[cfg(any(target_os = "linux", test))]
fn journal_stream_matches(stream: &str, device: u64, inode: u64) -> bool {
    let Some((dev, ino)) = stream.split_once(':') else {
        return false;
    };
    dev.parse::<u64>() == Ok(device) && ino.parse::<u64>() == Ok(inode)
}

fn stderr_log_line(level: Level, formatted: &str, journal: bool) -> String {
    let mut line = String::with_capacity(formatted.len() + 4);
    if journal {
        line.push_str(match level {
            Level::Error => "<3>",
            Level::Warn => "<4>",
            Level::Info => "<6>",
            Level::Debug | Level::Trace => "<7>",
        });
    }
    line.extend(formatted.chars().map(|c| {
        if c.is_ascii_graphic() || c == ' ' {
            c
        } else {
            '?'
        }
    }));
    line.push('\n');
    line
}

/// Writes to a file opened at init time. Each log line is a single write() syscall
/// to the kernel page cache (no BufWriter needed -- the kernel handles writeback).
pub struct FileLogWriter {
    file: parking_lot::Mutex<File>,
}

/// Keep a record on one physical line, including when its message is untrusted.
/// Preserve printable Unicode, but neutralize controls and Unicode line separators.
fn file_log_line(formatted: &str) -> String {
    fn safe(c: char) -> bool {
        !c.is_control() && c != '\u{2028}' && c != '\u{2029}'
    }

    let mut line = String::with_capacity(formatted.len() + 1);
    if formatted.chars().all(safe) {
        line.push_str(formatted);
    } else {
        line.extend(formatted.chars().map(|c| if safe(c) { c } else { '?' }));
    }
    line.push('\n');
    line
}

impl FileLogWriter {
    pub fn new(path: &str) -> std::io::Result<Self> {
        let file = OpenOptions::new().create(true).append(true).open(path)?;
        Ok(Self {
            file: parking_lot::Mutex::new(file),
        })
    }
}

impl LogWriter for FileLogWriter {
    fn write_log(&self, _record: &Record, formatted: &str) {
        let mut guard = self.file.lock();
        let line = file_log_line(formatted);
        let _ = guard.write_all(line.as_bytes());
    }

    fn flush(&self) {
        let _ = self.file.lock().flush();
    }
}

/// Writes to a file that may be set after logger init (FFI use case).
/// References a global `OnceLock<parking_lot::Mutex<Option<File>>>`.
// Used by iOS/Android FFI targets (ffi/ios.rs, ffi/android.rs).
#[allow(dead_code)]
pub struct DynamicFileLogWriter {
    file: &'static std::sync::OnceLock<parking_lot::Mutex<Option<File>>>,
}

// Used by iOS/Android FFI targets (ffi/ios.rs, ffi/android.rs).
#[allow(dead_code)]
impl DynamicFileLogWriter {
    pub fn new(file: &'static std::sync::OnceLock<parking_lot::Mutex<Option<File>>>) -> Self {
        Self { file }
    }
}

impl LogWriter for DynamicFileLogWriter {
    fn write_log(&self, _record: &Record, formatted: &str) {
        if let Some(mutex) = self.file.get() {
            let mut guard = mutex.lock();
            if let Some(ref mut file) = *guard {
                let line = file_log_line(formatted);
                let _ = file.write_all(line.as_bytes());
            }
        }
    }

    fn flush(&self) {
        if let Some(mutex) = self.file.get() {
            let mut guard = mutex.lock();
            if let Some(ref mut file) = *guard {
                let _ = file.flush();
            }
        }
    }
}

thread_local! {
    static FMT_BUF: Cell<String> = Cell::new(String::with_capacity(256));
}

/// A filter directive: optional target prefix + level.
/// When `name` is None, matches all targets (acts as the default level).
pub struct Directive {
    pub name: Option<String>,
    pub level: LevelFilter,
}

/// Dispatches formatted log lines to multiple `LogWriter` destinations.
/// Filters records using env_logger-compatible directive matching.
pub struct MultiLogger {
    writers: Vec<Box<dyn LogWriter>>,
    /// Sorted by name length ascending; walked in reverse for longest-prefix match.
    directives: Vec<Directive>,
}

impl MultiLogger {
    fn matches(&self, level: Level, target: &str) -> bool {
        for directive in self.directives.iter().rev() {
            match &directive.name {
                Some(name) if !target.starts_with(name.as_str()) => continue,
                _ => return level <= directive.level,
            }
        }
        false
    }
}

impl Log for MultiLogger {
    fn enabled(&self, metadata: &Metadata) -> bool {
        self.matches(metadata.level(), metadata.target())
    }

    fn log(&self, record: &Record) {
        if !self.matches(record.level(), record.target()) {
            return;
        }

        FMT_BUF.with(|cell| {
            let mut buf = cell.take();
            buf.clear();

            use std::fmt::Write as FmtWrite;
            let timestamp = chrono::Local::now().format("%Y-%m-%dT%H:%M:%S%.3f");
            let _ = write!(
                buf,
                "[{} {} {}] {}",
                timestamp,
                record.level(),
                record.target(),
                record.args()
            );

            for writer in &self.writers {
                writer.write_log(record, &buf);
            }

            cell.set(buf);
        });
    }

    fn flush(&self) {
        for writer in &self.writers {
            writer.flush();
        }
    }
}

/// Installs a `MultiLogger` as the global logger.
/// Directives are sorted by name length; `log::set_max_level` is set to the
/// maximum level across all directives so records reach our `log()` method.
pub fn init_multi_logger(writers: Vec<Box<dyn LogWriter>>, mut directives: Vec<Directive>) {
    directives.sort_by_key(|d| d.name.as_ref().map_or(0, |n| n.len()));
    let max_level = directives
        .iter()
        .map(|d| d.level)
        .max()
        .unwrap_or(LevelFilter::Off);
    let logger = MultiLogger {
        writers,
        directives,
    };
    log::set_boxed_logger(Box::new(logger)).expect("logger already initialized");
    log::set_max_level(max_level);
}

/// Parses a level string (case-insensitive). Returns `None` for unrecognized values.
pub fn parse_log_level(s: &str) -> Option<LevelFilter> {
    match s.to_lowercase().as_str() {
        "error" => Some(LevelFilter::Error),
        "warn" => Some(LevelFilter::Warn),
        "info" => Some(LevelFilter::Info),
        "debug" => Some(LevelFilter::Debug),
        "trace" => Some(LevelFilter::Trace),
        "off" => Some(LevelFilter::Off),
        _ => None,
    }
}

/// Parses a RUST_LOG-style directive string.
/// Examples: "info", "shoes=info", "warn,shoes=debug,quinn=error"
fn parse_directives(spec: &str) -> Vec<Directive> {
    let mut directives = Vec::new();
    for part in spec.split(',') {
        let part = part.trim();
        if part.is_empty() {
            continue;
        }
        if let Some((name, level_str)) = part.split_once('=') {
            if let Some(level) = parse_log_level(level_str) {
                directives.push(Directive {
                    name: Some(name.to_owned()),
                    level,
                });
            }
        } else if let Some(level) = parse_log_level(part) {
            directives.push(Directive { name: None, level });
        }
    }
    if directives.is_empty() {
        directives.push(Directive {
            name: None,
            level: LevelFilter::Error,
        });
    }
    directives
}

/// Determines filter directives from (in priority order):
/// 1. `.shoes-trace` / `.shoes-debug` marker files next to the binary
/// 2. `RUST_LOG` environment variable (supports `target=level` syntax)
/// 3. Default: error only (matches env_logger)
pub fn resolve_directives() -> Vec<Directive> {
    if let Some(dir) = std::env::current_exe()
        .ok()
        .and_then(|p| p.parent().map(|p| p.to_path_buf()))
    {
        if dir.join(".shoes-trace").exists() {
            eprintln!("Found marker file .shoes-trace, setting log level to TRACE");
            return vec![Directive {
                name: None,
                level: LevelFilter::Trace,
            }];
        }
        if dir.join(".shoes-debug").exists() {
            eprintln!("Found marker file .shoes-debug, setting log level to DEBUG");
            return vec![Directive {
                name: None,
                level: LevelFilter::Debug,
            }];
        }
    }

    if let Ok(val) = std::env::var("RUST_LOG") {
        return parse_directives(&val);
    }

    vec![Directive {
        name: None,
        level: LevelFilter::Error,
    }]
}

#[cfg(test)]
mod tests {
    use super::*;
    use log::Level;

    #[test]
    fn journal_priorities_map_levels_and_plain_stderr_is_unchanged() {
        for (level, prefix) in [
            (Level::Error, "<3>"),
            (Level::Warn, "<4>"),
            (Level::Info, "<6>"),
            (Level::Debug, "<7>"),
            (Level::Trace, "<7>"),
        ] {
            assert_eq!(
                stderr_log_line(level, "record", true),
                format!("{prefix}record\n")
            );
            assert_eq!(stderr_log_line(level, "record", false), "record\n");
        }
        assert_eq!(
            stderr_log_line(Level::Error, "a\n<0>forged\r\x1b\u{2028}", true),
            "<3>a?<0>forged???\n"
        );
    }

    #[test]
    fn journal_stream_requires_matching_device_and_inode() {
        assert!(journal_stream_matches("12:34", 12, 34));
        for stream in ["", "12", "12:35", "13:34", "x:34", "12:34:56"] {
            assert!(!journal_stream_matches(stream, 12, 34));
        }
    }

    #[test]
    fn close_details_are_debug_only_but_summary_is_visible_by_default() {
        let default = logger_from(parse_directives(""));
        assert!(!default.matches(Level::Debug, "shoes::tcp::connection_errors"));
        assert!(default.matches(Level::Error, "shoes::connection_errors"));
        let debug = logger_from(parse_directives("debug"));
        assert!(debug.matches(Level::Debug, "shoes::tcp::connection_errors"));
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn stderr_detects_the_actual_journal_descriptor_in_a_child_process() {
        use std::io::Read;
        use std::os::fd::{AsRawFd, OwnedFd};
        use std::os::unix::fs::MetadataExt;
        use std::os::unix::net::UnixStream;
        use std::process::{Command, Stdio};

        const CHILD: &str = "SHOES_TEST_JOURNAL_WRITER_CHILD";
        if std::env::var_os(CHILD).is_some() {
            for level in [
                Level::Error,
                Level::Warn,
                Level::Info,
                Level::Debug,
                Level::Trace,
            ] {
                StderrWriter.write_log(&Record::builder().level(level).build(), "safe\n<0>forged");
            }
            return;
        }
        for matched in [true, false] {
            let (mut reader, writer) = UnixStream::pair().unwrap();
            reader
                .set_read_timeout(Some(std::time::Duration::from_secs(5)))
                .unwrap();
            let metadata =
                std::fs::metadata(format!("/proc/self/fd/{}", writer.as_raw_fd())).unwrap();
            let stream = format!(
                "{}:{}",
                metadata.dev(),
                metadata.ino() + u64::from(!matched)
            );
            let mut child = Command::new(std::env::current_exe().unwrap())
                .args(["--exact", "logging::tests::stderr_detects_the_actual_journal_descriptor_in_a_child_process", "--nocapture"])
                .env(CHILD, "1")
                .env("JOURNAL_STREAM", stream)
                .stdout(Stdio::null())
                .stderr(Stdio::from(OwnedFd::from(writer)))
                .spawn().unwrap();
            let mut output = String::new();
            reader.read_to_string(&mut output).unwrap();
            assert!(child.wait().unwrap().success());
            let expected = if matched {
                "<3>safe?<0>forged\n<4>safe?<0>forged\n<6>safe?<0>forged\n<7>safe?<0>forged\n<7>safe?<0>forged\n".to_owned()
            } else {
                "safe?<0>forged\n".repeat(5)
            };
            assert_eq!(output, expected);
        }
    }

    #[test]
    fn file_log_lines_preserve_printable_text_and_neutralize_controls() {
        assert_eq!(file_log_line(""), "\n");
        assert_eq!(
            file_log_line("normal UTF-8: bağlantı"),
            "normal UTF-8: bağlantı\n"
        );
        assert_eq!(
            file_log_line("message\nforged\r\t\0\x1b[31m\x7f\u{0085}\u{2028}\u{2029}"),
            "message?forged????[31m????\n"
        );
    }

    #[test]
    fn file_writer_keeps_untrusted_messages_on_one_line() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("server.log");
        let writer = FileLogWriter::new(path.to_str().unwrap()).unwrap();
        let record = Record::builder().args(format_args!("unused")).build();
        writer.write_log(&record, "first\nforged\r\x1b[2J");
        writer.write_log(&record, "second: bağlantı");
        writer.flush();
        assert_eq!(
            std::fs::read_to_string(path).unwrap(),
            "first?forged??[2J\nsecond: bağlantı\n"
        );
    }

    #[test]
    fn dynamic_file_writer_keeps_untrusted_messages_on_one_line() {
        static FILE: std::sync::OnceLock<parking_lot::Mutex<Option<File>>> =
            std::sync::OnceLock::new();
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("mobile.log");
        FILE.set(parking_lot::Mutex::new(Some(File::create(&path).unwrap())))
            .unwrap();
        let writer = DynamicFileLogWriter::new(&FILE);
        let record = Record::builder().args(format_args!("unused")).build();
        writer.write_log(&record, "first\u{2028}forged\u{2029}\u{0085}");
        writer.write_log(&record, "second: bağlantı");
        writer.flush();
        assert_eq!(
            std::fs::read_to_string(path).unwrap(),
            "first?forged??\nsecond: bağlantı\n"
        );
        // Close the file before the temporary directory is removed on Windows.
        FILE.get().unwrap().lock().take();
    }

    /// Builds a MultiLogger (no writers) from directives for testing matches().
    fn logger_from(mut directives: Vec<Directive>) -> MultiLogger {
        directives.sort_by_key(|d| d.name.as_ref().map_or(0, |n| n.len()));
        MultiLogger {
            writers: vec![],
            directives,
        }
    }

    #[test]
    fn parse_log_level_valid() {
        assert_eq!(parse_log_level("info"), Some(LevelFilter::Info));
        assert_eq!(parse_log_level("ERROR"), Some(LevelFilter::Error));
        assert_eq!(parse_log_level("Debug"), Some(LevelFilter::Debug));
        assert_eq!(parse_log_level("off"), Some(LevelFilter::Off));
    }

    #[test]
    fn parse_log_level_invalid() {
        assert_eq!(parse_log_level(""), None);
        assert_eq!(parse_log_level("verbose"), None);
        assert_eq!(parse_log_level("inf"), None);
    }

    #[test]
    fn parse_directives_blanket_level() {
        let dirs = parse_directives("info");
        assert_eq!(dirs.len(), 1);
        assert!(dirs[0].name.is_none());
        assert_eq!(dirs[0].level, LevelFilter::Info);
    }

    #[test]
    fn parse_directives_single_target() {
        let dirs = parse_directives("shoes=debug");
        assert_eq!(dirs.len(), 1);
        assert_eq!(dirs[0].name.as_deref(), Some("shoes"));
        assert_eq!(dirs[0].level, LevelFilter::Debug);
    }

    #[test]
    fn parse_directives_mixed() {
        let dirs = parse_directives("warn,shoes=info,quinn=error");
        assert_eq!(dirs.len(), 3);
        // Should contain a blanket warn and two targeted directives
        let blanket = dirs.iter().find(|d| d.name.is_none()).unwrap();
        assert_eq!(blanket.level, LevelFilter::Warn);
        let shoes = dirs
            .iter()
            .find(|d| d.name.as_deref() == Some("shoes"))
            .unwrap();
        assert_eq!(shoes.level, LevelFilter::Info);
        let quinn = dirs
            .iter()
            .find(|d| d.name.as_deref() == Some("quinn"))
            .unwrap();
        assert_eq!(quinn.level, LevelFilter::Error);
    }

    #[test]
    fn parse_directives_empty_falls_back_to_error() {
        let dirs = parse_directives("");
        assert_eq!(dirs.len(), 1);
        assert!(dirs[0].name.is_none());
        assert_eq!(dirs[0].level, LevelFilter::Error);
    }

    #[test]
    fn parse_directives_invalid_level_skipped() {
        let dirs = parse_directives("shoes=bogus,info");
        // "shoes=bogus" is skipped, "info" is kept
        assert_eq!(dirs.len(), 1);
        assert!(dirs[0].name.is_none());
        assert_eq!(dirs[0].level, LevelFilter::Info);
    }

    #[test]
    fn matches_blanket_error_default() {
        let logger = logger_from(vec![Directive {
            name: None,
            level: LevelFilter::Error,
        }]);
        assert!(logger.matches(Level::Error, "shoes::tcp"));
        assert!(!logger.matches(Level::Warn, "shoes::tcp"));
        assert!(!logger.matches(Level::Info, "shoes::tcp"));
        assert!(logger.matches(Level::Error, "quinn::connection"));
    }

    #[test]
    fn matches_blanket_info() {
        let logger = logger_from(vec![Directive {
            name: None,
            level: LevelFilter::Info,
        }]);
        assert!(logger.matches(Level::Error, "shoes"));
        assert!(logger.matches(Level::Warn, "quinn"));
        assert!(logger.matches(Level::Info, "shoes::tcp"));
        assert!(!logger.matches(Level::Debug, "shoes::tcp"));
    }

    #[test]
    fn matches_targeted_only() {
        // shoes=info with no blanket → only shoes passes, others filtered out
        let logger = logger_from(vec![Directive {
            name: Some("shoes".into()),
            level: LevelFilter::Info,
        }]);
        assert!(logger.matches(Level::Info, "shoes::tcp"));
        assert!(logger.matches(Level::Error, "shoes"));
        assert!(!logger.matches(Level::Info, "quinn::connection"));
        assert!(!logger.matches(Level::Error, "rustls"));
    }

    #[test]
    fn matches_blanket_plus_override() {
        // warn,shoes=debug → shoes gets debug, everything else gets warn
        let logger = logger_from(parse_directives("warn,shoes=debug"));
        assert!(logger.matches(Level::Debug, "shoes::tcp"));
        assert!(logger.matches(Level::Info, "shoes"));
        assert!(logger.matches(Level::Warn, "quinn"));
        assert!(!logger.matches(Level::Info, "quinn"));
        assert!(!logger.matches(Level::Debug, "rustls"));
        assert!(logger.matches(Level::Error, "rustls"));
    }

    #[test]
    fn matches_longest_prefix_wins() {
        // shoes=warn,shoes::tcp=debug → shoes::tcp gets debug, shoes gets warn
        let logger = logger_from(parse_directives("shoes=warn,shoes::tcp=debug"));
        assert!(logger.matches(Level::Debug, "shoes::tcp"));
        assert!(logger.matches(Level::Debug, "shoes::tcp::handler"));
        assert!(!logger.matches(Level::Debug, "shoes::config"));
        assert!(logger.matches(Level::Warn, "shoes::config"));
        assert!(!logger.matches(Level::Info, "shoes::config"));
    }

    #[test]
    fn matches_off_suppresses() {
        let logger = logger_from(vec![Directive {
            name: None,
            level: LevelFilter::Off,
        }]);
        assert!(!logger.matches(Level::Error, "shoes"));
        assert!(!logger.matches(Level::Error, "anything"));
    }

    #[test]
    fn matches_no_directives() {
        let logger = logger_from(vec![]);
        assert!(!logger.matches(Level::Error, "shoes"));
    }
}
