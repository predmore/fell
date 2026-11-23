//! Shared file diagnostics for the inbox and the MCP process.

use std::cell::RefCell;
use std::cmp::Reverse;
use std::collections::VecDeque;
use std::env;
use std::fmt::{self, Write as _};
use std::fs::{self, DirBuilder, File, OpenOptions, Permissions};
use std::io::{self, Read as _, Seek as _, SeekFrom, Write as _};
use std::os::unix::fs::{DirBuilderExt, MetadataExt, OpenOptionsExt, PermissionsExt};
use std::panic::PanicHookInfo;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Once, OnceLock};
use std::time::{SystemTime, UNIX_EPOCH};

use backtrace::Backtrace;
use time::{OffsetDateTime, UtcOffset, format_description::well_known::Rfc3339};
#[doc(hidden)]
pub use tracing;
use tracing::field::{Field, Visit};
use tracing::{Event, Subscriber};
use tracing_subscriber::layer::{Context, Layer, SubscriberExt};
use tracing_subscriber::{Registry, filter::LevelFilter};

const TARGET: &str = "fell.diagnostics";
const LOG_FILES: usize = 5;
const CRASH_FILES: usize = 10;
const LOG_BYTES: u64 = 5 * 1024 * 1024;
const EVENT_BYTES: usize = 1024;
const RECENT_LINES: usize = 200;
const LOG_TAIL_BYTES: u64 = ((RECENT_LINES + 1) * EVENT_BYTES) as u64;

static PATHS: OnceLock<Paths> = OnceLock::new();
static PANIC_HOOK: Once = Once::new();
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
        let mut builder = DirBuilder::new();
        builder.recursive(true).mode(0o700);
        for private in [&self.0, &dir] {
            builder.create(private)?;
            let handle = OpenOptions::new()
                .read(true)
                .custom_flags(libc::O_DIRECTORY | libc::O_NOFOLLOW)
                .open(private)?;
            let mode = handle.metadata()?.mode() & 0o7777;
            if mode != 0o700 {
                handle.set_permissions(Permissions::from_mode(0o700))?;
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
                is_crash_file(&entry).then(|| (key, entry.path()))
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
    PANIC_HOOK.call_once(|| std::panic::set_hook(Box::new(handle_panic)));
    let paths = Paths::from_env()?;
    if PATHS.set(paths.clone()).is_err() {
        return Ok(());
    }
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

fn handle_panic(info: &PanicHookInfo<'_>) {
    let mut panic = Some(Crash::capture(info));
    let _ = WORKER_PANIC.try_with(|slot| {
        if let Some(worker) = slot.borrow_mut().as_mut() {
            *worker = panic.take();
        }
    });
    if let Some(panic) = panic {
        record_crash(panic, false);
    }
}

thread_local! {
    // The outer option marks an active catch scope, the inner holds its panic.
    static WORKER_PANIC: RefCell<Option<Option<Crash>>> = const { RefCell::new(None) };
}

/// Mark a panic in this scope as recoverable for the hook. The caller owns
/// the recovery and can report it to whichever UI started the worker.
pub fn catch_worker<T>(work: impl FnOnce() -> T) -> std::thread::Result<T> {
    let previous = WORKER_PANIC.replace(Some(None));
    let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(work));
    // Work locals have unwound. In particular, SQLite has released any
    // write transaction before we resolve symbols and touch the filesystem.
    let panic = WORKER_PANIC.replace(previous).flatten();
    if let (Err(_), Some(panic)) = (&result, panic) {
        record_crash(panic, true);
    }
    result
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
    let path = dir.join(name);
    read_private(&path).ok()?;
    Some(path)
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

struct Crash {
    thread: &'static str,
    id: std::thread::ThreadId,
    location: String,
    backtrace: Backtrace,
}

impl Crash {
    fn capture(info: &PanicHookInfo<'_>) -> Self {
        let current = std::thread::current();
        let thread = match current.name() {
            Some("main") => "main",
            Some("fell-sync") => "fell-sync",
            Some("fell-store") => "fell-store",
            Some("fell-store-background") => "fell-store-background",
            _ => "other",
        };
        let location = info.location().map_or_else(
            || "unknown location".into(),
            |location| {
                let file = location.file();
                let name = Path::new(file)
                    .file_name()
                    .and_then(|name| name.to_str())
                    .unwrap_or("unknown");
                format!("{name}:{}", location.line())
            },
        );
        Self {
            thread,
            id: current.id(),
            location,
            backtrace: Backtrace::new_unresolved(),
        }
    }
}

fn record_crash(mut panic: Crash, recovered: bool) {
    match PATHS
        .get()
        .and_then(|paths| write_crash(paths, &mut panic, recovered).ok())
    {
        Some(path) if !recovered => {
            // Name the report relative to Fell's state directory: the full
            // path contains the user's home directory, and this line is
            // likely to be pasted into an issue.
            let name = path
                .file_name()
                .map(|name| name.to_string_lossy().into_owned())
                .unwrap_or_default();
            let _ = writeln!(
                io::stderr(),
                "{}",
                fatal_line(panic.thread, &panic.location, &name)
            );
        }
        None => {
            let _ = writeln!(
                io::stderr(),
                "Fell panicked on {} at {}. Could not write a crash report.",
                panic.thread,
                panic.location
            );
        }
        Some(_) => {}
    }
}

fn fatal_line(thread: &str, location: &str, report: &str) -> String {
    format!(
        "Fell panicked on {thread} at {location}. Crash report: crashes/{report} \
         in Fell's state directory (`fell --report` bundles it)."
    )
}

fn write_crash(paths: &Paths, panic: &mut Crash, recovered: bool) -> io::Result<PathBuf> {
    let dir = paths.directory("crashes")?;
    let mut report = format!(
        "{}\nThread: {} ({:?})\nLocation: {}\nRecovered by worker: {recovered}\n\nBacktrace\n",
        fell_version::VERSION,
        panic.thread,
        panic.id,
        panic.location
    );
    // Backtrace symbols identify the code. Source paths can contain the
    // user's home directory, and panic payloads can contain message data.
    panic.backtrace.resolve();
    for (index, frame) in panic.backtrace.frames().iter().enumerate() {
        for symbol in frame.symbols() {
            if let Some(name) = symbol.name() {
                let _ = writeln!(report, "{index:4}: {name}");
            }
        }
    }
    report.push_str("\nRecent log\n");
    append_recent_log(&mut report, paths);
    // Both Fell and Fell MCP can publish here. Serializing the publish and
    // prune keeps another writer's staging file from looking abandoned.
    let lock = private_file(&dir.join(".lock"), false)?;
    lock.lock()?;
    let name = format!("{}.txt", report_name());
    let path = dir.join(&name);
    let written = publish(&path, report.as_bytes());
    if written.is_ok() && !recovered {
        let _ = publish(&dir.join("pending"), name.as_bytes());
    }
    prune_crashes(&dir, &path);
    written.map(|()| path)
}

fn is_crash_file(entry: &fs::DirEntry) -> bool {
    entry
        .metadata()
        .is_ok_and(|meta| meta.is_file() && meta.nlink() == 1 && meta.mode() & 0o7777 == 0o600)
}

fn prune_crashes(dir: &Path, published: &Path) {
    let Ok(entries) = fs::read_dir(dir) else {
        return;
    };
    // Keep the last uncaught crash until the next inbox launch can show it.
    let mut pending = String::new();
    if read_private(&dir.join("pending"))
        .and_then(|file| file.take(128).read_to_string(&mut pending))
        .is_err()
    {
        pending.clear();
    }
    let mut reports = Vec::new();
    let mut keep = CRASH_FILES;
    for entry in entries.flatten().filter(is_crash_file) {
        let name = entry.file_name();
        let Some(name) = name.to_str() else { continue };
        if let Some(key) = crash_key(name) {
            let path = entry.path();
            if path == published || name == pending {
                keep -= 1;
            } else {
                reports.push((key, path));
            }
        } else if name.ends_with(".part") {
            let _ = fs::remove_file(entry.path());
        }
    }
    reports.sort_unstable_by_key(|(key, _)| Reverse(*key));
    for (_, path) in reports.into_iter().skip(keep) {
        let _ = fs::remove_file(path);
    }
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
            log_timestamp(),
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

fn log_timestamp() -> String {
    let now = OffsetDateTime::now_utc();
    let now = now.replace_nanosecond(0).unwrap_or(now);
    let local = UtcOffset::local_offset_at(now)
        .ok()
        .and_then(|offset| now.checked_to_offset(offset))
        .unwrap_or(now);
    local
        .format(&Rfc3339)
        .or_else(|_| now.format(&Rfc3339))
        .unwrap_or_default()
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
            Error::DraftBodyUnavailable => "draft-body-unavailable",
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

    #[test]
    fn the_fatal_line_names_the_report_without_a_home_path() {
        let line = fatal_line("main", "compose.rs:120", "1730000000000000000-4242-0.txt");
        assert_eq!(
            line,
            "Fell panicked on main at compose.rs:120. Crash report: \
             crashes/1730000000000000000-4242-0.txt in Fell's state directory \
             (`fell --report` bundles it)."
        );
        assert!(!line.contains("/home"));
        assert!(!line.contains(".local"));
    }

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
        let stamp = log.split_ascii_whitespace().next().unwrap();
        assert_eq!(&stamp[10..11], "T");
        assert!(stamp.ends_with('Z') || matches!(stamp.as_bytes().get(19), Some(b'+' | b'-')));
        let report = write_crash(&paths, &mut synthetic_crash(), true).unwrap();
        let crash = fs::read_to_string(report).unwrap();
        assert!(crash.contains("Private input was discarded error=backend"));
        for secret in SECRETS {
            assert!(!crash.contains(secret));
        }
    }

    fn synthetic_crash() -> Crash {
        Crash {
            thread: "other",
            id: std::thread::current().id(),
            location: "diagnostics.rs:1".into(),
            backtrace: Backtrace::from(Vec::new()),
        }
    }

    #[test]
    fn crash_retention_preserves_the_pending_report_and_skips_other_files() {
        let dir = tempfile::tempdir().unwrap();
        let paths = Paths(dir.path().join("fell"));
        let crashes = paths.directory("crashes").unwrap();
        let create = |name: &str| private_file(&crashes.join(name), true).unwrap();
        for serial in 0..12 {
            create(&format!("1-1-{serial}.txt"));
        }
        create("orphan.part");
        let public = crashes.join("1-3-4.txt");
        fs::write(&public, b"not a private report").unwrap();
        fs::set_permissions(&public, Permissions::from_mode(0o644)).unwrap();
        std::os::unix::fs::symlink("1-1-0.txt", crashes.join("symlink.part")).unwrap();
        fs::create_dir(crashes.join("dir.part")).unwrap();
        create("pending").write_all(b"1-1-0.txt").unwrap();
        let report = write_crash(&paths, &mut synthetic_crash(), true).unwrap();
        assert_eq!(paths.latest_crash().unwrap(), report);
        assert!(crashes.join("1-1-0.txt").is_file(), "pending report");
        for serial in 1..4 {
            assert!(!crashes.join(format!("1-1-{serial}.txt")).exists());
        }
        for serial in 4..12 {
            assert!(crashes.join(format!("1-1-{serial}.txt")).is_file());
        }
        assert!(!crashes.join("orphan.part").exists());
        assert!(crashes.join("symlink.part").is_symlink());
        assert!(crashes.join("dir.part").is_dir());
        assert!(public.is_file());
    }

    #[test]
    fn existing_diagnostics_directories_become_private() {
        let dir = tempfile::tempdir().unwrap();
        let paths = Paths(dir.path().join("fell"));
        let logs = paths.logs();
        fs::create_dir_all(&logs).unwrap();
        fs::set_permissions(&paths.0, Permissions::from_mode(0o755)).unwrap();
        fs::set_permissions(&logs, Permissions::from_mode(0o775)).unwrap();
        paths.directory("logs").unwrap();
        for directory in [&paths.0, &logs] {
            assert_eq!(fs::metadata(directory).unwrap().mode() & 0o777, 0o700);
        }
    }

    #[test]
    fn worker_releases_sqlite_write_lock_before_publishing_crash() {
        // Use a fresh process so the panic hook and state path are isolated
        // from the other tests in this binary.
        const CHILD: &str = "FELL_TEST_DIAGNOSTICS_WORKER";
        if env::var_os(CHILD).is_none() {
            let dir = tempfile::tempdir().unwrap();
            let output = std::process::Command::new(env::current_exe().unwrap())
                .args([
                    "--exact",
                    "diagnostics::tests::worker_releases_sqlite_write_lock_before_publishing_crash",
                    "--nocapture",
                ])
                .env(CHILD, "1")
                .env("XDG_STATE_HOME", dir.path())
                .output()
                .unwrap();
            assert!(
                output.status.success(),
                "{}\n{}",
                String::from_utf8_lossy(&output.stdout),
                String::from_utf8_lossy(&output.stderr)
            );
            return;
        }

        use rusqlite::{Connection, TransactionBehavior};
        use std::{sync::mpsc, time::Duration};

        init().unwrap();
        let paths = PATHS.get().unwrap();
        let crashes = paths.directory("crashes").unwrap();
        let db = paths.0.join("test.db");
        let mut reader = Connection::open(&db).unwrap();
        reader.execute("CREATE TABLE test (value)", []).unwrap();
        let lock = private_file(&crashes.join(".lock"), false).unwrap();
        lock.lock().unwrap();
        let (dropped, unwound) = mpsc::channel::<()>();
        let worker = std::thread::spawn(move || {
            // Drop order rolls back the write before dropping the sender.
            assert!(
                catch_worker(move || {
                    let _dropped = dropped;
                    let mut conn = Connection::open(db).unwrap();
                    let tx = conn
                        .transaction_with_behavior(TransactionBehavior::Immediate)
                        .unwrap();
                    tx.execute("INSERT INTO test VALUES (1)", []).unwrap();
                    panic!("synthetic interrupted write");
                })
                .is_err()
            );
        });
        let unwind_result = unwound.recv_timeout(Duration::from_secs(10));
        let report_before_unlock = paths.latest_crash();
        let transaction = reader.transaction_with_behavior(TransactionBehavior::Immediate);
        drop(lock);
        worker.join().unwrap();
        assert_eq!(unwind_result, Err(mpsc::RecvTimeoutError::Disconnected));
        assert!(report_before_unlock.is_err());
        let tx = transaction.unwrap();
        assert_eq!(
            tx.query_row("SELECT count(*) FROM test", [], |row| row.get::<_, i64>(0))
                .unwrap(),
            0
        );
        let report = fs::read_to_string(paths.latest_crash().unwrap()).unwrap();
        assert!(report.contains("Recovered by worker: true"));
        assert!(!report.contains("synthetic interrupted write"));
        assert!(take_crash_notice().is_none());
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
