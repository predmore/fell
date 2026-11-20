//! `add`: the MCP config entry for each harness, and merging it into the
//! harness's own config file.
//!
//! The entry is named `fell` and holds only the command that starts this
//! server. It never carries an environment, an API key, or a token: Fell
//! does not see model keys. Scopes live in the mailbox's grant table, not in
//! the harness config, so a config entry cannot widen what the agent may do.
//!
//! Writing keeps every other entry, keeps a backup of the old file, refuses
//! symlinks, and refuses files it cannot parse exactly (JSON with comments,
//! for instance). Those cases fall back to printing the snippet.

use std::ffi::{OsStr, OsString};
use std::fs::{File, Metadata};
use std::io::{Read, Write};
use std::os::fd::OwnedFd;
use std::os::unix::fs::MetadataExt;
use std::path::{Component, Path, PathBuf};

use rustix::fs::{self, AtFlags, CWD, FlockOperation, Mode, OFlags, RenameFlags};
use rustix::io::Errno;

use saphyr::{LoadableYamlNode, YamlOwned};
use serde_json::{Map, Value, json};

/// The key of Fell's entry in every harness config.
pub(crate) const ENTRY: &str = "fell";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Harness {
    ClaudeCode,
    Codex,
    Gemini,
    OpenCode,
    Goose,
}

impl Harness {
    pub(crate) const ALL: [Harness; 5] = [
        Harness::ClaudeCode,
        Harness::Codex,
        Harness::Gemini,
        Harness::OpenCode,
        Harness::Goose,
    ];

    pub(crate) fn parse(name: &str) -> Option<Self> {
        match name.to_ascii_lowercase().as_str() {
            "claude-code" | "claude" => Some(Self::ClaudeCode),
            "codex" => Some(Self::Codex),
            "gemini" | "gemini-cli" => Some(Self::Gemini),
            "opencode" => Some(Self::OpenCode),
            "goose" => Some(Self::Goose),
            _ => None,
        }
    }

    /// Command-line name, and the default agent id.
    pub(crate) fn id(self) -> &'static str {
        match self {
            Self::ClaudeCode => "claude-code",
            Self::Codex => "codex",
            Self::Gemini => "gemini",
            Self::OpenCode => "opencode",
            Self::Goose => "goose",
        }
    }

    pub(crate) fn title(self) -> &'static str {
        match self {
            Self::ClaudeCode => "Claude Code",
            Self::Codex => "Codex",
            Self::Gemini => "Gemini CLI",
            Self::OpenCode => "OpenCode",
            Self::Goose => "Goose",
        }
    }

    /// The user-level config file the harness reads MCP servers from, when
    /// Fell may edit it.
    ///
    /// - Codex: `~/.codex/config.toml` (`$CODEX_HOME`), `[mcp_servers.NAME]`.
    /// - Gemini CLI: `~/.gemini/settings.json`, `mcpServers`.
    /// - OpenCode: `$OPENCODE_CONFIG_DIR/opencode.json`, else
    ///   `~/.config/opencode/opencode.json`, `mcp.servers` (a server under
    ///   `mcp.servers` wins over one of the same name directly under `mcp`,
    ///   OpenCode's older layout).
    /// - Goose: `~/.config/goose/config.yaml`, `extensions`.
    ///
    /// Claude Code is `None`. Its user-level servers live in `~/.claude.json`
    /// next to state Claude Code rewrites while it runs, so a write from here
    /// could be lost or clobber that state. Fell prints Claude Code's own
    /// `claude mcp add` command instead. The others are `None` only when
    /// home is unknown.
    pub(crate) fn config_path(self, env: &Env) -> Option<PathBuf> {
        let home = env.home.as_deref();
        let xdg_config = env
            .xdg_config
            .clone()
            .or_else(|| home.map(|home| home.join(".config")));
        match self {
            Self::ClaudeCode => None,
            Self::Codex => env
                .codex_home
                .clone()
                .or_else(|| home.map(|home| home.join(".codex")))
                .map(|dir| dir.join("config.toml")),
            Self::Gemini => home.map(|home| home.join(".gemini/settings.json")),
            Self::OpenCode => env
                .opencode_config_dir
                .clone()
                .or_else(|| xdg_config.map(|dir| dir.join("opencode")))
                .map(|dir| dir.join("opencode.json")),
            Self::Goose => xdg_config.map(|dir| dir.join("goose/config.yaml")),
        }
    }

    /// Whether Fell ever edits this harness's config file.
    pub(crate) fn edits_config(self) -> bool {
        self != Self::ClaudeCode
    }

    /// A file next to the config that, when present, means the harness reads
    /// a format Fell does not rewrite.
    fn shadowing_file(self, path: &Path) -> Option<PathBuf> {
        match self {
            Self::OpenCode => Some(path.with_extension("jsonc")),
            _ => None,
        }
    }
}

/// Environment that decides where configs live.
#[derive(Debug, Clone, Default)]
pub(crate) struct Env {
    pub home: Option<PathBuf>,
    pub xdg_config: Option<PathBuf>,
    pub codex_home: Option<PathBuf>,
    pub opencode_config_dir: Option<PathBuf>,
}

impl Env {
    pub(crate) fn from_process() -> Self {
        let dir = |name: &str| {
            std::env::var_os(name)
                .map(PathBuf::from)
                .filter(|path| path.is_absolute())
        };
        Self {
            home: dir("HOME"),
            xdg_config: dir("XDG_CONFIG_HOME"),
            codex_home: dir("CODEX_HOME"),
            opencode_config_dir: dir("OPENCODE_CONFIG_DIR"),
        }
    }

    /// `~/…` for paths under home, for messages. Snippets keep full paths.
    pub(crate) fn display(&self, path: &Path) -> String {
        if let Some(home) = &self.home
            && let Ok(rest) = path.strip_prefix(home)
        {
            return format!("~/{}", rest.display());
        }
        path.display().to_string()
    }
}

/// How the harness starts the server: a program and its full argv.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ServerSpec {
    pub program: String,
    pub args: Vec<String>,
}

/// The config text a harness needs, ready to paste.
pub(crate) fn snippet(harness: Harness, spec: &ServerSpec) -> String {
    match harness {
        Harness::ClaudeCode => {
            let mut words = vec![
                "claude".to_string(),
                "mcp".into(),
                "add".into(),
                "--scope".into(),
                "user".into(),
                ENTRY.into(),
                "--".into(),
                shell_word(&spec.program),
            ];
            words.extend(spec.args.iter().map(|arg| shell_word(arg)));
            format!("{}\n", words.join(" "))
        }
        Harness::Gemini => {
            json_text(&json!({ "mcpServers": { ENTRY: json_entry(harness, spec) } }))
        }
        Harness::OpenCode => {
            json_text(&json!({ "mcp": { OPENCODE_SERVERS: { ENTRY: json_entry(harness, spec) } } }))
        }
        Harness::Codex => toml_document(None, spec)
            .map(|(text, _)| text)
            .unwrap_or_default(),
        Harness::Goose => format!("extensions:\n{}", yaml_entry(spec, 2)),
    }
}

#[derive(Debug, PartialEq, Eq)]
pub(crate) enum Written {
    /// The file already had exactly this entry.
    Unchanged,
    Wrote {
        backup: Option<PathBuf>,
        replaced: bool,
        created: bool,
    },
}

/// Merge Fell's entry into the harness config at `path`. `Err` is a reason
/// for the person; nothing was written.
///
/// The path is walked from home (or `/` when it is elsewhere) one folder at
/// a time, never through a symlink, and every write goes through the final
/// folder's descriptor. The old file is locked from the read to the swap,
/// and the swap is refused when the file changed meanwhile (a harness saving
/// its own settings): Fell then starts over once with the new contents.
pub(crate) fn write_config(
    harness: Harness,
    path: &Path,
    spec: &ServerSpec,
    env: &Env,
    stamp: i64,
) -> Result<Written, String> {
    let shown = env.display(path);
    let Some(walk) = Walk::new(path, env) else {
        return Err(format!("{shown} is not a plain absolute path"));
    };
    for _ in 0..2 {
        if let Some(written) = write_once(harness, &walk, spec, env, stamp)? {
            return Ok(written);
        }
    }
    Err(format!(
        "{} rewrote {shown} while Fell was writing it; try again",
        harness.title()
    ))
}

/// A config path split for a walk that never follows symlinks.
struct Walk<'a> {
    path: &'a Path,
    /// Where the walk starts: home when the path is under it, else `/`.
    /// This one folder is opened normally (home may itself be a symlink, as
    /// on some distributions).
    root: PathBuf,
    /// Folders below `root`, each opened with `O_NOFOLLOW`.
    dirs: Vec<&'a OsStr>,
    name: &'a OsStr,
}

impl<'a> Walk<'a> {
    fn new(path: &'a Path, env: &Env) -> Option<Self> {
        if !path.is_absolute() {
            return None;
        }
        let (root, rest) = match env
            .home
            .as_deref()
            .and_then(|home| Some((home, path.strip_prefix(home).ok()?)))
        {
            Some((home, rest)) => (home.to_path_buf(), rest),
            None => (PathBuf::from("/"), path.strip_prefix("/").ok()?),
        };
        let mut dirs = Vec::new();
        for component in rest.components() {
            match component {
                Component::Normal(part) => dirs.push(part),
                Component::CurDir => {}
                _ => return None,
            }
        }
        let name = dirs.pop()?;
        Some(Self {
            path,
            root,
            dirs,
            name,
        })
    }

    /// The folder holding the config. Missing folders are created 0700 when
    /// `create` is set, else `Ok(None)`.
    fn open_dir(&self, create: bool, env: &Env) -> Result<Option<OwnedFd>, String> {
        let mut at = self.root.clone();
        let mut dir = fs::openat(
            CWD,
            &self.root,
            OFlags::RDONLY | OFlags::DIRECTORY | OFlags::CLOEXEC,
            Mode::empty(),
        )
        .map_err(|err| failed(&at, err, env))?;
        for part in &self.dirs {
            at.push(part);
            let next = match fs::openat(&dir, *part, DIR_FLAGS, Mode::empty()) {
                Ok(next) => next,
                Err(Errno::NOENT) if create => {
                    let made = match fs::mkdirat(&dir, *part, Mode::from_raw_mode(0o700)) {
                        Ok(()) => true,
                        Err(Errno::EXIST) => false,
                        Err(err) => return Err(failed(&at, err, env)),
                    };
                    let next = fs::openat(&dir, *part, DIR_FLAGS, Mode::empty())
                        .map_err(|err| refusal(&dir, part, &at, err, env))?;
                    if made {
                        // mkdir's mode is masked by the umask.
                        fs::fchmod(&next, Mode::from_raw_mode(0o700))
                            .map_err(|err| failed(&at, err, env))?;
                    }
                    next
                }
                Err(Errno::NOENT) => return Ok(None),
                Err(err) => return Err(refusal(&dir, part, &at, err, env)),
            };
            dir = next;
        }
        Ok(Some(dir))
    }
}

const DIR_FLAGS: OFlags = OFlags::RDONLY
    .union(OFlags::DIRECTORY)
    .union(OFlags::NOFOLLOW)
    .union(OFlags::CLOEXEC);

/// The config as Fell read it, locked until this is dropped.
struct Existing {
    _file: File,
    meta: Metadata,
    text: String,
}

/// One read, merge and swap. `Ok(None)`: the file changed under Fell.
fn write_once(
    harness: Harness,
    walk: &Walk<'_>,
    spec: &ServerSpec,
    env: &Env,
    stamp: i64,
) -> Result<Option<Written>, String> {
    let path = walk.path;
    let shown = env.display(path);
    let dir = walk.open_dir(false, env)?;
    if let (Some(dir), Some(other)) = (&dir, harness.shadowing_file(path))
        && let Some(other_name) = other.file_name()
        && lstat_at(dir, other_name).is_ok()
    {
        return Err(format!(
            "{} exists, and Fell does not rewrite JSON with comments",
            env.display(&other)
        ));
    }
    let existing = match &dir {
        Some(dir) => open_existing(dir, walk.name, path, env)?,
        None => None,
    };
    let merged = merge(
        harness,
        existing.as_ref().map(|old| old.text.as_str()),
        spec,
    )
    .map_err(|reason| format!("{shown} {reason}"))?;
    if merged.unchanged {
        return Ok(Some(Written::Unchanged));
    }
    let dir = match dir {
        Some(dir) => dir,
        None => walk
            .open_dir(true, env)?
            .ok_or_else(|| format!("{shown}: could not create its folder"))?,
    };
    let backup = match &existing {
        Some(old) => Some(write_backup(&dir, walk.name, &old.text, stamp).map_err(
            |(name, err)| format!("{}: {err}", env.display(&path.with_file_name(name))),
        )?),
        None => None,
    };
    let mode = existing
        .as_ref()
        .map_or(0o600, |old| old.meta.mode() & 0o777);
    let temp = suffixed(
        walk.name,
        &format!("fell-tmp-{}-{stamp}", std::process::id()),
    );
    let discard = |also_temp: bool| {
        if also_temp {
            let _ = fs::unlinkat(&dir, &temp, AtFlags::empty());
        }
        if let Some(backup) = &backup {
            let _ = fs::unlinkat(&dir, backup, AtFlags::empty());
        }
    };
    if let Err(err) = write_new_at(&dir, &temp, merged.text.as_bytes(), mode) {
        discard(true);
        return Err(format!("{shown}: {err}"));
    }
    #[cfg(test)]
    tests::before_swap(path);
    let swapped = match &existing {
        Some(old) => {
            if lstat_at(&dir, walk.name).is_ok_and(|now| same_version(&old.meta, &now)) {
                fs::renameat(&dir, &temp, &dir, walk.name).map(|()| true)
            } else {
                Ok(false)
            }
        }
        // A file that appeared meanwhile is never replaced.
        None => match fs::renameat_with(&dir, &temp, &dir, walk.name, RenameFlags::NOREPLACE) {
            Ok(()) => Ok(true),
            Err(Errno::EXIST) => Ok(false),
            // A filesystem without RENAME_NOREPLACE.
            Err(Errno::INVAL | Errno::NOSYS)
                if lstat_at(&dir, walk.name)
                    .is_err_and(|err| err.kind() == std::io::ErrorKind::NotFound) =>
            {
                fs::renameat(&dir, &temp, &dir, walk.name).map(|()| true)
            }
            Err(err) => Err(err),
        },
    };
    match swapped {
        Ok(true) => {}
        Ok(false) => {
            discard(true);
            return Ok(None);
        }
        Err(err) => {
            discard(true);
            return Err(failed(path, err, env));
        }
    }
    // Make the rename itself durable. The new file is already in place, so
    // a failure here is not a reason to report that nothing was written.
    let _ = fs::fsync(&dir);
    let created = existing.is_none();
    drop(existing);
    Ok(Some(Written::Wrote {
        backup: backup.map(|name| path.with_file_name(name)),
        replaced: merged.replaced,
        created,
    }))
}

/// The same file at the same version: same inode, size and change times.
fn same_version(before: &Metadata, now: &Metadata) -> bool {
    let key = |meta: &Metadata| {
        (
            meta.dev(),
            meta.ino(),
            meta.size(),
            meta.mtime(),
            meta.mtime_nsec(),
            meta.ctime(),
            meta.ctime_nsec(),
        )
    };
    key(before) == key(now)
}

/// Open, lock and read the config. `Ok(None)` when there is none yet.
fn open_existing(
    dir: &OwnedFd,
    name: &OsStr,
    path: &Path,
    env: &Env,
) -> Result<Option<Existing>, String> {
    let shown = env.display(path);
    let flags =
        OFlags::RDONLY | OFlags::NOFOLLOW | OFlags::CLOEXEC | OFlags::NONBLOCK | OFlags::NOCTTY;
    let fd = match fs::openat(dir, name, flags, Mode::empty()) {
        Ok(fd) => fd,
        Err(Errno::NOENT) => return Ok(None),
        Err(err) => return Err(refusal(dir, name, path, err, env)),
    };
    let mut file = File::from(fd);
    let meta = file.metadata().map_err(|err| format!("{shown}: {err}"))?;
    if !meta.is_file() {
        return Err(format!("{shown} is not a regular file"));
    }
    match fs::flock(&file, FlockOperation::NonBlockingLockExclusive) {
        Ok(()) => {}
        Err(Errno::WOULDBLOCK) => {
            return Err(format!("another program is writing {shown}; try again"));
        }
        Err(err) => return Err(failed(path, err, env)),
    }
    // The version the swap is checked against: read after the lock.
    let meta = file.metadata().map_err(|err| format!("{shown}: {err}"))?;
    let mut text = String::new();
    file.read_to_string(&mut text)
        .map_err(|err| format!("{shown}: {err}"))?;
    Ok(Some(Existing {
        _file: file,
        meta,
        text,
    }))
}

/// `<name>.fell-backup-<stamp>`, never overwriting an older backup.
fn write_backup(
    dir: &OwnedFd,
    name: &OsStr,
    text: &str,
    stamp: i64,
) -> Result<OsString, (OsString, std::io::Error)> {
    let mut n = 0;
    loop {
        let suffix = match n {
            0 => format!("fell-backup-{stamp}"),
            n => format!("fell-backup-{stamp}-{n}"),
        };
        let backup = suffixed(name, &suffix);
        match write_new_at(dir, &backup, text.as_bytes(), 0o600) {
            Ok(()) => return Ok(backup),
            Err(err) if err.kind() == std::io::ErrorKind::AlreadyExists && n < 100 => n += 1,
            Err(err) => return Err((backup, err)),
        }
    }
}

fn suffixed(name: &OsStr, suffix: &str) -> OsString {
    let mut name = name.to_os_string();
    name.push(".");
    name.push(suffix);
    name
}

/// Create `name` in `dir` with exactly `mode` (not masked by the umask),
/// write it and sync it.
fn write_new_at(dir: &OwnedFd, name: &OsStr, bytes: &[u8], mode: u32) -> std::io::Result<()> {
    let flags = OFlags::WRONLY | OFlags::CREATE | OFlags::EXCL | OFlags::NOFOLLOW | OFlags::CLOEXEC;
    let mut file = File::from(fs::openat(dir, name, flags, Mode::from_raw_mode(mode))?);
    file.write_all(bytes)?;
    fs::fchmod(&file, Mode::from_raw_mode(mode))?;
    file.sync_all()
}

/// `lstat` of `name` in `dir`.
fn lstat_at(dir: &OwnedFd, name: &OsStr) -> std::io::Result<Metadata> {
    let flags = OFlags::PATH | OFlags::NOFOLLOW | OFlags::CLOEXEC;
    File::from(fs::openat(dir, name, flags, Mode::empty())?).metadata()
}

/// Check a command path using directory descriptors from the filesystem root.
pub(crate) fn regular_file_without_symlinks(path: &Path) -> Option<Metadata> {
    let spelling = path.as_os_str().as_encoded_bytes();
    if spelling.ends_with(b"/") || spelling.ends_with(b"/.") {
        return None;
    }
    let mut components = path.strip_prefix("/").ok()?.components();
    let Component::Normal(name) = components.next_back()? else {
        return None;
    };
    let flags = OFlags::PATH | OFlags::DIRECTORY | OFlags::NOFOLLOW | OFlags::CLOEXEC;
    let mut dir = fs::openat(CWD, "/", flags, Mode::empty()).ok()?;
    for component in components {
        let Component::Normal(part) = component else {
            return None;
        };
        dir = fs::openat(&dir, part, flags, Mode::empty()).ok()?;
    }
    let metadata = lstat_at(&dir, name).ok()?;
    metadata.is_file().then_some(metadata)
}

/// Why `name` in `dir` (shown as `at`) could not be opened.
fn refusal(dir: &OwnedFd, name: &OsStr, at: &Path, err: Errno, env: &Env) -> String {
    let shown = env.display(at);
    match lstat_at(dir, name) {
        Ok(meta) if meta.file_type().is_symlink() => {
            format!("{shown} is a symlink, and Fell does not follow symlinks")
        }
        Ok(meta) if err == Errno::NOTDIR && !meta.is_dir() => format!("{shown} is not a folder"),
        _ => failed(at, err, env),
    }
}

fn failed(at: &Path, err: Errno, env: &Env) -> String {
    format!("{}: {}", env.display(at), std::io::Error::from(err))
}

#[derive(Debug)]
pub(crate) struct Merged {
    pub text: String,
    pub replaced: bool,
    pub unchanged: bool,
}

/// Merge into `existing` (absent or blank means a new file). `Err` reads as
/// the rest of a sentence that starts with the file name.
pub(crate) fn merge(
    harness: Harness,
    existing: Option<&str>,
    spec: &ServerSpec,
) -> Result<Merged, String> {
    let existing = existing.filter(|text| !text.trim().is_empty());
    match harness {
        Harness::ClaudeCode => Err("is not edited by Fell".into()),
        Harness::Gemini | Harness::OpenCode => merge_json(harness, existing, spec),
        Harness::Codex => merge_toml(existing, spec),
        Harness::Goose => merge_yaml(existing, spec),
    }
}

/// OpenCode keeps servers in `mcp.servers`, next to `mcp.timeout`.
const OPENCODE_SERVERS: &str = "servers";

/// One argument for a POSIX shell, quoted only when it needs to be.
pub(crate) fn shell_word(text: &str) -> String {
    let plain = !text.is_empty()
        && text
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || "-_./:=@%+,".contains(c));
    if plain {
        text.to_string()
    } else {
        format!("'{}'", text.replace('\'', "'\\''"))
    }
}

fn json_entry(harness: Harness, spec: &ServerSpec) -> Value {
    match harness {
        Harness::OpenCode => {
            let mut command = vec![spec.program.clone()];
            command.extend(spec.args.iter().cloned());
            // OpenCode's `disabled` defaults to false; there is no `enabled`.
            json!({ "type": "local", "command": command })
        }
        _ => json!({ "command": spec.program, "args": spec.args }),
    }
}

fn json_text(value: &Value) -> String {
    let mut text = serde_json::to_string_pretty(value).unwrap_or_default();
    text.push('\n');
    text
}

fn merge_json(
    harness: Harness,
    existing: Option<&str>,
    spec: &ServerSpec,
) -> Result<Merged, String> {
    let entry = json_entry(harness, spec);
    let mut root = match existing {
        None => {
            let mut root = Map::new();
            if harness == Harness::OpenCode {
                root.insert("$schema".into(), json!("https://opencode.ai/config.json"));
            }
            root
        }
        Some(text) => match serde_json::from_str::<Value>(text) {
            Ok(Value::Object(root)) => root,
            Ok(_) => return Err("is not a JSON object".into()),
            Err(_) => return Err("has comments or is not plain JSON".into()),
        },
    };
    let servers = match harness {
        Harness::OpenCode => {
            let mcp = root
                .entry("mcp")
                .or_insert_with(|| Value::Object(Map::new()));
            let Some(mcp) = mcp.as_object_mut() else {
                return Err("has an `mcp` that is not an object".into());
            };
            mcp.entry(OPENCODE_SERVERS)
                .or_insert_with(|| Value::Object(Map::new()))
                .as_object_mut()
                .ok_or_else(|| format!("has an `mcp.{OPENCODE_SERVERS}` that is not an object"))?
        }
        _ => root
            .entry("mcpServers")
            .or_insert_with(|| Value::Object(Map::new()))
            .as_object_mut()
            .ok_or_else(|| "has an `mcpServers` that is not an object".to_string())?,
    };
    let previous = servers.insert(ENTRY.into(), entry.clone());
    Ok(Merged {
        unchanged: previous.as_ref() == Some(&entry),
        replaced: previous.is_some(),
        text: json_text(&Value::Object(root)),
    })
}

/// The Codex document with Fell's table, and whether it replaced one.
fn toml_document(
    existing: Option<&str>,
    spec: &ServerSpec,
) -> Result<(String, Option<bool>), String> {
    use toml_edit::{Array, DocumentMut, Item, Table, value};
    let mut doc = match existing {
        None => DocumentMut::new(),
        Some(text) => text
            .parse::<DocumentMut>()
            .map_err(|_| "is not valid TOML".to_string())?,
    };
    let servers = doc.entry("mcp_servers").or_insert_with(|| {
        let mut table = Table::new();
        table.set_implicit(true);
        Item::Table(table)
    });
    let Some(servers) = servers.as_table_mut() else {
        return Err("has an `mcp_servers` that is not a [table]".into());
    };
    let same = servers.get(ENTRY).and_then(Item::as_table).map(|table| {
        let command = table.get("command").and_then(Item::as_str);
        let args: Option<Vec<&str>> = table
            .get("args")
            .and_then(Item::as_array)
            .map(|array| array.iter().filter_map(|value| value.as_str()).collect());
        table.len() == 2
            && command == Some(spec.program.as_str())
            && args.is_some_and(|args| args == spec.args)
    });
    if servers.get(ENTRY).is_some_and(|item| !item.is_table()) {
        return Err(format!(
            "has an `mcp_servers.{ENTRY}` that is not a [table]"
        ));
    }
    let mut table = Table::new();
    table.insert("command", value(spec.program.as_str()));
    let mut args = Array::from_iter(spec.args.iter().map(String::as_str));
    args.fmt();
    table.insert("args", value(args));
    servers.insert(ENTRY, Item::Table(table));
    Ok((doc.to_string(), same))
}

fn merge_toml(existing: Option<&str>, spec: &ServerSpec) -> Result<Merged, String> {
    let (text, same) = toml_document(existing, spec)?;
    Ok(Merged {
        text,
        replaced: same.is_some(),
        unchanged: same == Some(true),
    })
}

/// One string as a YAML double-quoted scalar. JSON's escapes are a subset of
/// YAML's, so any path or id survives.
fn yaml_string(text: &str) -> String {
    serde_json::to_string(text).unwrap_or_default()
}

/// Fell's Goose extension, as the block under `extensions:`, at `indent`.
fn yaml_entry(spec: &ServerSpec, indent: usize) -> String {
    let pad = " ".repeat(indent);
    let mut out = format!(
        "{pad}{ENTRY}:\n\
         {pad}  name: fell\n\
         {pad}  description: Fell mail (no send tool)\n\
         {pad}  type: stdio\n\
         {pad}  cmd: {}\n\
         {pad}  args:\n",
        yaml_string(&spec.program)
    );
    for arg in &spec.args {
        out.push_str(&format!("{pad}    - {}\n", yaml_string(arg)));
    }
    out.push_str(&format!(
        "{pad}  enabled: true\n{pad}  envs: {{}}\n{pad}  timeout: 300\n"
    ));
    out
}

fn yaml_docs(text: &str) -> Result<Vec<YamlOwned>, String> {
    YamlOwned::load_from_str(text).map_err(|_| "is not valid YAML".to_string())
}

fn yaml_get<'a>(node: &'a YamlOwned, key: &str) -> Option<&'a YamlOwned> {
    match node {
        YamlOwned::Mapping(map) => map
            .iter()
            .find(|(k, _)| k.as_str() == Some(key))
            .map(|(_, v)| v),
        _ => None,
    }
}

fn yaml_keys(node: &YamlOwned) -> Vec<&YamlOwned> {
    match node {
        YamlOwned::Mapping(map) => map.keys().collect(),
        _ => Vec::new(),
    }
}

fn indent_of(line: &str) -> usize {
    line.len() - line.trim_start_matches(' ').len()
}

fn is_key_line(line: &str, indent: usize, key: &str) -> bool {
    if indent_of(line) != indent {
        return false;
    }
    let Some(rest) = line[indent..].strip_prefix(key) else {
        return false;
    };
    let Some(rest) = rest.strip_prefix(':') else {
        return false;
    };
    let rest = rest.trim();
    rest.is_empty() || rest.starts_with('#')
}

fn is_blank_or_comment(line: &str) -> bool {
    let trimmed = line.trim();
    trimmed.is_empty() || trimmed.starts_with('#')
}

/// Goose rewrites `config.yaml` itself, but people edit it too. Fell edits
/// the lines of its own entry only, then parses the result and checks every
/// other key came through unchanged. Anything it cannot place that way (flow
/// style, anchors it would break, a quoted `fell` key) is refused.
fn merge_yaml(existing: Option<&str>, spec: &ServerSpec) -> Result<Merged, String> {
    let Some(text) = existing else {
        return Ok(Merged {
            text: snippet(Harness::Goose, spec),
            replaced: false,
            unchanged: false,
        });
    };
    let before = yaml_docs(text)?;
    if before.len() > 1 {
        return Err("has more than one YAML document".into());
    }
    let root = before.first();
    if let Some(root) = root
        && !matches!(root, YamlOwned::Mapping(_))
        && !root.is_null()
    {
        return Err("is not a YAML mapping".into());
    }
    let extensions = root.and_then(|root| yaml_get(root, "extensions"));
    if let Some(ext) = extensions
        && !matches!(ext, YamlOwned::Mapping(_))
        && !ext.is_null()
    {
        return Err("has an `extensions` that is not a mapping".into());
    }
    let previous = extensions.and_then(|ext| yaml_get(ext, ENTRY));
    let lines: Vec<&str> = text.lines().collect();
    if lines.iter().any(|line| line.contains('\t')) {
        return Err("uses tabs".into());
    }
    let header: Vec<usize> = (0..lines.len())
        .filter(|&i| is_key_line(lines[i], 0, "extensions"))
        .collect();
    let mut out: Vec<String> = Vec::new();
    match (extensions.is_some(), header.as_slice()) {
        (false, []) => {
            out.extend(lines.iter().map(|line| line.to_string()));
            out.push("extensions:".into());
            out.extend(yaml_entry(spec, 2).lines().map(str::to_string));
        }
        (true, [at]) => {
            let at = *at;
            let child = lines[at + 1..]
                .iter()
                .find(|line| !is_blank_or_comment(line))
                .map(|line| indent_of(line))
                .filter(|&indent| indent > 0)
                .unwrap_or(2);
            let block: Vec<String> = yaml_entry(spec, child)
                .lines()
                .map(str::to_string)
                .collect();
            out.extend(lines[..=at].iter().map(|line| line.to_string()));
            let rest = &lines[at + 1..];
            if previous.is_some() {
                let Some(start) = rest
                    .iter()
                    .take_while(|line| is_blank_or_comment(line) || indent_of(line) >= child)
                    .position(|line| is_key_line(line, child, ENTRY))
                else {
                    return Err(format!("has a `{ENTRY}` extension Fell cannot find"));
                };
                let end = rest[start + 1..]
                    .iter()
                    .position(|line| !line.trim().is_empty() && indent_of(line) <= child)
                    .map_or(rest.len(), |offset| start + 1 + offset);
                out.extend(rest[..start].iter().map(|line| line.to_string()));
                out.extend(block);
                out.extend(rest[end..].iter().map(|line| line.to_string()));
            } else {
                out.extend(block);
                out.extend(rest.iter().map(|line| line.to_string()));
            }
        }
        _ => return Err("has an `extensions` key Fell cannot edit safely".into()),
    }
    let mut merged = out.join("\n");
    merged.push('\n');
    let expected = yaml_docs(&snippet(Harness::Goose, spec))?;
    let expected_entry = expected
        .first()
        .and_then(|root| yaml_get(root, "extensions"))
        .and_then(|ext| yaml_get(ext, ENTRY))
        .cloned();
    if previous.is_some() && previous == expected_entry.as_ref() {
        return Ok(Merged {
            text: text.to_string(),
            replaced: true,
            unchanged: true,
        });
    }
    let after = yaml_docs(&merged).map_err(|_| "could not be merged safely".to_string())?;
    if !yaml_merge_kept_everything(root, after.first(), expected_entry.as_ref()) {
        return Err("could not be merged safely".into());
    }
    Ok(Merged {
        text: merged,
        replaced: previous.is_some(),
        unchanged: false,
    })
}

/// Every key but `extensions.fell` is unchanged, and that one is ours.
fn yaml_merge_kept_everything(
    before: Option<&YamlOwned>,
    after: Option<&YamlOwned>,
    entry: Option<&YamlOwned>,
) -> bool {
    let Some(after) = after else {
        return false;
    };
    let empty = YamlOwned::Mapping(Default::default());
    let before = before.filter(|root| !root.is_null()).unwrap_or(&empty);
    for key in yaml_keys(before) {
        let Some(name) = key.as_str() else {
            return false;
        };
        if name != "extensions" && yaml_get(before, name) != yaml_get(after, name) {
            return false;
        }
    }
    let before_ext = yaml_get(before, "extensions").filter(|ext| !ext.is_null());
    let Some(after_ext) = yaml_get(after, "extensions") else {
        return false;
    };
    let after_top: Vec<_> = yaml_keys(after)
        .into_iter()
        .filter_map(YamlOwned::as_str)
        .collect();
    let mut before_top: Vec<_> = yaml_keys(before)
        .into_iter()
        .filter_map(YamlOwned::as_str)
        .collect();
    if !before_top.contains(&"extensions") {
        before_top.push("extensions");
    }
    if after_top != before_top {
        return false;
    }
    if let Some(before_ext) = before_ext {
        for key in yaml_keys(before_ext) {
            let Some(name) = key.as_str() else {
                return false;
            };
            if name != ENTRY && yaml_get(before_ext, name) != yaml_get(after_ext, name) {
                return false;
            }
        }
        let expected_len = yaml_keys(before_ext)
            .into_iter()
            .filter(|key| key.as_str() != Some(ENTRY))
            .count()
            + 1;
        if yaml_keys(after_ext).len() != expected_len {
            return false;
        }
    } else if yaml_keys(after_ext).len() != 1 {
        return false;
    }
    entry.is_some() && yaml_get(after_ext, ENTRY) == entry
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use std::cell::RefCell;
    use std::os::unix::fs::PermissionsExt;
    use std::sync::atomic::{AtomicU64, Ordering};

    type SwapHook = Box<dyn FnMut(&Path)>;

    thread_local! {
        static BEFORE_SWAP: RefCell<Option<SwapHook>> = const { RefCell::new(None) };
    }

    /// Called by `write_once` after the new file is written and before it
    /// replaces the old one: where a harness saving its settings would race.
    pub(super) fn before_swap(path: &Path) {
        BEFORE_SWAP.with(|hook| {
            if let Some(hook) = hook.borrow_mut().as_mut() {
                hook(path);
            }
        });
    }

    /// Runs `hook` before every swap until dropped.
    struct OnSwap;

    impl OnSwap {
        fn new(hook: impl FnMut(&Path) + 'static) -> Self {
            BEFORE_SWAP.with(|slot| *slot.borrow_mut() = Some(Box::new(hook)));
            Self
        }
    }

    impl Drop for OnSwap {
        fn drop(&mut self) {
            BEFORE_SWAP.with(|slot| *slot.borrow_mut() = None);
        }
    }

    /// A fresh folder with no symlink anywhere in its path.
    pub(crate) struct TempDir(PathBuf);

    impl TempDir {
        pub(crate) fn new() -> Self {
            static N: AtomicU64 = AtomicU64::new(0);
            let base = std::fs::canonicalize(std::env::temp_dir()).unwrap();
            let dir = base.join(format!(
                "fell-mcp-setup-{}-{}",
                std::process::id(),
                N.fetch_add(1, Ordering::Relaxed)
            ));
            let _ = std::fs::remove_dir_all(&dir);
            std::fs::create_dir_all(&dir).unwrap();
            // Under a test's strict umask the folder would not be writable.
            std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o700)).unwrap();
            Self(dir)
        }

        pub(crate) fn path(&self) -> &Path {
            &self.0
        }
    }

    impl Drop for TempDir {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    const UMASK_CHILD: &str = "FELL_MCP_TEST_UMASK";

    /// Mode checks must not depend on the umask `cargo test` runs with. The
    /// umask is per process, so the test reruns itself in a child process
    /// with `umask`. Returns true in that child, where the test body runs;
    /// the parent only checks the child passed.
    pub(crate) fn in_child_with_umask(test: &str, umask: u32) -> bool {
        if let Some(value) = std::env::var_os(UMASK_CHILD) {
            assert_eq!(value.to_str(), Some(format!("{umask:03o}").as_str()));
            assert_eq!(
                current_umask(),
                umask,
                "the child runs with umask {umask:03o}"
            );
            return true;
        }
        let status = std::process::Command::new("sh")
            .arg("-c")
            .arg(format!("umask {umask:03o} && exec \"$0\" \"$@\""))
            .arg(std::env::current_exe().unwrap())
            .args(["--exact", test, "--test-threads=1", "--nocapture"])
            .env(UMASK_CHILD, format!("{umask:03o}"))
            .status()
            .unwrap();
        assert!(status.success(), "{test} failed under umask {umask:03o}");
        false
    }

    fn current_umask() -> u32 {
        let status = std::fs::read_to_string("/proc/self/status").unwrap();
        let line = status
            .lines()
            .find_map(|line| line.strip_prefix("Umask:"))
            .unwrap();
        u32::from_str_radix(line.trim(), 8).unwrap()
    }

    fn mode(path: &Path) -> u32 {
        std::fs::symlink_metadata(path)
            .unwrap()
            .permissions()
            .mode()
            & 0o777
    }

    fn names(dir: &Path) -> Vec<String> {
        let mut names: Vec<_> = std::fs::read_dir(dir)
            .unwrap()
            .map(|entry| entry.unwrap().file_name().into_string().unwrap())
            .collect();
        names.sort();
        names
    }

    /// A home folder in a temp dir, and an env pointing at it.
    fn home() -> (TempDir, PathBuf, Env) {
        let tmp = TempDir::new();
        let home = tmp.path().join("home");
        std::fs::create_dir(&home).unwrap();
        std::fs::set_permissions(&home, std::fs::Permissions::from_mode(0o700)).unwrap();
        let env = Env {
            home: Some(home.clone()),
            ..Env::default()
        };
        (tmp, home, env)
    }

    /// What a harness does when it saves: write a new file, rename it over.
    fn harness_saves(path: &Path, text: &str) {
        let temp = path.with_extension("harness-tmp");
        std::fs::write(&temp, text).unwrap();
        std::fs::rename(&temp, path).unwrap();
    }

    #[test]
    fn a_harness_rewrite_during_the_write_is_kept_and_fell_starts_over_once() {
        let (_tmp, home, env) = home();
        let dir = home.join(".codex");
        std::fs::create_dir(&dir).unwrap();
        let path = dir.join("config.toml");
        std::fs::write(&path, "model = \"o4\"\n").unwrap();

        // The harness saves once, between Fell's read and its swap.
        let calls = std::rc::Rc::new(std::cell::Cell::new(0));
        let seen = calls.clone();
        let _hook = OnSwap::new(move |path| {
            seen.set(seen.get() + 1);
            if seen.get() == 1 {
                harness_saves(path, "model = \"o4\"\nhistory = \"save-all\"\n");
            }
        });
        let written = write_config(Harness::Codex, &path, &spec(), &env, 7).unwrap();
        assert_eq!(calls.get(), 2, "Fell read the file again and retried once");
        let Written::Wrote { backup, .. } = written else {
            panic!("{written:?}");
        };
        let text = std::fs::read_to_string(&path).unwrap();
        assert!(text.contains("history = \"save-all\""), "{text}");
        assert!(text.contains("[mcp_servers.fell]"), "{text}");
        // The backup is the version Fell merged into: the harness's.
        assert_eq!(
            std::fs::read_to_string(backup.unwrap()).unwrap(),
            "model = \"o4\"\nhistory = \"save-all\"\n"
        );
        assert_eq!(names(&dir), ["config.toml", "config.toml.fell-backup-7"]);
    }

    #[test]
    fn a_harness_that_keeps_rewriting_wins_and_fell_says_try_again() {
        let (_tmp, home, env) = home();
        let dir = home.join(".codex");
        std::fs::create_dir(&dir).unwrap();
        let path = dir.join("config.toml");
        std::fs::write(&path, "model = \"o4\"\n").unwrap();

        let calls = std::rc::Rc::new(std::cell::Cell::new(0));
        let seen = calls.clone();
        let _hook = OnSwap::new(move |path| {
            seen.set(seen.get() + 1);
            // Written in place this time: same inode, new size and mtime.
            std::fs::write(path, format!("model = \"o{}\"\n# saved\n", seen.get())).unwrap();
        });
        let err = write_config(Harness::Codex, &path, &spec(), &env, 7).unwrap_err();
        assert_eq!(
            err,
            "Codex rewrote ~/.codex/config.toml while Fell was writing it; try again"
        );
        assert_eq!(calls.get(), 2);
        assert_eq!(
            std::fs::read_to_string(&path).unwrap(),
            "model = \"o2\"\n# saved\n"
        );
        // No temp file and no backup of a version that was never replaced.
        assert_eq!(names(&dir), ["config.toml"]);
    }

    #[test]
    fn a_config_created_meanwhile_is_merged_not_replaced() {
        let (_tmp, home, env) = home();
        let path = home.join(".gemini/settings.json");
        let calls = std::rc::Rc::new(std::cell::Cell::new(0));
        let seen = calls.clone();
        let _hook = OnSwap::new(move |path| {
            seen.set(seen.get() + 1);
            if seen.get() == 1 {
                std::fs::write(path, r#"{"theme":"Dracula"}"#).unwrap();
            }
        });
        let written = write_config(Harness::Gemini, &path, &spec(), &env, 7).unwrap();
        assert!(
            matches!(written, Written::Wrote { created: false, .. }),
            "{written:?}"
        );
        let value: Value = serde_json::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
        assert_eq!(value["theme"], "Dracula");
        assert_eq!(value["mcpServers"]["fell"]["command"], "/opt/fell/fell-mcp");
    }

    #[test]
    fn a_config_locked_by_another_program_is_not_written() {
        let (_tmp, home, env) = home();
        let dir = home.join(".codex");
        std::fs::create_dir(&dir).unwrap();
        let path = dir.join("config.toml");
        std::fs::write(&path, "model = \"o4\"\n").unwrap();
        let other = File::open(&path).unwrap();
        fs::flock(&other, FlockOperation::LockExclusive).unwrap();
        let err = write_config(Harness::Codex, &path, &spec(), &env, 7).unwrap_err();
        assert_eq!(
            err,
            "another program is writing ~/.codex/config.toml; try again"
        );
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "model = \"o4\"\n");
        assert_eq!(names(&dir), ["config.toml"]);
    }

    #[test]
    fn a_symlinked_folder_anywhere_below_home_is_refused() {
        let (tmp, home, env) = home();
        let elsewhere = tmp.path().join("elsewhere");
        std::fs::create_dir(&elsewhere).unwrap();

        // ~/.config itself is the symlink, two levels above the file.
        std::os::unix::fs::symlink(&elsewhere, home.join(".config")).unwrap();
        let path = Harness::OpenCode.config_path(&env).unwrap();
        let err = write_config(Harness::OpenCode, &path, &spec(), &env, 7).unwrap_err();
        assert_eq!(
            err,
            "~/.config is a symlink, and Fell does not follow symlinks"
        );
        let err = write_config(
            Harness::Goose,
            &home.join(".config/goose/config.yaml"),
            &spec(),
            &env,
            7,
        )
        .unwrap_err();
        assert_eq!(
            err,
            "~/.config is a symlink, and Fell does not follow symlinks"
        );
        assert!(
            names(&elsewhere).is_empty(),
            "nothing was written through it"
        );

        // The same when the folder already holds a config.
        std::fs::create_dir(elsewhere.join("opencode")).unwrap();
        std::fs::write(elsewhere.join("opencode/opencode.json"), "{}").unwrap();
        let err = write_config(Harness::OpenCode, &path, &spec(), &env, 7).unwrap_err();
        assert!(err.starts_with("~/.config is a symlink"), "{err}");
        assert_eq!(names(&elsewhere.join("opencode")), ["opencode.json"]);

        // A file where a folder should be.
        std::fs::write(home.join(".gemini"), "").unwrap();
        let err = write_config(
            Harness::Gemini,
            &home.join(".gemini/settings.json"),
            &spec(),
            &env,
            7,
        )
        .unwrap_err();
        assert_eq!(err, "~/.gemini is not a folder");
    }

    #[test]
    fn a_symlink_outside_home_is_refused_too() {
        let (tmp, _home, env) = home();
        let real = tmp.path().join("real");
        std::fs::create_dir(&real).unwrap();
        std::os::unix::fs::symlink(&real, tmp.path().join("link")).unwrap();
        let env = Env {
            xdg_config: Some(tmp.path().join("link/xdg")),
            ..env
        };
        let path = Harness::Goose.config_path(&env).unwrap();
        let err = write_config(Harness::Goose, &path, &spec(), &env, 7).unwrap_err();
        assert_eq!(
            err,
            format!(
                "{} is a symlink, and Fell does not follow symlinks",
                tmp.path().join("link").display()
            )
        );
        assert!(names(&real).is_empty());
        let relative = Path::new("goose/config.yaml");
        assert!(write_config(Harness::Goose, relative, &spec(), &env, 7).is_err());
        let dotted = tmp.path().join("real/../real/config.yaml");
        assert!(
            write_config(Harness::Goose, &dotted, &spec(), &env, 7)
                .unwrap_err()
                .contains("not a plain absolute path")
        );
    }

    #[test]
    fn modes_are_exact_under_a_strict_umask() {
        // 0277 would turn mkdir 0700 into 0500 and create 0600 into 0400.
        if !in_child_with_umask("setup::tests::modes_are_exact_under_a_strict_umask", 0o277) {
            return;
        }
        let (_tmp, home, env) = home();

        // New folders and a new file.
        let path = home.join(".config/goose/config.yaml");
        let written = write_config(Harness::Goose, &path, &spec(), &env, 7).unwrap();
        assert!(matches!(written, Written::Wrote { created: true, .. }));
        assert_eq!(mode(&home.join(".config")), 0o700);
        assert_eq!(mode(&home.join(".config/goose")), 0o700);
        assert_eq!(mode(&path), 0o600);

        // An existing file keeps its own mode; the backup is owner-only.
        let dir = home.join(".codex");
        std::fs::create_dir(&dir).unwrap();
        std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o755)).unwrap();
        let config = dir.join("config.toml");
        std::fs::write(&config, "model = \"o4\"\n").unwrap();
        std::fs::set_permissions(&config, std::fs::Permissions::from_mode(0o644)).unwrap();
        let written = write_config(Harness::Codex, &config, &spec(), &env, 7).unwrap();
        let Written::Wrote { backup, .. } = written else {
            panic!("{written:?}");
        };
        assert_eq!(mode(&config), 0o644);
        assert_eq!(mode(&backup.unwrap()), 0o600);
        // An existing folder keeps its mode.
        assert_eq!(mode(&dir), 0o755);
    }

    #[test]
    fn opencode_goes_under_mcp_servers_and_keeps_timeout_and_older_entries() {
        let existing = r#"{
  "$schema": "https://opencode.ai/config.json",
  "mcp": {
    "timeout": { "startup": 10000 },
    "servers": {
      "docs": { "type": "local", "command": ["docs-mcp"], "disabled": true }
    },
    "legacy": { "type": "local", "command": ["old-mcp"], "enabled": true }
  }
}"#;
        let merged = merge(Harness::OpenCode, Some(existing), &spec()).unwrap();
        assert!(!merged.replaced && !merged.unchanged);
        let value: Value = serde_json::from_str(&merged.text).unwrap();
        let mcp = value["mcp"].as_object().unwrap();
        assert_eq!(
            mcp.keys().collect::<Vec<_>>(),
            ["timeout", "servers", "legacy"]
        );
        assert_eq!(value["mcp"]["timeout"]["startup"], 10000);
        assert_eq!(value["mcp"]["legacy"]["command"][0], "old-mcp");
        let servers = value["mcp"]["servers"].as_object().unwrap();
        assert_eq!(servers.keys().collect::<Vec<_>>(), ["docs", "fell"]);
        assert_eq!(servers["docs"]["disabled"], true);
        let fell = servers["fell"].as_object().unwrap();
        assert_eq!(fell.keys().collect::<Vec<_>>(), ["type", "command"]);
        assert_eq!(fell["type"], "local");
        assert_eq!(fell["command"][0], "/opt/fell/fell-mcp");

        let again = merge(Harness::OpenCode, Some(&merged.text), &spec()).unwrap();
        assert!(again.unchanged);
        // An `mcp` with only older-style entries gains a `servers` map.
        let merged = merge(
            Harness::OpenCode,
            Some(r#"{"mcp":{"x":{"type":"local","command":["x"]}}}"#),
            &spec(),
        )
        .unwrap();
        let value: Value = serde_json::from_str(&merged.text).unwrap();
        assert_eq!(value["mcp"]["x"]["command"][0], "x");
        assert_eq!(value["mcp"]["servers"]["fell"]["type"], "local");
        for bad in [r#"{"mcp": 3}"#, r#"{"mcp": {"servers": []}}"#] {
            assert!(
                merge(Harness::OpenCode, Some(bad), &spec()).is_err(),
                "{bad}"
            );
        }
    }

    fn spec() -> ServerSpec {
        ServerSpec {
            program: "/opt/fell/fell-mcp".into(),
            args: vec![
                "serve".into(),
                "--store".into(),
                "/srv/mail.db".into(),
                "--account".into(),
                "demo".into(),
                "--agent".into(),
                "bot".into(),
            ],
        }
    }

    #[test]
    fn harness_names_round_trip() {
        for harness in Harness::ALL {
            assert_eq!(Harness::parse(harness.id()), Some(harness));
        }
        assert_eq!(Harness::parse("Claude"), Some(Harness::ClaudeCode));
        assert_eq!(Harness::parse("cursor"), None);
    }

    #[test]
    fn config_paths_follow_each_harness_and_its_overrides() {
        let mut env = Env {
            home: Some("/h".into()),
            ..Env::default()
        };
        let path = |harness: Harness, env: &Env| harness.config_path(env).unwrap();
        assert_eq!(Harness::ClaudeCode.config_path(&env), None);
        assert_eq!(
            path(Harness::Codex, &env),
            Path::new("/h/.codex/config.toml")
        );
        assert_eq!(
            path(Harness::Gemini, &env),
            Path::new("/h/.gemini/settings.json")
        );
        assert_eq!(
            path(Harness::OpenCode, &env),
            Path::new("/h/.config/opencode/opencode.json")
        );
        assert_eq!(
            path(Harness::Goose, &env),
            Path::new("/h/.config/goose/config.yaml")
        );
        env.xdg_config = Some("/x".into());
        env.codex_home = Some("/c".into());
        env.opencode_config_dir = Some("/oc".into());
        assert_eq!(
            path(Harness::OpenCode, &env),
            Path::new("/oc/opencode.json")
        );
        env.opencode_config_dir = None;
        assert_eq!(path(Harness::Codex, &env), Path::new("/c/config.toml"));
        assert_eq!(
            path(Harness::OpenCode, &env),
            Path::new("/x/opencode/opencode.json")
        );
        assert_eq!(
            path(Harness::Goose, &env),
            Path::new("/x/goose/config.yaml")
        );
        assert_eq!(
            env.display(Path::new("/h/.codex/config.toml")),
            "~/.codex/config.toml"
        );
        assert_eq!(env.display(Path::new("/x/y")), "/x/y");
    }

    #[test]
    fn snippets_never_carry_keys_tokens_env_or_trust() {
        for harness in Harness::ALL {
            let text = snippet(harness, &spec()).to_ascii_lowercase();
            for word in [
                "key", "token", "secret", "password", "trust", "\"env\"", "env =",
            ] {
                assert!(
                    !text.contains(word),
                    "{harness:?} snippet has {word}: {text}"
                );
            }
            for scope in ["--read", "--triage", "--draft"] {
                assert!(!text.contains(scope), "{harness:?} snippet grants a scope");
            }
        }
        // Goose's `envs` is present and empty.
        assert!(snippet(Harness::Goose, &spec()).contains("envs: {}"));
    }

    #[test]
    fn json_merge_keeps_other_servers_and_key_order() {
        let existing = r#"{
  "theme": "dark",
  "mcpServers": {
    "github": { "command": "gh-mcp", "env": { "GITHUB_TOKEN": "x" } }
  },
  "zeta": [1, 2]
}"#;
        let merged = merge(Harness::Gemini, Some(existing), &spec()).unwrap();
        assert!(!merged.replaced && !merged.unchanged);
        let value: Value = serde_json::from_str(&merged.text).unwrap();
        let keys: Vec<_> = value.as_object().unwrap().keys().cloned().collect();
        assert_eq!(keys, ["theme", "mcpServers", "zeta"]);
        let servers = value["mcpServers"].as_object().unwrap();
        assert_eq!(servers.keys().collect::<Vec<_>>(), ["github", "fell"]);
        assert_eq!(servers["github"]["env"]["GITHUB_TOKEN"], "x");
        assert_eq!(servers["fell"]["command"], "/opt/fell/fell-mcp");

        let again = merge(Harness::Gemini, Some(&merged.text), &spec()).unwrap();
        assert!(again.unchanged && again.replaced);

        let mut moved = spec();
        moved.args[2] = "/srv/other.db".into();
        let replaced = merge(Harness::Gemini, Some(&merged.text), &moved).unwrap();
        assert!(replaced.replaced && !replaced.unchanged);
        let value: Value = serde_json::from_str(&replaced.text).unwrap();
        assert_eq!(value["mcpServers"]["fell"]["args"][2], "/srv/other.db");
        assert_eq!(
            value["mcpServers"]
                .as_object()
                .unwrap()
                .keys()
                .collect::<Vec<_>>(),
            ["github", "fell"]
        );
    }

    #[test]
    fn json_merge_refuses_comments_and_odd_shapes() {
        let err = merge(Harness::OpenCode, Some("{ // hi\n}"), &spec()).unwrap_err();
        assert!(err.contains("comments"));
        let err = merge(Harness::Gemini, Some("[]"), &spec()).unwrap_err();
        assert!(err.contains("not a JSON object"));
        let err = merge(Harness::Gemini, Some(r#"{"mcpServers": 3}"#), &spec()).unwrap_err();
        assert!(err.contains("`mcpServers`"));
    }

    #[test]
    fn shell_words_survive_spaces_and_quotes() {
        assert_eq!(shell_word("/opt/fell/fell-mcp"), "/opt/fell/fell-mcp");
        assert_eq!(shell_word("--store"), "--store");
        assert_eq!(shell_word("/a b/it's"), "'/a b/it'\\''s'");
        assert_eq!(shell_word(""), "''");
        assert_eq!(shell_word("$HOME"), "'$HOME'");
    }

    #[test]
    fn opencode_new_file_gets_its_schema() {
        let merged = merge(Harness::OpenCode, None, &spec()).unwrap();
        let value: Value = serde_json::from_str(&merged.text).unwrap();
        assert_eq!(value["$schema"], "https://opencode.ai/config.json");
        let fell = &value["mcp"]["servers"]["fell"];
        assert_eq!(fell["type"], "local");
        assert_eq!(fell["command"][0], "/opt/fell/fell-mcp");
        assert_eq!(fell["command"][1], "serve");
        assert!(fell.get("enabled").is_none() && fell.get("disabled").is_none());
    }

    #[test]
    fn toml_merge_keeps_comments_and_other_servers() {
        let existing = "\
# my settings
model = \"o4\" # inline note

[mcp_servers.docs]
command = \"docs-mcp\"
env = { DOCS_KEY = \"abc\" }

[profiles.work]
model = \"o3\"
";
        let merged = merge(Harness::Codex, Some(existing), &spec()).unwrap();
        assert!(!merged.replaced && !merged.unchanged);
        // Fell's table lands next to the other servers; the rest is untouched.
        let expected = existing.replace(
            "\n[profiles.work]",
            "\n[mcp_servers.fell]\ncommand = \"/opt/fell/fell-mcp\"\nargs = [\"serve\", \"--store\", \"/srv/mail.db\", \"--account\", \"demo\", \"--agent\", \"bot\"]\n\n[profiles.work]",
        );
        assert_eq!(merged.text, expected);
        let doc: toml_edit::DocumentMut = merged.text.parse().unwrap();
        assert_eq!(
            doc["mcp_servers"]["docs"]["env"]["DOCS_KEY"].as_str(),
            Some("abc")
        );
        assert_eq!(doc["mcp_servers"]["fell"]["args"][6].as_str(), Some("bot"));
        assert_eq!(doc["profiles"]["work"]["model"].as_str(), Some("o3"));

        let again = merge(Harness::Codex, Some(&merged.text), &spec()).unwrap();
        assert!(again.unchanged);
        let mut moved = spec();
        moved.program = "/usr/bin/fell-mcp".into();
        let replaced = merge(Harness::Codex, Some(&merged.text), &moved).unwrap();
        assert!(replaced.replaced && !replaced.unchanged);
        assert_eq!(replaced.text.matches("[mcp_servers.fell]").count(), 1);
        assert!(replaced.text.contains("/usr/bin/fell-mcp"));
        assert!(replaced.text.contains("# my settings"));

        assert!(merge(Harness::Codex, Some("mcp_servers = 3\n"), &spec()).is_err());
        assert!(merge(Harness::Codex, Some("[oops\n"), &spec()).is_err());
    }

    #[test]
    fn yaml_merge_inserts_under_extensions_and_keeps_the_rest() {
        let existing = "\
# goose config
GOOSE_PROVIDER: anthropic
extensions:
    developer:
        bundled: true
        enabled: true
        name: developer
        timeout: 300
        type: builtin
GOOSE_MODEL: claude
";
        let merged = merge(Harness::Goose, Some(existing), &spec()).unwrap();
        assert!(!merged.replaced && !merged.unchanged);
        assert!(
            merged
                .text
                .starts_with("# goose config\nGOOSE_PROVIDER: anthropic\nextensions:\n    fell:\n")
        );
        assert!(merged.text.ends_with("    developer:\n        bundled: true\n        enabled: true\n        name: developer\n        timeout: 300\n        type: builtin\nGOOSE_MODEL: claude\n"));
        let docs = yaml_docs(&merged.text).unwrap();
        let fell = yaml_get(yaml_get(&docs[0], "extensions").unwrap(), "fell").unwrap();
        assert_eq!(
            yaml_get(fell, "cmd").unwrap().as_str(),
            Some("/opt/fell/fell-mcp")
        );
        assert_eq!(yaml_get(fell, "type").unwrap().as_str(), Some("stdio"));

        let again = merge(Harness::Goose, Some(&merged.text), &spec()).unwrap();
        assert!(again.unchanged);
        let mut moved = spec();
        moved.args[4] = "work".into();
        let replaced = merge(Harness::Goose, Some(&merged.text), &moved).unwrap();
        assert!(replaced.replaced && !replaced.unchanged);
        assert_eq!(replaced.text.matches("    fell:").count(), 1);
        assert!(replaced.text.contains("\"work\""));
        assert!(
            replaced
                .text
                .contains("    developer:\n        bundled: true")
        );
    }

    #[test]
    fn yaml_merge_appends_extensions_when_missing_and_refuses_what_it_cannot_place() {
        let merged = merge(Harness::Goose, Some("GOOSE_PROVIDER: openai\n"), &spec()).unwrap();
        assert!(
            merged
                .text
                .starts_with("GOOSE_PROVIDER: openai\nextensions:\n  fell:\n")
        );
        let merged = merge(
            Harness::Goose,
            Some("extensions:\nGOOSE_MODEL: x\n"),
            &spec(),
        )
        .unwrap();
        assert!(merged.text.starts_with("extensions:\n  fell:\n"));
        assert!(merged.text.ends_with("GOOSE_MODEL: x\n"));
        for bad in [
            "extensions: {developer: {enabled: true}}\n",
            "extensions:\n  \"fell\":\n    enabled: true\n",
            "- a\n- b\n",
            "a: [\n",
            "a: 1\n---\nb: 2\n",
        ] {
            assert!(merge(Harness::Goose, Some(bad), &spec()).is_err(), "{bad}");
        }
    }

    #[test]
    fn yaml_quotes_values_that_would_change_type() {
        let mut odd = spec();
        odd.args[4] = "123".into();
        odd.program = "/opt/my fell/fell-mcp: \"x\"".into();
        let text = snippet(Harness::Goose, &odd);
        let docs = yaml_docs(&text).unwrap();
        let fell = yaml_get(yaml_get(&docs[0], "extensions").unwrap(), "fell").unwrap();
        assert_eq!(
            yaml_get(fell, "cmd").unwrap().as_str(),
            Some(odd.program.as_str())
        );
        let YamlOwned::Sequence(args) = yaml_get(fell, "args").unwrap() else {
            panic!("args is not a sequence");
        };
        assert_eq!(args[4].as_str(), Some("123"));
    }
}
