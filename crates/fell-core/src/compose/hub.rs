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
use std::sync::{Arc, Mutex};

use serde_json::{Value, json};

use super::session::DraftSession;
use super::workspace::{self, draft_socket_path};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct HubEvent;

pub struct DraftHub {
    sessions: Mutex<HashMap<String, DraftSession>>,
    listeners: Mutex<Vec<Sender<()>>>,
    focused: Mutex<Option<String>>,
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
        }
    }

    pub fn subscribe(&self) -> Receiver<()> {
        let (tx, rx) = mpsc::channel();
        self.listeners.lock().expect("listeners").push(tx);
        rx
    }

    pub fn insert(&self, session: DraftSession) {
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
        let value = body(session);
        drop(sessions);
        self.emit();
        Some(value)
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
        if !draft_id.is_empty()
            && !sessions.contains_key(&draft_id)
            && let Ok(folder) = workspace::draft_folder(&draft_id)
            && folder.join("draft.md").is_file()
            && let Ok(session) =
                DraftSession::open_folder(&folder, crate::AccountId("local".into()))
        {
            sessions.insert(draft_id.clone(), session);
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
        let reply = match op {
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
                let snap = session.snapshot();
                json!({
                    "id": id,
                    "ok": true,
                    "transmitted": transmitted,
                    "opened": snap.open,
                    "focused": snap.focused,
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
        drop(sessions);
        self.emit();
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

pub fn editor_is_running() -> bool {
    UnixStream::connect(draft_socket_path()).is_ok()
}

pub fn file_fallback_get(draft_id: &str) -> Result<Value, String> {
    let folder = workspace::draft_folder(draft_id)?;
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

pub fn file_fallback_edit(draft_id: &str, old: &str, new: &str) -> Result<Value, String> {
    let folder = workspace::draft_folder(draft_id)?;
    let text = workspace::read_draft_file(&folder)?;
    let at = super::merge::find_unique(&text, old)?;
    let mut next = text.clone();
    next.replace_range(at..at + old.len(), new);
    if super::frontmatter::parse_draft(&next).is_err() {
        return Err("edit would make draft.md unreadable; the file was left unchanged".into());
    }
    workspace::write_draft_file(&folder, &next)?;
    Ok(json!({
        "ok": true,
        "transmitted": false,
        "draft_id": draft_id,
        "draft_md": next,
    }))
}

pub fn file_fallback_replace_body(draft_id: &str, body: &str) -> Result<Value, String> {
    let folder = workspace::draft_folder(draft_id)?;
    let text = workspace::read_draft_file(&folder)?;
    let mut parsed = super::frontmatter::parse_draft(&text).map_err(|err| err.message)?;
    let previous = parsed.body.clone();
    parsed.body = if body.ends_with('\n') || body.is_empty() {
        body.to_string()
    } else {
        format!("{body}\n")
    };
    let next = super::frontmatter::write_draft(&parsed.fields, &parsed.body);
    workspace::write_draft_file(&folder, &next)?;
    Ok(json!({
        "ok": true,
        "transmitted": false,
        "draft_id": draft_id,
        "draft_md": next,
        "previous_body": previous,
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
        let folder = workspace::materialize_in(&root, "d1", &fields, "alpha\n").unwrap();
        let session =
            DraftSession::create(folder, "d1", AccountId("a".into()), fields, "alpha\n").unwrap();
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
        assert_eq!(requested["opened"], true);
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
}
