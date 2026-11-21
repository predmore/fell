use std::io::{BufRead, BufReader, Write};
use std::os::unix::fs::{DirBuilderExt, PermissionsExt};
use std::path::Path;
use std::process::{Child, ChildStdin, Command, Stdio};
use std::sync::mpsc::{self, Receiver};
use std::time::Duration;

use fell_core::{Account, AccountId, AgentGrant, BackendKind, Store};
use serde_json::{Value, json};

struct Server {
    child: Child,
    input: ChildStdin,
    output: Receiver<Value>,
}

impl Server {
    fn start(root: &Path) -> Self {
        let mut command = Command::new(env!("CARGO_BIN_EXE_fell-mcp"));
        command
            .args(["serve", "--account", "demo", "--agent", "codex"])
            .env_clear()
            .env("HOME", root)
            .env("FELL_DRAFT_SOCK", root.join("draft.sock"))
            .env("FELL_DRAFT_ROOT", root.join("drafts"))
            .stdin(Stdio::piped())
            .stdout(Stdio::piped());
        for (name, dir) in [
            ("XDG_CONFIG_HOME", "config"),
            ("XDG_DATA_HOME", "data"),
            ("XDG_CACHE_HOME", "cache"),
            ("XDG_STATE_HOME", "state"),
            ("XDG_RUNTIME_DIR", "runtime"),
        ] {
            let path = root.join(dir);
            std::fs::DirBuilder::new()
                .recursive(true)
                .mode(0o700)
                .create(&path)
                .unwrap();
            command.env(name, path);
        }
        assert_eq!(
            root.join("runtime")
                .metadata()
                .unwrap()
                .permissions()
                .mode()
                & 0o777,
            0o700
        );
        let mut child = command.spawn().unwrap();
        let input = child.stdin.take().unwrap();
        let stdout = child.stdout.take().unwrap();
        let (tx, output) = mpsc::channel();
        std::thread::spawn(move || {
            for line in BufReader::new(stdout).lines() {
                let response =
                    serde_json::from_str(&line.unwrap()).expect("one JSON frame per line");
                if tx.send(response).is_err() {
                    break;
                }
            }
        });
        Self {
            child,
            input,
            output,
        }
    }

    fn send(&mut self, request: Value) {
        writeln!(self.input, "{request}").unwrap();
    }

    fn request(&mut self, id: u64, method: &str, params: Value) -> Value {
        self.send(json!({"jsonrpc": "2.0", "id": id, "method": method, "params": params}));
        let response = self
            .output
            .recv_timeout(Duration::from_secs(10))
            .expect("fell-mcp did not answer");
        assert_eq!(response["id"], id, "{response}");
        response["result"].clone()
    }
}

impl Drop for Server {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

#[test]
fn a_running_server_sees_revoke_and_undo_on_its_next_request() {
    let sandbox = tempfile::tempdir().unwrap();
    let db = sandbox.path().join("data/fell/mail.db");
    std::fs::create_dir_all(db.parent().unwrap()).unwrap();
    let store = Store::open(db).unwrap();
    let account = AccountId("demo".into());
    store
        .upsert_account(&Account {
            id: account.clone(),
            display_name: "Demo".into(),
            email: "demo@example.com".into(),
            backend_kind: BackendKind::Bridge,
        })
        .unwrap();
    let initial = AgentGrant::enabled(true, false, true);
    store.set_agent_grant(&account, "codex", initial).unwrap();
    let first = store.list_agent_grants().unwrap().pop().unwrap();

    let mut server = Server::start(sandbox.path());
    let init = server.request(1, "initialize", json!({"protocolVersion": "2025-06-18"}));
    assert_eq!(init["protocolVersion"], "2025-06-18");
    server.send(json!({"jsonrpc": "2.0", "method": "notifications/initialized"}));
    let tools = server.request(2, "tools/list", json!({}));
    assert!(
        tools["tools"]
            .as_array()
            .unwrap()
            .iter()
            .any(|tool| tool["name"] == "list_folders")
    );

    // The UI uses this same revision-checked write on its own connection.
    let revoked = store
        .update_agent_grant(&first, AgentGrant::DISABLED)
        .unwrap()
        .unwrap();
    assert_eq!(
        server.request(3, "tools/list", json!({}))["tools"],
        json!([])
    );
    let call = json!({"name": "list_folders", "arguments": {}});
    assert_eq!(
        server.request(4, "tools/call", call.clone())["isError"],
        true
    );

    store
        .update_agent_grant(&revoked, initial)
        .unwrap()
        .unwrap();
    assert_eq!(server.request(5, "tools/list", json!({})), tools);
    assert_eq!(server.request(6, "tools/call", call)["isError"], false);
}
