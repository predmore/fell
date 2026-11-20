//! Received files: use the MIME part cache for opens and copy bytes for saves.

use std::fs::{self, File, OpenOptions};
use std::io::{self, Read};
use std::os::unix::fs::{MetadataExt, OpenOptionsExt};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::{Arc, Mutex};

use fell_core::parts::{CachedPart, PartCache, PartEvent, PartFetcher, PartJob, PartRequest};
use fell_core::{AccountConfig, MimePart, PartRole};
use futures::channel::mpsc::{UnboundedReceiver, unbounded};

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Target {
    pub message: String,
    pub part: MimePart,
}

impl Target {
    pub fn name(&self) -> String {
        name(&self.part)
    }
}

pub fn name(part: &MimePart) -> String {
    fell_core::parts::sanitize_filename(part.filename.as_deref(), &part.mime_type)
}

pub fn visible_parts(parts: Vec<MimePart>) -> Vec<MimePart> {
    parts
        .into_iter()
        .filter(|part| part.role == PartRole::Attachment)
        .collect()
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Request {
    Open(Target),
    Save(Vec<Target>),
}

#[derive(Clone, Copy)]
pub struct Download {
    pub fetched: u64,
    pub total: u64,
    pub started: std::time::Instant,
}

impl Download {
    pub fn label(self) -> String {
        if self.total == 0 {
            size_label(self.fetched)
        } else {
            format!(
                "{}%",
                self.fetched
                    .saturating_mul(100)
                    .div_euclid(self.total)
                    .min(100)
            )
        }
    }
}

pub fn size_label(bytes: u64) -> String {
    if bytes >= 1024 * 1024 {
        format!("{:.1} MiB", bytes as f64 / (1024.0 * 1024.0))
    } else if bytes >= 1024 {
        format!("{:.1} KiB", bytes as f64 / 1024.0)
    } else {
        format!("{bytes} B")
    }
}

/// One fetch lane per window, initialized on the background executor.
#[derive(Clone)]
pub struct Fetcher {
    config: Option<AccountConfig>,
    worker: Arc<Mutex<Option<PartFetcher>>>,
}

type FetchEvents = (UnboundedReceiver<PartEvent>, CancelFetch);

impl Fetcher {
    pub fn new(config: Option<AccountConfig>) -> Self {
        Self {
            config,
            worker: Arc::new(Mutex::new(None)),
        }
    }

    pub fn queue(&self, request: PartRequest) -> Result<FetchEvents, String> {
        let mut worker = self.worker.lock().expect("attachment fetcher lock");
        if worker.is_none() {
            let cache =
                PartCache::open_default().map_err(|err| format!("Attachment cache: {err}"))?;
            let config = self.config.clone();
            let account = request.account_id.clone();
            *worker = Some(PartFetcher::start(
                move || fell_core::backend_for(&account, config.as_ref()),
                cache,
            ));
        }
        let (send, receive) = unbounded();
        let job = worker
            .as_ref()
            .expect("initialized worker")
            .fetch(request, move |event| {
                let _ = send.unbounded_send(event);
            });
        Ok((receive, CancelFetch(job)))
    }
}

/// Dropping the receiving task closes its IMAP session after the current window.
pub struct CancelFetch(PartJob);

impl Drop for CancelFetch {
    fn drop(&mut self) {
        self.0.cancel();
    }
}

pub fn downloads_dir() -> io::Result<PathBuf> {
    gtk::glib::user_special_dir(gtk::glib::UserDirectory::Downloads)
        .or_else(|| {
            std::env::var_os("HOME")
                .filter(|home| !home.is_empty())
                .map(|home| PathBuf::from(home).join("Downloads"))
        })
        .filter(|dir| dir.is_absolute())
        .ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, "Downloads folder is unavailable"))
}

/// Copy into a private temporary file in Downloads, then publish it without
/// replacing any file or symlink, even when another save races the name.
pub fn save_to(cached: &CachedPart, part: &MimePart, dir: &Path) -> io::Result<PathBuf> {
    let mut input = open_cache_file(cached)?.take(cached.size.saturating_add(1));
    fs::create_dir_all(dir)?;
    let mut tmp = tempfile::Builder::new()
        .prefix(".fell-download-")
        .tempfile_in(dir)?;
    if io::copy(&mut input, tmp.as_file_mut())? != cached.size {
        return Err(io::Error::other("the cached file changed"));
    }
    tmp.as_file().sync_all()?;
    let name = name(part);
    for copy in 0.. {
        let path = dir.join(collision_name(&name, copy));
        match tmp.persist_noclobber(&path) {
            Ok(_) => return Ok(path),
            Err(err) if err.error.kind() == io::ErrorKind::AlreadyExists => tmp = err.file,
            Err(err) => return Err(err.error),
        }
    }
    unreachable!()
}

fn collision_name(name: &str, copy: usize) -> String {
    if copy == 0 {
        return name.to_string();
    }
    match name.rsplit_once('.') {
        Some((stem, ext)) => format!("{stem} ({copy}).{ext}"),
        None => format!("{name} ({copy})"),
    }
}

fn open_cache_file(cached: &CachedPart) -> io::Result<File> {
    let file = OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK)
        .open(&cached.path)?;
    let meta = file.metadata()?;
    if !meta.is_file() || meta.len() != cached.size {
        return Err(io::Error::other("the cached file changed"));
    }
    Ok(file)
}

pub fn read_cached(cached: &CachedPart) -> io::Result<Vec<u8>> {
    let mut bytes = Vec::new();
    open_cache_file(cached)?
        .take(cached.size.saturating_add(1))
        .read_to_end(&mut bytes)?;
    if bytes.len() as u64 != cached.size {
        return Err(io::Error::other("the cached file changed"));
    }
    Ok(bytes)
}

pub enum Opened {
    Confirm,
    Launched(Child),
}

pub fn open_cached(cached: &CachedPart, part: &MimePart, confirmed: bool) -> io::Result<Opened> {
    if !cached.path.is_absolute() {
        return Err(io::Error::other("the cache path is not absolute"));
    }
    let file = open_cache_file(cached)?;
    let mode = file.metadata()?.mode();
    let mut prefix = Vec::new();
    file.take(8192).read_to_end(&mut prefix)?;
    if !confirmed && (mode & 0o111 != 0 || executable(part, &prefix)) {
        return Ok(Opened::Confirm);
    }

    // xdg-open receives exactly one absolute file path. Its argument is
    // never parsed by a shell or interpreted as a URL or a flag by Fell.
    Command::new("xdg-open")
        .arg(&cached.path)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .map(Opened::Launched)
}

fn executable(part: &MimePart, prefix: &[u8]) -> bool {
    let mime = part.mime_type.to_ascii_lowercase();
    let name = name(part).to_ascii_lowercase();
    let extension = name.rsplit_once('.').map(|(_, ext)| ext);
    matches!(
        extension,
        Some(
            "desktop"
                | "exe"
                | "com"
                | "bat"
                | "cmd"
                | "msi"
                | "scr"
                | "ps1"
                | "sh"
                | "bash"
                | "zsh"
                | "fish"
                | "ksh"
                | "run"
                | "bin"
                | "appimage"
                | "jar"
        )
    ) || matches!(
        mime.as_str(),
        "application/x-desktop"
            | "application/x-executable"
            | "application/x-pie-executable"
            | "application/x-sharedlib"
            | "application/x-elf"
            | "application/x-mach-binary"
            | "application/x-shellscript"
            | "text/x-shellscript"
            | "application/x-msdownload"
            | "application/vnd.microsoft.portable-executable"
            | "application/x-msdos-program"
            | "application/x-iso9660-appimage"
            | "application/x-appimage"
            | "application/java-archive"
    ) || matches!(
        prefix,
        [b'#', b'!', ..]
            | [0x7f, b'E', b'L', b'F', ..]
            | [b'M', b'Z', ..]
            | [0xfe, 0xed, 0xfa, 0xce | 0xcf, ..]
            | [0xce | 0xcf, 0xfa, 0xed, 0xfe, ..]
            | [0xca, 0xfe, 0xba, 0xbe | 0xbf, ..]
    ) || String::from_utf8_lossy(prefix)
        .lines()
        .any(|line| line.trim_start_matches('\u{feff}').trim() == "[Desktop Entry]")
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::symlink;

    fn part(filename: &str, mime_type: &str) -> MimePart {
        MimePart {
            section: "2".into(),
            role: PartRole::Attachment,
            mime_type: mime_type.into(),
            filename: Some(filename.into()),
            size: 3,
            encoded_size: 3,
            encoding: fell_core::TransferEncoding::Identity,
            content_id: None,
            disposition: fell_core::Disposition::Attachment,
            nested: false,
        }
    }

    #[test]
    fn chips_use_only_the_attachment_role() {
        let attachment = part("logo.png", "image/png");
        let mut inline = attachment.clone();
        inline.role = PartRole::Inline;
        let mut signature = part("signature.asc", "application/pgp-signature");
        signature.role = PartRole::Signature;
        assert_eq!(
            visible_parts(vec![inline, attachment.clone(), signature]),
            vec![attachment],
        );
    }

    #[test]
    fn saves_sanitize_the_name_and_treat_existing_files_and_symlinks_as_collisions() {
        let root = tempfile::tempdir().unwrap();
        let cached = CachedPart {
            path: root.path().join("cached"),
            size: 3,
        };
        fs::write(&cached.path, b"new").unwrap();
        let part = part(".. /Budget\\2026.pdf", "application/pdf");
        let filename = name(&part);
        assert_eq!(filename, "_Budget_2026.pdf");
        let dir = root.path().join("Downloads");
        fs::create_dir(&dir).unwrap();
        fs::write(dir.join(&filename), b"previous").unwrap();
        let link = dir.join("_Budget_2026 (1).pdf");
        symlink(dir.join("missing"), &link).unwrap();

        let saved = save_to(&cached, &part, &dir).unwrap();
        assert_eq!(saved, dir.join("_Budget_2026 (2).pdf"));
        assert_eq!(fs::read(&saved).unwrap(), b"new");
        assert_eq!(fs::read(dir.join(&filename)).unwrap(), b"previous");
        assert!(!dir.join("missing").exists());
        assert!(link.is_symlink());
        assert_eq!(saved.metadata().unwrap().mode() & 0o777, 0o600);
        assert_eq!(collision_name("README", 3), "README (3)");
        assert_eq!(dir.read_dir().unwrap().count(), 3);
    }

    #[test]
    fn concurrent_saves_advance_to_distinct_available_names() {
        let root = tempfile::tempdir().unwrap();
        let dir = root.path().join("Downloads");
        let cached = CachedPart {
            path: root.path().join("cached"),
            size: 3,
        };
        fs::write(&cached.path, b"new").unwrap();
        let part = part("notes.pdf", "application/pdf");
        let (first, second) = std::thread::scope(|scope| {
            let first = scope.spawn(|| save_to(&cached, &part, &dir).unwrap());
            let second = scope.spawn(|| save_to(&cached, &part, &dir).unwrap());
            (first.join().unwrap(), second.join().unwrap())
        });
        let mut names = [first, second];
        names.sort();
        assert_eq!(names, [dir.join("notes (1).pdf"), dir.join("notes.pdf")]);
    }

    #[test]
    fn opening_checks_names_types_and_bytes_before_launching() {
        let root = tempfile::tempdir().unwrap();
        let mut cached = CachedPart {
            path: root.path().join("input"),
            size: 0,
        };
        for (name, mime, bytes) in [
            (
                "Launcher.DESKTOP",
                "application/octet-stream",
                b"".as_slice(),
            ),
            ("Notes.txt", "application/x-executable", b""),
            ("Minutes.pdf", "application/pdf", b"#!/bin/sh\n"),
            ("Summary", "application/octet-stream", b"\x7fELF"),
            (
                "Schedule.txt",
                "text/plain",
                b"\xef\xbb\xbf[Desktop Entry]\nType=Application\n",
            ),
        ] {
            fs::write(&cached.path, bytes).unwrap();
            cached.size = bytes.len() as u64;
            assert!(matches!(
                open_cached(&cached, &part(name, mime), false).unwrap(),
                Opened::Confirm
            ));
        }
        assert!(!executable(
            &part("notes.pdf", "application/pdf"),
            b"%PDF-1.7"
        ));
    }
}
