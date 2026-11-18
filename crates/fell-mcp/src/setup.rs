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

use std::fs::{self, OpenOptions};
use std::io::{Read, Write};
use std::os::unix::fs::{DirBuilderExt, OpenOptionsExt, PermissionsExt};
use std::path::{Path, PathBuf};

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
    /// - OpenCode: `~/.config/opencode/opencode.json`, `mcp`.
    /// - Goose: `~/.config/goose/config.yaml`, `extensions`.
    ///
    /// Claude Code is `None`. Its user-level servers live in `~/.claude.json`
    /// next to state Claude Code rewrites while it runs, so a write from here
    /// could be lost or clobber that state. Fell prints Claude Code's own
    /// `claude mcp add` command instead.
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
            Self::OpenCode => xdg_config.map(|dir| dir.join("opencode/opencode.json")),
            Self::Goose => xdg_config.map(|dir| dir.join("goose/config.yaml")),
        }
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
        Harness::OpenCode => json_text(&json!({ "mcp": { ENTRY: json_entry(harness, spec) } })),
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
pub(crate) fn write_config(
    harness: Harness,
    path: &Path,
    spec: &ServerSpec,
    env: &Env,
    stamp: i64,
) -> Result<Written, String> {
    let shown = env.display(path);
    let Some(dir) = path.parent() else {
        return Err(format!("{shown} has no parent folder"));
    };
    if let Some(other) = harness.shadowing_file(path)
        && fs::symlink_metadata(&other).is_ok()
    {
        return Err(format!(
            "{} exists, and Fell does not rewrite JSON with comments",
            env.display(&other)
        ));
    }
    match fs::symlink_metadata(dir) {
        Ok(meta) if meta.file_type().is_symlink() => {
            return Err(format!(
                "{} is a symlink, and Fell does not follow symlinks",
                env.display(dir)
            ));
        }
        Ok(meta) if !meta.is_dir() => {
            return Err(format!("{} is not a folder", env.display(dir)));
        }
        Ok(_) => {}
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => {}
        Err(err) => return Err(format!("{}: {err}", env.display(dir))),
    }
    let existing = match fs::symlink_metadata(path) {
        Ok(meta) if meta.file_type().is_symlink() => {
            return Err(format!(
                "{shown} is a symlink, and Fell does not follow symlinks"
            ));
        }
        Ok(meta) if !meta.is_file() => return Err(format!("{shown} is not a regular file")),
        Ok(meta) => Some((
            read_no_follow(path).map_err(|err| format!("{shown}: {err}"))?,
            meta.permissions().mode() & 0o777,
        )),
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => None,
        Err(err) => return Err(format!("{shown}: {err}")),
    };
    let merged = merge(
        harness,
        existing.as_ref().map(|(text, _)| text.as_str()),
        spec,
    )
    .map_err(|reason| format!("{shown} {reason}"))?;
    if merged.unchanged {
        return Ok(Written::Unchanged);
    }
    if existing.is_none() {
        fs::DirBuilder::new()
            .recursive(true)
            .mode(0o700)
            .create(dir)
            .map_err(|err| format!("{}: {err}", env.display(dir)))?;
    }
    let backup = match &existing {
        Some((text, _)) => {
            let mut n = 0;
            loop {
                let suffix = match n {
                    0 => format!("fell-backup-{stamp}"),
                    n => format!("fell-backup-{stamp}-{n}"),
                };
                let backup = sibling(path, &suffix);
                match write_new(&backup, text.as_bytes(), 0o600) {
                    Ok(()) => break Some(backup),
                    Err(err) if err.kind() == std::io::ErrorKind::AlreadyExists && n < 100 => {
                        n += 1;
                    }
                    Err(err) => return Err(format!("{}: {err}", env.display(&backup))),
                }
            }
        }
        None => None,
    };
    let mode = existing.as_ref().map_or(0o600, |(_, mode)| *mode);
    let temp = sibling(path, &format!("fell-tmp-{}-{stamp}", std::process::id()));
    let result =
        write_new(&temp, merged.text.as_bytes(), mode).and_then(|()| fs::rename(&temp, path));
    if let Err(err) = result {
        let _ = fs::remove_file(&temp);
        return Err(format!("{shown}: {err}"));
    }
    Ok(Written::Wrote {
        backup,
        replaced: merged.replaced,
        created: existing.is_none(),
    })
}

fn sibling(path: &Path, suffix: &str) -> PathBuf {
    let mut name = path.file_name().unwrap_or_default().to_os_string();
    name.push(".");
    name.push(suffix);
    path.with_file_name(name)
}

fn read_no_follow(path: &Path) -> std::io::Result<String> {
    let mut file = OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NOFOLLOW)
        .open(path)?;
    let mut text = String::new();
    file.read_to_string(&mut text)?;
    Ok(text)
}

fn write_new(path: &Path, bytes: &[u8], mode: u32) -> std::io::Result<()> {
    let mut file = OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(mode)
        .custom_flags(libc::O_NOFOLLOW)
        .open(path)?;
    file.write_all(bytes)?;
    file.sync_all()
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

fn json_container(harness: Harness) -> &'static str {
    match harness {
        Harness::OpenCode => "mcp",
        _ => "mcpServers",
    }
}

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
            json!({ "type": "local", "command": command, "enabled": true })
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
    let container = json_container(harness);
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
    let servers = root
        .entry(container)
        .or_insert_with(|| Value::Object(Map::new()));
    let Some(servers) = servers.as_object_mut() else {
        return Err(format!("has a `{container}` that is not an object"));
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
mod tests {
    use super::*;

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
        assert_eq!(value["mcp"]["fell"]["type"], "local");
        assert_eq!(value["mcp"]["fell"]["command"][0], "/opt/fell/fell-mcp");
        assert_eq!(value["mcp"]["fell"]["command"][1], "serve");
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
