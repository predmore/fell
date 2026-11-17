//! Local socket the editor serves so `fell-mcp` reads the draft on screen.
//!
//! The socket is mode 0600 under the user's runtime directory. It is not a
//! network listener, and no request on it transmits mail.

use std::collections::HashMap;
use std::io::{BufRead, BufReader, Write};
use std::os::unix::fs::PermissionsExt;
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::Path;
use std::sync::mpsc::{self, Receiver, Sender};
use std::sync::{Arc, Condvar, Mutex};
use std::time::Duration;

use serde_json::{Value, json};

use super::session::DraftSession;
use super::workspace::{self, draft_socket_path};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct HubEvent;

pub struct DraftHub {
    sessions: Mutex<HashMap<String, DraftSession>>,
    listeners: Mutex<Vec<Sender<()>>>,
    focused: Mutex<Option<String>>,
    shown: Mutex<Option<String>>,
    shown_cv: Condvar,
}

impl Default for DraftHub {
    fn default() -> Self {
        Self::new()
    }
}

impl DraftHub {
    pub fn new() -> Self {
        Self {
            sessions: Mutex::new(HashMap::new()),
            listeners: Mutex::new(Vec::new()),
            focused: Mutex::new(None),
            shown: Mutex::new(None),
            shown_cv: Condvar::new(),
        }
    }

    pub fn subscribe(&self) -> Receiver<()> {
        let (tx, rx) = mpsc::channel();
        self.listeners.lock().expect("listeners").push(tx);
        rx
    }

    /// Add a live draft. Its saves run on a writer thread from now on, so
    /// holding the session lock never waits for disk.
    pub fn insert(&self, mut session: DraftSession) {
        session.attach_writer();
        let id = session.id().to_string();
        self.sessions.lock().expect("sessions").insert(id, session);
        self.emit();
    }

    pub fn with_session<T>(
        &self,
        id: &str,
        body: impl FnOnce(&mut DraftSession) -> T,
    ) -> Option<T> {
        let mut sessions = self.sessions.lock().expect("sessions");
        let session = sessions.get_mut(id)?;
        let before = session.paint_generation();
        let value = body(session);
        let changed = session.paint_generation() != before;
        drop(sessions);
        if changed {
            self.emit();
        }
        Some(value)
    }

    /// Wait until every live draft's queued save is on disk. The app calls
    /// this when it quits.
    pub fn settle_all(&self) {
        let waiters: Vec<super::SaveWaiter> = self
            .sessions
            .lock()
            .expect("sessions")
            .values()
            .filter_map(DraftSession::save_waiter)
            .collect();
        for waiter in waiters {
            let _ = waiter.wait();
        }
    }

    /// The compose window calls this after it is showing `id`.
    pub fn note_shown(&self, id: &str) {
        let mut shown = self.shown.lock().expect("shown");
        if shown.as_deref() == Some(id) {
            return;
        }
        *shown = Some(id.to_string());
        self.shown_cv.notify_all();
    }

    fn wait_shown(&self, id: &str) -> bool {
        {
            let shown = self.shown.lock().expect("shown");
            if shown.as_deref() == Some(id) {
                return true;
            }
        }
        if self.listeners.lock().expect("listeners").is_empty() {
            return false;
        }
        let shown = self.shown.lock().expect("shown");
        if shown.as_deref() == Some(id) {
            return true;
        }
        let (shown, _) = self
            .shown_cv
            .wait_timeout(shown, Duration::from_millis(500))
            .expect("shown");
        shown.as_deref() == Some(id)
    }

    pub fn snapshot_ids(&self) -> Vec<String> {
        self.sessions
            .lock()
            .expect("sessions")
            .keys()
            .cloned()
            .collect()
    }

    pub fn focused_id(&self) -> Option<String> {
        self.focused.lock().expect("focused").clone()
    }

    pub fn handle_line(&self, line: &str) -> String {
        let request: Value = match serde_json::from_str(line) {
            Ok(value) => value,
            Err(_) => {
                return json!({
                    "ok": false,
                    "transmitted": false,
                    "error": "draft request is not JSON"
                })
                .to_string();
            }
        };
        let id = request.get("id").cloned().unwrap_or(Value::Null);
        let op = request.get("op").and_then(Value::as_str).unwrap_or("");
        let draft_id = request
            .get("draft_id")
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_string();
        let agent = request
            .get("agent")
            .and_then(Value::as_str)
            .unwrap_or("agent")
            .to_string();
        let mut sessions = self.sessions.lock().expect("sessions");
        if !draft_id.is_empty() && !sessions.contains_key(&draft_id) {
            let Some(account) = super::persist::stored_compose_account(&draft_id) else {
                return denied(&id, "draft has no mailbox record");
            };
            match super::persist::stored_compose_owner(&draft_id).as_deref() {
                Some(owner) if owner == agent => {}
                Some(_) => return denied(&id, "this draft belongs to another agent"),
                None => return denied(&id, "draft has no mailbox record"),
            }
            let folder = match super::persist::ensure_draft_folder(&draft_id) {
                Ok(folder) => folder,
                Err(err) => return denied(&id, &err),
            };
            if workspace::draft_file_is_symlink(&folder) || !folder.join("draft.md").is_file() {
                return denied(&id, "draft has no mailbox record");
            }
            match DraftSession::open_folder(&folder, crate::AccountId(account)) {
                Ok(mut session) => {
                    session.attach_writer();
                    sessions.insert(draft_id.clone(), session);
                }
                Err(err) => return denied(&id, &err),
            }
        }
        let Some(session) = sessions.get_mut(&draft_id) else {
            return json!({
                "id": id,
                "ok": false,
                "transmitted": false,
                "error": "unknown draft"
            })
            .to_string();
        };
        if matches!(
            op,
            "get_draft"
                | "get_compose_context"
                | "edit_draft"
                | "replace_draft_body"
                | "request_send"
        ) && !session.allows_agent(&agent)
        {
            return denied(&id, "this draft belongs to another agent");
        }
        let mut reveal = None;
        let mut reply = match op {
            "get_draft" => ok_draft(id, session, &agent),
            "get_compose_context" => {
                let changes = session.take_user_changes(&agent);
                json!({
                    "id": id,
                    "ok": true,
                    "transmitted": false,
                    "draft_id": session.id(),
                    "cwd": session.folder(),
                    "context": "Use fell-mcp for contacts, search, read_thread, folders, and labels. This folder has no thread copy.",
                    "in_reply_to": session.fields().in_reply_to,
                    "user_changes_since_your_last_read": changes,
                })
            }
            "edit_draft" => {
                let changes = session.take_user_changes(&agent);
                let base = request
                    .get("base_revision")
                    .and_then(Value::as_u64)
                    .unwrap_or(0);
                let edits = request
                    .get("edits")
                    .and_then(Value::as_array)
                    .cloned()
                    .unwrap_or_default();
                let mut error = None;
                for edit in edits {
                    let old = edit.get("old").and_then(Value::as_str).unwrap_or("");
                    let new = edit.get("new").and_then(Value::as_str).unwrap_or("");
                    if let Err(err) = session.edit(base, old, new) {
                        error = Some(err);
                        break;
                    }
                }
                match error {
                    Some(err) => json!({
                        "id": id,
                        "ok": false,
                        "transmitted": false,
                        "error": err,
                        "revision": session.revision(),
                        "draft_md": session.serialize(),
                        "user_changes_since_your_last_read": changes,
                    }),
                    None => {
                        let _ = session.flush();
                        ok_draft_with(id, session, &agent, changes)
                    }
                }
            }
            "replace_draft_body" => {
                let changes = session.take_user_changes(&agent);
                let body = request.get("body").and_then(Value::as_str).unwrap_or("");
                match session.replace_body(body) {
                    Ok(()) => {
                        let _ = session.flush();
                        ok_draft_with(id, session, &agent, changes)
                    }
                    Err(err) => json!({
                        "id": id,
                        "ok": false,
                        "transmitted": false,
                        "error": err,
                        "user_changes_since_your_last_read": changes,
                    }),
                }
            }
            "request_send" => {
                let changes = session.take_user_changes(&agent);
                let transmitted = session.request_send();
                *self.focused.lock().expect("focused") = Some(draft_id.clone());
                reveal = Some(draft_id.clone());
                let snap = session.snapshot();
                json!({
                    "id": id,
                    "ok": true,
                    "transmitted": transmitted,
                    "opened": false,
                    "focused": false,
                    "draft_id": snap.id,
                    "revision": snap.revision,
                    "to": snap.to.iter().map(|item| item.raw.clone()).collect::<Vec<_>>(),
                    "cc": snap.cc.iter().map(|item| item.raw.clone()).collect::<Vec<_>>(),
                    "bcc": snap.bcc.iter().map(|item| item.raw.clone()).collect::<Vec<_>>(),
                    "subject": snap.subject,
                    "body": snap.body,
                    "in_reply_to": snap.in_reply_to,
                    "attachments": snap.attachments.iter().map(|item| {
                        json!({"path": item.path, "name": item.name})
                    }).collect::<Vec<_>>(),
                    "unreviewed": snap.unreviewed,
                    "user_changes_since_your_last_read": changes,
                    "note": "Nothing was sent. Mail leaves only when the person presses send in the compose editor.",
                })
            }
            _ => json!({
                "id": id,
                "ok": false,
                "transmitted": false,
                "error": "unknown draft op"
            }),
        };
        let saved = sessions.get(&draft_id).and_then(DraftSession::save_waiter);
        drop(sessions);
        self.emit();
        // An agent reads draft.md right after a tool returns, so the reply
        // waits for the save. The session lock is already released, so the
        // editor keeps painting meanwhile.
        if let Some(saved) = saved
            && let Err(err) = saved.wait()
            && let Some(object) = reply.as_object_mut()
        {
            // The edit is in the live draft and on screen; only the file
            // write failed, so the agent should not repeat the edit.
            object.insert("saved".into(), json!(false));
            object.insert("save_error".into(), json!(err));
        }
        if let Some(id) = reveal {
            let shown = self.wait_shown(&id);
            if let Some(object) = reply.as_object_mut() {
                object.insert("opened".into(), json!(shown));
                object.insert("focused".into(), json!(shown));
            }
        }
        reply.to_string()
    }

    pub fn bind(path: &Path) -> std::io::Result<UnixListener> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
            std::fs::set_permissions(parent, std::fs::Permissions::from_mode(0o700))?;
        }
        if path.exists() {
            std::fs::remove_file(path)?;
        }
        let listener = UnixListener::bind(path)?;
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600))?;
        Ok(listener)
    }

    pub fn serve_one(self: &Arc<Self>, stream: UnixStream) {
        let Ok(incoming) = stream.try_clone() else {
            return;
        };
        let mut reader = BufReader::new(incoming);
        let mut line = String::new();
        if reader.read_line(&mut line).is_err() {
            return;
        }
        let reply = self.handle_line(line.trim());
        let mut out = stream;
        let _ = writeln!(out, "{reply}");
    }

    fn emit(&self) {
        let mut listeners = self.listeners.lock().expect("listeners");
        listeners.retain(|listener| listener.send(()).is_ok());
    }
}

fn denied(id: &Value, error: &str) -> String {
    json!({
        "id": id,
        "ok": false,
        "transmitted": false,
        "error": error
    })
    .to_string()
}

fn ok_draft(id: Value, session: &mut DraftSession, agent: &str) -> Value {
    let changes = session.take_user_changes(agent);
    ok_draft_with(id, session, agent, changes)
}

fn ok_draft_with(id: Value, session: &DraftSession, _agent: &str, changes: String) -> Value {
    let snap = session.snapshot();
    json!({
        "id": id,
        "ok": true,
        "transmitted": false,
        "draft_id": snap.id,
        "revision": snap.revision,
        "draft_md": snap.text,
        "to": snap.to.iter().map(|item| item.raw.clone()).collect::<Vec<_>>(),
        "cc": snap.cc.iter().map(|item| item.raw.clone()).collect::<Vec<_>>(),
        "bcc": snap.bcc.iter().map(|item| item.raw.clone()).collect::<Vec<_>>(),
        "subject": snap.subject,
        "body": snap.body,
        "in_reply_to": snap.in_reply_to,
        "unreviewed": snap.unreviewed,
        "user_changes_since_your_last_read": changes,
        "cwd": session.folder(),
    })
}

pub fn rpc_call(request: &Value) -> Result<Value, String> {
    let path = draft_socket_path();
    let mut stream = UnixStream::connect(&path).map_err(|err| {
        if err.kind() == std::io::ErrorKind::NotFound
            || err.kind() == std::io::ErrorKind::ConnectionRefused
        {
            "compose editor is not running".into()
        } else {
            err.to_string()
        }
    })?;
    stream
        .set_write_timeout(Some(std::time::Duration::from_secs(2)))
        .ok();
    stream
        .set_read_timeout(Some(std::time::Duration::from_secs(2)))
        .ok();
    writeln!(stream, "{request}").map_err(|err| err.to_string())?;
    let mut line = String::new();
    BufReader::new(stream)
        .read_line(&mut line)
        .map_err(|err| err.to_string())?;
    serde_json::from_str(line.trim()).map_err(|err| err.to_string())
}

fn session_from_folder(folder: &std::path::Path, account: &str) -> Result<DraftSession, String> {
    let id = folder
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or("draft");
    if let Some(store) = super::persist::open_store_for_draft(id) {
        return DraftSession::open_with_store(
            folder,
            crate::AccountId(account.to_string()),
            &store,
        );
    }
    let text = workspace::read_draft_file(folder)?;
    let parsed = super::frontmatter::parse_draft(&text).map_err(|err| err.message)?;
    DraftSession::create(
        folder.to_path_buf(),
        id,
        crate::AccountId(account.to_string()),
        parsed.fields,
        parsed.body,
    )
}

fn open_draft_for_agent(
    draft_id: &str,
    agent: &str,
    account: &str,
) -> Result<std::path::PathBuf, String> {
    let folder = workspace::draft_folder(draft_id)?;
    if workspace::draft_file_is_symlink(&folder) {
        return Err("draft.md is a symlink".into());
    }
    if let Some(owner) = super::persist::stored_compose_owner(draft_id) {
        if owner != agent {
            return Err("this draft belongs to another agent".into());
        }
    } else if super::persist::stored_compose_account(draft_id).is_some() {
        return Err("draft has no mailbox record".into());
    }
    if let Some(stored) = super::persist::stored_compose_account(draft_id)
        && stored != account
    {
        return Err("this draft belongs to another account".into());
    }
    if folder.join("draft.md").is_file() {
        return Ok(folder);
    }
    super::persist::open_owned_draft(draft_id, agent, account)
}

pub fn editor_is_running() -> bool {
    UnixStream::connect(draft_socket_path()).is_ok()
}

pub fn file_fallback_get(draft_id: &str, agent: &str, account: &str) -> Result<Value, String> {
    let folder = open_draft_for_agent(draft_id, agent, account)?;
    let text = workspace::read_draft_file(&folder)?;
    Ok(json!({
        "ok": true,
        "transmitted": false,
        "draft_id": draft_id,
        "draft_md": text,
        "cwd": folder,
        "user_changes_since_your_last_read": "",
    }))
}

pub fn file_fallback_edit(
    draft_id: &str,
    agent: &str,
    account: &str,
    old: &str,
    new: &str,
) -> Result<Value, String> {
    let folder = open_draft_for_agent(draft_id, agent, account)?;
    let mut session = session_from_folder(&folder, account)?;
    session.edit(session.revision(), old, new)?;
    session.flush()?;
    let snap = session.snapshot();
    Ok(json!({
        "ok": true,
        "transmitted": false,
        "draft_id": draft_id,
        "draft_md": snap.text,
        "in_reply_to": snap.in_reply_to,
        "unreviewed": snap.unreviewed,
    }))
}

pub fn file_fallback_replace_body(
    draft_id: &str,
    agent: &str,
    account: &str,
    body: &str,
) -> Result<Value, String> {
    let folder = open_draft_for_agent(draft_id, agent, account)?;
    let mut session = session_from_folder(&folder, account)?;
    let previous = session.body().to_string();
    session.replace_body(body)?;
    session.flush()?;
    Ok(json!({
        "ok": true,
        "transmitted": false,
        "draft_id": draft_id,
        "draft_md": session.serialize(),
        "previous_body": previous,
        "in_reply_to": session.fields().in_reply_to,
    }))
}

pub fn request_send_without_editor() -> Value {
    json!({
        "ok": false,
        "transmitted": false,
        "opened": false,
        "error": "compose editor is not running; nothing was sent",
    })
}

#[cfg(test)]
mod tests {
    use super::super::frontmatter::DraftFields;
    use super::*;
    use crate::AccountId;
    use std::fs;

    #[test]
    fn request_send_and_edit_never_transmit() {
        let root = std::env::temp_dir().join(format!(
            "fell-hub-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let _ = fs::remove_dir_all(&root);
        let fields = DraftFields {
            from: "jon@example.com".into(),
            to: vec!["ada@example.com".into()],
            subject: "Hi".into(),
            ..DraftFields::default()
        };
        let folder =
            workspace::materialize_in(&root, "d1", &fields, "alpha\n", &AccountId("a".into()))
                .unwrap();
        let mut session =
            DraftSession::create(folder, "d1", AccountId("a".into()), fields, "alpha\n").unwrap();
        session.set_owner("bot");
        let hub = DraftHub::new();
        hub.insert(session);
        let edited = hub.handle_line(
            &json!({
                "id": 1,
                "op": "edit_draft",
                "draft_id": "d1",
                "agent": "bot",
                "base_revision": 1,
                "edits": [{"old": "alpha", "new": "beta"}],
                "send": true
            })
            .to_string(),
        );
        let edited: Value = serde_json::from_str(&edited).unwrap();
        assert_eq!(edited["transmitted"], false);
        assert_eq!(edited["ok"], true);
        assert!(edited["draft_md"].as_str().unwrap().contains("beta"));
        let requested = hub.handle_line(
            &json!({
                "id": 2,
                "op": "request_send",
                "draft_id": "d1",
                "agent": "bot",
                "send": true,
                "confirm": true
            })
            .to_string(),
        );
        let requested: Value = serde_json::from_str(&requested).unwrap();
        assert_eq!(requested["transmitted"], false);
        assert_eq!(requested["opened"], false);
        hub.note_shown("d1");
        let shown = hub.handle_line(
            &json!({
                "id": 3,
                "op": "request_send",
                "draft_id": "d1",
                "agent": "bot"
            })
            .to_string(),
        );
        let shown: Value = serde_json::from_str(&shown).unwrap();
        assert_eq!(shown["opened"], true);
        assert_eq!(shown["transmitted"], false);
        assert!(
            requested["note"]
                .as_str()
                .unwrap()
                .contains("Nothing was sent")
        );
        assert!(requested.get("remote_id").is_none());
        let offline = request_send_without_editor();
        assert_eq!(offline["transmitted"], false);
    }

    #[test]
    fn reading_a_session_does_not_wake_the_window() {
        let root = std::env::temp_dir().join(format!(
            "fell-hub-idle-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let _ = fs::remove_dir_all(&root);
        let fields = DraftFields {
            from: "jon@example.com".into(),
            to: vec!["ada@example.com".into()],
            subject: "Hi".into(),
            ..DraftFields::default()
        };
        let folder =
            workspace::materialize_in(&root, "d1", &fields, "alpha\n", &AccountId("a".into()))
                .unwrap();
        let session =
            DraftSession::create(folder, "d1", AccountId("a".into()), fields, "alpha\n").unwrap();
        let hub = DraftHub::new();
        hub.insert(session);
        let rx = hub.subscribe();
        while rx.try_recv().is_ok() {}
        hub.with_session("d1", |session| {
            let _ = session.snapshot();
            session.mark_seen();
        });
        assert!(rx.try_recv().is_err());
        hub.with_session("d1", |session| session.open_for_edit());
        assert!(rx.try_recv().is_ok());
    }

    #[test]
    fn request_send_waits_until_the_window_shows_that_draft() {
        let root = std::env::temp_dir().join(format!(
            "fell-hub-show-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let _ = fs::remove_dir_all(&root);
        let fields = DraftFields {
            from: "jon@example.com".into(),
            to: vec!["ada@example.com".into()],
            subject: "Hi".into(),
            ..DraftFields::default()
        };
        let folder =
            workspace::materialize_in(&root, "d1", &fields, "alpha\n", &AccountId("a".into()))
                .unwrap();
        let mut session =
            DraftSession::create(folder, "d1", AccountId("a".into()), fields, "alpha\n").unwrap();
        session.set_owner("bot");
        let hub = Arc::new(DraftHub::new());
        hub.insert(session);
        let _inbox = hub.subscribe();
        let waiter = Arc::clone(&hub);
        std::thread::spawn(move || {
            std::thread::sleep(std::time::Duration::from_millis(40));
            waiter.note_shown("d1");
        });
        let requested = hub.handle_line(
            &json!({
                "id": 1,
                "op": "request_send",
                "draft_id": "d1",
                "agent": "bot"
            })
            .to_string(),
        );
        let requested: Value = serde_json::from_str(&requested).unwrap();
        assert_eq!(requested["opened"], true);
        assert_eq!(requested["transmitted"], false);
    }

    #[test]
    fn another_agent_cannot_read_the_draft() {
        let root = std::env::temp_dir().join(format!(
            "fell-hub-owner-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let _ = fs::remove_dir_all(&root);
        let fields = DraftFields {
            from: "jon@example.com".into(),
            to: vec!["ada@example.com".into()],
            bcc: vec!["secret-bcc@example.com".into()],
            subject: "Hi".into(),
            ..DraftFields::default()
        };
        let folder = workspace::materialize_in(
            &root,
            "d1",
            &fields,
            "hidden body\n",
            &AccountId("a".into()),
        )
        .unwrap();
        let mut session = DraftSession::create(
            folder.clone(),
            "d1",
            AccountId("a".into()),
            fields,
            "hidden body\n",
        )
        .unwrap();
        session.set_owner("agent:ada");
        workspace::write_owner(&folder, "agent:bob").unwrap();
        let hub = DraftHub::new();
        hub.insert(session);
        let denied = hub.handle_line(
            &json!({
                "id": 1,
                "op": "get_draft",
                "draft_id": "d1",
                "agent": "agent:bob"
            })
            .to_string(),
        );
        assert!(denied.contains("another agent"), "{denied}");
        assert!(!denied.contains("secret-bcc@example.com"));
        assert!(!denied.contains("hidden body"));
        let allowed = hub.handle_line(
            &json!({
                "id": 2,
                "op": "get_draft",
                "draft_id": "d1",
                "agent": "agent:ada"
            })
            .to_string(),
        );
        assert!(allowed.contains("hidden body"), "{allowed}");
        assert!(allowed.contains("secret-bcc@example.com"), "{allowed}");
    }

    #[test]
    fn the_socket_loads_the_stored_account_not_local() {
        let id = format!(
            "acct{}{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        );
        let id = id.chars().take(32).collect::<String>();
        let folder = workspace::draft_folder(&id).unwrap();
        let control = workspace::control_dir(&folder);
        let index = super::super::persist::compose_index_file(&id);
        let db = std::env::temp_dir().join(format!("{id}.db"));
        let _ = fs::remove_dir_all(&folder);
        let _ = fs::remove_dir_all(&control);
        let _ = fs::remove_file(&db);
        let fields = DraftFields {
            from: "jon@example.com".into(),
            to: vec!["ada@example.com".into()],
            bcc: vec!["secret-bcc@example.com".into()],
            subject: "Hi".into(),
            ..DraftFields::default()
        };
        let root = folder.parent().unwrap().to_path_buf();
        workspace::materialize_in(
            &root,
            &id,
            &fields,
            "hidden body\n",
            &AccountId("acct-real".into()),
        )
        .unwrap();
        workspace::write_owner(&folder, "agent:bob").unwrap();
        let store = crate::Store::open(&db).unwrap();
        store
            .upsert_account(&crate::Account {
                id: AccountId("acct-real".into()),
                display_name: "Jon".into(),
                email: "jon@example.com".into(),
                backend_kind: crate::BackendKind::Bridge,
            })
            .unwrap();
        store
            .save_compose(&super::super::persist::StoredCompose {
                id: id.clone(),
                account_id: AccountId("acct-real".into()),
                fields: fields.clone(),
                body: "hidden body\n".into(),
                files: Vec::new(),
                identities: vec!["jon@example.com".into()],
                owner: "agent:ada".into(),
            })
            .unwrap();
        super::super::persist::remember_compose_store(&id, &db).unwrap();
        let hub = DraftHub::new();
        let denied = hub.handle_line(
            &json!({
                "id": 1,
                "op": "get_draft",
                "draft_id": id,
                "agent": "agent:bob"
            })
            .to_string(),
        );
        assert!(denied.contains("another agent"), "{denied}");
        assert!(!denied.contains("hidden body"), "{denied}");
        assert!(!denied.contains("secret-bcc"), "{denied}");
        let opened = hub.handle_line(
            &json!({
                "id": 2,
                "op": "get_draft",
                "draft_id": id,
                "agent": "agent:ada"
            })
            .to_string(),
        );
        assert!(opened.contains("hidden body"), "{opened}");
        let account = hub.with_session(&id, |session| session.account_id().as_str().to_string());
        assert_eq!(account.as_deref(), Some("acct-real"));
        let missing = DraftHub::new().handle_line(
            &json!({
                "id": 3,
                "op": "get_draft",
                "draft_id": format!("{id}x"),
                "agent": "agent:ada"
            })
            .to_string(),
        );
        assert!(missing.contains("mailbox"), "{missing}");
        assert!(!missing.contains("another agent"), "{missing}");
        let _ = fs::remove_dir_all(&folder);
        let _ = fs::remove_dir_all(&control);
        let _ = fs::remove_file(&db);
        if let Some(index) = index {
            let _ = fs::remove_file(index);
        }
    }
}
