//! Shared file diagnostics for the inbox and the MCP process.

use std::backtrace::Backtrace;
use std::cell::Cell;
use std::collections::VecDeque;
use std::env;
use std::fmt::{self, Write as _};
use std::fs::{self, DirBuilder, File, OpenOptions, Permissions};
use std::io::{self, Read as _, Seek as _, SeekFrom, Write as _};
use std::os::unix::fs::{DirBuilderExt, MetadataExt, OpenOptionsExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::sync::OnceLock;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

#[doc(hidden)]
pub use tracing;
use tracing::field::{Field, Visit};
use tracing::{Event, Subscriber};
use tracing_subscriber::layer::{Context, Layer, SubscriberExt};
use tracing_subscriber::{Registry, filter::LevelFilter};

const TARGET: &str = "fell.diagnostics";
const LOG_FILES: usize = 5;
const LOG_BYTES: u64 = 5 * 1024 * 1024;
const EVENT_BYTES: usize = 1024;
const RECENT_LINES: usize = 200;
const LOG_TAIL_BYTES: u64 = ((RECENT_LINES + 1) * EVENT_BYTES) as u64;

static PATHS: OnceLock<Paths> = OnceLock::new();
static SERIAL: AtomicU64 = AtomicU64::new(0);

/// Diagnostic event names are literals. The file subscriber only records
/// selected numeric measurements and error categories, at every level.
#[macro_export]
macro_rules! diagnostic {
    ($level:ident, $name:literal $(, $key:ident = $value:expr)* $(,)?) => {
        $crate::diagnostics::tracing::event!(
            name: $name,
            target: "fell.diagnostics",
            $crate::diagnostics::tracing::Level::$level,
            { $($key = $value),* }
        )
    };
}

#[derive(Clone)]
struct Paths(PathBuf);

impl Paths {
    fn from_env() -> io::Result<Self> {
        env::var_os("XDG_STATE_HOME")
            .map(PathBuf::from)
            .filter(|path| path.is_absolute())
            .or_else(|| {
                env::var_os("HOME")
                    .map(PathBuf::from)
                    .filter(|path| path.is_absolute())
                    .map(|home| home.join(".local/state"))
            })
            .map(|state| Self(state.join("fell")))
            .ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, "no state directory"))
    }

    fn directory(&self, name: &str) -> io::Result<PathBuf> {
        let dir = self.0.join(name);
        DirBuilder::new().recursive(true).mode(0o700).create(&dir)?;
        for private in [&self.0, &dir] {
            let metadata = private.symlink_metadata()?;
            if !metadata.is_dir() || metadata.mode() & 0o022 != 0 {
                return Err(io::Error::new(
                    io::ErrorKind::PermissionDenied,
                    "diagnostics directory must be writable only by its owner",
                ));
            }
        }
        Ok(dir)
    }

    fn logs(&self) -> PathBuf {
        self.0.join("logs")
    }

    fn crashes(&self) -> PathBuf {
        self.0.join("crashes")
    }

    fn latest_crash(&self) -> io::Result<PathBuf> {
        fs::read_dir(self.crashes())?
            .filter_map(Result::ok)
            .filter_map(|entry| {
                let name = entry.file_name();
                let key = crash_key(name.to_str()?)?;
                entry
                    .file_type()
                    .ok()?
                    .is_file()
                    .then(|| (key, entry.path()))
            })
            .max_by_key(|(key, _)| *key)
            .map(|(_, path)| path)
            .ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, "no crash report"))
    }
}

fn crash_key(name: &str) -> Option<(u128, u32, u64)> {
    let (time, rest) = name.strip_suffix(".txt")?.split_once('-')?;
    let (pid, serial) = rest.split_once('-')?;
    Some((time.parse().ok()?, pid.parse().ok()?, serial.parse().ok()?))
}

/// Install before launching any workers. The application can still start
/// if its state directory is unavailable.
pub fn init() -> io::Result<()> {
    let paths = Paths::from_env()?;
    if PATHS.set(paths.clone()).is_err() {
        return Ok(());
    }
    let crashes = paths.clone();
    std::panic::set_hook(Box::new(move |_| {
        let recovered = RECOVERABLE.get();
        if write_crash(&crashes, recovered).is_err() {
            let _ = writeln!(io::stderr(), "Fell could not write a crash report");
        }
    }));

    let directory = paths.directory("logs")?;
    let writer = RollingLog {
        directory,
        limit: LOG_BYTES,
    };
    writer.append(b"")?;
    let level = if env::var_os("FELL_LOG").is_some_and(|value| value == "debug") {
        LevelFilter::DEBUG
    } else {
        LevelFilter::INFO
    };
    let subscriber = Registry::default().with(level).with(writer);
    tracing::subscriber::set_global_default(subscriber).map_err(io::Error::other)
}

thread_local! {
    static RECOVERABLE: Cell<bool> = const { Cell::new(false) };
}

/// Mark a panic in this scope as recoverable for the hook. The caller owns
/// the recovery and can report it to whichever UI started the worker.
pub fn catch_worker<T>(work: impl FnOnce() -> T) -> std::thread::Result<T> {
    RECOVERABLE.with(|recoverable| {
        let previous = recoverable.replace(true);
        let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(work));
        recoverable.set(previous);
        result
    })
}

pub fn take_crash_notice() -> Option<PathBuf> {
    let dir = PATHS.get()?.crashes();
    let claimed = dir.join(format!("seen-{}", report_name()));
    // Move the marker out of the way atomically so concurrent launches can
    // only consume it once. The report remains available in the crash folder.
    fs::rename(dir.join("pending"), &claimed).ok()?;
    let mut name = String::new();
    let read = read_private(&claimed).and_then(|file| file.take(128).read_to_string(&mut name));
    let _ = fs::remove_file(claimed);
    read.ok()?;
    crash_key(&name)?;
    Some(dir.join(name))
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DiagnosticFile {
    Log,
    Crash,
}

impl DiagnosticFile {
    pub fn path(self) -> io::Result<PathBuf> {
        let paths = Paths::from_env()?;
        let path = match self {
            Self::Log => log_file(&paths.logs(), 0),
            Self::Crash => paths.latest_crash()?,
        };
        read_private(&path)?;
        Ok(path)
    }
}

pub fn write_report() -> io::Result<PathBuf> {
    let paths = Paths::from_env()?;
    let reports = paths.directory("reports")?;
    let path = reports.join(format!("{}.txt", report_name()));
    let mut report = format!("{}\n\nRecent log\n", fell_version::VERSION);
    append_recent_log(&mut report, &paths);
    report.push_str("\nLatest crash\n");
    match paths.latest_crash().and_then(|path| read_private(&path)) {
        Ok(file) => {
            file.take((RECENT_LINES * EVENT_BYTES * 2) as u64)
                .read_to_string(&mut report)?;
        }
        Err(err) if err.kind() == io::ErrorKind::NotFound => report.push_str("No crash report.\n"),
        Err(err) => return Err(err),
    }
    publish(&path, report.as_bytes())?;
    Ok(path)
}

fn write_crash(paths: &Paths, recovered: bool) -> io::Result<PathBuf> {
    let dir = paths.directory("crashes")?;
    let current = std::thread::current();
    let name = current
        .name()
        .filter(|name| {
            matches!(
                *name,
                "main" | "fell-sync" | "fell-store" | "fell-store-background"
            )
        })
        .unwrap_or("other");
    let mut report = format!(
        "{}\nThread: {name} ({:?})\nRecovered by worker: {recovered}\n\nBacktrace\n",
        fell_version::VERSION,
        current.id()
    );
    // Backtrace symbols identify the code. Source paths can contain the
    // user's home directory, and panic payloads can contain message data.
    for line in Backtrace::force_capture().to_string().lines() {
        if let Some((frame, _)) = line.trim().split_once(':')
            && frame.parse::<usize>().is_ok()
        {
            let _ = writeln!(report, "{line}");
        }
    }
    report.push_str("\nRecent log\n");
    append_recent_log(&mut report, paths);
    let name = format!("{}.txt", report_name());
    let path = dir.join(&name);
    publish(&path, report.as_bytes())?;
    if !recovered {
        publish(&dir.join("pending"), name.as_bytes())?;
    }
    Ok(path)
}

fn report_name() -> String {
    let time = now().as_nanos();
    let pid = std::process::id();
    let serial = SERIAL.fetch_add(1, Ordering::Relaxed);
    format!("{time}-{pid}-{serial}")
}

fn publish(path: &Path, contents: &[u8]) -> io::Result<()> {
    let staging = path.with_file_name(format!("{}.part", report_name()));
    let mut file = private_file(&staging, true)?;
    file.write_all(contents)?;
    fs::rename(staging, path)
}

fn now() -> std::time::Duration {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
}

fn read_private(path: &Path) -> io::Result<File> {
    let file = OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK)
        .open(path)?;
    let meta = file.metadata()?;
    if !meta.is_file() || meta.nlink() != 1 || meta.mode() & 0o077 != 0 {
        return Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            "not a private file",
        ));
    }
    Ok(file)
}

fn private_file(path: &Path, exclusive: bool) -> io::Result<File> {
    let mut opts = OpenOptions::new();
    opts.append(true)
        .mode(0o600)
        .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK);
    if exclusive {
        opts.create_new(true);
    } else {
        opts.create(true);
    }
    let file = opts.open(path)?;
    let meta = file.metadata()?;
    if !meta.is_file() || meta.nlink() != 1 {
        return Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            "not a regular file",
        ));
    }
    if meta.mode() & 0o777 != 0o600 {
        file.set_permissions(Permissions::from_mode(0o600))?;
    }
    Ok(file)
}

fn log_file(dir: &Path, generation: usize) -> PathBuf {
    if generation == 0 {
        dir.join("fell.log")
    } else {
        dir.join(format!("fell.log.{generation}"))
    }
}

struct RollingLog {
    directory: PathBuf,
    limit: u64,
}

impl RollingLog {
    fn append(&self, text: &[u8]) -> io::Result<()> {
        // Fell and multiple MCP agents may be live at once. Lock the
        // directory's separate lock inode across both rotation and append.
        let lock = private_file(&self.directory.join(".lock"), false)?;
        lock.lock()?;
        let logfile = log_file(&self.directory, 0);
        let mut file = private_file(&logfile, false)?;
        if file.metadata()?.len() + text.len() as u64 > self.limit {
            for generation in (1..LOG_FILES).rev() {
                let from = log_file(&self.directory, generation - 1);
                let to = log_file(&self.directory, generation);
                match fs::rename(from, to) {
                    Ok(()) => {}
                    Err(err) if err.kind() == io::ErrorKind::NotFound => {}
                    Err(err) => return Err(err),
                }
            }
            file = private_file(&logfile, true)?;
        }
        file.write_all(text)
    }
}

impl<S: Subscriber> Layer<S> for RollingLog {
    fn on_event(&self, event: &Event<'_>, _: Context<'_, S>) {
        let metadata = event.metadata();
        if metadata.target() != TARGET || metadata.name().starts_with("event ") {
            return;
        }
        let mut line = format!(
            "{} {:5} {} {}",
            now().as_secs(),
            metadata.level(),
            std::process::id(),
            metadata.name()
        );
        event.record(&mut Redact(&mut line));
        line.push('\n');
        if line.len() <= EVENT_BYTES {
            let _ = self.append(line.as_bytes());
        }
    }
}

/// Allowlist typed fields. In particular, never call Display or Debug on
/// errors, and discard strings entirely, including tracing's message field.
struct Redact<'a>(&'a mut String);

impl Visit for Redact<'_> {
    fn record_debug(&mut self, _: &Field, _: &dyn fmt::Debug) {}

    fn record_u64(&mut self, field: &Field, value: u64) {
        if matches!(field.name(), "batches" | "retry_ms") {
            let _ = write!(self.0, " {}={value}", field.name());
        }
    }

    fn record_bool(&mut self, field: &Field, value: bool) {
        if matches!(field.name(), "changed" | "write_back" | "web_view") {
            let _ = write!(self.0, " {}={value}", field.name());
        }
    }

    fn record_error(&mut self, field: &Field, error: &(dyn std::error::Error + 'static)) {
        if field.name() == "error" {
            let _ = write!(self.0, " error={}", error_kind(error));
        }
    }
}

fn error_kind(error: &(dyn std::error::Error + 'static)) -> &'static str {
    use crate::Error;
    if let Some(error) = error.downcast_ref::<Error>() {
        match error {
            Error::Store(_) => "sqlite",
            Error::Backend(_) => "backend",
            Error::Keyring(_) => "keyring",
            Error::Tls(_) => "tls",
            Error::SmtpCredentialsRejected(_) => "credentials-rejected",
            Error::SendUncertain(_) => "send-uncertain",
            Error::Io(_) => "io",
            _ => "mailbox",
        }
    } else if error.is::<io::Error>() {
        "io"
    } else {
        "redacted"
    }
}

fn append_recent_log(report: &mut String, paths: &Paths) {
    let mut lines = VecDeque::with_capacity(RECENT_LINES);
    // Read bounded tails in reverse generation order. A log you can't read
    // must not prevent the panic hook from publishing the backtrace.
    for generation in 0..LOG_FILES {
        let Ok(mut file) = read_private(&log_file(&paths.logs(), generation)) else {
            continue;
        };
        let mut contents = Vec::new();
        let read = (|| -> io::Result<()> {
            let start = file.metadata()?.len().saturating_sub(LOG_TAIL_BYTES);
            file.seek(SeekFrom::Start(start))?;
            file.take(LOG_TAIL_BYTES).read_to_end(&mut contents)?;
            if start > 0 {
                let first_newline = contents.iter().position(|&byte| byte == b'\n');
                contents.drain(..first_newline.map_or(contents.len(), |index| index + 1));
            }
            Ok(())
        })();
        if read.is_ok() {
            for line in contents
                .strip_suffix(b"\n")
                .unwrap_or(&contents)
                .rsplit(|&byte| byte == b'\n')
            {
                if lines.len() == RECENT_LINES {
                    break;
                }
                if !line.is_empty() {
                    lines.push_front(String::from_utf8_lossy(line).into_owned());
                }
            }
        }
        if lines.len() == RECENT_LINES {
            break;
        }
    }
    for line in lines {
        let _ = writeln!(report, "{line}");
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const SECRETS: [&str; 5] = [
        "pass-example-81234",
        "token-example-39520",
        "A body with unstructured private text",
        "Confidential quarterly subject",
        "somebody@example.test",
    ];

    #[test]
    fn info_log_never_formats_mail_or_error_text() {
        let dir = tempfile::tempdir().unwrap();
        let paths = Paths(dir.path().join("fell"));
        let directory = paths.directory("logs").unwrap();
        let subscriber = Registry::default()
            .with(LevelFilter::INFO)
            .with(RollingLog {
                directory,
                limit: LOG_BYTES,
            });
        let [password, token, body, subject, address] = SECRETS;
        let err = crate::Error::Backend(SECRETS.join(" "));
        let error = &err as &dyn std::error::Error;
        tracing::subscriber::with_default(subscriber, || {
            diagnostic!(
                INFO,
                "Private input was discarded",
                password = password,
                token = token,
                body = body,
                subject = subject,
                address = address,
                error = error
            );
            tracing::event!(name: "Display input was discarded", target: TARGET, tracing::Level::INFO,
                error = %err, message = ?err);
            tracing::info!(%err, "A dependency printed an error");
            diagnostic!(DEBUG, "Debug is disabled by default");
        });
        let log = fs::read_to_string(paths.logs().join("fell.log")).unwrap();
        assert!(log.contains("Private input was discarded error=backend"));
        assert!(log.contains("Display input was discarded"));
        assert!(!log.contains("A dependency printed an error"));
        assert!(!log.contains("Debug is disabled by default"));
        for secret in SECRETS {
            assert!(!log.contains(secret));
        }
        let report = write_crash(&paths, true).unwrap();
        let crash = fs::read_to_string(report).unwrap();
        assert!(crash.contains("Private input was discarded error=backend"));
        for secret in SECRETS {
            assert!(!crash.contains(secret));
        }
    }

    #[test]
    fn log_rolls_four_backups_and_reads_the_newest_two_hundred_lines_in_order() {
        let dir = tempfile::tempdir().unwrap();
        let paths = Paths(dir.path().join("fell"));
        let directory = paths.directory("logs").unwrap();
        let writer = RollingLog {
            directory,
            limit: 500,
        };
        for number in 0..725 {
            writer.append(format!("{number:04}\n").as_bytes()).unwrap();
        }
        assert_eq!(fs::read_dir(paths.logs()).unwrap().count(), LOG_FILES + 1);
        for generation in 0..LOG_FILES {
            let path = log_file(&paths.logs(), generation);
            let meta = fs::metadata(path).unwrap();
            assert_eq!(meta.mode() & 0o777, 0o600);
            assert!(meta.len() <= writer.limit);
        }
        let mut recent = String::new();
        append_recent_log(&mut recent, &paths);
        let parsed: Vec<usize> = recent.lines().map(|line| line.parse().unwrap()).collect();
        assert_eq!(parsed, (525..725).collect::<Vec<_>>());
    }
}
