//! `fell-mcp` command line. `add` connects a harness, `grant`, `list` and
//! `revoke` manage scopes in the local store, and `serve` speaks MCP. None of
//! them can send mail.

use std::ffi::OsString;
use std::io::{self, BufRead, IsTerminal, Write};
use std::path::{Path, PathBuf};

use fell_core::{Account, AccountId, AgentGrant, Store};

use crate::protocol::Session;
use crate::setup::{self, Env, Harness, ServerSpec, Written};

const HELP: &str = "\
fell-mcp — local MCP server for Fell

There is no send tool. Agents are off until granted a scope.

Usage:
  fell-mcp add HARNESS [--read] [--triage] [--draft] [--annotate] [--write | --print]
  fell-mcp list
  fell-mcp revoke AGENT [--account ID]
  fell-mcp grant  AGENT [--read] [--triage] [--draft] [--annotate] [--account ID]
  fell-mcp serve  --account ID --agent ID [--store PATH]
  fell-mcp --version

Harnesses: claude-code, codex, gemini, opencode, goose.

`add` grants the scopes you pick (read and draft unless you ask for triage)
and prints the harness's MCP entry. It asks before adding the entry to the
harness's config file; --write adds it without asking, --print never does.

Scopes:
  --read     search, read_thread, list_folders, list_threads, list_labels
  --triage   label, move, archive, star, mark_read, mark_unread, snooze,
             unsnooze, and undo of those
  --draft    create_draft, list_drafts, and undo of those
  --annotate annotate_thread and undo of agent notes

Every command uses Fell's mailbox ($FELL_STORE, else
$XDG_DATA_HOME/fell/mail.db) unless --store is given, and the only account
in it unless --account is given. --agent names the agent (default: the
harness name).
";

#[derive(Debug)]
pub struct CliError {
    message: String,
}

impl CliError {
    fn new(message: impl Into<String>) -> Self {
        Self {
            message: message.into(),
        }
    }
}

impl std::fmt::Display for CliError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.message)
    }
}

impl std::error::Error for CliError {}

impl From<io::Error> for CliError {
    fn from(err: io::Error) -> Self {
        Self::new(err.to_string())
    }
}

fn store_err(err: fell_core::Error) -> CliError {
    CliError::new(err.to_string())
}

/// What the commands read and write besides the store. Tests swap in buffers.
pub(crate) struct Io<'a> {
    pub input: &'a mut dyn BufRead,
    /// Snippets and `list` output.
    pub out: &'a mut dyn Write,
    /// Questions and progress.
    pub err: &'a mut dyn Write,
    pub tty: bool,
    pub env: Env,
    /// Fell's own mailbox, when the environment names one.
    pub default_store: Option<PathBuf>,
    pub registry: Option<fell_core::accounts::Registry>,
    /// This program, as a harness should start it.
    pub exe: Option<PathBuf>,
    /// The AppImage file when this runs inside one: harnesses start
    /// `APPIMAGE mcp serve …`, since the mounted path changes every run.
    pub appimage: Option<PathBuf>,
    /// How the person ran this program: `fell-mcp` or `fell mcp`.
    pub name: &'static str,
    pub now_unix: i64,
}

/// argv[0] when `fell mcp` runs this program.
pub const FELL_MCP_ARG0: &str = "fell mcp";

/// `args` is a full argv, including the program name.
pub fn cli_main<I, S>(args: I) -> Result<(), CliError>
where
    I: IntoIterator<Item = S>,
    S: Into<OsString>,
{
    let mut args: Vec<OsString> = args.into_iter().map(Into::into).collect();
    // `fell mcp …` starts this program with that argv[0], so hints match
    // what the person typed.
    let name = if args.first().is_some_and(|arg0| arg0 == FELL_MCP_ARG0) {
        FELL_MCP_ARG0
    } else {
        "fell-mcp"
    };
    if !args.is_empty() {
        args.remove(0);
    }
    let stdin = io::stdin();
    let mut input = stdin.lock();
    let mut out = io::stdout();
    let mut err = io::stderr();
    let tty = io::stdin().is_terminal() && io::stderr().is_terminal();
    let exe = std::env::current_exe().ok();
    let appimage = inside_appimage(
        exe.as_deref(),
        std::env::var_os("APPIMAGE").map(PathBuf::from),
        std::env::var_os("APPDIR").map(PathBuf::from),
    );
    let mut io = Io {
        input: &mut input,
        out: &mut out,
        err: &mut err,
        tty,
        env: Env::from_process(),
        default_store: fell_core::compose::mailbox_db_path(),
        registry: fell_core::accounts::Registry::resolve().ok(),
        exe,
        appimage,
        name,
        now_unix: std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_or(0, |d| d.as_secs() as i64),
    };
    run(&args, &mut io)
}

/// The AppImage to start, when this program is the `usr/bin/fell-mcp` of
/// the AppImage's mounted folder.
///
/// `APPIMAGE` and `APPDIR` are plain environment variables, so they are not
/// trusted alone: this binary must be the same file as
/// `$APPDIR/usr/bin/fell-mcp`, `APPDIR` must not be `/` or an install prefix
/// (`APPDIR=/` would make `/usr/bin/fell-mcp` look bundled), and both
/// `$APPDIR/usr/bin/fell-mcp` and `APPIMAGE` must be regular files reached
/// without following symlinks.
fn inside_appimage(
    exe: Option<&Path>,
    appimage: Option<PathBuf>,
    appdir: Option<PathBuf>,
) -> Option<PathBuf> {
    use std::os::unix::fs::MetadataExt;
    let (exe, appimage, appdir) = (exe?, appimage?, appdir?);
    let prefixes = ["/", "/usr", "/usr/local", "/opt"];
    if prefixes.iter().any(|prefix| appdir == Path::new(prefix)) {
        return None;
    }
    let bundled = setup::regular_file_without_symlinks(&appdir.join("usr/bin/fell-mcp"))?;
    let running = std::fs::metadata(exe).ok()?;
    if bundled.dev() != running.dev() || bundled.ino() != running.ino() {
        return None;
    }
    setup::regular_file_without_symlinks(&appimage)?;
    Some(appimage)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Command {
    Help,
    Version,
    Add,
    List,
    Grant,
    Revoke,
    Serve,
}

#[derive(Debug, Default)]
struct Opts {
    positional: Vec<String>,
    store: Option<PathBuf>,
    account: Option<String>,
    agent: Option<String>,
    read: bool,
    triage: bool,
    draft: bool,
    annotate: bool,
    write: bool,
    print: bool,
}

impl Opts {
    fn any_scope(&self) -> bool {
        self.read || self.triage || self.draft || self.annotate
    }
}

pub(crate) fn run(args: &[OsString], io: &mut Io<'_>) -> Result<(), CliError> {
    let (command, opts) = parse(args)?;
    match command {
        Command::Help => {
            writeln!(io.out, "{HELP}")?;
            Ok(())
        }
        Command::Version => Ok(writeln!(io.out, "{}", fell_version::VERSION)?),
        Command::Add => add(&opts, io),
        Command::List => list(&opts, io),
        Command::Grant => grant(&opts, io),
        Command::Revoke => revoke(&opts, io),
        Command::Serve => serve(&opts, io),
    }
}

const COMMANDS: &str = "add, list, revoke, grant, serve, --version, or --help";

fn parse(args: &[OsString]) -> Result<(Command, Opts), CliError> {
    let Some(first) = args.first() else {
        return Err(CliError::new(format!("missing command ({COMMANDS})")));
    };
    let command = match arg_str(first)? {
        "--help" | "-h" | "help" => return Ok((Command::Help, Opts::default())),
        "--version" | "-V" => return Ok((Command::Version, Opts::default())),
        "add" => Command::Add,
        "list" => Command::List,
        "grant" => Command::Grant,
        "revoke" => Command::Revoke,
        "serve" => Command::Serve,
        other => {
            return Err(CliError::new(format!(
                "unknown command {other} ({COMMANDS})"
            )));
        }
    };
    let mut opts = Opts::default();
    let mut index = 1;
    while index < args.len() {
        let flag = arg_str(&args[index])?;
        let mut value = |name: &str| -> Result<OsString, CliError> {
            index += 1;
            args.get(index)
                .cloned()
                .ok_or_else(|| CliError::new(format!("missing value for {name}")))
        };
        match flag {
            "--help" | "-h" => return Ok((Command::Help, Opts::default())),
            "--read" => opts.read = true,
            "--triage" => opts.triage = true,
            "--draft" => opts.draft = true,
            "--annotate" => opts.annotate = true,
            "--write" => opts.write = true,
            "--print" => opts.print = true,
            "--store" => opts.store = Some(PathBuf::from(value("--store")?)),
            "--account" => opts.account = Some(arg_str(&value("--account")?)?.to_string()),
            "--agent" => opts.agent = Some(arg_str(&value("--agent")?)?.to_string()),
            other if other.starts_with('-') => {
                return Err(CliError::new(format!("unknown flag {other}")));
            }
            other => opts.positional.push(other.to_string()),
        }
        index += 1;
    }
    if let Some(account) = &opts.account
        && (account.is_empty() || account != account.trim())
    {
        return Err(CliError::new("account id is empty"));
    }
    if (opts.write || opts.print) && command != Command::Add {
        return Err(CliError::new("--write and --print are for add"));
    }
    match command {
        Command::Add => {
            if opts.positional.len() != 1 {
                return Err(CliError::new(format!(
                    "add needs one harness: {}",
                    harness_names()
                )));
            }
            if opts.write && opts.print {
                return Err(CliError::new("pick one of --write and --print"));
            }
        }
        Command::List => {
            if !opts.positional.is_empty()
                || opts.any_scope()
                || opts.account.is_some()
                || opts.agent.is_some()
            {
                return Err(CliError::new("list takes only --store"));
            }
        }
        Command::Grant | Command::Revoke => {
            take_agent(&mut opts)?;
            if command == Command::Grant && !opts.any_scope() {
                return Err(CliError::new(
                    "grant needs at least one of --read, --triage, --draft, --annotate",
                ));
            }
            if command == Command::Revoke && opts.any_scope() {
                return Err(CliError::new("revoke clears every scope; omit scope flags"));
            }
        }
        Command::Serve => {
            if !opts.positional.is_empty() {
                return Err(CliError::new(format!(
                    "unexpected argument {}",
                    opts.positional[0]
                )));
            }
            if opts.any_scope() {
                return Err(CliError::new("serve does not take scope flags; use grant"));
            }
            if opts.account.is_none() {
                return Err(CliError::new("missing --account"));
            }
            if opts.agent.is_none() {
                return Err(CliError::new("missing --agent"));
            }
        }
        Command::Help | Command::Version => {}
    }
    if let Some(agent) = &opts.agent {
        check_agent_id(agent)?;
    }
    Ok((command, opts))
}

/// `grant AGENT` and `revoke AGENT`, or the older `--agent AGENT`.
fn take_agent(opts: &mut Opts) -> Result<(), CliError> {
    match (opts.positional.len(), &opts.agent) {
        (0, Some(_)) => Ok(()),
        (1, None) => {
            opts.agent = opts.positional.pop();
            Ok(())
        }
        (0, None) => Err(CliError::new("missing agent (fell-mcp list shows them)")),
        _ => Err(CliError::new("name one agent")),
    }
}

fn check_agent_id(agent: &str) -> Result<(), CliError> {
    if agent.is_empty()
        || agent != agent.trim()
        || agent.chars().any(|c| c.is_control() || c == '\u{1e}')
    {
        return Err(CliError::new(
            "agent id is empty or contains a control character",
        ));
    }
    Ok(())
}

fn harness_names() -> String {
    Harness::ALL
        .iter()
        .map(|harness| harness.id())
        .collect::<Vec<_>>()
        .join(", ")
}

fn arg_str(value: &OsString) -> Result<&str, CliError> {
    value
        .to_str()
        .ok_or_else(|| CliError::new("argument is not utf-8"))
}

/// The store path and the open store. The mailbox must already exist:
/// opening it here must not create an empty one.
fn open_store(opts: &Opts, io: &Io<'_>) -> Result<(PathBuf, Store), CliError> {
    let path = match (&opts.store, &io.default_store) {
        (Some(path), _) => path.clone(),
        (None, Some(base)) => registry_store(io.registry.as_ref(), base, opts.account.as_deref())?,
        (None, None) => {
            return Err(CliError::new(
                "no mailbox path; set HOME, XDG_DATA_HOME, or FELL_STORE, or pass --store",
            ));
        }
    };
    if !path.is_file() {
        return Err(CliError::new(format!(
            "no Fell mailbox at {}. Connect mail first with `fell account setup`",
            io.env.display(&path)
        )));
    }
    let path = std::path::absolute(&path)?;
    let store = Store::open(&path).map_err(store_err)?;
    store.finish_cid_repair().map_err(store_err)?;
    Ok((path, store))
}

fn registry_store(
    registry: Option<&fell_core::accounts::Registry>,
    base: &Path,
    id: Option<&str>,
) -> Result<PathBuf, CliError> {
    let Some(registry) = registry else {
        return Ok(base.to_owned());
    };
    let accounts = registry.load();
    if accounts.is_empty() && !registry.directory().exists() {
        // A demo or standalone store may have no account file.
        return Ok(base.to_owned());
    }
    let config = match id {
        Some(id) => registry
            .get(&AccountId(id.into()))
            .map_err(|err| CliError::new(err.to_string()))?
            .ok_or_else(|| CliError::new(format!("unknown account {id}")))?,
        None => accounts.default_config().cloned().ok_or_else(|| {
            CliError::new("No readable account configuration. Try `fell account list`.")
        })?,
    };
    Ok(registry.store_path(base, &config.id))
}

fn ask(io: &mut Io<'_>, question: &str, default: bool) -> Result<bool, CliError> {
    let hint = if default { "[Y/n]" } else { "[y/N]" };
    loop {
        write!(io.err, "{question} {hint} ")?;
        io.err.flush()?;
        let mut line = String::new();
        if io.input.read_line(&mut line)? == 0 {
            writeln!(io.err)?;
            return Ok(default);
        }
        match line.trim().to_ascii_lowercase().as_str() {
            "" => return Ok(default),
            "y" | "yes" => return Ok(true),
            "n" | "no" => return Ok(false),
            _ => writeln!(io.err, "Answer y or n.")?,
        }
    }
}

fn account_label(account: &Account) -> String {
    if account.email.is_empty() || account.email == account.id.as_str() {
        account.id.as_str().to_string()
    } else {
        format!("{} ({})", account.id.as_str(), account.email)
    }
}

/// The named account, else the only one, else (at a terminal) the one picked.
fn resolve_account(
    store: &Store,
    given: Option<&str>,
    io: &mut Io<'_>,
) -> Result<Account, CliError> {
    let accounts = store.list_accounts().map_err(store_err)?;
    let ids = || {
        accounts
            .iter()
            .map(|account| account.id.as_str())
            .collect::<Vec<_>>()
            .join(", ")
    };
    if let Some(given) = given {
        return accounts
            .iter()
            .find(|account| account.id.as_str() == given)
            .cloned()
            .ok_or_else(|| {
                CliError::new(if accounts.is_empty() {
                    format!("unknown account {given}")
                } else {
                    format!("unknown account {given} (this mailbox has {})", ids())
                })
            });
    }
    match accounts.as_slice() {
        [] => Err(CliError::new(
            "this mailbox has no account yet. Connect mail first with `fell account setup`",
        )),
        [only] => Ok(only.clone()),
        many if io.tty => {
            writeln!(io.err, "Which account?")?;
            for (n, account) in many.iter().enumerate() {
                writeln!(io.err, "  {}  {}", n + 1, account_label(account))?;
            }
            loop {
                write!(io.err, "Number: ")?;
                io.err.flush()?;
                let mut line = String::new();
                if io.input.read_line(&mut line)? == 0 {
                    return Err(CliError::new("no account picked"));
                }
                if let Ok(n) = line.trim().parse::<usize>()
                    && (1..=many.len()).contains(&n)
                {
                    return Ok(many[n - 1].clone());
                }
            }
        }
        _ => Err(CliError::new(format!(
            "this mailbox has {} accounts ({}); pick one with --account",
            accounts.len(),
            ids()
        ))),
    }
}

fn scope_names(grant: AgentGrant) -> String {
    if !grant.enabled {
        return "revoked".into();
    }
    let names: Vec<&str> = [
        (grant.read, "read"),
        (grant.triage, "triage"),
        (grant.draft, "draft"),
        (grant.annotate, "annotate"),
    ]
    .into_iter()
    .filter_map(|(on, name)| on.then_some(name))
    .collect();
    if names.is_empty() {
        "none".into()
    } else {
        names.join(", ")
    }
}

/// How the person runs this program again, for hints.
fn self_command(io: &Io<'_>) -> String {
    match &io.appimage {
        Some(appimage) => {
            let shown = io.env.display(appimage);
            match shown.strip_prefix("~/") {
                Some(rest) => format!("~/{} mcp", setup::shell_word(rest)),
                None => format!("{} mcp", setup::shell_word(&shown)),
            }
        }
        None => io.name.into(),
    }
}

fn path_text(path: &Path) -> Result<String, CliError> {
    path.to_str()
        .map(str::to_string)
        .ok_or_else(|| CliError::new(format!("{} is not a UTF-8 path", path.display())))
}

fn server_spec(
    io: &Io<'_>,
    store: Option<&Path>,
    account: &AccountId,
    agent: &str,
) -> Result<ServerSpec, CliError> {
    let mut args = Vec::new();
    let program = match (&io.appimage, &io.exe) {
        (Some(appimage), _) => {
            args.extend(["mcp".to_string(), "serve".into()]);
            path_text(appimage)?
        }
        (None, Some(exe)) => {
            args.push("serve".into());
            path_text(exe)?
        }
        (None, None) => return Err(CliError::new("cannot find the fell-mcp program")),
    };
    if let Some(store) = store {
        args.extend(["--store".into(), path_text(store)?]);
    }
    args.extend([
        "--account".into(),
        account.as_str().to_string(),
        "--agent".into(),
        agent.to_string(),
    ]);
    Ok(ServerSpec { program, args })
}

fn add(opts: &Opts, io: &mut Io<'_>) -> Result<(), CliError> {
    let name = &opts.positional[0];
    let Some(harness) = Harness::parse(name) else {
        return Err(CliError::new(format!(
            "unknown harness {name} ({})",
            harness_names()
        )));
    };
    let agent = opts
        .agent
        .clone()
        .unwrap_or_else(|| harness.id().to_string());
    let (store_path, store) = open_store(opts, io)?;
    let account = resolve_account(&store, opts.account.as_deref(), io)?;
    let grant = if opts.any_scope() {
        AgentGrant::enabled(opts.read, opts.triage, opts.draft).with_annotation(opts.annotate)
    } else if io.tty {
        let read = ask(io, &format!("Let {agent} search and read your mail?"), true)?;
        let draft = ask(
            io,
            &format!("Let {agent} save drafts for you to review?"),
            true,
        )?;
        let triage = ask(
            io,
            &format!("Let {agent} triage (label, move, archive, star, snooze)?"),
            false,
        )?;
        if !read && !triage && !draft {
            return Err(CliError::new("nothing granted; nothing changed"));
        }
        AgentGrant::enabled(read, triage, draft)
    } else {
        AgentGrant::enabled(true, false, true)
    };
    store
        .set_agent_grant(&account.id, &agent, grant)
        .map_err(store_err)?;
    writeln!(
        io.err,
        "Granted {agent} {} on {}. It cannot send mail.",
        scope_names(grant),
        account_label(&account)
    )?;
    let spec = server_spec(
        io,
        (opts.store.is_some() || io.registry.is_none()).then_some(store_path.as_path()),
        &account.id,
        &agent,
    )?;
    let snippet = setup::snippet(harness, &spec);
    let revoke_hint = format!(
        "Revoke any time with `{} revoke {agent}`.",
        self_command(io)
    );

    let Some(config) = harness.config_path(&io.env) else {
        if harness.edits_config() {
            // Without an absolute HOME there is no config path to write.
            writeln!(
                io.err,
                "\nHOME is not set to an absolute path, so Fell cannot find {}'s config. \
                 Add this to it yourself:\n",
                harness.title()
            )?;
            write!(io.out, "{snippet}")?;
            writeln!(io.err, "\n{revoke_hint}")?;
            if opts.write {
                return Err(CliError::new(
                    "did not write the config: HOME is not set to an absolute path",
                ));
            }
            return Ok(());
        }
        writeln!(io.err, "\nRun this to add Fell to {}:\n", harness.title())?;
        write!(io.out, "{snippet}")?;
        if opts.write {
            writeln!(
                io.err,
                "\nFell does not edit {}'s config: it rewrites that file while it runs.",
                harness.title()
            )?;
        }
        writeln!(io.err, "\n{revoke_hint}")?;
        return Ok(());
    };
    let shown = io.env.display(&config);
    writeln!(io.err, "\nFor {shown}:\n")?;
    write!(io.out, "{snippet}")?;
    io.out.flush()?;
    writeln!(io.err)?;
    let write = if opts.print {
        false
    } else if opts.write {
        true
    } else if io.tty {
        ask(io, &format!("Add this to {shown}?"), false)?
    } else {
        writeln!(io.err, "Run again with --write to add it to {shown}.")?;
        false
    };
    if !write {
        writeln!(io.err, "{revoke_hint}")?;
        return Ok(());
    }
    match setup::write_config(harness, &config, &spec, &io.env, io.now_unix) {
        Ok(Written::Unchanged) => writeln!(io.err, "{shown} already has this entry.")?,
        Ok(Written::Wrote {
            backup,
            replaced,
            created,
        }) => {
            let verb = if created {
                "Created"
            } else if replaced {
                "Replaced the fell entry in"
            } else {
                "Added fell to"
            };
            match backup {
                Some(backup) => writeln!(
                    io.err,
                    "{verb} {shown}. The old file is {}.",
                    io.env.display(&backup)
                )?,
                None => writeln!(io.err, "{verb} {shown}.")?,
            }
            writeln!(io.err, "Restart {} to load it.", harness.title())?;
        }
        Err(reason) => {
            // The grant is already saved, so the printed entry works once
            // pasted. Exit non-zero: the write that was asked for failed.
            writeln!(io.err, "Did not write: {reason}.")?;
            writeln!(
                io.err,
                "The grant is saved. Paste the entry above into {shown} yourself, \
                 or undo the grant with `{} revoke {agent}`.",
                self_command(io)
            )?;
            return Err(CliError::new(format!("did not write {shown}")));
        }
    }
    writeln!(io.err, "{revoke_hint}")?;
    Ok(())
}

fn ago(now: i64, then: i64) -> String {
    let secs = (now - then).max(0);
    match secs {
        0..60 => "just now".into(),
        60..3_600 => format!("{} min ago", secs / 60),
        3_600..86_400 => format!("{} h ago", secs / 3_600),
        86_400..172_800 => "yesterday".into(),
        _ => format!("{} days ago", secs / 86_400),
    }
}

fn list(opts: &Opts, io: &mut Io<'_>) -> Result<(), CliError> {
    let (_, store) = open_store(opts, io)?;
    let rows = store.list_agent_grants().map_err(store_err)?;
    if rows.is_empty() {
        writeln!(
            io.out,
            "No agents yet. Connect one with `{} add HARNESS`.",
            self_command(io)
        )?;
        return Ok(());
    }
    let table: Vec<[String; 4]> = rows
        .iter()
        .map(|row| {
            [
                row.agent_id.clone(),
                row.account_id.as_str().to_string(),
                scope_names(row.grant),
                row.last_action_unix
                    .map_or_else(|| "never".into(), |at| ago(io.now_unix, at)),
            ]
        })
        .collect();
    let header = ["AGENT", "ACCOUNT", "SCOPES", "LAST ACTION"].map(str::to_string);
    let mut widths = [0usize; 3];
    for row in std::iter::once(&header).chain(&table) {
        for (width, cell) in widths.iter_mut().zip(row) {
            *width = (*width).max(cell.chars().count());
        }
    }
    for row in std::iter::once(&header).chain(&table) {
        writeln!(
            io.out,
            "{:<w0$}  {:<w1$}  {:<w2$}  {}",
            row[0],
            row[1],
            row[2],
            row[3],
            w0 = widths[0],
            w1 = widths[1],
            w2 = widths[2],
        )?;
    }
    Ok(())
}

fn grant(opts: &Opts, io: &mut Io<'_>) -> Result<(), CliError> {
    let agent = opts.agent.as_deref().unwrap_or_default();
    let (_, store) = open_store(opts, io)?;
    let account = match &opts.account {
        // The store reports an unknown account itself.
        Some(id) => AccountId(id.clone()),
        None => resolve_account(&store, None, io)?.id,
    };
    let grant =
        AgentGrant::enabled(opts.read, opts.triage, opts.draft).with_annotation(opts.annotate);
    store
        .set_agent_grant(&account, agent, grant)
        .map_err(store_err)?;
    writeln!(
        io.err,
        "Granted {agent} {} on {}.",
        scope_names(grant),
        account.as_str()
    )?;
    Ok(())
}

fn revoke(opts: &Opts, io: &mut Io<'_>) -> Result<(), CliError> {
    let agent = opts.agent.as_deref().unwrap_or_default();
    let (_, store) = open_store(opts, io)?;
    let accounts: Vec<AccountId> = match &opts.account {
        Some(id) => vec![AccountId(id.clone())],
        None => {
            let rows: Vec<_> = store
                .list_agent_grants()
                .map_err(store_err)?
                .into_iter()
                .filter(|row| row.agent_id == agent)
                .collect();
            if rows.is_empty() {
                return Err(CliError::new(format!(
                    "no agent named {agent} (`{} list` shows them)",
                    self_command(io)
                )));
            }
            if rows.iter().all(|row| !row.grant.enabled) {
                writeln!(io.err, "{agent} is already revoked.")?;
                return Ok(());
            }
            rows.into_iter()
                .filter(|row| row.grant.enabled)
                .map(|row| row.account_id)
                .collect()
        }
    };
    for account in &accounts {
        store
            .set_agent_grant(account, agent, AgentGrant::DISABLED)
            .map_err(store_err)?;
    }
    let names: Vec<&str> = accounts.iter().map(AccountId::as_str).collect();
    writeln!(
        io.err,
        "Revoked {agent} on {}. Running sessions get no tools from their next call.",
        names.join(", ")
    )?;
    Ok(())
}

fn serve(opts: &Opts, io: &mut Io<'_>) -> Result<(), CliError> {
    fell_core::diagnostic!(INFO, "MCP server starting");
    let (_, store) = open_store(opts, io)?;
    let account = resolve_account(&store, opts.account.as_deref(), io)?.id;
    let agent = opts.agent.clone().unwrap_or_default();
    let mut session = Session::new(store, account, agent).map_err(CliError::new)?;
    // `cli_main` already holds the stdin lock in `io.input`; std's stdin
    // lock is not reentrant, so serve over the same handles.
    session
        .serve(&mut *io.input, &mut *io.out)
        .map_err(|err| CliError::new(err.to_string()))?;
    fell_core::diagnostic!(INFO, "MCP server stopped");
    Ok(())
}

#[cfg(test)]
mod tests;
