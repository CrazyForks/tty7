//! File logger — the `log::` records that otherwise go nowhere.
//!
//! tty7 depends on the `log` facade but shipped no backend, so every
//! `log::info!` / `log::warn!` in the tree was compiled in and then discarded.
//! That is survivable for the GUI, which can put a failure on screen. It is not
//! survivable for the **daemon**: [`crate::daemon::spawn`] detaches it with its
//! stdio pointed at `/dev/null`, so a remote install that refused, a connection
//! that dropped, or a pane that died left no trace anywhere — the only artifact
//! the process could produce was `crash.log`, and only if it panicked.
//!
//! So: one append-only file next to `crash.log`, same size cap and same
//! best-effort discipline. Logging must never be the reason something fails.
//!
//! ## Level
//!
//! `TTY7_LOG` (or `RUST_LOG`) sets it — `off` / `error` / `warn` / `info` /
//! `debug` / `trace`. **Default `off`**: this writes to a user's disk forever,
//! and a terminal that logs by default is a terminal that fills a disk while
//! nobody is watching. Ask for it when diagnosing, which is also the only time
//! the records are worth anything.

use std::fmt::Write as _;
use std::path::PathBuf;
use std::sync::{Mutex, OnceLock};

use log::{LevelFilter, Log, Metadata, Record};

/// Rewrite the log once it passes this. Same cap as `crash.log`, larger because
/// a debug session produces many small lines rather than a few big backtraces.
const MAX_BYTES: u64 = 4 * 1024 * 1024;

struct FileLogger {
    role: &'static str,
    path: PathBuf,
    /// Serializes writes so two threads cannot interleave halves of a line.
    /// Contended only while logging is on, which is not the default.
    lock: Mutex<()>,
}

impl Log for FileLogger {
    fn enabled(&self, metadata: &Metadata<'_>) -> bool {
        metadata.level() <= log::max_level()
    }

    fn log(&self, record: &Record<'_>) {
        if !self.enabled(record.metadata()) {
            return;
        }
        let mut line = String::new();
        let _ = write!(
            line,
            "{} {:5} {} [{}] {}\n",
            timestamp(),
            record.level(),
            self.role,
            record.target(),
            record.args(),
        );
        let _guard = self.lock.lock();
        append(&self.path, &line);
    }

    fn flush(&self) {}
}

/// Install the logger for this process, if the environment asks for one.
///
/// `role` labels the records, since the GUI and the daemon it spawns share one
/// config dir and therefore one log file — the same convention `crash.log`
/// uses, and the reason a line can be attributed at all.
///
/// Idempotent and silent on failure: a second call, a missing config dir, or a
/// read-only disk all leave the process running with no logger, which is
/// exactly what it had before.
pub fn install(role: &'static str) {
    let level = level_from_env();
    if level == LevelFilter::Off {
        return;
    }
    let Some(path) = log_path() else {
        return;
    };
    // A `static` rather than `set_boxed_logger`, which needs `log`'s `std`
    // feature — not enabled here, and not worth enabling for one allocation
    // that lives for the whole process anyway. `OnceLock` is also what makes a
    // second call harmless.
    static LOGGER: OnceLock<FileLogger> = OnceLock::new();
    let logger = LOGGER.get_or_init(|| FileLogger {
        role,
        path,
        lock: Mutex::new(()),
    });
    if log::set_logger(logger).is_ok() {
        log::set_max_level(level);
    }
}

/// `TTY7_LOG` first, then `RUST_LOG` — the former so turning on tty7's logging
/// does not also turn on every library that reads `RUST_LOG`.
///
/// Only a bare level is understood, not `RUST_LOG`'s per-module syntax: a
/// half-supported filter language is worse than an obvious one, because
/// `TTY7_LOG=tty7_core::daemon=debug` would silently mean "off".
fn level_from_env() -> LevelFilter {
    let raw = std::env::var("TTY7_LOG")
        .or_else(|_| std::env::var("RUST_LOG"))
        .unwrap_or_default();
    parse_level(&raw)
}

fn parse_level(raw: &str) -> LevelFilter {
    match raw.trim().to_ascii_lowercase().as_str() {
        "error" => LevelFilter::Error,
        "warn" | "warning" => LevelFilter::Warn,
        "info" => LevelFilter::Info,
        "debug" => LevelFilter::Debug,
        "trace" => LevelFilter::Trace,
        _ => LevelFilter::Off,
    }
}

fn append(path: &PathBuf, record: &str) {
    use std::io::Write as _;
    let truncate = std::fs::metadata(path).is_ok_and(|m| m.len() > MAX_BYTES);
    let Ok(mut file) = std::fs::OpenOptions::new()
        .create(true)
        .append(!truncate)
        .write(true)
        .truncate(truncate)
        .open(path)
    else {
        return;
    };
    let _ = file.write_all(record.as_bytes());
    let _ = file.flush();
}

fn log_path() -> Option<PathBuf> {
    crate::core::config::config_path("tty7.log")
}

/// `HH:MM:SS.mmm` — the time of day, which is what you compare against "I
/// clicked it just now". The date is in `crash.log`'s records and in the file's
/// own mtime; repeating it on every line would cost more than it tells.
fn timestamp() -> String {
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default();
    let secs = now.as_secs() % 86_400;
    format!(
        "{:02}:{:02}:{:02}.{:03}",
        secs / 3600,
        (secs % 3600) / 60,
        secs % 60,
        now.subsec_millis()
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use log::Level;

    /// The default has to be `Off`. A terminal that logs to disk unasked fills
    /// a disk on a machine nobody is watching — and the daemon outlives every
    /// window, so there is no session boundary to bound it.
    #[test]
    fn logging_is_off_unless_asked_for() {
        // Not via the environment: mutating it is `unsafe` in edition 2024 and
        // races every other test in the binary. The parser is the whole
        // decision, so it is what gets tested.
        assert_eq!(parse_level(""), LevelFilter::Off);
        assert_eq!(parse_level("   "), LevelFilter::Off);
        assert_eq!(parse_level("nonsense"), LevelFilter::Off);
        // `RUST_LOG`'s per-module syntax is deliberately *not* half-supported.
        assert_eq!(parse_level("tty7_core::daemon=debug"), LevelFilter::Off);
    }

    #[test]
    fn levels_parse_case_and_space_insensitively() {
        assert_eq!(parse_level("debug"), LevelFilter::Debug);
        assert_eq!(parse_level("  DEBUG "), LevelFilter::Debug);
        assert_eq!(parse_level("Warn"), LevelFilter::Warn);
        assert_eq!(parse_level("warning"), LevelFilter::Warn);
        assert_eq!(parse_level("TRACE"), LevelFilter::Trace);
    }

    /// A run away log must not grow without bound: past the cap the file is
    /// rewritten rather than appended to.
    #[test]
    fn the_file_is_rewritten_once_it_passes_the_cap() {
        let path = std::env::temp_dir().join(format!("tty7-logfile-{}.log", std::process::id()));
        let _ = std::fs::remove_file(&path);

        append(&path, "first\n");
        append(&path, "second\n");
        let both = std::fs::read_to_string(&path).unwrap();
        assert!(both.contains("first") && both.contains("second"), "appends");

        std::fs::write(&path, vec![b'x'; (MAX_BYTES + 1) as usize]).unwrap();
        append(&path, "after the cap\n");
        let after = std::fs::read_to_string(&path).unwrap();
        assert_eq!(after, "after the cap\n", "rewritten, not appended");

        let _ = std::fs::remove_file(&path);
    }

    /// Records name which process wrote them: the GUI and the daemon it spawns
    /// share one config dir, so an unattributed line is ambiguous exactly when
    /// it matters (which side dropped the connection?).
    #[test]
    fn a_record_names_its_role_and_target() {
        let path = std::env::temp_dir().join(format!("tty7-logrec-{}.log", std::process::id()));
        let _ = std::fs::remove_file(&path);
        let logger = FileLogger {
            role: "daemon",
            path: path.clone(),
            lock: Mutex::new(()),
        };
        log::set_max_level(LevelFilter::Info);
        logger.log(
            &Record::builder()
                .args(format_args!("remote build-box: installed tty7-server"))
                .level(Level::Info)
                .target("tty7_core::daemon::install")
                .build(),
        );
        let written = std::fs::read_to_string(&path).unwrap();
        assert!(written.contains("daemon"), "{written}");
        assert!(written.contains("tty7_core::daemon::install"), "{written}");
        assert!(written.contains("installed tty7-server"), "{written}");
        assert!(written.contains("INFO"), "{written}");
        let _ = std::fs::remove_file(&path);
    }
}
