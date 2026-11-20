//! Attachment bytes, fetched on demand and kept in a private cache.
//!
//! Sync lists each message's MIME parts ([`crate::MimePart`]) but stores no
//! attachment bytes. [`fetch_part`] reads one part from a server copy by its
//! IMAP section, undoes the transfer encoding, and writes the bytes under
//! `$XDG_CACHE_HOME/fell/parts` (else `~/.cache/fell/parts`). [`PartFetcher`]
//! does that on its own thread with its own short-lived IMAP session, so a
//! large part neither waits for nor holds up a sync pass, and the window
//! never waits on the network.
//!
//! Every name here comes from untrusted mail. A cached file is named by a
//! hash directory and [`sanitize_filename`]; nothing follows a
//! `Content-Location`, and nothing is opened or executed.

use std::fs;
use std::io::{self, Write};
use std::ops::ControlFlow;
use std::os::unix::fs::{MetadataExt, OpenOptionsExt};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, mpsc};
use std::time::{Duration, SystemTime};

use sha2::{Digest, Sha256};

use crate::account::AccountId;
use crate::backend::MailBackend;
use crate::embed::model_files::{
    O_NOFOLLOW, create_private_dir, create_private_dirs, create_private_file, user_id,
};
use crate::message::{MessageId, MimePart, PartRole, RemoteId, TransferEncoding};
use crate::{Error, Result};

/// The cache keeps at most this many bytes by default; the least recently
/// used parts go first.
pub const DEFAULT_CACHE_BYTES: u64 = 1024 * 1024 * 1024;

/// Longest cached file name, in bytes. The extension is kept.
const MAX_FILE_NAME: usize = 120;

/// A partial write older than this was left by a process that stopped.
const STALE_PARTIAL: Duration = Duration::from_secs(60 * 60);

/// One part to fetch: which message, which part, and the server copies that
/// hold it, best first. From [`crate::Store::part_request`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PartRequest {
    pub account_id: AccountId,
    /// The stored row's Message-ID.
    pub message_id: MessageId,
    pub part: MimePart,
    pub copies: Vec<RemoteId>,
}

/// A part's decoded bytes on disk.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CachedPart {
    /// Owner-only (0600) file named after the part. Its contents are untrusted.
    pub path: PathBuf,
    pub size: u64,
}

/// Owner-only directory of fetched parts, trimmed to a byte cap.
#[derive(Debug, Clone)]
pub struct PartCache {
    root: PathBuf,
    cap: u64,
}

impl PartCache {
    /// `$XDG_CACHE_HOME/fell/parts`, else `~/.cache/fell/parts`. Inside this
    /// crate's own tests it is `None`, so tests never touch a real cache.
    pub fn default_root() -> Option<PathBuf> {
        #[cfg(test)]
        {
            None
        }
        #[cfg(not(test))]
        {
            cache_root_from(
                std::env::var_os("XDG_CACHE_HOME").as_deref(),
                std::env::var_os("HOME").as_deref(),
            )
        }
    }

    /// Open (creating it, 0700, without following symlinks) the cache at
    /// `root`, holding at most `cap` bytes.
    pub fn open(root: impl Into<PathBuf>, cap: u64) -> io::Result<Self> {
        let root = root.into();
        create_private_dirs(&root)?;
        Ok(Self { root, cap })
    }

    /// The cache at [`PartCache::default_root`] with [`DEFAULT_CACHE_BYTES`].
    pub fn open_default() -> io::Result<Self> {
        let root = Self::default_root().ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::NotFound,
                "no cache directory (HOME is unset)",
            )
        })?;
        Self::open(root, DEFAULT_CACHE_BYTES)
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    fn entry_dir(&self, request: &PartRequest) -> PathBuf {
        let mut hash = Sha256::new();
        for field in [
            request.account_id.as_str(),
            request.message_id.as_str(),
            request.part.section.as_str(),
        ] {
            hash.update(field.as_bytes());
            hash.update([0]);
        }
        let digest = hash.finalize();
        let key: String = digest[..16]
            .iter()
            .map(|byte| format!("{byte:02x}"))
            .collect();
        self.root.join(key)
    }

    fn entry_path(&self, request: &PartRequest) -> PathBuf {
        self.entry_dir(request).join(sanitize_filename(
            request.part.filename.as_deref(),
            &request.part.mime_type,
        ))
    }

    /// The cached copy of `request`, when there is one. Marks it used. A
    /// file that is not the part's listed size (cut short, or changed on
    /// disk) is removed and counts as a miss.
    pub fn lookup(&self, request: &PartRequest) -> Option<CachedPart> {
        let dir = self.entry_dir(request);
        if !fs::symlink_metadata(&dir).is_ok_and(|meta| meta.is_dir()) {
            return None;
        }
        let path = self.entry_path(request);
        let file = fs::OpenOptions::new()
            .read(true)
            .custom_flags(O_NOFOLLOW | libc::O_NONBLOCK)
            .open(&path)
            .ok()?;
        let meta = file.metadata().ok()?;
        if !meta.is_file() || meta.uid() != user_id() {
            return None;
        }
        if meta.len() != request.part.size {
            drop(file);
            let _ = fs::remove_file(&path);
            return None;
        }
        let _ = file.set_modified(SystemTime::now());
        Some(CachedPart {
            path,
            size: meta.len(),
        })
    }

    /// Write `bytes` as the cached copy of `request`, then trim the cache.
    fn store(&self, request: &PartRequest, bytes: &[u8]) -> io::Result<CachedPart> {
        let dir = self.entry_dir(request);
        create_private_dir(&dir)?;
        let path = self.entry_path(request);
        let nanos = SystemTime::now()
            .duration_since(SystemTime::UNIX_EPOCH)
            .map_or(0, |elapsed| elapsed.as_nanos());
        let partial = dir.join(format!(".partial-{}-{nanos}", std::process::id()));
        let written = (|| {
            let mut file = create_private_file(&partial)?;
            file.write_all(bytes)?;
            file.sync_all()?;
            fs::rename(&partial, &path)
        })();
        if let Err(err) = written {
            let _ = fs::remove_file(&partial);
            return Err(err);
        }
        self.trim(&dir);
        Ok(CachedPart {
            path,
            size: bytes.len() as u64,
        })
    }

    /// Remove the least recently used entries until the cache is under its
    /// cap. `keep` (the entry just written) stays even when it alone is over.
    fn trim(&self, keep: &Path) {
        let Ok(entries) = fs::read_dir(&self.root) else {
            return;
        };
        let mut found: Vec<(SystemTime, u64, PathBuf)> = Vec::new();
        let mut total = 0u64;
        for entry in entries.flatten() {
            let dir = entry.path();
            let is_key = entry.file_name().to_str().is_some_and(|name| {
                name.len() == 32 && name.bytes().all(|b| b.is_ascii_hexdigit())
            });
            if !is_key || !fs::symlink_metadata(&dir).is_ok_and(|meta| meta.is_dir()) {
                continue;
            }
            let (used, size) = entry_usage(&dir);
            total += size;
            if dir != keep {
                found.push((used, size, dir));
            }
        }
        if total <= self.cap {
            return;
        }
        found.sort_by_key(|(used, _, _)| *used);
        for (_, size, dir) in found {
            if total <= self.cap {
                break;
            }
            if fs::remove_dir_all(&dir).is_ok() {
                total = total.saturating_sub(size);
            }
        }
    }
}

/// When an entry was last used and how many bytes it holds. Stale partial
/// writes are removed on the way.
fn entry_usage(dir: &Path) -> (SystemTime, u64) {
    let mut used = SystemTime::UNIX_EPOCH;
    let mut size = 0;
    let Ok(files) = fs::read_dir(dir) else {
        return (used, size);
    };
    for file in files.flatten() {
        let Ok(meta) = file.metadata() else {
            continue;
        };
        let modified = meta.modified().unwrap_or(SystemTime::UNIX_EPOCH);
        let partial = file.file_name().to_string_lossy().starts_with(".partial-");
        if partial
            && modified
                .elapsed()
                .is_ok_and(|elapsed| elapsed > STALE_PARTIAL)
        {
            let _ = fs::remove_file(file.path());
            continue;
        }
        size += meta.len();
        used = used.max(modified);
    }
    (used, size)
}

#[cfg_attr(test, allow(dead_code))]
fn cache_root_from(
    xdg_cache: Option<&std::ffi::OsStr>,
    home: Option<&std::ffi::OsStr>,
) -> Option<PathBuf> {
    if let Some(cache) = xdg_cache.filter(|dir| !dir.is_empty()) {
        return Some(PathBuf::from(cache).join("fell/parts"));
    }
    home.filter(|dir| !dir.is_empty())
        .map(|home| PathBuf::from(home).join(".cache/fell/parts"))
}

/// A file name for a part, safe to create in a directory: no path
/// separators, control or bidi-override characters, no leading dot, at
/// most 120 bytes with the extension kept. A part with no usable name is
/// `attachment`, or `message.eml` for an attached message.
pub fn sanitize_filename(name: Option<&str>, mime_type: &str) -> String {
    let fallback = if mime_type.eq_ignore_ascii_case("message/rfc822") {
        "message.eml"
    } else {
        "attachment"
    };
    let cleaned: String = name
        .unwrap_or("")
        .chars()
        .filter(|ch| !crate::imap::is_bidi_control(*ch))
        .map(|ch| {
            if ch.is_control() || "/\\:*?\"<>|".contains(ch) {
                '_'
            } else {
                ch
            }
        })
        .collect();
    let cleaned = cleaned.split_whitespace().collect::<Vec<_>>().join(" ");
    let cleaned = cleaned
        .trim_start_matches(|ch: char| ch == '.' || ch.is_whitespace())
        .trim_end_matches(|ch: char| ch == '.' || ch.is_whitespace());
    if cleaned.is_empty() {
        return fallback.to_string();
    }
    if cleaned.len() <= MAX_FILE_NAME {
        return cleaned.to_string();
    }
    let (stem, extension) = match cleaned.rsplit_once('.') {
        Some((stem, ext)) if !stem.is_empty() && ext.len() <= 16 => (stem, Some(ext)),
        _ => (cleaned, None),
    };
    let room = MAX_FILE_NAME - extension.map_or(0, |ext| ext.len() + 1);
    let mut end = stem.len().min(room);
    while !stem.is_char_boundary(end) {
        end -= 1;
    }
    let stem = stem[..end].trim_end();
    match extension {
        Some(ext) => format!("{stem}.{ext}"),
        None => stem.to_string(),
    }
}

/// Undo a part's transfer encoding. Malformed input decodes best-effort, as
/// sync's parser reads it.
pub fn decode_part(encoding: TransferEncoding, raw: &[u8]) -> Vec<u8> {
    let encoding = match encoding {
        TransferEncoding::Identity => mail_parser::Encoding::None,
        TransferEncoding::QuotedPrintable => mail_parser::Encoding::QuotedPrintable,
        TransferEncoding::Base64 => mail_parser::Encoding::Base64,
    };
    encoding.decode(raw).into_owned()
}

/// The decoded bytes of one part, from the cache or else from the first
/// server copy that still has it at the listed size, cached on the way. Blocks on the network:
/// call it from [`PartFetcher`] or another background thread, never the UI
/// thread. `progress` hears encoded bytes read so far (compare with
/// [`MimePart::encoded_size`]).
pub fn fetch_part(
    backend: &dyn MailBackend,
    cache: &PartCache,
    request: &PartRequest,
    progress: &mut dyn FnMut(u64),
) -> Result<CachedPart> {
    if let Some(hit) = cache.lookup(request) {
        return Ok(hit);
    }
    fetch_uncached_part(backend, cache, request, &mut |fetched| {
        progress(fetched);
        ControlFlow::Continue(())
    })
}

/// [`fetch_part`], stopped (and its session closed) when `progress`
/// answers `Break`.
fn fetch_uncached_part(
    backend: &dyn MailBackend,
    cache: &PartCache,
    request: &PartRequest,
    progress: &mut dyn FnMut(u64) -> ControlFlow<()>,
) -> Result<CachedPart> {
    let mut differed = false;
    for copy in &request.copies {
        let Some(raw) = backend.fetch_part(
            copy,
            &request.part.section,
            request.part.encoded_size,
            progress,
        )?
        else {
            continue;
        };
        let bytes = decode_part(request.part.encoding, &raw);
        drop(raw);
        // A copy merged from another delivery (a list echo with a footer)
        // may number its parts differently. Its part of another size is
        // some other part: never hand that over as this one.
        if bytes.len() as u64 != request.part.size {
            differed = true;
            continue;
        }
        return cache
            .store(request, &bytes)
            .map_err(|err| Error::Backend(format!("part cache: {err}")));
    }
    Err(Error::Backend(if differed {
        "the server's copy of this message no longer has this part where it was listed".into()
    } else {
        "no server copy of this message has the part any more".into()
    }))
}

/// Fetch the images an HTML body shows by `cid:` (role
/// [`PartRole::Inline`]) for one opened message, so it renders with them.
/// Parts already cached are not fetched again. Returns the cached parts with
/// their Content-IDs.
pub fn prefetch_inline_parts(
    backend: &dyn MailBackend,
    cache: &PartCache,
    requests: &[PartRequest],
) -> Result<Vec<(String, CachedPart)>> {
    let mut out = Vec::new();
    for request in requests {
        let Some(cid) = &request.part.content_id else {
            continue;
        };
        if request.part.role != PartRole::Inline || !request.part.mime_type.starts_with("image/") {
            continue;
        }
        let cached = fetch_part(backend, cache, request, &mut |_| {})?;
        out.push((cid.clone(), cached));
    }
    Ok(out)
}

/// What a [`PartFetcher`] reports for one request.
#[derive(Debug)]
pub enum PartEvent {
    /// The part missed the cache and is about to be requested from the server.
    Downloading {
        total: u64,
    },
    /// Encoded bytes read so far, out of the part's encoded size.
    Progress {
        fetched: u64,
        total: u64,
    },
    Done(Result<CachedPart>),
}

type Events = Box<dyn FnMut(PartEvent) + Send>;
type Job = (PartRequest, Arc<AtomicBool>, Events);

/// A background thread that fetches parts one at a time, each over its own
/// short-lived read-only IMAP session. [`PartFetcher::fetch`] returns at
/// once, so it is safe to call from the UI; a sync pass on another
/// connection carries on meanwhile.
///
/// Dropping it cancels every job and waits for the thread: queued jobs end
/// at once, and the one running stops after the window it is reading, then
/// closes its session.
pub struct PartFetcher {
    jobs: Option<mpsc::Sender<Job>>,
    stopping: Arc<AtomicBool>,
    thread: Option<std::thread::JoinHandle<()>>,
}

/// One queued fetch. [`PartJob::cancel`] stops it: before it starts, it
/// never connects; while it runs, it stops after the current window and
/// closes its session. Either way `Done` reports an error.
#[derive(Debug, Clone)]
pub struct PartJob {
    cancelled: Arc<AtomicBool>,
}

impl PartJob {
    pub fn cancel(&self) {
        self.cancelled.store(true, Ordering::Relaxed);
    }
}

fn cancelled() -> Error {
    Error::Backend(crate::imap::PART_FETCH_CANCELLED.into())
}

impl PartFetcher {
    /// `backend` makes a backend for each fetch. It may resolve a password,
    /// so it runs on the fetch thread.
    pub fn start<F>(backend: F, cache: PartCache) -> Self
    where
        F: Fn() -> Result<Box<dyn MailBackend + Send>> + Send + 'static,
    {
        let (jobs, queue) = mpsc::channel::<Job>();
        let stopping = Arc::new(AtomicBool::new(false));
        let stop = Arc::clone(&stopping);
        let thread = std::thread::Builder::new()
            .name("fell-parts".into())
            .spawn(move || {
                for (request, job, mut events) in queue {
                    let stopped = || job.load(Ordering::Relaxed) || stop.load(Ordering::Relaxed);
                    if stopped() {
                        events(PartEvent::Done(Err(cancelled())));
                        continue;
                    }
                    // Cached files are usable offline, including when the
                    // configured password command is unavailable.
                    if let Some(hit) = cache.lookup(&request) {
                        events(PartEvent::Done(Ok(hit)));
                        continue;
                    }
                    let total = request.part.encoded_size;
                    let result = backend().and_then(|backend| {
                        if !request.copies.is_empty() {
                            events(PartEvent::Downloading { total });
                        }
                        fetch_uncached_part(backend.as_ref(), &cache, &request, &mut |fetched| {
                            events(PartEvent::Progress { fetched, total });
                            if stopped() {
                                ControlFlow::Break(())
                            } else {
                                ControlFlow::Continue(())
                            }
                        })
                    });
                    events(PartEvent::Done(result));
                }
            })
            .expect("spawn part fetch thread");
        Self {
            jobs: Some(jobs),
            stopping,
            thread: Some(thread),
        }
    }

    /// Queue `request`. `events` runs on the fetch thread: progress after
    /// each window, then `Done` once.
    pub fn fetch(
        &self,
        request: PartRequest,
        events: impl FnMut(PartEvent) + Send + 'static,
    ) -> PartJob {
        let job = PartJob {
            cancelled: Arc::new(AtomicBool::new(false)),
        };
        let sent = match &self.jobs {
            Some(jobs) => jobs
                .send((request, Arc::clone(&job.cancelled), Box::new(events)))
                .map_err(|mpsc::SendError((_, _, events))| events),
            None => Err(Box::new(events) as Events),
        };
        if let Err(mut events) = sent {
            events(PartEvent::Done(Err(Error::Backend(
                "the part fetch thread stopped".into(),
            ))));
        }
        job
    }
}

impl Drop for PartFetcher {
    fn drop(&mut self) {
        self.stopping.store(true, Ordering::Relaxed);
        drop(self.jobs.take());
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::PermissionsExt;

    use crate::message::Disposition;

    pub(crate) fn temp_root(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "fell-parts-{tag}-{}-{:?}",
            std::process::id(),
            std::thread::current().id()
        ));
        let _ = fs::remove_dir_all(&dir);
        dir
    }

    pub(crate) fn request(message_id: &str, section: &str, name: Option<&str>) -> PartRequest {
        PartRequest {
            account_id: AccountId("a1".into()),
            message_id: MessageId::new(message_id),
            part: MimePart {
                section: section.into(),
                role: PartRole::Attachment,
                mime_type: "application/pdf".into(),
                filename: name.map(str::to_string),
                size: 4,
                encoded_size: 8,
                encoding: TransferEncoding::Base64,
                content_id: None,
                disposition: Disposition::Attachment,
                nested: false,
            },
            copies: vec![RemoteId("imap:1:1:INBOX".into())],
        }
    }

    fn mode(path: &Path) -> u32 {
        fs::symlink_metadata(path).unwrap().permissions().mode() & 0o777
    }

    #[test]
    fn names_from_mail_are_made_safe() {
        let cases = [
            (Some("report.pdf"), "report.pdf"),
            (
                Some("../../.ssh/authorized_keys"),
                "_.._.ssh_authorized_keys",
            ),
            (Some(".bashrc"), "bashrc"),
            (Some("a\\b:c*d?e\"f<g>h|i"), "a_b_c_d_e_f_g_h_i"),
            (Some("invoice\u{202e}fdp.exe"), "invoicefdp.exe"),
            (Some("line\nbreak\u{0}.txt"), "line_break_.txt"),
            (Some("  spaced   out  .pdf. "), "spaced out .pdf"),
            (Some("..."), "attachment"),
            (Some(""), "attachment"),
            (None, "attachment"),
        ];
        for (name, expected) in cases {
            assert_eq!(
                sanitize_filename(name, "application/pdf"),
                expected,
                "{name:?}"
            );
        }
        assert_eq!(sanitize_filename(None, "message/rfc822"), "message.eml");
        let long = format!("{}.pdf", "é".repeat(200));
        let short = sanitize_filename(Some(&long), "application/pdf");
        assert!(short.len() <= MAX_FILE_NAME, "{}", short.len());
        assert!(short.ends_with("é.pdf"), "{short}");
    }

    #[test]
    fn the_cache_root_follows_xdg_then_home() {
        use std::ffi::OsStr;
        assert_eq!(
            cache_root_from(Some(OsStr::new("/x/cache")), Some(OsStr::new("/h"))),
            Some(PathBuf::from("/x/cache/fell/parts"))
        );
        assert_eq!(
            cache_root_from(Some(OsStr::new("")), Some(OsStr::new("/h"))),
            Some(PathBuf::from("/h/.cache/fell/parts"))
        );
        assert_eq!(cache_root_from(None, None), None);
        assert_eq!(
            PartCache::default_root(),
            None,
            "tests never use a real cache"
        );
    }

    #[test]
    fn a_cached_part_does_not_resolve_backend_credentials_or_report_download_progress() {
        let root = temp_root("offline-hit");
        let cache = PartCache::open(&root, DEFAULT_CACHE_BYTES).unwrap();
        let request = request("msg@x", "2", Some("report.pdf"));
        let cached = cache.store(&request, b"data").unwrap();
        let fetcher = PartFetcher::start(
            || Err(Error::Backend("credentials unavailable".into())),
            cache,
        );
        let (send, receive) = mpsc::channel();
        fetcher.fetch(request, move |event| {
            let _ = send.send(event);
        });
        match receive
            .recv_timeout(std::time::Duration::from_secs(5))
            .unwrap()
        {
            PartEvent::Done(Ok(hit)) => assert_eq!(hit, cached),
            other => panic!("expected an offline cache hit, got {other:?}"),
        }
        drop(fetcher);
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn cached_parts_are_owner_only_and_named_for_the_part() {
        let root = temp_root("modes");
        let cache = PartCache::open(root.join("fell/parts"), DEFAULT_CACHE_BYTES).unwrap();
        assert_eq!(mode(cache.root()), 0o700);
        let wanted = request("m@x", "2", Some("../report.pdf"));
        assert!(cache.lookup(&wanted).is_none());
        let stored = cache.store(&wanted, b"%PDF").unwrap();
        assert_eq!(stored.path.file_name().unwrap(), "_report.pdf");
        assert_eq!(
            stored.path.parent().unwrap().parent().unwrap(),
            cache.root()
        );
        assert_eq!(mode(&stored.path), 0o600);
        assert_eq!(mode(stored.path.parent().unwrap()), 0o700);
        assert_eq!(fs::read(&stored.path).unwrap(), b"%PDF");
        assert_eq!(cache.lookup(&wanted), Some(stored.clone()));
        // Another section or message is another entry.
        assert!(
            cache
                .lookup(&request("m@x", "3", Some("../report.pdf")))
                .is_none()
        );
        assert!(
            cache
                .lookup(&request("n@x", "2", Some("../report.pdf")))
                .is_none()
        );
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn a_symlinked_root_or_entry_is_not_used() {
        let root = temp_root("links");
        let elsewhere = root.join("elsewhere");
        fs::create_dir_all(&elsewhere).unwrap();
        fs::set_permissions(&elsewhere, fs::Permissions::from_mode(0o755)).unwrap();
        let link = root.join("parts");
        std::os::unix::fs::symlink(&elsewhere, &link).unwrap();
        assert!(PartCache::open(&link, DEFAULT_CACHE_BYTES).is_err());
        assert_eq!(mode(&elsewhere), 0o755, "not chmodded through the link");

        let cache = PartCache::open(root.join("real"), DEFAULT_CACHE_BYTES).unwrap();
        let wanted = request("m@x", "2", Some("a.pdf"));
        let stored = cache.store(&wanted, b"%PDF").unwrap();
        // A file swapped for a symlink is a miss, never read through.
        fs::remove_file(&stored.path).unwrap();
        std::os::unix::fs::symlink("/etc/passwd", &stored.path).unwrap();
        assert!(cache.lookup(&wanted).is_none());
        // So is an entry directory swapped for a symlink.
        let dir = stored.path.parent().unwrap().to_path_buf();
        fs::remove_dir_all(&dir).unwrap();
        std::os::unix::fs::symlink(&elsewhere, &dir).unwrap();
        fs::write(elsewhere.join("a.pdf"), b"planted").unwrap();
        assert!(cache.lookup(&wanted).is_none());
        assert!(cache.store(&wanted, b"%PDF").is_err());
        assert_eq!(fs::read(elsewhere.join("a.pdf")).unwrap(), b"planted");
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn a_cached_file_of_the_wrong_size_is_a_miss_and_is_refetched() {
        struct Serves(&'static [u8]);
        impl MailBackend for Serves {
            fn kind(&self) -> crate::BackendKind {
                crate::BackendKind::Imap
            }
            fn fetch_changes(
                &self,
                _cursor: Option<&crate::backend::SyncCursor>,
            ) -> Result<crate::backend::MailboxDelta> {
                unreachable!()
            }
            fn fetch_part(
                &self,
                _copy: &RemoteId,
                _section: &str,
                _encoded_size: u64,
                _progress: &mut dyn FnMut(u64) -> ControlFlow<()>,
            ) -> Result<Option<Vec<u8>>> {
                Ok(Some(self.0.to_vec()))
            }
            fn submit_draft(&self, _draft: crate::Draft) -> Result<crate::PendingSend> {
                unreachable!()
            }
            fn confirm_send(
                &self,
                _pending: crate::PendingSend,
                _confirmation: crate::UserSendConfirmation,
            ) -> Result<crate::SentMessage> {
                unreachable!()
            }
        }
        let root = temp_root("size");
        let cache = PartCache::open(&root, DEFAULT_CACHE_BYTES).unwrap();
        let wanted = request("m@x", "2", Some("a.pdf"));
        let stored = cache.store(&wanted, b"%PDF").unwrap();
        // Cut short on disk (a crash, a full disk, a hand edit).
        fs::write(&stored.path, b"%P").unwrap();
        assert!(cache.lookup(&wanted).is_none());
        assert!(!stored.path.exists(), "the short file is removed");
        fs::write(&stored.path, b"%PDF-and-more").unwrap();
        let backend = Serves(b"JVBERg==");
        let fetched = fetch_part(&backend, &cache, &wanted, &mut |_| {}).unwrap();
        assert_eq!(fs::read(&fetched.path).unwrap(), b"%PDF");
        assert_eq!(fetched.size, 4);
        assert_eq!(cache.lookup(&wanted), Some(fetched));
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn the_least_recently_used_parts_go_past_the_cap() {
        let root = temp_root("lru");
        let cache = PartCache::open(&root, 250).unwrap();
        let sized = |id: &str, name: &str, size: u64| {
            let mut wanted = request(id, "2", Some(name));
            wanted.part.size = size;
            wanted
        };
        let old = sized("old@x", "old.bin", 100);
        let used = sized("used@x", "used.bin", 100);
        let new = sized("new@x", "new.bin", 100);
        let old_at = cache.store(&old, &[1; 100]).unwrap();
        let used_at = cache.store(&used, &[2; 100]).unwrap();
        let past = SystemTime::now() - Duration::from_secs(600);
        for path in [&old_at.path, &used_at.path] {
            fs::OpenOptions::new()
                .write(true)
                .open(path)
                .unwrap()
                .set_modified(past)
                .unwrap();
        }
        // Reading marks it used.
        assert!(cache.lookup(&used).is_some());
        cache.store(&new, &[3; 100]).unwrap();
        assert!(cache.lookup(&old).is_none(), "oldest went");
        assert!(cache.lookup(&used).is_some());
        assert!(cache.lookup(&new).is_some());
        // One part larger than the whole cap is still kept, alone.
        let huge = sized("huge@x", "huge.bin", 300);
        cache.store(&huge, &[4; 300]).unwrap();
        assert!(cache.lookup(&huge).is_some());
        assert!(cache.lookup(&used).is_none() && cache.lookup(&new).is_none());
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn decoding_undoes_each_transfer_encoding() {
        assert_eq!(
            decode_part(TransferEncoding::Base64, b"JVBE\r\nRg=="),
            b"%PDF"
        );
        assert_eq!(
            decode_part(TransferEncoding::QuotedPrintable, b"a=3Db=\r\nc=C3=A9"),
            "a=bcé".as_bytes()
        );
        assert_eq!(
            decode_part(TransferEncoding::Identity, b"raw\r\n"),
            b"raw\r\n"
        );
    }
}
