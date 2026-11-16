//! `fell-mcp` command line. Grant writes scopes into the local store.
//! Serve speaks MCP. Neither command can send mail.

use std::ffi::OsString;
use std::io;
use std::path::PathBuf;

use fell_core::{AccountId, AgentGrant, Store};

use crate::protocol::Session;

const HELP: &str = "\
fell-mcp — local MCP server for Fell

There is no send tool. Agents are off until granted a scope.

Usage:
  fell-mcp serve  --store PATH --account ID --agent ID
  fell-mcp grant  --store PATH --account ID --agent ID --read [--triage] [--draft]
  fell-mcp revoke --store PATH --account ID --agent ID

Scopes:
  --read     search, read_thread, list_labels
  --triage   label, archive, mark_read, snooze, and undo of those
  --draft    create_draft, list_drafts, and undo of those
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

enum Command {
    Help,
    Serve,
    Grant,
    Revoke,
}

struct Parsed {
    command: Command,
    store: PathBuf,
    account: String,
    agent: String,
    read: bool,
    triage: bool,
    draft: bool,
}

/// `args` is a full argv, including the program name.
pub fn cli_main<I, S>(args: I) -> Result<(), CliError>
where
    I: IntoIterator<Item = S>,
    S: Into<OsString>,
{
    let args: Vec<OsString> = args.into_iter().map(Into::into).skip(1).collect();
    let parsed = parse(&args)?;
    match parsed.command {
        Command::Help => {
            println!("{HELP}");
            Ok(())
        }
        Command::Grant => apply_grant(&parsed, true),
        Command::Revoke => apply_grant(&parsed, false),
        Command::Serve => serve(&parsed),
    }
}

fn parse(args: &[OsString]) -> Result<Parsed, CliError> {
    if args.is_empty() {
        return Err(CliError::new(
            "missing command (serve, grant, revoke, or --help)",
        ));
    }
    let command_name = arg_str(&args[0])?;
    if command_name == "--help" || command_name == "-h" || command_name == "help" {
        return Ok(empty(Command::Help));
    }
    let command = match command_name {
        "serve" => Command::Serve,
        "grant" => Command::Grant,
        "revoke" => Command::Revoke,
        other => {
            return Err(CliError::new(format!(
                "unknown command {other} (serve, grant, revoke, or --help)"
            )));
        }
    };
    let mut store = None;
    let mut account = None;
    let mut agent = None;
    let mut read = false;
    let mut triage = false;
    let mut draft = false;
    let mut index = 1;
    while index < args.len() {
        let flag = arg_str(&args[index])?;
        match flag {
            "--help" | "-h" => return Ok(empty(Command::Help)),
            "--read" => read = true,
            "--triage" => triage = true,
            "--draft" => draft = true,
            "--store" => {
                index += 1;
                store = Some(PathBuf::from(require_value(args, index, "--store")?));
            }
            "--account" => {
                index += 1;
                account = Some(arg_str(&require_value(args, index, "--account")?)?.to_string());
            }
            "--agent" => {
                index += 1;
                agent = Some(arg_str(&require_value(args, index, "--agent")?)?.to_string());
            }
            other if other.starts_with('-') => {
                return Err(CliError::new(format!("unknown flag {other}")));
            }
            other => return Err(CliError::new(format!("unexpected argument {other}"))),
        }
        index += 1;
    }
    if matches!(command, Command::Help) {
        return Ok(empty(Command::Help));
    }
    let store = store.ok_or_else(|| CliError::new("missing --store"))?;
    let account = account.ok_or_else(|| CliError::new("missing --account"))?;
    let agent = agent.ok_or_else(|| CliError::new("missing --agent"))?;
    if account.is_empty() || account != account.trim() {
        return Err(CliError::new("account id is empty"));
    }
    if agent.is_empty()
        || agent != agent.trim()
        || agent.chars().any(|c| c.is_control() || c == '\u{1e}')
    {
        return Err(CliError::new(
            "agent id is empty or contains a control character",
        ));
    }
    match command {
        Command::Grant if !read && !triage && !draft => {
            return Err(CliError::new(
                "grant needs at least one of --read, --triage, --draft",
            ));
        }
        Command::Serve if read || triage || draft => {
            return Err(CliError::new("serve does not take scope flags; use grant"));
        }
        Command::Revoke if read || triage || draft => {
            return Err(CliError::new("revoke clears every scope; omit scope flags"));
        }
        _ => {}
    }
    Ok(Parsed {
        command,
        store,
        account,
        agent,
        read,
        triage,
        draft,
    })
}

fn empty(command: Command) -> Parsed {
    Parsed {
        command,
        store: PathBuf::new(),
        account: String::new(),
        agent: String::new(),
        read: false,
        triage: false,
        draft: false,
    }
}

fn require_value(args: &[OsString], index: usize, flag: &str) -> Result<OsString, CliError> {
    args.get(index)
        .cloned()
        .ok_or_else(|| CliError::new(format!("missing value for {flag}")))
}

fn arg_str(value: &OsString) -> Result<&str, CliError> {
    value
        .to_str()
        .ok_or_else(|| CliError::new("argument is not utf-8"))
}

fn apply_grant(parsed: &Parsed, enable: bool) -> Result<(), CliError> {
    let store = Store::open(&parsed.store).map_err(|err| CliError::new(err.to_string()))?;
    let account = AccountId(parsed.account.clone());
    let grant = if enable {
        AgentGrant::enabled(parsed.read, parsed.triage, parsed.draft)
    } else {
        AgentGrant::DISABLED
    };
    store
        .set_agent_grant(&account, &parsed.agent, grant)
        .map_err(|err| CliError::new(err.to_string()))?;
    if enable {
        eprintln!(
            "fell-mcp: enabled agent {} on account {}",
            parsed.agent, parsed.account
        );
    } else {
        eprintln!(
            "fell-mcp: disabled agent {} on account {}",
            parsed.agent, parsed.account
        );
    }
    Ok(())
}

fn serve(parsed: &Parsed) -> Result<(), CliError> {
    let store = Store::open(&parsed.store).map_err(|err| CliError::new(err.to_string()))?;
    let mut session = Session::new(
        store,
        AccountId(parsed.account.clone()),
        parsed.agent.clone(),
    )
    .map_err(CliError::new)?;
    eprintln!(
        "fell-mcp: serving account {} for agent {} (no send tool)",
        parsed.account, parsed.agent
    );
    let stdin = io::stdin();
    let stdout = io::stdout();
    session
        .serve(stdin.lock(), stdout.lock())
        .map_err(|err| CliError::new(err.to_string()))?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use fell_core::{Account, AccountId, BackendKind};
    use std::sync::atomic::{AtomicU64, Ordering};
    use std::time::{SystemTime, UNIX_EPOCH};

    struct TempDir(PathBuf);

    impl TempDir {
        fn new() -> Self {
            static N: AtomicU64 = AtomicU64::new(0);
            let nanos = SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_nanos();
            let dir = std::env::temp_dir().join(format!(
                "fell-mcp-{}-{}-{}",
                std::process::id(),
                nanos,
                N.fetch_add(1, Ordering::Relaxed)
            ));
            std::fs::create_dir_all(&dir).unwrap();
            Self(dir)
        }

        fn path(&self) -> &std::path::Path {
            &self.0
        }
    }

    impl Drop for TempDir {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    #[test]
    fn grant_and_revoke_round_trip() {
        let dir = TempDir::new();
        let path = dir.path().join("mail.sqlite");
        {
            let store = Store::open(&path).unwrap();
            store
                .upsert_account(&Account {
                    id: AccountId("a1".into()),
                    display_name: "Caleb".into(),
                    email: "c@x".into(),
                    backend_kind: BackendKind::Bridge,
                })
                .unwrap();
        }
        let path_str = path.to_str().unwrap();
        cli_main([
            "fell-mcp",
            "grant",
            "--store",
            path_str,
            "--account",
            "a1",
            "--agent",
            "bot",
            "--read",
        ])
        .unwrap();
        {
            let store = Store::open(&path).unwrap();
            let grant = store.agent_grant(&AccountId("a1".into()), "bot").unwrap();
            assert!(grant.enabled && grant.read && !grant.triage && !grant.draft);
        }
        cli_main([
            "fell-mcp",
            "revoke",
            "--store",
            path_str,
            "--account",
            "a1",
            "--agent",
            "bot",
        ])
        .unwrap();
        let store = Store::open(&path).unwrap();
        assert_eq!(
            store.agent_grant(&AccountId("a1".into()), "bot").unwrap(),
            AgentGrant::DISABLED
        );
    }

    #[test]
    fn grant_requires_a_scope_and_a_known_account() {
        let err = cli_main([
            "fell-mcp",
            "grant",
            "--store",
            "x",
            "--account",
            "a",
            "--agent",
            "bot",
        ])
        .unwrap_err();
        assert!(err.to_string().contains("at least one"));
        let dir = TempDir::new();
        let path = dir.path().join("mail.sqlite");
        Store::open(&path).unwrap();
        let err = cli_main([
            "fell-mcp",
            "grant",
            "--store",
            path.to_str().unwrap(),
            "--account",
            "missing",
            "--agent",
            "bot",
            "--draft",
        ])
        .unwrap_err();
        assert!(err.to_string().contains("unknown account"));
        let err = cli_main([
            "fell-mcp",
            "serve",
            "--store",
            "x",
            "--account",
            "a",
            "--agent",
            "bot",
            "--read",
        ])
        .unwrap_err();
        assert!(err.to_string().contains("does not take scope flags"));
    }

    #[test]
    fn help_mentions_that_there_is_no_send_tool() {
        assert!(HELP.contains("no send tool"));
        assert!(!HELP.lines().any(|line| line.starts_with("fell-mcp send")));
    }
}
