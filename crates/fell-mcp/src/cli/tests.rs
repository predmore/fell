use super::*;
use fell_core::{Account, AccountId, BackendKind};
use serde_json::{Value, json};
use std::os::unix::fs::PermissionsExt;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

const NOW: i64 = 1_800_000_000;
const EXE: &str = "/opt/fell/fell-mcp";

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

fn account(id: &str) -> Account {
    Account {
        id: AccountId(id.into()),
        display_name: "Demo".into(),
        email: format!("{id}@example.com"),
        backend_kind: BackendKind::Bridge,
    }
}

/// A home folder and Fell's mailbox in it, with one account.
struct Ctx {
    dir: TempDir,
    appimage: Option<PathBuf>,
    name: &'static str,
    /// HOME unset (or relative, which `Env::from_process` also drops).
    no_home: bool,
}

struct Ran {
    out: String,
    err: String,
    result: Result<(), CliError>,
}

impl Ctx {
    fn new() -> Self {
        let ctx = Self::empty();
        Store::open(ctx.store())
            .unwrap()
            .upsert_account(&account("demo"))
            .unwrap();
        ctx
    }

    fn empty() -> Self {
        let dir = TempDir::new();
        std::fs::create_dir_all(dir.path().join("home")).unwrap();
        Self {
            dir,
            appimage: None,
            name: "fell-mcp",
            no_home: false,
        }
    }

    fn store(&self) -> PathBuf {
        self.dir.path().join("mail.db")
    }

    fn home(&self) -> PathBuf {
        self.dir.path().join("home")
    }

    fn run(&self, args: &[&str], input: &str, tty: bool) -> Ran {
        let args: Vec<OsString> = args.iter().skip(1).map(OsString::from).collect();
        let mut input = input.as_bytes();
        let mut out = Vec::new();
        let mut err = Vec::new();
        let mut io = Io {
            input: &mut input,
            out: &mut out,
            err: &mut err,
            tty,
            env: Env {
                home: (!self.no_home).then(|| self.home()),
                ..Env::default()
            },
            default_store: Some(self.store()),
            exe: Some(EXE.into()),
            appimage: self.appimage.clone(),
            name: self.name,
            now_unix: NOW,
        };
        let result = run(&args, &mut io);
        let tidy = |bytes: Vec<u8>| {
            String::from_utf8(bytes)
                .unwrap()
                .replace(self.dir.path().to_str().unwrap(), "$TMP")
        };
        Ran {
            out: tidy(out),
            err: tidy(err),
            result,
        }
    }

    fn grant(&self, agent: &str) -> AgentGrant {
        Store::open(self.store())
            .unwrap()
            .agent_grant(&AccountId("demo".into()), agent)
            .unwrap()
    }
}

fn golden(name: &str, actual: &str) {
    let path = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("src/cli/golden")
        .join(name);
    if std::env::var_os("UPDATE_GOLDEN").is_some() {
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(&path, actual).unwrap();
        return;
    }
    let expected = std::fs::read_to_string(&path)
        .unwrap_or_else(|_| panic!("missing {}; run with UPDATE_GOLDEN=1", path.display()));
    assert_eq!(
        actual, expected,
        "{name} changed; rerun with UPDATE_GOLDEN=1 if that is intended"
    );
}

#[test]
fn add_prints_the_exact_entry_for_each_harness() {
    for harness in Harness::ALL {
        let ctx = Ctx::new();
        let ran = ctx.run(&["fell-mcp", "add", harness.id(), "--print"], "", false);
        ran.result.unwrap();
        golden(
            &format!("add-{}.txt", harness.id()),
            &format!("--- stdout\n{}--- stderr\n{}", ran.out, ran.err),
        );
        assert_eq!(
            ctx.grant(harness.id()),
            AgentGrant::enabled(true, false, true)
        );
        // Printing writes nothing outside the store.
        assert_eq!(std::fs::read_dir(ctx.home()).unwrap().count(), 0);
    }
}

#[test]
fn add_at_a_terminal_asks_for_scopes_and_writes_after_a_yes() {
    // Under umask 077 a create mode of 0640 would come out 0600.
    if !crate::setup::tests::in_child_with_umask(
        "cli::tests::add_at_a_terminal_asks_for_scopes_and_writes_after_a_yes",
        0o077,
    ) {
        return;
    }
    let ctx = Ctx::new();
    let codex = ctx.home().join(".codex");
    std::fs::create_dir_all(&codex).unwrap();
    let config = codex.join("config.toml");
    let original = "model = \"o4\"\n\n[mcp_servers.docs]\ncommand = \"docs-mcp\"\n";
    std::fs::write(&config, original).unwrap();
    std::fs::set_permissions(&config, std::fs::Permissions::from_mode(0o640)).unwrap();

    // read: yes (default), draft: no, triage: yes, write: yes.
    let ran = ctx.run(&["fell-mcp", "add", "codex"], "\nn\ny\ny\n", true);
    ran.result.unwrap();
    assert_eq!(ctx.grant("codex"), AgentGrant::enabled(true, true, false));
    assert!(
        ran.err
            .contains("Let codex search and read your mail? [Y/n]")
    );
    assert!(
        ran.err
            .contains("Let codex triage (label, move, archive, star, snooze)? [y/N]")
    );
    assert!(
        ran.err
            .contains("Granted codex read, triage on demo (demo@example.com).")
    );
    assert!(ran.err.contains("Add this to ~/.codex/config.toml? [y/N]"));
    assert!(ran.err.contains("Added fell to ~/.codex/config.toml. The old file is ~/.codex/config.toml.fell-backup-1800000000."));

    let written = std::fs::read_to_string(&config).unwrap();
    assert!(written.starts_with(original), "{written}");
    assert!(written.contains("[mcp_servers.fell]\ncommand = \"/opt/fell/fell-mcp\"\n"));
    let backup = codex.join("config.toml.fell-backup-1800000000");
    assert_eq!(std::fs::read_to_string(&backup).unwrap(), original);
    let mode = |path: &Path| std::fs::metadata(path).unwrap().permissions().mode() & 0o777;
    assert_eq!(mode(&config), 0o640);
    assert_eq!(mode(&backup), 0o600);
    // No stray temp files.
    assert_eq!(std::fs::read_dir(&codex).unwrap().count(), 2);

    // Running it again changes nothing.
    let ran = ctx.run(
        &["fell-mcp", "add", "codex", "--read", "--write"],
        "",
        false,
    );
    ran.result.unwrap();
    assert!(
        ran.err
            .contains("~/.codex/config.toml already has this entry.")
    );
    assert_eq!(ctx.grant("codex"), AgentGrant::enabled(true, false, false));
}

#[test]
fn add_at_a_terminal_writes_nothing_on_the_default_answer() {
    let ctx = Ctx::new();
    let ran = ctx.run(&["fell-mcp", "add", "gemini"], "\n\n\n\n", true);
    ran.result.unwrap();
    assert_eq!(ctx.grant("gemini"), AgentGrant::enabled(true, false, true));
    assert!(!ctx.home().join(".gemini").exists());
    let ran = ctx.run(&["fell-mcp", "add", "gemini"], "n\nn\nn\n", true);
    assert!(
        ran.result
            .unwrap_err()
            .to_string()
            .contains("nothing granted")
    );
    // The earlier grant stands.
    assert_eq!(ctx.grant("gemini"), AgentGrant::enabled(true, false, true));
}

#[test]
fn add_without_a_terminal_grants_read_and_draft_and_does_not_write() {
    let ctx = Ctx::new();
    let ran = ctx.run(&["fell-mcp", "add", "opencode"], "", false);
    ran.result.unwrap();
    assert_eq!(
        ctx.grant("opencode"),
        AgentGrant::enabled(true, false, true)
    );
    assert!(
        ran.err
            .contains("Run again with --write to add it to ~/.config/opencode/opencode.json.")
    );
    assert!(!ctx.home().join(".config").exists());

    let ran = ctx.run(
        &[
            "fell-mcp",
            "add",
            "opencode",
            "--triage",
            "--write",
            "--agent",
            "oc-triage",
        ],
        "",
        false,
    );
    ran.result.unwrap();
    assert_eq!(
        ctx.grant("oc-triage"),
        AgentGrant::enabled(false, true, false)
    );
    assert!(
        ran.err
            .contains("Created ~/.config/opencode/opencode.json.")
    );
    let path = ctx.home().join(".config/opencode/opencode.json");
    let value: Value = serde_json::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
    assert_eq!(
        value["mcp"]["servers"]["fell"]["command"]
            .as_array()
            .unwrap()
            .last()
            .unwrap(),
        "oc-triage"
    );
    let mode = std::fs::metadata(&path).unwrap().permissions().mode() & 0o777;
    assert_eq!(mode, 0o600);
}

#[test]
fn write_merges_into_an_existing_json_config() {
    let ctx = Ctx::new();
    let dir = ctx.home().join(".gemini");
    std::fs::create_dir_all(&dir).unwrap();
    let path = dir.join("settings.json");
    let original = r#"{"theme":"Dracula","mcpServers":{"github":{"command":"gh-mcp","env":{"GITHUB_TOKEN":"$GH"}},"fell":{"command":"old"}},"autoAccept":false}"#;
    std::fs::write(&path, original).unwrap();
    let ran = ctx.run(&["fell-mcp", "add", "gemini", "--write"], "", false);
    ran.result.unwrap();
    assert!(
        ran.err
            .contains("Replaced the fell entry in ~/.gemini/settings.json.")
    );
    let value: Value = serde_json::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
    let keys: Vec<_> = value.as_object().unwrap().keys().collect();
    assert_eq!(keys, ["theme", "mcpServers", "autoAccept"]);
    assert_eq!(value["mcpServers"]["github"]["env"]["GITHUB_TOKEN"], "$GH");
    assert_eq!(value["mcpServers"]["fell"]["command"], EXE);
    assert!(value["mcpServers"]["fell"].get("env").is_none());
    assert!(value["mcpServers"]["fell"].get("trust").is_none());
    let backups: Vec<_> = std::fs::read_dir(&dir)
        .unwrap()
        .map(|entry| entry.unwrap().file_name().into_string().unwrap())
        .filter(|name| name.contains("fell-backup"))
        .collect();
    assert_eq!(backups.len(), 1);
    assert_eq!(
        std::fs::read_to_string(dir.join(&backups[0])).unwrap(),
        original
    );
}

#[test]
fn write_refuses_symlinks_and_files_it_cannot_parse() {
    let ctx = Ctx::new();
    let elsewhere = ctx.dir.path().join("elsewhere.json");
    std::fs::write(&elsewhere, "{}").unwrap();
    let dir = ctx.home().join(".gemini");
    std::fs::create_dir_all(&dir).unwrap();
    std::os::unix::fs::symlink(&elsewhere, dir.join("settings.json")).unwrap();
    let ran = ctx.run(&["fell-mcp", "add", "gemini", "--write"], "", false);
    // A write that was asked for and refused exits non-zero, and says the
    // grant stands.
    assert_eq!(
        ran.result.unwrap_err().to_string(),
        "did not write ~/.gemini/settings.json"
    );
    assert_eq!(ctx.grant("gemini"), AgentGrant::enabled(true, false, true));
    assert!(
        ran.err
            .contains("Did not write: ~/.gemini/settings.json is a symlink"),
        "{}",
        ran.err
    );
    assert!(
        ran.err.contains(
            "The grant is saved. Paste the entry above into ~/.gemini/settings.json yourself, \
             or undo the grant with `fell-mcp revoke gemini`."
        ),
        "{}",
        ran.err
    );
    assert_eq!(std::fs::read_to_string(&elsewhere).unwrap(), "{}");
    assert!(!ran.out.is_empty(), "the entry is still printed to paste");

    // A symlinked config folder.
    let real = ctx.dir.path().join("real-goose");
    std::fs::create_dir_all(&real).unwrap();
    std::fs::create_dir_all(ctx.home().join(".config")).unwrap();
    std::os::unix::fs::symlink(&real, ctx.home().join(".config/goose")).unwrap();
    let ran = ctx.run(&["fell-mcp", "add", "goose", "--write"], "", false);
    assert!(ran.result.is_err());
    assert!(
        ran.err
            .contains("Did not write: ~/.config/goose is a symlink"),
        "{}",
        ran.err
    );
    assert_eq!(std::fs::read_dir(&real).unwrap().count(), 0);

    // OpenCode's JSON-with-comments file.
    let oc = ctx.home().join(".config/opencode");
    std::fs::create_dir_all(&oc).unwrap();
    std::fs::write(oc.join("opencode.jsonc"), "{ // mine\n}").unwrap();
    let ran = ctx.run(&["fell-mcp", "add", "opencode", "--write"], "", false);
    assert!(ran.result.is_err());
    assert!(ran.err.contains("opencode.jsonc exists"), "{}", ran.err);
    assert!(!oc.join("opencode.json").exists());

    // Codex TOML that does not parse is left alone.
    let codex = ctx.home().join(".codex");
    std::fs::create_dir_all(&codex).unwrap();
    std::fs::write(codex.join("config.toml"), "[broken\n").unwrap();
    let ran = ctx.run(&["fell-mcp", "add", "codex", "--write"], "", false);
    assert!(ran.result.is_err());
    assert!(
        ran.err.contains("~/.codex/config.toml is not valid TOML"),
        "{}",
        ran.err
    );
    assert_eq!(
        std::fs::read_to_string(codex.join("config.toml")).unwrap(),
        "[broken\n"
    );
}

#[test]
fn write_merges_goose_yaml_without_touching_other_extensions() {
    let ctx = Ctx::new();
    let dir = ctx.home().join(".config/goose");
    std::fs::create_dir_all(&dir).unwrap();
    let original = "\
GOOSE_PROVIDER: anthropic
extensions:
  developer:
    bundled: true
    enabled: true
    name: developer
    timeout: 300
    type: builtin
";
    std::fs::write(dir.join("config.yaml"), original).unwrap();
    let ran = ctx.run(&["fell-mcp", "add", "goose", "--write"], "", false);
    ran.result.unwrap();
    assert!(
        ran.err
            .contains("Added fell to ~/.config/goose/config.yaml."),
        "{}",
        ran.err
    );
    let written = std::fs::read_to_string(dir.join("config.yaml")).unwrap();
    assert!(
        written.starts_with("GOOSE_PROVIDER: anthropic\nextensions:\n  fell:\n"),
        "{written}"
    );
    assert!(written.ends_with("  developer:\n    bundled: true\n    enabled: true\n    name: developer\n    timeout: 300\n    type: builtin\n"));
}

#[test]
fn claude_code_gets_its_own_command_and_no_file_is_written() {
    let ctx = Ctx::new();
    let ran = ctx.run(&["fell-mcp", "add", "claude-code", "--write"], "", false);
    ran.result.unwrap();
    assert!(
        ran.out
            .starts_with("claude mcp add --scope user fell -- /opt/fell/fell-mcp serve --store ")
    );
    assert!(ran.err.contains("Fell does not edit Claude Code's config"));
    assert!(!ctx.home().join(".claude.json").exists());
    assert_eq!(std::fs::read_dir(ctx.home()).unwrap().count(), 0);
}

#[test]
fn add_never_puts_scopes_or_secrets_in_the_entry() {
    let ctx = Ctx::new();
    for harness in Harness::ALL {
        let ran = ctx.run(
            &["fell-mcp", "add", harness.id(), "--draft", "--print"],
            "",
            false,
        );
        ran.result.unwrap();
        let lower = ran.out.to_ascii_lowercase();
        for word in [
            "--read", "--triage", "--draft", "token", "api_key", "apikey", "secret", "password",
        ] {
            assert!(!lower.contains(word), "{} entry has {word}", harness.id());
        }
        assert_eq!(
            ctx.grant(harness.id()),
            AgentGrant::enabled(false, false, true)
        );
    }
}

#[test]
fn grant_list_revoke_round_trip_empties_a_running_session() {
    let ctx = Ctx::new();
    let ran = ctx.run(&["fell-mcp", "list"], "", false);
    ran.result.unwrap();
    assert_eq!(
        ran.out,
        "No agents yet. Connect one with `fell-mcp add HARNESS`.\n"
    );

    ctx.run(&["fell-mcp", "add", "codex", "--print"], "", false)
        .result
        .unwrap();
    ctx.run(
        &["fell-mcp", "grant", "goose", "--read", "--triage"],
        "",
        false,
    )
    .result
    .unwrap();

    // A session that is already running, as a harness would hold it.
    let mut session = Session::new(
        Store::open(ctx.store()).unwrap(),
        AccountId("demo".into()),
        "codex",
    )
    .unwrap();
    let call = |session: &mut Session, id: i64, method: &str, params: Value| -> Value {
        let line = json!({ "jsonrpc": "2.0", "id": id, "method": method, "params": params });
        serde_json::from_str(&session.handle_line(&line.to_string()).unwrap()).unwrap()
    };
    call(
        &mut session,
        1,
        "initialize",
        json!({ "protocolVersion": "2025-06-18" }),
    );
    let tools = call(&mut session, 2, "tools/list", json!({}));
    let names: Vec<_> = tools["result"]["tools"]
        .as_array()
        .unwrap()
        .iter()
        .map(|tool| tool["name"].as_str().unwrap().to_string())
        .collect();
    assert!(names.contains(&"search".to_string()) && names.contains(&"create_draft".to_string()));
    assert!(
        !names.contains(&"archive".to_string()),
        "triage was not granted"
    );

    // The goose agent archives something, so it has a last action.
    Store::open(ctx.store())
        .unwrap()
        .upsert_message(&fell_core::Message {
            account_id: AccountId("demo".into()),
            headers: fell_core::MessageHeaders {
                message_id: fell_core::MessageId::new("m1@x"),
                proton_conversation_id: None,
                in_reply_to: None,
                references: Vec::new(),
                from: fell_core::Address {
                    name: None,
                    email: "a@example.com".into(),
                },
                subject: "hi".into(),
                date_unix: 1,
            },
            remote_id: fell_core::RemoteId("r1".into()),
            thread_id: fell_core::ThreadId::from_message_id(&fell_core::MessageId::new("m1@x")),
            snippet: String::new(),
            body_text: None,
            unread: true,
            folders: vec!["INBOX".into()],
            labels: Vec::new(),
            arrived_via: Vec::new(),
            to: Vec::new(),
            cc: Vec::new(),
            bcc: Vec::new(),
            starred: false,
            has_attachment: false,
            archived: false,
            snooze_until: None,
            list_id: None,
            reply_to: Vec::new(),
            attachment_names: Vec::new(),
            parts: None,
        })
        .and_then(|thread| {
            Store::open(ctx.store())?.agent_archive_thread(
                &AccountId("demo".into()),
                &thread,
                "agent:goose",
            )
        })
        .unwrap();

    let ran = ctx.run(&["fell-mcp", "list"], "", false);
    ran.result.unwrap();
    let lines: Vec<_> = ran.out.lines().collect();
    assert_eq!(lines[0], "AGENT  ACCOUNT  SCOPES        LAST ACTION");
    assert_eq!(lines[1], "codex  demo     read, draft   never");
    assert!(
        lines[2].starts_with("goose  demo     read, triage  "),
        "{}",
        lines[2]
    );
    assert_eq!(lines.len(), 3);

    let ran = ctx.run(&["fell-mcp", "revoke", "codex"], "", false);
    ran.result.unwrap();
    assert_eq!(
        ran.err,
        "Revoked codex on demo. Running sessions get no tools from their next call.\n"
    );
    let tools = call(&mut session, 3, "tools/list", json!({}));
    assert_eq!(tools["result"]["tools"], json!([]));
    let denied = call(
        &mut session,
        4,
        "tools/call",
        json!({ "name": "search", "arguments": { "query": "hi" } }),
    );
    assert_eq!(denied["result"]["isError"], true);

    let ran = ctx.run(&["fell-mcp", "list"], "", false);
    assert!(
        ran.out
            .lines()
            .nth(1)
            .unwrap()
            .starts_with("codex  demo     revoked")
    );
    let ran = ctx.run(&["fell-mcp", "revoke", "codex"], "", false);
    ran.result.unwrap();
    assert_eq!(ran.err, "codex is already revoked.\n");
    let err = ctx
        .run(&["fell-mcp", "revoke", "nobody"], "", false)
        .result
        .unwrap_err();
    assert!(err.to_string().contains("no agent named nobody"));
}

#[test]
fn ago_reads_naturally() {
    assert_eq!(ago(NOW, NOW - 5), "just now");
    assert_eq!(ago(NOW, NOW - 125), "2 min ago");
    assert_eq!(ago(NOW, NOW - 3 * 3_600), "3 h ago");
    assert_eq!(ago(NOW, NOW - 90_000), "yesterday");
    assert_eq!(ago(NOW, NOW - 10 * 86_400), "10 days ago");
    assert_eq!(ago(NOW, NOW + 50), "just now");
}

#[test]
fn accounts_resolve_to_the_only_one_or_ask() {
    let ctx = Ctx::new();
    Store::open(ctx.store())
        .unwrap()
        .upsert_account(&account("work"))
        .unwrap();
    let err = ctx
        .run(&["fell-mcp", "add", "codex"], "", false)
        .result
        .unwrap_err();
    assert_eq!(
        err.to_string(),
        "this mailbox has 2 accounts (demo, work); pick one with --account"
    );
    let ran = ctx.run(
        &["fell-mcp", "add", "codex", "--print"],
        "9\n2\n\n\n\n",
        true,
    );
    ran.result.unwrap();
    assert!(ran.err.contains("  2  work (work@example.com)"));
    assert!(ran.out.contains("\"work\""));
    let err = ctx
        .run(
            &["fell-mcp", "add", "codex", "--account", "home"],
            "",
            false,
        )
        .result
        .unwrap_err();
    assert_eq!(
        err.to_string(),
        "unknown account home (this mailbox has demo, work)"
    );
}

#[test]
fn a_missing_mailbox_is_not_created() {
    let ctx = Ctx::empty();
    let err = ctx
        .run(&["fell-mcp", "add", "codex"], "", false)
        .result
        .unwrap_err();
    assert!(err.to_string().contains("fell account setup"), "{err}");
    assert!(!ctx.store().exists());
    let err = ctx
        .run(&["fell-mcp", "list"], "", false)
        .result
        .unwrap_err();
    assert!(err.to_string().contains("no Fell mailbox"));
}

#[test]
fn inside_an_appimage_harnesses_start_the_appimage() {
    let mut ctx = Ctx::new();
    ctx.appimage = Some(ctx.home().join("Apps/fell-x86_64.AppImage"));
    let ran = ctx.run(&["fell-mcp", "add", "codex", "--print"], "", false);
    ran.result.unwrap();
    assert!(
        ran.out
            .contains("command = \"$TMP/home/Apps/fell-x86_64.AppImage\"\n")
    );
    assert!(ran.out.contains("args = [\"mcp\", \"serve\", \"--store\""));
    assert!(
        ran.err
            .contains("`~/Apps/fell-x86_64.AppImage mcp revoke codex`")
    );

    // A mounted AppDir with this program in usr/bin, and the AppImage file.
    let root = ctx.dir.path();
    let appdir = root.join(".mount_fellX");
    std::fs::create_dir_all(appdir.join("usr/bin")).unwrap();
    let bundled = appdir.join("usr/bin/fell-mcp");
    std::fs::write(&bundled, "").unwrap();
    let image = root.join("fell.AppImage");
    std::fs::write(&image, "").unwrap();
    let detect = |exe: &Path, image: &Path, appdir: &Path| {
        inside_appimage(Some(exe), Some(image.into()), Some(appdir.into()))
    };
    assert_eq!(detect(&bundled, &image, &appdir), Some(image.clone()));
    // A hard link is the same file.
    let linked = root.join("linked-fell-mcp");
    std::fs::hard_link(&bundled, &linked).unwrap();
    assert_eq!(detect(&linked, &image, &appdir), Some(image.clone()));

    // A program outside the AppDir, even one under it, is not bundled.
    let other = appdir.join("usr/bin/other");
    std::fs::write(&other, "").unwrap();
    assert_eq!(detect(&other, &image, &appdir), None);
    let installed = root.join("usr/bin/fell-mcp");
    std::fs::create_dir_all(installed.parent().unwrap()).unwrap();
    std::fs::write(&installed, "").unwrap();
    assert_eq!(detect(&installed, &image, &appdir), None);
    // `APPDIR=/` or an install prefix would make an installed fell-mcp look
    // bundled; `..` could spell the same.
    for spoofed in ["/", "/usr", "/usr/local", "/opt"] {
        assert_eq!(
            detect(&bundled, &image, Path::new(spoofed)),
            None,
            "{spoofed}"
        );
    }
    assert_eq!(detect(&bundled, &image, &appdir.join("usr/..")), None);
    // APPIMAGE must be a real file, given as a plain absolute path.
    assert_eq!(
        detect(&bundled, &root.join("missing.AppImage"), &appdir),
        None
    );
    assert_eq!(detect(&bundled, &appdir, &appdir), None);
    assert_eq!(detect(&bundled, Path::new("fell.AppImage"), &appdir), None);
    assert_eq!(
        inside_appimage(Some(&bundled), None, Some(appdir.clone())),
        None
    );
    assert_eq!(inside_appimage(Some(&bundled), Some(image), None), None);
}

#[test]
fn without_home_add_says_so_and_a_write_fails() {
    let mut ctx = Ctx::new();
    ctx.no_home = true;
    let ran = ctx.run(&["fell-mcp", "add", "codex"], "", false);
    ran.result.unwrap();
    assert!(
        ran.err.contains(
            "HOME is not set to an absolute path, so Fell cannot find Codex's config. \
             Add this to it yourself:"
        ),
        "{}",
        ran.err
    );
    assert!(!ran.err.contains("rewrites that file"), "{}", ran.err);
    assert!(ran.out.contains("[mcp_servers.fell]"));
    let ran = ctx.run(&["fell-mcp", "add", "goose", "--write"], "", false);
    assert_eq!(
        ran.result.unwrap_err().to_string(),
        "did not write the config: HOME is not set to an absolute path"
    );
    assert!(ran.out.contains("extensions:"));
    assert_eq!(std::fs::read_dir(ctx.home()).unwrap().count(), 0);
}

#[test]
fn hints_name_the_command_the_person_ran() {
    let mut ctx = Ctx::new();
    ctx.name = FELL_MCP_ARG0;
    let ran = ctx.run(&["fell mcp", "add", "goose", "--print"], "", false);
    ran.result.unwrap();
    assert!(ran.err.contains("`fell mcp revoke goose`"), "{}", ran.err);
}

#[test]
fn flags_are_checked() {
    let ctx = Ctx::new();
    let err = |args: &[&str]| ctx.run(args, "", false).result.unwrap_err().to_string();
    assert!(err(&["fell-mcp", "add"]).contains("add needs one harness"));
    assert!(err(&["fell-mcp", "add", "cursor"]).contains("unknown harness cursor"));
    assert!(err(&["fell-mcp", "add", "codex", "--write", "--print"]).contains("pick one"));
    assert!(err(&["fell-mcp", "list", "--read"]).contains("list takes only --store"));
    assert!(err(&["fell-mcp", "grant", "--write", "x", "--read"]).contains("for add"));
    assert!(err(&["fell-mcp", "revoke"]).contains("missing agent"));
    assert!(err(&["fell-mcp", "revoke", "a", "b"]).contains("name one agent"));
    assert!(err(&["fell-mcp", "revoke", "a", "--draft"]).contains("omit scope flags"));
    assert!(err(&["fell-mcp", "serve", "--account", "demo"]).contains("missing --agent"));
    assert!(err(&["fell-mcp", "frobnicate"]).contains("unknown command"));
}

#[test]
fn grant_and_revoke_round_trip() {
    let dir = TempDir::new();
    let path = dir.path().join("mail.sqlite");
    {
        let store = Store::open(&path).unwrap();
        store.upsert_account(&account("a1")).unwrap();
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
    assert!(
        !HELP
            .lines()
            .any(|line| line.trim_start().starts_with("fell-mcp send"))
    );
}
